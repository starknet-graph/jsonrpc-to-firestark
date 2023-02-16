use std::{env, fs::File, io::Write, path::PathBuf, time::Duration};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use starknet::{
    core::{serde::unsigned_field_element::UfeHex, types::FieldElement},
    providers::jsonrpc::{
        models::{
            BlockId, BlockWithTxs, EmittedEvent, ErrorCode, EventFilter, InvokeTransaction,
            MaybePendingBlockWithTxs, Transaction,
        },
        HttpTransport, JsonRpcClient, JsonRpcClientError, RpcError,
    },
};
use url::Url;

const HEAD_BACKOFF: Duration = Duration::new(1, 0);
const CHECKPOINT_BLOCK_INTERVAL: u64 = 10;
const MAX_BLOCK_HISTORY: usize = 64;

#[serde_as]
#[derive(Serialize, Deserialize)]
struct Checkpoint {
    latest_blocks: Vec<BlockInfo>,
}

#[serde_as]
#[derive(Serialize, Deserialize)]
struct BlockInfo {
    number: u64,
    #[serde_as(as = "UfeHex")]
    hash: FieldElement,
}

#[tokio::main]
async fn main() -> Result<()> {
    let jsonrpc_url = env::var("JSONRPC")
        .expect("Environment variable JSONRPC not found")
        .parse::<Url>()
        .expect("Invalid JSONRPC URL");
    let checkpoint_path = env::var("CHECKPOINT_FILE")
        .expect("Environment variable CHECKPOINT_FILE not found")
        .parse::<PathBuf>()
        .expect("Invalid checkpoint file path");

    let jsonrpc_client = JsonRpcClient::new(HttpTransport::new_with_client(
        jsonrpc_url,
        reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(10))
            .build()?,
    ));

    run(&jsonrpc_client, &checkpoint_path).await?;

    Ok(())
}

async fn run(
    jsonrpc_client: &JsonRpcClient<HttpTransport>,
    checkpoint_path: &PathBuf,
) -> Result<()> {
    let temp_checkpoint_file_path =
        PathBuf::from(&format!("{}.tmp", checkpoint_path.to_string_lossy()));

    println!("{}", temp_checkpoint_file_path.to_string_lossy());

    let mut checkpoint: Checkpoint = if checkpoint_path.exists() {
        serde_json::from_reader(&mut File::open(checkpoint_path)?)?
    } else {
        Checkpoint {
            latest_blocks: vec![],
        }
    };

    loop {
        let (expected_parent_hash, current_block_number) = match checkpoint.latest_blocks.last() {
            Some(last_block) => (last_block.hash, last_block.number + 1),
            None => (FieldElement::ZERO, 0),
        };

        let current_block = match jsonrpc_client
            .get_block_with_txs(&BlockId::Number(current_block_number))
            .await
        {
            Ok(block) => match block {
                MaybePendingBlockWithTxs::Block(block) => block,
                MaybePendingBlockWithTxs::PendingBlock(_) => {
                    anyhow::bail!("Unexpected pending block")
                }
            },
            Err(JsonRpcClientError::RpcError(RpcError::Code(ErrorCode::BlockNotFound))) => {
                // No new block. Wait a bit before trying again
                tokio::time::sleep(HEAD_BACKOFF).await;
                continue;
            }
            Err(_) => anyhow::bail!("Retry on failure not implemented"),
        };

        if current_block.parent_hash != expected_parent_hash {
            if checkpoint.latest_blocks.len() == 1 {
                // Not possible to determine common ancestor
                anyhow::bail!("Unable to handle reorg after consuming the whole history");
            } else {
                checkpoint.latest_blocks.pop();
                continue;
            }
        }

        handle_block(jsonrpc_client, &current_block).await?;

        let new_block_info = BlockInfo {
            number: current_block.block_number,
            hash: current_block.block_hash,
        };

        if checkpoint.latest_blocks.len() >= MAX_BLOCK_HISTORY {
            checkpoint.latest_blocks.remove(0);
        }
        checkpoint.latest_blocks.push(new_block_info);

        if current_block.block_number % CHECKPOINT_BLOCK_INTERVAL == 0 {
            serde_json::to_writer(&mut File::create(&temp_checkpoint_file_path)?, &checkpoint)?;
            std::fs::rename(&temp_checkpoint_file_path, checkpoint_path)?;
        }
    }
}

async fn handle_block(
    jsonrpc_client: &JsonRpcClient<HttpTransport>,
    block: &BlockWithTxs,
) -> Result<()> {
    let mut buffer = Vec::new();

    writeln!(&mut buffer, "FIRE BLOCK_BEGIN {}", block.block_number)?;

    let block_events = get_block_events(jsonrpc_client, block.block_hash).await?;

    for tx in block.transactions.iter() {
        let (tx_hash, type_str) = match tx {
            Transaction::Declare(tx) => (tx.transaction_hash, "DECLARE"),
            Transaction::Deploy(tx) => (tx.transaction_hash, "DEPLOY"),
            Transaction::DeployAccount(tx) => (tx.transaction_hash, "DEPLOY_ACCOUNT"),
            Transaction::Invoke(tx) => (
                match tx {
                    InvokeTransaction::V0(tx) => tx.transaction_hash,
                    InvokeTransaction::V1(tx) => tx.transaction_hash,
                },
                "INVOKE_FUNCTION",
            ),
            Transaction::L1Handler(tx) => (tx.transaction_hash, "L1_HANDLER"),
        };

        writeln!(&mut buffer, "FIRE BEGIN_TRX {tx_hash:#064x} {type_str}")?;

        for (ind_event, event) in block_events
            .iter()
            .filter(|event| event.transaction_hash == tx_hash)
            .enumerate()
        {
            writeln!(
                &mut buffer,
                "FIRE TRX_BEGIN_EVENT {:#064x} {:#064x}",
                tx_hash, event.from_address
            )?;

            for key in event.keys.iter() {
                writeln!(
                    &mut buffer,
                    "FIRE TRX_EVENT_KEY {:#064x} {} {:#064x}",
                    tx_hash, ind_event, key
                )?;
            }

            for data in event.data.iter() {
                writeln!(
                    &mut buffer,
                    "FIRE TRX_EVENT_DATA {:#064x} {} {:#064x}",
                    tx_hash, ind_event, data
                )?;
            }
        }
    }

    writeln!(
        &mut buffer,
        "FIRE BLOCK_END {} {:#064x} {:#064x} {} {}",
        block.block_number,
        block.block_hash,
        block.parent_hash,
        block.timestamp,
        block.transactions.len(),
    )?;

    print!("{}", String::from_utf8(buffer)?);

    Ok(())
}

async fn get_block_events(
    jsonrpc_client: &JsonRpcClient<HttpTransport>,
    block_hash: FieldElement,
) -> Result<Vec<EmittedEvent>> {
    const CHUNK_SIZE: u64 = 1000;

    let mut token = None;
    let mut events = vec![];

    loop {
        let mut result = jsonrpc_client
            .get_events(
                EventFilter {
                    from_block: Some(BlockId::Hash(block_hash)),
                    to_block: Some(BlockId::Hash(block_hash)),
                    address: None,
                    keys: None,
                },
                token,
                CHUNK_SIZE,
            )
            .await?;

        events.append(&mut result.events);

        match result.continuation_token {
            Some(new_token) => {
                token = Some(new_token);
            }
            None => break,
        }
    }

    Ok(events)
}

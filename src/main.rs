use std::{fs::File, io::Write, path::PathBuf, time::Duration};

use anyhow::Result;
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use starknet::{
    core::{
        serde::unsigned_field_element::UfeHex,
        types::{
            BlockId, BlockWithTxs, EmittedEvent, EventFilter, FieldElement,
            MaybePendingBlockWithTxs, StarknetError, Transaction,
        },
    },
    providers::{
        jsonrpc::{HttpTransport, JsonRpcClient},
        Provider,
    },
};
use url::Url;

const HEAD_BACKOFF: Duration = Duration::new(1, 0);
const FAILURE_BACKOFF: Duration = Duration::new(1, 0);
const CHECKPOINT_BLOCK_INTERVAL: u64 = 50;
const MAX_BLOCK_HISTORY: usize = 64;

#[derive(Debug, Parser)]
#[clap(author, version, about)]
struct Cli {
    #[clap(long, env = "JSONRPC", help = "URL of the trusted JSON-RPC endpoint")]
    jsonrpc: Url,
    #[clap(long, env = "CHECKPOINT_FILE", help = "Path to the checkpoint file")]
    checkpoint: PathBuf,
}

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
    let cli = Cli::parse();

    let jsonrpc_client = JsonRpcClient::new(HttpTransport::new_with_client(
        cli.jsonrpc,
        reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(10))
            .build()?,
    ));

    let mut checkpoint: Checkpoint = if cli.checkpoint.exists() {
        serde_json::from_reader(&mut File::open(&cli.checkpoint)?)?
    } else {
        Checkpoint {
            latest_blocks: vec![],
        }
    };

    run(&jsonrpc_client, &mut checkpoint, &cli.checkpoint).await;

    Ok(())
}

async fn run(
    jsonrpc_client: &JsonRpcClient<HttpTransport>,
    checkpoint: &mut Checkpoint,
    checkpoint_path: &PathBuf,
) {
    loop {
        let (expected_parent_hash, current_block_number) = match checkpoint.latest_blocks.last() {
            Some(last_block) => (last_block.hash, last_block.number + 1),
            None => (FieldElement::ZERO, 0),
        };

        let current_block = match jsonrpc_client
            .get_block_with_txs(BlockId::Number(current_block_number))
            .await
        {
            Ok(block) => match block {
                MaybePendingBlockWithTxs::Block(block) => block,
                MaybePendingBlockWithTxs::PendingBlock(_) => {
                    eprintln!("Unexpected pending block");
                    tokio::time::sleep(FAILURE_BACKOFF).await;
                    continue;
                }
            },
            Err(starknet::providers::ProviderError::StarknetError(
                StarknetError::BlockNotFound,
            )) => {
                // No new block. Wait a bit before trying again
                tokio::time::sleep(HEAD_BACKOFF).await;
                continue;
            }
            Err(err) => {
                eprintln!("Error downloading block: {err}");
                tokio::time::sleep(FAILURE_BACKOFF).await;
                continue;
            }
        };

        if current_block.parent_hash != expected_parent_hash {
            if checkpoint.latest_blocks.len() == 1 {
                // Not possible to determine common ancestor
                panic!("Unable to handle reorg after consuming the whole history");
            } else {
                checkpoint.latest_blocks.pop();
                continue;
            }
        }

        if let Err(err) = handle_block(jsonrpc_client, &current_block).await {
            eprintln!("Error handling block: {err}");
            tokio::time::sleep(FAILURE_BACKOFF).await;
            continue;
        }

        println!(
            "Processed block #{} ({:#064x})",
            current_block.block_number, current_block.block_hash
        );

        let new_block_info = BlockInfo {
            number: current_block.block_number,
            hash: current_block.block_hash,
        };

        if checkpoint.latest_blocks.len() >= MAX_BLOCK_HISTORY {
            checkpoint.latest_blocks.remove(0);
        }
        checkpoint.latest_blocks.push(new_block_info);

        if current_block.block_number % CHECKPOINT_BLOCK_INTERVAL == 0 {
            if let Err(err) = try_persist_checkpoint(checkpoint_path, checkpoint) {
                eprintln!("Error persisting checkpoint: {err}");
            }
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
            Transaction::Declare(tx) => (*tx.transaction_hash(), "DECLARE"),
            Transaction::Deploy(tx) => (tx.transaction_hash, "DEPLOY"),
            Transaction::DeployAccount(tx) => (*tx.transaction_hash(), "DEPLOY_ACCOUNT"),
            Transaction::Invoke(tx) => (*tx.transaction_hash(), "INVOKE_FUNCTION"),
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

fn try_persist_checkpoint(path: &PathBuf, checkpoint: &Checkpoint) -> Result<()> {
    let temp_path = PathBuf::from(&format!("{}.tmp", path.to_string_lossy()));

    serde_json::to_writer(&mut File::create(&temp_path)?, &checkpoint)?;
    std::fs::rename(temp_path, path)?;

    Ok(())
}

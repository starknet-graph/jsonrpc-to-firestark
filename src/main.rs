use std::{env, io::Write, time::Duration};

use anyhow::Result;
use starknet::{
    core::types::FieldElement,
    providers::jsonrpc::{
        models::{
            BlockId, BlockWithTxs, EmittedEvent, ErrorCode, EventFilter, InvokeTransaction,
            MaybePendingBlockWithTxHashes, MaybePendingBlockWithTxs, Transaction,
        },
        HttpTransport, JsonRpcClient, JsonRpcClientError, RpcError,
    },
};
use url::Url;

const HEAD_BACKOFF: Duration = Duration::new(1, 0);

#[tokio::main]
async fn main() -> Result<()> {
    let jsonrpc_url = env::var("JSONRPC")
        .expect("Environment variable JSONRPC not found")
        .parse::<Url>()
        .expect("Invalid JSONRPC URL");
    let jsonrpc_client = JsonRpcClient::new(HttpTransport::new_with_client(
        jsonrpc_url,
        reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(10))
            .build()?,
    ));

    run_from_block(&jsonrpc_client, 0).await?;

    Ok(())
}

async fn run_from_block(
    jsonrpc_client: &JsonRpcClient<HttpTransport>,
    start_block_number: u64,
) -> Result<()> {
    let mut last_block_hash = if start_block_number == 0 {
        FieldElement::ZERO
    } else {
        let last_block = jsonrpc_client
            .get_block_with_tx_hashes(&BlockId::Number(start_block_number - 1))
            .await?;

        match last_block {
            MaybePendingBlockWithTxHashes::Block(block) => block.block_hash,
            MaybePendingBlockWithTxHashes::PendingBlock(_) => {
                anyhow::bail!("Unexpected pending block")
            }
        }
    };

    let mut current_block_number = start_block_number;

    loop {
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

        if current_block.parent_hash != last_block_hash {
            anyhow::bail!("Reorg handling not implemented");
        }

        handle_block(jsonrpc_client, &current_block).await?;

        last_block_hash = current_block.block_hash;
        current_block_number += 1;
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

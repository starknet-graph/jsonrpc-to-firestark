use std::{env, io::Write, time::Duration};

use anyhow::Result;
use starknet::{
    core::types::FieldElement,
    providers::jsonrpc::{
        models::{
            BlockId, EmittedEvent, EventFilter, InvokeTransaction, MaybePendingBlockWithTxs,
            Transaction,
        },
        HttpTransport, JsonRpcClient,
    },
};
use url::Url;

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

    for block_num in 0..1000 {
        handle_block(&jsonrpc_client, block_num).await?;
    }

    Ok(())
}

async fn handle_block(
    jsonrpc_client: &JsonRpcClient<HttpTransport>,
    block_number: u64,
) -> Result<()> {
    let mut buffer = Vec::new();

    let block = jsonrpc_client
        .get_block_with_txs(&BlockId::Number(block_number))
        .await?;
    let block = match block {
        MaybePendingBlockWithTxs::Block(block) => block,
        MaybePendingBlockWithTxs::PendingBlock(_) => anyhow::bail!("Unexpected pending block"),
    };

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

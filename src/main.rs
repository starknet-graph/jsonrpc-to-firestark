use std::{collections::HashSet, fs::File, io::Write, path::PathBuf, time::Duration};

use anyhow::Result;
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use starknet::{
    core::{
        serde::unsigned_field_element::UfeHex,
        types::{
            BlockId, BlockTag, BlockWithTxs, EmittedEvent, EventFilter, FieldElement,
            MaybePendingBlockWithTxs, PendingBlockWithTxs, StarknetError, Transaction,
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

enum MaybePendingBlock {
    Confirmed(BlockWithTxs),
    Pending {
        pseudo_height: u64,
        block: PendingBlockWithTxs,
    },
}

struct PendingBlockInfo {
    tx_count: usize,
}

impl MaybePendingBlock {
    pub fn parent_hash(&self) -> FieldElement {
        match self {
            Self::Confirmed(block) => block.parent_hash,
            Self::Pending { block, .. } => block.parent_hash,
        }
    }

    pub fn timestamp(&self) -> u64 {
        match self {
            Self::Confirmed(block) => block.timestamp,
            Self::Pending { block, .. } => block.timestamp,
        }
    }

    pub fn height(&self) -> u64 {
        match self {
            Self::Confirmed(block) => block.block_number,
            Self::Pending { pseudo_height, .. } => *pseudo_height,
        }
    }

    pub fn transactions(&self) -> &[Transaction] {
        match self {
            Self::Confirmed(block) => &block.transactions,
            Self::Pending { block, .. } => &block.transactions,
        }
    }

    pub fn block_hash(&self) -> FieldElement {
        match self {
            Self::Confirmed(block) => block.block_hash,
            Self::Pending {
                pseudo_height,
                block,
            } => {
                // +-------+-----------+--------------+---------------------+------------------+
                // | Fixed | "PENDING" | Block Number | Transaction Count   | Reserved         |
                // | 1 byte| 7 bytes   | 8 bytes      | 4 bytes             | 12 bytes         |
                // +-------+-----------+--------------+---------------------+------------------+
                // |  00   |    50     |      NN      |         NN          |        00        |
                // |       |    45     |      NN      |         NN          |        00        |
                // |       |    4E     |      NN      |         NN          |        00        |
                // |       |    44     |      NN      |         NN          |        00        |
                // |       |    49     |      NN      |                     |        00        |
                // |       |    4E     |      NN      |                     |        00        |
                // |       |    47     |      NN      |                     |        00        |
                // |       |           |      NN      |                     |        00        |
                // +-------+-----------+--------------+---------------------+------------------+

                let mut buffer = [0u8; 32];
                buffer[1] = b'P';
                buffer[2] = b'E';
                buffer[3] = b'N';
                buffer[4] = b'D';
                buffer[5] = b'I';
                buffer[6] = b'N';
                buffer[7] = b'G';

                buffer[8..(8 + 8)].copy_from_slice(&u64::to_be_bytes(*pseudo_height));
                buffer[(8 + 8)..(8 + 8 + 4)]
                    .copy_from_slice(&u32::to_be_bytes(block.transactions.len() as u32));

                // This cannot fail as the buffer is always in range
                FieldElement::from_bytes_be(&buffer).unwrap()
            }
        }
    }
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
    let mut last_pending_block: Option<PendingBlockInfo> = None;

    loop {
        let (expected_parent_hash, current_block_number) = match checkpoint.latest_blocks.last() {
            Some(last_block) => (last_block.hash, last_block.number + 1),
            None => (FieldElement::ZERO, 0),
        };

        let current_block = match jsonrpc_client
            .get_block_with_txs(BlockId::Number(current_block_number))
            .await
        {
            Ok(MaybePendingBlockWithTxs::Block(block)) => {
                // Got a confirmed block. Clear the pending info now
                last_pending_block = None;

                MaybePendingBlock::Confirmed(block)
            }
            Ok(MaybePendingBlockWithTxs::PendingBlock(_)) => {
                eprintln!("Unexpected pending block");
                tokio::time::sleep(FAILURE_BACKOFF).await;
                continue;
            }
            Err(starknet::providers::ProviderError::StarknetError(
                StarknetError::BlockNotFound,
            )) => {
                // No new block. Try the pending block now
                match jsonrpc_client
                    .get_block_with_txs(BlockId::Tag(BlockTag::Pending))
                    .await
                {
                    Ok(MaybePendingBlockWithTxs::Block(_)) => {
                        eprintln!("Unexpected confirmed block");
                        tokio::time::sleep(FAILURE_BACKOFF).await;
                        continue;
                    }
                    Ok(MaybePendingBlockWithTxs::PendingBlock(block)) => {
                        // Unlike with confirmed blocks, we simply discard non-linkable pending
                        // blocks
                        if block.parent_hash != expected_parent_hash {
                            eprintln!("Found unlinkable pending block");
                            tokio::time::sleep(HEAD_BACKOFF).await;
                            continue;
                        }

                        if let Some(last_pending_block) = last_pending_block.as_ref() {
                            if last_pending_block.tx_count >= block.transactions.len() {
                                eprintln!("No newer pending block found");
                                tokio::time::sleep(HEAD_BACKOFF).await;
                                continue;
                            }
                        }

                        MaybePendingBlock::Pending {
                            pseudo_height: current_block_number,
                            block,
                        }
                    }
                    Err(err) => {
                        eprintln!("Error downloading pending block: {err}");
                        tokio::time::sleep(HEAD_BACKOFF).await;
                        continue;
                    }
                }
            }
            Err(err) => {
                eprintln!("Error downloading confirmed block: {err}");
                tokio::time::sleep(FAILURE_BACKOFF).await;
                continue;
            }
        };

        // This can only happen with confirmed blocks as we discard non-linkable pending blocks
        if current_block.parent_hash() != expected_parent_hash {
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

        // Progress log
        match &current_block {
            MaybePendingBlock::Confirmed(block) => {
                println!(
                    "Processed block #{} ({:#064x})",
                    block.block_number, block.block_hash
                );
            }
            MaybePendingBlock::Pending {
                pseudo_height,
                block,
            } => {
                println!(
                    "Processed pending block with pseudo height #{} ({} transactions)",
                    pseudo_height,
                    block.transactions.len()
                );
            }
        }

        match &current_block {
            MaybePendingBlock::Confirmed(block) => {
                let new_block_info = BlockInfo {
                    number: block.block_number,
                    hash: block.block_hash,
                };

                if checkpoint.latest_blocks.len() >= MAX_BLOCK_HISTORY {
                    // TODO: use ring buffer instead as this is extremely inefficient
                    checkpoint.latest_blocks.remove(0);
                }
                checkpoint.latest_blocks.push(new_block_info);

                if block.block_number % CHECKPOINT_BLOCK_INTERVAL == 0 {
                    if let Err(err) = try_persist_checkpoint(checkpoint_path, checkpoint) {
                        eprintln!("Error persisting checkpoint: {err}");
                    }
                }
            }
            MaybePendingBlock::Pending { block, .. } => {
                last_pending_block = Some(PendingBlockInfo {
                    tx_count: block.transactions.len(),
                })
            }
        }
    }
}

async fn handle_block(
    jsonrpc_client: &JsonRpcClient<HttpTransport>,
    block: &MaybePendingBlock,
) -> Result<()> {
    let mut buffer = Vec::new();

    writeln!(&mut buffer, "FIRE BLOCK_BEGIN {}", block.height())?;

    let block_events = get_block_events(jsonrpc_client, block).await?;

    // When using pending blocks, there's a race condition where a new block is confirmed before
    // the `starknet_getEvents` request. We discard the block when:
    //
    // 1. Event list is empty when tx list is not; or
    // 2. Any event refers to a tx that's not in the block.
    if let MaybePendingBlock::Pending { block, .. } = block {
        if block_events.is_empty() && !block.transactions.is_empty() {
            anyhow::bail!("inconsistent pending block events: empty event list");
        }

        let block_tx_set = block
            .transactions
            .iter()
            .map(|tx| *tx.transaction_hash())
            .collect::<HashSet<_>>();

        if block_events
            .iter()
            .any(|event| !block_tx_set.contains(&event.transaction_hash))
        {
            anyhow::bail!("inconsistent pending block events: invalid transaction reference");
        }
    }

    for tx in block.transactions().iter() {
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
        block.height(),
        block.block_hash(),
        block.parent_hash(),
        block.timestamp(),
        block.transactions().len(),
    )?;

    print!("{}", String::from_utf8(buffer)?);

    Ok(())
}

// Use `get_block_with_receipts` once we move to RPC v0.7.x
async fn get_block_events(
    jsonrpc_client: &JsonRpcClient<HttpTransport>,
    block: &MaybePendingBlock,
) -> Result<Vec<EmittedEvent>> {
    const CHUNK_SIZE: u64 = 1000;

    let mut token = None;
    let mut events = vec![];

    let event_filter = match block {
        MaybePendingBlock::Confirmed(block) => EventFilter {
            from_block: Some(BlockId::Hash(block.block_hash)),
            to_block: Some(BlockId::Hash(block.block_hash)),
            address: None,
            keys: None,
        },
        MaybePendingBlock::Pending { .. } => EventFilter {
            from_block: Some(BlockId::Tag(BlockTag::Pending)),
            to_block: Some(BlockId::Tag(BlockTag::Pending)),
            address: None,
            keys: None,
        },
    };

    loop {
        let mut result = jsonrpc_client
            .get_events(event_filter.clone(), token, CHUNK_SIZE)
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

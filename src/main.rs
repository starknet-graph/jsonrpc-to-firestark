use std::{fs::File, io::Write, path::PathBuf, time::Duration};

use anyhow::Result;
use clap::Parser;
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use sha1::Digest;
use starknet::{
    core::{
        serde::unsigned_field_element::UfeHex,
        types::{
            BlockId, BlockTag, BlockWithReceipts, Felt, MaybePendingBlockWithReceipts,
            PendingBlockWithReceipts, StarknetError, Transaction, TransactionReceipt,
            TransactionWithReceipt,
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
    hash: Felt,
}

enum MaybePendingBlock {
    Confirmed(BlockWithReceipts),
    Pending {
        pseudo_height: u64,
        block: PendingBlockWithReceipts,
    },
}

struct PendingBlockInfo {
    tx_count: usize,
}

impl MaybePendingBlock {
    pub fn parent_hash(&self) -> Felt {
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

    pub fn transactions(&self) -> &[TransactionWithReceipt] {
        match self {
            Self::Confirmed(block) => &block.transactions,
            Self::Pending { block, .. } => &block.transactions,
        }
    }

    pub fn block_hash(&self) -> Felt {
        match self {
            Self::Confirmed(block) => block.block_hash,
            Self::Pending {
                pseudo_height,
                block,
            } => {
                // +--------+-----------+--------------+-------------------+----------+----------+
                // | Fixed  | "PENDING" | Block Number | Transaction Count | Reserved | Checksum |
                // | 1 byte | 7 bytes   | 8 bytes      | 4 bytes           | 8 bytes  | 4 bytes  |
                // +--------+-----------+--------------+-------------------+----------+----------+
                // |  00    |    50     |      NN      |         NN        |    00    |   NN     |
                // |        |    45     |      NN      |         NN        |    00    |   NN     |
                // |        |    4E     |      NN      |         NN        |    00    |   NN     |
                // |        |    44     |      NN      |         NN        |    00    |   NN     |
                // |        |    49     |      NN      |                   |    00    |          |
                // |        |    4E     |      NN      |                   |    00    |          |
                // |        |    47     |      NN      |                   |    00    |          |
                // |        |           |      NN      |                   |    00    |          |
                // +--------+-----------+--------------+-------------------+----------+----------+

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

                let mut hasher = sha1::Sha1::new();
                hasher.update(&buffer[..(8 + 8 + 4 + 8)]);
                buffer[(8 + 8 + 4 + 8)..].copy_from_slice(&hasher.finalize()[0..4]);

                // This cannot fail as the buffer is always in range
                Felt::from_bytes_be(&buffer)
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "jsonrpc_to_firestark=debug");
    }

    env_logger::init();

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
            None => (Felt::ZERO, 0),
        };

        let current_block = match jsonrpc_client
            .get_block_with_receipts(BlockId::Number(current_block_number))
            .await
        {
            Ok(MaybePendingBlockWithReceipts::Block(block)) => {
                // Got a confirmed block. Clear the pending info now
                last_pending_block = None;

                MaybePendingBlock::Confirmed(block)
            }
            Ok(MaybePendingBlockWithReceipts::PendingBlock(_)) => {
                error!("Unexpected pending block");
                tokio::time::sleep(FAILURE_BACKOFF).await;
                continue;
            }
            Err(starknet::providers::ProviderError::StarknetError(
                StarknetError::BlockNotFound,
            )) => {
                // No new block. Try the pending block now
                match jsonrpc_client
                    .get_block_with_receipts(BlockId::Tag(BlockTag::Pending))
                    .await
                {
                    Ok(MaybePendingBlockWithReceipts::Block(_)) => {
                        error!("Unexpected confirmed block");
                        tokio::time::sleep(FAILURE_BACKOFF).await;
                        continue;
                    }
                    Ok(MaybePendingBlockWithReceipts::PendingBlock(block)) => {
                        // Unlike with confirmed blocks, we simply discard non-linkable pending
                        // blocks
                        if block.parent_hash != expected_parent_hash {
                            debug!("Found unlinkable pending block");
                            tokio::time::sleep(HEAD_BACKOFF).await;
                            continue;
                        }

                        if let Some(last_pending_block) = last_pending_block.as_ref() {
                            if last_pending_block.tx_count >= block.transactions.len() {
                                debug!("No newer pending block found");
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
                        error!("Error downloading pending block: {err}");
                        tokio::time::sleep(HEAD_BACKOFF).await;
                        continue;
                    }
                }
            }
            Err(err) => {
                error!("Error downloading confirmed block: {err}");
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

        if let Err(err) = handle_block(&current_block).await {
            error!("Error handling block: {err}");
            tokio::time::sleep(FAILURE_BACKOFF).await;
            continue;
        }

        // Progress log
        match &current_block {
            MaybePendingBlock::Confirmed(block) => {
                info!(
                    "Processed block #{} ({:#064x})",
                    block.block_number, block.block_hash
                );
            }
            MaybePendingBlock::Pending {
                pseudo_height,
                block,
            } => {
                info!(
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
                        error!("Error persisting checkpoint: {err}");
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

async fn handle_block(block: &MaybePendingBlock) -> Result<()> {
    let mut buffer = Vec::new();

    writeln!(&mut buffer, "FIRE BLOCK_BEGIN {}", block.height())?;

    for tx in block.transactions().iter() {
        let (tx_hash, type_str) = match &tx.transaction {
            Transaction::Declare(tx) => (*tx.transaction_hash(), "DECLARE"),
            Transaction::Deploy(tx) => (tx.transaction_hash, "DEPLOY"),
            Transaction::DeployAccount(tx) => (*tx.transaction_hash(), "DEPLOY_ACCOUNT"),
            Transaction::Invoke(tx) => (*tx.transaction_hash(), "INVOKE_FUNCTION"),
            Transaction::L1Handler(tx) => (tx.transaction_hash, "L1_HANDLER"),
        };

        writeln!(&mut buffer, "FIRE BEGIN_TRX {tx_hash:#064x} {type_str}")?;

        let tx_events = match &tx.receipt {
            TransactionReceipt::Invoke(receipt) => &receipt.events,
            TransactionReceipt::L1Handler(receipt) => &receipt.events,
            TransactionReceipt::Declare(receipt) => &receipt.events,
            TransactionReceipt::Deploy(receipt) => &receipt.events,
            TransactionReceipt::DeployAccount(receipt) => &receipt.events,
        };

        for (ind_event, event) in tx_events.iter().enumerate() {
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

fn try_persist_checkpoint(path: &PathBuf, checkpoint: &Checkpoint) -> Result<()> {
    let temp_path = PathBuf::from(&format!("{}.tmp", path.to_string_lossy()));

    serde_json::to_writer(&mut File::create(&temp_path)?, &checkpoint)?;
    std::fs::rename(temp_path, path)?;

    Ok(())
}

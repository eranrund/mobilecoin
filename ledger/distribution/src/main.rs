// Copyright (c) 2018-2020 MobileCoin Inc.

//! A helper utility for collecting blocks from a local ledger file and storing them as
//! Protobuf-serialized files on S3.

pub mod uri;

use crate::uri::{Destination, Uri};
use mc_api::{block_num_to_s3block_path, blockchain, merged_block_num_to_s3block_path};
use mc_common::logger::{create_app_logger, log, o, Logger};
use mc_ledger_db::{Error as LedgerDbError, Ledger, LedgerDB};
use mc_transaction_core::{Block, BlockContents, BlockIndex, BlockSignature};
use protobuf::{Message, RepeatedField};
use rusoto_core::{Region, RusotoError};
use rusoto_s3::{PutObjectError, PutObjectRequest, S3Client, S3};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, str::FromStr};
use structopt::StructOpt;

/// Bucket sizes for producing merged archived blocks (files that store multiple consecutive blocks).
pub const MERGE_BUCKET_SIZES: &[usize] = &[10000];
//pub const MERGE_BUCKET_SIZES: &[usize] = &[100, 1000];

fn create_archive_block(
    block: &Block,
    block_contents: &BlockContents,
    signature: &Option<BlockSignature>,
) -> blockchain::ArchiveBlock {
    let bc_block = blockchain::Block::from(block);
    let bc_block_contents = blockchain::BlockContents::from(block_contents);

    let mut archive_block_v1 = blockchain::ArchiveBlockV1::new();
    archive_block_v1.set_block(bc_block);
    archive_block_v1.set_block_contents(bc_block_contents);

    if let Some(signature) = signature {
        let bc_signature = blockchain::BlockSignature::from(signature);
        archive_block_v1.set_signature(bc_signature);
    }

    let mut archive_block = blockchain::ArchiveBlock::new();
    archive_block.set_v1(archive_block_v1);

    archive_block
}

pub trait BlockWriter {
    fn write_single_block(
        &mut self,
        block: &Block,
        block_contents: &BlockContents,
        signature: &Option<BlockSignature>,
    );

    fn write_multiple_blocks(&mut self, blocks: &[(Block, BlockContents, Option<BlockSignature>)]);
}

/// Block to start syncing from.
#[derive(Clone, Debug)]
pub enum StartFrom {
    /// Start from the origin block.
    Zero,

    /// Sync new blocks only, skipping all blocks initially in the ledger.
    Next,

    /// Start from the last block we successfully synced (stored inside a state file).
    Last,
}

impl FromStr for StartFrom {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "zero" => Ok(Self::Zero),
            "next" => Ok(Self::Next),
            "last" => Ok(Self::Last),
            _ => Err("Unknown value, valid values are zero/next/last".into()),
        }
    }
}

#[derive(Clone, Debug, StructOpt)]
#[structopt(
    name = "ledger_distribution",
    about = "The MobileCoin Ledger Distribution Service."
)]
pub struct Config {
    /// Path to local LMDB db file.
    #[structopt(long, parse(from_os_str))]
    pub ledger_path: PathBuf,

    /// Destination to upload to.
    #[structopt(long = "dest")]
    pub destination: Uri,

    /// Block to start from.
    #[structopt(long, default_value = "zero")]
    pub start_from: StartFrom,

    /// State file, defaults to ~/.mc-ledger-distribution-state
    #[structopt(long)]
    pub state_file: Option<PathBuf>,
}

/// State file contents.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct StateData {
    next_block: BlockIndex,
}

/// S3 block writer.
pub struct S3BlockWriter {
    path: PathBuf,
    s3_client: S3Client,
    logger: Logger,
}

impl S3BlockWriter {
    fn new(path: PathBuf, region: Region, logger: Logger) -> S3BlockWriter {
        log::debug!(
            logger,
            "Creating S3 Block Writer with path={:?} region={:?}",
            path,
            region
        );

        let s3_client = S3Client::new(region);
        S3BlockWriter {
            path,
            s3_client,
            logger,
        }
    }

    fn write_bytes_to_s3(&self, path: &str, filename: &str, value: &[u8]) {
        let result: Result<
            retry::OperationResult<(), ()>,
            retry::Error<retry::OperationResult<(), RusotoError<PutObjectError>>>,
        > = retry::retry(
            retry::delay::Exponential::from_millis(10).map(retry::delay::jitter),
            || {
                let req = PutObjectRequest {
                    bucket: path.to_string(),
                    key: String::from(filename),
                    body: Some(value.to_vec().into()),
                    acl: Some("public-read".to_string()),
                    ..Default::default()
                };

                self.s3_client
                    .put_object(req)
                    .sync()
                    .map(|_| retry::OperationResult::Ok(()))
                    .map_err(|err: RusotoError<PutObjectError>| {
                        log::warn!(
                            self.logger,
                            "Failed writing {}: {:?}, retrying...",
                            filename,
                            err
                        );
                        retry::OperationResult::Retry(err)
                    })
            },
        );

        // We should always succeed since retrying should never stop until that happens.
        assert!(result.is_ok());
    }
}

impl BlockWriter for S3BlockWriter {
    fn write_single_block(
        &mut self,
        block: &Block,
        block_contents: &BlockContents,
        signature: &Option<BlockSignature>,
    ) {
        log::info!(self.logger, "S3: Writing block {}", block.index);

        let archive_block = create_archive_block(block, block_contents, signature);

        let dest = self
            .path
            .as_path()
            .join(block_num_to_s3block_path(block.index));

        let dir = dest.as_path().parent().expect("failed getting parent");
        let filename = dest.file_name().unwrap();

        self.write_bytes_to_s3(
            dir.to_str().unwrap(),
            filename.to_str().unwrap(),
            &archive_block
                .write_to_bytes()
                .expect("failed to serialize ArchiveBlock"),
        );
    }

    fn write_multiple_blocks(&mut self, blocks: &[(Block, BlockContents, Option<BlockSignature>)]) {
        let num_blocks = blocks.len() as u64;
        assert!(num_blocks > 1);

        // Sanity: blocks must be consecutive
        for i in 1..num_blocks as usize {
            assert_eq!(blocks[i].0.index, blocks[i - 1].0.index + 1);
        }

        log::info!(
            self.logger,
            "Local: Writing blocks {}-{}",
            blocks[0].0.index,
            blocks[0].0.index + num_blocks
        );

        let mut archive_blocks = blockchain::ArchiveBlocks::new();
        archive_blocks.set_blocks(RepeatedField::from_vec(
            blocks
                .into_iter()
                .map(|(block, block_contents, signature)| {
                    create_archive_block(block, block_contents, signature)
                })
                .collect(),
        ));

        let bytes = archive_blocks
            .write_to_bytes()
            .expect("failed to serialize ArchiveBlocks");

        let dest = self.path.as_path().join(merged_block_num_to_s3block_path(
            blocks.len(),
            blocks[0].0.index,
        ));

        let dir = dest.as_path().parent().expect("failed getting parent");
        let filename = dest.file_name().unwrap();

        self.write_bytes_to_s3(dir.to_str().unwrap(), filename.to_str().unwrap(), &bytes);
    }
}

/// Local directory block writer.
pub struct LocalBlockWriter {
    path: PathBuf,
    logger: Logger,
}

impl LocalBlockWriter {
    fn new(path: PathBuf, logger: Logger) -> LocalBlockWriter {
        log::debug!(logger, "Creating Local Block Writer with path={:?}", path,);

        LocalBlockWriter { path, logger }
    }
}

impl BlockWriter for LocalBlockWriter {
    fn write_single_block(
        &mut self,
        block: &Block,
        block_contents: &BlockContents,
        signature: &Option<BlockSignature>,
    ) {
        log::info!(self.logger, "Local: Writing block {}", block.index);

        let archive_block = create_archive_block(block, block_contents, signature);

        let bytes = archive_block
            .write_to_bytes()
            .expect("failed to serialize ArchiveBlock");

        let dest = self
            .path
            .as_path()
            .join(block_num_to_s3block_path(block.index));
        let dir = dest.as_path().parent().expect("failed getting parent");

        fs::create_dir_all(dir)
            .unwrap_or_else(|e| panic!("failed creating directory {:?}: {:?}", dir, e));
        fs::write(&dest, bytes)
            .unwrap_or_else(|_| panic!("failed writing block #{} to {:?}", block.index, dest));
    }

    fn write_multiple_blocks(&mut self, blocks: &[(Block, BlockContents, Option<BlockSignature>)]) {
        let num_blocks = blocks.len() as u64;
        assert!(num_blocks > 1);

        // Sanity: blocks must be consecutive
        for i in 1..num_blocks as usize {
            assert_eq!(blocks[i].0.index, blocks[i - 1].0.index + 1);
        }

        log::info!(
            self.logger,
            "Local: Writing blocks {}-{}",
            blocks[0].0.index,
            blocks[0].0.index + num_blocks
        );

        let mut archive_blocks = blockchain::ArchiveBlocks::new();
        archive_blocks.set_blocks(RepeatedField::from_vec(
            blocks
                .into_iter()
                .map(|(block, block_contents, signature)| {
                    create_archive_block(block, block_contents, signature)
                })
                .collect(),
        ));

        let bytes = archive_blocks
            .write_to_bytes()
            .expect("failed to serialize ArchiveBlocks");

        let dest = self
            .path
            .as_path()
            .join(format!("merged-{}", blocks.len()))
            .join(block_num_to_s3block_path(blocks[0].0.index));
        let dir = dest.as_path().parent().expect("failed getting parent");

        fs::create_dir_all(dir)
            .unwrap_or_else(|e| panic!("failed creating directory {:?}: {:?}", dir, e));
        fs::write(&dest, bytes).unwrap_or_else(|_| {
            panic!(
                "failed writing blocks #{}-#{} to {:?}",
                blocks[0].0.index,
                blocks[0].0.index + num_blocks,
                dest
            )
        });
    }
}

// Implements the ledger db polling loop
fn main() {
    let config = Config::from_args();

    mc_common::setup_panic_handler();
    let _sentry_guard = mc_common::sentry::init();
    let (logger, _global_logger_guard) = create_app_logger(o!());

    // Get path to our state file.
    let state_file_path = config.state_file.clone().unwrap_or_else(|| {
        let mut home_dir = dirs::home_dir().unwrap_or_else(|| panic!("Unable to get home directory, please specify state file explicitly with --state-file"));
        home_dir.push(".mc-ledger-distribution-state");
        home_dir
    });

    log::info!(logger, "State file is {:?}", state_file_path);

    // Open ledger
    log::info!(logger, "Opening ledger db {:?}", config.ledger_path);
    let ledger_db = LedgerDB::open(config.ledger_path.clone()).expect("Could not read ledger DB");

    // Figure out the first block to sync from.
    let first_desired_block = match config.start_from {
        StartFrom::Zero => 0,
        StartFrom::Next => {
            // See if the state file exists and read it if it does.
            if state_file_path.as_path().exists() {
                let file_data = fs::read_to_string(&state_file_path).unwrap_or_else(|e| {
                    panic!("Failed reading state file {:?}: {:?}", state_file_path, e)
                });
                let state_data: StateData = serde_json::from_str(&file_data).unwrap_or_else(|e| {
                    panic!("Failed parsing state file {:?}: {:?}", state_file_path, e)
                });
                state_data.next_block
            } else {
                0
            }
        }
        StartFrom::Last => ledger_db
            .num_blocks()
            .expect("Failed getting number of blocks in ledger"),
    };

    // Create block writer
    let mut block_writer: Box<dyn BlockWriter> = match config.destination.destination {
        Destination::S3 { path, region } => {
            Box::new(S3BlockWriter::new(path, region, logger.clone()))
        }

        Destination::Local { path } => {
            fs::create_dir_all(&path).unwrap_or_else(|_| {
                panic!("Failed creating local destination directory {:?}", path)
            });
            Box::new(LocalBlockWriter::new(path, logger.clone()))
        }
    };

    // Poll ledger for new blocks and process them as they come.
    log::info!(
        logger,
        "Polling for blocks, starting at {}...",
        first_desired_block
    );
    let mut next_block_num = first_desired_block;
    loop {
        while let (Ok(_block_contents), Ok(block)) = (
            ledger_db.get_block_contents(next_block_num),
            ledger_db.get_block(next_block_num),
        ) {
            log::trace!(logger, "Writing block #{}", next_block_num);

            let _signature = match ledger_db.get_block_signature(next_block_num) {
                Ok(signature) => Some(signature),
                Err(LedgerDbError::NotFound) => None,
                Err(err) => {
                    log::error!(
                        logger,
                        "Failed getting signature for block #{}: {:?}",
                        next_block_num,
                        err
                    );
                    None
                }
            };

            // block_writer.write_single_block(&block, &block_contents, &signature);

            for bucket_size in MERGE_BUCKET_SIZES {
                if (block.index as usize + 1) % bucket_size != 0 {
                    continue;
                }

                let first_block_index = block.index + 1 - *bucket_size as u64;
                let last_block_index = block.index;

                log::debug!(
                    logger,
                    "Preparing to write merged block #{}-{}",
                    first_block_index,
                    last_block_index
                );

                let mut blocks_data = Vec::new();
                for block_index in first_block_index..=last_block_index {
                    // We panic here since this block and its associated data is expected to be in the ledger
                    // due to block_index < next_block_num (which we successfully fetched or otherwise
                    // this code wouldn't be running).
                    let block = ledger_db.get_block(block_index).unwrap_or_else(|err| {
                        panic!("failed getting block #{}: {}", block_index, err)
                    });
                    let block_contents =
                        ledger_db
                            .get_block_contents(block_index)
                            .unwrap_or_else(|err| {
                                panic!("failed getting block contents #{}: {}", block_index, err)
                            });
                    let block_signature = match ledger_db.get_block_signature(next_block_num) {
                        Ok(signature) => Some(signature),
                        Err(LedgerDbError::NotFound) => None,
                        Err(err) => {
                            panic!(
                                "Failed getting signature for block #{}: {:?}",
                                block_index, err
                            );
                        }
                    };

                    blocks_data.push((block, block_contents, block_signature));
                }

                block_writer.write_multiple_blocks(&blocks_data);
            }

            next_block_num += 1;

            let state = StateData {
                next_block: next_block_num,
            };
            let json_data = serde_json::to_string(&state).expect("failed serializing state data");
            fs::write(&state_file_path, json_data).expect("failed writing state file");
        }

        // TODO: make this configurable
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

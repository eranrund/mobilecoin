// Copyright (c) 2018-2020 MobileCoin Inc.

//! Implementation of the `TransactionsFetcher` trait that fetches transactions data over http(s)
//! using the `reqwest` library. It can be used, for example, to get transaction data from S3.

use crate::transactions_fetcher_trait::{TransactionFetcherError, TransactionsFetcher};
use failure::Fail;
use mc_api::{block_num_to_s3block_path, blockchain, merged_block_num_to_s3block_path};
use mc_common::{
    logger::{log, Logger},
    lru::LruCache,
    ResponderId,
};
use mc_transaction_core::{compute_block_id, Block, BlockContents, BlockID, BlockSignature};
use protobuf::Message;
use reqwest::Error as ReqwestError;
use serde::{Deserialize, Serialize};
use std::{
    convert::TryFrom,
    fs,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use url::Url;

#[derive(Debug, Fail)]
pub enum ReqwestTransactionsFetcherError {
    #[fail(display = "Url parse error on {}: {}", _0, _1)]
    UrlParse(String, url::ParseError),

    #[fail(display = "reqwest error on {}: {:?}", _0, _1)]
    ReqwestError(String, ReqwestError),

    #[fail(display = "IO error on {}: {:?}", _0, _1)]
    IO(String, std::io::Error),

    #[fail(display = "Received an invalid block from {}: {}", _0, _1)]
    InvalidBlockReceived(String, String),

    #[fail(display = "Block not found at {}", _0)]
    BlockNotFound(String),

    #[fail(display = "No URLs configured.")]
    NoUrlsConfigured,
}

impl From<ReqwestError> for ReqwestTransactionsFetcherError {
    fn from(src: ReqwestError) -> Self {
        ReqwestTransactionsFetcherError::ReqwestError(
            String::from(src.url().map_or("", |v| v.as_str())),
            src,
        )
    }
}

impl TransactionFetcherError for ReqwestTransactionsFetcherError {}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ArchiveBlockData {
    pub block: Block,
    pub block_contents: BlockContents,
    pub signature: Option<BlockSignature>,
}

impl TryFrom<(&Url, &blockchain::ArchiveBlock)> for ArchiveBlockData {
    type Error = ReqwestTransactionsFetcherError;

    fn try_from(src: (&Url, &blockchain::ArchiveBlock)) -> Result<Self, Self::Error> {
        let (url, archive_block) = src;

        if !archive_block.has_v1() {
            return Err(ReqwestTransactionsFetcherError::InvalidBlockReceived(
                url.to_string(),
                "v1 block not present".to_owned(),
            ));
        }

        let block = Block::try_from(archive_block.get_v1().get_block()).map_err(|err| {
            ReqwestTransactionsFetcherError::InvalidBlockReceived(
                url.to_string(),
                format!("Block conversion failed: {:?}", err),
            )
        })?;

        let block_contents = BlockContents::try_from(archive_block.get_v1().get_block_contents())
            .map_err(|err| {
            ReqwestTransactionsFetcherError::InvalidBlockReceived(
                url.to_string(),
                format!("Block contents conversion failed: {:?}", err),
            )
        })?;

        let signature = archive_block
            .get_v1()
            .signature
            .as_ref()
            .map(BlockSignature::try_from)
            .transpose()
            .map_err(|err| {
                ReqwestTransactionsFetcherError::InvalidBlockReceived(
                    url.to_string(),
                    format!("Invalid block signature: {:?}", err),
                )
            })?;

        if let Some(signature) = signature.as_ref() {
            signature.verify(&block).map_err(|err| {
                ReqwestTransactionsFetcherError::InvalidBlockReceived(
                    url.to_string(),
                    format!("Unable to verify block signature: {:?}", err),
                )
            })?;
        }

        if block.contents_hash != block_contents.hash() {
            return Err(ReqwestTransactionsFetcherError::InvalidBlockReceived(
                url.to_string(),
                format!(
                    "Invalid block contents hash. Block: {:?}, BlockContents: {:?}",
                    block, block_contents
                ),
            ));
        }

        Ok(ArchiveBlockData {
            block,
            block_contents,
            signature,
        })
    }
}

#[derive(Clone)]
pub struct ReqwestTransactionsFetcher {
    pub source_urls: Vec<Url>,
    client: reqwest::blocking::Client,
    logger: Logger,
    source_index_counter: Arc<AtomicU64>,
    blocks_cache: Arc<Mutex<LruCache<BlockID, ArchiveBlockData>>>,
    hits: Arc<AtomicU64>,
    misses: Arc<AtomicU64>,
}

impl ReqwestTransactionsFetcher {
    pub fn new(
        source_urls: Vec<String>,
        logger: Logger,
    ) -> Result<Self, ReqwestTransactionsFetcherError> {
        Self::new_with_client(source_urls, reqwest::blocking::Client::new(), logger)
    }

    pub fn new_with_client(
        source_urls: Vec<String>,
        client: reqwest::blocking::Client,
        logger: Logger,
    ) -> Result<Self, ReqwestTransactionsFetcherError> {
        let source_urls = source_urls
            .into_iter()
            // All source_urls must end with a '/'
            .map(|mut url| {
                if !url.ends_with('/') {
                    url.push_str("/");
                }

                url
            })
            // Parse into a Url object
            .map(|url| {
                Url::parse(&url).map_err(|err| ReqwestTransactionsFetcherError::UrlParse(url, err))
            })
            .collect::<Result<Vec<Url>, ReqwestTransactionsFetcherError>>()?;

        let obj = Self {
            source_urls,
            client,
            logger,
            source_index_counter: Arc::new(AtomicU64::new(0)),
            blocks_cache: Arc::new(Mutex::new(LruCache::new(100000))), // TODO constant
            hits: Arc::new(AtomicU64::new(0)),
            misses: Arc::new(AtomicU64::new(0)),
        };

        /*        let obj2 = obj.clone();
        let _ = std::thread::spawn(move || {
            println!("THREAD STARTED!!!!");
            let mut idx = 0;
            loop {
                let source_url = &obj2.source_urls[0];
                let bucket = 1000;
                {
                    log::crit!(obj2.logger, "attempting prefetch {}", idx);
                    let filename = block_num_to_s3block_path(idx)
                        .into_os_string()
                        .into_string()
                        .unwrap();
                    let url = source_url
                        .join(&format!("merged-{}/{}", bucket, filename))
                        .map_err(|e| ReqwestTransactionsFetcherError::UrlParse(filename.clone(), e))
                        .unwrap();

                    if let Ok(blocks_data) = obj2.blocks_from_url(&url) {
                        log::crit!(obj2.logger, "Cache prefetch got {}", blocks_data.len());
                        idx += blocks_data.len() as u64;
                        let mut cache2 = obj2.cache2.lock().expect("lock poisoned");
                        for block_data in blocks_data.into_iter() {
                            cache2.insert(block_data.block.index, block_data);
                        }
                    } else {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
            }
        }); */

        Ok(obj)
    }

    // Fetches a single block from a given url.
    pub fn block_from_url(
        &self,
        url: &Url,
    ) -> Result<ArchiveBlockData, ReqwestTransactionsFetcherError> {
        let archive_block: blockchain::ArchiveBlock = self.fetch_protobuf_object(url)?;
        ArchiveBlockData::try_from((url, &archive_block))
    }

    // Fetches multiple blocks (a "merged block") from a given url.
    pub fn blocks_from_url(
        &self,
        url: &Url,
    ) -> Result<Vec<ArchiveBlockData>, ReqwestTransactionsFetcherError> {
        let archive_blocks: blockchain::ArchiveBlocks = self.fetch_protobuf_object(url)?;

        let results = archive_blocks
            .get_blocks()
            .into_iter()
            .map(|archive_block| ArchiveBlockData::try_from((url, archive_block)))
            .collect::<Result<Vec<_>, ReqwestTransactionsFetcherError>>()?;

        // Ensure results form a legitimate chain of blocks.
        for i in 1..results.len() {
            let parent_block = &results[i - 1].block;
            let block = &results[i].block;

            let expected_block_id = compute_block_id(
                block.version,
                &parent_block.id,
                block.index,
                block.cumulative_txo_count,
                &block.root_element,
                &block.contents_hash,
            );
            if expected_block_id != block.id {
                return Err(ReqwestTransactionsFetcherError::InvalidBlockReceived(
                    url.to_string(),
                    format!(
                        "block id mismatch for block #{} ({:?} != {:?})",
                        block.index, expected_block_id, block.id
                    ),
                ));
            }
        }

        Ok(results)
    }

    pub fn get_origin_block_and_transactions(
        &self,
    ) -> Result<(Block, BlockContents), ReqwestTransactionsFetcherError> {
        let source_url = &self
            .source_urls
            .get(0)
            .ok_or(ReqwestTransactionsFetcherError::NoUrlsConfigured)?;
        let filename = block_num_to_s3block_path(0)
            .into_os_string()
            .into_string()
            .unwrap();
        let url = source_url.join(&filename).unwrap();
        let s3block = self.block_from_url(&url)?;

        Ok((s3block.block, s3block.block_contents))
    }

    fn fetch_protobuf_object<M: Message>(
        &self,
        url: &Url,
    ) -> Result<M, ReqwestTransactionsFetcherError> {
        // Special treatment for file:// to read from a local directory.
        let bytes: Vec<u8> = if url.scheme() == "file" {
            let path = &url[url::Position::BeforeHost..url::Position::AfterPath];
            fs::read(path)
                .map_err(|err| ReqwestTransactionsFetcherError::IO(path.to_string(), err))?
                .to_vec()
        } else {
            let mut response = self.client.get(url.as_str()).send().map_err(|err| {
                ReqwestTransactionsFetcherError::ReqwestError(url.to_string(), err)
            })?;
            if response.status() == 404 {
                return Err(ReqwestTransactionsFetcherError::BlockNotFound(
                    url.to_string(),
                ));
            }

            let mut bytes = Vec::new();
            response.copy_to(&mut bytes)?;
            bytes
        };

        let obj: M = protobuf::parse_from_bytes(&bytes).map_err(|err| {
            ReqwestTransactionsFetcherError::InvalidBlockReceived(
                url.to_string(),
                format!("protobuf parse failed: {:?}", err),
            )
        })?;

        Ok(obj)
    }
}

impl TransactionsFetcher for ReqwestTransactionsFetcher {
    type Error = ReqwestTransactionsFetcherError;

    fn get_block_contents(
        &self,
        _safe_responder_ids: &[ResponderId],
        block: &Block,
    ) -> Result<BlockContents, Self::Error> {
        // Try and see if we can get this block from our cache.
        {
            let mut blocks_cache = self.blocks_cache.lock().expect("mutex poisoned");

            // Note: If this block id is in the cache, we take it out under the assumption that our
            // primary caller, LedgerSyncService, is not going to try and fetch the same block
            // twice if it managed to get a valid block.
            if let Some(block_data) = blocks_cache.pop(&block.id) {
                if block_data.block == *block {
                    let hits = self.hits.fetch_add(1, Ordering::SeqCst);
                    let misses = self.misses.load(Ordering::SeqCst);
                    log::crit!(self.logger, "{} / {}", hits, misses);
                    return Ok(block_data.block_contents.clone());
                }
            }
        }

        // Get the source to fetch from.
        let source_index_counter =
            self.source_index_counter.fetch_add(1, Ordering::SeqCst) as usize;
        let source_url = &self.source_urls[source_index_counter % self.source_urls.len()];

        // TODO bucket sizes need to live somewhere that isn't here.
        for bucket in &[10000, 1000, 100] {
            if block.index % bucket == 0 {
                log::debug!(
                    self.logger,
                    "Attempting to fetch a merged block for #{} (bucket size {})",
                    block.index,
                    bucket
                );
                let filename = merged_block_num_to_s3block_path(*bucket as usize, block.index)
                    .into_os_string()
                    .into_string()
                    .unwrap();
                let url = source_url
                    .join(&filename)
                    .map_err(|e| ReqwestTransactionsFetcherError::UrlParse(filename.clone(), e))?;

                if let Ok(blocks_data) = self.blocks_from_url(&url) {
                    log::debug!(
                        self.logger,
                        "Got a merged block for #{} (bucket size {}): {} entries",
                        block.index,
                        bucket,
                        blocks_data.len()
                    );

                    let mut blocks_cache = self.blocks_cache.lock().expect("mutex poisoned");
                    for block_data in blocks_data.into_iter() {
                        blocks_cache.put(block_data.block.id.clone(), block_data);
                    }
                    break;
                }
            }
        }

        // Construct URL for the block we are trying to fetch.
        let filename = block_num_to_s3block_path(block.index)
            .into_os_string()
            .into_string()
            .unwrap();
        let url = source_url
            .join(&filename)
            .map_err(|e| ReqwestTransactionsFetcherError::UrlParse(filename, e))?;

        // Try and get the block.
        log::debug!(
            self.logger,
            "Attempting to fetch block {} from {}",
            block.index,
            url
        );

        let s3_block_data = self.block_from_url(&url)?;

        // Check that we received data for the block we actually asked about.
        if *block != s3_block_data.block {
            return Err(ReqwestTransactionsFetcherError::InvalidBlockReceived(
                url.to_string(),
                "block data mismatch".to_string(),
            ));
        }

        let hits = self.hits.load(Ordering::SeqCst);
        let misses = self.misses.fetch_add(1, Ordering::SeqCst);
        log::crit!(self.logger, "{} / {}", hits, misses);

        // Got what we wanted!
        Ok(s3_block_data.block_contents)
    }
}

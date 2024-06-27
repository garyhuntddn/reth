//! Handler that can download blocks on demand (e.g. from the network).

use crate::{engine::DownloadRequest, metrics::BlockDownloaderMetrics};
use futures::FutureExt;
use reth_beacon_consensus::EthBeaconConsensus;
use reth_chainspec::ChainSpec;
use reth_consensus::Consensus;
use reth_network_p2p::{
    bodies::client::BodiesClient,
    full_block::{FetchFullBlockFuture, FetchFullBlockRangeFuture, FullBlockClient},
    headers::client::HeadersClient,
};
use reth_primitives::{SealedBlock, SealedBlockWithSenders, B256};
use std::{
    cmp::{Ordering, Reverse},
    collections::{binary_heap::PeekMut, BinaryHeap, HashSet},
    sync::Arc,
    task::{Context, Poll},
};
use tracing::trace;

/// A trait that can download blocks on demand.
pub trait BlockDownloader: Send + Sync {
    /// Handle an action.
    fn on_action(&mut self, event: DownloadAction);

    /// Advance in progress requests if any
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<DownloadOutcome>;
}

/// Actions that can be performed by the block downloader.
#[derive(Debug)]
pub enum DownloadAction {
    /// Stop downloading blocks.
    Clear,
    /// Download given blocks
    Download(DownloadRequest),
}

/// Outcome of downloaded blocks.
#[derive(Debug)]
pub enum DownloadOutcome {
    /// Downloaded blocks.
    Blocks(Vec<SealedBlockWithSenders>),
}

/// Basic [BlockDownloader].
pub struct BasicBlockDownloader<Client>
where
    Client: HeadersClient + BodiesClient + Clone + Unpin + 'static,
{
    /// A downloader that can download full blocks from the network.
    full_block_client: FullBlockClient<Client>,
    /// In-flight full block requests in progress.
    inflight_full_block_requests: Vec<FetchFullBlockFuture<Client>>,
    /// In-flight full block _range_ requests in progress.
    inflight_block_range_requests: Vec<FetchFullBlockRangeFuture<Client>>,
    /// Buffered blocks from downloads - this is a min-heap of blocks, using the block number for
    /// ordering. This means the blocks will be popped from the heap with ascending block numbers.
    set_buffered_blocks: BinaryHeap<Reverse<OrderedSealedBlockWithSenders>>,
    /// Engine download metrics.
    metrics: BlockDownloaderMetrics,
}

impl<Client> BasicBlockDownloader<Client>
where
    Client: HeadersClient + BodiesClient + Clone + Unpin + 'static,
{
    /// Create a new instance
    pub(crate) fn new(client: Client, consensus: Arc<dyn Consensus>) -> Self {
        Self {
            full_block_client: FullBlockClient::new(client, consensus),
            inflight_full_block_requests: Vec::new(),
            inflight_block_range_requests: Vec::new(),
            set_buffered_blocks: BinaryHeap::new(),
            metrics: BlockDownloaderMetrics::default(),
        }
    }

    /// Clears the stored inflight requests.
    fn clear(&mut self) {
        self.inflight_full_block_requests.clear();
        self.inflight_block_range_requests.clear();
        self.set_buffered_blocks.clear();
        self.update_block_download_metrics();
    }

    /// Processes a download request.
    fn download(&mut self, request: DownloadRequest) {
        match request {
            DownloadRequest::BlockSet(hashes) => self.download_block_set(hashes),
            DownloadRequest::BlockRange(hash, count) => self.download_block_range(hash, count),
        }
    }

    /// Processes a block set download request.
    fn download_block_set(&mut self, hashes: HashSet<B256>) {
        for hash in hashes {
            self.download_full_block(hash);
        }
    }

    /// Processes a block range download request.
    fn download_block_range(&mut self, hash: B256, count: u64) {
        if count == 1 {
            self.download_full_block(hash);
        } else {
            trace!(
                target: "consensus::engine",
                ?hash,
                ?count,
                "start downloading full block range."
            );

            let request = self.full_block_client.get_full_block_range(hash, count);
            self.inflight_block_range_requests.push(request);
        }
    }

    /// Starts requesting a full block from the network.
    ///
    /// Returns `true` if the request was started, `false` if there's already a request for the
    /// given hash.
    fn download_full_block(&mut self, hash: B256) -> bool {
        if self.is_inflight_request(hash) {
            return false
        }
        trace!(
            target: "consensus::engine::sync",
            ?hash,
            "Start downloading full block"
        );

        let request = self.full_block_client.get_full_block(hash);
        self.inflight_full_block_requests.push(request);

        self.update_block_download_metrics();

        true
    }

    /// Returns true if there's already a request for the given hash.
    fn is_inflight_request(&self, hash: B256) -> bool {
        self.inflight_full_block_requests.iter().any(|req| *req.hash() == hash)
    }

    /// Sets the metrics for the active downloads
    fn update_block_download_metrics(&self) {
        self.metrics.active_block_downloads.set(self.inflight_full_block_requests.len() as f64);
        // TODO: full block range metrics
    }
}

impl<Client> BlockDownloader for BasicBlockDownloader<Client>
where
    Client: HeadersClient + BodiesClient + Clone + Unpin + 'static,
{
    /// Handles incoming download actions.
    fn on_action(&mut self, event: DownloadAction) {
        match event {
            DownloadAction::Clear => self.clear(),
            DownloadAction::Download(request) => self.download(request),
        }
    }

    /// Advances the download process.
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<DownloadOutcome> {
        // advance all full block requests
        for idx in (0..self.inflight_full_block_requests.len()).rev() {
            let mut request = self.inflight_full_block_requests.swap_remove(idx);
            if let Poll::Ready(block) = request.poll_unpin(cx) {
                trace!(target: "consensus::engine", block=?block.num_hash(), "Received single full block, buffering");
                self.set_buffered_blocks.push(Reverse(block.into()));
            } else {
                // still pending
                self.inflight_full_block_requests.push(request);
            }
        }

        // advance all full block range requests
        for idx in (0..self.inflight_block_range_requests.len()).rev() {
            let mut request = self.inflight_block_range_requests.swap_remove(idx);
            if let Poll::Ready(blocks) = request.poll_unpin(cx) {
                trace!(target: "consensus::engine", len=?blocks.len(), first=?blocks.first().map(|b| b.num_hash()), last=?blocks.last().map(|b| b.num_hash()), "Received full block range, buffering");
                self.set_buffered_blocks.extend(
                    blocks
                        .into_iter()
                        .map(|b| {
                            let senders = b.senders().unwrap_or_default();
                            OrderedSealedBlockWithSenders(SealedBlockWithSenders {
                                block: b,
                                senders,
                            })
                        })
                        .map(Reverse),
                );
            } else {
                // still pending
                self.inflight_block_range_requests.push(request);
            }
        }

        self.update_block_download_metrics();

        if self.set_buffered_blocks.is_empty() {
            return Poll::Pending;
        }

        // drain all unique element of the block buffer if there are any
        let mut downloaded_blocks: Vec<SealedBlockWithSenders> =
            Vec::with_capacity(self.set_buffered_blocks.len());
        while let Some(block) = self.set_buffered_blocks.pop() {
            // peek ahead and pop duplicates
            while let Some(peek) = self.set_buffered_blocks.peek_mut() {
                if peek.0 .0.hash() == block.0 .0.hash() {
                    PeekMut::pop(peek);
                } else {
                    break
                }
            }
            downloaded_blocks.push(block.0.into());
        }
        Poll::Ready(DownloadOutcome::Blocks(downloaded_blocks))
    }
}

/// A wrapper type around [`SealedBlockWithSenders`] that implements the [Ord]
/// trait by block number.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OrderedSealedBlockWithSenders(SealedBlockWithSenders);

impl PartialOrd for OrderedSealedBlockWithSenders {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedSealedBlockWithSenders {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.number.cmp(&other.0.number)
    }
}

impl From<SealedBlock> for OrderedSealedBlockWithSenders {
    fn from(block: SealedBlock) -> Self {
        let senders = block.senders().unwrap_or_default();
        Self(SealedBlockWithSenders { block, senders })
    }
}

impl From<OrderedSealedBlockWithSenders> for SealedBlockWithSenders {
    fn from(value: OrderedSealedBlockWithSenders) -> Self {
        let senders = value.0.senders;
        Self { block: value.0.block, senders }
    }
}

/// A [BlockDownloader] that does nothing.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct NoopBlockDownloader;

impl BlockDownloader for NoopBlockDownloader {
    fn on_action(&mut self, _event: DownloadAction) {}

    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<DownloadOutcome> {
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use reth_chainspec::{ChainSpecBuilder, MAINNET};
    use reth_network_p2p::test_utils::TestFullBlockClient;
    use reth_primitives::{constants::ETHEREUM_BLOCK_GAS_LIMIT, BlockBody, Header, SealedHeader};
    use std::{future::poll_fn, ops::Range, sync::Arc};

    struct TestHarness {
        block_downloader: BasicBlockDownloader<TestFullBlockClient>,
        client: TestFullBlockClient,
    }

    impl TestHarness {
        fn new(total_blocks: usize) -> Self {
            let chain_spec = Arc::new(
                ChainSpecBuilder::default()
                    .chain(MAINNET.chain)
                    .genesis(MAINNET.genesis.clone())
                    .paris_activated()
                    .build(),
            );

            let client = TestFullBlockClient::default();
            let header = Header {
                base_fee_per_gas: Some(7),
                gas_limit: ETHEREUM_BLOCK_GAS_LIMIT,
                ..Default::default()
            }
            .seal_slow();

            insert_headers_into_client(&client, header, 0..total_blocks);
            let consensus = Arc::new(EthBeaconConsensus::new(chain_spec));

            let block_downloader = BasicBlockDownloader::new(client.clone(), consensus);
            Self { block_downloader, client }
        }
    }

    fn insert_headers_into_client(
        client: &TestFullBlockClient,
        genesis_header: SealedHeader,
        range: Range<usize>,
    ) {
        let mut sealed_header = genesis_header;
        let body = BlockBody::default();
        for _ in range {
            let (mut header, hash) = sealed_header.split();
            // update to the next header
            header.parent_hash = hash;
            header.number += 1;
            header.timestamp += 1;
            sealed_header = header.seal_slow();
            client.insert(sealed_header.clone(), body.clone());
        }
    }

    #[tokio::test]
    async fn block_downloader_range_request() {
        const TOTAL_BLOCKS: usize = 10;
        let TestHarness { mut block_downloader, client } = TestHarness::new(TOTAL_BLOCKS);
        let tip = client.highest_block().expect("there should be blocks here");

        // send block range download request
        block_downloader.on_action(DownloadAction::Download(DownloadRequest::BlockRange(
            tip.hash(),
            tip.number,
        )));

        // ensure we have one in flight range request
        assert_eq!(block_downloader.inflight_block_range_requests.len(), 1);

        // ensure the range request is made correctly
        let first_req = block_downloader.inflight_block_range_requests.first().unwrap();
        assert_eq!(first_req.start_hash(), tip.hash());
        assert_eq!(first_req.count(), tip.number);

        // poll downloader
        let sync_future = poll_fn(|cx| block_downloader.poll(cx));
        let next_ready = sync_future.await;

        assert_matches!(next_ready, DownloadOutcome::Blocks(blocks) => {
            // ensure all blocks were obtained
            assert_eq!(blocks.len(), TOTAL_BLOCKS);

            // ensure they are in ascending order
            for num in 1..=TOTAL_BLOCKS {
                assert_eq!(blocks[num-1].number, num as u64);
            }
        });
    }

    #[tokio::test]
    async fn block_downloader_set_request() {
        const TOTAL_BLOCKS: usize = 2;
        let TestHarness { mut block_downloader, client } = TestHarness::new(TOTAL_BLOCKS);

        let tip = client.highest_block().expect("there should be blocks here");

        // send block set download request
        block_downloader.on_action(DownloadAction::Download(DownloadRequest::BlockSet(
            HashSet::from([tip.hash(), tip.parent_hash]),
        )));

        // ensure we have TOTAL_BLOCKS in flight full block request
        assert_eq!(block_downloader.inflight_full_block_requests.len(), TOTAL_BLOCKS);

        // poll downloader
        let sync_future = poll_fn(|cx| block_downloader.poll(cx));
        let next_ready = sync_future.await;

        assert_matches!(next_ready, DownloadOutcome::Blocks(blocks) => {
            // ensure all blocks were obtained
            assert_eq!(blocks.len(), TOTAL_BLOCKS);

            // ensure they are in ascending order
            for num in 1..=TOTAL_BLOCKS {
                assert_eq!(blocks[num-1].number, num as u64);
            }
        });
    }

    #[tokio::test]
    async fn block_downloader_clear_request() {
        const TOTAL_BLOCKS: usize = 10;
        let TestHarness { mut block_downloader, client } = TestHarness::new(TOTAL_BLOCKS);

        let tip = client.highest_block().expect("there should be blocks here");

        // send block range download request
        block_downloader.on_action(DownloadAction::Download(DownloadRequest::BlockRange(
            tip.hash(),
            tip.number,
        )));

        // send block set download request
        let download_set = HashSet::from([tip.hash(), tip.parent_hash]);
        block_downloader
            .on_action(DownloadAction::Download(DownloadRequest::BlockSet(download_set.clone())));

        // ensure we have one in flight range request
        assert_eq!(block_downloader.inflight_block_range_requests.len(), 1);

        // ensure the range request is made correctly
        let first_req = block_downloader.inflight_block_range_requests.first().unwrap();
        assert_eq!(first_req.start_hash(), tip.hash());
        assert_eq!(first_req.count(), tip.number);

        // ensure we have download_set.len() in flight full block request
        assert_eq!(block_downloader.inflight_full_block_requests.len(), download_set.len());

        // send clear request
        block_downloader.on_action(DownloadAction::Clear);

        // ensure we have no in flight range request
        assert_eq!(block_downloader.inflight_block_range_requests.len(), 0);

        // ensure we have no in flight full block request
        assert_eq!(block_downloader.inflight_full_block_requests.len(), 0);
    }
}

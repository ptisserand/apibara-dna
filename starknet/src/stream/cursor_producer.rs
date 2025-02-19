use std::{
    pin::Pin,
    sync::Arc,
    task::{self, Poll, Waker},
};

use apibara_core::{node::v1alpha2::DataFinality, starknet::v1alpha2};
use apibara_node::{
    async_trait,
    stream::{
        BatchCursor, CursorProducer, IngestionMessage, IngestionResponse, ReconfigureResponse,
        StreamConfiguration, StreamError,
    },
};
use futures::{stream::FusedStream, Stream};
use tracing::debug;

use crate::{core::GlobalBlockId, db::StorageReader};

/// A [CursorProducer] that produces sequential cursors.
pub struct SequentialCursorProducer<R: StorageReader + Send + Sync + 'static> {
    configuration: Option<BatchConfiguration>,
    ingestion_state: Option<IngestionState>,
    storage: Arc<R>,
    waker: Option<Waker>,
}

struct BatchConfiguration {
    current: Option<GlobalBlockId>,
    pending_sent: bool,
    data_finality: DataFinality,
    batch_size: usize,
}

#[derive(Default, Debug)]
struct IngestionState {
    finalized: Option<GlobalBlockId>,
    accepted: Option<GlobalBlockId>,
    pending: Option<GlobalBlockId>,
}

impl<R> SequentialCursorProducer<R>
where
    R: StorageReader + Send + Sync + 'static,
{
    pub fn new(storage: Arc<R>) -> Self {
        SequentialCursorProducer {
            configuration: None,
            storage,
            ingestion_state: None,
            waker: None,
        }
    }

    pub fn next_cursor(&mut self) -> Result<Option<BatchCursor<GlobalBlockId>>, R::Error> {
        if self.configuration.is_some() {
            self.next_cursor_with_configuration()
        } else {
            Ok(None)
        }
    }

    fn next_cursor_with_configuration(
        &mut self,
    ) -> Result<Option<BatchCursor<GlobalBlockId>>, R::Error> {
        // We call this from inside a `is_some` check.
        let state = self.get_ingestion_state()?;
        // keep borrow checker happy
        let pending_cursor = state.pending;
        let accepted_cursor = state.accepted;
        let finalized_cursor = state.finalized;

        let configuration = self.configuration.as_mut().expect("configuration");
        let starting_cursor = configuration.current;

        let next_block_number = configuration.current.map(|c| c.number() + 1).unwrap_or(0);

        if let Some(finalized) = finalized_cursor {
            if next_block_number <= finalized.number() {
                return self.next_cursor_finalized(starting_cursor, next_block_number, &finalized);
            }
        }

        if let Some(accepted) = accepted_cursor {
            if next_block_number <= accepted.number() {
                return self.next_cursor_accepted(starting_cursor, next_block_number);
            }
        }

        if let Some(pending) = pending_cursor {
            if next_block_number <= pending.number() {
                return self.next_cursor_pending(starting_cursor, next_block_number);
            }
        }

        Ok(None)
    }

    fn next_cursor_finalized(
        &mut self,
        starting_cursor: Option<GlobalBlockId>,
        next_block_number: u64,
        finalized: &GlobalBlockId,
    ) -> Result<Option<BatchCursor<GlobalBlockId>>, R::Error> {
        // always send finalized data.
        let configuration = self.configuration.as_mut().expect("configuration");
        let mut cursors = Vec::with_capacity(configuration.batch_size);
        let final_block_number = u64::min(
            finalized.number(),
            next_block_number + (configuration.batch_size as u64) - 1,
        );
        for block_number in next_block_number..=final_block_number {
            match self.storage.canonical_block_id(block_number)? {
                Some(cursor) => {
                    cursors.push(cursor);
                }
                None => break,
            }
        }

        if cursors.is_empty() {
            return Ok(None);
        }

        let batch_cursor = BatchCursor::new_finalized(starting_cursor, cursors);
        configuration.current = Some(*batch_cursor.end_cursor());
        Ok(Some(batch_cursor))
    }

    fn next_cursor_accepted(
        &mut self,
        starting_cursor: Option<GlobalBlockId>,
        next_block_number: u64,
    ) -> Result<Option<BatchCursor<GlobalBlockId>>, R::Error> {
        let configuration = self.configuration.as_mut().expect("configuration");
        if configuration.data_finality == DataFinality::DataStatusFinalized
            || configuration.data_finality == DataFinality::DataStatusUnknown
        {
            return Ok(None);
        }

        match self.storage.canonical_block_id(next_block_number)? {
            Some(cursor) => {
                let batch_cursor = BatchCursor::new_accepted(starting_cursor, cursor);
                configuration.current = Some(*batch_cursor.end_cursor());
                Ok(Some(batch_cursor))
            }
            None => Ok(None),
        }
    }

    fn next_cursor_pending(
        &mut self,
        starting_cursor: Option<GlobalBlockId>,
        next_block_number: u64,
    ) -> Result<Option<BatchCursor<GlobalBlockId>>, R::Error> {
        let configuration = self.configuration.as_mut().expect("configuration");
        if configuration.data_finality != DataFinality::DataStatusPending
            || configuration.pending_sent
        {
            return Ok(None);
        }

        match self.storage.canonical_block_id(next_block_number)? {
            Some(cursor) => {
                let batch_cursor = BatchCursor::new_pending(starting_cursor, cursor);
                configuration.pending_sent = true;
                Ok(Some(batch_cursor))
            }
            None => Ok(None),
        }
    }

    fn get_ingestion_state(&mut self) -> Result<&IngestionState, R::Error> {
        let state = self.get_ingestion_state_mut()?;
        Ok(state)
    }

    fn get_ingestion_state_mut(&mut self) -> Result<&mut IngestionState, R::Error> {
        // Read new state only if we don't have one yet.
        // Initialize with default value otherwise to make the borrow checker happy.
        let new_state = if self.ingestion_state.is_some() {
            IngestionState::default()
        } else {
            let accepted = self.storage.highest_accepted_block()?;
            let finalized = self.storage.highest_finalized_block()?;
            IngestionState {
                accepted,
                finalized,
                pending: None,
            }
        };

        Ok(self.ingestion_state.get_or_insert(new_state))
    }

    /// wake up the stream if it was waiting for a new block
    fn wake(&mut self) {
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

fn lowest_cursor(a: GlobalBlockId, b: GlobalBlockId) -> GlobalBlockId {
    if a.number() < b.number() {
        a
    } else {
        b
    }
}

#[async_trait]
impl<R> CursorProducer for SequentialCursorProducer<R>
where
    R: StorageReader + Send + Sync + 'static,
{
    type Cursor = GlobalBlockId;
    type Filter = v1alpha2::Filter;

    async fn reconfigure(
        &mut self,
        configuration: &StreamConfiguration<Self::Cursor, Self::Filter>,
    ) -> Result<ReconfigureResponse<Self::Cursor>, StreamError> {
        let (current, response) = match configuration.starting_cursor {
            None => (None, ReconfigureResponse::Ok),
            Some(starting_cursor) => {
                let starting_cursor = if starting_cursor.hash().is_zero() {
                    // the user specified a block number but not a hash. Find the hash
                    // corresponding to the block number.
                    match self
                        .storage
                        .canonical_block_id(starting_cursor.number())
                        .map_err(StreamError::internal)?
                    {
                        Some(starting_cursor) => starting_cursor,
                        None => return Ok(ReconfigureResponse::MissingStartingCursor),
                    }
                } else {
                    starting_cursor
                };

                debug!(starting_cursor = ?starting_cursor, "reconfigure stream with starting cursor");
                let starting_status = match self
                    .storage
                    .read_status(&starting_cursor)
                    .map_err(StreamError::internal)?
                {
                    None => return Ok(ReconfigureResponse::MissingStartingCursor),
                    Some(starting_status) => starting_status,
                };

                if starting_status.is_accepted() || starting_status.is_finalized() {
                    (Some(starting_cursor), ReconfigureResponse::Ok)
                } else {
                    // the user-specified cursor is not part of the canonical chain anymore.
                    // walk bakcwards until finding a canonical chain and use that as starting
                    // cursor.
                    let mut new_root = starting_cursor;
                    loop {
                        let status = match self
                            .storage
                            .read_status(&new_root)
                            .map_err(StreamError::internal)?
                        {
                            None => return Ok(ReconfigureResponse::MissingStartingCursor),
                            Some(status) => status,
                        };

                        if status.is_accepted() || status.is_finalized() {
                            break;
                        }

                        let header = match self
                            .storage
                            .read_header(&new_root)
                            .map_err(StreamError::internal)?
                        {
                            None => return Ok(ReconfigureResponse::MissingStartingCursor),
                            Some(header) => header,
                        };

                        new_root = GlobalBlockId::from_block_header_parent(&header)
                            .map_err(StreamError::internal)?;
                    }

                    (Some(new_root), ReconfigureResponse::Invalidate(new_root))
                }
            }
        };

        let configuration = BatchConfiguration {
            data_finality: configuration.finality,
            pending_sent: false,
            current,
            batch_size: configuration.batch_size,
        };
        self.configuration = Some(configuration);

        self.wake();

        Ok(response)
    }

    async fn handle_ingestion_message(
        &mut self,
        message: &IngestionMessage<Self::Cursor>,
    ) -> Result<IngestionResponse<Self::Cursor>, StreamError> {
        let mut state = self
            .get_ingestion_state_mut()
            .map_err(StreamError::internal)?;
        let response = match message {
            IngestionMessage::Pending(cursor) => {
                state.pending = Some(*cursor);
                // mark pending as ready to send
                if let Some(mut configuration) = self.configuration.as_mut() {
                    configuration.pending_sent = false;
                }
                IngestionResponse::Ok
            }
            IngestionMessage::Accepted(cursor) => {
                state.finalized = None;
                state.accepted = Some(*cursor);
                IngestionResponse::Ok
            }
            IngestionMessage::Finalized(cursor) => {
                state.finalized = Some(*cursor);
                IngestionResponse::Ok
            }
            IngestionMessage::Invalidate(cursor) => {
                state.pending = None;
                state.accepted = state.accepted.map(|c| lowest_cursor(c, *cursor));
                state.finalized = state.finalized.map(|c| lowest_cursor(c, *cursor));
                // if the current cursor is after the new head, then data was invalidated.
                if let Some(mut configuration) = self.configuration.as_mut() {
                    let is_invalidated = configuration
                        .current
                        .map(|c| c.number() > cursor.number())
                        .unwrap_or(false);

                    configuration.current =
                        configuration.current.map(|c| lowest_cursor(c, *cursor));

                    if is_invalidated {
                        IngestionResponse::Invalidate(*cursor)
                    } else {
                        IngestionResponse::Ok
                    }
                } else {
                    IngestionResponse::Ok
                }
            }
        };

        self.wake();

        Ok(response)
    }
}

impl<R> Stream for SequentialCursorProducer<R>
where
    R: StorageReader + Send + Sync + 'static,
{
    type Item = Result<BatchCursor<GlobalBlockId>, StreamError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Option<Self::Item>> {
        match self.next_cursor() {
            Err(err) => {
                let err = StreamError::internal(err);
                Poll::Ready(Some(Err(err)))
            }
            Ok(None) => {
                // no new block yet, store waker and wake after a new ingestion message
                self.waker = Some(cx.waker().clone());
                Poll::Pending
            }
            Ok(Some(batch_cursor)) => Poll::Ready(Some(Ok(batch_cursor))),
        }
    }
}

impl<R> FusedStream for SequentialCursorProducer<R>
where
    R: StorageReader + Send + Sync + 'static,
{
    fn is_terminated(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use apibara_core::{
        node::v1alpha2::DataFinality,
        starknet::v1alpha2::{BlockHeader, BlockStatus, Filter},
    };
    use apibara_node::stream::{
        CursorProducer, IngestionMessage, ReconfigureResponse, StreamConfiguration,
    };
    use assert_matches::assert_matches;
    use futures::{FutureExt, StreamExt, TryStreamExt};
    use mockall::predicate::eq;

    use crate::{
        core::{BlockHash, GlobalBlockId},
        db::{MockStorageReader, StorageReader},
    };

    use super::SequentialCursorProducer;

    fn new_block_hash(n: u64, c: u8) -> BlockHash {
        let mut b = [0; 32];
        b[24..].copy_from_slice(&n.to_be_bytes());
        b[0] = c;
        BlockHash::from_slice(&b).unwrap()
    }

    fn new_block_id(num: u64) -> GlobalBlockId {
        let hash = new_block_hash(num, 0);
        GlobalBlockId::new(num, hash)
    }

    fn new_block_header(
        number: u64,
        hash: GlobalBlockId,
        parent_hash: GlobalBlockId,
    ) -> BlockHeader {
        BlockHeader {
            block_number: number,
            block_hash: Some(hash.hash().into()),
            parent_block_hash: Some(parent_hash.hash().into()),
            ..BlockHeader::default()
        }
    }

    fn new_configuration(
        starting_cursor: Option<GlobalBlockId>,
        finality: DataFinality,
    ) -> StreamConfiguration<GlobalBlockId, Filter> {
        StreamConfiguration {
            batch_size: 3,
            stream_id: 0,
            finality,
            starting_cursor,
            filter: Filter::default(),
        }
    }

    async fn new_producer<R>(
        cursor: Option<GlobalBlockId>,
        finality: DataFinality,
        storage: Arc<R>,
    ) -> SequentialCursorProducer<R>
    where
        R: StorageReader + Send + Sync + 'static,
    {
        let mut producer = SequentialCursorProducer::new(storage);
        producer
            .reconfigure(&new_configuration(cursor, finality))
            .await
            .unwrap();
        producer
    }

    /// This test checks that the cursor producer keeps producing finalized batches with the
    /// requested number of cursors.
    ///
    /// Finality: FINALIZED
    #[tokio::test]
    async fn test_produce_full_batch_finalized() {
        let mut storage = MockStorageReader::new();
        storage
            .expect_canonical_block_id()
            .returning(|i| Ok(Some(new_block_id(i))));
        storage
            .expect_highest_accepted_block()
            .returning(|| Ok(Some(new_block_id(100))));
        storage
            .expect_highest_finalized_block()
            .returning(|| Ok(Some(new_block_id(90))));

        let producer =
            new_producer(None, DataFinality::DataStatusFinalized, Arc::new(storage)).await;

        let batches: Vec<_> = producer.take(5).try_collect().await.unwrap();
        assert_eq!(batches.len(), 5);
        let mut i = 0;
        for batch in batches {
            let cursors = batch.as_finalized().unwrap();
            for cursor in cursors {
                assert_eq!(cursor.number(), i as u64);
                i += 1;
            }
        }
    }

    /// This test checks that the producer doesn't produce any cursor if the requested block is
    /// after the most recent finalized block.
    ///
    /// Finality: FINALIZED
    #[tokio::test]
    async fn test_produce_nothing_if_after_finalized_as_finalized() {
        let mut storage = MockStorageReader::new();
        storage
            .expect_read_status()
            .returning(|_| Ok(Some(BlockStatus::AcceptedOnL1)));
        storage
            .expect_canonical_block_id()
            .returning(|i| Ok(Some(new_block_id(i))));
        storage
            .expect_highest_accepted_block()
            .returning(|| Ok(Some(new_block_id(100))));
        storage
            .expect_highest_finalized_block()
            .returning(|| Ok(Some(new_block_id(90))));

        let mut producer = new_producer(
            Some(new_block_id(90)),
            DataFinality::DataStatusFinalized,
            Arc::new(storage),
        )
        .await;

        let batch = producer.try_next().now_or_never();
        assert!(batch.is_none());
    }

    /// This test checks the transition between finalized and accepted. Since the requested data is
    /// finalized, the producer should produce partial batches with only the finalized cursors.
    ///
    /// Finality: FINALIZED
    #[tokio::test]
    async fn test_reach_accepted_as_finalized() {
        let mut storage = MockStorageReader::new();
        storage
            .expect_canonical_block_id()
            .returning(|i| Ok(Some(new_block_id(i))));
        storage
            .expect_highest_accepted_block()
            .returning(|| Ok(Some(new_block_id(15))));
        storage
            .expect_highest_finalized_block()
            .returning(|| Ok(Some(new_block_id(10))));

        let producer =
            new_producer(None, DataFinality::DataStatusFinalized, Arc::new(storage)).await;

        let batches: Vec<_> = producer.take(4).try_collect().await.unwrap();
        assert_eq!(batches.len(), 4);
        let mut i = 0;
        for (batch_idx, batch) in batches.iter().enumerate() {
            let cursors = batch.as_finalized().unwrap();
            if batch_idx == 3 {
                // last batch is partial because it cannot contain block 11, which is accepted
                assert_eq!(cursors.len(), 2);
            } else {
                assert_eq!(cursors.len(), 3);
            }
            for cursor in cursors {
                assert_eq!(cursor.number(), i as u64);
                i += 1;
            }
        }
    }

    /// This test checks that the producer starts producing new batches after the chain finality
    /// status is updated.
    ///
    /// Finality: FINALIZED
    #[tokio::test]
    async fn test_handle_finalized_message_as_finalized() {
        let mut storage = MockStorageReader::new();
        storage
            .expect_canonical_block_id()
            .returning(|i| Ok(Some(new_block_id(i))));
        storage
            .expect_highest_accepted_block()
            .returning(|| Ok(Some(new_block_id(15))));
        storage
            .expect_highest_finalized_block()
            .returning(|| Ok(Some(new_block_id(10))));

        let mut producer =
            new_producer(None, DataFinality::DataStatusFinalized, Arc::new(storage)).await;

        for _ in 0..4 {
            let batch = producer.try_next().await.unwrap().unwrap();
            assert!(batch.as_finalized().is_some());
        }

        let batch = producer.try_next().now_or_never();
        assert!(batch.is_none());

        producer
            .handle_ingestion_message(&IngestionMessage::Finalized(new_block_id(14)))
            .await
            .unwrap();

        let mut expected_block = 11;
        for _ in 0..2 {
            let batch = producer.try_next().await.unwrap().unwrap();
            let cursors = batch.as_finalized().unwrap();
            for cursor in cursors {
                assert_eq!(cursor.number(), expected_block);
                expected_block += 1;
            }
        }

        let batch = producer.try_next().now_or_never();
        assert!(batch.is_none());
    }

    /// This test checks that the producer produces messages after the invalidated cursor.
    ///
    /// Finality: FINALIZED
    #[tokio::test]
    async fn test_handle_invalidate_message_as_finalized() {
        let mut storage = MockStorageReader::new();
        storage
            .expect_read_status()
            .returning(|_| Ok(Some(BlockStatus::AcceptedOnL1)));
        storage
            .expect_canonical_block_id()
            .returning(|i| Ok(Some(new_block_id(i))));
        storage
            .expect_highest_accepted_block()
            .returning(|| Ok(Some(new_block_id(15))));
        storage
            .expect_highest_finalized_block()
            .returning(|| Ok(Some(new_block_id(10))));

        let mut producer = new_producer(
            Some(new_block_id(8)),
            DataFinality::DataStatusFinalized,
            Arc::new(storage),
        )
        .await;

        let batch = producer.try_next().await.unwrap().unwrap();
        assert!(batch.as_finalized().is_some());

        let batch = producer.try_next().now_or_never();
        assert!(batch.is_none());

        // invalidate after current. nothing happens
        producer
            .handle_ingestion_message(&IngestionMessage::Invalidate(new_block_id(14)))
            .await
            .unwrap();

        let batch = producer.try_next().now_or_never();
        assert!(batch.is_none());

        // invalidate before current. goes back
        producer
            .handle_ingestion_message(&IngestionMessage::Invalidate(new_block_id(4)))
            .await
            .unwrap();

        // still no new finalized.
        let batch = producer.try_next().now_or_never();
        assert!(batch.is_none());

        producer
            .handle_ingestion_message(&IngestionMessage::Finalized(new_block_id(6)))
            .await
            .unwrap();

        let batch = producer.try_next().await.unwrap().unwrap();
        assert!(batch.as_finalized().is_some());
    }

    /// This test checks that no data is produced if the node has not ingested any finalized block
    /// yet.
    ///
    /// Finality: FINALIZED
    #[tokio::test]
    async fn test_no_finalized_as_finalized() {
        let mut storage = MockStorageReader::new();
        storage
            .expect_canonical_block_id()
            .returning(|i| Ok(Some(new_block_id(i))));
        storage
            .expect_highest_accepted_block()
            .returning(|| Ok(Some(new_block_id(14))));
        storage
            .expect_highest_finalized_block()
            .returning(|| Ok(None));

        let mut producer =
            new_producer(None, DataFinality::DataStatusFinalized, Arc::new(storage)).await;

        let batch = producer.try_next().now_or_never();
        assert!(batch.is_none());

        producer
            .handle_ingestion_message(&IngestionMessage::Finalized(new_block_id(13)))
            .await
            .unwrap();

        let batch = producer.try_next().await.unwrap().unwrap();
        assert!(batch.as_finalized().is_some());
    }

    /// This test checks that no data is produced if the node has not ingested any finalized block
    /// yet.
    ///
    /// Finality: FINALIZED
    #[tokio::test]
    async fn test_no_accepted_as_finalized() {
        let mut storage = MockStorageReader::new();
        storage
            .expect_canonical_block_id()
            .returning(|i| Ok(Some(new_block_id(i))));
        storage
            .expect_highest_accepted_block()
            .returning(|| Ok(None));
        storage
            .expect_highest_finalized_block()
            .returning(|| Ok(Some(new_block_id(15))));

        let mut producer =
            new_producer(None, DataFinality::DataStatusFinalized, Arc::new(storage)).await;

        let batch = producer.try_next().await.unwrap().unwrap();
        assert!(batch.as_finalized().is_some());
    }

    /// This test checks that the producer switches between producing finalized cursors and
    /// accepted cursors.
    ///
    /// Finality: ACCEPTED
    #[tokio::test]
    async fn test_full_batch_as_accepted() {
        let mut storage = MockStorageReader::new();
        storage
            .expect_read_status()
            .returning(|_| Ok(Some(BlockStatus::AcceptedOnL1)));
        storage
            .expect_canonical_block_id()
            .returning(|i| Ok(Some(new_block_id(i))));
        storage
            .expect_highest_accepted_block()
            .returning(|| Ok(Some(new_block_id(15))));
        storage
            .expect_highest_finalized_block()
            .returning(|| Ok(Some(new_block_id(10))));

        let mut producer = new_producer(
            Some(new_block_id(8)),
            DataFinality::DataStatusAccepted,
            Arc::new(storage),
        )
        .await;

        // finalized batch
        let batch = producer.try_next().await.unwrap().unwrap();
        assert!(batch.as_finalized().is_some());

        // accepted batches
        for block_num in 11..=15 {
            let batch = producer.try_next().await.unwrap().unwrap();
            assert!(batch.as_finalized().is_none());
            let accepted = batch.as_accepted().unwrap();
            assert_eq!(accepted.number(), block_num);
        }

        let batch = producer.try_next().now_or_never();
        assert!(batch.is_none());
    }

    /// This test checks that the producer goes back to producing finalized blocks after receiving
    /// a finalized message, if the new finalized cursor is after the current cursor.
    ///
    /// Finality: ACCEPTED
    #[tokio::test]
    async fn test_handle_finalized_message_as_accepted() {
        let mut storage = MockStorageReader::new();
        storage
            .expect_read_status()
            .returning(|_| Ok(Some(BlockStatus::AcceptedOnL1)));
        storage
            .expect_canonical_block_id()
            .returning(|i| Ok(Some(new_block_id(i))));
        storage
            .expect_highest_accepted_block()
            .returning(|| Ok(Some(new_block_id(15))));
        storage
            .expect_highest_finalized_block()
            .returning(|| Ok(Some(new_block_id(10))));

        let mut producer = new_producer(
            Some(new_block_id(8)),
            DataFinality::DataStatusAccepted,
            Arc::new(storage),
        )
        .await;

        // finalized batch
        let batch = producer.try_next().await.unwrap().unwrap();
        assert!(batch.as_finalized().is_some());

        // one finalized batch
        let batch = producer.try_next().await.unwrap().unwrap();
        assert!(batch.as_finalized().is_none());
        let accepted = batch.as_accepted().unwrap();
        assert_eq!(accepted.number(), 11);

        producer
            .handle_ingestion_message(&IngestionMessage::Finalized(new_block_id(13)))
            .await
            .unwrap();

        // finalized with block 12, 13
        let batch = producer.try_next().await.unwrap().unwrap();
        assert!(batch.as_finalized().is_some());

        // one finalized batch
        let batch = producer.try_next().await.unwrap().unwrap();
        assert!(batch.as_finalized().is_none());
        let accepted = batch.as_accepted().unwrap();
        assert_eq!(accepted.number(), 14);
    }

    /// This test checks that the producer resumes producing accepted cursors after receiving an
    /// accepted message.
    ///
    /// Finality: ACCEPTED
    #[tokio::test]
    async fn test_handle_accepted_message_as_accepted() {
        let mut storage = MockStorageReader::new();
        storage
            .expect_read_status()
            .returning(|_| Ok(Some(BlockStatus::AcceptedOnL1)));
        storage
            .expect_canonical_block_id()
            .returning(|i| Ok(Some(new_block_id(i))));
        storage
            .expect_highest_accepted_block()
            .returning(|| Ok(Some(new_block_id(15))));
        storage
            .expect_highest_finalized_block()
            .returning(|| Ok(Some(new_block_id(10))));

        let mut producer = new_producer(
            Some(new_block_id(11)),
            DataFinality::DataStatusAccepted,
            Arc::new(storage),
        )
        .await;

        // accepted batches
        for block_num in 12..=15 {
            let batch = producer.try_next().await.unwrap().unwrap();
            assert!(batch.as_finalized().is_none());
            let accepted = batch.as_accepted().unwrap();
            assert_eq!(accepted.number(), block_num);
        }

        let batch = producer.try_next().now_or_never();
        assert!(batch.is_none());

        producer
            .handle_ingestion_message(&IngestionMessage::Accepted(new_block_id(16)))
            .await
            .unwrap();

        // one finalized batch
        let batch = producer.try_next().await.unwrap().unwrap();
        assert!(batch.as_finalized().is_none());
        let accepted = batch.as_accepted().unwrap();
        assert_eq!(accepted.number(), 16);

        let batch = producer.try_next().now_or_never();
        assert!(batch.is_none());
    }

    /// This test checks that the producer produces messages after the invalidated cursor.
    ///
    /// Finality: ACCEPTED
    #[tokio::test]
    async fn test_handle_invalidate_message_as_accepted() {
        let mut storage = MockStorageReader::new();
        storage
            .expect_read_status()
            .returning(|_| Ok(Some(BlockStatus::AcceptedOnL1)));
        storage
            .expect_canonical_block_id()
            .returning(|i| Ok(Some(new_block_id(i))));
        storage
            .expect_highest_accepted_block()
            .returning(|| Ok(Some(new_block_id(15))));
        storage
            .expect_highest_finalized_block()
            .returning(|| Ok(Some(new_block_id(10))));

        let mut producer = new_producer(
            Some(new_block_id(11)),
            DataFinality::DataStatusAccepted,
            Arc::new(storage),
        )
        .await;

        for _ in 0..2 {
            let batch = producer.try_next().await.unwrap().unwrap();
            assert!(batch.as_accepted().is_some());
        }

        // invalidate after current. nothing happens
        producer
            .handle_ingestion_message(&IngestionMessage::Invalidate(new_block_id(14)))
            .await
            .unwrap();

        let batch = producer.try_next().await.unwrap().unwrap();
        assert_eq!(batch.as_accepted().unwrap().number(), 14);

        // invalidate before current. goes back
        producer
            .handle_ingestion_message(&IngestionMessage::Invalidate(new_block_id(11)))
            .await
            .unwrap();

        // still no new accepted.
        let batch = producer.try_next().now_or_never();
        assert!(batch.is_none());

        producer
            .handle_ingestion_message(&IngestionMessage::Accepted(new_block_id(15)))
            .await
            .unwrap();

        let batch = producer.try_next().await.unwrap().unwrap();
        assert_eq!(batch.as_accepted().unwrap().number(), 12);
    }

    /// This test checks that data is produced if the node has not ingested any finalized data, but
    /// the client requested accepted data. This happens on devnet.
    ///
    /// Finality: ACCEPTED
    #[tokio::test]
    async fn test_no_finalized_as_accepted() {
        let mut storage = MockStorageReader::new();
        storage
            .expect_canonical_block_id()
            .returning(|i| Ok(Some(new_block_id(i))));
        storage
            .expect_highest_accepted_block()
            .returning(|| Ok(Some(new_block_id(14))));
        storage
            .expect_highest_finalized_block()
            .returning(|| Ok(None));

        let mut producer =
            new_producer(None, DataFinality::DataStatusAccepted, Arc::new(storage)).await;

        let batch = producer.try_next().await.unwrap().unwrap();
        assert_eq!(batch.as_accepted().unwrap().number(), 0);
    }

    /// This test checks that finalized cursors are produced even if no accepted data has been
    /// ingested. This happens when initially syncing the node.
    ///
    /// Finality: ACCEPTED
    #[tokio::test]
    async fn test_no_accepted_as_accepted() {
        let mut storage = MockStorageReader::new();
        storage
            .expect_canonical_block_id()
            .returning(|i| Ok(Some(new_block_id(i))));
        storage
            .expect_highest_accepted_block()
            .returning(|| Ok(None));
        storage
            .expect_highest_finalized_block()
            .returning(|| Ok(Some(new_block_id(15))));

        let mut producer =
            new_producer(None, DataFinality::DataStatusAccepted, Arc::new(storage)).await;

        let batch = producer.try_next().await.unwrap().unwrap();
        assert!(batch.as_finalized().is_some());
    }

    /// This test checks that the pending producer produces finalized/accepted cursors until
    /// reaching the head. At that point, it produces one pending block (if any).
    ///
    /// Finality: PENDING
    #[tokio::test]
    async fn test_produce_full_batch_pending() {
        let mut storage = MockStorageReader::new();
        storage
            .expect_read_status()
            .returning(|_| Ok(Some(BlockStatus::AcceptedOnL1)));
        storage
            .expect_canonical_block_id()
            .returning(|i| Ok(Some(new_block_id(i))));
        storage
            .expect_highest_accepted_block()
            .returning(|| Ok(Some(new_block_id(15))));
        storage
            .expect_highest_finalized_block()
            .returning(|| Ok(Some(new_block_id(10))));

        let mut producer = new_producer(
            Some(new_block_id(8)),
            DataFinality::DataStatusPending,
            Arc::new(storage),
        )
        .await;

        let batch = producer.try_next().await.unwrap().unwrap();
        assert!(batch.as_finalized().is_some());

        for i in 11..=15 {
            let batch = producer.try_next().await.unwrap().unwrap();
            assert_eq!(batch.as_accepted().unwrap().number(), i);
        }

        // no pending block yet.
        let batch = producer.try_next().now_or_never();
        assert!(batch.is_none());

        producer
            .handle_ingestion_message(&IngestionMessage::Pending(new_block_id(16)))
            .await
            .unwrap();

        let batch = producer.try_next().await.unwrap().unwrap();
        assert_eq!(batch.as_pending().unwrap().number(), 16);

        // only produce one pending.
        let batch = producer.try_next().now_or_never();
        assert!(batch.is_none());

        producer
            .handle_ingestion_message(&IngestionMessage::Accepted(new_block_id(16)))
            .await
            .unwrap();

        let batch = producer.try_next().await.unwrap().unwrap();
        assert_eq!(batch.as_accepted().unwrap().number(), 16);

        // no pending block yet.
        let batch = producer.try_next().now_or_never();
        assert!(batch.is_none());
    }

    #[tokio::test]
    async fn test_configure_with_valid_starting_cursor() {
        let mut storage = MockStorageReader::new();
        storage
            .expect_read_status()
            .returning(|_| Ok(Some(BlockStatus::AcceptedOnL1)));
        storage
            .expect_canonical_block_id()
            .returning(|i| Ok(Some(new_block_id(i))));
        storage
            .expect_highest_accepted_block()
            .returning(|| Ok(Some(new_block_id(15))));
        storage
            .expect_highest_finalized_block()
            .returning(|| Ok(Some(new_block_id(10))));

        let cursor = new_block_id(8);
        let mut producer = SequentialCursorProducer::new(Arc::new(storage));
        let response = producer
            .reconfigure(&new_configuration(
                Some(cursor),
                DataFinality::DataStatusAccepted,
            ))
            .await
            .unwrap();
        assert_matches!(response, ReconfigureResponse::Ok);
    }

    #[tokio::test]
    async fn test_configure_with_invalidated_starting_cursor() {
        let mut storage = MockStorageReader::new();
        storage
            .expect_read_status()
            .with(eq(new_block_id(8)))
            .returning(|_| Ok(Some(BlockStatus::Rejected)));
        storage
            .expect_read_status()
            .with(eq(new_block_id(7)))
            .returning(|_| Ok(Some(BlockStatus::Rejected)));
        storage
            .expect_read_status()
            .with(eq(new_block_id(6)))
            .returning(|_| Ok(Some(BlockStatus::AcceptedOnL1)));
        storage
            .expect_read_header()
            .with(eq(new_block_id(8)))
            .returning(|_| Ok(Some(new_block_header(8, new_block_id(8), new_block_id(7)))));
        storage
            .expect_read_header()
            .with(eq(new_block_id(7)))
            .returning(|_| Ok(Some(new_block_header(7, new_block_id(7), new_block_id(6)))));
        storage
            .expect_canonical_block_id()
            .returning(|i| Ok(Some(new_block_id(i))));
        storage
            .expect_highest_accepted_block()
            .returning(|| Ok(Some(new_block_id(15))));
        storage
            .expect_highest_finalized_block()
            .returning(|| Ok(Some(new_block_id(10))));

        let cursor = new_block_id(8);
        let mut producer = SequentialCursorProducer::new(Arc::new(storage));
        let response = producer
            .reconfigure(&new_configuration(
                Some(cursor),
                DataFinality::DataStatusAccepted,
            ))
            .await
            .unwrap();
        assert_matches!(response, ReconfigureResponse::Invalidate(_));
    }

    #[tokio::test]
    async fn test_configure_with_non_existing_starting_cursor() {
        let mut storage = MockStorageReader::new();
        storage.expect_read_status().returning(|_| Ok(None));
        storage
            .expect_canonical_block_id()
            .returning(|i| Ok(Some(new_block_id(i))));
        storage
            .expect_highest_accepted_block()
            .returning(|| Ok(Some(new_block_id(15))));
        storage
            .expect_highest_finalized_block()
            .returning(|| Ok(Some(new_block_id(10))));

        let cursor = new_block_id(8);
        let mut producer = SequentialCursorProducer::new(Arc::new(storage));
        let response = producer
            .reconfigure(&new_configuration(
                Some(cursor),
                DataFinality::DataStatusAccepted,
            ))
            .await
            .unwrap();
        assert_matches!(response, ReconfigureResponse::MissingStartingCursor);
    }
}

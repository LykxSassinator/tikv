// Copyright 2024 TiKV Project Authors. Licensed under Apache-2.0.

use core::slice::SlicePattern;
use std::{fmt::Debug, ops::Deref, result, sync::Arc};

use bytes::Bytes;
use crossbeam::epoch;
use crossbeam_skiplist::{SkipList, base::OwnedIter};
use engine_rocks::{raw::SliceTransform, util::FixedSuffixSliceTransform};
use engine_traits::{
    CF_DEFAULT, CacheRegion, CfNamesExt, DbVector, Error, FailedReason, IterMetricsCollector,
    IterOptions, Iterable, Iterator, MetricsExt, Peekable, ReadOptions, Result, Snapshot,
    SnapshotMiscExt,
};
use prometheus::local::LocalHistogram;
use slog_global::error;
use tikv_util::{box_err, time::Instant};

use crate::{
    RegionCacheMemoryEngine,
    background::BackgroundTask,
    engine::{SkiplistEngine, cf_to_id},
    keys::{
        InternalBytes, InternalKey, ValueType, decode_key, encode_seek_for_prev_key,
        encode_seek_key,
    },
    metrics::IN_MEMORY_ENGINE_SEEK_DURATION,
    perf_context::PERF_CONTEXT,
    perf_counter_add,
    statistics::{LocalStatistics, Statistics, Tickers},
};

// The max snapshot number that can exist in the RocksDB. This is typically used
// for search.
pub const MAX_SEQUENCE_NUMBER: u64 = (1 << 56) - 1;

#[derive(PartialEq)]
enum Direction {
    Uninit,
    Forward,
    Backward,
}

#[derive(Clone, Debug)]
pub struct RegionCacheSnapshotMeta {
    pub(crate) region: CacheRegion,
    pub(crate) snapshot_ts: u64,
    // Sequence number is shared between RegionCacheEngine and disk KvEnigne to
    // provide atomic write
    pub(crate) sequence_number: u64,
}

impl RegionCacheSnapshotMeta {
    pub(crate) fn new(region: CacheRegion, snapshot_ts: u64, sequence_number: u64) -> Self {
        Self {
            region,
            snapshot_ts,
            sequence_number,
        }
    }
}

#[derive(Debug)]
pub struct RegionCacheSnapshot {
    snapshot_meta: RegionCacheSnapshotMeta,
    skiplist_engine: SkiplistEngine,
    engine: RegionCacheMemoryEngine,
}

impl RegionCacheSnapshot {
    pub fn new(
        engine: RegionCacheMemoryEngine,
        region: CacheRegion,
        read_ts: u64,
        seq_num: u64,
    ) -> result::Result<Self, FailedReason> {
        engine
            .core
            .region_manager
            .region_snapshot(region.id, region.epoch_version, read_ts)?;
        Ok(RegionCacheSnapshot {
            snapshot_meta: RegionCacheSnapshotMeta::new(region, read_ts, seq_num),
            skiplist_engine: engine.core.engine.clone(),
            engine: engine.clone(),
        })
    }

    pub(crate) fn snapshot_meta(&self) -> &RegionCacheSnapshotMeta {
        &self.snapshot_meta
    }
}

impl Drop for RegionCacheSnapshot {
    fn drop(&mut self) {
        let regions_removable = self
            .engine
            .core
            .region_manager
            .remove_region_snapshot(&self.snapshot_meta);
        if !regions_removable.is_empty() {
            if let Err(e) = self
                .engine
                .bg_worker_manager()
                .schedule_task(BackgroundTask::DeleteRegions(regions_removable))
            {
                error!(
                    "ime schedule delete range failed";
                    "err" => ?e,
                );
                assert!(tikv_util::thread_group::is_shutdown(!cfg!(test)));
            }
        }
    }
}

impl Snapshot for RegionCacheSnapshot {}

impl Iterable for RegionCacheSnapshot {
    type Iterator = RegionCacheIterator;

    fn iterator_opt(&self, cf: &str, opts: IterOptions) -> Result<Self::Iterator> {
        let iter = self.skiplist_engine.data[cf_to_id(cf)].owned_iter();
        let prefix_extractor = if opts.prefix_same_as_start() {
            Some(FixedSuffixSliceTransform::new(8))
        } else {
            None
        };

        let (lower_bound, upper_bound) = opts.build_bounds();
        // only support with lower/upper bound set
        if lower_bound.is_none() || upper_bound.is_none() {
            return Err(Error::BoundaryNotSet);
        }

        let (lower_bound, upper_bound) = (lower_bound.unwrap(), upper_bound.unwrap());
        if lower_bound < self.snapshot_meta.region.start
            || upper_bound > self.snapshot_meta.region.end
        {
            return Err(Error::Other(box_err!(
                "the boundaries required [{}, {}] exceeds the range of the snapshot [{}, {}]",
                log_wrappers::Value(&lower_bound),
                log_wrappers::Value(&upper_bound),
                log_wrappers::Value(&self.snapshot_meta.region.start),
                log_wrappers::Value(&self.snapshot_meta.region.end)
            )));
        }

        Ok(RegionCacheIterator {
            valid: false,
            prefix: None,
            lower_bound,
            upper_bound,
            iter,
            sequence_number: self.sequence_number(),
            saved_user_key: vec![],
            saved_value: None,
            direction: Direction::Uninit,
            statistics: self.engine.statistics(),
            prefix_extractor,
            local_stats: LocalStatistics::default(),
            seek_duration: IN_MEMORY_ENGINE_SEEK_DURATION.local(),
            snapshot_read_ts: self.snapshot_meta.snapshot_ts,
        })
    }
}

impl Peekable for RegionCacheSnapshot {
    type DbVector = RegionCacheDbVector;

    fn get_value_opt(&self, opts: &ReadOptions, key: &[u8]) -> Result<Option<Self::DbVector>> {
        self.get_value_cf_opt(opts, CF_DEFAULT, key)
    }

    fn get_value_cf_opt(
        &self,
        _: &ReadOptions,
        cf: &str,
        key: &[u8],
    ) -> Result<Option<Self::DbVector>> {
        if !self.snapshot_meta.region.contains_key(key) {
            return Err(Error::Other(box_err!(
                "key {} not in range[{}, {}]",
                log_wrappers::Value(key),
                log_wrappers::Value(&self.snapshot_meta.region.start),
                log_wrappers::Value(&self.snapshot_meta.region.end)
            )));
        }
        let mut iter = self.skiplist_engine.data[cf_to_id(cf)].owned_iter();
        let seek_key = encode_seek_key(key, self.sequence_number());

        let guard = &epoch::pin();
        iter.seek(&seek_key, guard);
        if !iter.valid() {
            return Ok(None);
        }

        match decode_key(iter.key().as_slice()) {
            InternalKey {
                user_key,
                v_type: ValueType::Value,
                ..
            } if user_key == key => {
                let value = iter.value().clone_bytes();
                self.engine
                    .statistics()
                    .record_ticker(Tickers::BytesRead, value.len() as u64);
                perf_counter_add!(get_read_bytes, value.len() as u64);
                Ok(Some(RegionCacheDbVector(value)))
            }
            _ => Ok(None),
        }
    }
}

impl CfNamesExt for RegionCacheSnapshot {
    fn cf_names(&self) -> Vec<&str> {
        unimplemented!()
    }
}

impl SnapshotMiscExt for RegionCacheSnapshot {
    fn sequence_number(&self) -> u64 {
        self.snapshot_meta.sequence_number
    }
}

pub struct RegionCacheIterator {
    valid: bool,
    iter: OwnedIter<Arc<SkipList<InternalBytes, InternalBytes>>, InternalBytes, InternalBytes>,
    // The lower bound is inclusive while the upper bound is exclusive if set
    // Note: bounds (region boundaries) have no mvcc versions
    pub(crate) lower_bound: Vec<u8>,
    pub(crate) upper_bound: Vec<u8>,
    // A snapshot sequence number passed from RocksEngine Snapshot to guarantee suitable
    // visibility.
    pub(crate) sequence_number: u64,

    saved_user_key: Vec<u8>,
    // This is only used by backwawrd iteration where the value we want may not be pointed by the
    // `iter`
    saved_value: Option<Bytes>,

    // Not None means we are performing prefix seek
    // Note: prefix_seek doesn't support seek_to_first and seek_to_last.
    prefix_extractor: Option<FixedSuffixSliceTransform>,
    prefix: Option<Vec<u8>>,

    direction: Direction,

    statistics: Arc<Statistics>,
    local_stats: LocalStatistics,
    seek_duration: LocalHistogram,

    pub(crate) snapshot_read_ts: u64,
}

impl Drop for RegionCacheIterator {
    fn drop(&mut self) {
        self.statistics
            .record_ticker(Tickers::IterBytesRead, self.local_stats.bytes_read);
        self.statistics
            .record_ticker(Tickers::NumberDbSeek, self.local_stats.number_db_seek);
        self.statistics.record_ticker(
            Tickers::NumberDbSeekFound,
            self.local_stats.number_db_seek_found,
        );
        self.statistics
            .record_ticker(Tickers::NumberDbNext, self.local_stats.number_db_next);
        self.statistics.record_ticker(
            Tickers::NumberDbNextFound,
            self.local_stats.number_db_next_found,
        );
        self.statistics
            .record_ticker(Tickers::NumberDbPrev, self.local_stats.number_db_prev);
        self.statistics.record_ticker(
            Tickers::NumberDbPrevFound,
            self.local_stats.number_db_prev_found,
        );
        perf_counter_add!(iter_read_bytes, self.local_stats.bytes_read);
        self.seek_duration.flush();
    }
}

impl RegionCacheIterator {
    // If `skipping_saved_key` is true, the function will keep iterating until it
    // finds a user key that is larger than `saved_user_key`.
    // If `prefix` is not None, the iterator needs to stop when all keys for the
    // prefix are exhausted and the iterator is set to invalid.
    fn find_next_visible_key(&mut self, mut skip_saved_key: bool, guard: &epoch::Guard) {
        while self.iter.valid() {
            let InternalKey {
                user_key,
                sequence,
                v_type,
            } = decode_key(self.iter.key().as_slice());

            if user_key >= self.upper_bound.as_slice() {
                break;
            }

            if let Some(ref prefix) = self.prefix {
                if prefix != self.prefix_extractor.as_mut().unwrap().transform(user_key) {
                    // stop iterating due to unmatched prefix
                    break;
                }
            }

            if self.is_visible(sequence) {
                if skip_saved_key && user_key == self.saved_user_key.as_slice() {
                    // the user key has been met before, skip it.
                    perf_counter_add!(internal_key_skipped_count, 1);
                } else {
                    self.saved_user_key.clear();
                    self.saved_user_key.extend_from_slice(user_key);
                    // self.saved_user_key =
                    // Key::from_encoded(user_key.to_vec()).into_raw().unwrap();

                    match v_type {
                        ValueType::Deletion => {
                            skip_saved_key = true;
                            perf_counter_add!(internal_delete_skipped_count, 1);
                        }
                        ValueType::Value => {
                            self.valid = true;
                            return;
                        }
                    }
                }
            } else if skip_saved_key && user_key > self.saved_user_key.as_slice() {
                // user key changed, so no need to skip it
                skip_saved_key = false;
            }

            self.iter.next(guard);
        }

        self.valid = false;
    }

    fn is_visible(&self, seq: u64) -> bool {
        seq <= self.sequence_number
    }

    fn seek_internal(&mut self, key: &InternalBytes) {
        let guard = &epoch::pin();
        self.iter.seek(key, guard);
        self.local_stats.number_db_seek += 1;
        if self.iter.valid() {
            self.find_next_visible_key(false, guard);
        } else {
            self.valid = false;
        }
    }

    fn seek_for_prev_internal(&mut self, key: &InternalBytes) {
        let guard = &epoch::pin();
        self.iter.seek_for_prev(key, guard);
        self.local_stats.number_db_seek += 1;
        self.prev_internal(guard);
    }

    fn prev_internal(&mut self, guard: &epoch::Guard) {
        while self.iter.valid() {
            let InternalKey { user_key, .. } = decode_key(self.iter.key().as_slice());
            self.saved_user_key.clear();
            self.saved_user_key.extend_from_slice(user_key);

            if user_key < self.lower_bound.as_slice() {
                break;
            }

            if let Some(ref prefix) = self.prefix {
                if prefix != self.prefix_extractor.as_mut().unwrap().transform(user_key) {
                    // stop iterating due to unmatched prefix
                    break;
                }
            }

            if !self.find_value_for_current_key(guard) {
                return;
            }

            self.find_user_key_before_saved(guard);

            if self.valid {
                return;
            }
        }

        // We have not found any key
        self.valid = false;
    }

    // Used for backwards iteration.
    // Looks at the entries with user key `saved_user_key` and finds the most
    // up-to-date value for it. Sets `valid`` to true if the value is found and is
    // ready to be presented to the user through value().
    fn find_value_for_current_key(&mut self, guard: &epoch::Guard) -> bool {
        assert!(self.iter.valid());
        let mut last_key_entry_type = ValueType::Deletion;
        while self.iter.valid() {
            let InternalKey {
                user_key,
                sequence,
                v_type,
            } = decode_key(self.iter.key().as_slice());

            if !self.is_visible(sequence) || self.saved_user_key != user_key {
                // no further version is visible or the user key changed
                break;
            }

            last_key_entry_type = v_type;
            match v_type {
                ValueType::Value => {
                    self.saved_value = Some(self.iter.value().clone_bytes());
                }
                ValueType::Deletion => {
                    self.saved_value.take();
                    perf_counter_add!(internal_delete_skipped_count, 1);
                }
            }

            perf_counter_add!(internal_key_skipped_count, 1);
            self.iter.prev(guard);
        }

        self.valid = last_key_entry_type == ValueType::Value;
        self.iter.valid()
    }

    // Move backwards until the key smaller than `saved_user_key`.
    // Changes valid only if return value is false.
    fn find_user_key_before_saved(&mut self, guard: &epoch::Guard) {
        while self.iter.valid() {
            let InternalKey { user_key, .. } = decode_key(self.iter.key().as_slice());

            if user_key < self.saved_user_key.as_slice() {
                return;
            }

            if self.is_visible(self.sequence_number) {
                perf_counter_add!(internal_key_skipped_count, 1);
            }

            self.iter.prev(guard);
        }
    }

    fn reverse_to_backward(&mut self, guard: &epoch::Guard) {
        self.direction = Direction::Backward;
        self.find_user_key_before_saved(guard);
    }

    fn reverse_to_forward(&mut self, guard: &epoch::Guard) {
        if self.prefix_extractor.is_some() || !self.iter.valid() {
            let seek_key = encode_seek_key(&self.saved_user_key, MAX_SEQUENCE_NUMBER);
            self.iter.seek(&seek_key, guard);
        }

        self.direction = Direction::Forward;
        while self.iter.valid() {
            let InternalKey { user_key, .. } = decode_key(self.iter.key().as_slice());
            if user_key >= self.saved_user_key.as_slice() {
                return;
            }
            self.iter.next(guard);
        }
    }
}

impl Iterator for RegionCacheIterator {
    fn key(&self) -> &[u8] {
        assert!(self.valid);
        &self.saved_user_key
    }

    fn value(&self) -> &[u8] {
        assert!(self.valid);
        if self.direction == Direction::Backward {
            self.saved_value.as_ref().unwrap().as_slice()
        } else {
            self.iter.value().as_slice()
        }
    }

    fn next(&mut self) -> Result<bool> {
        assert!(self.valid);
        let guard = &epoch::pin();

        if self.direction == Direction::Backward {
            self.reverse_to_forward(guard);
        }

        self.iter.next(guard);

        perf_counter_add!(internal_key_skipped_count, 1);
        self.local_stats.number_db_next += 1;

        self.valid = self.iter.valid();
        if self.valid {
            // self.valid can be changed after this
            self.find_next_visible_key(true, guard);
        }

        if self.valid {
            self.local_stats.number_db_next_found += 1;
            self.local_stats.bytes_read += (self.key().len() + self.value().len()) as u64;
        }

        Ok(self.valid)
    }

    fn prev(&mut self) -> Result<bool> {
        assert!(self.valid);
        let guard = &epoch::pin();

        if self.direction == Direction::Forward {
            self.reverse_to_backward(guard);
        }

        self.prev_internal(guard);

        self.local_stats.number_db_prev += 1;
        if self.valid {
            self.local_stats.number_db_prev_found += 1;
            self.local_stats.bytes_read += (self.key().len() + self.value().len()) as u64;
        }

        Ok(self.valid)
    }

    fn seek(&mut self, key: &[u8]) -> Result<bool> {
        fail::fail_point!("ime_on_iterator_seek");
        let begin = Instant::now();
        self.direction = Direction::Forward;
        if let Some(ref mut extractor) = self.prefix_extractor {
            assert!(key.len() >= 8);
            self.prefix = Some(extractor.transform(key).to_vec())
        }

        let seek_key = if key < self.lower_bound.as_slice() {
            self.lower_bound.as_slice()
        } else {
            key
        };

        let seek_key = encode_seek_key(seek_key, self.sequence_number);
        self.seek_internal(&seek_key);
        if self.valid {
            self.local_stats.bytes_read += (self.key().len() + self.value().len()) as u64;
            self.local_stats.number_db_seek_found += 1;
        }
        self.seek_duration.observe(begin.saturating_elapsed_secs());

        Ok(self.valid)
    }

    fn seek_for_prev(&mut self, key: &[u8]) -> Result<bool> {
        let begin = Instant::now();
        self.direction = Direction::Backward;
        if let Some(ref mut extractor) = self.prefix_extractor {
            assert!(key.len() >= 8);
            self.prefix = Some(extractor.transform(key).to_vec())
        }

        let seek_key = if key > self.upper_bound.as_slice() {
            encode_seek_for_prev_key(self.upper_bound.as_slice(), u64::MAX)
        } else {
            encode_seek_for_prev_key(key, 0)
        };

        self.seek_for_prev_internal(&seek_key);
        if self.valid {
            self.local_stats.bytes_read += (self.key().len() + self.value().len()) as u64;
            self.local_stats.number_db_seek_found += 1;
        }
        self.seek_duration.observe(begin.saturating_elapsed_secs());

        Ok(self.valid)
    }

    fn seek_to_first(&mut self) -> Result<bool> {
        let begin = Instant::now();
        assert!(self.prefix_extractor.is_none());
        self.direction = Direction::Forward;
        let seek_key = encode_seek_key(&self.lower_bound, self.sequence_number);
        self.seek_internal(&seek_key);

        if self.valid {
            self.local_stats.bytes_read += (self.key().len() + self.value().len()) as u64;
            self.local_stats.number_db_seek_found += 1;
        }
        self.seek_duration.observe(begin.saturating_elapsed_secs());

        Ok(self.valid)
    }

    fn seek_to_last(&mut self) -> Result<bool> {
        let begin = Instant::now();
        assert!(self.prefix_extractor.is_none());
        self.direction = Direction::Backward;
        let seek_key = encode_seek_for_prev_key(&self.upper_bound, u64::MAX);
        self.seek_for_prev_internal(&seek_key);

        if !self.valid {
            return Ok(false);
        }

        if self.valid {
            self.local_stats.bytes_read += (self.key().len() + self.value().len()) as u64;
            self.local_stats.number_db_seek_found += 1;
        }
        self.seek_duration.observe(begin.saturating_elapsed_secs());

        Ok(self.valid)
    }

    fn valid(&self) -> Result<bool> {
        Ok(self.valid)
    }
}

pub struct RegionCacheIterMetricsCollector;

impl IterMetricsCollector for RegionCacheIterMetricsCollector {
    fn internal_delete_skipped_count(&self) -> u64 {
        PERF_CONTEXT.with(|perf_context| perf_context.borrow().internal_delete_skipped_count)
    }

    fn internal_key_skipped_count(&self) -> u64 {
        PERF_CONTEXT.with(|perf_context| perf_context.borrow().internal_key_skipped_count)
    }
}

impl MetricsExt for RegionCacheIterator {
    type Collector = RegionCacheIterMetricsCollector;
    fn metrics_collector(&self) -> Self::Collector {
        RegionCacheIterMetricsCollector {}
    }
}

#[derive(Debug)]
pub struct RegionCacheDbVector(Bytes);

impl Deref for RegionCacheDbVector {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl DbVector for RegionCacheDbVector {}

impl PartialEq<&[u8]> for RegionCacheDbVector {
    fn eq(&self, rhs: &&[u8]) -> bool {
        self.0.as_slice() == *rhs
    }
}

#[cfg(test)]
mod tests {
    use core::ops::Range;
    use std::{
        iter::{self, StepBy},
        ops::Deref,
        sync::Arc,
        time::Duration,
    };

    use bytes::{BufMut, Bytes};
    use crossbeam::epoch;
    use crossbeam_skiplist::SkipList;
    use engine_rocks::{
        RocksDbOptions, RocksStatistics, raw::DBStatisticsTickerType, util::new_engine_opt,
    };
    use engine_traits::{
        CF_DEFAULT, CF_LOCK, CF_WRITE, CacheRegion, EvictReason, FailedReason,
        IterMetricsCollector, IterOptions, Iterable, Iterator, MetricsExt, Mutable, Peekable,
        ReadOptions, RegionCacheEngine, RegionCacheEngineExt, RegionEvent, WriteBatch,
        WriteBatchExt,
    };
    use keys::DATA_PREFIX_KEY;
    use tempfile::Builder;
    use tikv_util::config::VersionTrack;

    use super::{RegionCacheIterator, RegionCacheSnapshot};
    use crate::{
        InMemoryEngineConfig, InMemoryEngineContext, RegionCacheMemoryEngine,
        RegionCacheWriteBatch,
        engine::{SkiplistEngine, cf_to_id},
        keys::{
            InternalBytes, ValueType, construct_key, construct_region_key, construct_user_key,
            construct_value, decode_key, encode_key, encode_seek_key,
        },
        perf_context::PERF_CONTEXT,
        region_manager::RegionState,
        statistics::Tickers,
        test_util::new_region,
    };

    #[test]
    fn test_snapshot() {
        let engine = RegionCacheMemoryEngine::new(InMemoryEngineContext::new_for_tests(Arc::new(
            VersionTrack::new(InMemoryEngineConfig::config_for_test()),
        )));
        let region = new_region(1, b"k00", b"k10");
        engine.new_region(region.clone());

        let verify_snapshot_count = |snapshot_ts, count| {
            let regions_map = engine.core.region_manager.regions_map.read();
            if count > 0 {
                assert_eq!(
                    *regions_map.regions()[&region.id]
                        .region_snapshot_list()
                        .lock()
                        .unwrap()
                        .0
                        .get(&snapshot_ts)
                        .unwrap(),
                    count
                );
            } else {
                assert!(
                    !regions_map.regions()[&region.id]
                        .region_snapshot_list()
                        .lock()
                        .unwrap()
                        .0
                        .contains_key(&snapshot_ts)
                )
            }
        };

        let cache_region = CacheRegion::from_region(&region);
        let s1 = engine.snapshot(cache_region.clone(), 5, u64::MAX).unwrap();

        assert!(engine.core.region_manager.set_safe_point(region.id, 5));
        assert_eq!(
            engine
                .snapshot(cache_region.clone(), 5, u64::MAX)
                .unwrap_err(),
            FailedReason::TooOldRead
        );
        let s2 = engine.snapshot(cache_region.clone(), 10, u64::MAX).unwrap();

        verify_snapshot_count(5, 1);
        verify_snapshot_count(10, 1);
        let s3 = engine.snapshot(cache_region.clone(), 10, u64::MAX).unwrap();
        verify_snapshot_count(10, 2);

        drop(s1);
        verify_snapshot_count(5, 0);
        drop(s2);
        verify_snapshot_count(10, 1);
        let s4 = engine.snapshot(cache_region.clone(), 10, u64::MAX).unwrap();
        verify_snapshot_count(10, 2);
        drop(s4);
        verify_snapshot_count(10, 1);
        drop(s3);
        {
            let regions_map = engine.core.region_manager.regions_map.read();
            assert!(
                regions_map
                    .region_meta(region.id)
                    .unwrap()
                    .region_snapshot_list()
                    .lock()
                    .unwrap()
                    .is_empty()
            );
        }
    }

    fn fill_data_in_skiplist(
        sl: Arc<SkipList<InternalBytes, InternalBytes>>,
        key_range: StepBy<Range<u64>>,
        mvcc_range: Range<u64>,
        mut start_seq: u64,
    ) {
        let guard = &epoch::pin();
        for mvcc in mvcc_range {
            for i in key_range.clone() {
                let key = construct_key(i, mvcc);
                let val = construct_value(i, mvcc);
                let key = encode_key(&key, start_seq, ValueType::Value);
                sl.insert(key, InternalBytes::from_vec(val.into_bytes()), guard)
                    .release(guard);
            }
            start_seq += 1;
        }
    }

    fn delete_data_in_skiplist(
        sl: Arc<SkipList<InternalBytes, InternalBytes>>,
        key_range: StepBy<Range<u64>>,
        mvcc_range: Range<u64>,
        mut seq: u64,
    ) {
        let guard = &epoch::pin();
        for i in key_range {
            for mvcc in mvcc_range.clone() {
                let key = construct_key(i, mvcc);
                let key = encode_key(&key, seq, ValueType::Deletion);
                sl.insert(key, InternalBytes::from_bytes(Bytes::default()), guard)
                    .release(guard);
            }
            seq += 1;
        }
    }

    fn construct_mvcc_key(key: &str, mvcc: u64) -> Vec<u8> {
        let mut k = vec![];
        k.extend_from_slice(DATA_PREFIX_KEY);
        k.extend_from_slice(key.as_bytes());
        k.put_u64(!mvcc);
        k
    }

    fn put_key_val(
        sl: &Arc<SkipList<InternalBytes, InternalBytes>>,
        key: &str,
        val: &str,
        mvcc: u64,
        seq: u64,
    ) {
        let key = construct_mvcc_key(key, mvcc);
        let key = encode_key(&key, seq, ValueType::Value);
        let guard = &epoch::pin();
        sl.insert(
            key,
            InternalBytes::from_vec(val.to_owned().into_bytes()),
            guard,
        )
        .release(guard);
    }

    fn delete_key(
        sl: &Arc<SkipList<InternalBytes, InternalBytes>>,
        key: &str,
        mvcc: u64,
        seq: u64,
    ) {
        let key = construct_mvcc_key(key, mvcc);
        let key = encode_key(&key, seq, ValueType::Deletion);
        let guard = &epoch::pin();
        sl.insert(key, InternalBytes::from_vec(b"".to_vec()), guard)
            .release(guard);
    }

    fn verify_key_value(k: &[u8], v: &[u8], i: u64, mvcc: u64) {
        let key = construct_key(i, mvcc);
        let val = construct_value(i, mvcc);
        assert_eq!(k, &key);
        assert_eq!(v, val.as_bytes());
    }

    fn verify_key_not_equal(k: &[u8], i: u64, mvcc: u64) {
        let key = construct_key(i, mvcc);
        assert_ne!(k, &key);
    }

    fn verify_key_values<I: iter::Iterator<Item = u32>, J: iter::Iterator<Item = u32> + Clone>(
        iter: &mut RegionCacheIterator,
        key_range: I,
        mvcc_range: J,
        foward: bool,
        ended: bool,
    ) {
        for i in key_range {
            for mvcc in mvcc_range.clone() {
                let k = iter.key();
                let val = iter.value();
                verify_key_value(k, val, i as u64, mvcc as u64);
                if foward {
                    iter.next().unwrap();
                } else {
                    iter.prev().unwrap();
                }
            }
        }

        if ended {
            assert!(!iter.valid().unwrap());
        }
    }

    #[test]
    fn test_seek() {
        let engine = RegionCacheMemoryEngine::new(InMemoryEngineContext::new_for_tests(Arc::new(
            VersionTrack::new(InMemoryEngineConfig::config_for_test()),
        )));
        let region = new_region(1, b"", b"z");
        let range = CacheRegion::from_region(&region);
        engine.new_region(region.clone());

        engine.core.region_manager().set_safe_point(region.id, 5);
        let sl = engine.core.engine.data[cf_to_id("write")].clone();
        put_key_val(&sl, "b", "val", 10, 5);
        put_key_val(&sl, "c", "vall", 10, 5);

        let snapshot = engine.snapshot(range.clone(), u64::MAX, 100).unwrap();
        let mut iter_opt = IterOptions::default();
        iter_opt.set_upper_bound(&range.end, 0);
        iter_opt.set_lower_bound(&range.start, 0);
        let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();

        let key = construct_mvcc_key("b", 10);
        iter.seek(&key).unwrap();
        assert_eq!(iter.value(), b"val");
        let key = construct_mvcc_key("d", 10);
        iter.seek(&key).unwrap();
        assert!(!iter.valid().unwrap());

        let key = construct_mvcc_key("b", 10);
        iter.seek_for_prev(&key).unwrap();
        assert_eq!(iter.value(), b"val");
        let key = construct_mvcc_key("a", 10);
        iter.seek_for_prev(&key).unwrap();
        assert!(!iter.valid().unwrap());
    }

    #[test]
    fn test_get_value() {
        let engine = RegionCacheMemoryEngine::new(InMemoryEngineContext::new_for_tests(Arc::new(
            VersionTrack::new(InMemoryEngineConfig::config_for_test()),
        )));
        let region = new_region(1, b"", b"z");
        let cache_region = CacheRegion::from_region(&region);
        engine.new_region(region.clone());

        engine.core.region_manager.set_safe_point(region.id, 5);
        let sl = engine.core.engine.data[cf_to_id("write")].clone();
        fill_data_in_skiplist(sl.clone(), (1..10).step_by(1), 1..50, 1);
        // k1 is deleted at seq_num 150 while k49 is deleted at seq num 101
        delete_data_in_skiplist(sl, (1..10).step_by(1), 1..50, 100);

        let opts = ReadOptions::default();
        {
            let snapshot = engine.snapshot(cache_region.clone(), 10, 60).unwrap();
            for i in 1..10 {
                for mvcc in 1..50 {
                    let k = construct_key(i, mvcc);
                    let v = snapshot
                        .get_value_cf_opt(&opts, "write", &k)
                        .unwrap()
                        .unwrap();
                    verify_key_value(&k, &v, i, mvcc);
                }
                let k = construct_key(i, 50);
                assert!(
                    snapshot
                        .get_value_cf_opt(&opts, "write", &k)
                        .unwrap()
                        .is_none()
                );
            }
        }

        // all deletions
        {
            let snapshot = engine.snapshot(cache_region.clone(), 10, u64::MAX).unwrap();
            for i in 1..10 {
                for mvcc in 1..50 {
                    let k = construct_key(i, mvcc);
                    assert!(
                        snapshot
                            .get_value_cf_opt(&opts, "write", &k)
                            .unwrap()
                            .is_none()
                    );
                }
            }
        }

        // some deletions
        {
            let snapshot = engine.snapshot(cache_region.clone(), 10, 105).unwrap();
            for mvcc in 1..50 {
                for i in 1..7 {
                    let k = construct_key(i, mvcc);
                    assert!(
                        snapshot
                            .get_value_cf_opt(&opts, "write", &k)
                            .unwrap()
                            .is_none()
                    );
                }
                for i in 7..10 {
                    let k = construct_key(i, mvcc);
                    let v = snapshot
                        .get_value_cf_opt(&opts, "write", &k)
                        .unwrap()
                        .unwrap();
                    verify_key_value(&k, &v, i, mvcc);
                }
            }
        }
    }

    #[test]
    fn test_iterator_forawrd() {
        let engine = RegionCacheMemoryEngine::new(InMemoryEngineContext::new_for_tests(Arc::new(
            VersionTrack::new(InMemoryEngineConfig::config_for_test()),
        )));
        let region = new_region(1, b"", b"z");
        let range = CacheRegion::from_region(&region);
        engine.new_region(region.clone());

        let step = 2;
        engine.core.region_manager.set_safe_point(region.id, 5);
        let sl = engine.core.engine.data[cf_to_id("write")].clone();
        fill_data_in_skiplist(sl.clone(), (1..100).step_by(step), 1..10, 1);
        delete_data_in_skiplist(sl, (1..100).step_by(step), 1..10, 200);

        let mut iter_opt = IterOptions::default();
        let snapshot = engine.snapshot(range.clone(), 10, u64::MAX).unwrap();
        // boundaries are not set
        assert!(snapshot.iterator_opt("lock", iter_opt.clone()).is_err());

        let lower_bound = construct_user_key(1);
        let upper_bound = construct_user_key(100);
        iter_opt.set_upper_bound(&upper_bound, 0);
        iter_opt.set_lower_bound(&lower_bound, 0);

        let mut iter = snapshot.iterator_opt("lock", iter_opt.clone()).unwrap();
        assert!(!iter.seek_to_first().unwrap());

        let mut iter = snapshot.iterator_opt("default", iter_opt.clone()).unwrap();
        assert!(!iter.seek_to_first().unwrap());

        // Not restricted by bounds, no deletion (seq_num 150)
        {
            let snapshot = engine.snapshot(range.clone(), 100, 150).unwrap();
            let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();
            iter.seek_to_first().unwrap();
            verify_key_values(&mut iter, (1..100).step_by(step), (1..10).rev(), true, true);

            // seek key that is in the skiplist
            let seek_key = construct_key(11, u64::MAX);
            iter.seek(&seek_key).unwrap();
            verify_key_values(
                &mut iter,
                (11..100).step_by(step),
                (1..10).rev(),
                true,
                true,
            );

            // seek key that is not in the skiplist
            let seek_key = construct_key(12, u64::MAX);
            iter.seek(&seek_key).unwrap();
            verify_key_values(
                &mut iter,
                (13..100).step_by(step),
                (1..10).rev(),
                true,
                true,
            );
        }

        // Not restricted by bounds, some deletions (seq_num 230)
        {
            let snapshot = engine.snapshot(range.clone(), 10, 230).unwrap();
            let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();
            iter.seek_to_first().unwrap();
            verify_key_values(
                &mut iter,
                (63..100).step_by(step),
                (1..10).rev(),
                true,
                true,
            );

            // sequence can see the deletion
            {
                // seek key that is in the skiplist
                let seek_key = construct_key(21, u64::MAX);
                assert!(iter.seek(&seek_key).unwrap());
                verify_key_not_equal(iter.key(), 21, 9);

                // seek key that is not in the skiplist
                let seek_key = construct_key(22, u64::MAX);
                assert!(iter.seek(&seek_key).unwrap());
                verify_key_not_equal(iter.key(), 23, 9);
            }

            // sequence cannot see the deletion
            {
                // seek key that is in the skiplist
                let seek_key = construct_key(65, u64::MAX);
                iter.seek(&seek_key).unwrap();
                verify_key_value(iter.key(), iter.value(), 65, 9);

                // seek key that is not in the skiplist
                let seek_key = construct_key(66, u64::MAX);
                iter.seek(&seek_key).unwrap();
                verify_key_value(iter.key(), iter.value(), 67, 9);
            }
        }

        // with bounds, no deletion (seq_num 150)
        let lower_bound = construct_user_key(20);
        let upper_bound = construct_user_key(40);
        iter_opt.set_upper_bound(&upper_bound, 0);
        iter_opt.set_lower_bound(&lower_bound, 0);
        {
            let snapshot = engine.snapshot(range.clone(), 10, 150).unwrap();
            let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();

            assert!(iter.seek_to_first().unwrap());
            verify_key_values(&mut iter, (21..40).step_by(step), (1..10).rev(), true, true);

            // seek a key that is below the lower bound is the same with seek_to_first
            let seek_key = construct_key(19, u64::MAX);
            assert!(iter.seek(&seek_key).unwrap());
            verify_key_values(&mut iter, (21..40).step_by(step), (1..10).rev(), true, true);

            // seek a key that is larger or equal to upper bound won't get any key
            let seek_key = construct_key(41, u64::MAX);
            assert!(!iter.seek(&seek_key).unwrap());
            assert!(!iter.valid().unwrap());

            let seek_key = construct_key(32, u64::MAX);
            assert!(iter.seek(&seek_key).unwrap());
            verify_key_values(&mut iter, (33..40).step_by(step), (1..10).rev(), true, true);
        }

        // with bounds, some deletions (seq_num 215)
        {
            let snapshot = engine.snapshot(range.clone(), 10, 215).unwrap();
            let mut iter = snapshot.iterator_opt("write", iter_opt).unwrap();

            // sequence can see the deletion
            {
                // seek key that is in the skiplist
                let seek_key = construct_key(21, u64::MAX);
                assert!(iter.seek(&seek_key).unwrap());
                verify_key_not_equal(iter.key(), 21, 9);

                // seek key that is not in the skiplist
                let seek_key = construct_key(20, u64::MAX);
                assert!(iter.seek(&seek_key).unwrap());
                verify_key_not_equal(iter.key(), 21, 9);
            }

            // sequence cannot see the deletion
            {
                // seek key that is in the skiplist
                let seek_key = construct_key(33, u64::MAX);
                iter.seek(&seek_key).unwrap();
                verify_key_value(iter.key(), iter.value(), 33, 9);

                // seek key that is not in the skiplist
                let seek_key = construct_key(32, u64::MAX);
                iter.seek(&seek_key).unwrap();
                verify_key_value(iter.key(), iter.value(), 33, 9);
            }
        }
    }

    #[test]
    fn test_iterator_backward() {
        let engine = RegionCacheMemoryEngine::new(InMemoryEngineContext::new_for_tests(Arc::new(
            VersionTrack::new(InMemoryEngineConfig::config_for_test()),
        )));
        let region = new_region(1, b"", b"z");
        let range = CacheRegion::from_region(&region);
        engine.new_region(region.clone());
        let step = 2;

        {
            engine.core.region_manager.set_safe_point(region.id, 5);
            let sl = engine.core.engine.data[cf_to_id("write")].clone();
            fill_data_in_skiplist(sl.clone(), (1..100).step_by(step), 1..10, 1);
            delete_data_in_skiplist(sl, (1..100).step_by(step), 1..10, 200);
        }

        let mut iter_opt = IterOptions::default();
        let lower_bound = construct_user_key(1);
        let upper_bound = construct_user_key(100);
        iter_opt.set_upper_bound(&upper_bound, 0);
        iter_opt.set_lower_bound(&lower_bound, 0);

        // Not restricted by bounds, no deletion (seq_num 150)
        {
            let snapshot = engine.snapshot(range.clone(), 10, 150).unwrap();
            let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();
            assert!(iter.seek_to_last().unwrap());
            verify_key_values(&mut iter, (1..100).step_by(step).rev(), 1..10, false, true);

            // seek key that is in the skiplist
            let seek_key = construct_key(81, 0);
            assert!(iter.seek_for_prev(&seek_key).unwrap());
            verify_key_values(&mut iter, (1..82).step_by(step).rev(), 1..10, false, true);

            // seek key that is in the skiplist
            let seek_key = construct_key(80, 0);
            assert!(iter.seek_for_prev(&seek_key).unwrap());
            verify_key_values(&mut iter, (1..80).step_by(step).rev(), 1..10, false, true);
        }

        let lower_bound = construct_user_key(21);
        let upper_bound = construct_user_key(39);
        iter_opt.set_upper_bound(&upper_bound, 0);
        iter_opt.set_lower_bound(&lower_bound, 0);
        {
            let snapshot = engine.snapshot(range.clone(), 10, 150).unwrap();
            let mut iter = snapshot.iterator_opt("write", iter_opt).unwrap();

            assert!(iter.seek_to_last().unwrap());
            verify_key_values(&mut iter, (21..38).step_by(step).rev(), 1..10, false, true);

            // seek a key that is above the upper bound is the same with seek_to_last
            let seek_key = construct_key(40, 0);
            assert!(iter.seek_for_prev(&seek_key).unwrap());
            verify_key_values(&mut iter, (21..38).step_by(step).rev(), 1..10, false, true);

            // seek a key that is less than the lower bound won't get any key
            let seek_key = construct_key(20, u64::MAX);
            assert!(!iter.seek_for_prev(&seek_key).unwrap());
            assert!(!iter.valid().unwrap());

            let seek_key = construct_key(26, 0);
            assert!(iter.seek_for_prev(&seek_key).unwrap());
            verify_key_values(&mut iter, (21..26).step_by(step).rev(), 1..10, false, true);
        }
    }

    #[test]
    fn test_seq_visibility() {
        let engine = RegionCacheMemoryEngine::new(InMemoryEngineContext::new_for_tests(Arc::new(
            VersionTrack::new(InMemoryEngineConfig::config_for_test()),
        )));
        let region = new_region(1, b"", b"z");
        let range = CacheRegion::from_region(&region);
        engine.new_region(region.clone());

        {
            engine.core.region_manager.set_safe_point(region.id, 5);
            let sl = engine.core.engine.data[cf_to_id("write")].clone();

            put_key_val(&sl, "aaa", "va1", 10, 1);
            put_key_val(&sl, "aaa", "va2", 10, 3);
            delete_key(&sl, "aaa", 10, 4);
            put_key_val(&sl, "aaa", "va4", 10, 6);

            put_key_val(&sl, "bbb", "vb1", 10, 2);
            put_key_val(&sl, "bbb", "vb2", 10, 4);

            put_key_val(&sl, "ccc", "vc1", 10, 2);
            put_key_val(&sl, "ccc", "vc2", 10, 4);
            put_key_val(&sl, "ccc", "vc3", 10, 5);
            delete_key(&sl, "ccc", 10, 6);
        }

        let mut iter_opt = IterOptions::default();
        iter_opt.set_upper_bound(&range.end, 0);
        iter_opt.set_lower_bound(&range.start, 0);

        // seq num 1
        {
            let snapshot = engine.snapshot(range.clone(), u64::MAX, 1).unwrap();
            let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();
            iter.seek_to_first().unwrap();
            assert_eq!(iter.value(), b"va1");
            assert!(!iter.next().unwrap());
            let key = construct_mvcc_key("aaa", 10);
            assert_eq!(
                snapshot
                    .get_value_cf("write", &key)
                    .unwrap()
                    .unwrap()
                    .deref(),
                "va1".as_bytes()
            );
            assert!(iter.seek(&key).unwrap());
            assert_eq!(iter.value(), "va1".as_bytes());

            let key = construct_mvcc_key("bbb", 10);
            assert!(snapshot.get_value_cf("write", &key).unwrap().is_none());
            assert!(!iter.seek(&key).unwrap());

            let key = construct_mvcc_key("ccc", 10);
            assert!(snapshot.get_value_cf("write", &key).unwrap().is_none());
            assert!(!iter.seek(&key).unwrap());
        }

        // seq num 2
        {
            let snapshot = engine.snapshot(range.clone(), u64::MAX, 2).unwrap();
            let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();
            iter.seek_to_first().unwrap();
            assert_eq!(iter.value(), b"va1");
            iter.next().unwrap();
            assert_eq!(iter.value(), b"vb1");
            iter.next().unwrap();
            assert_eq!(iter.value(), b"vc1");
            assert!(!iter.next().unwrap());
        }

        // seq num 5
        {
            let snapshot = engine.snapshot(range.clone(), u64::MAX, 5).unwrap();
            let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();
            iter.seek_to_first().unwrap();
            assert_eq!(iter.value(), b"vb2");
            iter.next().unwrap();
            assert_eq!(iter.value(), b"vc3");
            assert!(!iter.next().unwrap());
        }

        // seq num 6
        {
            let snapshot = engine.snapshot(range.clone(), u64::MAX, 6).unwrap();
            let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();
            iter.seek_to_first().unwrap();
            assert_eq!(iter.value(), b"va4");
            iter.next().unwrap();
            assert_eq!(iter.value(), b"vb2");
            assert!(!iter.next().unwrap());

            let key = construct_mvcc_key("aaa", 10);
            assert_eq!(
                snapshot
                    .get_value_cf("write", &key)
                    .unwrap()
                    .unwrap()
                    .deref(),
                "va4".as_bytes()
            );
            assert!(iter.seek(&key).unwrap());
            assert_eq!(iter.value(), "va4".as_bytes());

            let key = construct_mvcc_key("bbb", 10);
            assert_eq!(
                snapshot
                    .get_value_cf("write", &key)
                    .unwrap()
                    .unwrap()
                    .deref(),
                "vb2".as_bytes()
            );
            assert!(iter.seek(&key).unwrap());
            assert_eq!(iter.value(), "vb2".as_bytes());

            let key = construct_mvcc_key("ccc", 10);
            assert!(snapshot.get_value_cf("write", &key).unwrap().is_none());
            assert!(!iter.seek(&key).unwrap());
        }
    }

    #[test]
    fn test_seq_visibility_backward() {
        let engine = RegionCacheMemoryEngine::new(InMemoryEngineContext::new_for_tests(Arc::new(
            VersionTrack::new(InMemoryEngineConfig::config_for_test()),
        )));
        let region = new_region(1, b"", b"z");
        let range = CacheRegion::from_region(&region);
        engine.new_region(region.clone());

        {
            engine.core.region_manager.set_safe_point(region.id, 5);
            let sl = engine.core.engine.data[cf_to_id("write")].clone();

            put_key_val(&sl, "aaa", "va1", 10, 2);
            put_key_val(&sl, "aaa", "va2", 10, 4);
            put_key_val(&sl, "aaa", "va3", 10, 5);
            delete_key(&sl, "aaa", 10, 6);

            put_key_val(&sl, "bbb", "vb1", 10, 2);
            put_key_val(&sl, "bbb", "vb2", 10, 4);

            put_key_val(&sl, "ccc", "vc1", 10, 1);
            put_key_val(&sl, "ccc", "vc2", 10, 3);
            delete_key(&sl, "ccc", 10, 4);
            put_key_val(&sl, "ccc", "vc4", 10, 6);
        }

        let mut iter_opt = IterOptions::default();
        iter_opt.set_upper_bound(&range.end, 0);
        iter_opt.set_lower_bound(&range.start, 0);

        // seq num 1
        {
            let snapshot = engine.snapshot(range.clone(), u64::MAX, 1).unwrap();
            let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();
            iter.seek_to_last().unwrap();
            assert_eq!(iter.value(), b"vc1");
            assert!(!iter.prev().unwrap());
            let key = construct_mvcc_key("aaa", 10);
            assert!(!iter.seek_for_prev(&key).unwrap());

            let key = construct_mvcc_key("bbb", 10);
            assert!(!iter.seek_for_prev(&key).unwrap());

            let key = construct_mvcc_key("ccc", 10);
            assert!(iter.seek_for_prev(&key).unwrap());
            assert_eq!(iter.value(), "vc1".as_bytes());
        }

        // seq num 2
        {
            let snapshot = engine.snapshot(range.clone(), u64::MAX, 2).unwrap();
            let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();
            iter.seek_to_last().unwrap();
            assert_eq!(iter.value(), b"vc1");
            iter.prev().unwrap();
            assert_eq!(iter.value(), b"vb1");
            iter.prev().unwrap();
            assert_eq!(iter.value(), b"va1");
            assert!(!iter.prev().unwrap());
        }

        // seq num 5
        {
            let snapshot = engine.snapshot(range.clone(), u64::MAX, 5).unwrap();
            let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();
            iter.seek_to_last().unwrap();
            assert_eq!(iter.value(), b"vb2");
            iter.prev().unwrap();
            assert_eq!(iter.value(), b"va3");
            assert!(!iter.prev().unwrap());
        }

        // seq num 6
        {
            let snapshot = engine.snapshot(range.clone(), u64::MAX, 6).unwrap();
            let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();
            iter.seek_to_last().unwrap();
            assert_eq!(iter.value(), b"vc4");
            iter.prev().unwrap();
            assert_eq!(iter.value(), b"vb2");
            assert!(!iter.prev().unwrap());

            let key = construct_mvcc_key("ccc", 10);
            assert!(iter.seek_for_prev(&key).unwrap());
            assert_eq!(iter.value(), "vc4".as_bytes());

            let key = construct_mvcc_key("bbb", 10);
            assert!(iter.seek_for_prev(&key).unwrap());
            assert_eq!(iter.value(), "vb2".as_bytes());

            let key = construct_mvcc_key("aaa", 10);
            assert!(!iter.seek_for_prev(&key).unwrap());
        }
    }

    #[test]
    fn test_iter_user_skip() {
        let mut iter_opt = IterOptions::default();
        let region = new_region(1, b"", b"z");
        let range = CacheRegion::from_region(&region);
        iter_opt.set_upper_bound(&range.end, 0);
        iter_opt.set_lower_bound(&range.start, 0);

        // backward, all put
        {
            let engine = RegionCacheMemoryEngine::new(InMemoryEngineContext::new_for_tests(
                Arc::new(VersionTrack::new(InMemoryEngineConfig::config_for_test())),
            ));
            engine.new_region(region.clone());
            let sl = {
                engine.core.region_manager.set_safe_point(region.id, 5);
                engine.core.engine.data[cf_to_id("write")].clone()
            };

            let mut s = 1;
            for seq in 2..50 {
                put_key_val(&sl, "a", "val", 10, s + 1);
                for i in 2..50 {
                    let v = construct_value(i, i);
                    put_key_val(&sl, "b", v.as_str(), 10, s + i);
                }

                let snapshot = engine.snapshot(range.clone(), 10, s + seq).unwrap();
                let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();
                assert!(iter.seek_to_last().unwrap());
                let k = construct_mvcc_key("b", 10);
                let v = construct_value(seq, seq);
                assert_eq!(iter.key(), &k);
                assert_eq!(iter.value(), v.as_bytes());

                assert!(iter.prev().unwrap());
                let k = construct_mvcc_key("a", 10);
                assert_eq!(iter.key(), &k);
                assert_eq!(iter.value(), b"val");
                assert!(!iter.prev().unwrap());
                assert!(!iter.valid().unwrap());
                s += 100;
            }
        }

        // backward, all deletes
        {
            let engine = RegionCacheMemoryEngine::new(InMemoryEngineContext::new_for_tests(
                Arc::new(VersionTrack::new(InMemoryEngineConfig::config_for_test())),
            ));
            engine.new_region(region.clone());
            let sl = {
                engine.core.region_manager.set_safe_point(region.id, 5);
                engine.core.engine.data[cf_to_id("write")].clone()
            };

            let mut s = 1;
            for seq in 2..50 {
                put_key_val(&sl, "a", "val", 10, s + 1);
                for i in 2..50 {
                    delete_key(&sl, "b", 10, s + i);
                }

                let snapshot = engine.snapshot(range.clone(), 10, s + seq).unwrap();
                let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();
                assert!(iter.seek_to_last().unwrap());
                let k = construct_mvcc_key("a", 10);
                assert_eq!(iter.key(), &k);
                assert_eq!(iter.value(), b"val");
                assert!(!iter.prev().unwrap());
                assert!(!iter.valid().unwrap());
                s += 100;
            }
        }

        // backward, all deletes except for last put, last put's seq
        {
            let engine = RegionCacheMemoryEngine::new(InMemoryEngineContext::new_for_tests(
                Arc::new(VersionTrack::new(InMemoryEngineConfig::config_for_test())),
            ));
            engine.new_region(region.clone());
            let sl = {
                engine.core.region_manager.set_safe_point(region.id, 5);
                engine.core.engine.data[cf_to_id("write")].clone()
            };
            put_key_val(&sl, "a", "val", 10, 1);
            for i in 2..50 {
                delete_key(&sl, "b", 10, i);
            }
            let v = construct_value(50, 50);
            put_key_val(&sl, "b", v.as_str(), 10, 50);
            let snapshot = engine.snapshot(range.clone(), 10, 50).unwrap();
            let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();
            assert!(iter.seek_to_last().unwrap());
            let k = construct_mvcc_key("b", 10);
            let v = construct_value(50, 50);
            assert_eq!(iter.key(), &k);
            assert_eq!(iter.value(), v.as_bytes());

            assert!(iter.prev().unwrap());
            let k = construct_mvcc_key("a", 10);
            assert_eq!(iter.key(), &k);
            assert_eq!(iter.value(), b"val");
            assert!(!iter.prev().unwrap());
            assert!(!iter.valid().unwrap());
        }

        // all deletes except for last put, deletions' seq
        {
            let engine = RegionCacheMemoryEngine::new(InMemoryEngineContext::new_for_tests(
                Arc::new(VersionTrack::new(InMemoryEngineConfig::config_for_test())),
            ));
            engine.new_region(region.clone());
            let sl = {
                engine.core.region_manager.set_safe_point(region.id, 5);
                engine.core.engine.data[cf_to_id("write")].clone()
            };
            let mut s = 1;
            for seq in 2..50 {
                for i in 2..50 {
                    delete_key(&sl, "b", 10, s + i);
                }
                let v = construct_value(50, 50);
                put_key_val(&sl, "b", v.as_str(), 10, s + 50);

                let snapshot = engine.snapshot(range.clone(), 10, s + seq).unwrap();
                let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();
                assert!(!iter.seek_to_first().unwrap());
                assert!(!iter.valid().unwrap());

                assert!(!iter.seek_to_last().unwrap());
                assert!(!iter.valid().unwrap());

                s += 100;
            }
        }
    }

    #[test]
    fn test_prefix_seek() {
        let engine = RegionCacheMemoryEngine::new(InMemoryEngineContext::new_for_tests(Arc::new(
            VersionTrack::new(InMemoryEngineConfig::config_for_test()),
        )));
        let region = new_region(1, b"k000", b"k100");
        let range = CacheRegion::from_region(&region);
        engine.new_region(region.clone());

        {
            engine.core.region_manager.set_safe_point(region.id, 5);
            let sl = engine.core.engine.data[cf_to_id("write")].clone();

            let guard = &epoch::pin();
            for i in 1..5 {
                for mvcc in 10..20 {
                    let user_key = construct_key(i, mvcc);
                    let internal_key = encode_key(&user_key, 10, ValueType::Value);
                    let v = format!("v{:02}{:02}", i, mvcc);
                    sl.insert(internal_key, InternalBytes::from_vec(v.into_bytes()), guard)
                        .release(guard);
                }
            }
        }

        let mut iter_opt = IterOptions::default();
        let lower_bound = construct_user_key(1);
        let upper_bound = construct_user_key(5);
        iter_opt.set_upper_bound(&upper_bound, 0);
        iter_opt.set_lower_bound(&lower_bound, 0);
        iter_opt.set_prefix_same_as_start(true);
        let snapshot = engine.snapshot(range.clone(), u64::MAX, u64::MAX).unwrap();
        let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();

        // prefix seek, forward
        for i in 1..5 {
            let seek_key = construct_key(i, 100);
            assert!(iter.seek(&seek_key).unwrap());
            let mut start = 19;
            while iter.valid().unwrap() {
                let user_key = iter.key();
                let mvcc = !u64::from_be_bytes(user_key[user_key.len() - 8..].try_into().unwrap());
                assert_eq!(mvcc, start);
                let v = format!("v{:02}{:02}", i, start);
                assert_eq!(v.as_bytes(), iter.value());
                start -= 1;
                iter.next().unwrap();
            }
            assert_eq!(start, 9);
        }

        // prefix seek, backward
        for i in 1..5 {
            let seek_key = construct_key(i, 0);
            assert!(iter.seek_for_prev(&seek_key).unwrap());
            let mut start = 10;
            while iter.valid().unwrap() {
                let user_key = iter.key();
                let mvcc = !u64::from_be_bytes(user_key[user_key.len() - 8..].try_into().unwrap());
                assert_eq!(mvcc, start);
                let v = format!("v{:02}{:02}", i, start);
                assert_eq!(v.as_bytes(), iter.value());
                start += 1;
                iter.prev().unwrap();
            }
            assert_eq!(start, 20);
        }
    }

    #[test]
    fn test_skiplist_engine_evict_range() {
        let sl_engine = SkiplistEngine::new();
        sl_engine.data.iter().for_each(|sl| {
            fill_data_in_skiplist(sl.clone(), (1..60).step_by(1), 1..2, 1);
        });

        let evict_range = CacheRegion::new(1, 0, construct_user_key(20), construct_user_key(40));
        sl_engine.delete_range(&evict_range);
        sl_engine.data.iter().for_each(|sl| {
            let mut iter = sl.owned_iter();
            let guard = &epoch::pin();
            iter.seek_to_first(guard);
            for i in 1..20 {
                let internal_key = decode_key(iter.key().as_slice());
                let expected_key = construct_key(i, 1);
                assert_eq!(internal_key.user_key, &expected_key);
                iter.next(guard);
            }

            for i in 40..60 {
                let internal_key = decode_key(iter.key().as_slice());
                let expected_key = construct_key(i, 1);
                assert_eq!(internal_key.user_key, &expected_key);
                iter.next(guard);
            }
            assert!(!iter.valid());
        });
    }

    fn verify_evict_region_deleted(engine: &RegionCacheMemoryEngine, region: &CacheRegion) {
        test_util::eventually(
            Duration::from_millis(100),
            Duration::from_millis(2000),
            || {
                !engine
                    .core
                    .region_manager()
                    .regions_map
                    .read()
                    .regions()
                    .values()
                    .any(|m| m.get_state().is_evict())
            },
        );
        let write_handle = engine.core.engine.cf_handle("write");
        let start_key = encode_seek_key(&region.start, u64::MAX);
        let mut iter = write_handle.iterator();

        let guard = &epoch::pin();
        iter.seek(&start_key, guard);
        let end = encode_seek_key(&region.end, u64::MAX);
        assert!(iter.key() > &end || !iter.valid());
    }

    #[test]
    fn test_evict_region_without_snapshot() {
        let engine = RegionCacheMemoryEngine::new(InMemoryEngineContext::new_for_tests(Arc::new(
            VersionTrack::new(InMemoryEngineConfig::config_for_test()),
        )));
        let region = new_region(1, construct_region_key(0), construct_region_key(30));
        let range = CacheRegion::from_region(&region);
        engine.new_region(region.clone());

        let guard = &epoch::pin();
        {
            engine.core.region_manager.set_safe_point(region.id, 5);
            let sl = engine.core.engine.data[cf_to_id("write")].clone();
            for i in 0..30 {
                let user_key = construct_key(i, 10);
                let internal_key = encode_key(&user_key, 10, ValueType::Value);
                let v = construct_value(i, 10);
                sl.insert(internal_key, InternalBytes::from_vec(v.into_bytes()), guard)
                    .release(guard);
            }
        }

        let new_regions = vec![
            CacheRegion::new(1, 1, construct_user_key(0), construct_user_key(10)),
            CacheRegion::new(2, 1, construct_user_key(10), construct_user_key(20)),
            CacheRegion::new(3, 1, construct_user_key(20), construct_user_key(30)),
        ];

        engine.on_region_event(RegionEvent::Split {
            source: CacheRegion::from_region(&region),
            new_regions: new_regions.clone(),
        });

        let evict_region = new_regions[1].clone();
        engine.evict_region(&evict_region, EvictReason::AutoEvict, None);
        assert_eq!(
            engine.snapshot(range.clone(), 10, 200).unwrap_err(),
            FailedReason::EpochNotMatch
        );
        assert_eq!(
            engine.snapshot(evict_region.clone(), 10, 200).unwrap_err(),
            FailedReason::NotCached
        );

        let r_left = new_regions[0].clone();
        let r_right = new_regions[2].clone();
        let snap_left = engine.snapshot(r_left, 10, 200).unwrap();

        let mut iter_opt = IterOptions::default();
        let lower_bound = construct_user_key(0);
        let upper_bound = construct_user_key(10);
        iter_opt.set_upper_bound(&upper_bound, 0);
        iter_opt.set_lower_bound(&lower_bound, 0);
        let mut iter = snap_left.iterator_opt("write", iter_opt.clone()).unwrap();
        iter.seek_to_first().unwrap();
        verify_key_values(&mut iter, (0..10).step_by(1), 10..11, true, true);

        let snap_right = engine.snapshot(r_right, 10, 200).unwrap();
        let lower_bound = construct_user_key(20);
        let upper_bound = construct_user_key(30);
        iter_opt.set_upper_bound(&upper_bound, 0);
        iter_opt.set_lower_bound(&lower_bound, 0);
        let mut iter = snap_right.iterator_opt("write", iter_opt).unwrap();
        iter.seek_to_first().unwrap();
        verify_key_values(&mut iter, (20..30).step_by(1), 10..11, true, true);

        // verify the key, values are delete
        verify_evict_region_deleted(&engine, &evict_region);
    }

    #[test]
    fn test_evict_range_with_snapshot() {
        let engine = RegionCacheMemoryEngine::new(InMemoryEngineContext::new_for_tests(Arc::new(
            VersionTrack::new(InMemoryEngineConfig::config_for_test()),
        )));
        let region = new_region(1, construct_region_key(0), construct_region_key(30));
        engine.new_region(region.clone());

        let guard = &epoch::pin();
        {
            engine.core.region_manager.set_safe_point(region.id, 5);
            let sl = engine.core.engine.data[cf_to_id("write")].clone();
            for i in 0..30 {
                let user_key = construct_key(i, 10);
                let internal_key = encode_key(&user_key, 10, ValueType::Value);
                let v = construct_value(i, 10);
                sl.insert(
                    internal_key,
                    InternalBytes::from_vec(v.clone().into_bytes()),
                    guard,
                )
                .release(guard);
            }
        }

        let cache_region = CacheRegion::from_region(&region);
        let s1 = engine.snapshot(cache_region.clone(), 10, 10);
        let s2 = engine.snapshot(cache_region.clone(), 20, 20);

        let new_regions = vec![
            CacheRegion::new(1, 1, construct_user_key(0), construct_user_key(10)),
            CacheRegion::new(2, 1, construct_user_key(10), construct_user_key(20)),
            CacheRegion::new(3, 1, construct_user_key(20), construct_user_key(30)),
        ];
        engine.on_region_event(RegionEvent::Split {
            source: cache_region.clone(),
            new_regions: new_regions.clone(),
        });

        let evict_region = new_regions[1].clone();
        engine.evict_region(&evict_region, EvictReason::AutoEvict, None);

        let r_left = new_regions[0].clone();
        let s3 = engine.snapshot(r_left.clone(), 20, 20).unwrap();
        let r_right = new_regions[2].clone();
        let s4 = engine.snapshot(r_right, 20, 20).unwrap();

        drop(s3);
        engine.evict_region(&r_left, EvictReason::AutoEvict, None);

        // todo(SpadeA): memory limiter
        {
            // evict_range is not eligible for delete
            assert_eq!(
                engine
                    .core
                    .region_manager()
                    .regions_map
                    .read()
                    .region_meta(evict_region.id)
                    .unwrap()
                    .get_state(),
                RegionState::PendingEvict
            );
        }

        drop(s1);
        {
            // evict_range is still not eligible for delete
            assert_eq!(
                engine
                    .core
                    .region_manager()
                    .regions_map
                    .read()
                    .region_meta(evict_region.id)
                    .unwrap()
                    .get_state(),
                RegionState::PendingEvict
            );
        }
        drop(s2);
        // Now, all snapshots before evicting `evict_range` are released
        verify_evict_region_deleted(&engine, &evict_region);

        drop(s4);
        verify_evict_region_deleted(&engine, &r_left);
    }

    #[test]
    fn test_tombstone_count_when_iterating() {
        let engine = RegionCacheMemoryEngine::new(InMemoryEngineContext::new_for_tests(Arc::new(
            VersionTrack::new(InMemoryEngineConfig::config_for_test()),
        )));
        let region = new_region(1, b"", b"z");
        let range = CacheRegion::from_region(&region);
        engine.new_region(region.clone());

        {
            engine.core.region_manager.set_safe_point(region.id, 5);
            let sl = engine.core.engine.data[cf_to_id("write")].clone();

            delete_key(&sl, "a", 10, 5);
            delete_key(&sl, "b", 10, 5);
            put_key_val(&sl, "c", "valc", 10, 5);
            put_key_val(&sl, "d", "vald", 10, 5);
            put_key_val(&sl, "e", "vale", 10, 5);
            delete_key(&sl, "f", 10, 5);
            delete_key(&sl, "g", 10, 5);
        }

        let mut iter_opt = IterOptions::default();
        iter_opt.set_upper_bound(&range.end, 0);
        iter_opt.set_lower_bound(&range.start, 0);
        let snapshot = engine.snapshot(range.clone(), u64::MAX, 100).unwrap();
        let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();
        iter.seek_to_first().unwrap();
        while iter.valid().unwrap() {
            iter.next().unwrap();
        }

        let collector = iter.metrics_collector();
        assert_eq!(4, collector.internal_delete_skipped_count());
        assert_eq!(3, collector.internal_key_skipped_count());

        iter.seek_to_last().unwrap();
        while iter.valid().unwrap() {
            iter.prev().unwrap();
        }
        assert_eq!(8, collector.internal_delete_skipped_count());
        assert_eq!(10, collector.internal_key_skipped_count());
    }

    #[test]
    fn test_read_flow_metrics() {
        let engine = RegionCacheMemoryEngine::new(InMemoryEngineContext::new_for_tests(Arc::new(
            VersionTrack::new(InMemoryEngineConfig::config_for_test()),
        )));
        let region = new_region(1, b"", b"z");
        let range = CacheRegion::from_region(&region);
        engine.new_region(region.clone());

        {
            engine.core.region_manager.set_safe_point(region.id, 5);
            let sl = engine.core.engine.data[cf_to_id("write")].clone();

            put_key_val(&sl, "a", "val", 10, 5);
            put_key_val(&sl, "b", "vall", 10, 5);
            put_key_val(&sl, "c", "valll", 10, 5);
            put_key_val(&sl, "d", "vallll", 10, 5);
        }

        // Also write data to rocksdb for verification
        let path = Builder::new().prefix("temp").tempdir().unwrap();
        let mut db_opts = RocksDbOptions::default();
        let rocks_statistics = RocksStatistics::new_titan();
        db_opts.set_statistics(&rocks_statistics);
        let cf_opts = [CF_DEFAULT, CF_LOCK, CF_WRITE]
            .iter()
            .map(|name| (*name, Default::default()))
            .collect();
        let rocks_engine = new_engine_opt(path.path().to_str().unwrap(), db_opts, cf_opts).unwrap();
        {
            let mut wb = rocks_engine.write_batch();
            let key = construct_mvcc_key("a", 10);
            wb.put_cf("write", &key, b"val").unwrap();
            let key = construct_mvcc_key("b", 10);
            wb.put_cf("write", &key, b"vall").unwrap();
            let key = construct_mvcc_key("c", 10);
            wb.put_cf("write", &key, b"valll").unwrap();
            let key = construct_mvcc_key("d", 10);
            wb.put_cf("write", &key, b"vallll").unwrap();
            let _ = wb.write();
        }

        let statistics = engine.statistics();
        let snapshot = engine.snapshot(range.clone(), u64::MAX, 100).unwrap();
        assert_eq!(PERF_CONTEXT.with(|c| c.borrow().get_read_bytes), 0);
        let key = construct_mvcc_key("a", 10);
        snapshot.get_value_cf("write", &key).unwrap();
        rocks_engine.get_value_cf("write", &key).unwrap();
        assert_eq!(PERF_CONTEXT.with(|c| c.borrow().get_read_bytes), 3);
        let key = construct_mvcc_key("b", 10);
        snapshot.get_value_cf("write", &key).unwrap();
        rocks_engine.get_value_cf("write", &key).unwrap();
        assert_eq!(PERF_CONTEXT.with(|c| c.borrow().get_read_bytes), 7);
        let key = construct_mvcc_key("c", 10);
        snapshot.get_value_cf("write", &key).unwrap();
        rocks_engine.get_value_cf("write", &key).unwrap();
        assert_eq!(PERF_CONTEXT.with(|c| c.borrow().get_read_bytes), 12);
        let key = construct_mvcc_key("d", 10);
        snapshot.get_value_cf("write", &key).unwrap();
        rocks_engine.get_value_cf("write", &key).unwrap();
        assert_eq!(PERF_CONTEXT.with(|c| c.borrow().get_read_bytes), 18);
        assert_eq!(statistics.get_ticker_count(Tickers::BytesRead), 18);
        assert_eq!(
            rocks_statistics.get_and_reset_ticker_count(DBStatisticsTickerType::BytesRead),
            statistics.get_and_reset_ticker_count(Tickers::BytesRead)
        );

        let mut iter_opt = IterOptions::default();
        iter_opt.set_upper_bound(&range.end, 0);
        iter_opt.set_lower_bound(&range.start, 0);
        let mut rocks_iter = rocks_engine
            .iterator_opt("write", iter_opt.clone())
            .unwrap();
        let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();
        assert_eq!(PERF_CONTEXT.with(|c| c.borrow().iter_read_bytes), 0);
        iter.seek_to_first().unwrap();
        rocks_iter.seek_to_first().unwrap();
        let key = construct_mvcc_key("b", 10);
        iter.seek(&key).unwrap();
        rocks_iter.seek(&key).unwrap();
        iter.next().unwrap();
        rocks_iter.next().unwrap();
        iter.next().unwrap();
        rocks_iter.next().unwrap();
        drop(iter);
        assert_eq!(PERF_CONTEXT.with(|c| c.borrow().iter_read_bytes), 58);
        assert_eq!(2, statistics.get_ticker_count(Tickers::NumberDbSeek));
        assert_eq!(2, statistics.get_ticker_count(Tickers::NumberDbSeekFound));
        assert_eq!(2, statistics.get_ticker_count(Tickers::NumberDbNext));
        assert_eq!(2, statistics.get_ticker_count(Tickers::NumberDbNextFound));

        let mut iter = snapshot.iterator_opt("write", iter_opt.clone()).unwrap();
        iter.seek_to_last().unwrap();
        rocks_iter.seek_to_last().unwrap();
        iter.prev().unwrap();
        rocks_iter.prev().unwrap();
        iter.prev().unwrap();
        rocks_iter.prev().unwrap();
        iter.prev().unwrap();
        rocks_iter.prev().unwrap();
        drop(rocks_iter);
        drop(iter);
        assert_eq!(statistics.get_ticker_count(Tickers::IterBytesRead), 116);
        assert_eq!(
            rocks_statistics.get_and_reset_ticker_count(DBStatisticsTickerType::IterBytesRead),
            statistics.get_and_reset_ticker_count(Tickers::IterBytesRead)
        );
        assert_eq!(PERF_CONTEXT.with(|c| c.borrow().iter_read_bytes), 116);
        assert_eq!(3, statistics.get_ticker_count(Tickers::NumberDbSeek));
        assert_eq!(3, statistics.get_ticker_count(Tickers::NumberDbSeekFound));
        assert_eq!(3, statistics.get_ticker_count(Tickers::NumberDbPrev));
        assert_eq!(3, statistics.get_ticker_count(Tickers::NumberDbPrevFound));
    }

    fn set_up_for_iteator<F>(
        wb_sequence: u64,
        snap_sequence: u64,
        put_entries: F,
    ) -> (
        RegionCacheMemoryEngine,
        RegionCacheSnapshot,
        RegionCacheIterator,
    )
    where
        F: FnOnce(&mut RegionCacheWriteBatch),
    {
        let engine = RegionCacheMemoryEngine::new(InMemoryEngineContext::new_for_tests(Arc::new(
            VersionTrack::new(InMemoryEngineConfig::config_for_test()),
        )));
        let region = new_region(1, b"", b"z");
        let range = CacheRegion::from_region(&region);
        engine.new_region(region.clone());

        let mut wb = engine.write_batch();
        wb.prepare_for_region(&region);
        put_entries(&mut wb);
        wb.set_sequence_number(wb_sequence).unwrap();
        wb.write().unwrap();

        let snap = engine.snapshot(range.clone(), 100, snap_sequence).unwrap();
        let mut iter_opt = IterOptions::default();
        iter_opt.set_upper_bound(&range.end, 0);
        iter_opt.set_lower_bound(&range.start, 0);

        let iter = snap.iterator_opt("default", iter_opt).unwrap();
        (engine, snap, iter)
    }

    // copied from RocksDB TEST_F(DBIteratorTest, DBIterator10)
    #[test]
    fn test_iterator() {
        let (.., mut iter) = set_up_for_iteator(100, 200, |wb| {
            wb.put(b"za", b"1").unwrap();
            wb.put(b"zb", b"2").unwrap();
            wb.put(b"zc", b"3").unwrap();
            wb.put(b"zd", b"4").unwrap();
        });

        iter.seek(b"zc").unwrap();
        assert!(iter.valid().unwrap());
        iter.prev().unwrap();
        assert!(iter.valid().unwrap());
        assert_eq!(iter.key(), b"zb");
        assert_eq!(iter.value(), b"2");

        iter.next().unwrap();
        assert!(iter.valid().unwrap());
        assert_eq!(iter.key(), b"zc");
        assert_eq!(iter.value(), b"3");

        iter.seek_for_prev(b"zc").unwrap();
        assert!(iter.valid().unwrap());
        iter.next().unwrap();
        assert!(iter.valid().unwrap());
        assert_eq!(iter.key(), b"zd");
        assert_eq!(iter.value(), b"4");

        iter.prev().unwrap();
        assert!(iter.valid().unwrap());
        assert_eq!(iter.key(), b"zc");
        assert_eq!(iter.value(), b"3");
    }

    // copied from RocksDB TEST_P(DBIteratorTest, IterNextWithNewerSeq) and
    // TEST_P(DBIteratorTest, IterPrevWithNewerSeq)
    #[test]
    fn test_next_with_newer_seq() {
        let (engine, _, mut iter) = set_up_for_iteator(100, 110, |wb| {
            wb.put(b"z0", b"0").unwrap();
            wb.put(b"za", b"b").unwrap();
            wb.put(b"zc", b"d").unwrap();
            wb.put(b"zd", b"e").unwrap();
        });

        let mut wb = engine.write_batch();
        let region = new_region(1, b"", b"z");
        wb.prepare_for_region(&region);
        wb.put(b"zb", b"f").unwrap();
        wb.set_sequence_number(200).unwrap();

        iter.seek(b"za").unwrap();
        assert_eq!(iter.key(), b"za");
        assert_eq!(iter.value(), b"b");

        iter.next().unwrap();
        assert_eq!(iter.key(), b"zc");
        assert_eq!(iter.value(), b"d");

        iter.seek_for_prev(b"zb").unwrap();
        assert_eq!(iter.key(), b"za");
        assert_eq!(iter.value(), b"b");

        iter.next().unwrap();
        assert_eq!(iter.key(), b"zc");
        assert_eq!(iter.value(), b"d");

        iter.seek(b"zd").unwrap();
        assert_eq!(iter.key(), b"zd");
        assert_eq!(iter.value(), b"e");

        iter.prev().unwrap();
        assert_eq!(iter.key(), b"zc");
        assert_eq!(iter.value(), b"d");

        iter.prev().unwrap();
        assert_eq!(iter.key(), b"za");
        assert_eq!(iter.value(), b"b");

        iter.prev().unwrap();
        iter.seek_for_prev(b"zd").unwrap();
        assert_eq!(iter.key(), b"zd");
        assert_eq!(iter.value(), b"e");

        iter.prev().unwrap();
        assert_eq!(iter.key(), b"zc");
        assert_eq!(iter.value(), b"d");

        iter.prev().unwrap();
        assert_eq!(iter.key(), b"za");
        assert_eq!(iter.value(), b"b");
    }

    #[test]
    fn test_reverse_direction() {
        let (engine, ..) = set_up_for_iteator(100, 100, |wb| {
            wb.put(b"a", b"val_a1").unwrap(); // seq 100
            wb.put(b"b", b"val_b1").unwrap(); // seq 101
            wb.put(b"c", b"val_c1").unwrap(); // seq 102

            wb.put(b"a", b"val_a2").unwrap(); // seq 103
            wb.put(b"b", b"val_b2").unwrap(); // seq 104

            wb.put(b"c", b"val_c2").unwrap(); // seq 105
            wb.put(b"a", b"val_a3").unwrap(); // seq 106
            wb.put(b"b", b"val_b3").unwrap(); // seq 107
            wb.put(b"c", b"val_c3").unwrap(); // seq 108
        });

        // For sequence number 102
        let range = CacheRegion::new(1, 0, b"".to_vec(), b"z".to_vec());
        let snap = engine.snapshot(range.clone(), 100, 102).unwrap();
        let mut iter_opt = IterOptions::default();
        iter_opt.set_upper_bound(&range.end, 0);
        iter_opt.set_lower_bound(&range.start, 0);

        let mut iter = snap.iterator_opt("default", iter_opt.clone()).unwrap();
        iter.seek(b"c").unwrap();
        assert_eq!(iter.key(), b"c");
        assert_eq!(iter.value(), b"val_c1");

        iter.prev().unwrap();
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"val_b1");

        iter.seek(b"b").unwrap();
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"val_b1");

        iter.prev().unwrap();
        assert_eq!(iter.key(), b"a");
        assert_eq!(iter.value(), b"val_a1");

        iter.next().unwrap();
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"val_b1");

        iter.seek_for_prev(b"a").unwrap();
        assert_eq!(iter.key(), b"a");
        assert_eq!(iter.value(), b"val_a1");

        iter.next().unwrap();
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"val_b1");

        iter.next().unwrap();
        assert_eq!(iter.key(), b"c");
        assert_eq!(iter.value(), b"val_c1");

        iter.next().unwrap();
        assert!(!iter.valid().unwrap());

        // For sequence number 104
        let snap = engine.snapshot(range.clone(), 100, 104).unwrap();
        let mut iter = snap.iterator_opt("default", iter_opt.clone()).unwrap();
        iter.seek(b"c").unwrap();
        assert_eq!(iter.key(), b"c");
        assert_eq!(iter.value(), b"val_c1");

        iter.prev().unwrap();
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"val_b2");

        iter.seek(b"b").unwrap();
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"val_b2");

        iter.prev().unwrap();
        assert_eq!(iter.key(), b"a");
        assert_eq!(iter.value(), b"val_a2");

        iter.next().unwrap();
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"val_b2");

        iter.seek_for_prev(b"a").unwrap();
        assert_eq!(iter.key(), b"a");
        assert_eq!(iter.value(), b"val_a2");

        iter.next().unwrap();
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"val_b2");

        iter.next().unwrap();
        assert_eq!(iter.key(), b"c");
        assert_eq!(iter.value(), b"val_c1");

        iter.next().unwrap();
        assert!(!iter.valid().unwrap());

        // For sequence number 108
        let snap = engine.snapshot(range.clone(), 100, 108).unwrap();
        let mut iter = snap.iterator_opt("default", iter_opt.clone()).unwrap();
        iter.seek(b"c").unwrap();
        assert_eq!(iter.key(), b"c");
        assert_eq!(iter.value(), b"val_c3");

        iter.prev().unwrap();
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"val_b3");

        iter.seek(b"b").unwrap();
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"val_b3");

        iter.prev().unwrap();
        assert_eq!(iter.key(), b"a");
        assert_eq!(iter.value(), b"val_a3");

        iter.next().unwrap();
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"val_b3");

        iter.seek_for_prev(b"a").unwrap();
        assert_eq!(iter.key(), b"a");
        assert_eq!(iter.value(), b"val_a3");

        iter.next().unwrap();
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"val_b3");

        iter.next().unwrap();
        assert_eq!(iter.key(), b"c");
        assert_eq!(iter.value(), b"val_c3");

        iter.next().unwrap();
        assert!(!iter.valid().unwrap());
    }
}

// Copyright 2016 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    collections::{
        BTreeMap,
        Bound::{Excluded, Included, Unbounded},
        VecDeque,
    },
    fmt::{self, Display, Formatter},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use engine_traits::{
    CF_LOCK, CF_RAFT, DeleteStrategy, KvEngine, Mutable, Range, WriteBatch, WriteOptions,
};
use fail::fail_point;
use kvproto::raft_serverpb::{PeerState, RaftApplyState, RegionLocalState};
use tikv_util::{
    box_err, box_try,
    config::VersionTrack,
    defer, error, info,
    time::Instant,
    warn,
    worker::{Runnable, RunnableWithTimer},
};

use super::metrics::*;
use crate::{
    coprocessor::CoprocessorHost,
    store::{
        ApplyOptions, CasualMessage, Config, SnapEntry, SnapKey, SnapManager, check_abort,
        peer_storage::{
            JOB_STATUS_CANCELLED, JOB_STATUS_CANCELLING, JOB_STATUS_FAILED, JOB_STATUS_FINISHED,
            JOB_STATUS_PENDING, JOB_STATUS_RUNNING,
        },
        snap::{Error, Result, SNAPSHOT_CFS, plain_file_used},
        transport::CasualRouter,
    },
};

const CLEANUP_MAX_REGION_COUNT: usize = 64;

/// Region related task
#[derive(Debug)]
pub enum Task {
    Apply {
        region_id: u64,
        status: Arc<AtomicUsize>,
        peer_id: u64,
        create_time: Instant,
    },
    /// Destroy data between [start_key, end_key).
    ///
    /// The actual deletion may be delayed if the engine is overloaded or a
    /// reader is still referencing the data.
    Destroy {
        region_id: u64,
        start_key: Vec<u8>,
        end_key: Vec<u8>,
    },
}

impl Task {
    pub fn destroy(region_id: u64, start_key: Vec<u8>, end_key: Vec<u8>) -> Task {
        Task::Destroy {
            region_id,
            start_key,
            end_key,
        }
    }
}

impl Display for Task {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match *self {
            Task::Apply { region_id, .. } => write!(f, "Snap apply for {}", region_id),
            Task::Destroy {
                region_id,
                ref start_key,
                ref end_key,
            } => write!(
                f,
                "Destroy {} [{}, {})",
                region_id,
                log_wrappers::Value::key(start_key),
                log_wrappers::Value::key(end_key)
            ),
        }
    }
}

#[derive(Clone)]
struct StalePeerInfo {
    // the start_key is stored as a key in PendingDeleteRanges
    // below are stored as a value in PendingDeleteRanges
    pub region_id: u64,
    pub end_key: Vec<u8>,
    // Once the oldest snapshot sequence exceeds this, it ensures that no one is
    // reading on this peer anymore. So we can safely call `delete_files_in_range`,
    // which may break the consistency of snapshot, of this peer range.
    pub stale_sequence: u64,
}

/// A structure records all ranges to be deleted with some delay.
/// The delay is because there may be some coprocessor requests related to these
/// ranges.
#[derive(Clone, Default)]
struct PendingDeleteRanges {
    ranges: BTreeMap<Vec<u8>, StalePeerInfo>, // start_key -> StalePeerInfo
}

impl PendingDeleteRanges {
    /// Finds ranges that overlap with [start_key, end_key).
    fn find_overlap_ranges(
        &self,
        start_key: &[u8],
        end_key: &[u8],
    ) -> Vec<(u64, Vec<u8>, Vec<u8>, u64)> {
        let mut ranges = Vec::new();
        // find the first range that may overlap with [start_key, end_key)
        let sub_range = self.ranges.range((Unbounded, Excluded(start_key.to_vec())));
        if let Some((s_key, peer_info)) = sub_range.last() {
            if peer_info.end_key > start_key.to_vec() {
                ranges.push((
                    peer_info.region_id,
                    s_key.clone(),
                    peer_info.end_key.clone(),
                    peer_info.stale_sequence,
                ));
            }
        }

        // find the rest ranges that overlap with [start_key, end_key)
        for (s_key, peer_info) in self
            .ranges
            .range((Included(start_key.to_vec()), Excluded(end_key.to_vec())))
        {
            ranges.push((
                peer_info.region_id,
                s_key.clone(),
                peer_info.end_key.clone(),
                peer_info.stale_sequence,
            ));
        }
        ranges
    }

    /// Gets ranges that overlap with [start_key, end_key).
    pub fn drain_overlap_ranges(
        &mut self,
        start_key: &[u8],
        end_key: &[u8],
    ) -> Vec<(u64, Vec<u8>, Vec<u8>, u64)> {
        let ranges = self.find_overlap_ranges(start_key, end_key);

        for (_, s_key, ..) in &ranges {
            self.ranges.remove(s_key).unwrap();
        }
        ranges
    }

    /// Removes and returns the peer info with the `start_key`.
    fn remove(&mut self, start_key: &[u8]) -> Option<(u64, Vec<u8>, Vec<u8>)> {
        self.ranges
            .remove(start_key)
            .map(|peer_info| (peer_info.region_id, start_key.to_owned(), peer_info.end_key))
    }

    /// Inserts a new range waiting to be deleted.
    ///
    /// Before an insert is called, it must call drain_overlap_ranges to clean
    /// the overlapping range.
    fn insert(
        &mut self,
        region_id: u64,
        start_key: Vec<u8>,
        end_key: Vec<u8>,
        stale_sequence: u64,
    ) {
        if !self.find_overlap_ranges(&start_key, &end_key).is_empty() {
            panic!(
                "[region {}] register deleting data in [{}, {}) failed due to overlap",
                region_id,
                log_wrappers::Value::key(&start_key),
                log_wrappers::Value::key(&end_key),
            );
        }
        let info = StalePeerInfo {
            region_id,
            end_key,
            stale_sequence,
        };
        self.ranges.insert(start_key, info);
    }

    /// Gets all stale ranges info.
    pub fn stale_ranges(&self, oldest_sequence: u64) -> impl Iterator<Item = (u64, &[u8], &[u8])> {
        self.ranges
            .iter()
            .filter(move |&(_, info)| info.stale_sequence < oldest_sequence)
            .map(|(start_key, info)| {
                (
                    info.region_id,
                    start_key.as_slice(),
                    info.end_key.as_slice(),
                )
            })
    }

    pub fn len(&self) -> usize {
        self.ranges.len()
    }
}

pub struct Runner<EK, R>
where
    EK: KvEngine,
{
    batch_size: usize,
    use_delete_range: bool,
    ingest_copy_symlink: bool,
    clean_stale_tick: usize,
    clean_stale_check_interval: Duration,
    clean_stale_ranges_tick: usize,

    // we may delay some apply tasks if level 0 files to write stall threshold,
    // pending_applies records all delayed apply task, and will check again later
    pending_applies: VecDeque<Task>,
    // Ranges that have been logically destroyed at a specific sequence number. We can
    // assume there will be no reader (engine snapshot) newer than that sequence number. Therefore,
    // they can be physically deleted with `DeleteFiles` when we're sure there is no older
    // reader as well.
    // To protect this assumption, before a new snapshot is applied, the overlapping pending ranges
    // must first be removed.
    // The sole purpose of maintaining this list is to optimize deletion with `DeleteFiles`
    // whenever we can. Errors while processing them can be ignored.
    pending_delete_ranges: PendingDeleteRanges,

    engine: EK,
    mgr: SnapManager,
    coprocessor_host: CoprocessorHost<EK>,
    router: R,
}

impl<EK, R> Runner<EK, R>
where
    EK: KvEngine,
    R: CasualRouter<EK>,
{
    pub fn new(
        engine: EK,
        mgr: SnapManager,
        cfg: Arc<VersionTrack<Config>>,
        coprocessor_host: CoprocessorHost<EK>,
        router: R,
    ) -> Runner<EK, R> {
        Runner {
            batch_size: cfg.value().snap_apply_batch_size.0 as usize,
            use_delete_range: cfg.value().use_delete_range,
            ingest_copy_symlink: cfg.value().snap_apply_copy_symlink,
            clean_stale_tick: 0,
            clean_stale_check_interval: Duration::from_millis(
                cfg.value().region_worker_tick_interval.as_millis(),
            ),
            clean_stale_ranges_tick: cfg.value().clean_stale_ranges_tick,
            pending_applies: VecDeque::new(),
            pending_delete_ranges: PendingDeleteRanges::default(),
            engine,
            mgr,
            coprocessor_host,
            router,
        }
    }

    fn region_state(&self, region_id: u64) -> Result<RegionLocalState> {
        let region_key = keys::region_state_key(region_id);
        let region_state: RegionLocalState =
            match box_try!(self.engine.get_msg_cf(CF_RAFT, &region_key)) {
                Some(state) => state,
                None => {
                    return Err(box_err!(
                        "failed to get region_state from {}",
                        log_wrappers::Value::key(&region_key)
                    ));
                }
            };
        Ok(region_state)
    }

    fn apply_state(&self, region_id: u64) -> Result<RaftApplyState> {
        let state_key = keys::apply_state_key(region_id);
        let apply_state: RaftApplyState =
            match box_try!(self.engine.get_msg_cf(CF_RAFT, &state_key)) {
                Some(state) => state,
                None => {
                    return Err(box_err!(
                        "failed to get apply_state from {}",
                        log_wrappers::Value::key(&state_key)
                    ));
                }
            };
        Ok(apply_state)
    }

    /// Applies snapshot data of the Region.
    fn apply_snap(&mut self, region_id: u64, peer_id: u64, abort: Arc<AtomicUsize>) -> Result<()> {
        info!("begin apply snap data"; "region_id" => region_id, "peer_id" => peer_id);
        fail_point!("region_apply_snap", |_| { Ok(()) });
        fail_point!("region_apply_snap_io_err", |_| {
            Err(crate::store::SnapError::Other(box_err!("io error")))
        });
        check_abort(&abort)?;

        let mut region_state = self.region_state(region_id)?;
        let region = region_state.get_region().clone();

        let start_key = keys::enc_start_key(&region);
        let end_key = keys::enc_end_key(&region);
        check_abort(&abort)?;
        self.clean_overlap_ranges(start_key, end_key)?;
        check_abort(&abort)?;
        fail_point!("apply_snap_cleanup_range", |_| { Ok(()) });

        // apply snapshot
        let apply_state = self.apply_state(region_id)?;
        let term = apply_state.get_truncated_state().get_term();
        let idx = apply_state.get_truncated_state().get_index();
        let snap_key = SnapKey::new(region_id, term, idx);
        self.mgr.register(snap_key.clone(), SnapEntry::Applying);
        defer!({
            self.mgr.deregister(&snap_key, &SnapEntry::Applying);
        });
        let mut s = box_try!(self.mgr.get_snapshot_for_applying(&snap_key));
        if !s.exists() {
            return Err(box_err!("missing snapshot file {}", s.path()));
        }
        check_abort(&abort)?;
        let timer = Instant::now();
        let options = ApplyOptions {
            db: self.engine.clone(),
            region: region.clone(),
            abort: Arc::clone(&abort),
            write_batch_size: self.batch_size,
            coprocessor_host: self.coprocessor_host.clone(),
            ingest_copy_symlink: self.ingest_copy_symlink,
        };
        s.apply(options)?;
        self.coprocessor_host
            .post_apply_snapshot(&region, peer_id, &snap_key, Some(&s));

        fail_point!("region_apply_snap_before_write", |_| { Ok(()) });
        // Delete snapshot state and assure the relative region state and snapshot state
        // is updated and flushed into kvdb.
        region_state.set_state(PeerState::Normal);
        let mut wb = self.engine.write_batch();
        box_try!(wb.put_msg_cf(CF_RAFT, &keys::region_state_key(region_id), &region_state));
        box_try!(wb.delete_cf(CF_RAFT, &keys::snapshot_raft_state_key(region_id)));
        let mut wopts = WriteOptions::default();
        wopts.set_sync(true);
        wb.write_opt(&wopts).unwrap_or_else(|e| {
            panic!("{} failed to save apply_snap result: {:?}", region_id, e);
        });
        self.coprocessor_host
            .on_apply_snapshot_committed(&region, peer_id, &snap_key, Some(&s));
        info!(
            "apply new data";
            "region_id" => region_id,
            "time_takes" => ?timer.saturating_elapsed(),
        );
        fail_point!("apply_snapshot_finished");
        Ok(())
    }

    /// Tries to apply the snapshot of the specified Region. It calls
    /// `apply_snap` to do the actual work.
    fn handle_apply(&mut self, region_id: u64, peer_id: u64, status: Arc<AtomicUsize>) {
        let _ = status.compare_exchange(
            JOB_STATUS_PENDING,
            JOB_STATUS_RUNNING,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        SNAP_COUNTER.apply.start.inc();

        let start = Instant::now();

        let tombstone = match self.apply_snap(region_id, peer_id, Arc::clone(&status)) {
            Ok(()) => {
                fail_point!("region_apply_return_not_change_state");
                status.swap(JOB_STATUS_FINISHED, Ordering::SeqCst);
                SNAP_COUNTER.apply.success.inc();
                false
            }
            Err(Error::Abort) => {
                warn!("applying snapshot is aborted"; "region_id" => region_id);
                self.coprocessor_host
                    .cancel_apply_snapshot(region_id, peer_id);
                assert_eq!(
                    status.swap(JOB_STATUS_CANCELLED, Ordering::SeqCst),
                    JOB_STATUS_CANCELLING
                );
                SNAP_COUNTER.apply.abort.inc();
                // The snapshot is applied abort, it's not necessary to tombstone the peer.
                false
            }
            Err(e) => {
                warn!("failed to apply snap!!!"; "region_id" => region_id, "err" => %e);
                self.coprocessor_host
                    .cancel_apply_snapshot(region_id, peer_id);
                status.swap(JOB_STATUS_FAILED, Ordering::SeqCst);
                SNAP_COUNTER.apply.fail.inc();
                // As the snapshot failed, the related peer should be marked tombstone.
                // And as for the abnormal snapshot, it will be automatically cleaned up by
                // the CleanupWorker later.
                true
            }
        };

        SNAP_HISTOGRAM
            .apply
            .observe(start.saturating_elapsed_secs());
        let _ = self.router.send(
            region_id,
            CasualMessage::SnapshotApplied { peer_id, tombstone },
        );
    }

    /// Tries to clean up files in pending ranges overlapping with the given
    /// bounds. These pending ranges will be removed. Returns an updated range
    /// that also includes these ranges. Caller must ensure the remaining keys
    /// in the returning range will be deleted properly.
    fn clean_overlap_ranges_roughly(
        &mut self,
        mut start_key: Vec<u8>,
        mut end_key: Vec<u8>,
    ) -> (Vec<u8>, Vec<u8>) {
        let overlap_ranges = self
            .pending_delete_ranges
            .drain_overlap_ranges(&start_key, &end_key);
        if overlap_ranges.is_empty() {
            return (start_key, end_key);
        }
        CLEAN_COUNTER_VEC.with_label_values(&["overlap"]).inc();
        let oldest_sequence = self
            .engine
            .get_oldest_snapshot_sequence_number()
            .unwrap_or(u64::MAX);
        let df_ranges: Vec<_> = overlap_ranges
            .iter()
            .filter_map(|(region_id, cur_start, cur_end, stale_sequence)| {
                info!(
                    "delete data in range because of overlap"; "region_id" => region_id,
                    "start_key" => log_wrappers::Value::key(cur_start),
                    "end_key" => log_wrappers::Value::key(cur_end)
                );
                if &start_key > cur_start {
                    start_key = cur_start.clone();
                }
                if &end_key < cur_end {
                    end_key = cur_end.clone();
                }
                if *stale_sequence < oldest_sequence {
                    Some(Range::new(cur_start, cur_end))
                } else {
                    SNAP_COUNTER_VEC
                        .with_label_values(&["overlap", "not_delete_files"])
                        .inc();
                    None
                }
            })
            .collect();
        self.engine
            .delete_ranges_cfs(
                &WriteOptions::default(),
                DeleteStrategy::DeleteFiles,
                &df_ranges,
            )
            .map_err(|e| {
                error!("failed to delete files in range"; "err" => %e);
            })
            .unwrap();
        (start_key, end_key)
    }

    /// Cleans up data in the given range and all pending ranges overlapping
    /// with it.
    fn clean_overlap_ranges(&mut self, start_key: Vec<u8>, end_key: Vec<u8>) -> Result<()> {
        fail_point!("before_clean_overlap_ranges", |_| { Ok(()) });
        let (start_key, end_key) = self.clean_overlap_ranges_roughly(start_key, end_key);
        // We enable the `allow_write_during_ingestion` to enable RocksDB
        // IngestExternalFileOptions.allow_write = true, minimizing the impact on
        // foreground performance.
        // This is safe because no overlapping writes occur during ingestion; for the
        // reason, refer to the comments in `apply_sst_cf_files_by_ingest`.
        self.delete_all_in_range(
            &[Range::new(&start_key, &end_key)],
            false,
            true, // allow_write_during_ingestion
        )
    }

    /// Inserts a new pending range, and it will be cleaned up with some delay.
    fn insert_pending_delete_range(
        &mut self,
        region_id: u64,
        start_key: Vec<u8>,
        end_key: Vec<u8>,
    ) {
        let (start_key, end_key) = self.clean_overlap_ranges_roughly(start_key, end_key);
        info!("register deleting data in range";
            "region_id" => region_id,
            "start_key" => log_wrappers::Value::key(&start_key),
            "end_key" => log_wrappers::Value::key(&end_key),
        );
        let seq = self.engine.get_latest_sequence_number();
        self.pending_delete_ranges
            .insert(region_id, start_key, end_key, seq);
    }

    /// Cleans up stale ranges.
    fn clean_stale_ranges(&mut self) {
        fail_point!("before_clean_stale_ranges", |_| {});
        if self.mgr.is_offlined() {
            info!("no need to clear stale range as store is marked offlined");
            return;
        }
        STALE_PEER_PENDING_DELETE_RANGE_GAUGE.set(self.pending_delete_ranges.len() as f64);
        if self.ingest_maybe_stall() {
            return;
        }
        let oldest_sequence = self
            .engine
            .get_oldest_snapshot_sequence_number()
            .unwrap_or(u64::MAX);
        let mut region_ranges: Vec<(u64, Vec<u8>, Vec<u8>)> = self
            .pending_delete_ranges
            .stale_ranges(oldest_sequence)
            .map(|(region_id, s, e)| (region_id, s.to_vec(), e.to_vec()))
            .collect();
        if region_ranges.is_empty() {
            return;
        }
        CLEAN_COUNTER_VEC.with_label_values(&["destroy"]).inc_by(1);
        region_ranges.sort_by(|a, b| a.1.cmp(&b.1));
        region_ranges.truncate(CLEANUP_MAX_REGION_COUNT);
        let ranges: Vec<_> = region_ranges
            .iter()
            .map(|(region_id, start, end)| {
                info!("delete data in range because of stale"; "region_id" => region_id,
                    "start_key" => log_wrappers::Value::key(start),
                    "end_key" => log_wrappers::Value::key(end));
                Range::new(start, end)
            })
            .collect();
        // Clear sst files belonging to the given range directly.
        self.engine
            .delete_ranges_cfs(
                &WriteOptions::default(),
                DeleteStrategy::DeleteFiles,
                &ranges,
            )
            .map_err(|e| {
                error!("failed to delete files in range"; "err" => %e);
            })
            .unwrap();
        fail_point!("after_delete_files_in_range", |_| {});
        // Remove all overlapped ranges directly without ingesting.
        if let Err(e) = self.delete_all_in_range(&ranges, true, false) {
            error!("failed to cleanup stale range"; "err" => %e);
            return;
        }
        // Clear related blob files belonging to the given range directly after clearing
        // all stale keys in sst files.
        self.engine
            .delete_ranges_cfs(
                &WriteOptions::default(),
                DeleteStrategy::DeleteBlobs,
                &ranges,
            )
            .map_err(|e| {
                error!("failed to delete blobs in range"; "err" => %e);
            })
            .unwrap();

        for (_, key, _) in region_ranges {
            assert!(
                self.pending_delete_ranges.remove(&key).is_some(),
                "cleanup pending_delete_ranges {} should exist",
                log_wrappers::Value::key(&key)
            );
        }
    }

    /// Checks the number of files at level 0 to avoid write stall after
    /// ingesting sst. Returns true if the ingestion causes write stall.
    fn ingest_maybe_stall(&self) -> bool {
        for cf in SNAPSHOT_CFS {
            // no need to check lock cf
            if plain_file_used(cf) {
                continue;
            }
            if self.engine.ingest_maybe_slowdown_writes(cf, 0).expect("cf") {
                return true;
            }
        }
        false
    }

    fn delete_all_in_range(
        &self,
        ranges: &[Range<'_>],
        forcely_by_key: bool,
        allow_write_during_ingestion: bool,
    ) -> Result<()> {
        let wopts = WriteOptions::default();
        for cf in self.engine.cf_names() {
            // CF_LOCK usually contains fewer keys than other CFs, so we delete them by key.
            let (strategy, observer) = if cf == CF_LOCK {
                (
                    DeleteStrategy::DeleteByKey,
                    &CLEAR_OVERLAP_REGION_DURATION.by_key,
                )
            } else if self.use_delete_range {
                (
                    DeleteStrategy::DeleteByRange,
                    &CLEAR_OVERLAP_REGION_DURATION.by_range,
                )
            // Meanwhile, if `forcely_by_key` is specified and
            // `cfg.use_delete_range` is not enabled, it should remove the
            // range by delete keys rather than ingestion.
            } else if forcely_by_key {
                (
                    DeleteStrategy::DeleteByKey,
                    &CLEAR_OVERLAP_REGION_DURATION.by_key,
                )
            } else {
                // Deprecated method for clearing stale ranges due to potential latency
                // jitters. It's still applicable when clearing overlapped regions
                // before applying the newly added snapshot.
                (
                    DeleteStrategy::DeleteByWriter {
                        sst_path: self.mgr.get_temp_path_for_ingest(),
                        allow_write_during_ingestion,
                    },
                    &CLEAR_OVERLAP_REGION_DURATION.by_ingest_files,
                )
            };
            let start = Instant::now();
            box_try!(self.engine.delete_ranges_cf(&wopts, cf, strategy, ranges));
            observer.observe(start.saturating_elapsed_secs());
        }

        Ok(())
    }

    /// Calls observer `pre_apply_snapshot` for every task.
    /// Multiple task can be `pre_apply_snapshot` at the same time.
    fn pre_apply_snapshot(&self, task: &Task) -> Result<()> {
        let (region_id, abort, peer_id) = match task {
            Task::Apply {
                region_id,
                status,
                peer_id,
                ..
            } => (region_id, status.clone(), peer_id),
            _ => panic!("invalid apply snapshot task"),
        };

        let region_state = self.region_state(*region_id)?;
        let apply_state = self.apply_state(*region_id)?;

        check_abort(&abort)?;

        let term = apply_state.get_truncated_state().get_term();
        let idx = apply_state.get_truncated_state().get_index();
        let snap_key = SnapKey::new(*region_id, term, idx);
        let s = box_try!(self.mgr.get_snapshot_for_applying(&snap_key));
        if !s.exists() {
            self.coprocessor_host.pre_apply_snapshot(
                region_state.get_region(),
                *peer_id,
                &snap_key,
                None,
            );
            return Err(box_err!("missing snapshot file {}", s.path()));
        }
        check_abort(&abort)?;
        self.coprocessor_host.pre_apply_snapshot(
            region_state.get_region(),
            *peer_id,
            &snap_key,
            Some(&s),
        );
        Ok(())
    }

    /// Tries to apply pending tasks if there is some.
    fn handle_pending_applies(&mut self, is_timeout: bool) {
        fail_point!("apply_pending_snapshot", |_| {});
        let mut new_batch = true;
        while !self.pending_applies.is_empty() {
            // should not handle too many applies than the number of files that can be
            // ingested. check level 0 every time because we can not make sure
            // how does the number of level 0 files change.
            if self.ingest_maybe_stall() {
                SNAP_COUNTER.apply.ingest_delay.inc();
                break;
            }
            if let Some(Task::Apply { region_id, .. }) = self.pending_applies.front() {
                fail_point!("handle_new_pending_applies", |_| {});
                if !self.engine.can_apply_snapshot(
                    is_timeout,
                    new_batch,
                    *region_id,
                    self.pending_applies.len(),
                ) {
                    // KvEngine can't apply snapshot for other reasons.
                    SNAP_COUNTER.apply.ingest_delay.inc();
                    break;
                }
                if let Some(Task::Apply {
                    region_id,
                    status,
                    peer_id,
                    create_time,
                }) = self.pending_applies.pop_front()
                {
                    SNAP_APPLY_WAIT_DURATION_HISTOGRAM
                        .observe(create_time.saturating_elapsed_secs());
                    new_batch = false;
                    self.handle_apply(region_id, peer_id, status);
                    self.mgr.set_pending_apply_count(self.pending_applies.len());
                }
            }
        }
        SNAP_PENDING_APPLIES_GAUGE.set(self.pending_applies.len() as i64);
    }
}

impl<EK, R> Runnable for Runner<EK, R>
where
    EK: KvEngine,
    R: CasualRouter<EK> + Send + Clone + 'static,
{
    type Task = Task;

    fn run(&mut self, task: Task) {
        match task {
            task @ Task::Apply { .. } => {
                fail_point!("on_region_worker_apply", true, |_| {});
                if self.coprocessor_host.should_pre_apply_snapshot() {
                    let _ = self.pre_apply_snapshot(&task);
                }
                SNAP_COUNTER.apply.all.inc();
                // to makes sure applying snapshots in order.
                self.pending_applies.push_back(task);
                self.mgr.set_pending_apply_count(self.pending_applies.len());
                self.handle_pending_applies(false);
                if !self.pending_applies.is_empty() {
                    // delay the apply and retry later
                    SNAP_COUNTER.apply.delay.inc()
                }
            }
            Task::Destroy {
                region_id,
                start_key,
                end_key,
            } => {
                fail_point!("on_region_worker_destroy", true, |_| {});
                // try to delay the range deletion because
                // there might be a coprocessor request related to this range
                self.insert_pending_delete_range(region_id, start_key, end_key);
                self.clean_stale_ranges();
                fail_point!("after_region_worker_destroy");
            }
        }
    }
}

impl<EK, R> RunnableWithTimer for Runner<EK, R>
where
    EK: KvEngine,
    R: CasualRouter<EK> + Send + Clone + 'static,
{
    fn on_timeout(&mut self) {
        self.handle_pending_applies(true);
        self.clean_stale_tick += 1;
        if self.clean_stale_tick >= self.clean_stale_ranges_tick {
            self.clean_stale_ranges();
            self.clean_stale_tick = 0;
        }
    }

    fn get_interval(&self) -> Duration {
        self.clean_stale_check_interval
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        io,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize},
            mpsc,
        },
        thread,
        time::Duration,
    };

    use engine_test::{ctor::CfOptions, kv::KvTestEngine};
    use engine_traits::{
        CF_DEFAULT, CF_WRITE, CompactExt, FlowControlFactorsExt, KvEngine, MiscExt, Mutable,
        Peekable, RaftEngineReadOnly, SyncMutable, WriteBatch, WriteBatchExt,
    };
    use keys::data_key;
    use kvproto::raft_serverpb::{PeerState, RaftApplyState, RaftSnapshotData, RegionLocalState};
    use pd_client::RpcClient;
    use protobuf::Message;
    use tempfile::Builder;
    use tikv_util::{
        config::{ReadableDuration, ReadableSize},
        worker::{LazyWorker, Worker},
    };

    use super::*;
    use crate::{
        coprocessor::{
            ApplySnapshotObserver, BoxApplySnapshotObserver, Coprocessor, CoprocessorHost,
            ObserverContext,
        },
        store::{
            CasualMessage, SnapKey, SnapManager,
            peer_storage::JOB_STATUS_PENDING,
            snap::tests::get_test_db_for_regions,
            worker::{RegionRunner, SnapGenRunner, SnapGenTask},
        },
    };

    const PENDING_APPLY_CHECK_INTERVAL: Duration = Duration::from_millis(200);
    const STALE_PEER_CHECK_TICK: usize = 1;

    pub fn make_raftstore_cfg(use_delete_range: bool) -> Arc<VersionTrack<Config>> {
        let mut store_cfg = Config::default();
        store_cfg.snap_apply_batch_size = ReadableSize(0);
        store_cfg.region_worker_tick_interval = ReadableDuration(PENDING_APPLY_CHECK_INTERVAL);
        store_cfg.clean_stale_ranges_tick = STALE_PEER_CHECK_TICK;
        store_cfg.use_delete_range = use_delete_range;
        store_cfg.snap_generator_pool_size = 2;
        Arc::new(VersionTrack::new(store_cfg))
    }

    fn insert_range(
        pending_delete_ranges: &mut PendingDeleteRanges,
        id: u64,
        s: &str,
        e: &str,
        stale_sequence: u64,
    ) {
        pending_delete_ranges.insert(
            id,
            s.as_bytes().to_owned(),
            e.as_bytes().to_owned(),
            stale_sequence,
        );
    }

    #[test]
    #[allow(clippy::string_lit_as_bytes)]
    fn test_pending_delete_ranges() {
        let mut pending_delete_ranges = PendingDeleteRanges::default();
        let id = 0;

        let timeout1 = 10;
        insert_range(&mut pending_delete_ranges, id, "a", "c", timeout1);
        insert_range(&mut pending_delete_ranges, id, "m", "n", timeout1);
        insert_range(&mut pending_delete_ranges, id, "x", "z", timeout1);
        insert_range(&mut pending_delete_ranges, id + 1, "f", "i", timeout1);
        insert_range(&mut pending_delete_ranges, id + 1, "p", "t", timeout1);
        assert_eq!(pending_delete_ranges.len(), 5);

        //  a____c    f____i    m____n    p____t    x____z
        //              g___________________q
        // when we want to insert [g, q), we first extract overlap ranges,
        // which are [f, i), [m, n), [p, t)
        let timeout2 = 12;
        let overlap_ranges = pending_delete_ranges.drain_overlap_ranges(b"g", b"q");
        assert_eq!(
            overlap_ranges,
            [
                (id + 1, b"f".to_vec(), b"i".to_vec(), timeout1),
                (id, b"m".to_vec(), b"n".to_vec(), timeout1),
                (id + 1, b"p".to_vec(), b"t".to_vec(), timeout1),
            ]
        );
        assert_eq!(pending_delete_ranges.len(), 2);
        insert_range(&mut pending_delete_ranges, id + 2, "g", "q", timeout2);
        assert_eq!(pending_delete_ranges.len(), 3);

        // at t1, [a, c) and [x, z) will timeout
        {
            let now = 11;
            let ranges: Vec<_> = pending_delete_ranges.stale_ranges(now).collect();
            assert_eq!(
                ranges,
                [
                    (id, "a".as_bytes(), "c".as_bytes()),
                    (id, "x".as_bytes(), "z".as_bytes()),
                ]
            );
            for start_key in ranges
                .into_iter()
                .map(|(_, start, _)| start.to_vec())
                .collect::<Vec<Vec<u8>>>()
            {
                pending_delete_ranges.remove(&start_key);
            }
            assert_eq!(pending_delete_ranges.len(), 1);
        }

        // at t2, [g, q) will timeout
        {
            let now = 14;
            let ranges: Vec<_> = pending_delete_ranges.stale_ranges(now).collect();
            assert_eq!(ranges, [(id + 2, "g".as_bytes(), "q".as_bytes())]);
            for start_key in ranges
                .into_iter()
                .map(|(_, start, _)| start.to_vec())
                .collect::<Vec<Vec<u8>>>()
            {
                pending_delete_ranges.remove(&start_key);
            }
            assert_eq!(pending_delete_ranges.len(), 0);
        }
    }

    #[test]
    fn test_stale_peer() {
        let temp_dir = Builder::new().prefix("test_stale_peer").tempdir().unwrap();
        let engine = get_test_db_for_regions(&temp_dir, None, None, None, &[1]).unwrap();

        let snap_dir = Builder::new().prefix("snap_dir").tempdir().unwrap();
        let mgr = SnapManager::new(snap_dir.path().to_str().unwrap());
        let bg_worker = Worker::new("region-worker");
        let mut worker: LazyWorker<Task> = bg_worker.lazy_build("region-worker");
        let sched = worker.scheduler();
        let (router, _) = mpsc::sync_channel(11);
        let cfg = make_raftstore_cfg(false);
        let mut runner = RegionRunner::new(
            engine.kv.clone(),
            mgr,
            cfg,
            CoprocessorHost::<KvTestEngine>::default(),
            router,
        );
        runner.clean_stale_check_interval = Duration::from_millis(100);

        let mut ranges = vec![];
        for i in 0..10 {
            let mut key = b"k0".to_vec();
            key.extend_from_slice(i.to_string().as_bytes());
            engine.kv.put(&key, b"v1").unwrap();
            ranges.push(key);
        }
        engine.kv.put(b"k1", b"v1").unwrap();
        let snap = engine.kv.snapshot();
        engine.kv.put(b"k2", b"v2").unwrap();

        sched
            .schedule(Task::Destroy {
                region_id: 1,
                start_key: b"k1".to_vec(),
                end_key: b"k2".to_vec(),
            })
            .unwrap();
        for i in 0..9 {
            sched
                .schedule(Task::Destroy {
                    region_id: i as u64 + 2,
                    start_key: ranges[i].clone(),
                    end_key: ranges[i + 1].clone(),
                })
                .unwrap();
        }
        worker.start_with_timer(runner);
        thread::sleep(Duration::from_millis(20));
        drop(snap);
        thread::sleep(Duration::from_millis(200));
        assert!(engine.kv.get_value(b"k1").unwrap().is_none());
        assert_eq!(engine.kv.get_value(b"k2").unwrap().unwrap(), b"v2");
        for i in 0..9 {
            assert!(engine.kv.get_value(&ranges[i]).unwrap().is_none());
        }
    }

    #[test]
    fn test_pending_applies() {
        let temp_dir = Builder::new()
            .prefix("test_pending_applies")
            .tempdir()
            .unwrap();
        let obs = MockApplySnapshotObserver::default();
        let mut host = CoprocessorHost::<KvTestEngine>::default();
        host.registry
            .register_apply_snapshot_observer(1, BoxApplySnapshotObserver::new(obs.clone()));

        let mut cf_opts = CfOptions::new();
        cf_opts.set_level_zero_slowdown_writes_trigger(5);
        cf_opts.set_disable_auto_compactions(true);
        let kv_cfs_opts = vec![
            (CF_DEFAULT, cf_opts.clone()),
            (CF_WRITE, cf_opts.clone()),
            (CF_LOCK, cf_opts.clone()),
            (CF_RAFT, cf_opts.clone()),
        ];
        let engine = get_test_db_for_regions(
            &temp_dir,
            None,
            None,
            Some(kv_cfs_opts),
            &[1, 2, 3, 4, 5, 6, 7],
        )
        .unwrap();

        for cf_name in &["default", "write", "lock"] {
            for i in 0..7 {
                engine
                    .kv
                    .put_cf(cf_name, &data_key(i.to_string().as_bytes()), &[i])
                    .unwrap();
                engine
                    .kv
                    .put_cf(cf_name, &data_key((i + 1).to_string().as_bytes()), &[i + 1])
                    .unwrap();
                engine.kv.flush_cf(cf_name, true).unwrap();
                // check level 0 files
                assert_eq!(
                    engine
                        .kv
                        .get_cf_num_files_at_level(cf_name, 0)
                        .unwrap()
                        .unwrap(),
                    u64::from(i) + 1
                );
            }
        }

        let snap_dir = Builder::new().prefix("snap_dir").tempdir().unwrap();
        let mgr = SnapManager::new(snap_dir.path().to_str().unwrap());
        mgr.init().unwrap();
        let bg_worker = Worker::new("snap-manager");
        let mut worker = bg_worker.lazy_build("snap-manager");
        let sched = worker.scheduler();
        let (router, receiver) = mpsc::sync_channel(1);
        let cfg = make_raftstore_cfg(true);
        let runner = RegionRunner::new(
            engine.kv.clone(),
            mgr.clone(),
            cfg.clone(),
            host,
            router.clone(),
        );
        worker.start_with_timer(runner);

        let mut snap_gen_worker = LazyWorker::new("snap-generator");
        let snap_gen_sched = snap_gen_worker.scheduler();
        let snap_gen_runner = SnapGenRunner::new(
            engine.kv.clone(),
            mgr,
            router,
            Option::<Arc<RpcClient>>::None,
            snap_gen_worker.pool(),
        );
        snap_gen_worker.start(snap_gen_runner);

        let gen_and_apply_snap = |id: u64| {
            // construct snapshot
            let (tx, rx) = mpsc::sync_channel(1);
            let apply_state: RaftApplyState = engine
                .kv
                .get_msg_cf(CF_RAFT, &keys::apply_state_key(id))
                .unwrap()
                .unwrap();
            let idx = apply_state.get_applied_index();
            let entry = engine.raft.get_entry(id, idx).unwrap().unwrap();
            snap_gen_sched
                .schedule(SnapGenTask::Gen {
                    region_id: id,
                    kv_snap: engine.kv.snapshot(),
                    last_applied_term: entry.get_term(),
                    last_applied_state: apply_state,
                    canceled: Arc::new(AtomicBool::new(false)),
                    notifier: tx,
                    for_balance: false,
                    to_store_id: 0,
                })
                .unwrap();
            let s1 = rx.recv().unwrap();
            match receiver.recv() {
                Ok((region_id, CasualMessage::SnapshotGenerated)) => {
                    assert_eq!(region_id, id);
                }
                msg => panic!("expected SnapshotGenerated, but got {:?}", msg),
            }
            let mut data = RaftSnapshotData::default();
            data.merge_from_bytes(s1.get_data()).unwrap();
            let key = SnapKey::from_snap(&s1).unwrap();
            let mgr = SnapManager::new(snap_dir.path().to_str().unwrap());
            let mut s2 = mgr.get_snapshot_for_sending(&key).unwrap();
            let mut s3 = mgr
                .get_snapshot_for_receiving(&key, data.take_meta())
                .unwrap();
            io::copy(&mut s2, &mut s3).unwrap();
            s3.save().unwrap();

            // set applying state
            let mut wb = engine.kv.write_batch();
            let region_key = keys::region_state_key(id);
            let mut region_state = engine
                .kv
                .get_msg_cf::<RegionLocalState>(CF_RAFT, &region_key)
                .unwrap()
                .unwrap();
            region_state.set_state(PeerState::Applying);
            wb.put_msg_cf(CF_RAFT, &region_key, &region_state).unwrap();
            wb.write().unwrap();

            // apply snapshot
            let status = Arc::new(AtomicUsize::new(JOB_STATUS_PENDING));
            sched
                .schedule(Task::Apply {
                    region_id: id,
                    status,
                    peer_id: 1,
                    create_time: Instant::now(),
                })
                .unwrap();
        };
        let destroy_region = |id: u64| {
            let start_key = data_key(id.to_string().as_bytes());
            let end_key = data_key((id + 1).to_string().as_bytes());
            // destroy region
            sched
                .schedule(Task::Destroy {
                    region_id: id,
                    start_key,
                    end_key,
                })
                .unwrap();
        };

        let check_region_exist = |id: u64| -> bool {
            let key = data_key(id.to_string().as_bytes());
            let v = engine.kv.get_value(&key).unwrap();
            v.is_some()
        };

        let wait_apply_finish = |ids: &[u64]| {
            for id in ids {
                match receiver.recv_timeout(Duration::from_secs(5)) {
                    Ok((region_id, CasualMessage::SnapshotApplied { .. })) => {
                        assert_eq!(region_id, *id);
                    }
                    msg => panic!("expected {} SnapshotApplied, but got {:?}", id, msg),
                }
                let region_key = keys::region_state_key(*id);
                assert_eq!(
                    engine
                        .kv
                        .get_msg_cf::<RegionLocalState>(CF_RAFT, &region_key)
                        .unwrap()
                        .unwrap()
                        .get_state(),
                    PeerState::Normal
                )
            }
        };

        // snapshot will not ingest cause already write stall
        gen_and_apply_snap(1);
        assert_eq!(
            engine
                .kv
                .get_cf_num_files_at_level(CF_DEFAULT, 0)
                .unwrap()
                .unwrap(),
            7
        );

        // compact all files to the bottomest level
        engine.kv.compact_files_in_range(None, None, None).unwrap();
        assert_eq!(
            engine
                .kv
                .get_cf_num_files_at_level(CF_DEFAULT, 0)
                .unwrap()
                .unwrap(),
            0
        );

        wait_apply_finish(&[1]);
        assert_eq!(obs.pre_apply_count.load(Ordering::SeqCst), 1);
        assert_eq!(obs.post_apply_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            obs.pre_apply_hash.load(Ordering::SeqCst),
            obs.post_apply_hash.load(Ordering::SeqCst)
        );
        assert_eq!(obs.cancel_apply.load(Ordering::SeqCst), 0);

        // the pending apply task should be finished and snapshots are ingested.
        // note that when ingest sst, it may flush memtable if overlap,
        // so here will two level 0 files.
        assert_eq!(
            engine
                .kv
                .get_cf_num_files_at_level(CF_DEFAULT, 0)
                .unwrap()
                .unwrap(),
            2
        );

        // no write stall, ingest without delay
        gen_and_apply_snap(2);
        wait_apply_finish(&[2]);
        assert_eq!(
            engine
                .kv
                .get_cf_num_files_at_level(CF_DEFAULT, 0)
                .unwrap()
                .unwrap(),
            4
        );

        // snapshot will not ingest cause it may cause write stall
        gen_and_apply_snap(3);
        assert_eq!(
            engine
                .kv
                .get_cf_num_files_at_level(CF_DEFAULT, 0)
                .unwrap()
                .unwrap(),
            4
        );
        gen_and_apply_snap(4);
        assert_eq!(
            engine
                .kv
                .get_cf_num_files_at_level(CF_DEFAULT, 0)
                .unwrap()
                .unwrap(),
            4
        );
        gen_and_apply_snap(5);
        destroy_region(6);
        thread::sleep(PENDING_APPLY_CHECK_INTERVAL * 2);
        assert!(check_region_exist(6));
        assert_eq!(
            engine
                .kv
                .get_cf_num_files_at_level(CF_DEFAULT, 0)
                .unwrap()
                .unwrap(),
            4
        );

        // compact all files to the bottomest level
        engine.kv.compact_files_in_range(None, None, None).unwrap();
        assert_eq!(
            engine
                .kv
                .get_cf_num_files_at_level(CF_DEFAULT, 0)
                .unwrap()
                .unwrap(),
            0
        );

        // make sure have checked pending applies
        wait_apply_finish(&[3, 4]);

        // before two pending apply tasks should be finished and snapshots are ingested
        // and one still in pending.
        assert_eq!(
            engine
                .kv
                .get_cf_num_files_at_level(CF_DEFAULT, 0)
                .unwrap()
                .unwrap(),
            4
        );

        // make sure have checked pending applies
        engine.kv.compact_files_in_range(None, None, None).unwrap();
        assert_eq!(
            engine
                .kv
                .get_cf_num_files_at_level(CF_DEFAULT, 0)
                .unwrap()
                .unwrap(),
            0
        );
        wait_apply_finish(&[5]);

        // the last one pending task finished
        assert_eq!(
            engine
                .kv
                .get_cf_num_files_at_level(CF_DEFAULT, 0)
                .unwrap()
                .unwrap(),
            2
        );
        thread::sleep(PENDING_APPLY_CHECK_INTERVAL * 2);
        assert!(!check_region_exist(6));

        #[cfg(feature = "failpoints")]
        {
            let must_not_finish = |ids: &[u64]| {
                for id in ids {
                    let region_key = keys::region_state_key(*id);
                    assert_eq!(
                        engine
                            .kv
                            .get_msg_cf::<RegionLocalState>(CF_RAFT, &region_key)
                            .unwrap()
                            .unwrap()
                            .get_state(),
                        PeerState::Applying
                    )
                }
            };

            engine.kv.compact_files_in_range(None, None, None).unwrap();
            fail::cfg("handle_new_pending_applies", "return").unwrap();
            gen_and_apply_snap(7);
            thread::sleep(PENDING_APPLY_CHECK_INTERVAL * 2);
            must_not_finish(&[7]);
            fail::remove("handle_new_pending_applies");
            thread::sleep(PENDING_APPLY_CHECK_INTERVAL * 2);
            wait_apply_finish(&[7]);
        }
        bg_worker.stop();
        // Wait the timer fired. Otherwise deletion of directory may race with timer
        // task.
        thread::sleep(PENDING_APPLY_CHECK_INTERVAL * 2);
    }

    #[derive(Clone, Default)]
    struct MockApplySnapshotObserver {
        pub pre_apply_count: Arc<AtomicUsize>,
        pub post_apply_count: Arc<AtomicUsize>,
        pub pre_apply_hash: Arc<AtomicUsize>,
        pub post_apply_hash: Arc<AtomicUsize>,
        pub cancel_apply: Arc<AtomicUsize>,
    }

    impl Coprocessor for MockApplySnapshotObserver {}

    impl ApplySnapshotObserver for MockApplySnapshotObserver {
        fn pre_apply_snapshot(
            &self,
            _: &mut ObserverContext<'_>,
            peer_id: u64,
            key: &crate::store::SnapKey,
            snapshot: Option<&crate::store::Snapshot>,
        ) {
            let code =
                snapshot.unwrap().total_size() + key.term + key.region_id + key.idx + peer_id;
            self.pre_apply_count.fetch_add(1, Ordering::SeqCst);
            self.pre_apply_hash
                .fetch_add(code as usize, Ordering::SeqCst);
        }

        fn post_apply_snapshot(
            &self,
            _: &mut ObserverContext<'_>,
            peer_id: u64,
            key: &crate::store::SnapKey,
            snapshot: Option<&crate::store::Snapshot>,
        ) {
            let code =
                snapshot.unwrap().total_size() + key.term + key.region_id + key.idx + peer_id;
            self.post_apply_count.fetch_add(1, Ordering::SeqCst);
            self.post_apply_hash
                .fetch_add(code as usize, Ordering::SeqCst);
        }

        fn should_pre_apply_snapshot(&self) -> bool {
            true
        }

        fn cancel_apply_snapshot(&self, _: u64, _: u64) {
            self.cancel_apply.fetch_add(1, Ordering::SeqCst);
        }
    }
}

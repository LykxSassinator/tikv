// Copyright 2016 TiKV Project Authors. Licensed under Apache-2.0.

use lazy_static::lazy_static;
use prometheus::*;
use prometheus_static_metric::*;

make_auto_flush_static_metric! {
    pub label_enum PerfContextType {
        write_wal_time,
        write_delay_time,
        write_scheduling_flushes_compactions_time,
        db_condition_wait_nanos,
        write_memtable_time,
        pre_and_post_process,
        write_thread_wait,
        db_mutex_lock_nanos,
    }

    pub label_enum WriteCmdType {
        put,
        delete,
        delete_range,
        ingest_sst,
    }

    pub label_enum AdminCmdType {
        conf_change,
        add_peer,
        remove_peer,
        add_learner,
        batch_split : "batch-split",
        prepare_merge,
        commit_merge,
        rollback_merge,
        compact,
        transfer_leader,
        prepare_flashback,
        finish_flashback,
        batch_switch_witness : "batch-switch-witness",
    }

    pub label_enum AdminCmdStatus {
        reject_unsafe,
        all,
        success,
    }

    pub label_enum SnapValidationType {
        stale,
        decode,
        epoch,
        cancel,
    }

    pub label_enum RegionHashType {
        verify,
        compute,
    }

    pub label_enum RegionHashResult {
        miss,
        matched,
        all,
        failed,
    }

    pub label_enum CfNames {
        default,
        lock,
        write,
        raft,
        ver_default,
    }

    pub label_enum RaftEntryType {
        hit,
        miss,
        async_fetch,
        sync_fetch,
        fallback_fetch,
        fetch_invalid,
        fetch_unused,
    }

    pub label_enum WarmUpEntryCacheType {
        started,
        timeout,
        finished,
        stale,
    }

    pub label_enum CompactionGuardAction {
        init,
        init_failure,
        partition,
        skip_partition,
    }

    pub struct RaftEntryFetches : LocalIntCounter {
        "type" => RaftEntryType
    }

    pub struct WarmUpEntryCacheCounter : LocalIntCounter {
        "type" => WarmUpEntryCacheType
    }

    pub struct SnapCf : LocalHistogram {
        "type" => CfNames,
    }
    pub struct SnapCfSize : LocalHistogram {
        "type" => CfNames,
    }
    pub struct RegionHashCounter: LocalIntCounter {
        "type" => RegionHashType,
        "result" => RegionHashResult,
    }

    pub struct AdminCmdVec : LocalIntCounter {
        "type" => AdminCmdType,
        "status" => AdminCmdStatus,
    }

    pub struct WriteCmdVec : LocalIntCounter {
        "type" => WriteCmdType,
    }

    pub struct SnapValidVec : LocalIntCounter {
        "type" => SnapValidationType
    }
    pub struct PerfContextTimeDuration : LocalHistogram {
        "type" => PerfContextType
    }

    pub struct CompactionGuardActionVec: LocalIntCounter {
        "cf" => CfNames,
        "type" => CompactionGuardAction,
    }
}

make_static_metric! {
    pub label_enum RaftReadyType {
        message,
        commit,
        append,
        snapshot,
        pending_region,
        has_ready_region,
        propose_delay,
    }

    pub label_enum RaftSentMessageCounterType {
        append,
        append_resp,
        prevote,
        prevote_resp,
        vote,
        vote_resp,
        snapshot,
        heartbeat,
        heartbeat_resp,
        transfer_leader,
        timeout_now,
        read_index,
        read_index_resp,
    }

    pub label_enum SendStatus {
        accept,
        drop,
    }

    pub label_enum RaftDroppedMessage {
        mismatch_store_id,
        mismatch_region_epoch,
        mismatch_witness_snapshot,
        stale_msg,
        region_overlap,
        region_no_peer,
        region_tombstone_peer,
        region_nonexistent,
        applying_snap,
        disk_full,
        non_witness,
        recovery,
        unsafe_vote,
    }

    pub label_enum ProposalType {
        all,
        local_read,
        read_index,
        unsafe_read_index,
        normal,
        transfer_leader,
        conf_change,
        batch,
        dropped_read_index,
    }

    pub label_enum RaftInvalidProposal {
        mismatch_store_id,
        region_not_found,
        not_leader,
        mismatch_peer_id,
        stale_command,
        epoch_not_match,
        read_index_no_leader,
        region_not_initialized,
        is_applying_snapshot,
        force_leader,
        witness,
        flashback_in_progress,
        flashback_not_prepared,
        non_witness,
    }

    pub label_enum RaftEventDurationType {
        compact_check,
        periodic_full_compact,
        load_metrics_window,
        pd_store_heartbeat,
        pd_report_min_resolved_ts,
        snap_gc,
        compact_lock_cf,
        consistency_check,
        cleanup_import_sst,
        raft_engine_purge,
        peer_msg,
        store_msg,
    }

    pub label_enum RaftLogGcSkippedReason {
        reserve_log,
        compact_idx_too_small,
        threshold_limit,
    }

    pub label_enum LoadBaseSplitEventType {
        // Workload fits the QPS threshold or byte threshold.
        load_fit,
        // Workload fits the CPU threshold.
        cpu_load_fit,
        // The statistical key is empty.
        empty_statistical_key,
        // Split info has been collected, ready to split.
        ready_to_split,
        // Split info has not been collected yet, not ready to split.
        not_ready_to_split,
        // The number of sampled keys does not meet the threshold.
        no_enough_sampled_key,
        // The number of sampled keys located on left and right does not meet the threshold.
        no_enough_lr_key,
        // The number of balanced keys does not meet the score.
        no_balance_key,
        // The number of contained keys does not meet the score.
        no_uncross_key,
        // Split info for the top hot CPU region has been collected, ready to split.
        ready_to_split_cpu_top,
        // Hottest key range for the top hot CPU region could not be found.
        empty_hottest_key_range,
        // The top hot CPU region could not be split.
        unable_to_split_cpu_top,
    }

    pub label_enum SnapshotBrWaitApplyEventType {
        sent,
        trivial,
        accepted,
        term_not_match,
        epoch_not_match,
        duplicated,
        finished,
    }

    pub label_enum SnapshotGenerateBytesType {
        kv,
        sst,
        plain,
        io,
    }

    pub struct SnapshotBrWaitApplyEvent : IntCounter {
        "event" => SnapshotBrWaitApplyEventType
    }

    pub label_enum SnapshotBrLeaseEventType {
        create,
        renew,
        expired,
        reset,
    }

    pub struct SnapshotBrLeaseEvent : IntCounter {
        "event" => SnapshotBrLeaseEventType
    }

    pub struct HibernatedPeerStateGauge: IntGauge {
        "state" => {
            awaken,
            hibernated,
        },
    }

    pub struct RaftReadyCounterVec : LocalIntCounter {
        "type" => RaftReadyType,
    }

    pub struct RaftSentMessageCounterVec : LocalIntCounter {
        "type" => RaftSentMessageCounterType,
        "status" => SendStatus,
    }

    pub struct RaftDroppedMessageCounterVec : LocalIntCounter {
        "type" => RaftDroppedMessage,
    }

    pub struct RaftProposalCounterVec: LocalIntCounter {
        "type" => ProposalType,
    }

    pub struct RaftInvalidProposalCounterVec : LocalIntCounter {
        "type" => RaftInvalidProposal
    }

    pub struct RaftEventDurationVec : LocalHistogram {
        "type" => RaftEventDurationType
    }

    pub struct RaftLogGcSkippedCounterVec: LocalIntCounter {
        "reason" => RaftLogGcSkippedReason,
    }

    pub struct LoadBaseSplitEventCounterVec: IntCounter {
        "type" => LoadBaseSplitEventType,
    }

    pub struct StoreBusyOnApplyRegionsGaugeVec: IntGauge {
        "type" => {
            busy_apply_peers,
            completed_apply_peers,
        },
    }

    pub struct StoreBusyStateGaugeVec: IntGauge {
        "type" => {
            raftstore_busy,
            applystore_busy,
        },
    }

    pub struct SnapshotGenerateBytesTypeVec: IntCounter {
        "type" => SnapshotGenerateBytesType,
    }
}

lazy_static! {
    pub static ref STORE_TIME_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_store_duration_secs",
            "Bucketed histogram of store time duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();
    pub static ref APPLY_TIME_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_apply_duration_secs",
            "Bucketed histogram of apply time duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();

    pub static ref STORE_WRITE_TASK_WAIT_DURATION_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_store_write_task_wait_duration_secs",
            "Bucketed histogram of store write task wait time duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();
    pub static ref STORE_WRITE_HANDLE_MSG_DURATION_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_store_write_handle_msg_duration_secs",
            "Bucketed histogram of handle store write msg duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();
    pub static ref STORE_WRITE_TRIGGER_SIZE_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_store_write_trigger_wb_bytes",
            "Bucketed histogram of store write task size of raft writebatch.",
            exponential_buckets(8.0, 2.0, 24).unwrap()
        ).unwrap();
    pub static ref STORE_WRITE_KVDB_DURATION_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_store_write_kvdb_duration_seconds",
            "Bucketed histogram of store write kv db duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();
    pub static ref STORE_WRITE_RAFTDB_DURATION_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_store_write_raftdb_duration_seconds",
            "Bucketed histogram of store write raft db duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();
    pub static ref STORE_WRITE_SEND_DURATION_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_store_write_send_duration_seconds",
            "Bucketed histogram of sending msg duration after writing db.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();
    pub static ref STORE_WRITE_CALLBACK_DURATION_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_store_write_callback_duration_seconds",
            "Bucketed histogram of sending callback to store thread duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();
    pub static ref STORE_WRITE_TO_DB_DURATION_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_append_log_duration_seconds",
            "Bucketed histogram of peer appending log duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();
    pub static ref STORE_WRITE_LOOP_DURATION_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_store_write_loop_duration_seconds",
            "Bucketed histogram of store write loop duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();
    pub static ref STORE_WRITE_MSG_BLOCK_WAIT_DURATION_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_store_write_msg_block_wait_duration_seconds",
            "Bucketed histogram of write msg block wait duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();

    /// Waterfall Metrics
    pub static ref STORE_WF_BATCH_WAIT_DURATION_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_store_wf_batch_wait_duration_seconds",
            "Bucketed histogram of proposals' wait batch duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();
    pub static ref STORE_WF_SEND_TO_QUEUE_DURATION_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_store_wf_send_to_queue_duration_seconds",
            "Bucketed histogram of proposals' send to write queue duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();
    pub static ref STORE_WF_SEND_PROPOSAL_DURATION_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_store_wf_send_proposal_duration_seconds",
            "Bucketed histogram of proposals' waterfall send duration",
            exponential_buckets(1e-6, 2.0, 26).unwrap()
        ).unwrap();
    pub static ref STORE_WF_BEFORE_WRITE_DURATION_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_store_wf_before_write_duration_seconds",
            "Bucketed histogram of proposals' before write duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();
    pub static ref STORE_WF_WRITE_KVDB_END_DURATION_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_store_wf_write_kvdb_end_duration_seconds",
            "Bucketed histogram of proposals' write kv db end duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();
    pub static ref STORE_WF_WRITE_END_DURATION_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_store_wf_write_end_duration_seconds",
            "Bucketed histogram of proposals' write db end duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();
    pub static ref STORE_WF_PERSIST_LOG_DURATION_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_store_wf_persist_duration_seconds",
            "Bucketed histogram of proposals' persist duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();
    pub static ref STORE_WF_COMMIT_LOG_DURATION_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_store_wf_commit_log_duration_seconds",
            "Bucketed histogram of proposals' commit and persist duration.",
            exponential_buckets(0.00001, 2.0, 32).unwrap() // 10us ~ 42949s.
        ).unwrap();
    pub static ref STORE_WF_COMMIT_NOT_PERSIST_LOG_DURATION_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_store_wf_commit_not_persist_log_duration_seconds",
            "Bucketed histogram of proposals' commit but not persist duration",
            exponential_buckets(0.00001, 2.0, 32).unwrap() // 10us ~ 42949s.
        ).unwrap();

    pub static ref STORE_IO_DURATION_HISTOGRAM: HistogramVec =
        register_histogram_vec!(
            "tikv_raftstore_io_duration_seconds",
            "Bucketed histogram of raftstore IO duration",
            &["type", "reason"],
            exponential_buckets(0.00001, 2.0, 26).unwrap() // 10us ~ 671s.
        ).unwrap();

    pub static ref PEER_PROPOSAL_COUNTER_VEC: IntCounterVec =
        register_int_counter_vec!(
            "tikv_raftstore_proposal_total",
            "Total number of proposal made.",
            &["type"]
        ).unwrap();

    pub static ref PEER_ADMIN_CMD_COUNTER_VEC: IntCounterVec =
        register_int_counter_vec!(
            "tikv_raftstore_admin_cmd_total",
            "Total number of admin cmd processed.",
            &["type", "status"]
        ).unwrap();
    pub static ref PEER_ADMIN_CMD_COUNTER: AdminCmdVec =
        auto_flush_from!(PEER_ADMIN_CMD_COUNTER_VEC, AdminCmdVec);

    pub static ref PEER_WRITE_CMD_COUNTER_VEC: IntCounterVec =
        register_int_counter_vec!(
            "tikv_raftstore_write_cmd_total",
            "Total number of write cmd processed.",
            &["type"]
        ).unwrap();
    pub static ref PEER_WRITE_CMD_COUNTER: WriteCmdVec =
        auto_flush_from!(PEER_WRITE_CMD_COUNTER_VEC, WriteCmdVec);

    pub static ref PEER_COMMIT_LOG_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_commit_log_duration_seconds",
            "Bucketed histogram of peer commits logs duration.",
            exponential_buckets(0.00001, 2.0, 32).unwrap() // 10us ~ 42949s.
        ).unwrap();


    pub static ref STORE_APPLY_LOG_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_apply_log_duration_seconds",
            "Bucketed histogram of peer applying log duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();

    pub static ref APPLY_TASK_WAIT_TIME_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_apply_wait_time_duration_secs",
            "Bucketed histogram of apply task wait time duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();

    pub static ref APPLY_MSG_LEN: Histogram =
        register_histogram!(
            "tikv_raftstore_apply_msg_len",
            "Length of apply msg.",
            exponential_buckets(1.0, 2.0, 20).unwrap() // max 1024 * 1024
        ).unwrap();

    pub static ref STORE_RAFT_READY_COUNTER_VEC: IntCounterVec =
        register_int_counter_vec!(
            "tikv_raftstore_raft_ready_handled_total",
            "Total number of raft ready handled.",
            &["type"]
        ).unwrap();

    pub static ref STORE_RAFT_SENT_MESSAGE_COUNTER_VEC: IntCounterVec =
        register_int_counter_vec!(
            "tikv_raftstore_raft_sent_message_total",
            "Total number of raft ready sent messages.",
            &["type", "status"]
        ).unwrap();

    pub static ref STORE_RAFT_DROPPED_MESSAGE_COUNTER_VEC: IntCounterVec =
        register_int_counter_vec!(
            "tikv_raftstore_raft_dropped_message_total",
            "Total number of raft dropped messages.",
            &["type"]
        ).unwrap();

    pub static ref STORE_SNAPSHOT_TRAFFIC_GAUGE_VEC: IntGaugeVec =
        register_int_gauge_vec!(
            "tikv_raftstore_snapshot_traffic_total",
            "Total number of raftstore snapshot traffic.",
            &["type"]
        ).unwrap();

    pub static ref STORE_SNAPSHOT_VALIDATION_FAILURE_COUNTER_VEC: IntCounterVec =
        register_int_counter_vec!(
            "tikv_raftstore_snapshot_validation_failure_total",
            "Total number of raftstore snapshot validation failure.",
            &["type"]
        ).unwrap();
    pub static ref STORE_SNAPSHOT_VALIDATION_FAILURE_COUNTER: SnapValidVec =
        auto_flush_from!(STORE_SNAPSHOT_VALIDATION_FAILURE_COUNTER_VEC, SnapValidVec);

    pub static ref PEER_RAFT_PROCESS_DURATION: HistogramVec =
        register_histogram_vec!(
            "tikv_raftstore_raft_process_duration_secs",
            "Bucketed histogram of peer processing raft duration.",
            &["type"],
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();

    pub static ref PEER_PROPOSE_LOG_SIZE_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_propose_log_size",
            "Bucketed histogram of peer proposing log size.",
            exponential_buckets(8.0, 2.0, 22).unwrap()
        ).unwrap();

    pub static ref STORE_APPLY_KEY_SIZE_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_apply_key_size",
            "Bucketed histogram of apply key size.",
            exponential_buckets(8.0, 2.0, 17).unwrap()
        ).unwrap();
    pub static ref STORE_APPLY_VALUE_SIZE_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_apply_value_size",
            "Bucketed histogram of apply value size.",
            exponential_buckets(8.0, 2.0, 23).unwrap()
        ).unwrap();

    pub static ref REGION_HASH_COUNTER_VEC: IntCounterVec =
        register_int_counter_vec!(
            "tikv_raftstore_hash_total",
            "Total number of hash has been computed.",
            &["type", "result"]
        ).unwrap();
    pub static ref REGION_HASH_COUNTER: RegionHashCounter =
        auto_flush_from!(REGION_HASH_COUNTER_VEC, RegionHashCounter);

    pub static ref REGION_MAX_LOG_LAG: Histogram =
        register_histogram!(
            "tikv_raftstore_log_lag",
            "Bucketed histogram of log lag in a region.",
            vec![2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0,
                    512.0, 1024.0, 5120.0, 10240.0]
        ).unwrap();

    pub static ref REQUEST_WAIT_TIME_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_request_wait_time_duration_secs",
            "Bucketed histogram of request wait time duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();

    pub static ref RAFT_MESSAGE_WAIT_TIME_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_raft_msg_wait_time_duration_secs",
            "Bucketed histogram of raft message wait time duration.",
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();

    pub static ref PEER_GC_RAFT_LOG_COUNTER: IntCounter =
        register_int_counter!(
            "tikv_raftstore_gc_raft_log_total",
            "Total number of GC raft log."
        ).unwrap();

    pub static ref UPDATE_REGION_SIZE_BY_COMPACTION_COUNTER: IntCounter =
        register_int_counter!(
            "update_region_size_count_by_compaction",
            "Total number of update region size caused by compaction."
        ).unwrap();

    pub static ref COMPACTION_RELATED_REGION_COUNT: HistogramVec =
        register_histogram_vec!(
            "compaction_related_region_count",
            "Associated number of regions for each compaction job.",
            &["output_level"],
            exponential_buckets(1.0, 2.0, 20).unwrap()
        ).unwrap();

    pub static ref COMPACTION_DECLINED_BYTES: HistogramVec =
        register_histogram_vec!(
            "compaction_declined_bytes",
            "Total bytes declined for each compaction job.",
            &["output_level"],
            exponential_buckets(1024.0, 2.0, 30).unwrap()
        ).unwrap();

    pub static ref SNAPSHOT_CF_KV_COUNT_VEC: HistogramVec =
        register_histogram_vec!(
            "tikv_snapshot_cf_kv_count",
            "Total number of kv in each cf file of snapshot.",
            &["type"],
            exponential_buckets(100.0, 2.0, 20).unwrap()
        ).unwrap();
    pub static ref SNAPSHOT_CF_KV_COUNT: SnapCf =
        auto_flush_from!(SNAPSHOT_CF_KV_COUNT_VEC, SnapCf);

    pub static ref SNAPSHOT_CF_SIZE_VEC: HistogramVec =
        register_histogram_vec!(
            "tikv_snapshot_cf_size",
            "Total size of each cf file of snapshot.",
            &["type"],
            exponential_buckets(1024.0, 2.0, 31).unwrap()
        ).unwrap();
    pub static ref SNAPSHOT_CF_SIZE: SnapCfSize =
        auto_flush_from!(SNAPSHOT_CF_SIZE_VEC, SnapCfSize);
    pub static ref SNAPSHOT_BUILD_TIME_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_snapshot_build_time_duration_secs",
            "Bucketed histogram of snapshot build time duration.",
            exponential_buckets(0.05, 2.0, 20).unwrap()
        ).unwrap();

    pub static ref SNAPSHOT_KV_COUNT_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_snapshot_kv_count",
            "Total number of kv in snapshot.",
            exponential_buckets(100.0, 2.0, 20).unwrap() //100,100*2^1,...100M
        ).unwrap();

    pub static ref SNAPSHOT_SIZE_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_snapshot_size",
            "Size of snapshot.",
            exponential_buckets(1024.0, 2.0, 22).unwrap() // 1024,1024*2^1,..,4G
        ).unwrap();

    pub static ref RAFT_ENTRY_FETCHES_VEC: IntCounterVec =
        register_int_counter_vec!(
            "tikv_raftstore_entry_fetches",
            "Total number of raft entry fetches.",
            &["type"]
        ).unwrap();
    pub static ref RAFT_ENTRY_FETCHES: RaftEntryFetches =
        auto_flush_from!(RAFT_ENTRY_FETCHES_VEC, RaftEntryFetches);

    // The max task duration can be a few minutes.
    pub static ref RAFT_ENTRY_FETCHES_TASK_DURATION_HISTOGRAM: Histogram =
        register_histogram!(
            "tikv_raftstore_entry_fetches_task_duration_seconds",
            "Bucketed histogram of raft entry fetches task duration.",
            exponential_buckets(0.0005, 2.0, 21).unwrap()  // 500us ~ 8.7m
        ).unwrap();

    pub static ref WARM_UP_ENTRY_CACHE_COUNTER_VEC: IntCounterVec =
        register_int_counter_vec!(
            "tikv_raftstore_prefill_entry_cache_total",
            "Total number of prefill entry cache.",
            &["type"]
        ).unwrap();
    pub static ref WARM_UP_ENTRY_CACHE_COUNTER: WarmUpEntryCacheCounter =
        auto_flush_from!(WARM_UP_ENTRY_CACHE_COUNTER_VEC, WarmUpEntryCacheCounter);

    pub static ref LEADER_MISSING: IntGauge =
        register_int_gauge!(
            "tikv_raftstore_leader_missing",
            "Total number of leader missed region."
        ).unwrap();

    pub static ref CHECK_STALE_PEER_COUNTER: IntCounter = register_int_counter!(
        "tikv_raftstore_check_stale_peer",
        "Total number of checking stale peers."
    ).unwrap();

    pub static ref RAFT_INVALID_PROPOSAL_COUNTER_VEC: IntCounterVec =
        register_int_counter_vec!(
            "tikv_raftstore_raft_invalid_proposal_total",
            "Total number of raft invalid proposal.",
            &["type"]
        ).unwrap();

    pub static ref RAFT_EVENT_DURATION_VEC: HistogramVec =
        register_histogram_vec!(
            "tikv_raftstore_event_duration",
            "Duration of raft store events.",
            &["type"],
            exponential_buckets(0.001, 1.59, 20).unwrap() // max 10s
        ).unwrap();

    pub static ref PEER_MSG_LEN: Histogram =
        register_histogram!(
            "tikv_raftstore_peer_msg_len",
            "Length of peer msg.",
            exponential_buckets(1.0, 2.0, 20).unwrap() // max 1024 * 1024
        ).unwrap();

    pub static ref RAFT_READ_INDEX_PENDING_DURATION: Histogram =
        register_histogram!(
            "tikv_raftstore_read_index_pending_duration",
            "Duration of pending read index.",
            exponential_buckets(0.001, 2.0, 20).unwrap() // max 1000s
        ).unwrap();

    pub static ref RAFT_READ_INDEX_PENDING_COUNT: IntGauge =
        register_int_gauge!(
            "tikv_raftstore_read_index_pending",
            "Pending read index count."
        ).unwrap();

    pub static ref READ_QPS_TOPN: GaugeVec =
        register_gauge_vec!(
            "tikv_read_qps_topn",
            "Collect topN of read qps.",
        &["order"]
        ).unwrap();

    pub static ref LOAD_BASE_SPLIT_EVENT: LoadBaseSplitEventCounterVec =
        register_static_int_counter_vec!(
            LoadBaseSplitEventCounterVec,
            "tikv_load_base_split_event",
            "Load base split event.",
            &["type"]
        ).unwrap();

    pub static ref LOAD_BASE_SPLIT_SAMPLE_VEC: HistogramVec = register_histogram_vec!(
        "tikv_load_base_split_sample",
        "Histogram of query balance",
        &["type"],
        linear_buckets(0.0, 0.05, 20).unwrap()
    ).unwrap();

    pub static ref LOAD_BASE_SPLIT_DURATION_HISTOGRAM : Histogram = register_histogram!(
        "tikv_load_base_split_duration_seconds",
        "Histogram of the time load base split costs in seconds"
    ).unwrap();

    pub static ref QUERY_REGION_VEC: HistogramVec = register_histogram_vec!(
        "tikv_query_region",
        "Histogram of query",
        &["type"],
        exponential_buckets(8.0, 2.0, 24).unwrap()
    ).unwrap();

    pub static ref RAFT_APPLY_AHEAD_PERSIST_HISTOGRAM: Histogram = register_histogram!(
        "tikv_raft_apply_ahead_of_persist",
        "Histogram of the raft log lag between persisted index and applied index",
        exponential_buckets(1.0, 2.0, 20).unwrap()
    ).unwrap();

    pub static ref RAFT_ENABLE_UNPERSISTED_APPLY_GAUGE: IntGauge = register_int_gauge!(
        "tikv_raft_enable_unpersisted_apply_regions",
        "The number of regions that disable apply unpersisted raft log."
    ).unwrap();

    pub static ref RAFT_ENTRIES_CACHES_GAUGE: IntGauge = register_int_gauge!(
        "tikv_raft_entries_caches",
        "Total memory size of raft entries caches."
    ).unwrap();

    pub static ref RAFT_ENTRIES_EVICT_BYTES: IntCounter = register_int_counter!(
        "tikv_raft_entries_evict_bytes",
        "Cache evict bytes."
    ).unwrap();

    pub static ref COMPACTION_GUARD_ACTION_COUNTER_VEC: IntCounterVec =
        register_int_counter_vec!(
            "tikv_raftstore_compaction_guard_action_total",
            "Total number of compaction guard actions.",
            &["cf", "type"]
        ).unwrap();
    pub static ref COMPACTION_GUARD_ACTION_COUNTER: CompactionGuardActionVec =
        auto_flush_from!(COMPACTION_GUARD_ACTION_COUNTER_VEC, CompactionGuardActionVec);

    pub static ref RAFT_PEER_PENDING_DURATION: Histogram =
    register_histogram!(
        "tikv_raftstore_peer_pending_duration_seconds",
        "Bucketed histogram of region peer pending duration.",
        exponential_buckets(0.1, 1.5, 30).unwrap()  // 0.1s ~ 5.3 hours
    ).unwrap();

    pub static ref HIBERNATED_PEER_STATE_GAUGE: HibernatedPeerStateGauge = register_static_int_gauge_vec!(
        HibernatedPeerStateGauge,
        "tikv_raftstore_hibernated_peer_state",
        "Number of peers in hibernated state.",
        &["state"],
    ).unwrap();

    pub static ref STORE_IO_RESCHEDULE_PEER_TOTAL_GAUGE: IntGauge = register_int_gauge!(
        "tikv_raftstore_io_reschedule_region_total",
        "Total number of io rescheduling peers"
    ).unwrap();

    pub static ref STORE_IO_RESCHEDULE_PENDING_TASKS_TOTAL_GAUGE: IntGauge = register_int_gauge!(
        "tikv_raftstore_io_reschedule_pending_tasks_total",
        "Total number of pending write tasks from io rescheduling peers"
    ).unwrap();

    pub static ref STORE_INSPECT_DURATION_HISTOGRAM: HistogramVec =
        register_histogram_vec!(
            "tikv_raftstore_inspect_duration_seconds",
            "Bucketed histogram of inspect duration.",
            &["type"],
            exponential_buckets(0.00001, 2.0, 26).unwrap()
        ).unwrap();

    pub static ref STORE_SLOW_SCORE_GAUGE: IntGaugeVec = register_int_gauge_vec!(
        "tikv_raftstore_slow_score",
        "Slow score of the store.",
        &["type"]
    ).unwrap();

    pub static ref STORE_SLOW_TREND_GAUGE: Gauge =
    register_gauge!("tikv_raftstore_slow_trend", "Slow trend changing rate.").unwrap();

    pub static ref STORE_SLOW_TREND_L0_GAUGE: Gauge =
    register_gauge!("tikv_raftstore_slow_trend_l0", "Slow trend L0 window avg value.").unwrap();
    pub static ref STORE_SLOW_TREND_L1_GAUGE: Gauge =
    register_gauge!("tikv_raftstore_slow_trend_l1", "Slow trend L1 window avg value.").unwrap();
    pub static ref STORE_SLOW_TREND_L2_GAUGE: Gauge =
    register_gauge!("tikv_raftstore_slow_trend_l2", "Slow trend L2 window avg value.").unwrap();

    pub static ref STORE_SLOW_TREND_L0_L1_GAUGE: Gauge =
    register_gauge!("tikv_raftstore_slow_trend_l0_l1", "Slow trend changing rate: L0/L1.").unwrap();
    pub static ref STORE_SLOW_TREND_L1_L2_GAUGE: Gauge =
    register_gauge!("tikv_raftstore_slow_trend_l1_l2", "Slow trend changing rate: L1/L2.").unwrap();

    pub static ref STORE_SLOW_TREND_L1_MARGIN_ERROR_GAUGE: Gauge =
    register_gauge!("tikv_raftstore_slow_trend_l1_margin_error", "Slow trend: L1 margin error range").unwrap();
    pub static ref STORE_SLOW_TREND_L2_MARGIN_ERROR_GAUGE: Gauge =
    register_gauge!("tikv_raftstore_slow_trend_l2_margin_error", "Slow trend: L2 margin error range").unwrap();

    pub static ref STORE_SLOW_TREND_MARGIN_ERROR_WINDOW_GAP_GAUGE_VEC: IntGaugeVec =
    register_int_gauge_vec!(
        "tikv_raftstore_slow_trend_margin_error_gap",
        "Slow trend: the gap between margin window time and current sampling time",
        &["window"]
    ).unwrap();

    pub static ref STORE_SLOW_TREND_MISC_GAUGE_VEC: IntGaugeVec =
    register_int_gauge_vec!(
        "tikv_raftstore_slow_trend_misc",
        "Slow trend uncatelogued gauge(s)",
        &["window"]
    ).unwrap();

    pub static ref STORE_SLOW_TREND_RESULT_VALUE_GAUGE: Gauge =
    register_gauge!("tikv_raftstore_slow_trend_result_value", "Store slow trend result meantime value").unwrap();
    pub static ref STORE_SLOW_TREND_RESULT_GAUGE: Gauge =
    register_gauge!("tikv_raftstore_slow_trend_result", "Store slow trend result changing rate").unwrap();

    pub static ref STORE_SLOW_TREND_RESULT_L0_GAUGE: Gauge =
    register_gauge!("tikv_raftstore_slow_trend_result_l0", "Slow trend result L0 window avg value.").unwrap();
    pub static ref STORE_SLOW_TREND_RESULT_L1_GAUGE: Gauge =
    register_gauge!("tikv_raftstore_slow_trend_result_l1", "Slow trend result L1 window avg value.").unwrap();
    pub static ref STORE_SLOW_TREND_RESULT_L2_GAUGE: Gauge =
    register_gauge!("tikv_raftstore_slow_trend_result_l2", "Slow trend result L2 window avg value.").unwrap();

    pub static ref STORE_SLOW_TREND_RESULT_L0_L1_GAUGE: Gauge =
    register_gauge!("tikv_raftstore_slow_trend_result_l0_l1", "Slow trend result changing rate: L0/L1.").unwrap();
    pub static ref STORE_SLOW_TREND_RESULT_L1_L2_GAUGE: Gauge =
    register_gauge!("tikv_raftstore_slow_trend_result_l1_l2", "Slow trend result changing rate: L1/L2.").unwrap();

    pub static ref STORE_SLOW_TREND_RESULT_L1_MARGIN_ERROR_GAUGE: Gauge =
    register_gauge!("tikv_raftstore_slow_trend_result_l1_margin_error", "Slow trend result: L1 margin error range").unwrap();
    pub static ref STORE_SLOW_TREND_RESULT_L2_MARGIN_ERROR_GAUGE: Gauge =
    register_gauge!("tikv_raftstore_slow_trend_result_l2_margin_error", "Slow trend result: L2 margin error range").unwrap();

    pub static ref STORE_SLOW_TREND_RESULT_MARGIN_ERROR_WINDOW_GAP_GAUGE_VEC: IntGaugeVec =
    register_int_gauge_vec!(
        "tikv_raftstore_slow_trend_result_margin_error_gap",
        "Slow trend result: the gap between margin window time and current sampling time",
        &["window"]
    ).unwrap();

    pub static ref STORE_SLOW_TREND_RESULT_MISC_GAUGE_VEC: IntGaugeVec =
    register_int_gauge_vec!(
        "tikv_raftstore_slow_trend_result_misc",
        "Slow trend result uncatelogued gauge(s)",
        &["type"]
    ).unwrap();

    pub static ref RAFT_LOG_GC_SKIPPED_VEC: IntCounterVec = register_int_counter_vec!(
        "tikv_raftstore_raft_log_gc_skipped",
        "Total number of skipped raft log gc.",
        &["reason"]
    )
    .unwrap();

    pub static ref RAFT_APPLYING_SST_GAUGE: IntGaugeVec = register_int_gauge_vec!(
        "tikv_raft_applying_sst",
        "Sum of applying sst.",
        &["type"]
    ).unwrap();

    pub static ref SNAPSHOT_LIMIT_GENERATE_BYTES_VEC: SnapshotGenerateBytesTypeVec = register_static_int_counter_vec!(
        SnapshotGenerateBytesTypeVec,
        "tikv_snapshot_limit_generate_bytes",
        "Total snapshot generate limit used",
        &["type"],
    )
    .unwrap();

    pub static ref MESSAGE_RECV_BY_STORE: IntCounterVec = register_int_counter_vec!(
        "tikv_raftstore_message_recv_by_store",
        "Messages received by store",
        &["store"]
    )
    .unwrap();

    pub static ref PEER_IN_FLASHBACK_STATE: IntGauge = register_int_gauge!(
        "tikv_raftstore_peer_in_flashback_state",
        "Total number of peers in the flashback state"
    ).unwrap();

    pub static ref SNAP_BR_SUSPEND_COMMAND_TYPE: IntCounterVec = register_int_counter_vec!(
        "tikv_raftstore_snap_br_suspend_command_type",
        "The statistic of rejecting some admin commands being proposed.",
        &["type"]
    ).unwrap();

    pub static ref SNAP_BR_WAIT_APPLY_EVENT: SnapshotBrWaitApplyEvent = register_static_int_counter_vec!(
        SnapshotBrWaitApplyEvent,
        "tikv_raftstore_snap_br_wait_apply_event",
        "The events of wait apply issued by snapshot br.",
        &["event"]
    ).unwrap();

    pub static ref SNAP_BR_SUSPEND_COMMAND_LEASE_UNTIL: IntGauge = register_int_gauge!(
        "tikv_raftstore_snap_br_suspend_command_lease_until",
        "The lease that snapshot br holds of rejecting some type of commands. (In unix timestamp.)"
    ).unwrap();

    pub static ref SNAP_BR_LEASE_EVENT: SnapshotBrLeaseEvent = register_static_int_counter_vec!(
        SnapshotBrLeaseEvent,
        "tikv_raftstore_snap_br_lease_event",
        "The events of the lease to denying new admin commands being proposed by snapshot br.",
        &["event"]
    ).unwrap();

    pub static ref STORE_BUSY_ON_APPLY_REGIONS_GAUGE_VEC: StoreBusyOnApplyRegionsGaugeVec =
        register_static_int_gauge_vec!(
            StoreBusyOnApplyRegionsGaugeVec,
            "tikv_raftstore_busy_on_apply_region_total",
            "Total number of regions busy on apply or complete apply.",
            &["type"]
        ).unwrap();

    pub static ref STORE_PROCESS_BUSY_GAUGE_VEC: StoreBusyStateGaugeVec =
        register_static_int_gauge_vec!(
            StoreBusyStateGaugeVec,
            "tikv_raftstore_process_busy",
            "Is raft process busy or not",
            &["type"]
        ).unwrap();
}

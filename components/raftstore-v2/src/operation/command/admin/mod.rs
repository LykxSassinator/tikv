// Copyright 2022 TiKV Project Authors. Licensed under Apache-2.0.

mod compact_log;
mod conf_change;
mod merge;
mod split;
mod transfer_leader;

pub use compact_log::CompactLogContext;
use compact_log::CompactLogResult;
use conf_change::{ConfChangeResult, UpdateGcPeersResult};
use engine_traits::{KvEngine, RaftEngine};
use kvproto::{
    metapb::PeerRole,
    raft_cmdpb::{AdminCmdType, RaftCmdRequest},
    raft_serverpb::{ExtraMessageType, FlushMemtable, RaftMessage},
};
use merge::{commit::CommitMergeResult, prepare::PrepareMergeResult};
pub use merge::{
    commit::{CatchUpLogs, MERGE_IN_PROGRESS_PREFIX},
    MergeContext, MERGE_SOURCE_PREFIX,
};
use protobuf::Message;
use raftstore::{
    store::{
        cmd_resp,
        fsm::{apply, apply::validate_batch_split},
        msg::ErrorCallback,
        Transport,
    },
    Error,
};
use slog::{error, info};
use split::SplitResult;
pub use split::{
    report_split_init_finish, temp_split_path, RequestHalfSplit, RequestSplit, SplitFlowControl,
    SplitInit, SPLIT_PREFIX,
};
use tikv_util::{box_err, log::SlogFormat};
use txn_types::WriteBatchFlags;

use crate::{
    batch::StoreContext,
    raft::Peer,
    router::{CmdResChannel, PeerMsg, RaftRequest},
};

#[derive(Debug)]
pub enum AdminCmdResult {
    // No side effect produced by the command
    None,
    SplitRegion(SplitResult),
    ConfChange(ConfChangeResult),
    TransferLeader(u64),
    CompactLog(CompactLogResult),
    UpdateGcPeers(UpdateGcPeersResult),
    PrepareMerge(PrepareMergeResult),
    CommitMerge(CommitMergeResult),
}

impl<EK: KvEngine, ER: RaftEngine> Peer<EK, ER> {
    #[inline]
    pub fn on_admin_command<T: Transport>(
        &mut self,
        ctx: &mut StoreContext<EK, ER, T>,
        mut req: RaftCmdRequest,
        ch: CmdResChannel,
    ) {
        if !self.serving() {
            apply::notify_req_region_removed(self.region_id(), ch);
            return;
        }
        if !req.has_admin_request() {
            let e = box_err!(
                "{} expect only execute admin command",
                SlogFormat(&self.logger)
            );
            let resp = cmd_resp::new_error(e);
            ch.report_error(resp);
            return;
        }
        if let Err(e) = ctx.coprocessor_host.pre_propose(self.region(), &mut req) {
            let resp = cmd_resp::new_error(e.into());
            ch.report_error(resp);
            return;
        }
        let cmd_type = req.get_admin_request().get_cmd_type();
        if let Err(e) =
            self.validate_command(req.get_header(), Some(cmd_type), &mut ctx.raft_metrics)
        {
            let resp = cmd_resp::new_error(e);
            ch.report_error(resp);
            return;
        }

        let pre_transfer_leader = cmd_type == AdminCmdType::TransferLeader
            && !WriteBatchFlags::from_bits_truncate(req.get_header().get_flags())
                .contains(WriteBatchFlags::TRANSFER_LEADER_PROPOSAL);

        // The admin request is rejected because it may need to update epoch checker
        // which introduces an uncertainty and may breaks the correctness of epoch
        // checker.
        // As pre transfer leader is just a warmup phase, applying to the current term
        // is not required.
        if !self.applied_to_current_term() && !pre_transfer_leader {
            let e = box_err!(
                "{} peer has not applied to current term, applied_term {}, current_term {}",
                SlogFormat(&self.logger),
                self.storage().entry_storage().applied_term(),
                self.term()
            );
            let resp = cmd_resp::new_error(e);
            ch.report_error(resp);
            return;
        }
        if let Some(conflict) = self.proposal_control_mut().check_conflict(Some(cmd_type)) {
            conflict.delay_channel(ch);
            return;
        }
        if self.proposal_control().has_pending_prepare_merge()
            && cmd_type != AdminCmdType::PrepareMerge
            || self.proposal_control().is_merging() && cmd_type != AdminCmdType::RollbackMerge
        {
            let resp = cmd_resp::new_error(Error::ProposalInMergingMode(self.region_id()));
            ch.report_error(resp);
            return;
        }
        // To maintain propose order, we need to make pending proposal first.
        self.propose_pending_writes(ctx);
        let res = if apply::is_conf_change_cmd(&req) {
            self.propose_conf_change(ctx, req)
        } else {
            // propose other admin command.
            match cmd_type {
                AdminCmdType::Split => Err(box_err!(
                    "Split is deprecated. Please use BatchSplit instead."
                )),
                AdminCmdType::BatchSplit => {
                    #[allow(clippy::question_mark)]
                    if let Err(err) = validate_batch_split(req.get_admin_request(), self.region()) {
                        Err(err)
                    } else {
                        // To reduce the impact of the expensive operation of `checkpoint` (it will
                        // flush memtables of the rocksdb) in applying batch split, we split the
                        // BatchSplit cmd into two phases:
                        //
                        // 1. Schedule flush memtable task so that the memtables of the rocksdb can
                        // be flushed in advance in a way that will not block the normal raft
                        // operations (`checkpoint` will still cause flush but it will be
                        // significantly lightweight). At the same time, send flush memtable msgs to
                        // the follower so that they can flush memtalbes in advance too.
                        //
                        // 2. When the task finishes, it will propose a batch split with
                        // `PRE_FLUSH_FINISHED` flag.
                        if !WriteBatchFlags::from_bits_truncate(req.get_header().get_flags())
                            .contains(WriteBatchFlags::PRE_FLUSH_FINISHED)
                        {
                            if self.tablet_being_flushed() {
                                return;
                            }

                            let region_id = self.region().get_id();
                            self.set_tablet_being_flushed(true);
                            info!(
                                self.logger,
                                "Schedule flush tablet";
                            );

                            let mailbox = match ctx.router.mailbox(region_id) {
                                Some(mailbox) => mailbox,
                                None => {
                                    // None means the node is shutdown concurrently and thus the
                                    // mailboxes in router have been cleared
                                    assert!(
                                        ctx.router.is_shutdown(),
                                        "{} router should have been closed",
                                        SlogFormat(&self.logger)
                                    );
                                    return;
                                }
                            };

                            let logger = self.logger.clone();
                            let on_flush_finish = move || {
                                req.mut_header()
                                    .set_flags(WriteBatchFlags::PRE_FLUSH_FINISHED.bits());
                                if let Err(e) = mailbox
                                    .try_send(PeerMsg::AdminCommand(RaftRequest::new(req, ch)))
                                {
                                    error!(
                                        logger,
                                        "send split request fail after pre-flush finished";
                                        "err" => ?e,
                                    );
                                }
                            };

                            if let Err(e) =
                                ctx.schedulers.tablet.schedule(crate::TabletTask::Flush {
                                    region_id,
                                    cb: Some(Box::new(on_flush_finish)),
                                })
                            {
                                error!(
                                    self.logger,
                                    "Fail to schedule flush task";
                                    "err" => ?e,
                                )
                            }

                            // Notify followers to flush their relevant memtables
                            let peers = self.region().get_peers().to_vec();
                            for p in peers {
                                if p == *self.peer()
                                    || p.get_role() != PeerRole::Voter
                                    || p.is_witness
                                {
                                    continue;
                                }
                                let mut msg = RaftMessage::default();
                                msg.set_region_id(region_id);
                                msg.set_from_peer(self.peer().clone());
                                msg.set_to_peer(p.clone());
                                msg.set_region_epoch(self.region().get_region_epoch().clone());
                                let extra_msg = msg.mut_extra_msg();
                                extra_msg.set_type(ExtraMessageType::MsgFlushMemtable);
                                let mut flush_memtable = FlushMemtable::new();
                                flush_memtable.set_region_id(region_id);
                                extra_msg.set_flush_memtable(flush_memtable);

                                self.send_raft_message(ctx, msg);
                            }

                            return;
                        }

                        info!(
                            self.logger,
                            "Propose split";
                        );
                        self.set_tablet_being_flushed(false);
                        self.propose_split(ctx, req)
                    }
                }
                AdminCmdType::TransferLeader => {
                    // Containing TRANSFER_LEADER_PROPOSAL flag means the this transfer leader
                    // request should be proposed to the raft group
                    if WriteBatchFlags::from_bits_truncate(req.get_header().get_flags())
                        .contains(WriteBatchFlags::TRANSFER_LEADER_PROPOSAL)
                    {
                        let data = req.write_to_bytes().unwrap();
                        self.propose(ctx, data)
                    } else {
                        if self.propose_transfer_leader(ctx, req, ch) {
                            self.set_has_ready();
                        }
                        return;
                    }
                }
                AdminCmdType::CompactLog => self.propose_compact_log(ctx, req),
                AdminCmdType::UpdateGcPeer => {
                    let data = req.write_to_bytes().unwrap();
                    self.propose(ctx, data)
                }
                AdminCmdType::PrepareMerge => self.propose_prepare_merge(ctx, req),
                AdminCmdType::CommitMerge => self.propose_commit_merge(ctx, req),
                _ => unimplemented!(),
            }
        };
        match &res {
            Ok(index) => {
                self.proposal_control_mut()
                    .record_proposed_admin(cmd_type, *index);
                if self.proposal_control_mut().has_uncommitted_admin() {
                    self.raft_group_mut().skip_bcast_commit(false);
                }
            }
            Err(e) => {
                info!(
                    self.logger,
                    "failed to propose admin command";
                    "cmd_type" => ?cmd_type,
                    "error" => ?e,
                );
            }
        }
        self.post_propose_command(ctx, res, vec![ch], true);
    }
}
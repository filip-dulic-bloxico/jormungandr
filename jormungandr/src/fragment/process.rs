use crate::{
    fragment::{Logs, Pools},
    intercom::{NetworkMsg, TransactionMsg},
    stats_counter::StatsCounter,
    utils::{
        async_msg::{MessageBox, MessageQueue},
        task::TokioServiceInfo,
    },
};

use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use chrono::{Duration, DurationRound, Utc};
use futures::future;
use thiserror::Error;
use tokio_stream::StreamExt;
use tracing::{span, Level};
use tracing_futures::Instrument;

pub struct Process {
    pool_max_entries: usize,
    logs_max_entries: usize,
    network_msg_box: MessageBox<NetworkMsg>,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("transaction pool error")]
    Pool(#[from] crate::fragment::pool::Error),
    #[error("failed to open persistent log file")]
    PersistentLog(#[source] io::Error),
}

impl Process {
    pub fn new(
        pool_max_entries: usize,
        logs_max_entries: usize,
        network_msg_box: MessageBox<NetworkMsg>,
    ) -> Self {
        Process {
            pool_max_entries,
            logs_max_entries,
            network_msg_box,
        }
    }

    pub async fn start<P: AsRef<Path>>(
        self,
        n_pools: usize,
        service_info: TokioServiceInfo,
        stats_counter: StatsCounter,
        mut input: MessageQueue<TransactionMsg>,
        persistent_log_dir: Option<P>,
    ) -> Result<(), Error> {
        async fn hourly_wakeup(enabled: bool) {
            if enabled {
                let now = Utc::now();
                let current_hour = now.duration_trunc(Duration::hours(1)).unwrap();
                let next_hour = current_hour + Duration::hours(1);
                let sleep_duration = (next_hour - now).to_std().unwrap();
                tokio::time::sleep(sleep_duration).await
            } else {
                future::pending().await
            }
        }

        fn open_log_file(dir: &Path) -> Result<File, Error> {
            let mut path: PathBuf = dir.into();
            if !path.exists() {
                std::fs::create_dir_all(dir).map_err(Error::PersistentLog)?;
            }
            let log_file_name = Utc::now().format("%Y-%m-%d_%H.log").to_string();
            path.push(log_file_name);
            tracing::debug!("creating fragment log file `{:?}`", path);
            fs::OpenOptions::new()
                .append(true)
                .create(true)
                .read(false)
                .open(path)
                .map_err(Error::PersistentLog)
        }

        let min_logs_size = n_pools * self.pool_max_entries;
        if self.logs_max_entries < min_logs_size {
            tracing::warn!(
                "Having 'log_max_entries' < 'pool_max_entries' * n_pools is not recommendend. Overriding 'log_max_entries' to {}", min_logs_size
            );
        }
        let logs = Logs::new(std::cmp::max(self.logs_max_entries, min_logs_size));

        let mut wakeup = Box::pin(hourly_wakeup(persistent_log_dir.is_some()));

        async move {
            let persistent_log = match &persistent_log_dir {
                None => None,
                Some(dir) => {
                    let file = open_log_file(dir.as_ref())?;
                    Some(file)
                }
            };

            let mut pool = Pools::new(
                self.pool_max_entries,
                n_pools,
                logs,
                self.network_msg_box,
                persistent_log,
            );

            loop {
                tokio::select! {
                    maybe_msg = input.next() => {
                        match maybe_msg {
                            None => break,
                            Some(msg) => match msg {
                                TransactionMsg::SendTransactions { origin, fragments, fail_fast, reply_handle } => {
                                    // Note that we cannot use apply_block here, since we don't have a valid context to which to apply
                                    // those blocks. one valid tx in a given context, could be invalid in another. for example
                                    // fee calculations, existence utxo / account solvency.

                                    // FIXME/TODO check that the txs are valid within themselves with basic requirements (e.g. inputs >= outputs).
                                    // we also want to keep a basic capability to filter away repetitive queries or definitely discarded txid.

                                    // This interface only makes sense for messages coming from arbitrary users (like transaction, certificates),
                                    // for other message we don't want to receive them through this interface, and possibly
                                    // put them in another pool.

                                    let stats_counter = stats_counter.clone();

                                    let summary = pool
                            .insert_and_propagate_all(origin, fragments, fail_fast)
                            .await?;

                        stats_counter.add_tx_recv_cnt(summary.accepted.len());

                        reply_handle.reply_ok(summary);
                                }
                                TransactionMsg::RemoveTransactions(fragment_ids, status) => {
                                    tracing::debug!(
                                        "removing fragments added to block {:?}: {:?}",
                                        status,
                                        fragment_ids
                                    );
                                    pool.remove_added_to_block(fragment_ids, status);
                                }
                                TransactionMsg::GetLogs(reply_handle) => {
                                    let logs = pool.logs().logs().cloned().collect();
                                    reply_handle.reply_ok(logs);
                                }
                                TransactionMsg::GetStatuses(fragment_ids, reply_handle) => {
                                    let mut statuses = HashMap::new();
                                    pool.logs().logs_by_ids(fragment_ids).into_iter().for_each(
                                        |(fragment_id, log)| {
                                            statuses.insert(fragment_id, log.status().clone());
                                        },
                                    );
                                    reply_handle.reply_ok(statuses);
                                }
                                TransactionMsg::SelectTransactions {
                                    pool_idx,
                                    ledger,
                                    ledger_params,
                                    selection_alg,
                                    reply_handle,
                                    soft_deadline_future,
                                    hard_deadline_future,
                                } => {
                                    let contents = pool
                                        .select(
                                            pool_idx,
                                            ledger,
                                            ledger_params,
                                            selection_alg,
                                            soft_deadline_future,
                                            hard_deadline_future,
                                        )
                                        .await;
                                    reply_handle.reply_ok(contents);
                                }
                            }
                        }
                    }
                    _ = &mut wakeup => {
                        pool.close_persistent_log();
                        let dir = persistent_log_dir.as_ref().unwrap();
                        let file = open_log_file(dir.as_ref())?;
                        pool.set_persistent_log(file);
                        wakeup = Box::pin(hourly_wakeup(true));
                    }
                }
            }
            Ok(())
        }
        .instrument(span!(parent: service_info.span(), Level::TRACE, "process", kind = "fragment"))
        .await
    }
}

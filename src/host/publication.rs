use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::runtime::{HostError, HostInner};
use crate::protocol::{TaskId, UnixMillis};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PublicationStatus {
    pub initial_pass_complete: bool,
    pub stopped: bool,
    pub completed_passes: u64,
    pub published_events: u64,
    pub failures: u64,
    pub last_error: Option<String>,
    pub last_completed_at: Option<UnixMillis>,
}

impl HostInner {
    pub(super) async fn publish_metadata(
        self: Arc<Self>,
        cancellation: CancellationToken,
        status: watch::Sender<PublicationStatus>,
        initialized: oneshot::Sender<()>,
    ) -> Result<(), HostError> {
        let mut initialized = Some(initialized);
        let mut scheduler = self.scheduler.subscribe();
        let mut receipts = self.receipts.subscribe();
        let mut final_pass = false;
        'passes: loop {
            if cancellation.is_cancelled() && !final_pass {
                while !self
                    .workers_drained
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    receipts.changed().await.map_err(|_| HostError::Worker)?;
                }
                final_pass = true;
            }
            let started = tokio::time::Instant::now();
            let mut task_cursor: Option<TaskId> = None;
            loop {
                if cancellation.is_cancelled() && !final_pass {
                    continue 'passes;
                }
                let tasks = match self.agent.task_publication_ids(task_cursor, 64).await {
                    Ok(tasks) => tasks,
                    Err(error) => {
                        publication_failure(&status, "task source", &error);
                        break;
                    }
                };
                let complete = tasks.len() < 64;
                for task_id in tasks {
                    if cancellation.is_cancelled() && !final_pass {
                        continue 'passes;
                    }
                    task_cursor = Some(task_id);
                    match self.agent.publish_task_state(task_id).await {
                        Ok(receipt) => {
                            if receipt.appended_events > 0 {
                                self.metadata_changed
                                    .send_modify(|value| *value = value.wrapping_add(1));
                                status.send_modify(|status| {
                                    status.published_events = status
                                        .published_events
                                        .saturating_add(receipt.appended_events.into())
                                });
                            }
                        }
                        Err(error) => publication_failure(&status, "task state", &error),
                    }
                }
                if complete {
                    break;
                }
                tokio::task::yield_now().await;
            }
            let mut permission_cursor = None;
            loop {
                if cancellation.is_cancelled() && !final_pass {
                    continue 'passes;
                }
                let after = permission_cursor.clone();
                let decisions = match self
                    .blocking(move |host| {
                        Ok(host
                            .authorizer
                            .decision_publication_ids(after.as_deref(), 64)?)
                    })
                    .await
                {
                    Ok(decisions) => decisions,
                    Err(error) => {
                        publication_failure(&status, "permission source", &error);
                        break;
                    }
                };
                let complete = decisions.len() < 64;
                for decision in decisions {
                    if cancellation.is_cancelled() && !final_pass {
                        continue 'passes;
                    }
                    permission_cursor = Some(decision.clone());
                    let proof = self
                        .blocking(move |host| {
                            Ok(host.authorizer.decision_publication(&decision)?)
                        })
                        .await;
                    let result = match proof {
                        Ok(proof) => self
                            .agent
                            .publish_permission_decision(proof)
                            .await
                            .map_err(HostError::from),
                        Err(error) => Err(error),
                    };
                    match result {
                        Ok(receipt) => {
                            if !receipt.duplicate {
                                self.metadata_changed
                                    .send_modify(|value| *value = value.wrapping_add(1));
                                status.send_modify(|status| {
                                    status.published_events =
                                        status.published_events.saturating_add(1)
                                });
                            }
                        }
                        Err(error) => publication_failure(&status, "permission decision", &error),
                    }
                }
                if complete {
                    break;
                }
                tokio::task::yield_now().await;
            }
            let now = UnixMillis::now()?;
            status.send_modify(|status| {
                status.initial_pass_complete = true;
                status.completed_passes = status.completed_passes.saturating_add(1);
                status.last_completed_at = Some(now);
            });
            if let Some(initialized) = initialized.take() {
                let _receiver_closed = initialized.send(());
            }
            if final_pass {
                status.send_modify(|status| status.stopped = true);
                return Ok(());
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => continue 'passes,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {},
                changed = scheduler.changed() => changed.map_err(|_| HostError::Worker)?,
                changed = receipts.changed() => changed.map_err(|_| HostError::Worker)?,
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => continue 'passes,
                _ = tokio::time::sleep_until(started + Duration::from_millis(250)) => {},
            }
            scheduler.borrow_and_update();
            receipts.borrow_and_update();
        }
    }
}

fn publication_failure(
    status: &watch::Sender<PublicationStatus>,
    source: &str,
    error: &impl std::fmt::Display,
) {
    let diagnostic: String = format!("{source}: {error}").chars().take(256).collect();
    status.send_modify(|status| {
        status.failures = status.failures.saturating_add(1);
        status.last_error = Some(diagnostic);
    });
}

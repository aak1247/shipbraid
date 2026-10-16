use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::domain::{
    Command, CommandDeliveryState, CommandKind, DeliveryRun, DeploymentReceipt, ExecutorResult,
    Phase, ReceiptResult, RunState, State,
};

/// M1 executor boundary. Implementations must report unknown when they cannot
/// establish ownership; HTTP acceptance or a timeout is not stop proof.
pub trait Executor: Send + Sync {
    fn dispatch(&self, command: &Command, run: &DeliveryRun) -> ExecutorResult;
    fn observe(&self, state: &State, run: &DeliveryRun) -> ExecutorResult;
    fn cancel(&self, command: &Command, run: &DeliveryRun) -> ExecutorResult;
    fn verify(&self, run: &DeliveryRun, observed_at: DateTime<Utc>) -> ExecutorResult;
}

/// A visibly synthetic executor for local M1 development and protocol tests.
/// Its durable identity is the acknowledged command ledger: after a restart,
/// only an acknowledged dispatch proves a running synthetic execution, and an
/// acknowledged cancel proves it stopped. A delivered but unacknowledged
/// command remains unknown and is never replayed.
#[derive(Clone, Debug)]
pub struct ScriptedExecutor {
    phase_duration: Duration,
}

impl ScriptedExecutor {
    pub fn new(phase_duration: Duration) -> Self {
        Self { phase_duration }
    }

    pub fn phase_duration(&self) -> Duration {
        self.phase_duration
    }

    pub fn next_phase(&self, run: &DeliveryRun, now: DateTime<Utc>) -> Option<Phase> {
        if run.state != RunState::Running {
            return None;
        }
        let elapsed = now.signed_duration_since(run.phase_started_at);
        let threshold = ChronoDuration::milliseconds(
            i64::try_from(self.phase_duration.as_millis()).unwrap_or(i64::MAX),
        );
        if elapsed < threshold {
            return None;
        }
        match &run.phase {
            Phase::Preparing => Some(Phase::Building),
            Phase::Building => Some(Phase::Deploying),
            Phase::Deploying => Some(Phase::Verifying),
            Phase::Verifying => None,
        }
    }

    pub fn synthetic_physical_start_count(state: &State) -> usize {
        state
            .commands
            .values()
            .filter(|command| {
                command.kind == CommandKind::Dispatch
                    && command.delivery_state == CommandDeliveryState::Acknowledged
            })
            .count()
    }

    fn evidence(run: &DeliveryRun, label: &str) -> String {
        let payload = format!(
            "scripted-v1:{label}:{}:{}:{}:{}",
            run.id,
            run.generation,
            run.target.revision,
            run.executor_ref.as_deref().unwrap_or("")
        );
        hex::encode(Sha256::digest(payload.as_bytes()))
    }
}

impl Default for ScriptedExecutor {
    fn default() -> Self {
        Self::new(Duration::from_secs(3))
    }
}

impl Executor for ScriptedExecutor {
    fn dispatch(&self, command: &Command, run: &DeliveryRun) -> ExecutorResult {
        if command.kind != CommandKind::Dispatch
            || command.run_id != run.id
            || command.generation != run.generation
        {
            return ExecutorResult::Unknown {
                reason: "synthetic command does not match the frozen run".into(),
            };
        }
        ExecutorResult::Started {
            executor_ref: format!("scripted:{}:{}", run.id, command.id),
        }
    }

    fn observe(&self, state: &State, run: &DeliveryRun) -> ExecutorResult {
        let cancel = state
            .commands
            .values()
            .filter(|command| {
                command.run_id == run.id
                    && command.generation == run.generation
                    && command.kind == CommandKind::Cancel
            })
            .max_by_key(|command| command.created_at);
        if let Some(command) = cancel {
            match command.delivery_state {
                CommandDeliveryState::Acknowledged => {
                    return ExecutorResult::Stopped {
                        executor_ref: run.executor_ref.clone().unwrap_or_default(),
                    };
                }
                CommandDeliveryState::Delivered | CommandDeliveryState::Unknown => {
                    return ExecutorResult::Unknown {
                        reason: "scripted cancel delivery has no durable stop acknowledgment"
                            .into(),
                    };
                }
                CommandDeliveryState::Pending => {}
            }
        }

        let dispatch = state
            .commands
            .values()
            .filter(|command| {
                command.run_id == run.id
                    && command.generation == run.generation
                    && command.kind == CommandKind::Dispatch
            })
            .max_by_key(|command| command.created_at);
        match dispatch.map(|command| &command.delivery_state) {
            Some(CommandDeliveryState::Acknowledged) if run.executor_ref.is_some() => {
                ExecutorResult::Started {
                    executor_ref: run.executor_ref.clone().unwrap_or_default(),
                }
            }
            Some(CommandDeliveryState::Pending) => ExecutorResult::Unknown {
                reason: "dispatch intent is durable but has not been delivered".into(),
            },
            Some(CommandDeliveryState::Delivered | CommandDeliveryState::Unknown) => {
                ExecutorResult::Unknown {
                    reason: "scripted dispatch has no durable acknowledgment".into(),
                }
            }
            Some(CommandDeliveryState::Acknowledged) => ExecutorResult::Unknown {
                reason: "scripted execution identity is missing".into(),
            },
            None => match run.state {
                RunState::Canceled
                | RunState::Superseded
                | RunState::Succeeded
                | RunState::Failed => ExecutorResult::Stopped {
                    executor_ref: run.executor_ref.clone().unwrap_or_default(),
                },
                _ => ExecutorResult::Unknown {
                    reason: "no durable scripted dispatch identity is recorded".into(),
                },
            },
        }
    }

    fn cancel(&self, command: &Command, run: &DeliveryRun) -> ExecutorResult {
        if command.kind != CommandKind::Cancel
            || command.run_id != run.id
            || command.generation != run.generation
            || !run.phase.is_preemptible()
        {
            return ExecutorResult::Unknown {
                reason: "synthetic run is not safely stoppable in its recorded phase".into(),
            };
        }
        match run.executor_ref.clone() {
            Some(executor_ref) => ExecutorResult::Stopped { executor_ref },
            None => ExecutorResult::Unknown {
                reason: "synthetic run has no executor identity".into(),
            },
        }
    }

    fn verify(&self, run: &DeliveryRun, observed_at: DateTime<Utc>) -> ExecutorResult {
        if !matches!(run.state, RunState::Running | RunState::Unknown)
            || run.phase != Phase::Verifying
        {
            return ExecutorResult::Unknown {
                reason: "synthetic verification requested outside verifying phase".into(),
            };
        }
        let mut component_digests = BTreeMap::new();
        for component in &run.target.components {
            let synthetic = format!(
                "scripted-v1:{}:{}:{}:{}",
                run.id, run.generation, run.target.revision, component
            );
            component_digests.insert(component.clone(), hex::encode(Sha256::digest(synthetic)));
        }
        let receipt_material = format!("scripted-v1:receipt:{}:{}", run.id, run.generation);
        let receipt_digest = Sha256::digest(receipt_material.as_bytes());
        let mut receipt_bytes = [0_u8; 16];
        receipt_bytes.copy_from_slice(&receipt_digest[..16]);
        ExecutorResult::VerifiedReceipt(DeploymentReceipt {
            id: Uuid::from_bytes(receipt_bytes),
            run_id: run.id,
            generation: run.generation,
            observed_at,
            source_revision: run.target.revision.clone(),
            component_digests,
            profile_digest: run.target.profile_digest.clone(),
            verification_report: format!(
                "SYNTHETIC ScriptedExecutor receipt; no repository build or environment deployment occurred. Evidence {}",
                Self::evidence(run, "verified")
            ),
            result: ReceiptResult::Success,
        })
    }
}

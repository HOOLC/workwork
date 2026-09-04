//! Pure next-action selection from durable state plus explicit world facts.

use std::collections::BTreeSet;

use super::events::{
    AutoWaitEndReason, DeadlineKind, OutstandingItem, Purpose, StepInterruptionReason,
    ToolInvocation, TurnOutcome, HANDOFF_TOOL_NAME,
};
use super::state::{SessionState, ToolDeliveryState};
use super::tools::ToolChange;

#[derive(Clone, Debug)]
pub struct DecisionWorld {
    pub now_ms: i64,
    pub live_tools: BTreeSet<String>,
    pub tool_changes: Vec<ToolChange>,
    pub outstanding: Vec<OutstandingItem>,
    pub estimated_input_tokens: Option<u64>,
    pub input_budget: Option<u64>,
    pub provider_retry_limit: u32,
    pub context_attempt_limit: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    InterruptStep {
        step_id: String,
        reason: StepInterruptionReason,
    },
    InterruptTools {
        invocation_ids: Vec<String>,
    },
    StartTurn,
    EndAutoWait {
        step_id: String,
        reason: AutoWaitEndReason,
    },
    StartStep {
        purpose: Purpose,
        include_pending_tools: bool,
        outstanding: Vec<OutstandingItem>,
    },
    ApplyContext {
        purpose: Purpose,
        document: Option<String>,
        failure: Option<String>,
    },
    FinishTurn {
        outcome: TurnOutcome,
        outstanding: Vec<OutstandingItem>,
    },
    ReachDeadline {
        deadline: DeadlineKind,
    },
    Park {
        until_ms: Option<i64>,
    },
}

pub fn decide(state: &SessionState, world: &DecisionWorld) -> Decision {
    if let Some(step) = &state.active_step {
        return Decision::InterruptStep {
            step_id: step.step_id.clone(),
            reason: StepInterruptionReason::Recovery,
        };
    }

    let interrupted = state
        .pending_tools
        .values()
        .filter(|pending| {
            pending.result.is_none()
                && !world.live_tools.contains(&pending.invocation.invocation_id)
        })
        .map(|pending| pending.invocation.invocation_id.clone())
        .collect::<Vec<_>>();
    if !interrupted.is_empty() {
        return Decision::InterruptTools {
            invocation_ids: interrupted,
        };
    }

    if state
        .active_turn
        .as_ref()
        .is_some_and(|turn| turn.cancel_requested)
    {
        return Decision::FinishTurn {
            outcome: TurnOutcome::Cancelled,
            outstanding: world.outstanding.clone(),
        };
    }

    if let Some(wait) = &state.auto_wait {
        let all_finished = wait.invocation_ids.iter().all(|id| {
            state
                .pending(id)
                .is_none_or(|pending| pending.result.is_some())
        });
        let reason = if all_finished {
            Some(AutoWaitEndReason::BatchCompleted)
        } else if !state.unconsumed_inputs.is_empty() {
            Some(AutoWaitEndReason::NewInput)
        } else if world.now_ms >= wait.deadline_ms {
            Some(AutoWaitEndReason::TimedOut)
        } else {
            None
        };
        return reason.map_or(
            Decision::Park {
                until_ms: Some(wait.deadline_ms),
            },
            |reason| Decision::EndAutoWait {
                step_id: wait.step_id.clone(),
                reason,
            },
        );
    }

    if let Some(turn) = &state.active_turn {
        if turn.consecutive_provider_failures > 0 {
            let failure = state.last_step_failure.as_ref();
            if failure.is_some_and(|failure| failure.error.is_context_overflow()) {
                let purpose = state.context_progress().map_or_else(
                    || state.context_config.strategy.into(),
                    |progress| progress.purpose,
                );
                // An independent summary can still rescue a conversation that
                // the provider rejected as too large. A rejected maintenance
                // request itself must take the shared no-document exit.
                if purpose == Purpose::Compaction && state.context_progress().is_none() {
                    return Decision::StartStep {
                        purpose,
                        include_pending_tools: true,
                        outstanding: Vec::new(),
                    };
                }
                return Decision::ApplyContext {
                    purpose,
                    document: None,
                    failure: failure.map(|failure| failure.error.message.clone()),
                };
            }
            if !turn.provider_retry_allowed
                || turn.consecutive_provider_failures >= world.provider_retry_limit.max(1)
            {
                return Decision::FinishTurn {
                    outcome: TurnOutcome::Failed,
                    outstanding: world.outstanding.clone(),
                };
            }
            let purpose = state.context_progress().map_or_else(
                || failure.map_or(Purpose::Conversation, |failure| failure.purpose),
                |progress| progress.purpose,
            );
            return Decision::StartStep {
                purpose,
                include_pending_tools: true,
                outstanding: Vec::new(),
            };
        }
    }

    if let Some(wait) = &state.wait_deadline {
        if world.now_ms >= wait.deadline_ms {
            return Decision::ReachDeadline {
                deadline: DeadlineKind::Wait {
                    invocation_id: wait.invocation_id.clone(),
                },
            };
        }
        if state.unconsumed_inputs.is_empty() && state.pending_notices.is_empty() {
            return Decision::Park {
                until_ms: Some(wait.deadline_ms),
            };
        }
    }

    if state.active_turn.is_none() {
        let completed_tool_result = state
            .pending_tools
            .values()
            .any(|pending| pending.result.is_some())
            && state.last_turn_outcome != Some(TurnOutcome::Cancelled);
        let proactive_notice = !state.pending_notices.is_empty()
            && state.last_turn_outcome != Some(TurnOutcome::Failed);
        if !state.unconsumed_inputs.is_empty() || proactive_notice || completed_tool_result {
            return Decision::StartTurn;
        }
        return Decision::Park {
            until_ms: state.wait_deadline.as_ref().map(|wait| wait.deadline_ms),
        };
    }

    if let Some(progress) = state.context_progress() {
        if let Some(reason) = context_limit_failure(state, world) {
            return Decision::ApplyContext {
                purpose: progress.purpose,
                document: None,
                failure: Some(reason),
            };
        }
        return Decision::StartStep {
            purpose: progress.purpose,
            include_pending_tools: true,
            outstanding: Vec::new(),
        };
    }

    if let Some(decision) = control_decision(state, world) {
        return decision;
    }

    if should_prepare_context(state, world) {
        return Decision::StartStep {
            purpose: state.context_config.strategy.into(),
            include_pending_tools: true,
            outstanding: Vec::new(),
        };
    }

    let include_pending_tools = state
        .pending_tools
        .values()
        .any(|pending| pending.delivery == ToolDeliveryState::Fresh);
    Decision::StartStep {
        purpose: Purpose::Conversation,
        include_pending_tools,
        outstanding: if state
            .latest_assistant()
            .is_some_and(|(_, invocations)| invocations.is_empty())
        {
            world.outstanding.clone()
        } else {
            Vec::new()
        },
    }
}

fn context_limit_failure(state: &SessionState, world: &DecisionWorld) -> Option<String> {
    let progress = state.context_progress()?;
    (progress.attempts >= world.context_attempt_limit.max(1)).then(|| {
        format!(
            "the model did not produce a valid context document in {} attempts",
            progress.attempts
        )
    })
}

fn should_prepare_context(state: &SessionState, world: &DecisionWorld) -> bool {
    state.anchor().is_some()
        && !state.generation.entries.is_empty()
        && matches!(
            (world.estimated_input_tokens, world.input_budget),
            (Some(estimate), Some(budget)) if estimate > budget
        )
}

fn control_decision(state: &SessionState, world: &DecisionWorld) -> Option<Decision> {
    let (_, invocations) = state.latest_assistant()?;

    if let Some(end) = invocations.iter().find(|invocation| {
        invocation.rejection.is_none() && invocation.tool == super::events::END_TOOL_NAME
    }) {
        let pending = state.pending(&end.invocation_id)?;
        if pending.result.as_ref()?.outcome != super::events::ToolOutcome::Succeeded {
            return None;
        }
        let acknowledge = end
            .arguments
            .get("acknowledge_outstanding")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if world.outstanding.is_empty() || acknowledge {
            return Some(Decision::FinishTurn {
                outcome: TurnOutcome::Finished,
                outstanding: world.outstanding.clone(),
            });
        }
        return Some(Decision::StartStep {
            purpose: Purpose::Conversation,
            include_pending_tools: true,
            outstanding: world.outstanding.clone(),
        });
    }

    None
}

pub(super) fn handoff_document(invocations: &[ToolInvocation]) -> Result<String, String> {
    let [invocation] = invocations else {
        return Err(format!(
            "expected exactly one handoff call, received {} calls",
            invocations.len()
        ));
    };
    if let Some(reason) = &invocation.rejection {
        return Err(format!("invalid provider call: {reason}"));
    }
    if invocation.tool != HANDOFF_TOOL_NAME {
        return Err(format!("expected handoff, received {}", invocation.tool));
    }
    let Some(arguments) = invocation.arguments.as_object() else {
        return Err("handoff arguments must be an object".into());
    };
    let Some(document) = arguments
        .get("document")
        .and_then(serde_json::Value::as_str)
    else {
        return Err("handoff document must be a string".into());
    };
    if document.trim().is_empty() {
        return Err("handoff document must not be empty".into());
    }
    Ok(document.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::state::{ActiveTurn, ContextProgress};

    #[test]
    // Contract: docs/zork-agent-architecture.md [HANDOFF-01, RETRY-01]
    fn interrupted_handoff_retry_keeps_its_purpose_and_retry_limit() {
        let mut state = SessionState::empty("session");
        state.active_turn = Some(ActiveTurn {
            turn_id: "turn".into(),
            started_at_ms: 0,
            cancel_requested: false,
            consecutive_provider_failures: 1,
            provider_retry_allowed: true,
            context: Some(ContextProgress {
                purpose: Purpose::Handoff,
                attempts: 0,
                source_entries: 0,
                retain_from: 0,
            }),
        });
        let world = DecisionWorld {
            now_ms: 0,
            live_tools: BTreeSet::new(),
            tool_changes: Vec::new(),
            outstanding: Vec::new(),
            estimated_input_tokens: None,
            input_budget: None,
            provider_retry_limit: 10,
            context_attempt_limit: 3,
        };

        assert_eq!(
            decide(&state, &world),
            Decision::StartStep {
                purpose: Purpose::Handoff,
                include_pending_tools: true,
                outstanding: Vec::new(),
            }
        );

        state
            .active_turn
            .as_mut()
            .unwrap()
            .consecutive_provider_failures = 10;
        assert!(matches!(
            decide(&state, &world),
            Decision::FinishTurn {
                outcome: TurnOutcome::Failed,
                ..
            }
        ));
    }
}

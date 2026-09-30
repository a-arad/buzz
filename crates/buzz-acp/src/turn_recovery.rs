//! Durable correlation between harness delivery and native execution receipts.
use std::sync::Arc;

use crate::acp::{AcpError, StopReason};
use crate::pool::{
    OwnedAgent, PromptContext, PromptOutcome, PromptResult, PromptSource, SessionState,
};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Pending {
    pub attempt: String,
    pub session: String,
    pub source: PromptSource,
    pub delivered: Vec<String>,
    pub standing: bool,
    pub reminder: Option<(String, String)>,
}

impl SessionState {
    pub(crate) fn complete_recovery(&mut self, completed: bool) -> anyhow::Result<()> {
        if let Some(pending) = self.pending_turn.take() {
            if completed {
                match pending.source {
                    PromptSource::Channel(scope) => {
                        self.mark_scope_delivery_success(scope, pending.standing, pending.delivered)
                    }
                    _ => self.heartbeat_standing_context_sent |= pending.standing,
                }
                if let Some(reminder) = pending.reminder {
                    self.recovered_reminders.push(reminder);
                }
            }
        }
        self.turn_active = false;
        self.recovery_held = false;
        self.checkpoint()
    }
}

pub(crate) fn begin(agent: &mut OwnedAgent, pending: Pending) -> Result<(), AcpError> {
    if !agent.acp.recovery_supported() {
        return Ok(());
    }
    if agent.state.store.is_none() {
        return Err(AcpError::Protocol(
            "durable adapter requires a harness session store".into(),
        ));
    }
    agent.acp.set_recovery_attempt(pending.attempt.clone());
    agent.state.turn_active = true;
    agent.state.pending_turn = Some(pending);
    agent
        .state
        .checkpoint()
        .map_err(|e| AcpError::Protocol(e.to_string()))
}

pub(crate) async fn run(
    mut agent: OwnedAgent,
    ctx: Arc<PromptContext>,
    result_tx: tokio::sync::mpsc::UnboundedSender<PromptResult>,
) {
    let Some(pending) = agent.state.pending_turn.clone() else {
        return;
    };
    let result = async {
        if agent.state.pending_loads.contains(&pending.session) {
            agent
                .acp
                .session_load(&pending.session, &ctx.cwd, ctx.mcp_servers.clone())
                .await?;
            agent.state.pending_loads.remove(&pending.session);
        }
        agent
            .acp
            .recover_turn(
                &pending.session,
                &pending.attempt,
                ctx.idle_timeout,
                ctx.max_turn_duration,
            )
            .await
    }
    .await;
    let outcome = match result {
        Ok(StopReason::EndTurn) => match agent.state.complete_recovery(true) {
            Ok(()) => {
                tracing::info!(attempt = %pending.attempt, session = %pending.session, "native_turn_recovered");
                PromptOutcome::Ok(StopReason::EndTurn)
            }
            Err(error) => {
                agent.state.store_error = Some(error.to_string());
                agent.state.recovery_held = true;
                PromptOutcome::Error(AcpError::Protocol(error.to_string()))
            }
        },
        Ok(StopReason::Cancelled) => match agent.state.complete_recovery(false) {
            Ok(()) => PromptOutcome::Ok(StopReason::Cancelled),
            Err(error) => {
                agent.state.store_error = Some(error.to_string());
                agent.state.recovery_held = true;
                PromptOutcome::Error(AcpError::Protocol(error.to_string()))
            }
        },
        result => {
            agent.state.recovery_held = true;
            tracing::error!(attempt = %pending.attempt, session = %pending.session, ?result, "native_recovery_requires_attention");
            if let Err(error) = agent.state.checkpoint() {
                agent.state.store_error = Some(error.to_string());
            }
            PromptOutcome::Error(AcpError::AgentError {
                code: -32073,
                message: "native recovery requires inspection; conversation retained".into(),
            })
        }
    };
    let _ = result_tx.send(PromptResult {
        agent,
        source: PromptSource::Heartbeat,
        turn_id: pending.attempt,
        outcome,
        batch: None,
    });
}

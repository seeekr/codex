use std::sync::Arc;

use codex_protocol::models::ResponseItem;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::session::TurnInput;
use crate::session::turn::run_turn;
use crate::session::turn_context::TurnContext;
use crate::session_startup_prewarm::SessionStartupPrewarmResolution;
use crate::state::TaskKind;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnStartedEvent;
use tracing::Instrument;
use tracing::trace_span;

use super::SessionTask;
use super::SessionTaskContext;
use super::SessionTaskResult;

struct CorrectionPreflight {
    frame: ResponseItem,
    result: oneshot::Sender<codex_protocol::error::Result<()>>,
}

#[derive(Default)]
pub(crate) struct RegularTask {
    correction_preflight: Mutex<Option<CorrectionPreflight>>,
}

impl RegularTask {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn with_correction(
        frame: ResponseItem,
    ) -> (Self, oneshot::Receiver<codex_protocol::error::Result<()>>) {
        let (result, receiver) = oneshot::channel();
        (
            Self {
                correction_preflight: Mutex::new(Some(CorrectionPreflight { frame, result })),
            },
            receiver,
        )
    }
}

impl SessionTask for RegularTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.turn"
    }

    fn defers_steer_until_turn_started(&self) -> bool {
        true
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        let sess = session.clone_session();
        let turn_extension_data = session.turn_extension_data();
        let run_turn_span = trace_span!("run_turn");
        // Regular turns emit `TurnStarted` inline so first-turn lifecycle does
        // not wait on startup prewarm resolution.
        let event = EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: ctx.sub_id.clone(),
            trace_id: ctx.trace_id.clone(),
            started_at: ctx.turn_timing_state.started_at_unix_secs().await,
            model_context_window: ctx.model_context_window(),
            collaboration_mode_kind: ctx.collaboration_mode.mode,
        });
        sess.send_event(ctx.as_ref(), event).await;
        sess.publish_turn_started_for_steering(&ctx.sub_id).await;

        // Idle correction commits enter the durable turn segment only after TurnStarted and
        // before the first model request. The RPC waiter is released only after append, flush, and
        // live-history installation all succeed.
        if let Some(preflight) = self.correction_preflight.lock().await.take() {
            match sess
                .persist_and_install_correction(ctx.as_ref(), preflight.frame)
                .await
            {
                Ok(()) => {
                    let _ = preflight.result.send(Ok(()));
                }
                Err(error) => {
                    let _ = preflight.result.send(Err(error));
                    return Ok(None);
                }
            }
        }

        let prewarmed_client_session = async {
            sess.set_server_reasoning_included(/*included*/ false).await;
            sess.consume_startup_prewarm_for_regular_turn(&cancellation_token)
                .await
        }
        .instrument(trace_span!("regular_task.prepare_run_turn"))
        .await;
        let prewarmed_client_session = match prewarmed_client_session {
            SessionStartupPrewarmResolution::Cancelled => return Ok(None),
            SessionStartupPrewarmResolution::Unavailable { .. } => None,
            SessionStartupPrewarmResolution::Ready(prewarmed_client_session) => {
                Some(*prewarmed_client_session)
            }
        };
        let mut next_input = input;
        let mut prewarmed_client_session = prewarmed_client_session;
        loop {
            let last_agent_message = run_turn(
                Arc::clone(&sess),
                Arc::clone(&ctx),
                Arc::clone(&turn_extension_data),
                next_input,
                prewarmed_client_session.take(),
                cancellation_token.child_token(),
            )
            .instrument(run_turn_span.clone())
            .await?;
            if sess.close_regular_turn_steering_if_idle(&ctx.sub_id).await {
                return Ok(last_agent_message);
            }
            next_input = Vec::new();
        }
    }
}

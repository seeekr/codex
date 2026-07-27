mod compact;
mod lifecycle;
mod regular;
mod review;
mod user_shell;

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use codex_extension_api::ExtensionData;
use futures::future::BoxFuture;
use tokio::select;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;
use tracing::Span;
use tracing::field;
use tracing::info_span;
use tracing::trace;
use tracing::trace_span;
use tracing::warn;

use crate::codex_thread::BackgroundTerminalInfo;
use crate::config::Config;
use crate::context::ContextualUserFragment;
use crate::hook_runtime::inspect_pending_input;
use crate::hook_runtime::record_additional_contexts;
use crate::hook_runtime::record_pending_input;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::ActiveTurn;
use crate::state::RunningTask;
use crate::state::TaskKind;
use crate::state::TaskPublication;
use codex_analytics::TurnProfileFact;
use codex_analytics::TurnTokenUsageFact;
use codex_login::AuthManager;
use codex_models_manager::manager::SharedModelsManager;
use codex_otel::SessionTelemetry;
use codex_otel::TURN_E2E_DURATION_METRIC;
use codex_otel::TURN_MEMORY_METRIC;
use codex_otel::TURN_NETWORK_PROXY_METRIC;
use codex_otel::TURN_TOKEN_USAGE_METRIC;
use codex_otel::TURN_TOOL_CALL_METRIC;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::WarningEvent;

use codex_features::Feature;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ContentItem;
pub(crate) use compact::CompactTask;
pub(crate) use regular::RegularTask;
pub(crate) use review::ReviewTask;
pub(crate) use user_shell::UserShellCommandMode;
pub(crate) use user_shell::UserShellCommandTask;
pub(crate) use user_shell::execute_user_shell_command;

const GRACEFULL_INTERRUPTION_TIMEOUT_MS: u64 = 100;
const TASK_COMPACT_METRIC: &str = "codex.task.compact";

pub(crate) type SessionTaskResult = CodexResult<Option<String>>;

pub(crate) struct TaskStartReservation {
    pub(crate) turn_state: Arc<tokio::sync::Mutex<crate::state::TurnState>>,
    startup_done: Arc<TaskPublication>,
    resumes_real_work: bool,
}

impl Drop for TaskStartReservation {
    fn drop(&mut self) {
        self.startup_done.abandon();
    }
}

struct TerminalPublicationGuard {
    done: Arc<TaskPublication>,
    published: bool,
}

impl TerminalPublicationGuard {
    fn new(done: Arc<TaskPublication>) -> Self {
        Self {
            done,
            published: false,
        }
    }

    fn done(&self) -> &Arc<TaskPublication> {
        &self.done
    }

    fn mark_published(&mut self) {
        self.published = true;
    }
}

impl Drop for TerminalPublicationGuard {
    fn drop(&mut self) {
        if !self.published {
            self.done.abandon();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterruptedTurnHistoryMarker {
    Disabled,
    ContextualUser,
    Developer,
}

impl InterruptedTurnHistoryMarker {
    pub(crate) fn from_config_and_version(
        config: &Config,
        multi_agent_version: MultiAgentVersion,
    ) -> Self {
        if !config.agent_interrupt_message_enabled {
            return Self::Disabled;
        }
        if multi_agent_version == MultiAgentVersion::V2 {
            Self::Developer
        } else {
            Self::ContextualUser
        }
    }
}

/// Shared model-visible marker used by both the real interrupt path and
/// interrupted fork snapshots.
pub(crate) fn interrupted_turn_history_marker(
    marker: InterruptedTurnHistoryMarker,
) -> Option<ResponseItem> {
    match marker {
        InterruptedTurnHistoryMarker::Disabled => None,
        InterruptedTurnHistoryMarker::ContextualUser => Some(ContextualUserFragment::into(
            crate::context::TurnAborted::new(crate::context::TurnAborted::INTERRUPTED_GUIDANCE),
        )),
        InterruptedTurnHistoryMarker::Developer => {
            let marker = crate::context::TurnAborted::new(
                crate::context::TurnAborted::INTERRUPTED_DEVELOPER_GUIDANCE,
            );
            Some(ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: marker.render(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            })
        }
    }
}

fn emit_turn_network_proxy_metric(
    session_telemetry: &SessionTelemetry,
    network_proxy_active: bool,
    tmp_mem: (&str, &str),
) {
    let active = if network_proxy_active {
        "true"
    } else {
        "false"
    };
    session_telemetry.counter(
        TURN_NETWORK_PROXY_METRIC,
        /*inc*/ 1,
        &[("active", active), tmp_mem],
    );
}

fn emit_turn_memory_metric(
    session_telemetry: &SessionTelemetry,
    feature_enabled: bool,
    config_enabled: bool,
    has_citations: bool,
) {
    let read_allowed = feature_enabled && config_enabled;
    session_telemetry.counter(
        TURN_MEMORY_METRIC,
        /*inc*/ 1,
        &[
            ("read_allowed", bool_tag(read_allowed)),
            ("feature_enabled", bool_tag(feature_enabled)),
            ("config_use_memories", bool_tag(config_enabled)),
            ("has_citations", bool_tag(has_citations)),
        ],
    );
}

pub(crate) fn emit_compact_metric(
    session_telemetry: &SessionTelemetry,
    compact_type: &'static str,
    manual: bool,
) {
    session_telemetry.counter(
        TASK_COMPACT_METRIC,
        /*inc*/ 1,
        &[("type", compact_type), ("manual", bool_tag(manual))],
    );
}

fn bool_tag(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

/// Thin wrapper that exposes the parts of [`Session`] task runners need.
#[derive(Clone)]
pub(crate) struct SessionTaskContext {
    session: Arc<Session>,
    turn_extension_data: Arc<ExtensionData>,
}

impl SessionTaskContext {
    pub(crate) fn new(session: Arc<Session>, turn_extension_data: Arc<ExtensionData>) -> Self {
        Self {
            session,
            turn_extension_data,
        }
    }

    pub(crate) fn clone_session(&self) -> Arc<Session> {
        Arc::clone(&self.session)
    }

    pub(crate) fn turn_extension_data(&self) -> Arc<ExtensionData> {
        Arc::clone(&self.turn_extension_data)
    }

    pub(crate) fn auth_manager(&self) -> Arc<AuthManager> {
        Arc::clone(&self.session.services.auth_manager)
    }

    pub(crate) fn models_manager(&self) -> SharedModelsManager {
        Arc::clone(&self.session.services.models_manager)
    }
}

/// Async task that drives a [`Session`] turn.
///
/// Implementations encapsulate a specific Codex workflow (regular chat,
/// reviews, ghost snapshots, etc.). Each task instance is owned by a
/// [`Session`] and executed on a background Tokio task. The trait is
/// intentionally small: implementers identify themselves via
/// [`SessionTask::kind`], perform their work in [`SessionTask::run`], and may
/// release resources in [`SessionTask::abort`].
pub(crate) trait SessionTask: Send + Sync + 'static {
    /// Describes the type of work the task performs so the session can
    /// surface it in telemetry and UI.
    fn kind(&self) -> TaskKind;

    /// Returns the tracing name for a spawned task span.
    fn span_name(&self) -> &'static str;

    /// Whether steerable input must remain closed until the task explicitly publishes its
    /// model-visible `TurnStarted` event.
    fn defers_steer_until_turn_started(&self) -> bool {
        false
    }

    /// Executes the task until completion or cancellation.
    ///
    /// Implementations typically stream protocol events using `session` and
    /// `ctx`, returning an optional final agent message when finished. The
    /// provided `cancellation_token` is cancelled when the session requests an
    /// abort; implementers should watch for it and terminate quickly once it
    /// fires. Returning [`Some`] yields a final message that
    /// [`Session::on_task_finished`] will emit to the client. Returning
    /// [`CodexErr::TurnAborted`] completes the task through the aborted-turn
    /// lifecycle instead.
    fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = SessionTaskResult> + Send;

    /// Gives the task a chance to perform cleanup after an abort.
    ///
    /// The default implementation is a no-op; override this if additional
    /// teardown or notifications are required once
    /// [`Session::abort_all_tasks`] cancels the task.
    fn abort(
        &self,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
    ) -> impl std::future::Future<Output = ()> + Send {
        async move {
            let _ = (session, ctx);
        }
    }
}

pub(crate) trait AnySessionTask: Send + Sync + 'static {
    fn kind(&self) -> TaskKind;

    fn span_name(&self) -> &'static str;

    fn defers_steer_until_turn_started(&self) -> bool;

    fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> BoxFuture<'static, SessionTaskResult>;

    fn abort<'a>(
        &'a self,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
    ) -> BoxFuture<'a, ()>;
}

impl<T> AnySessionTask for T
where
    T: SessionTask,
{
    fn kind(&self) -> TaskKind {
        SessionTask::kind(self)
    }

    fn span_name(&self) -> &'static str {
        SessionTask::span_name(self)
    }

    fn defers_steer_until_turn_started(&self) -> bool {
        SessionTask::defers_steer_until_turn_started(self)
    }

    fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> BoxFuture<'static, SessionTaskResult> {
        Box::pin(SessionTask::run(
            self,
            session,
            ctx,
            input,
            cancellation_token,
        ))
    }

    fn abort<'a>(
        &'a self,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
    ) -> BoxFuture<'a, ()> {
        Box::pin(SessionTask::abort(self, session, ctx))
    }
}

impl Session {
    pub(crate) async fn reserve_task_start(&self) -> Option<TaskStartReservation> {
        self.reserve_task_start_inner(false).await
    }

    /// Reserves a startup slot tagged to resume user-visible work.
    ///
    /// The prior Stop latch remains unchanged until the validated task is
    /// installed. Stop can therefore wait on a taskless reservation without
    /// losing a durable latch if startup is released or abandoned.
    pub(crate) async fn reserve_real_work_start(&self) -> Option<TaskStartReservation> {
        self.reserve_task_start_inner(true).await
    }

    async fn reserve_task_start_inner(
        &self,
        resumes_real_work: bool,
    ) -> Option<TaskStartReservation> {
        let mut active = self.active_turn.lock().await;
        if active.as_ref().is_some_and(|active_turn| {
            active_turn.task.is_none()
                && (active_turn
                    .startup_done
                    .as_ref()
                    .is_some_and(|publication| publication.is_abandoned())
                    || active_turn
                        .terminal_done
                        .as_ref()
                        .is_some_and(|publication| publication.is_abandoned()))
        }) {
            *active = None;
        }
        if active.is_some() {
            return None;
        }
        let startup_done = Arc::new(TaskPublication::new());
        let active_turn = ActiveTurn {
            startup_done: Some(Arc::clone(&startup_done)),
            ..Default::default()
        };
        let reservation = TaskStartReservation {
            turn_state: Arc::clone(&active_turn.turn_state),
            startup_done,
            resumes_real_work,
        };
        *active = Some(active_turn);
        Some(reservation)
    }

    pub(crate) async fn release_task_start(&self, reservation: &TaskStartReservation) {
        let released = {
            let mut active = self.active_turn.lock().await;
            if active.as_ref().is_some_and(|active_turn| {
                active_turn.task.is_none()
                    && Arc::ptr_eq(&active_turn.turn_state, &reservation.turn_state)
                    && active_turn
                        .startup_done
                        .as_ref()
                        .is_some_and(|done| Arc::ptr_eq(done, &reservation.startup_done))
            }) {
                *active = None;
                true
            } else {
                false
            }
        };
        if released {
            reservation.startup_done.publish();
        }
    }

    pub async fn spawn_task<T: SessionTask>(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: Vec<TurnInput>,
        task: T,
    ) {
        let reservation = loop {
            self.abort_all_tasks(TurnAbortReason::Replaced).await;
            self.clear_connector_selection().await;
            if let Some(reservation) = self.reserve_real_work_start().await {
                break reservation;
            }
        };
        assert!(
            self.start_reserved_task(reservation, turn_context, input, task)
                .await,
            "task-start reservation must remain owned until task publication"
        );
    }

    pub(crate) fn start_reserved_task<T: SessionTask>(
        self: &Arc<Self>,
        reservation: TaskStartReservation,
        turn_context: Arc<TurnContext>,
        input: Vec<TurnInput>,
        task: T,
    ) -> BoxFuture<'static, bool> {
        let session = Arc::clone(self);
        Box::pin(async move {
            session
                .start_reserved_task_inner(reservation, turn_context, input, task)
                .await
        })
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "the active-turn lock linearizes task publication with clearing the prior Stop latch"
    )]
    async fn start_reserved_task_inner<T: SessionTask>(
        self: &Arc<Self>,
        reservation: TaskStartReservation,
        turn_context: Arc<TurnContext>,
        input: Vec<TurnInput>,
        task: T,
    ) -> bool {
        let task: Arc<dyn AnySessionTask> = Arc::new(task);
        let task_kind = task.kind();
        let correction_bootstrap = task_kind == TaskKind::Regular
            && input
                .iter()
                .any(|item| matches!(item, TurnInput::CommittedCorrection));
        let accepts_steer = !task.defers_steer_until_turn_started();
        let span_name = task.span_name();
        let started_at = Instant::now();
        let turn_started_at_unix_ms = turn_context
            .turn_timing_state
            .mark_turn_started(started_at)
            .await;
        turn_context
            .turn_metadata_state
            .set_turn_started_at_unix_ms(turn_started_at_unix_ms);
        let token_usage_at_turn_start = self.total_token_usage().await.unwrap_or_default();

        let cancellation_token = CancellationToken::new();
        let done = Arc::new(Notify::new());
        let terminal_done = Arc::new(TaskPublication::new());

        self.services
            .guardian_rejection_circuit_breaker
            .lock()
            .await
            .clear_turn(&turn_context.sub_id);

        let turn_state = Arc::clone(&reservation.turn_state);

        let turn_extension_data = Arc::clone(&turn_context.extension_data);
        let mut active = self.active_turn.lock().await;
        let Some(turn) = active.as_mut() else {
            return false;
        };
        if turn.task.is_some()
            || !Arc::ptr_eq(&turn.turn_state, &reservation.turn_state)
            || !turn
                .startup_done
                .as_ref()
                .is_some_and(|done| Arc::ptr_eq(done, &reservation.startup_done))
        {
            return false;
        }
        let agent_execution_guard = self.services.agent_control.execution_guard(
            turn_context.multi_agent_version,
            &turn_context.session_source,
        );
        let done_clone = Arc::clone(&done);
        let session_ctx = Arc::new(SessionTaskContext::new(
            Arc::clone(self),
            Arc::clone(&turn_extension_data),
        ));
        let ctx = Arc::clone(&turn_context);
        let task_for_run = Arc::clone(&task);
        let task_input = input;
        let task_cancellation_token = cancellation_token.child_token();
        let (start_tx, start_rx) = oneshot::channel();
        // Task-owned turn spans keep a core-owned span open for the
        // full task lifecycle after the submission dispatch span ends.
        let reasoning_effort = turn_context.effective_reasoning_effort_for_tracing();
        let task_span = info_span!(
            "turn",
            otel.name = span_name,
            thread.id = %self.thread_id,
            turn.id = %turn_context.sub_id,
            model = %turn_context.model_info.slug,
            codex.turn.reasoning_effort = %reasoning_effort,
            codex.turn.token_usage.input_tokens = field::Empty,
            codex.turn.token_usage.cached_input_tokens = field::Empty,
            codex.turn.token_usage.non_cached_input_tokens = field::Empty,
            codex.turn.token_usage.output_tokens = field::Empty,
            codex.turn.token_usage.reasoning_output_tokens = field::Empty,
            codex.turn.token_usage.total_tokens = field::Empty,
        );
        let handle = tokio::spawn(
            async move {
                if start_rx.await.is_err() {
                    done_clone.notify_waiters();
                    return;
                }
                let ctx_for_finish = Arc::clone(&ctx);
                let sess = session_ctx.clone_session();
                let pending_items = sess.input_queue.get_pending_input(&sess.active_turn).await;
                turn_state.lock().await.token_usage_at_turn_start =
                    token_usage_at_turn_start.clone();
                sess.input_queue
                    .extend_pending_input_for_turn_state(turn_state.as_ref(), pending_items)
                    .await;
                sess.emit_turn_start_lifecycle(
                    ctx_for_finish.as_ref(),
                    &token_usage_at_turn_start,
                )
                .await;
                let task_result = task_for_run
                    .run(
                        Arc::clone(&session_ctx),
                        ctx,
                        task_input,
                        task_cancellation_token.child_token(),
                    )
                    .instrument(trace_span!("session_task.run"))
                    .await;
                if let Err(err) = sess.flush_rollout().await {
                    warn!("failed to flush rollout before completing turn: {err}");
                    sess.send_event(
                        ctx_for_finish.as_ref(),
                        EventMsg::Warning(WarningEvent {
                            message: format!(
                                "Failed to save the conversation transcript; Codex will continue retrying. Error: {err}"
                            ),
                        }),
                    )
                    .await;
                }
                if !task_cancellation_token.is_cancelled() {
                    // Finish uniformly from the spawn site so all tasks share the same lifecycle.
                    Box::pin(
                        sess.on_task_finished(Arc::clone(&ctx_for_finish), task_result),
                    )
                    .await;
                }
                done_clone.notify_waiters();
            }
            .instrument(task_span),
        );
        let timer = turn_context
            .session_telemetry
            .start_timer(TURN_E2E_DURATION_METRIC, &[])
            .ok();
        let running_task = RunningTask {
            done: Arc::clone(&done),
            terminal_done: Arc::clone(&terminal_done),
            handle: AbortOnDropHandle::new(handle),
            kind: task_kind,
            correction_bootstrap,
            accepts_steer,
            task,
            cancellation_token,
            turn_context: Arc::clone(&turn_context),
            turn_extension_data,
            _agent_execution_guard: agent_execution_guard,
            _timer: timer,
        };
        turn.startup_done = None;
        turn.terminal_done = Some(terminal_done);
        turn.task = Some(running_task);
        if turn.interrupt_pending {
            drop(start_tx);
        } else {
            if reservation.resumes_real_work {
                self.allow_correction_auto_start().await;
            }
            assert!(
                start_tx.send(()).is_ok(),
                "newly spawned task must retain its start receiver until publication"
            );
        }
        drop(active);
        reservation.startup_done.publish();
        true
    }

    pub(crate) async fn publish_turn_started_for_steering(&self, turn_id: &str) {
        let mut active = self.active_turn.lock().await;
        let Some(active_turn) = active.as_mut() else {
            return;
        };
        if let Some(task) = active_turn.task.as_mut()
            && task.turn_context.sub_id == turn_id
        {
            task.accepts_steer = true;
        }
    }

    /// Clear the exact closing-turn reservation and wake waiters after its terminal event.
    async fn publish_turn_terminal(&self, done: &Arc<TaskPublication>) {
        let cleared = {
            let mut active = self.active_turn.lock().await;
            if active.as_ref().is_some_and(|active_turn| {
                active_turn.task.is_none()
                    && active_turn
                        .terminal_done
                        .as_ref()
                        .is_some_and(|active_done| Arc::ptr_eq(active_done, done))
            }) {
                *active = None;
                true
            } else {
                false
            }
        };
        if cleared {
            self.emit_thread_idle_lifecycle_if_idle().await;
        }
        done.publish();
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "the active-turn lock linearizes terminal publication with automatic correction follow-up reservation"
    )]
    pub(crate) async fn publish_turn_terminal_and_reserve_correction(
        &self,
        done: &Arc<TaskPublication>,
        should_schedule_correction: bool,
    ) -> Option<TaskStartReservation> {
        let (reservation, became_idle) = {
            let mut active = self.active_turn.lock().await;
            if !active.as_ref().is_some_and(|active_turn| {
                active_turn.task.is_none()
                    && active_turn
                        .terminal_done
                        .as_ref()
                        .is_some_and(|active_done| Arc::ptr_eq(active_done, done))
            }) {
                (None, false)
            } else if should_schedule_correction
                && self.correction_auto_start_allowed().await
                && self.has_pending_corrections().await
            {
                let startup_done = Arc::new(TaskPublication::new());
                let next_turn = ActiveTurn {
                    interrupt_pending: active
                        .as_ref()
                        .is_some_and(|active_turn| active_turn.interrupt_pending),
                    startup_done: Some(Arc::clone(&startup_done)),
                    ..Default::default()
                };
                let reservation = TaskStartReservation {
                    turn_state: Arc::clone(&next_turn.turn_state),
                    startup_done,
                    resumes_real_work: false,
                };
                *active = Some(next_turn);
                (Some(reservation), false)
            } else {
                *active = None;
                (None, true)
            }
        };
        if became_idle {
            self.emit_thread_idle_lifecycle_if_idle().await;
        }
        done.publish();
        reservation
    }

    /// Starts a regular turn when the session is idle and pending work is waiting.
    ///
    /// Pending work currently includes mailbox mail marked with `trigger_turn`.
    ///
    /// This helper generates a fresh sub-id for the synthetic turn before delegating to the
    /// explicit-sub-id variant.
    pub(crate) async fn maybe_start_turn_for_pending_work(self: &Arc<Self>) {
        self.maybe_start_turn_for_pending_work_with_sub_id(uuid::Uuid::new_v4().to_string())
            .await;
    }

    /// Starts a regular turn with the provided sub-id when pending work should wake an idle
    /// session.
    ///
    /// The turn is created only when there is mailbox mail marked with `trigger_turn`, and only
    /// if the session is currently idle.
    pub(crate) async fn maybe_start_turn_for_pending_work_with_sub_id(
        self: &Arc<Self>,
        sub_id: String,
    ) {
        if !self.input_queue.has_trigger_turn_mailbox_items().await {
            return;
        }

        let Some(reservation) = self.reserve_real_work_start().await else {
            return;
        };

        let turn_context = self.new_default_turn_with_sub_id(sub_id).await;
        self.maybe_emit_model_warnings_for_turn(turn_context.as_ref())
            .await;
        assert!(
            self.start_reserved_task(reservation, turn_context, Vec::new(), RegularTask::new())
                .await,
            "pending-work task-start reservation must remain owned until task publication"
        );
    }

    pub async fn abort_all_tasks(self: &Arc<Self>, reason: TurnAbortReason) {
        self.abort_all_tasks_inner(reason).await;
    }

    pub(crate) async fn interrupt_all_tasks(self: &Arc<Self>) -> bool {
        self.abort_all_tasks_inner(TurnAbortReason::Interrupted)
            .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "the active-turn lock makes the Stop latch atomic with taking the running task"
    )]
    async fn abort_all_tasks_inner(self: &Arc<Self>, reason: TurnAbortReason) -> bool {
        let (task, turn_state) = loop {
            let (task, turn_state, pending_publication) = {
                let mut active = self.active_turn.lock().await;
                match active.as_mut() {
                    Some(active_turn) => match active_turn.task.take() {
                        Some(task) => {
                            if reason == TurnAbortReason::Interrupted {
                                self.suppress_correction_auto_start().await;
                            }
                            (Some(task), Some(Arc::clone(&active_turn.turn_state)), None)
                        }
                        None => {
                            if let Some(startup) = active_turn.startup_done.as_ref().map(Arc::clone)
                            {
                                let notified = startup.notified();
                                if startup.is_abandoned() {
                                    *active = None;
                                    (None, None, None)
                                } else {
                                    active_turn.interrupt_pending = true;
                                    (None, None, Some(notified))
                                }
                            } else if let Some(terminal) =
                                active_turn.terminal_done.as_ref().map(Arc::clone)
                            {
                                let notified = terminal.notified();
                                if terminal.is_abandoned() {
                                    *active = None;
                                    (None, None, None)
                                } else {
                                    active_turn.interrupt_pending = true;
                                    (None, None, Some(notified))
                                }
                            } else {
                                *active = None;
                                (None, None, None)
                            }
                        }
                    },
                    None => (None, None, None),
                }
            };
            if let Some(pending_publication) = pending_publication {
                pending_publication.await;
                continue;
            }
            break (task, turn_state);
        };

        let turn_context = task.as_ref().map(|task| Arc::clone(&task.turn_context));
        let mut terminal_guard = task
            .as_ref()
            .map(|task| TerminalPublicationGuard::new(Arc::clone(&task.terminal_done)));
        if let Some(task) = task {
            self.handle_task_abort(task, reason.clone()).await;
        }
        if let Some(turn_context) = turn_context.as_deref() {
            self.emit_turn_abort_lifecycle(reason.clone(), turn_context.extension_data.as_ref())
                .await;
        }
        if let Some(turn_state) = turn_state.as_ref() {
            // Let interrupted tasks observe cancellation before dropping pending approvals, or an
            // in-flight approval wait can surface as a model-visible rejection before TurnAborted.
            self.input_queue
                .clear_pending_for_turn_state(turn_state.as_ref())
                .await;
        }
        if let Some(terminal_guard) = terminal_guard.as_mut() {
            self.publish_turn_terminal(terminal_guard.done()).await;
            terminal_guard.mark_published();
        }
        if reason == TurnAbortReason::Interrupted && turn_context.is_some() {
            self.maybe_start_turn_for_pending_work().await;
        }
        turn_context.is_some()
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "the active-turn lock makes the targeted Stop latch atomic with taking its task"
    )]
    pub(crate) async fn abort_turn_if_active(
        self: &Arc<Self>,
        turn_id: &str,
        reason: TurnAbortReason,
    ) -> bool {
        let (task, turn_state) = {
            let mut active = self.active_turn.lock().await;
            let Some(active_turn) = active.as_mut() else {
                return false;
            };
            if active_turn
                .task
                .as_ref()
                .is_none_or(|task| task.turn_context.sub_id != turn_id)
            {
                return false;
            }
            let task = active_turn.task.take();
            if task.is_some() && reason == TurnAbortReason::Interrupted {
                self.suppress_correction_auto_start().await;
            }
            (task, Arc::clone(&active_turn.turn_state))
        };
        let Some(task) = task else {
            return false;
        };

        let turn_context = Arc::clone(&task.turn_context);
        let mut terminal_guard = TerminalPublicationGuard::new(Arc::clone(&task.terminal_done));
        self.handle_task_abort(task, reason.clone()).await;
        self.emit_turn_abort_lifecycle(reason.clone(), turn_context.extension_data.as_ref())
            .await;
        // Let interrupted tasks observe cancellation before dropping pending approvals, or an
        // in-flight approval wait can surface as a model-visible rejection before TurnAborted.
        self.input_queue
            .clear_pending_for_turn_state(turn_state.as_ref())
            .await;
        self.publish_turn_terminal(terminal_guard.done()).await;
        terminal_guard.mark_published();

        if reason == TurnAbortReason::Interrupted {
            self.maybe_start_turn_for_pending_work().await;
        }

        true
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "the active-turn lock makes a self-aborted task's Stop latch atomic with terminal ownership"
    )]
    pub async fn on_task_finished(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        task_result: SessionTaskResult,
    ) -> bool {
        let task_succeeded = task_result.is_ok();
        let (last_agent_message, abort_reason) = match task_result {
            Ok(last_agent_message) => (last_agent_message, None),
            Err(CodexErr::TurnAborted) => (None, Some(TurnAbortReason::Interrupted)),
            Err(err) => {
                warn!(%err, "session task returned an unexpected error");
                (None, None)
            }
        };
        let terminal_interrupted = abort_reason
            .as_ref()
            .is_some_and(|reason| *reason == TurnAbortReason::Interrupted);
        turn_context
            .turn_metadata_state
            .cancel_git_enrichment_task();

        let turn_state_and_kind = {
            let mut active = self.active_turn.lock().await;
            active.as_mut().and_then(|active_turn| {
                let task = active_turn.task.as_mut()?;
                if task.turn_context.sub_id != turn_context.sub_id {
                    return None;
                }
                task.accepts_steer = false;
                Some((
                    Arc::clone(&active_turn.turn_state),
                    task.kind,
                    task.correction_bootstrap,
                ))
            })
        };
        let Some((turn_state, task_kind, correction_bootstrap)) = turn_state_and_kind else {
            return false;
        };
        let pending_input = self
            .input_queue
            .take_pending_input_for_turn_state(turn_state.as_ref())
            .await;
        let (
            turn_had_memory_citation,
            turn_tool_calls,
            token_usage_at_turn_start,
            normal_sampling_attempted,
            completed_normal_sampling,
        ) = {
            let ts = turn_state.lock().await;
            (
                ts.has_memory_citation,
                ts.tool_calls,
                ts.token_usage_at_turn_start.clone(),
                ts.normal_sampling_attempted,
                ts.completed_normal_sampling,
            )
        };
        if !pending_input.is_empty() {
            for pending_input_item in pending_input {
                let hook_outcome =
                    inspect_pending_input(self, &turn_context, &pending_input_item).await;
                if hook_outcome.should_stop {
                    record_additional_contexts(
                        self,
                        &turn_context,
                        hook_outcome.additional_contexts,
                    )
                    .await;
                } else {
                    record_pending_input(
                        self,
                        &turn_context,
                        pending_input_item,
                        hook_outcome.additional_contexts,
                    )
                    .await;
                }
            }
        }
        // Emit token usage metrics.
        {
            // TODO(jif): drop this
            let tmp_mem = (
                "tmp_mem_enabled",
                if self.enabled(Feature::MemoryTool) {
                    "true"
                } else {
                    "false"
                },
            );
            let network_proxy = self.services.network_proxy.load_full();
            let network_proxy_active = match network_proxy.as_ref() {
                Some(started_network_proxy) => {
                    match started_network_proxy.proxy().current_cfg().await {
                        Ok(config) => config.network.enabled,
                        Err(err) => {
                            warn!(
                                "failed to read managed network proxy state for turn metrics: {err:#}"
                            );
                            false
                        }
                    }
                }
                None => false,
            };
            emit_turn_network_proxy_metric(
                &self.services.session_telemetry,
                network_proxy_active,
                tmp_mem,
            );
            self.services.session_telemetry.histogram(
                TURN_TOOL_CALL_METRIC,
                i64::try_from(turn_tool_calls).unwrap_or(i64::MAX),
                &[tmp_mem],
            );
            let total_token_usage = self.total_token_usage().await.unwrap_or_default();
            let turn_token_usage = TokenUsage {
                input_tokens: (total_token_usage.input_tokens
                    - token_usage_at_turn_start.input_tokens)
                    .max(0),
                cached_input_tokens: (total_token_usage.cached_input_tokens
                    - token_usage_at_turn_start.cached_input_tokens)
                    .max(0),
                output_tokens: (total_token_usage.output_tokens
                    - token_usage_at_turn_start.output_tokens)
                    .max(0),
                reasoning_output_tokens: (total_token_usage.reasoning_output_tokens
                    - token_usage_at_turn_start.reasoning_output_tokens)
                    .max(0),
                total_tokens: (total_token_usage.total_tokens
                    - token_usage_at_turn_start.total_tokens)
                    .max(0),
            };
            let current_span = Span::current();
            current_span.record(
                "codex.turn.token_usage.input_tokens",
                turn_token_usage.input_tokens,
            );
            current_span.record(
                "codex.turn.token_usage.cached_input_tokens",
                turn_token_usage.cached_input(),
            );
            current_span.record(
                "codex.turn.token_usage.non_cached_input_tokens",
                turn_token_usage.non_cached_input(),
            );
            current_span.record(
                "codex.turn.token_usage.output_tokens",
                turn_token_usage.output_tokens,
            );
            current_span.record(
                "codex.turn.token_usage.reasoning_output_tokens",
                turn_token_usage.reasoning_output_tokens,
            );
            current_span.record(
                "codex.turn.token_usage.total_tokens",
                turn_token_usage.total_tokens,
            );
            self.services
                .analytics_events_client
                .track_turn_token_usage(TurnTokenUsageFact {
                    turn_id: turn_context.sub_id.clone(),
                    thread_id: self.thread_id.to_string(),
                    token_usage: turn_token_usage.clone(),
                });
            self.services.session_telemetry.histogram(
                TURN_TOKEN_USAGE_METRIC,
                turn_token_usage.total_tokens,
                &[("token_type", "total"), tmp_mem],
            );
            self.services.session_telemetry.histogram(
                TURN_TOKEN_USAGE_METRIC,
                turn_token_usage.input_tokens,
                &[("token_type", "input"), tmp_mem],
            );
            self.services.session_telemetry.histogram(
                TURN_TOKEN_USAGE_METRIC,
                turn_token_usage.cached_input(),
                &[("token_type", "cached_input"), tmp_mem],
            );
            self.services.session_telemetry.histogram(
                TURN_TOKEN_USAGE_METRIC,
                turn_token_usage.output_tokens,
                &[("token_type", "output"), tmp_mem],
            );
            self.services.session_telemetry.histogram(
                TURN_TOKEN_USAGE_METRIC,
                turn_token_usage.reasoning_output_tokens,
                &[("token_type", "reasoning_output"), tmp_mem],
            );
        }
        emit_turn_memory_metric(
            &self.services.session_telemetry,
            turn_context.config.features.enabled(Feature::MemoryTool),
            turn_context.config.memories.use_memories,
            turn_had_memory_citation,
        );
        let (completed_at, duration_ms) = turn_context
            .turn_timing_state
            .completed_at_and_duration_ms()
            .await;
        self.services
            .analytics_events_client
            .track_turn_profile(TurnProfileFact {
                turn_id: turn_context.sub_id.clone(),
                profile: turn_context.turn_timing_state.complete_profile(),
            });
        let event = if let Some(reason) = abort_reason {
            self.emit_turn_abort_lifecycle(reason.clone(), turn_context.extension_data.as_ref())
                .await;
            EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some(turn_context.sub_id.clone()),
                reason,
                completed_at,
                duration_ms,
            })
        } else {
            let time_to_first_token_ms = turn_context
                .turn_timing_state
                .time_to_first_token_ms()
                .await;
            self.emit_turn_stop_lifecycle(turn_context.extension_data.as_ref())
                .await;
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: turn_context.sub_id.clone(),
                last_agent_message,
                completed_at,
                duration_ms,
                time_to_first_token_ms,
            })
        };
        let mut terminal_guard = {
            let mut active = self.active_turn.lock().await;
            let Some(active_turn) = active.as_mut() else {
                return false;
            };
            let Some(task) = active_turn.task.take() else {
                return false;
            };
            if task.turn_context.sub_id != turn_context.sub_id {
                active_turn.task = Some(task);
                return false;
            }
            if terminal_interrupted {
                self.suppress_correction_auto_start().await;
            }
            let terminal_done = Arc::clone(&task.terminal_done);
            task.handle.detach();
            TerminalPublicationGuard::new(terminal_done)
        };

        self.send_event(turn_context.as_ref(), event).await;
        self.services
            .guardian_rejection_circuit_breaker
            .lock()
            .await
            .clear_turn(&turn_context.sub_id);

        // Regular items were flushed before this terminal event was appended; buffering
        // thread writers may not flush it without another explicit barrier.
        if let Err(err) = self.flush_rollout().await {
            warn!("failed to flush rollout after emitting terminal turn event: {err}");
        }
        let should_schedule_correction = task_succeeded
            && (task_kind != TaskKind::Regular
                || (!correction_bootstrap && !normal_sampling_attempted)
                || completed_normal_sampling);
        let correction_reservation = self
            .publish_turn_terminal_and_reserve_correction(
                terminal_guard.done(),
                should_schedule_correction,
            )
            .await;
        terminal_guard.mark_published();
        if let Some(reservation) = correction_reservation {
            let turn_context = self.new_default_turn().await;
            self.maybe_emit_model_warnings_for_turn(turn_context.as_ref())
                .await;
            assert!(
                self.start_reserved_task(
                    reservation,
                    turn_context,
                    vec![TurnInput::CommittedCorrection],
                    RegularTask::correction_bootstrap(),
                )
                .await,
                "terminal-to-correction reservation must remain owned until task publication"
            );
        }
        true
    }

    pub(crate) async fn close_unified_exec_processes(&self) {
        self.services
            .unified_exec_manager
            .terminate_all_processes()
            .await;
    }

    pub(crate) async fn list_background_terminals(&self) -> Vec<BackgroundTerminalInfo> {
        self.services.unified_exec_manager.list_processes().await
    }

    pub(crate) async fn terminate_background_terminal(&self, process_id: i32) -> bool {
        self.services
            .unified_exec_manager
            .terminate_process(process_id)
            .await
    }

    async fn handle_task_abort(self: &Arc<Self>, task: RunningTask, reason: TurnAbortReason) {
        let sub_id = task.turn_context.sub_id.clone();
        if task.cancellation_token.is_cancelled() {
            return;
        }

        trace!(task_kind = ?task.kind, sub_id, "aborting running task");
        task.cancellation_token.cancel();
        task.turn_context
            .turn_metadata_state
            .cancel_git_enrichment_task();
        let session_task = task.task;

        select! {
            _ = task.done.notified() => {
            },
            _ = tokio::time::sleep(Duration::from_millis(GRACEFULL_INTERRUPTION_TIMEOUT_MS)) => {
                warn!("task {sub_id} didn't complete gracefully after {}ms", GRACEFULL_INTERRUPTION_TIMEOUT_MS);
            }
        }

        task.handle.abort();

        let session_ctx = Arc::new(SessionTaskContext::new(
            Arc::clone(self),
            Arc::clone(&task.turn_extension_data),
        ));
        session_task
            .abort(session_ctx, Arc::clone(&task.turn_context))
            .await;

        if reason == TurnAbortReason::Interrupted
            && let Some(marker) = interrupted_turn_history_marker(
                InterruptedTurnHistoryMarker::from_config_and_version(
                    task.turn_context.config.as_ref(),
                    task.turn_context.multi_agent_version,
                ),
            )
        {
            self.record_conversation_items(
                task.turn_context.as_ref(),
                std::slice::from_ref(&marker),
            )
            .await;
            // Ensure the marker is durably visible before emitting TurnAborted: some clients
            // synchronously re-read the rollout on receipt of the abort event.
            if let Err(err) = self.flush_rollout().await {
                warn!("failed to flush interrupted-turn marker before emitting TurnAborted: {err}");
            }
        }

        let (completed_at, duration_ms) = task
            .turn_context
            .turn_timing_state
            .completed_at_and_duration_ms()
            .await;
        self.services
            .analytics_events_client
            .track_turn_profile(TurnProfileFact {
                turn_id: task.turn_context.sub_id.clone(),
                profile: task.turn_context.turn_timing_state.complete_profile(),
            });
        let event = EventMsg::TurnAborted(TurnAbortedEvent {
            turn_id: Some(task.turn_context.sub_id.clone()),
            reason,
            completed_at,
            duration_ms,
        });
        self.send_event(task.turn_context.as_ref(), event).await;
        self.services
            .guardian_rejection_circuit_breaker
            .lock()
            .await
            .clear_turn(&task.turn_context.sub_id);
        // Regular items were flushed before this terminal event was appended; buffering
        // thread writers may not flush it without another explicit barrier.
        if let Err(err) = self.flush_rollout().await {
            warn!("failed to flush rollout after emitting terminal turn event: {err}");
        }
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

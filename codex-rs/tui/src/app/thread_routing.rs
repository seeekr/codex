//! Thread routing, buffering, and app-server operation submission for the TUI app.
//!
//! This module manages active thread channels, routes server requests and notifications into those
//! channels, submits thread-scoped operations through the app server, and replays buffered events
//! when the visible thread changes.

use super::*;
use crate::session_resume::read_session_model;

const JSONRPC_INVALID_REQUEST: i64 = -32600;
const JSONRPC_INVALID_PARAMS: i64 = -32602;
const COMPOSER_CORRECTION_REQUEST_TIMEOUT: Duration = Duration::from_millis(1_500);
const COMPOSER_SUBMISSION_REQUEST_TIMEOUT: Duration = Duration::from_millis(1_500);

fn surviving_composer_submission_ids(thread: &Thread) -> (HashSet<String>, HashSet<Uuid>) {
    let client_ids = thread
        .turns
        .iter()
        .flat_map(|turn| &turn.items)
        .filter_map(|item| match item {
            ThreadItem::UserMessage {
                client_id: Some(client_id),
                ..
            } if client_id.starts_with("koenig-composer-") => Some(client_id.clone()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let submission_ids = client_ids
        .iter()
        .filter_map(|client_id| {
            client_id
                .strip_prefix("koenig-composer-")
                .and_then(|id| Uuid::parse_str(id).ok())
        })
        .collect();
    (client_ids, submission_ids)
}

fn correction_server_error_outcome(code: i64) -> CorrectionDispatchOutcome {
    if matches!(code, JSONRPC_INVALID_REQUEST | JSONRPC_INVALID_PARAMS) {
        CorrectionDispatchOutcome::NotApplied
    } else {
        CorrectionDispatchOutcome::Unknown
    }
}

#[derive(Clone, Copy)]
pub(super) enum ThreadRollbackOrigin {
    Backtrack,
    SafetyBufferingRetry,
}

impl App {
    pub(super) fn finish_composer_tui_event(
        &mut self,
        state: &mut crate::composer_control::NativeComposerControlState,
    ) {
        state.finish_tui_event(&self.chat_widget);
        self.resolve_notified_composer_submissions_for_active_thread();
    }

    pub(super) fn composer_correction_thread_id(
        &self,
        pending: &PendingComposerCorrection,
    ) -> std::result::Result<ThreadId, CorrectionDispatchOutcome> {
        let Ok(thread_id) = ThreadId::from_string(pending.thread_id()) else {
            return Err(CorrectionDispatchOutcome::NotApplied);
        };
        Ok(thread_id)
    }

    pub(super) async fn dispatch_native_composer_submit(
        &mut self,
        app_server: &mut AppServerSession,
        prepared: &PreparedNativeComposerSubmit,
    ) -> SubmissionDispatchOutcome {
        let submission = prepared.submission();
        let Ok(thread_id) = ThreadId::from_string(submission.thread_id()) else {
            return SubmissionDispatchOutcome::NotApplied(ErrorCode::ComposerUnavailable);
        };
        if self.active_thread_id != Some(thread_id)
            || self.chat_widget.thread_id() != Some(thread_id)
        {
            return SubmissionDispatchOutcome::NotApplied(ErrorCode::ComposerUnavailable);
        }

        let client_id = submission.client_user_message_id();
        self.pending_composer_submissions.insert(
            client_id.clone(),
            super::PendingComposerSubmissionCommit {
                submission: submission.clone(),
                turn_id: None,
                external_commit: Some(prepared.commit()),
                item_committed: false,
            },
        );
        let dispatch =
            self.dispatch_native_composer_submit_unbounded(app_server, thread_id, prepared.op());
        let accepted_turn_id =
            match tokio::time::timeout(COMPOSER_SUBMISSION_REQUEST_TIMEOUT, dispatch).await {
                Ok(Ok(turn_id)) => turn_id,
                Ok(Err(outcome)) => {
                    if matches!(outcome, SubmissionDispatchOutcome::NotApplied(_)) {
                        self.pending_composer_submissions.remove(&client_id);
                    }
                    return outcome;
                }
                Err(_) => return SubmissionDispatchOutcome::Unknown,
            };

        let committed = self
            .pending_composer_submissions
            .get(&client_id)
            .and_then(|pending| pending.external_commit.as_ref().cloned())
            .is_some_and(|commit| self.chat_widget.commit_native_composer_submit(&commit));
        if let Some(pending) = self.pending_composer_submissions.get_mut(&client_id) {
            pending.turn_id = Some(accepted_turn_id);
            if committed {
                pending.external_commit = None;
            }
        }
        if committed {
            SubmissionDispatchOutcome::Accepted
        } else {
            self.chat_widget.add_error_message(
                "Koenig send was accepted, but the exact draft could not be cleared. Automatic resubmission is disabled."
                    .to_string(),
            );
            SubmissionDispatchOutcome::AcceptedButUncommitted
        }
    }

    async fn dispatch_native_composer_submit_unbounded(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        op: &AppCommand,
    ) -> std::result::Result<String, SubmissionDispatchOutcome> {
        let AppCommand::UserTurn {
            items,
            cwd,
            approval_policy,
            approvals_reviewer,
            active_permission_profile,
            model,
            effort,
            summary,
            service_tier,
            final_output_json_schema,
            collaboration_mode,
            personality,
            composer_submission: Some(composer_submission),
        } = op
        else {
            return Err(SubmissionDispatchOutcome::NotApplied(
                ErrorCode::SubmissionUnavailable,
            ));
        };
        let client_user_message_id = Some(composer_submission.client_user_message_id());
        let composer_receipt = Some(composer_submission.receipt_context());
        if let Some(turn_id) = self.active_turn_id_for_thread(thread_id).await {
            let mut steer_turn_id = turn_id;
            let mut retried_after_turn_mismatch = false;
            loop {
                match app_server
                    .turn_steer(
                        thread_id,
                        steer_turn_id.clone(),
                        client_user_message_id.clone(),
                        composer_receipt.clone(),
                        items.to_vec(),
                    )
                    .await
                {
                    Ok(response) => return Ok(response.turn_id),
                    Err(error) => {
                        if active_turn_not_steerable_turn_error(&error).is_some() {
                            return Err(SubmissionDispatchOutcome::NotApplied(
                                ErrorCode::SubmissionUnavailable,
                            ));
                        }
                        match active_turn_steer_race(&error) {
                            Some(ActiveTurnSteerRace::Missing) => {
                                if let Some(channel) = self.thread_event_channels.get(&thread_id) {
                                    let mut store = channel.store.lock().await;
                                    store.clear_active_turn_id();
                                }
                                break;
                            }
                            Some(ActiveTurnSteerRace::ExpectedTurnMismatch { actual_turn_id })
                                if !retried_after_turn_mismatch
                                    && actual_turn_id != steer_turn_id =>
                            {
                                if let Some(channel) = self.thread_event_channels.get(&thread_id) {
                                    let mut store = channel.store.lock().await;
                                    store.active_turn_id = Some(actual_turn_id.clone());
                                }
                                steer_turn_id = actual_turn_id;
                                retried_after_turn_mismatch = true;
                            }
                            Some(ActiveTurnSteerRace::ExpectedTurnMismatch { actual_turn_id }) => {
                                if let Some(channel) = self.thread_event_channels.get(&thread_id) {
                                    let mut store = channel.store.lock().await;
                                    store.active_turn_id = Some(actual_turn_id);
                                }
                                return Err(SubmissionDispatchOutcome::NotApplied(
                                    ErrorCode::SubmissionUnavailable,
                                ));
                            }
                            None => return Err(submission_typed_error_outcome(&error)),
                        }
                    }
                }
            }
        }
        let config = self.chat_widget.config_ref();
        let approvals_reviewer = approvals_reviewer.unwrap_or(config.approvals_reviewer);
        let permissions_override = Self::turn_permissions_override_from_config(
            config,
            active_permission_profile.as_ref(),
            self.runtime_permission_profile_override
                .as_ref()
                .map(|profile| &profile.permission_profile),
        );
        match app_server
            .turn_start(
                thread_id,
                client_user_message_id,
                composer_receipt,
                items.to_vec(),
                cwd.clone(),
                *approval_policy,
                approvals_reviewer,
                permissions_override,
                config.permissions.user_visible_workspace_roots(),
                model.to_string(),
                effort.clone(),
                *summary,
                service_tier.clone(),
                collaboration_mode.clone(),
                *personality,
                final_output_json_schema.clone(),
            )
            .await
        {
            Ok(response) => {
                if self.active_thread_id == Some(thread_id)
                    && self.chat_widget.thread_id() == Some(thread_id)
                {
                    self.chat_widget
                        .record_safety_buffering_turn(response.turn.id.clone(), op);
                }
                Ok(response.turn.id)
            }
            Err(error) => Err(error
                .downcast_ref::<TypedRequestError>()
                .map(submission_typed_error_outcome)
                .unwrap_or(SubmissionDispatchOutcome::Unknown)),
        }
    }
}

fn submission_typed_error_outcome(error: &TypedRequestError) -> SubmissionDispatchOutcome {
    match error {
        TypedRequestError::Server { .. } => {
            SubmissionDispatchOutcome::NotApplied(ErrorCode::SubmissionUnavailable)
        }
        TypedRequestError::Transport { .. } | TypedRequestError::Deserialize { .. } => {
            SubmissionDispatchOutcome::Unknown
        }
    }
}

async fn dispatch_composer_correction_unbounded(
    request_handle: &AppServerRequestHandle,
    thread_id: ThreadId,
    pending: &PendingComposerCorrection,
) -> CorrectionDispatchOutcome {
    for retried_after_transport in [false, true] {
        let response: std::result::Result<ThreadCorrectionCommitResponse, TypedRequestError> =
            request_handle
                .request_typed(ClientRequest::ThreadCorrectionCommit {
                    request_id: RequestId::String(format!(
                        "composer-correction-{}",
                        Uuid::new_v4()
                    )),
                    params: ThreadCorrectionCommitParams {
                        thread_id: thread_id.to_string(),
                        correction_id: pending.correction_id().to_string(),
                        expected_client_user_message_id: pending
                            .expected_client_user_message_id()
                            .to_string(),
                        payload: pending.payload().to_string(),
                    },
                })
                .await;
        match response {
            Ok(_) => return CorrectionDispatchOutcome::Accepted,
            Err(TypedRequestError::Server { source, .. }) => {
                return correction_server_error_outcome(source.code);
            }
            Err(TypedRequestError::Transport { .. } | TypedRequestError::Deserialize { .. })
                if !retried_after_transport => {}
            Err(TypedRequestError::Transport { .. } | TypedRequestError::Deserialize { .. }) => {
                return CorrectionDispatchOutcome::Unknown;
            }
        }
    }
    unreachable!("correction commit transport retry loop should return")
}

async fn correction_dispatch_with_timeout(
    dispatch: impl std::future::Future<Output = CorrectionDispatchOutcome>,
    timeout: Duration,
) -> CorrectionDispatchOutcome {
    tokio::time::timeout(timeout, dispatch)
        .await
        .unwrap_or(CorrectionDispatchOutcome::Unknown)
}

pub(super) async fn dispatch_composer_correction(
    request_handle: AppServerRequestHandle,
    thread_id: ThreadId,
    pending: &PendingComposerCorrection,
) -> CorrectionDispatchOutcome {
    correction_dispatch_with_timeout(
        dispatch_composer_correction_unbounded(&request_handle, thread_id, pending),
        COMPOSER_CORRECTION_REQUEST_TIMEOUT,
    )
    .await
}

#[cfg(test)]
mod correction_dispatch_tests {
    use super::*;

    #[tokio::test]
    async fn correction_dispatch_expiry_is_unknown() {
        let outcome = correction_dispatch_with_timeout(
            std::future::pending::<CorrectionDispatchOutcome>(),
            Duration::ZERO,
        )
        .await;
        assert_eq!(outcome, CorrectionDispatchOutcome::Unknown);
    }
}

impl App {
    pub(super) async fn shutdown_current_thread(&mut self, app_server: &mut AppServerSession) {
        if let Some(thread_id) = self.chat_widget.thread_id() {
            // Clear any in-flight rollback guard when switching threads.
            self.backtrack.pending_rollback = None;
            if let Err(err) = app_server.thread_unsubscribe(thread_id).await {
                tracing::warn!("failed to unsubscribe thread {thread_id}: {err}");
            }
            self.abort_thread_event_listener(thread_id);
        }
    }

    pub(super) fn abort_thread_event_listener(&mut self, thread_id: ThreadId) {
        if let Some(handle) = self.thread_event_listener_tasks.remove(&thread_id) {
            handle.abort();
        }
    }

    pub(super) fn abort_all_thread_event_listeners(&mut self) {
        for handle in self
            .thread_event_listener_tasks
            .drain()
            .map(|(_, handle)| handle)
        {
            handle.abort();
        }
    }

    pub(super) fn ensure_thread_channel(&mut self, thread_id: ThreadId) -> &mut ThreadEventChannel {
        self.thread_event_channels
            .entry(thread_id)
            .or_insert_with(|| ThreadEventChannel::new(THREAD_EVENT_CHANNEL_CAPACITY))
    }

    pub(super) async fn set_thread_active(&mut self, thread_id: ThreadId, active: bool) {
        if let Some(channel) = self.thread_event_channels.get_mut(&thread_id) {
            let mut store = channel.store.lock().await;
            store.active = active;
        }
    }

    pub(super) async fn activate_thread_channel(&mut self, thread_id: ThreadId) {
        if self.active_thread_id.is_some() {
            return;
        }
        self.set_thread_active(thread_id, /*active*/ true).await;
        let receiver = if let Some(channel) = self.thread_event_channels.get_mut(&thread_id) {
            channel.receiver.take()
        } else {
            None
        };
        self.active_thread_id = Some(thread_id);
        self.active_thread_rx = receiver;
        self.refresh_pending_thread_approvals().await;
    }

    pub(super) async fn store_active_thread_receiver(&mut self) {
        let Some(active_id) = self.active_thread_id else {
            return;
        };
        let input_state = self.chat_widget.capture_thread_input_state();
        if let Some(channel) = self.thread_event_channels.get_mut(&active_id) {
            let receiver = self.active_thread_rx.take();
            let mut store = channel.store.lock().await;
            store.active = false;
            store.input_state = input_state;
            if let Some(receiver) = receiver {
                channel.receiver = Some(receiver);
            }
        }
    }

    pub(super) async fn activate_thread_for_replay(
        &mut self,
        thread_id: ThreadId,
    ) -> Option<(mpsc::Receiver<ThreadBufferedEvent>, ThreadEventSnapshot)> {
        let channel = self.thread_event_channels.get_mut(&thread_id)?;
        let receiver = channel.receiver.take()?;
        let mut store = channel.store.lock().await;
        store.active = true;
        let snapshot = store.snapshot();
        Some((receiver, snapshot))
    }

    pub(super) async fn clear_active_thread(&mut self) {
        if let Some(active_id) = self.active_thread_id.take() {
            self.set_thread_active(active_id, /*active*/ false).await;
        }
        self.active_thread_rx = None;
        self.refresh_pending_thread_approvals().await;
    }

    pub(super) async fn note_thread_outbound_op(&mut self, thread_id: ThreadId, op: &AppCommand) {
        let Some(channel) = self.thread_event_channels.get(&thread_id) else {
            return;
        };
        let mut store = channel.store.lock().await;
        store.note_outbound_op(op);
    }

    pub(super) async fn note_active_thread_outbound_op(&mut self, op: &AppCommand) {
        if !ThreadEventStore::op_can_change_pending_replay_state(op) {
            return;
        }
        let Some(thread_id) = self.active_thread_id else {
            return;
        };
        self.note_thread_outbound_op(thread_id, op).await;
    }

    pub(super) async fn active_turn_id_for_thread(&self, thread_id: ThreadId) -> Option<String> {
        let channel = self.thread_event_channels.get(&thread_id)?;
        let store = channel.store.lock().await;
        store.active_turn_id().map(ToOwned::to_owned)
    }

    pub(super) fn thread_label(&self, thread_id: ThreadId) -> String {
        let is_primary = self.primary_thread_id == Some(thread_id);
        let fallback_label = if is_primary {
            "Main [default]".to_string()
        } else {
            let thread_id = thread_id.to_string();
            let short_id: String = thread_id.chars().take(8).collect();
            format!("Agent ({short_id})")
        };
        if let Some(entry) = self.agent_navigation.get(&thread_id) {
            let label = format_agent_picker_item_name(
                entry.agent_nickname.as_deref(),
                entry.agent_role.as_deref(),
                is_primary,
            );
            if label == "Agent" {
                let thread_id = thread_id.to_string();
                let short_id: String = thread_id.chars().take(8).collect();
                format!("{label} ({short_id})")
            } else {
                label
            }
        } else {
            fallback_label
        }
    }

    /// Returns the thread whose transcript is currently on screen.
    ///
    /// `active_thread_id` is the source of truth during steady state, but the widget can briefly
    /// lag behind thread bookkeeping during transitions. The footer label and adjacent-thread
    /// navigation both follow what the user is actually looking at, not whichever thread most
    /// recently began switching.
    pub(super) fn current_displayed_thread_id(&self) -> Option<ThreadId> {
        self.active_thread_id.or(self.chat_widget.thread_id())
    }

    pub(super) fn ignore_same_thread_resume(
        &mut self,
        target_session: &crate::resume_picker::SessionTarget,
    ) -> bool {
        if self.active_thread_id != Some(target_session.thread_id) {
            return false;
        };

        self.chat_widget.add_info_message(
            format!("Already viewing {}.", target_session.display_label()),
            /*hint*/ None,
        );
        true
    }

    /// Mirrors the visible thread into the contextual footer row.
    ///
    /// The footer sometimes shows ambient context instead of an instructional hint. In multi-agent
    /// sessions, that contextual row includes the currently viewed agent label. The label is
    /// intentionally hidden until there is more than one known thread so single-thread sessions do
    /// not spend footer space restating that the user is already on the main conversation.
    pub(super) fn sync_active_agent_label(&mut self) {
        let label = self
            .agent_navigation
            .active_agent_label(self.current_displayed_thread_id(), self.primary_thread_id);
        self.chat_widget.set_active_agent_label(label);
        self.sync_side_thread_ui();
    }

    pub(super) async fn thread_cwd(&self, thread_id: ThreadId) -> Option<AbsolutePathBuf> {
        let channel = self.thread_event_channels.get(&thread_id)?;
        let store = channel.store.lock().await;
        store.session.as_ref().map(|session| session.cwd.clone())
    }

    async fn thread_file_change_changes(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
        item_id: &str,
    ) -> Option<Vec<codex_app_server_protocol::FileUpdateChange>> {
        let channel = self.thread_event_channels.get(&thread_id)?;
        let store = channel.store.lock().await;
        store.file_change_changes(turn_id, item_id)
    }

    pub(super) async fn interactive_request_for_thread_request(
        &self,
        thread_id: ThreadId,
        request: &ServerRequest,
    ) -> std::io::Result<Option<ThreadInteractiveRequest>> {
        let thread_label = Some(self.thread_label(thread_id));
        Ok(match request {
            ServerRequest::CommandExecutionRequestApproval { params, .. } => {
                let network_approval_context = params.network_approval_context.clone();
                let additional_permissions = params.additional_permissions.clone();
                let proposed_execpolicy_amendment = params.proposed_execpolicy_amendment.clone();
                let proposed_network_policy_amendments =
                    params.proposed_network_policy_amendments.clone();
                Some(ThreadInteractiveRequest::Approval(ApprovalRequest::Exec {
                    thread_id,
                    thread_label,
                    id: params
                        .approval_id
                        .clone()
                        .unwrap_or_else(|| params.item_id.clone()),
                    environment_id: params.environment_id.clone(),
                    command: params
                        .command
                        .as_deref()
                        .map(split_command_string)
                        .unwrap_or_default(),
                    reason: params.reason.clone(),
                    available_decisions: params.available_decisions.clone().unwrap_or_else(|| {
                        default_exec_approval_decisions(
                            network_approval_context.as_ref(),
                            proposed_execpolicy_amendment.as_ref(),
                            proposed_network_policy_amendments.as_deref(),
                            additional_permissions.as_ref(),
                        )
                    }),
                    network_approval_context,
                    additional_permissions,
                }))
            }
            ServerRequest::FileChangeRequestApproval { params, .. } => Some(
                ThreadInteractiveRequest::Approval(ApprovalRequest::ApplyPatch {
                    thread_id,
                    thread_label,
                    id: params.item_id.clone(),
                    reason: params.reason.clone(),
                    cwd: self
                        .thread_cwd(thread_id)
                        .await
                        .unwrap_or_else(|| self.config.cwd.clone()),
                    changes: self
                        .thread_file_change_changes(thread_id, &params.turn_id, &params.item_id)
                        .await
                        .map(crate::app_server_approval_conversions::file_update_changes_to_display)
                        .unwrap_or_default(),
                }),
            ),
            ServerRequest::McpServerElicitationRequest { request_id, params } => {
                if let Some(params) = AppLinkViewParams::from_url_app_server_request(
                    thread_id,
                    &params.server_name,
                    request_id.clone(),
                    &params.request,
                ) {
                    Some(ThreadInteractiveRequest::AppLink(params))
                } else if let Some(request) =
                    McpServerElicitationFormRequest::from_app_server_request(
                        thread_id,
                        request_id.clone(),
                        params.clone(),
                    )
                {
                    Some(ThreadInteractiveRequest::McpServerElicitation(request))
                } else {
                    match &params.request {
                        codex_app_server_protocol::McpServerElicitationRequest::Form {
                            message,
                            ..
                        } => Some(ThreadInteractiveRequest::Approval(
                            ApprovalRequest::McpElicitation {
                                thread_id,
                                thread_label,
                                server_name: params.server_name.clone(),
                                request_id: request_id.clone(),
                                message: message.clone(),
                            },
                        )),
                        codex_app_server_protocol::McpServerElicitationRequest::OpenAiForm {
                            ..
                        }
                        | codex_app_server_protocol::McpServerElicitationRequest::Url { .. } => {
                            self.app_event_tx.resolve_elicitation(
                                thread_id,
                                params.server_name.clone(),
                                request_id.clone(),
                                codex_app_server_protocol::McpServerElicitationAction::Decline,
                                /*content*/ None,
                                /*meta*/ None,
                            );
                            None
                        }
                    }
                }
            }
            ServerRequest::PermissionsRequestApproval { params, .. } => {
                // TODO(anp): Remove this native-path localization error path once core permission
                // paths remain PathUri after crossing the app-server boundary.
                let permissions = params.permissions.clone().try_into().map_err(|err| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("failed to localize requested filesystem paths: {err}"),
                    )
                })?;
                Some(ThreadInteractiveRequest::Approval(
                    ApprovalRequest::Permissions {
                        thread_id,
                        thread_label,
                        call_id: params.item_id.clone(),
                        environment_id: params.environment_id.clone(),
                        reason: params.reason.clone(),
                        permissions,
                    },
                ))
            }
            _ => None,
        })
    }

    pub(super) fn push_thread_interactive_request(&mut self, request: ThreadInteractiveRequest) {
        match request {
            ThreadInteractiveRequest::AppLink(params) => {
                self.chat_widget.open_app_link_view(params);
            }
            ThreadInteractiveRequest::Approval(request) => {
                self.render_inactive_patch_preview(&request);
                self.chat_widget.push_approval_request(request);
            }
            ThreadInteractiveRequest::McpServerElicitation(request) => {
                self.chat_widget
                    .push_mcp_server_elicitation_request(request);
            }
        }
    }

    fn render_inactive_patch_preview(&mut self, request: &ApprovalRequest) {
        let ApprovalRequest::ApplyPatch {
            thread_label,
            cwd,
            changes,
            ..
        } = request
        else {
            return;
        };
        if thread_label.is_none() || changes.is_empty() {
            return;
        }
        self.chat_widget
            .add_to_history(history_cell::new_patch_event(changes.clone(), cwd));
    }

    pub(super) async fn pending_inactive_thread_requests(&self) -> Vec<(ThreadId, ServerRequest)> {
        let channels: Vec<(ThreadId, Arc<Mutex<ThreadEventStore>>)> = self
            .thread_event_channels
            .iter()
            .map(|(thread_id, channel)| (*thread_id, Arc::clone(&channel.store)))
            .collect();

        let mut requests = Vec::new();
        for (thread_id, store) in channels {
            if Some(thread_id) == self.active_thread_id {
                continue;
            }

            let store = store.lock().await;
            requests.extend(
                store
                    .pending_replay_requests()
                    .into_iter()
                    .map(|request| (thread_id, request)),
            );
        }
        requests
    }

    pub(super) async fn surface_pending_inactive_thread_interactive_requests(
        &mut self,
    ) -> Result<()> {
        if self.active_side_parent_thread_id().is_some() {
            return Ok(());
        }

        let requests = self.pending_inactive_thread_requests().await;
        for (thread_id, request) in requests {
            if let Some(request) = self
                .interactive_request_for_thread_request(thread_id, &request)
                .await?
            {
                self.push_thread_interactive_request(request);
            }
        }
        Ok(())
    }

    pub(super) async fn submit_active_thread_op(
        &mut self,
        app_server: &mut AppServerSession,
        op: AppCommand,
    ) -> Result<()> {
        let Some(thread_id) = self.active_thread_id else {
            self.chat_widget
                .add_error_message("No active thread is available.".to_string());
            return Ok(());
        };

        self.submit_thread_op(app_server, thread_id, op).await
    }

    pub(super) async fn submit_thread_op(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        op: AppCommand,
    ) -> Result<()> {
        crate::session_log::log_outbound_op(&op);

        if self
            .try_resolve_app_server_request(app_server, thread_id, &op)
            .await?
        {
            return Ok(());
        }

        if self
            .try_submit_active_thread_op_via_app_server(app_server, thread_id, &op)
            .await?
        {
            if ThreadEventStore::op_can_change_pending_replay_state(&op) {
                self.note_thread_outbound_op(thread_id, &op).await;
                self.refresh_pending_thread_approvals().await;
                self.refresh_side_parent_status_from_store(thread_id).await;
            }
            return Ok(());
        }

        self.chat_widget
            .add_error_message(format!("Not available in TUI yet for thread {thread_id}."));
        Ok(())
    }

    /// Persist prompt text in the local cross-session message history.
    pub(super) fn append_message_history_entry(&self, thread_id: ThreadId, text: String) {
        let history_config = codex_message_history::HistoryConfig::new(
            self.chat_widget.config_ref().codex_home.clone(),
            &self.chat_widget.config_ref().history,
        );
        tokio::spawn(async move {
            if let Err(err) =
                codex_message_history::append_entry(&text, thread_id, &history_config).await
            {
                tracing::warn!(
                    thread_id = %thread_id,
                    error = %err,
                    "failed to append to message history"
                );
            }
        });
    }

    /// Fetch one local cross-session message history entry for the requesting thread.
    pub(super) async fn lookup_message_history_entry(
        &mut self,
        thread_id: ThreadId,
        offset: usize,
        log_id: u64,
    ) -> Result<()> {
        let history_config = codex_message_history::HistoryConfig::new(
            self.chat_widget.config_ref().codex_home.clone(),
            &self.chat_widget.config_ref().history,
        );
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let entry_opt = tokio::task::spawn_blocking(move || {
                codex_message_history::lookup(log_id, offset, &history_config)
            })
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(error = %err, "history lookup task failed");
                None
            });

            app_event_tx.send(AppEvent::ThreadHistoryEntryResponse {
                thread_id,
                event: HistoryLookupResponse {
                    offset,
                    log_id,
                    entry: entry_opt.map(|entry| entry.text),
                },
            });
        });
        Ok(())
    }

    pub(super) async fn try_submit_active_thread_op_via_app_server(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        op: &AppCommand,
    ) -> Result<bool> {
        match op {
            AppCommand::Interrupt { .. } => {
                if let Some(turn_id) = self.active_turn_id_for_thread(thread_id).await {
                    let mut interrupt_turn_id = turn_id;
                    for retried_after_turn_mismatch in [false, true] {
                        match app_server
                            .turn_interrupt(thread_id, interrupt_turn_id.clone())
                            .await
                        {
                            Ok(()) => return Ok(true),
                            Err(error) if !retried_after_turn_mismatch => {
                                let Some(actual_turn_id) = active_turn_interrupt_race(&error)
                                else {
                                    return Err(error).wrap_err("turn/interrupt failed in TUI");
                                };
                                if actual_turn_id == interrupt_turn_id {
                                    return Err(error).wrap_err("turn/interrupt failed in TUI");
                                }
                                // Review flows can swap the active turn before the TUI processes
                                // the corresponding notification. Retry once with the
                                // server-reported turn id so Ctrl+C/Esc do not fatally exit on that
                                // stale cache, but let lifecycle notifications own the cached
                                // active turn id.
                                interrupt_turn_id = actual_turn_id;
                            }
                            Err(error) => {
                                return Err(error).wrap_err("turn/interrupt failed in TUI");
                            }
                        }
                    }
                    unreachable!("interrupt retry loop should return");
                } else {
                    app_server
                        .startup_interrupt(thread_id)
                        .await
                        .wrap_err("turn/interrupt failed in TUI")?;
                }
                Ok(true)
            }
            AppCommand::UserTurn {
                items,
                cwd,
                approval_policy,
                approvals_reviewer,
                active_permission_profile,
                model,
                effort,
                summary,
                service_tier,
                final_output_json_schema,
                collaboration_mode,
                personality,
                composer_submission,
            } => {
                let client_user_message_id = composer_submission
                    .as_ref()
                    .map(NativeComposerSubmission::client_user_message_id);
                let composer_receipt = composer_submission
                    .as_ref()
                    .map(NativeComposerSubmission::receipt_context);
                let mut should_start_turn = true;
                if let Some(turn_id) = self.active_turn_id_for_thread(thread_id).await {
                    let mut steer_turn_id = turn_id;
                    let mut retried_after_turn_mismatch = false;
                    loop {
                        match app_server
                            .turn_steer(
                                thread_id,
                                steer_turn_id.clone(),
                                client_user_message_id.clone(),
                                composer_receipt.clone(),
                                items.to_vec(),
                            )
                            .await
                        {
                            Ok(response) => {
                                if let Some(submission) = composer_submission {
                                    self.register_pending_composer_submission(
                                        submission.clone(),
                                        response.turn_id,
                                    );
                                }
                                return Ok(true);
                            }
                            Err(error) => {
                                if let Some(turn_error) =
                                    active_turn_not_steerable_turn_error(&error)
                                {
                                    if !self.chat_widget.enqueue_rejected_steer() {
                                        self.chat_widget.add_error_message(turn_error.message);
                                    }
                                    return Ok(true);
                                }
                                match active_turn_steer_race(&error) {
                                    Some(ActiveTurnSteerRace::Missing) => {
                                        if let Some(channel) =
                                            self.thread_event_channels.get(&thread_id)
                                        {
                                            let mut store = channel.store.lock().await;
                                            store.clear_active_turn_id();
                                        }
                                        should_start_turn = true;
                                        break;
                                    }
                                    Some(ActiveTurnSteerRace::ExpectedTurnMismatch {
                                        actual_turn_id,
                                    }) if !retried_after_turn_mismatch
                                        && actual_turn_id != steer_turn_id =>
                                    {
                                        // Review flows can swap the active turn before the TUI
                                        // processes the corresponding notification. Retry once with
                                        // the server-reported turn id so non-steerable review turns
                                        // still fall through to the existing queueing behavior.
                                        if let Some(channel) =
                                            self.thread_event_channels.get(&thread_id)
                                        {
                                            let mut store = channel.store.lock().await;
                                            store.active_turn_id = Some(actual_turn_id.clone());
                                        }
                                        steer_turn_id = actual_turn_id;
                                        retried_after_turn_mismatch = true;
                                    }
                                    Some(ActiveTurnSteerRace::ExpectedTurnMismatch {
                                        actual_turn_id,
                                    }) => {
                                        if let Some(channel) =
                                            self.thread_event_channels.get(&thread_id)
                                        {
                                            let mut store = channel.store.lock().await;
                                            store.active_turn_id = Some(actual_turn_id);
                                        }
                                        return Err(error.into());
                                    }
                                    None => return Err(error.into()),
                                }
                            }
                        }
                    }
                }
                if should_start_turn {
                    let config = self.chat_widget.config_ref();
                    let approvals_reviewer =
                        approvals_reviewer.unwrap_or(config.approvals_reviewer);
                    let permissions_override = Self::turn_permissions_override_from_config(
                        config,
                        active_permission_profile.as_ref(),
                        self.runtime_permission_profile_override
                            .as_ref()
                            .map(|profile| &profile.permission_profile),
                    );
                    let response = app_server
                        .turn_start(
                            thread_id,
                            client_user_message_id,
                            composer_receipt,
                            items.to_vec(),
                            cwd.clone(),
                            *approval_policy,
                            approvals_reviewer,
                            permissions_override,
                            config.permissions.user_visible_workspace_roots(),
                            model.to_string(),
                            effort.clone(),
                            *summary,
                            service_tier.clone(),
                            collaboration_mode.clone(),
                            *personality,
                            final_output_json_schema.clone(),
                        )
                        .await?;
                    if let Some(submission) = composer_submission {
                        self.register_pending_composer_submission(
                            submission.clone(),
                            response.turn.id.clone(),
                        );
                    }
                    if self.active_thread_id == Some(thread_id)
                        && self.chat_widget.thread_id() == Some(thread_id)
                    {
                        self.chat_widget
                            .record_safety_buffering_turn(response.turn.id, op);
                    }
                }
                Ok(true)
            }
            AppCommand::ListSkills { cwds, force_reload } => {
                self.handle_skills_list_result(
                    app_server
                        .skills_list(codex_app_server_protocol::SkillsListParams {
                            cwds: cwds.clone(),
                            force_reload: *force_reload,
                        })
                        .await,
                    "failed to refresh skills",
                );
                Ok(true)
            }
            AppCommand::Compact => {
                app_server.thread_compact_start(thread_id).await?;
                Ok(true)
            }
            AppCommand::SetThreadName { name } => {
                app_server
                    .thread_set_name(thread_id, name.to_string())
                    .await?;
                Ok(true)
            }
            AppCommand::ThreadRollback { num_turns } => {
                let response = match app_server.thread_rollback(thread_id, *num_turns).await {
                    Ok(response) => response,
                    Err(err) => {
                        self.handle_backtrack_rollback_failed();
                        return Err(err);
                    }
                };
                self.handle_thread_rollback_response(thread_id, *num_turns, &response)
                    .await;
                Ok(true)
            }
            AppCommand::Review { target } => {
                let response = app_server.review_start(thread_id, target.clone()).await?;
                let review_thread_id = ThreadId::from_string(&response.review_thread_id)
                    .wrap_err("review/start returned invalid review thread id")?;
                let store = Arc::clone(&self.ensure_thread_channel(review_thread_id).store);
                let mut store = store.lock().await;
                store.active_turn_id = Some(response.turn.id);
                Ok(true)
            }
            AppCommand::CleanBackgroundTerminals => {
                app_server
                    .thread_background_terminals_clean(thread_id)
                    .await?;
                Ok(true)
            }
            AppCommand::RunUserShellCommand { command } => {
                app_server
                    .thread_shell_command(thread_id, command.to_string())
                    .await?;
                Ok(true)
            }
            AppCommand::ReloadUserConfig => {
                app_server.reload_user_config().await?;
                Ok(true)
            }
            AppCommand::OverrideTurnContext { .. } => {
                self.sync_override_turn_context_settings(app_server, thread_id, op)
                    .await;
                Ok(true)
            }
            AppCommand::ApproveGuardianDeniedAction { event } => {
                app_server
                    .thread_approve_guardian_denied_action(thread_id, event)
                    .await?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn turn_permissions_override_from_config(
        config: &Config,
        active_permission_profile: Option<&ActivePermissionProfile>,
        runtime_permission_profile_override: Option<&PermissionProfile>,
    ) -> TurnPermissionsOverride {
        if let Some(active_permission_profile) = active_permission_profile {
            return TurnPermissionsOverride::ActiveProfile(active_permission_profile.clone());
        }

        let effective_permission_profile = config.permissions.effective_permission_profile();
        let runtime_permission_profile_override =
            runtime_permission_profile_override.map(|profile| {
                profile
                    .clone()
                    .materialize_project_roots_with_workspace_roots(
                        &config.effective_workspace_roots(),
                    )
            });
        if runtime_permission_profile_override
            .as_ref()
            .is_some_and(|profile| profile == &effective_permission_profile)
        {
            return TurnPermissionsOverride::LegacySandbox(effective_permission_profile);
        }

        TurnPermissionsOverride::Preserve
    }

    pub(super) fn handle_skills_list_result(
        &mut self,
        result: Result<SkillsListResponse>,
        failure_message: &str,
    ) {
        match result {
            Ok(response) => self.handle_skills_list_response(response),
            Err(err) => {
                tracing::warn!("{failure_message}: {err:#}");
                self.chat_widget
                    .add_error_message(format!("{failure_message}: {err:#}"));
            }
        }
    }

    pub(super) async fn try_resolve_app_server_request(
        &mut self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
        op: &AppCommand,
    ) -> Result<bool> {
        let Some(resolution) = self
            .pending_app_server_requests
            .take_resolution(op)
            .map_err(|err| color_eyre::eyre::eyre!(err))?
        else {
            return Ok(false);
        };

        match app_server
            .resolve_server_request(resolution.request_id, resolution.result)
            .await
        {
            Ok(()) => {
                if ThreadEventStore::op_can_change_pending_replay_state(op) {
                    self.note_thread_outbound_op(thread_id, op).await;
                    self.refresh_pending_thread_approvals().await;
                    self.refresh_side_parent_status_from_store(thread_id).await;
                }
                Ok(true)
            }
            Err(err) => {
                self.chat_widget.add_error_message(format!(
                    "Failed to resolve app-server request for thread {thread_id}: {err}"
                ));
                Ok(false)
            }
        }
    }

    pub(super) async fn refresh_pending_thread_approvals(&mut self) {
        let side_parent_thread_id = self.active_side_parent_thread_id();
        let channels: Vec<(ThreadId, Arc<Mutex<ThreadEventStore>>)> = self
            .thread_event_channels
            .iter()
            .map(|(thread_id, channel)| (*thread_id, Arc::clone(&channel.store)))
            .collect();

        let mut pending_thread_ids = Vec::new();
        for (thread_id, store) in channels {
            if Some(thread_id) == self.active_thread_id || Some(thread_id) == side_parent_thread_id
            {
                continue;
            }

            let store = store.lock().await;
            if store.has_pending_thread_approvals() {
                pending_thread_ids.push(thread_id);
            }
        }

        pending_thread_ids.sort_by_key(ThreadId::to_string);

        let threads = pending_thread_ids
            .into_iter()
            .map(|thread_id| self.thread_label(thread_id))
            .collect();

        self.chat_widget.set_pending_thread_approvals(threads);
    }

    pub(super) async fn refresh_side_parent_status_from_store(&mut self, thread_id: ThreadId) {
        let Some(channel) = self.thread_event_channels.get(&thread_id) else {
            return;
        };
        let status = {
            let store = channel.store.lock().await;
            store.side_parent_pending_status()
        };
        if let Some(status) = status {
            self.set_side_parent_status(thread_id, Some(status));
        } else {
            self.clear_side_parent_action_status(thread_id);
        }
    }

    pub(super) async fn enqueue_thread_notification(
        &mut self,
        thread_id: ThreadId,
        notification: ServerNotification,
    ) -> Result<()> {
        if matches!(notification, ServerNotification::ThreadSettingsUpdated(_))
            && self.primary_thread_id.is_some()
            && self.primary_thread_id != Some(thread_id)
            && !self.thread_event_channels.contains_key(&thread_id)
        {
            return Ok(());
        }
        self.note_composer_submission_notification(&notification);
        if let ServerNotification::ThreadSettingsUpdated(notification) = &notification {
            self.apply_thread_settings_to_cached_session(thread_id, &notification.thread_settings)
                .await;
        }
        let inferred_session = self
            .infer_session_for_thread_notification(thread_id, &notification)
            .await;
        let (sender, store) = {
            let channel = self.ensure_thread_channel(thread_id);
            (channel.sender.clone(), Arc::clone(&channel.store))
        };

        let (should_send, pending_status) = {
            let mut guard = store.lock().await;
            if guard.session.is_none()
                && let Some(session) = inferred_session
            {
                guard.session = Some(session);
            }
            guard.push_notification(notification.clone());
            (guard.active, guard.side_parent_pending_status())
        };
        let notification_status_change = SideParentStatusChange::for_notification(&notification);

        if should_send {
            match sender.try_send(ThreadBufferedEvent::Notification(notification)) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    tokio::spawn(async move {
                        if let Err(err) = sender.send(event).await {
                            tracing::warn!("thread {thread_id} event channel closed: {err}");
                        }
                    });
                }
                Err(TrySendError::Closed(_)) => {
                    tracing::warn!("thread {thread_id} event channel closed");
                }
            }
        }
        if let Some(status) = pending_status {
            self.set_side_parent_status(thread_id, Some(status));
        } else if let Some(change) = notification_status_change {
            self.apply_side_parent_status_change(thread_id, change);
        }
        self.refresh_pending_thread_approvals().await;
        Ok(())
    }

    /// Locally remembers receiver threads referenced by a collab notification.
    ///
    /// This intentionally avoids app-server reads on the active-thread rendering path. During large
    /// fan-outs the app-server can be saturated with spawn work, and blocking here would freeze the
    /// TUI event loop. Metadata from `ThreadStarted` or explicit picker refreshes still fills in
    /// names and roles later; until then, rendering falls back to the thread id.
    pub(super) fn cache_collab_receiver_threads_for_notification(
        &mut self,
        notification: &ServerNotification,
    ) {
        if let Some(activity) =
            sub_agent_activity_item(notification).and_then(sub_agent_activity_display)
        {
            self.agent_navigation.record_sub_agent_activity(activity);
            self.sync_active_agent_label();
            return;
        }

        let Some(receiver_thread_ids) = collab_receiver_thread_ids(notification) else {
            return;
        };

        for receiver_thread_id in receiver_thread_ids {
            if collab_receiver_is_not_found(notification, receiver_thread_id) {
                continue;
            }

            let Ok(thread_id) = ThreadId::from_string(receiver_thread_id) else {
                tracing::warn!(
                    thread_id = receiver_thread_id,
                    "ignoring collab receiver with invalid thread id during local caching"
                );
                continue;
            };

            if self.agent_navigation.get(&thread_id).is_some() {
                continue;
            }

            self.upsert_agent_picker_thread(
                thread_id, /*agent_nickname*/ None, /*agent_role*/ None,
                /*is_closed*/ false,
            );
        }
    }

    pub(super) async fn infer_session_for_thread_notification(
        &mut self,
        thread_id: ThreadId,
        notification: &ServerNotification,
    ) -> Option<ThreadSessionState> {
        let ServerNotification::ThreadStarted(notification) = notification else {
            return None;
        };
        let mut session = self.primary_session_configured.clone()?;
        session.thread_id = thread_id;
        session.thread_name = notification.thread.name.clone();
        session.model_provider_id = notification.thread.model_provider.clone();
        session
            .set_cwd_retargeting_implicit_runtime_workspace_root(notification.thread.cwd.clone());
        let rollout_path = notification.thread.path.clone();
        if let Some(model) =
            read_session_model(self.state_db.as_deref(), thread_id, rollout_path.as_deref()).await
        {
            session.model = model;
        } else if rollout_path.is_some() {
            session.model.clear();
        }
        session.message_history = None;
        session.rollout_path = rollout_path;
        self.upsert_agent_picker_thread(
            thread_id,
            notification.thread.agent_nickname.clone(),
            notification.thread.agent_role.clone(),
            /*is_closed*/ false,
        );
        Some(session)
    }

    pub(super) async fn enqueue_thread_request(
        &mut self,
        thread_id: ThreadId,
        request: ServerRequest,
    ) -> Result<()> {
        let inactive_interactive_request = if self.active_thread_id != Some(thread_id) {
            self.interactive_request_for_thread_request(thread_id, &request)
                .await?
        } else {
            None
        };
        let (sender, store) = {
            let channel = self.ensure_thread_channel(thread_id);
            (channel.sender.clone(), Arc::clone(&channel.store))
        };

        let (should_send, pending_status) = {
            let mut guard = store.lock().await;
            guard.push_request(request.clone());
            (guard.active, guard.side_parent_pending_status())
        };
        let request_status = SideParentStatus::for_request(&request);

        if should_send {
            match sender.try_send(ThreadBufferedEvent::Request(request)) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    tokio::spawn(async move {
                        if let Err(err) = sender.send(event).await {
                            tracing::warn!("thread {thread_id} event channel closed: {err}");
                        }
                    });
                }
                Err(TrySendError::Closed(_)) => {
                    tracing::warn!("thread {thread_id} event channel closed");
                }
            }
        } else if self.active_side_parent_thread_id().is_none()
            && let Some(request) = inactive_interactive_request
        {
            self.push_thread_interactive_request(request);
        }
        if let Some(status) = pending_status.or(request_status) {
            self.set_side_parent_status(thread_id, Some(status));
        }
        self.refresh_pending_thread_approvals().await;
        Ok(())
    }

    pub(super) async fn enqueue_thread_history_entry_response(
        &mut self,
        thread_id: ThreadId,
        event: HistoryLookupResponse,
    ) -> Result<()> {
        let (sender, store) = {
            let channel = self.ensure_thread_channel(thread_id);
            (channel.sender.clone(), Arc::clone(&channel.store))
        };

        let should_send = {
            let mut guard = store.lock().await;
            guard
                .buffer
                .push_back(ThreadBufferedEvent::HistoryEntryResponse(event.clone()));
            if guard.buffer.len() > guard.capacity
                && let Some(removed) = guard.buffer.pop_front()
                && let ThreadBufferedEvent::Request(request) = &removed
            {
                guard
                    .pending_interactive_replay
                    .note_evicted_server_request(request);
            }
            guard.active
        };

        if should_send {
            match sender.try_send(ThreadBufferedEvent::HistoryEntryResponse(event)) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    tokio::spawn(async move {
                        if let Err(err) = sender.send(event).await {
                            tracing::warn!("thread {thread_id} event channel closed: {err}");
                        }
                    });
                }
                Err(TrySendError::Closed(_)) => {
                    tracing::warn!("thread {thread_id} event channel closed");
                }
            }
        }
        Ok(())
    }

    pub(super) async fn enqueue_primary_thread_session(
        &mut self,
        session: ThreadSessionState,
        turns: Vec<Turn>,
    ) -> Result<()> {
        let thread_id = session.thread_id;
        self.primary_thread_id = Some(thread_id);
        self.primary_session_configured = Some(session.clone());
        self.upsert_agent_picker_thread(
            thread_id, /*agent_nickname*/ None, /*agent_role*/ None,
            /*is_closed*/ false,
        );
        let channel = self.ensure_thread_channel(thread_id);
        {
            let mut store = channel.store.lock().await;
            store.set_session(session.clone(), turns.clone());
        }
        self.activate_thread_channel(thread_id).await;
        self.chat_widget
            .set_initial_user_message_submit_suppressed(/*suppressed*/ true);
        self.chat_widget.handle_thread_session(session);
        let should_buffer_initial_replay = !turns.is_empty();
        if should_buffer_initial_replay {
            self.app_event_tx
                .send(AppEvent::BeginInitialHistoryReplayBuffer);
        }
        self.chat_widget
            .replay_thread_turns(turns, ReplayKind::ResumeInitialMessages);
        if should_buffer_initial_replay {
            self.app_event_tx
                .send(AppEvent::EndInitialHistoryReplayBuffer);
        }
        let pending = std::mem::take(&mut self.pending_primary_events);
        for pending_event in pending {
            match pending_event {
                ThreadBufferedEvent::Notification(notification) => {
                    self.enqueue_thread_notification(thread_id, notification)
                        .await?;
                }
                ThreadBufferedEvent::Request(request) => {
                    self.enqueue_thread_request(thread_id, request).await?;
                }
                ThreadBufferedEvent::HistoryEntryResponse(event) => {
                    self.enqueue_thread_history_entry_response(thread_id, event)
                        .await?;
                }
                ThreadBufferedEvent::FeedbackSubmission(event) => {
                    self.enqueue_thread_feedback_event(thread_id, event).await;
                }
            }
        }
        self.chat_widget
            .set_initial_user_message_submit_suppressed(/*suppressed*/ false);
        self.chat_widget.submit_initial_user_message_if_pending();
        Ok(())
    }

    pub(super) async fn enqueue_primary_thread_notification(
        &mut self,
        notification: ServerNotification,
    ) -> Result<()> {
        if let Some(thread_id) = self.primary_thread_id {
            return self
                .enqueue_thread_notification(thread_id, notification)
                .await;
        }
        self.pending_primary_events
            .push_back(ThreadBufferedEvent::Notification(notification));
        Ok(())
    }

    pub(super) async fn enqueue_primary_thread_request(
        &mut self,
        request: ServerRequest,
    ) -> Result<()> {
        if let Some(thread_id) = self.primary_thread_id {
            return self.enqueue_thread_request(thread_id, request).await;
        }
        self.pending_primary_events
            .push_back(ThreadBufferedEvent::Request(request));
        Ok(())
    }

    pub(super) async fn refresh_snapshot_session_if_needed(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        is_replay_only: bool,
        snapshot: &mut ThreadEventSnapshot,
    ) {
        if !self.should_refresh_snapshot_session(thread_id, is_replay_only, snapshot) {
            return;
        }

        match app_server
            .resume_thread(self.config.clone(), thread_id, self.resume_model_settings())
            .await
        {
            Ok(started) => {
                self.apply_refreshed_snapshot_thread(thread_id, started, snapshot)
                    .await
            }
            Err(err) => {
                tracing::warn!(
                    thread_id = %thread_id,
                    error = %err,
                    "failed to refresh inferred thread session before replay"
                );
            }
        }
    }

    pub(super) fn should_refresh_snapshot_session(
        &self,
        thread_id: ThreadId,
        is_replay_only: bool,
        snapshot: &ThreadEventSnapshot,
    ) -> bool {
        !is_replay_only
            && !self.side_threads.contains_key(&thread_id)
            && snapshot.session.as_ref().is_none_or(|session| {
                session.model.trim().is_empty() || session.rollout_path.is_none()
            })
    }

    pub(super) async fn apply_refreshed_snapshot_thread(
        &mut self,
        thread_id: ThreadId,
        started: AppServerStartedThread,
        snapshot: &mut ThreadEventSnapshot,
    ) {
        let AppServerStartedThread { session, turns } = started;
        if let Some(channel) = self.thread_event_channels.get(&thread_id) {
            let mut store = channel.store.lock().await;
            store.set_session(session.clone(), turns.clone());
            store.rebase_buffer_after_session_refresh();
        }
        snapshot.session = Some(session);
        snapshot.turns = turns;
        snapshot
            .events
            .retain(ThreadEventStore::event_survives_session_refresh);
    }

    /// Opens the `/agent` picker after refreshing cached labels for known threads.
    ///
    /// The picker state is derived from long-lived thread channels plus best-effort metadata
    /// refreshes from the backend. Refresh failures are treated as "thread is only inspectable by
    /// historical id now" and converted into closed picker entries instead of deleting them, so
    /// the stable traversal order remains intact for review and keyboard navigation.
    pub(super) async fn drain_active_thread_events(&mut self, tui: &mut tui::Tui) -> Result<()> {
        let Some(mut rx) = self.active_thread_rx.take() else {
            return Ok(());
        };

        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(event) => self.handle_thread_event_now(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        if !disconnected {
            self.active_thread_rx = Some(rx);
        } else {
            self.clear_active_thread().await;
        }

        if self.backtrack_render_pending {
            tui.frame_requester().schedule_frame();
        }
        Ok(())
    }

    /// Returns `(closed_thread_id, primary_thread_id)` when a non-primary active
    /// thread has died and we should fail over to the primary thread.
    ///
    /// A user-requested shutdown (`ExitMode::ShutdownFirst`) sets
    /// `pending_shutdown_exit_thread_id`; matching shutdown completions are ignored
    /// here so Ctrl+C-like exits don't accidentally resurrect the main thread.
    ///
    /// Failover is only eligible when all of these are true:
    /// 1. the event is `thread/closed`;
    /// 2. the active thread differs from the primary thread;
    /// 3. the active thread is not the pending shutdown-exit thread.
    pub(super) fn active_non_primary_shutdown_target(
        &self,
        notification: &ServerNotification,
    ) -> Option<(ThreadId, ThreadId)> {
        if !matches!(notification, ServerNotification::ThreadClosed(_)) {
            return None;
        }
        let active_thread_id = self.active_thread_id?;
        let primary_thread_id = self.primary_thread_id?;
        if self.pending_shutdown_exit_thread_id == Some(active_thread_id) {
            return None;
        }
        (active_thread_id != primary_thread_id).then_some((active_thread_id, primary_thread_id))
    }

    pub(super) fn replay_thread_snapshot(
        &mut self,
        snapshot: ThreadEventSnapshot,
        resume_restored_queue: bool,
    ) {
        self.refresh_mcp_startup_expected_servers_from_config();
        let should_buffer_replay = !snapshot.turns.is_empty() || !snapshot.events.is_empty();
        if should_buffer_replay {
            self.app_event_tx
                .send(AppEvent::BeginThreadSwitchHistoryReplayBuffer);
        }
        let suppress_replay_notices =
            replay_filter::snapshot_has_pending_interactive_request(&snapshot);
        if let Some(session) = snapshot.session {
            if session.reasoning_effort != Some(ReasoningEffortConfig::Ultra) {
                self.chat_widget
                    .set_plan_mode_reasoning_effort(self.config.plan_mode_reasoning_effort.clone());
            }
            if self.side_threads.contains_key(&session.thread_id) {
                self.chat_widget.handle_side_thread_session(session);
            } else if suppress_replay_notices {
                self.chat_widget.handle_thread_session_quiet(session);
            } else {
                self.chat_widget.handle_thread_session(session);
            }
        }
        self.chat_widget
            .set_queue_autosend_suppressed(/*suppressed*/ true);
        self.chat_widget
            .restore_thread_input_state(snapshot.input_state);
        self.resolve_notified_composer_submissions_for_active_thread();
        if !snapshot.turns.is_empty() {
            self.chat_widget
                .replay_thread_turns(snapshot.turns, ReplayKind::ThreadSnapshot);
        }
        for event in snapshot.events {
            if suppress_replay_notices && replay_filter::event_is_notice(&event) {
                continue;
            }
            self.handle_thread_event_replay(event);
        }
        if should_buffer_replay {
            self.app_event_tx
                .send(AppEvent::EndInitialHistoryReplayBuffer);
        }
        self.chat_widget
            .set_queue_autosend_suppressed(/*suppressed*/ false);
        self.chat_widget
            .set_initial_user_message_submit_suppressed(/*suppressed*/ false);
        self.chat_widget.submit_initial_user_message_if_pending();
        if resume_restored_queue {
            self.chat_widget.maybe_send_next_queued_input();
        }
        self.refresh_status_line();
    }

    pub(super) fn should_wait_for_initial_session(session_selection: &SessionSelection) -> bool {
        matches!(
            session_selection,
            SessionSelection::StartFresh | SessionSelection::Exit
        )
    }

    pub(super) fn should_prompt_for_paused_goal_after_startup_resume(
        session_selection: &SessionSelection,
        initial_prompt: &Option<String>,
        initial_images: &[PathBuf],
    ) -> bool {
        matches!(session_selection, SessionSelection::Resume(_))
            && initial_prompt.is_none()
            && initial_images.is_empty()
    }

    pub(super) fn should_handle_active_thread_events(
        waiting_for_initial_session_configured: bool,
        has_active_thread_receiver: bool,
    ) -> bool {
        has_active_thread_receiver && !waiting_for_initial_session_configured
    }

    pub(super) fn should_stop_waiting_for_initial_session(
        waiting_for_initial_session_configured: bool,
        primary_thread_id: Option<ThreadId>,
    ) -> bool {
        waiting_for_initial_session_configured && primary_thread_id.is_some()
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn handle_skills_list_response(&mut self, response: SkillsListResponse) {
        let cwd = self.chat_widget.config_ref().cwd.clone();
        let errors = errors_for_cwd(&cwd, &response);
        let errors = self.skill_load_warnings.newly_active_errors(&errors);
        emit_skill_load_warnings(&self.app_event_tx, &errors);
        self.chat_widget.handle_skills_list_response(response);
    }

    pub(super) async fn handle_thread_rollback_response(
        &mut self,
        thread_id: ThreadId,
        num_turns: u32,
        response: &ThreadRollbackResponse,
    ) {
        self.handle_thread_rollback_response_with_origin(
            thread_id,
            num_turns,
            response,
            ThreadRollbackOrigin::Backtrack,
        )
        .await;
    }

    pub(super) async fn handle_thread_rollback_response_with_origin(
        &mut self,
        thread_id: ThreadId,
        num_turns: u32,
        response: &ThreadRollbackResponse,
        origin: ThreadRollbackOrigin,
    ) {
        let (surviving_client_ids, surviving_submission_ids) =
            surviving_composer_submission_ids(&response.thread);
        let pending_client_ids = self
            .pending_composer_submissions
            .iter()
            .filter_map(|(client_id, pending)| {
                (pending.submission.thread_id() == thread_id.to_string())
                    .then_some(client_id.clone())
            })
            .collect::<Vec<_>>();
        for client_id in pending_client_ids {
            if surviving_client_ids.contains(&client_id) {
                if let Some(pending) = self.pending_composer_submissions.get_mut(&client_id) {
                    pending.item_committed = true;
                    self.composer_submission_transitions.push_back(
                        super::ComposerSubmissionTransition::Pending(pending.submission.clone()),
                    );
                }
                self.resolve_notified_composer_submission(&client_id);
            } else if let Some(pending) = self.pending_composer_submissions.remove(&client_id) {
                self.composer_submission_transitions.push_back(
                    super::ComposerSubmissionTransition::Abandoned(pending.submission),
                );
            }
        }
        self.composer_submission_transitions.push_back(
            super::ComposerSubmissionTransition::ThreadRolledBack {
                thread_id: thread_id.to_string(),
                surviving_submission_ids,
            },
        );
        if let Some(channel) = self.thread_event_channels.get(&thread_id) {
            let mut store = channel.store.lock().await;
            store.apply_thread_rollback(response);
        }
        if self.active_thread_id == Some(thread_id)
            && let Some(mut rx) = self.active_thread_rx.take()
        {
            let mut disconnected = false;
            loop {
                match rx.try_recv() {
                    Ok(_) => {}
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }

            if !disconnected {
                self.active_thread_rx = Some(rx);
            } else {
                self.clear_active_thread().await;
            }
        }
        match origin {
            ThreadRollbackOrigin::Backtrack => self.handle_backtrack_rollback_succeeded(num_turns),
            ThreadRollbackOrigin::SafetyBufferingRetry => {
                self.apply_non_pending_thread_rollback(num_turns);
            }
        }
    }

    pub(super) fn handle_thread_event_now(&mut self, event: ThreadBufferedEvent) {
        let needs_refresh = matches!(
            &event,
            ThreadBufferedEvent::Notification(ServerNotification::TurnStarted(_))
                | ThreadBufferedEvent::Notification(ServerNotification::ThreadTokenUsageUpdated(_))
        );
        match event {
            ThreadBufferedEvent::Notification(notification) => {
                self.cache_collab_receiver_threads_for_notification(&notification);
                self.chat_widget
                    .handle_server_notification(notification, /*replay_kind*/ None);
            }
            ThreadBufferedEvent::Request(request) => {
                if self
                    .pending_app_server_requests
                    .contains_server_request(&request)
                {
                    self.chat_widget
                        .handle_server_request(request, /*replay_kind*/ None);
                }
            }
            ThreadBufferedEvent::HistoryEntryResponse(event) => {
                self.chat_widget.handle_history_entry_response(event);
            }
            ThreadBufferedEvent::FeedbackSubmission(event) => {
                self.handle_feedback_thread_event(event);
            }
        }
        if needs_refresh {
            self.refresh_status_line();
        }
    }

    pub(super) fn handle_thread_event_replay(&mut self, event: ThreadBufferedEvent) {
        match event {
            ThreadBufferedEvent::Notification(notification) => {
                self.chat_widget
                    .handle_server_notification(notification, Some(ReplayKind::ThreadSnapshot));
            }
            ThreadBufferedEvent::Request(request) => self
                .chat_widget
                .handle_server_request(request, Some(ReplayKind::ThreadSnapshot)),
            ThreadBufferedEvent::HistoryEntryResponse(event) => {
                self.chat_widget.handle_history_entry_response(event)
            }
            ThreadBufferedEvent::FeedbackSubmission(event) => {
                self.handle_feedback_thread_event(event);
            }
        }
    }

    pub(super) fn register_pending_composer_submission(
        &mut self,
        submission: NativeComposerSubmission,
        turn_id: String,
    ) {
        let client_id = submission.client_user_message_id();
        self.pending_composer_submissions.insert(
            client_id,
            super::PendingComposerSubmissionCommit {
                submission: submission.clone(),
                turn_id: Some(turn_id),
                external_commit: None,
                item_committed: false,
            },
        );
        self.composer_submission_transitions
            .push_back(super::ComposerSubmissionTransition::Pending(submission));
    }

    pub(super) fn note_composer_submission_notification(
        &mut self,
        notification: &ServerNotification,
    ) {
        match notification {
            ServerNotification::ItemCompleted(notification) => {
                let ThreadItem::UserMessage {
                    client_id: Some(client_id),
                    ..
                } = &notification.item
                else {
                    return;
                };
                let Some(pending) = self.pending_composer_submissions.get_mut(client_id) else {
                    return;
                };
                pending.item_committed = true;
                self.resolve_notified_composer_submission(client_id);
            }
            ServerNotification::TurnCompleted(notification) => {
                let abandoned_ids = self
                    .pending_composer_submissions
                    .iter()
                    .filter_map(|(client_id, pending)| {
                        (!pending.item_committed
                            && pending.turn_id.as_deref() == Some(notification.turn.id.as_str())
                            && pending.submission.thread_id() == notification.thread_id)
                            .then_some(client_id.clone())
                    })
                    .collect::<Vec<_>>();
                for client_id in abandoned_ids {
                    if let Some(pending) = self.pending_composer_submissions.remove(&client_id) {
                        self.composer_submission_transitions.push_back(
                            super::ComposerSubmissionTransition::Abandoned(pending.submission),
                        );
                    }
                }
            }
            _ => {}
        }
    }

    fn resolve_notified_composer_submission(&mut self, client_id: &str) {
        let Some(pending) = self.pending_composer_submissions.get(client_id) else {
            return;
        };
        if !pending.item_committed {
            return;
        }
        let external_commit_waits_for_thread = pending.external_commit.is_some()
            && self
                .chat_widget
                .thread_id()
                .is_none_or(|thread_id| thread_id.to_string() != pending.submission.thread_id());
        if external_commit_waits_for_thread {
            return;
        }

        if let Some(commit) = pending.external_commit.as_ref()
            && !commit.local_clear_relinquished()
            && !self.chat_widget.commit_native_composer_submit(commit)
        {
            self.chat_widget.add_error_message(
                "Koenig send committed, but its exact retained draft could not be cleared. Submission remains fenced."
                    .to_string(),
            );
            return;
        }

        let Some(pending) = self.pending_composer_submissions.remove(client_id) else {
            return;
        };
        self.composer_submission_transitions.push_back(
            super::ComposerSubmissionTransition::Committed(pending.submission),
        );
    }

    pub(super) fn resolve_notified_composer_submissions_for_active_thread(&mut self) {
        let client_ids = self
            .pending_composer_submissions
            .iter()
            .filter_map(|(client_id, pending)| {
                (pending.item_committed && pending.external_commit.is_some())
                    .then_some(client_id.clone())
            })
            .collect::<Vec<_>>();
        for client_id in client_ids {
            self.resolve_notified_composer_submission(&client_id);
        }
    }

    /// Handles an event emitted by the currently active thread.
    ///
    /// This function enforces shutdown intent routing: unexpected non-primary
    /// thread shutdowns fail over to the primary thread, while user-requested
    /// app exits consume only the tracked shutdown completion and then proceed.
    pub(super) async fn handle_active_thread_event(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        event: ThreadBufferedEvent,
    ) -> Result<()> {
        // Capture this before any potential thread switch: we only want to clear
        // the exit marker when the currently active thread acknowledges shutdown.
        let pending_shutdown_exit_completed = matches!(
            &event,
            ThreadBufferedEvent::Notification(ServerNotification::ThreadClosed(_))
        ) && self.pending_shutdown_exit_thread_id
            == self.active_thread_id;

        // Processing order matters:
        //
        // 1. handle unexpected non-primary shutdown failover first;
        // 2. clear pending exit marker for matching shutdown;
        // 3. forward the event through normal handling.
        //
        // This preserves the mental model that user-requested exits do not trigger
        // failover, while true sub-agent deaths still do.
        if let ThreadBufferedEvent::Notification(notification) = &event
            && let Some((closed_thread_id, primary_thread_id)) =
                self.active_non_primary_shutdown_target(notification)
        {
            self.mark_agent_picker_thread_closed(closed_thread_id);
            if self.side_threads.contains_key(&closed_thread_id) {
                self.discard_closed_side_thread(closed_thread_id).await;
                self.select_agent_thread(tui, app_server, primary_thread_id)
                    .await?;
            } else {
                self.select_agent_thread_and_discard_side(tui, app_server, primary_thread_id)
                    .await?;
            }
            if self.active_thread_id == Some(primary_thread_id) {
                self.chat_widget.add_info_message(
                    format!(
                        "Agent thread {closed_thread_id} closed. Switched back to main thread."
                    ),
                    /*hint*/ None,
                );
            } else {
                self.clear_active_thread().await;
                self.chat_widget.add_error_message(format!(
                    "Agent thread {closed_thread_id} closed. Failed to switch back to main thread {primary_thread_id}.",
                ));
            }
            return Ok(());
        }

        if pending_shutdown_exit_completed {
            // Clear only after seeing the shutdown completion for the tracked
            // thread, so unrelated shutdowns cannot consume this marker.
            self.pending_shutdown_exit_thread_id = None;
        }
        self.handle_thread_event_now(event);
        if self.backtrack_render_pending {
            tui.frame_requester().schedule_frame();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::ActivePermissionProfile;
    use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_WORKSPACE;

    #[test]
    fn correction_server_error_outcome_only_treats_request_rejection_as_not_applied() {
        assert_eq!(
            correction_server_error_outcome(JSONRPC_INVALID_REQUEST),
            CorrectionDispatchOutcome::NotApplied
        );
        assert_eq!(
            correction_server_error_outcome(JSONRPC_INVALID_PARAMS),
            CorrectionDispatchOutcome::NotApplied
        );
        for code in [-32601, -32603, -32000] {
            assert_eq!(
                correction_server_error_outcome(code),
                CorrectionDispatchOutcome::Unknown
            );
        }
    }

    async fn config_with_workspace_profile() -> Config {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        ConfigBuilder::default()
            .codex_home(temp_dir.path().to_path_buf())
            .harness_overrides(ConfigOverrides {
                default_permissions: Some(BUILT_IN_PERMISSION_PROFILE_WORKSPACE.to_string()),
                ..ConfigOverrides::default()
            })
            .build()
            .await
            .expect("config should build")
    }

    #[tokio::test]
    async fn turn_permissions_use_active_profile_when_available() {
        let config = config_with_workspace_profile().await;
        let active_permission_profile = config.permissions.active_permission_profile();

        assert_eq!(
            App::turn_permissions_override_from_config(
                &config,
                active_permission_profile.as_ref(),
                /*runtime_permission_profile_override*/ None,
            ),
            TurnPermissionsOverride::ActiveProfile(ActivePermissionProfile::new(
                BUILT_IN_PERMISSION_PROFILE_WORKSPACE
            ))
        );
    }

    #[tokio::test]
    async fn turn_permissions_preserve_server_snapshot_without_local_override() {
        let mut config = config_with_workspace_profile().await;
        config
            .permissions
            .set_permission_profile(PermissionProfile::read_only())
            .expect("read-only profile should be allowed");

        assert_eq!(
            App::turn_permissions_override_from_config(
                &config, /*active_permission_profile*/ None,
                /*runtime_permission_profile_override*/ None,
            ),
            TurnPermissionsOverride::Preserve
        );
    }

    #[tokio::test]
    async fn turn_permissions_send_legacy_sandbox_for_local_override() {
        let mut config = config_with_workspace_profile().await;
        let permission_profile = PermissionProfile::workspace_write();
        config
            .permissions
            .set_permission_profile(permission_profile.clone())
            .expect("workspace profile should be allowed");
        let effective_permission_profile = config.permissions.effective_permission_profile();

        assert_eq!(
            App::turn_permissions_override_from_config(
                &config,
                /*active_permission_profile*/ None,
                Some(&permission_profile),
            ),
            TurnPermissionsOverride::LegacySandbox(effective_permission_profile)
        );
    }
}

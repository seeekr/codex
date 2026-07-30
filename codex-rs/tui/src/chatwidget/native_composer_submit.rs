//! Two-phase native composer submission.
//!
//! Preparation is read-only and admits only an exact plain draft. App-server acceptance happens
//! before [`ChatWidget::commit_native_composer_submit`] clears or records the draft, so a definite
//! refusal leaves the composer byte-for-byte untouched.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use super::*;
use crate::bottom_pane::ComposerLeaseId;
use crate::bottom_pane::PlainComposerSubmission;
use crate::composer_control::ErrorCode;
use crate::composer_control::NativeComposerSubmission;
use crate::composer_control::SubmitFence;

/// Immutable app-server request plus the exact local commit awaiting acceptance. It deliberately
/// has no `Debug` implementation because it carries composer text.
pub(crate) struct PreparedNativeComposerSubmit {
    op: AppCommand,
    submission: NativeComposerSubmission,
    commit: Option<Arc<NativeComposerCommit>>,
}

/// Exact local composer commit retained across an ambiguous app-server acknowledgement.
///
/// The shared fence is relinquished before any queued user edit is applied, preventing a later
/// client-ID notification from clearing user-owned text.
pub(crate) struct NativeComposerCommit {
    preview: PlainComposerSubmission,
    native: ComposerLeaseId,
    expected: String,
    user_message: UserMessage,
    render_in_history: bool,
    pending_steer_compare_key: PendingSteerCompareKey,
    submission: NativeComposerSubmission,
    fence: SubmitFence,
}

impl NativeComposerCommit {
    pub(crate) fn local_clear_relinquished(&self) -> bool {
        self.fence.is_relinquished()
    }
}

impl PreparedNativeComposerSubmit {
    pub(crate) fn op(&self) -> &AppCommand {
        &self.op
    }

    pub(crate) fn submission(&self) -> &NativeComposerSubmission {
        &self.submission
    }

    pub(crate) fn commit(&self) -> Option<Arc<NativeComposerCommit>> {
        self.commit.as_ref().map(Arc::clone)
    }
}

impl ChatWidget {
    fn koenig_composer_submission_available(&self) -> bool {
        self.composer_control_available()
            && self.is_session_configured()
            && !self.is_plan_streaming_in_tui()
            && !self.input_queue.suppress_queue_autosend
            && !self.only_user_shell_commands_running()
    }

    pub(crate) fn direct_send_acquisition_available(&self) -> bool {
        self.koenig_composer_submission_available()
            && self.bottom_pane.composer_is_strict_plain_empty()
    }

    pub(crate) fn plain_draft_send_acquisition_available(&self) -> bool {
        if !self.koenig_composer_submission_available() {
            return false;
        }
        self.bottom_pane
            .composer_strict_plain_submission_text()
            .is_some_and(|text| !self.plain_text_resolves_rich_mentions(&text))
    }

    pub(crate) fn prepare_native_composer_submit(
        &self,
        native: ComposerLeaseId,
        expected: &str,
        fence: SubmitFence,
    ) -> Result<PreparedNativeComposerSubmit, ErrorCode> {
        let thread_id = self.thread_id.ok_or(ErrorCode::ComposerUnavailable)?;
        if !self.koenig_composer_submission_available() {
            return Err(ErrorCode::SubmissionUnavailable);
        }
        let preview = self
            .bottom_pane
            .preview_owned_plain_submission(native, expected)
            .ok_or(ErrorCode::SubmissionUnavailable)?;
        if self.plain_text_resolves_rich_mentions(&preview.submitted_text) {
            return Err(ErrorCode::SubmissionUnavailable);
        }
        let submission = NativeComposerSubmission::new(
            thread_id.to_string(),
            &preview.submitted_text,
            preview.leases.clone(),
        )
        .ok_or(ErrorCode::SubmissionUnavailable)?;
        let user_message = UserMessage {
            text: preview.submitted_text.clone(),
            local_images: Vec::new(),
            remote_image_urls: Vec::new(),
            text_elements: Vec::new(),
            mention_bindings: Vec::new(),
        };
        let (op, items) = self.prepare_koenig_user_turn(&user_message.text, submission.clone())?;
        let commit = Arc::new(NativeComposerCommit {
            preview,
            native,
            expected: expected.to_string(),
            user_message,
            render_in_history: !self.turn_lifecycle.agent_turn_running,
            pending_steer_compare_key: Self::pending_steer_compare_key_from_items(&items),
            submission: submission.clone(),
            fence,
        });
        Ok(PreparedNativeComposerSubmit {
            op,
            submission,
            commit: Some(commit),
        })
    }

    pub(crate) fn prepare_reserved_send(
        &self,
        submission: NativeComposerSubmission,
        text: &str,
    ) -> Result<PreparedNativeComposerSubmit, ErrorCode> {
        let thread_id = self.thread_id.ok_or(ErrorCode::ComposerUnavailable)?;
        if submission.thread_id() != thread_id.to_string()
            || !submission.is_direct()
            || submission.submitted_text() != text
            || !self.is_session_configured()
        {
            return Err(ErrorCode::SubmissionUnavailable);
        }
        let (op, _) = self.prepare_koenig_user_turn(text, submission.clone())?;
        Ok(PreparedNativeComposerSubmit {
            op,
            submission,
            commit: None,
        })
    }

    fn prepare_koenig_user_turn(
        &self,
        text: &str,
        submission: NativeComposerSubmission,
    ) -> Result<(AppCommand, Vec<UserInput>), ErrorCode> {
        let mut items = vec![UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }];
        if self.ide_context.is_enabled()
            && let Ok(context) = crate::ide_context::fetch_ide_context(&self.config.cwd)
        {
            crate::ide_context::apply_ide_context_to_user_input(&context, &mut items);
        }

        let effective_mode = self.effective_collaboration_mode();
        if effective_mode.model().trim().is_empty() {
            return Err(ErrorCode::SubmissionUnavailable);
        }
        let collaboration_mode = if self.collaboration_modes_enabled() {
            self.active_collaboration_mask
                .as_ref()
                .map(|_| effective_mode.clone())
        } else {
            None
        };
        let personality = self
            .config
            .personality
            .filter(|_| self.config.features.enabled(Feature::Personality))
            .filter(|_| self.current_model_supports_personality());
        let op = AppCommand::user_turn(
            items.clone(),
            self.config.cwd.to_path_buf(),
            AskForApproval::from(self.config.permissions.approval_policy.value()),
            self.config.permissions.active_permission_profile(),
            effective_mode.model().to_string(),
            effective_mode.reasoning_effort(),
            /*summary*/ None,
            self.service_tier_update_for_core(),
            /*final_output_json_schema*/ None,
            collaboration_mode,
            personality,
        )
        .with_composer_submission(Some(submission));
        Ok((op, items))
    }

    fn plain_text_resolves_rich_mentions(&self, text: &str) -> bool {
        let mentions = collect_tool_mentions(text, &HashMap::new());
        let mut skill_names_lower = HashSet::new();
        if let Some(skills) = self.bottom_pane.skills() {
            if !find_skill_mentions_with_tool_mentions(&mentions, skills).is_empty() {
                return true;
            }
            skill_names_lower.extend(skills.iter().map(|skill| skill.name.to_ascii_lowercase()));
        }
        self.connectors_for_mentions()
            .is_some_and(|apps| !find_app_mentions(&mentions, apps, &skill_names_lower).is_empty())
    }

    pub(crate) fn commit_native_composer_submit(&mut self, commit: &NativeComposerCommit) -> bool {
        if commit.fence.is_relinquished()
            || self.thread_id.map(|thread_id| thread_id.to_string())
                != Some(commit.submission.thread_id().to_string())
            || !self.bottom_pane.commit_owned_plain_submission(
                &commit.preview,
                commit.native,
                &commit.expected,
            )
        {
            return false;
        }

        self.reasoning_buffer.clear();
        self.full_reasoning_buffer.clear();
        self.set_status_header(String::from("Working"));
        self.append_message_history_entry(commit.user_message.text.clone());
        if commit.render_in_history {
            self.locally_rendered_composer_submission_ids
                .insert(commit.submission.client_user_message_id());
            self.input_queue.user_turn_pending_start = true;
            self.record_cancel_edit_candidate(commit.user_message.clone());
            self.on_user_message_display(user_message_display_for_history(
                commit.user_message.clone(),
                &UserMessageHistoryRecord::UserMessageText,
            ));
        } else {
            self.input_queue.pending_steers.push_back(PendingSteer {
                user_message: commit.user_message.clone(),
                history_record: UserMessageHistoryRecord::UserMessageText,
                compare_key: commit.pending_steer_compare_key.clone(),
                composer_submission: Some(commit.submission.clone()),
            });
            self.transcript.saw_plan_item_this_turn = false;
            self.refresh_pending_input_preview();
        }
        self.transcript.needs_final_message_separator = false;
        self.refresh_plan_mode_nudge();
        true
    }
}

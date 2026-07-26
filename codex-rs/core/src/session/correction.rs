use std::io;
use std::sync::Arc;

use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::MAX_ADDITIONAL_CONTEXT_VALUE_TOKENS;
use codex_protocol::protocol::RolloutItem;
use codex_utils_string::truncate_middle_with_token_budget;
use uuid::Uuid;

use super::CorrectionCommitStatus;
use super::Session;
use super::TurnInput;
use crate::context::AdditionalContextDeveloperFragment;
use crate::context::ContextualUserFragment;
use crate::state::TaskKind;
use crate::tasks::RegularTask;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;

const CORRECTION_FRAME_ID_PREFIX: &str = "msg_correction_";

pub(super) fn correction_frame_id(item: &ResponseItem) -> Option<&str> {
    let id = item.id()?;
    let uuid = id.strip_prefix(CORRECTION_FRAME_ID_PREFIX)?;
    Uuid::parse_str(uuid).ok()?;
    Some(id)
}

pub(super) fn correction_frames_have_same_payload(
    left: &ResponseItem,
    right: &ResponseItem,
) -> bool {
    match (left, right) {
        (
            ResponseItem::Message {
                id: left_id,
                role: left_role,
                content: left_content,
                ..
            },
            ResponseItem::Message {
                id: right_id,
                role: right_role,
                content: right_content,
                ..
            },
        ) => left_id == right_id && left_role == right_role && left_content == right_content,
        _ => false,
    }
}

fn correction_frame(correction_id: Uuid, payload: String) -> ResponseItem {
    let stable_id = format!("{CORRECTION_FRAME_ID_PREFIX}{}", correction_id.simple());
    let fragment = AdditionalContextDeveloperFragment::new(stable_id.clone(), payload)
        .into_response_input_item();
    let mut frame = ResponseItem::from(fragment);
    frame.set_id(Some(stable_id));
    frame
}

enum ExistingCorrection {
    Missing,
    Identical,
    Conflict,
}

impl Session {
    fn validate_correction(correction_id: &str, payload: String) -> CodexResult<ResponseItem> {
        let correction_id = Uuid::parse_str(correction_id).map_err(|error| {
            CodexErr::InvalidRequest(format!("correctionId must be a UUID: {error}"))
        })?;
        if payload.is_empty() {
            return Err(CodexErr::InvalidRequest(
                "correction payload must not be empty".to_string(),
            ));
        }
        if truncate_middle_with_token_budget(&payload, MAX_ADDITIONAL_CONTEXT_VALUE_TOKENS)
            .1
            .is_some()
        {
            return Err(CodexErr::InvalidRequest(format!(
                "correction payload exceeds the {MAX_ADDITIONAL_CONTEXT_VALUE_TOKENS}-token limit"
            )));
        }
        Ok(correction_frame(correction_id, payload))
    }

    async fn reserve_correction_intent(&self, expected: &ResponseItem) -> ExistingCorrection {
        let Some(stable_id) = correction_frame_id(expected) else {
            return ExistingCorrection::Conflict;
        };
        let mut state = self.state.lock().await;
        if state.correction_receipt_conflicts.contains(stable_id) {
            return ExistingCorrection::Conflict;
        }
        match state.correction_receipts.get(stable_id) {
            Some(existing) if correction_frames_have_same_payload(existing, expected) => {
                return ExistingCorrection::Identical;
            }
            Some(_) => return ExistingCorrection::Conflict,
            None => {}
        }
        match state.correction_intents.get(stable_id) {
            Some(existing) if correction_frames_have_same_payload(existing, expected) => {
                ExistingCorrection::Missing
            }
            Some(_) => ExistingCorrection::Conflict,
            None => {
                state
                    .correction_intents
                    .insert(stable_id.to_string(), expected.clone());
                ExistingCorrection::Missing
            }
        }
    }

    async fn release_uncommitted_correction_intent(&self, expected: &ResponseItem) {
        let Some(stable_id) = correction_frame_id(expected) else {
            return;
        };
        let mut state = self.state.lock().await;
        if state.correction_receipts.contains_key(stable_id) {
            return;
        }
        if state
            .correction_intents
            .get(stable_id)
            .is_some_and(|intent| correction_frames_have_same_payload(intent, expected))
        {
            state.correction_intents.remove(stable_id);
        }
    }

    pub(crate) async fn persist_and_install_correction(
        &self,
        turn_context: &super::TurnContext,
        frame: ResponseItem,
    ) -> CodexResult<()> {
        let prepared = self
            .prepare_conversation_items_for_history(turn_context, std::slice::from_ref(&frame))
            .into_owned();
        let [prepared_frame] = prepared.as_slice() else {
            return Err(CodexErr::Fatal(
                "correction frame preparation produced an invalid item count".to_string(),
            ));
        };
        let live_thread = self
            .live_thread_for_persistence("commit correction")
            .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
        live_thread
            .append_items(&[RolloutItem::ResponseItem(prepared_frame.clone())])
            .await
            .map_err(|error| CodexErr::Io(io::Error::other(error)))?;
        live_thread
            .flush()
            .await
            .map_err(|error| CodexErr::Io(io::Error::other(error)))?;

        {
            let mut state = self.state.lock().await;
            state
                .current_time_reminder
                .note_recorded_items(std::slice::from_ref(prepared_frame));
            state.record_items(
                std::slice::from_ref(prepared_frame).iter(),
                turn_context.model_info.truncation_policy.into(),
            );
            let stable_id = correction_frame_id(prepared_frame)
                .expect("prepared correction frame must retain its stable id");
            state
                .correction_receipts
                .insert(stable_id.to_string(), prepared_frame.clone());
            state.correction_intents.remove(stable_id);
        }
        self.send_raw_response_items(turn_context, std::slice::from_ref(prepared_frame))
            .await;
        Ok(())
    }

    /// Commit a correction through the same serialized submission loop as ordinary user input.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "the active-turn lock keeps the final sampling boundary atomic with durable commit"
    )]
    pub(super) async fn commit_correction(
        self: &Arc<Self>,
        correction_id: String,
        payload: String,
    ) -> CodexResult<CorrectionCommitStatus> {
        let frame = Self::validate_correction(&correction_id, payload)?;
        match self.reserve_correction_intent(&frame).await {
            ExistingCorrection::Identical => {
                return Ok(CorrectionCommitStatus::AlreadyCommitted);
            }
            ExistingCorrection::Conflict => {
                return Err(CodexErr::InvalidRequest(format!(
                    "correctionId `{correction_id}` was reused with different content"
                )));
            }
            ExistingCorrection::Missing => {}
        }

        loop {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(active_turn) => {
                    let Some(active_task) = active_turn.task.as_ref() else {
                        if active_turn
                            .startup_done
                            .as_ref()
                            .is_some_and(|publication| publication.is_abandoned())
                            || active_turn
                                .terminal_done
                                .as_ref()
                                .is_some_and(|publication| publication.is_abandoned())
                        {
                            *active = None;
                            drop(active);
                            continue;
                        }
                        drop(active);
                        return Err(CodexErr::Io(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "correction commit is waiting for a turn lifecycle transition",
                        )));
                    };
                    if active_task.kind != TaskKind::Regular {
                        drop(active);
                        self.release_uncommitted_correction_intent(&frame).await;
                        return Err(CodexErr::InvalidRequest(
                            "correction commit is unavailable during review or compaction"
                                .to_string(),
                        ));
                    }
                    if !active_task.accepts_steer {
                        drop(active);
                        return Err(CodexErr::Io(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "correction commit is waiting for the active turn to close",
                        )));
                    }

                    let turn_context = Arc::clone(&active_task.turn_context);
                    let turn_state = Arc::clone(&active_turn.turn_state);
                    self.persist_and_install_correction(turn_context.as_ref(), frame.clone())
                        .await?;
                    self.input_queue
                        .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                            turn_state.as_ref(),
                            vec![TurnInput::CommittedCorrection],
                        )
                        .await;
                    return Ok(CorrectionCommitStatus::Committed);
                }
                None => {
                    drop(active);
                    let Some(reservation) = self.reserve_task_start().await else {
                        continue;
                    };

                    let turn_context = self.new_default_turn().await;
                    let (task, committed) = RegularTask::with_correction(frame.clone());
                    self.maybe_emit_model_warnings_for_turn(turn_context.as_ref())
                        .await;
                    // Keep the expanded generic task-start future off Tokio's default 2 MiB worker
                    // stack; inlining it here overflows on the idle correction path.
                    if !Box::pin(self.start_reserved_task(
                        reservation,
                        turn_context,
                        vec![TurnInput::CommittedCorrection],
                        task,
                    ))
                    .await
                    {
                        return Err(CodexErr::InternalAgentDied);
                    }
                    return match committed.await {
                        Ok(Ok(())) => Ok(CorrectionCommitStatus::Committed),
                        Ok(Err(error)) => Err(error),
                        Err(_) => Err(CodexErr::InternalAgentDied),
                    };
                }
            }
        }
    }

    pub(super) async fn resolve_correction_commit(
        &self,
        submission_id: &str,
        result: CodexResult<CorrectionCommitStatus>,
    ) {
        if let Some(waiter) = self
            .pending_correction_commits
            .lock()
            .await
            .remove(submission_id)
        {
            let _ = waiter.send(result);
        }
    }
}

#[cfg(test)]
pub(super) fn correction_payload_text(item: &ResponseItem) -> Option<&str> {
    use codex_protocol::models::ContentItem;

    match item {
        ResponseItem::Message { role, content, .. } if role == "developer" => {
            match content.as_slice() {
                [ContentItem::InputText { text }] => Some(text),
                _ => None,
            }
        }
        _ => None,
    }
}

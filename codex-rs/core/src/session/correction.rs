use std::io;
use std::sync::Arc;

use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CorrectionIntent;
use codex_protocol::protocol::MAX_ADDITIONAL_CONTEXT_VALUE_TOKENS;
use codex_protocol::protocol::RawResponseItemEvent;
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
use futures::future::BoxFuture;

const CORRECTION_FRAME_ID_PREFIX: &str = "msg_correction_";
const CORRECTION_ACK_FRAME_ID_PREFIX: &str = "msg_correction_ack_";
const CORRECTION_ACK_TEXT_SUFFIX: &str = "was included in an earlier successful normal sampling \
step. Treat it as already-evaluated historical context, not a new instruction. Do not initiate or \
repeat an action solely because this correction frame is present after replay or rollback. \
Continue any unfinished work independently required by the surviving conversation state.";

pub(super) struct CorrectionSamplingInput {
    pub(super) input: Vec<ResponseItem>,
    pub(super) correction_ids: Vec<String>,
}

struct PendingCorrection {
    correction_id: String,
    stable_id: String,
    frame: ResponseItem,
    present_in_history: bool,
}

pub(super) fn correction_frame_stable_id(correction_id: Uuid) -> String {
    format!("{CORRECTION_FRAME_ID_PREFIX}{}", correction_id.simple())
}

fn correction_ack_frame_stable_id(correction_id: Uuid) -> String {
    format!("{CORRECTION_ACK_FRAME_ID_PREFIX}{}", correction_id.simple())
}

pub(super) fn correction_ack_frame_id(item: &ResponseItem) -> Option<Uuid> {
    let id = item.id()?;
    let uuid = id.strip_prefix(CORRECTION_ACK_FRAME_ID_PREFIX)?;
    Uuid::parse_str(uuid).ok()
}

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

pub(super) fn correction_frame(correction_id: Uuid, payload: String) -> ResponseItem {
    let stable_id = correction_frame_stable_id(correction_id);
    let fragment = AdditionalContextDeveloperFragment::new(stable_id.clone(), payload)
        .into_response_input_item();
    let mut frame = ResponseItem::from(fragment);
    frame.set_id(Some(stable_id));
    frame
}

pub(super) fn correction_ack_frame(correction_id: Uuid) -> ResponseItem {
    let stable_id = correction_ack_frame_stable_id(correction_id);
    let payload = format!("Correction {correction_id} {CORRECTION_ACK_TEXT_SUFFIX}");
    let fragment = AdditionalContextDeveloperFragment::new(stable_id.clone(), payload)
        .into_response_input_item();
    let mut frame = ResponseItem::from(fragment);
    frame.set_id(Some(stable_id));
    frame
}

pub(super) fn project_correction_acks(
    history: Vec<ResponseItem>,
    processed_correction_ids: &std::collections::HashSet<String>,
) -> Vec<ResponseItem> {
    let mut projected = Vec::with_capacity(history.len() + processed_correction_ids.len());
    for item in history {
        if correction_ack_frame_id(&item).is_some() {
            continue;
        }
        let correction_id = correction_frame_id(&item).and_then(|stable_id| {
            processed_correction_ids
                .contains(stable_id)
                .then(|| stable_id.strip_prefix(CORRECTION_FRAME_ID_PREFIX))
                .flatten()
                .and_then(|id| Uuid::parse_str(id).ok())
        });
        projected.push(item);
        if let Some(correction_id) = correction_id {
            projected.push(correction_ack_frame(correction_id));
        }
    }
    projected
}

enum ExistingCorrection {
    Missing,
    Unflushed,
    Queued,
    Sampled,
    Conflict,
}

impl Session {
    pub(super) fn validate_correction(
        correction_id: &str,
        payload: String,
    ) -> CodexResult<ResponseItem> {
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

    async fn classify_correction_intent(
        &self,
        intent: &CorrectionIntent,
        expected: &ResponseItem,
    ) -> CodexResult<ExistingCorrection> {
        let Some(stable_id) = correction_frame_id(expected) else {
            return Ok(ExistingCorrection::Conflict);
        };
        let state = self.state.lock().await;
        if !state
            .surviving_client_user_message_ids
            .contains(&intent.expected_client_user_message_id)
        {
            return Err(CodexErr::InvalidRequest(format!(
                "expectedClientUserMessageId `{}` does not identify a surviving user message",
                intent.expected_client_user_message_id
            )));
        }
        if state.correction_receipt_conflicts.contains(stable_id) {
            return Ok(ExistingCorrection::Conflict);
        }
        let existing_intent = state
            .correction_intents
            .iter()
            .find(|existing| existing.correction_id == intent.correction_id);
        if existing_intent.is_some_and(|existing| existing != intent) {
            return Ok(ExistingCorrection::Conflict);
        }
        if state
            .correction_receipts
            .get(stable_id)
            .is_some_and(|existing| !correction_frames_have_same_payload(existing, expected))
        {
            return Ok(ExistingCorrection::Conflict);
        }
        if existing_intent.is_some() {
            if state.processed_correction_ids.contains(stable_id) {
                Ok(ExistingCorrection::Sampled)
            } else if state.persisted_correction_intent_ids.contains(stable_id) {
                Ok(ExistingCorrection::Queued)
            } else {
                Ok(ExistingCorrection::Unflushed)
            }
        } else {
            Ok(ExistingCorrection::Missing)
        }
    }

    async fn reserve_new_correction_intent(&self, intent: &CorrectionIntent) {
        let mut state = self.state.lock().await;
        if !state
            .correction_intents
            .iter()
            .any(|existing| existing.correction_id == intent.correction_id)
        {
            state.correction_intents.push(intent.clone());
        }
    }

    async fn persist_correction_intent(
        &self,
        intent: &CorrectionIntent,
        stable_id: &str,
    ) -> CodexResult<()> {
        let live_thread = self
            .live_thread_for_persistence("queue correction intent")
            .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
        live_thread
            .append_items(&[RolloutItem::EventMsg(
                codex_protocol::protocol::EventMsg::RawResponseItem(
                    RawResponseItemEvent::correction_intent(intent.clone()),
                ),
            )])
            .await
            .map_err(|error| CodexErr::Io(io::Error::other(error)))?;
        live_thread
            .flush()
            .await
            .map_err(|error| CodexErr::Io(io::Error::other(error)))?;
        self.state
            .lock()
            .await
            .persisted_correction_intent_ids
            .insert(stable_id.to_string());
        Ok(())
    }

    async fn pending_corrections(&self) -> CodexResult<Vec<PendingCorrection>> {
        let state = self.state.lock().await;
        state
            .correction_intents
            .iter()
            .filter_map(|intent| {
                if !state
                    .surviving_client_user_message_ids
                    .contains(&intent.expected_client_user_message_id)
                {
                    return None;
                }
                let frame = match Self::validate_correction(
                    &intent.correction_id,
                    intent.payload.clone(),
                ) {
                    Ok(frame) => frame,
                    Err(error) => {
                        return Some(Err(CodexErr::Fatal(format!(
                            "durable correction `{}` is invalid: {error}",
                            intent.correction_id
                        ))));
                    }
                };
                let stable_id = correction_frame_id(&frame)?.to_string();
                if !state.persisted_correction_intent_ids.contains(&stable_id)
                    || state.correction_receipt_conflicts.contains(&stable_id)
                    || state.processed_correction_ids.contains(&stable_id)
                {
                    return None;
                }
                if state
                    .correction_receipts
                    .get(&stable_id)
                    .is_some_and(|receipt| !correction_frames_have_same_payload(receipt, &frame))
                {
                    return Some(Err(CodexErr::Fatal(format!(
                        "correction `{}` has a conflicting durable receipt",
                        intent.correction_id
                    ))));
                }
                let matching_history_frames = state
                    .history
                    .raw_items()
                    .iter()
                    .filter(|item| correction_frame_id(item) == Some(stable_id.as_str()))
                    .collect::<Vec<_>>();
                if matching_history_frames
                    .iter()
                    .any(|item| !correction_frames_have_same_payload(item, &frame))
                {
                    return Some(Err(CodexErr::Fatal(format!(
                        "correction `{}` conflicts with current model history",
                        intent.correction_id
                    ))));
                }
                Some(Ok(PendingCorrection {
                    correction_id: intent.correction_id.clone(),
                    stable_id,
                    frame,
                    present_in_history: !matching_history_frames.is_empty(),
                }))
            })
            .collect()
    }

    pub(crate) async fn has_pending_corrections(&self) -> bool {
        let state = self.state.lock().await;
        state.correction_intents.iter().any(|intent| {
            let Ok(correction_id) = Uuid::parse_str(&intent.correction_id) else {
                return false;
            };
            let stable_id = correction_frame_stable_id(correction_id);
            state
                .surviving_client_user_message_ids
                .contains(&intent.expected_client_user_message_id)
                && state.persisted_correction_intent_ids.contains(&stable_id)
                && !state.correction_receipt_conflicts.contains(&stable_id)
                && !state.processed_correction_ids.contains(&stable_id)
        })
    }

    pub(crate) async fn correction_auto_start_allowed(&self) -> bool {
        !self.state.lock().await.correction_auto_start_suppressed
    }

    pub(crate) async fn suppress_correction_auto_start(&self) {
        self.state.lock().await.correction_auto_start_suppressed = true;
    }

    pub(crate) async fn allow_correction_auto_start(&self) {
        self.state.lock().await.correction_auto_start_suppressed = false;
    }

    async fn materialize_pending_corrections(
        &self,
        turn_context: &super::TurnContext,
    ) -> CodexResult<Vec<PendingCorrection>> {
        let mut pending = self.pending_corrections().await?;
        let missing_frames = pending
            .iter()
            .filter(|correction| !correction.present_in_history)
            .map(|correction| correction.frame.clone())
            .collect::<Vec<_>>();
        if !missing_frames.is_empty() {
            let prepared = self
                .prepare_conversation_items_for_history(turn_context, missing_frames.as_slice())
                .into_owned();
            if prepared.len() != missing_frames.len() {
                return Err(CodexErr::Fatal(
                    "correction frame preparation produced an invalid item count".to_string(),
                ));
            }
            let live_thread = self
                .live_thread_for_persistence("materialize correction")
                .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
            let rollout_items = prepared
                .iter()
                .cloned()
                .map(RolloutItem::ResponseItem)
                .collect::<Vec<_>>();
            live_thread
                .append_items(rollout_items.as_slice())
                .await
                .map_err(|error| CodexErr::Io(io::Error::other(error)))?;
            live_thread
                .flush()
                .await
                .map_err(|error| CodexErr::Io(io::Error::other(error)))?;

            let mut state = self.state.lock().await;
            for prepared_frame in &prepared {
                state
                    .current_time_reminder
                    .note_recorded_items(std::slice::from_ref(prepared_frame));
                state.record_items(
                    std::slice::from_ref(prepared_frame).iter(),
                    turn_context.model_info.truncation_policy.into(),
                );
                let Some(stable_id) = correction_frame_id(prepared_frame) else {
                    return Err(CodexErr::Fatal(
                        "prepared correction frame lost its stable id".to_string(),
                    ));
                };
                state
                    .correction_receipts
                    .entry(stable_id.to_string())
                    .or_insert_with(|| prepared_frame.clone());
            }
            for correction in &mut pending {
                correction.present_in_history = true;
            }
        }
        Ok(pending)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "the active-turn lock linearizes correction acceptance with the exact prompt snapshot"
    )]
    pub(super) async fn sampling_input_with_pending_corrections(
        &self,
        turn_context: &super::TurnContext,
    ) -> CodexResult<CorrectionSamplingInput> {
        let active = self.active_turn.lock().await;
        let Some(active_turn) = active.as_ref() else {
            return Err(CodexErr::TurnAborted);
        };
        let Some(active_task) = active_turn.task.as_ref() else {
            return Err(CodexErr::TurnAborted);
        };
        if active_task.turn_context.sub_id != turn_context.sub_id {
            return Err(CodexErr::TurnAborted);
        }

        let pending = self.materialize_pending_corrections(turn_context).await?;
        self.input_queue
            .remove_committed_correction_markers_for_turn_state(active_turn.turn_state.as_ref())
            .await;
        let history = self.clone_history().await;
        let input = history.for_prompt(&turn_context.model_info.input_modalities);
        let prompt_correction_ids = input
            .iter()
            .filter_map(correction_frame_id)
            .collect::<std::collections::HashSet<_>>();
        let correction_ids = pending
            .iter()
            .filter(|correction| prompt_correction_ids.contains(correction.stable_id.as_str()))
            .map(|correction| correction.correction_id.clone())
            .collect();
        Ok(CorrectionSamplingInput {
            input,
            correction_ids,
        })
    }

    pub(crate) fn maybe_start_correction_turn_if_idle(self: &Arc<Self>) -> BoxFuture<'static, ()> {
        let session = Arc::clone(self);
        Box::pin(async move {
            if !session.correction_auto_start_allowed().await
                || !session.has_pending_corrections().await
            {
                return;
            }
            let Some(reservation) = session.reserve_task_start().await else {
                return;
            };
            if !session.correction_auto_start_allowed().await
                || !session.has_pending_corrections().await
            {
                session.release_task_start(&reservation).await;
                return;
            }

            let turn_context = session.new_correction_appendix().await;
            session
                .maybe_emit_model_warnings_for_turn(turn_context.as_ref())
                .await;
            assert!(
                session
                    .start_reserved_task(
                        reservation,
                        turn_context,
                        vec![TurnInput::CommittedCorrection],
                        RegularTask::new(),
                    )
                    .await,
                "correction task-start reservation must remain owned until task publication"
            );
        })
    }

    pub(crate) async fn persist_sampled_corrections(
        &self,
        correction_ids: &[String],
    ) -> CodexResult<()> {
        if correction_ids.is_empty() {
            return Ok(());
        }
        let mut canonical_ids = Vec::with_capacity(correction_ids.len());
        for correction_id in correction_ids {
            let correction_id = Uuid::parse_str(correction_id).map_err(|error| {
                CodexErr::Fatal(format!(
                    "sampled correction ID `{correction_id}` is invalid: {error}"
                ))
            })?;
            canonical_ids.push(correction_id.to_string());
        }

        let rollout_item =
            RolloutItem::EventMsg(codex_protocol::protocol::EventMsg::RawResponseItem(
                RawResponseItemEvent::corrections_sampled(canonical_ids.clone()),
            ));
        let live_thread = self
            .live_thread_for_persistence("record sampled corrections")
            .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
        live_thread
            .append_items(&[rollout_item])
            .await
            .map_err(|error| CodexErr::Io(io::Error::other(error)))?;
        live_thread
            .flush()
            .await
            .map_err(|error| CodexErr::Io(io::Error::other(error)))?;

        let mut state = self.state.lock().await;
        state.processed_correction_ids.extend(
            canonical_ids
                .iter()
                .filter_map(|correction_id| Uuid::parse_str(correction_id).ok())
                .map(correction_frame_stable_id),
        );
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
        expected_client_user_message_id: String,
        payload: String,
    ) -> CodexResult<CorrectionCommitStatus> {
        let correction_uuid = Uuid::parse_str(&correction_id).map_err(|error| {
            CodexErr::InvalidRequest(format!("correctionId must be a UUID: {error}"))
        })?;
        let correction_id = correction_uuid.to_string();
        let frame = Self::validate_correction(&correction_id, payload.clone())?;
        let Some(stable_id) = correction_frame_id(&frame).map(str::to_string) else {
            return Err(CodexErr::Fatal(
                "validated correction lost its stable id".to_string(),
            ));
        };
        let intent = CorrectionIntent {
            correction_id: correction_id.clone(),
            expected_client_user_message_id,
            payload,
        };

        let active = self.active_turn.lock().await;
        let wake_turn_state = active.as_ref().and_then(|turn| {
            turn.task
                .as_ref()
                .filter(|task| task.kind == TaskKind::Regular && task.accepts_steer)
                .map(|_| Arc::clone(&turn.turn_state))
        });
        let idle = active.is_none();

        let existing = self.classify_correction_intent(&intent, &frame).await?;
        let should_wake = !matches!(existing, ExistingCorrection::Queued);
        match existing {
            ExistingCorrection::Sampled => {
                return Ok(CorrectionCommitStatus::AlreadyCommitted);
            }
            ExistingCorrection::Conflict => {
                return Err(CodexErr::InvalidRequest(format!(
                    "correctionId `{correction_id}` was reused with different content or target"
                )));
            }
            ExistingCorrection::Queued => {}
            ExistingCorrection::Missing | ExistingCorrection::Unflushed => {
                if matches!(existing, ExistingCorrection::Missing) {
                    self.reserve_new_correction_intent(&intent).await;
                }
                self.persist_correction_intent(&intent, &stable_id).await?;
            }
        }

        if should_wake && let Some(turn_state) = wake_turn_state {
            self.input_queue
                .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                    turn_state.as_ref(),
                    vec![TurnInput::CommittedCorrection],
                )
                .await;
        }
        drop(active);
        if idle {
            self.maybe_start_correction_turn_if_idle().await;
        }
        Ok(CorrectionCommitStatus::Queued)
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

use super::*;

use super::tests::build_world_state_from_turn_context;
use super::tests::make_session_and_context;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::ResumedHistory;
use codex_protocol::protocol::SessionContextWindow;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::WorldStateItem;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::PathBuf;
use uuid::Uuid;

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn assistant_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn correction_message(id: Uuid, payload: &str) -> ResponseItem {
    correction::correction_frame(id, payload.to_string())
}

fn correction_intent(id: Uuid, target_client_id: &str, payload: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::RawResponseItem(
        codex_protocol::protocol::RawResponseItemEvent::correction_intent(
            codex_protocol::protocol::CorrectionIntent {
                correction_id: id.to_string(),
                expected_client_user_message_id: target_client_id.to_string(),
                payload: payload.to_string(),
            },
        ),
    ))
}

fn corrections_sampled(ids: &[Uuid]) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::RawResponseItem(
        codex_protocol::protocol::RawResponseItemEvent::corrections_sampled(
            ids.iter().map(Uuid::to_string).collect(),
        ),
    ))
}

fn turn_started(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(
        codex_protocol::protocol::TurnStartedEvent {
            turn_id: turn_id.to_string(),
            trace_id: None,
            started_at: None,
            model_context_window: Some(128_000),
            collaboration_mode_kind: ModeKind::Default,
        },
    ))
}

fn interrupted(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnAborted(
        codex_protocol::protocol::TurnAbortedEvent {
            turn_id: Some(turn_id.to_string()),
            reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
            completed_at: None,
            duration_ms: None,
        },
    ))
}

fn turn_complete(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(
        codex_protocol::protocol::TurnCompleteEvent {
            turn_id: turn_id.to_string(),
            last_agent_message: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        },
    ))
}

fn completed_client_user_message(turn_id: &str, client_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(
        codex_protocol::protocol::ItemCompletedEvent {
            thread_id: ThreadId::default(),
            turn_id: turn_id.to_string(),
            item: codex_protocol::items::TurnItem::UserMessage(
                codex_protocol::items::UserMessageItem {
                    id: format!("item-{client_id}"),
                    client_id: Some(client_id.to_string()),
                    content: Vec::new(),
                },
            ),
            completed_at_ms: 0,
        },
    ))
}

fn legacy_client_user_message(client_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::UserMessage(
        codex_protocol::protocol::UserMessageEvent {
            client_id: Some(client_id.to_string()),
            message: "legacy user projection".to_string(),
            ..Default::default()
        },
    ))
}

fn inter_agent_message(text: &str) -> InterAgentCommunication {
    InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root().join("worker").expect("worker path"),
        Vec::new(),
        text.to_string(),
        /*trigger_turn*/ true,
    )
}

#[test]
fn surviving_client_message_projection_rolls_back_only_the_latest_user_boundary() {
    let rollout_items = vec![
        turn_started("turn-1"),
        RolloutItem::ResponseItem(user_message("initial")),
        completed_client_user_message("turn-1", "initial-client-id"),
        RolloutItem::ResponseItem(user_message("steer")),
        completed_client_user_message("turn-1", "steer-client-id"),
        RolloutItem::Compacted(CompactedItem {
            message: "summary".to_string(),
            replacement_history: Some(vec![assistant_message("compacted")]),
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
    ];

    assert_eq!(
        rollout_reconstruction::reconstruct_surviving_client_user_message_ids(&rollout_items),
        HashSet::from(["initial-client-id".to_string()])
    );
}

#[test]
fn surviving_client_message_projection_ignores_compaction_and_non_user_turns() {
    let rollout_items = vec![
        turn_started("turn-1"),
        RolloutItem::ResponseItem(user_message("initial")),
        completed_client_user_message("turn-1", "surviving-client-id"),
        RolloutItem::Compacted(CompactedItem {
            message: "summary without raw client ID".to_string(),
            replacement_history: Some(vec![assistant_message("summary")]),
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
        turn_started("correction-only-turn"),
    ];

    assert_eq!(
        rollout_reconstruction::reconstruct_surviving_client_user_message_ids(&rollout_items),
        HashSet::from(["surviving-client-id".to_string()])
    );
}

#[test]
fn surviving_client_message_projection_pairs_legacy_user_event() {
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("initial")),
        legacy_client_user_message("legacy-client-id"),
    ];

    assert_eq!(
        rollout_reconstruction::reconstruct_surviving_client_user_message_ids(&rollout_items),
        HashSet::from(["legacy-client-id".to_string()])
    );
}

#[test]
fn surviving_client_message_projection_counts_metadata_and_agent_message_once() {
    let communication = inter_agent_message("follow-up task");
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("initial")),
        completed_client_user_message("turn-1", "initial-client-id"),
        RolloutItem::InterAgentCommunicationMetadata { trigger_turn: true },
        RolloutItem::ResponseItem(communication.to_model_input_item()),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
    ];

    assert_eq!(
        rollout_reconstruction::reconstruct_surviving_client_user_message_ids(&rollout_items),
        HashSet::from(["initial-client-id".to_string()])
    );
}

#[test]
fn surviving_client_message_projection_counts_typed_inter_agent_boundary() {
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("initial")),
        completed_client_user_message("turn-1", "initial-client-id"),
        RolloutItem::InterAgentCommunication(inter_agent_message("legacy follow-up task")),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
    ];

    assert_eq!(
        rollout_reconstruction::reconstruct_surviving_client_user_message_ids(&rollout_items),
        HashSet::from(["initial-client-id".to_string()])
    );
}

#[tokio::test]
async fn correction_stop_latch_replay_clears_on_surviving_real_work() {
    let (session, turn_context) = make_session_and_context().await;
    let legacy_review = RolloutItem::EventMsg(EventMsg::EnteredReviewMode(
        codex_protocol::protocol::EnteredReviewModeEvent {
            target: codex_protocol::protocol::ReviewTarget::Custom {
                instructions: "review".to_string(),
            },
            user_facing_hint: None,
            turn_id: Some("review-legacy".to_string()),
            item_id: Some("review-item-legacy".to_string()),
        },
    ));
    let paginated_review = RolloutItem::EventMsg(EventMsg::ItemCompleted(
        codex_protocol::protocol::ItemCompletedEvent {
            thread_id: ThreadId::default(),
            turn_id: "review-paginated".to_string(),
            item: codex_protocol::items::TurnItem::EnteredReviewMode(
                codex_protocol::items::EnteredReviewModeItem {
                    id: "review-item-paginated".to_string(),
                    target: codex_protocol::protocol::ReviewTarget::Custom {
                        instructions: "review".to_string(),
                    },
                    user_facing_hint: String::new(),
                },
            ),
            completed_at_ms: 0,
        },
    ));

    let suppressed = session
        .reconstruct_history_from_rollout(&turn_context, &[interrupted("stopped")])
        .await;
    assert!(suppressed.correction_auto_start_suppressed);

    for resumed in [
        turn_started("started"),
        turn_complete("completed"),
        legacy_review,
        paginated_review,
    ] {
        let reconstructed = session
            .reconstruct_history_from_rollout(&turn_context, &[interrupted("stopped"), resumed])
            .await;
        assert!(
            !reconstructed.correction_auto_start_suppressed,
            "surviving real-work lifecycle must clear replayed Stop suppression"
        );
    }
}

#[tokio::test]
async fn correction_intent_carrier_is_not_model_history() {
    let (session, turn_context) = make_session_and_context().await;
    let user = user_message("dictated text");
    let carrier = RolloutItem::EventMsg(EventMsg::RawResponseItem(
        codex_protocol::protocol::RawResponseItemEvent::correction_intent(
            codex_protocol::protocol::CorrectionIntent {
                correction_id: "b7754d6f-f4df-4cfe-8621-8b735d348fb3".to_string(),
                expected_client_user_message_id: "client-user-1".to_string(),
                payload: "Tori should be Tauri".to_string(),
            },
        ),
    ));

    let reconstructed = session
        .reconstruct_history_from_rollout(
            &turn_context,
            &[RolloutItem::ResponseItem(user.clone()), carrier],
        )
        .await;

    assert_eq!(reconstructed.history, vec![user]);
}

#[tokio::test]
async fn correction_intent_replay_recovers_pending_target_without_model_leakage() {
    let (session, turn_context) = make_session_and_context().await;
    let correction_id = Uuid::new_v4();
    let user = user_message("dictated text");
    let rollout_items = vec![
        RolloutItem::ResponseItem(user.clone()),
        completed_client_user_message("turn-1", "target-client-id"),
        correction_intent(correction_id, "target-client-id", "Tori should be Tauri"),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.surviving_client_user_message_ids,
        HashSet::from(["target-client-id".to_string()])
    );
    assert_eq!(
        reconstructed.correction_intents,
        vec![codex_protocol::protocol::CorrectionIntent {
            correction_id: correction_id.to_string(),
            expected_client_user_message_id: "target-client-id".to_string(),
            payload: "Tori should be Tauri".to_string(),
        }]
    );
    assert!(reconstructed.correction_receipts.is_empty());
    assert_eq!(reconstructed.history, vec![user]);
}

#[tokio::test]
async fn correction_replay_retains_target_anchored_frame_across_unrelated_rollback() {
    let (session, turn_context) = make_session_and_context().await;
    let correction_id = Uuid::new_v4();
    let initial = user_message("initial dictated text");
    let later = user_message("later steer");
    let correction = correction_message(correction_id, "Tori should be Tauri");
    let after_rollback = user_message("input after rollback");
    let rollout_items = vec![
        RolloutItem::ResponseItem(initial.clone()),
        completed_client_user_message("turn-1", "target-client-id"),
        RolloutItem::ResponseItem(later),
        completed_client_user_message("turn-1", "later-client-id"),
        correction_intent(correction_id, "target-client-id", "Tori should be Tauri"),
        RolloutItem::ResponseItem(correction.clone()),
        RolloutItem::ResponseItem(assistant_message(
            "A model response that belongs to the rolled-back input.",
        )),
        corrections_sampled(&[correction_id]),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
        RolloutItem::ResponseItem(after_rollback.clone()),
        completed_client_user_message("turn-3", "after-rollback-client-id"),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.surviving_client_user_message_ids,
        HashSet::from([
            "target-client-id".to_string(),
            "after-rollback-client-id".to_string(),
        ])
    );
    assert_eq!(
        reconstructed
            .correction_receipts
            .get(correction.id().expect("correction stable id")),
        Some(&correction)
    );
    assert_eq!(
        reconstructed.processed_correction_ids,
        HashSet::from([correction.id().expect("correction stable id").to_string()])
    );
    assert_eq!(
        reconstructed.history,
        vec![initial.clone(), correction.clone(), after_rollback.clone()]
    );
    assert_eq!(
        correction::project_correction_acks(
            reconstructed.history.clone(),
            &reconstructed.processed_correction_ids,
        ),
        vec![
            initial,
            correction,
            correction::correction_ack_frame(correction_id),
            after_rollback,
        ]
    );
}

#[tokio::test]
async fn session_clone_history_projects_ack_without_mutating_canonical_history_or_rollout() {
    let (session, turn_context) = make_session_and_context().await;
    let correction_id = Uuid::new_v4();
    let initial = user_message("initial dictated text");
    let later = user_message("later steer");
    let correction = correction_message(correction_id, "Tori should be Tauri");
    let after_rollback = user_message("input after rollback");
    let rollout_items = vec![
        RolloutItem::ResponseItem(initial.clone()),
        completed_client_user_message("turn-1", "target-client-id"),
        RolloutItem::ResponseItem(later),
        completed_client_user_message("turn-2", "later-client-id"),
        correction_intent(correction_id, "target-client-id", "Tori should be Tauri"),
        RolloutItem::ResponseItem(correction.clone()),
        corrections_sampled(&[correction_id]),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
        RolloutItem::ResponseItem(after_rollback.clone()),
        completed_client_user_message("turn-3", "after-rollback-client-id"),
    ];

    session
        .apply_rollout_reconstruction(&turn_context, &rollout_items)
        .await;

    let canonical = session.state.lock().await.clone_history();
    assert_eq!(
        canonical.raw_items(),
        &[initial.clone(), correction.clone(), after_rollback.clone()]
    );
    assert!(
        rollout_items.iter().all(|item| {
            !matches!(
                item,
                RolloutItem::ResponseItem(response)
                    if correction::correction_ack_frame_id(response).is_some()
            )
        }),
        "the rollout must never contain a projected acknowledgement"
    );
    assert_eq!(
        session.clone_history().await.raw_items(),
        &[
            initial,
            correction,
            correction::correction_ack_frame(correction_id),
            after_rollback,
        ]
    );
}

#[tokio::test]
async fn session_clone_history_keeps_sampled_compaction_summary_canonical() {
    let (session, turn_context) = make_session_and_context().await;
    let correction_id = Uuid::new_v4();
    let correction = correction_message(correction_id, "Tori should be Tauri");
    let summary = assistant_message("The terminology correction was incorporated.");
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("dictated text")),
        completed_client_user_message("turn-1", "target-client-id"),
        correction_intent(correction_id, "target-client-id", "Tori should be Tauri"),
        RolloutItem::ResponseItem(correction),
        corrections_sampled(&[correction_id]),
        RolloutItem::Compacted(CompactedItem {
            message: "compacted".to_string(),
            replacement_history: Some(vec![summary.clone()]),
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
    ];

    session
        .apply_rollout_reconstruction(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        session.state.lock().await.clone_history().raw_items(),
        std::slice::from_ref(&summary)
    );
    assert_eq!(session.clone_history().await.raw_items(), &[summary]);
    assert!(!session.has_pending_corrections().await);
}

#[tokio::test]
async fn correction_replay_drops_intent_and_frame_when_target_is_rolled_back() {
    let (session, turn_context) = make_session_and_context().await;
    let correction_id = Uuid::new_v4();
    let correction = correction_message(correction_id, "Tori should be Tauri");
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("dictated text")),
        completed_client_user_message("turn-1", "target-client-id"),
        correction_intent(correction_id, "target-client-id", "Tori should be Tauri"),
        RolloutItem::ResponseItem(correction),
        corrections_sampled(&[correction_id]),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert!(reconstructed.surviving_client_user_message_ids.is_empty());
    assert_eq!(reconstructed.correction_intents.len(), 1);
    assert!(reconstructed.correction_receipts.is_empty());
    assert!(reconstructed.processed_correction_ids.is_empty());
    assert!(reconstructed.history.is_empty());
}

#[tokio::test]
async fn correction_appendix_replay_rolls_back_with_target_turn() {
    let (session, turn_context) = make_session_and_context().await;
    let correction_id = Uuid::new_v4();
    let target = user_message("Use Tori for the Rust app.");
    let target_result = assistant_message("I will use Tori.");
    let correction = correction_message(correction_id, "Tori should be Tauri");
    let appendix_result = assistant_message("I incorporated the Tauri correction.");
    let rollout_items = vec![
        turn_started("target-turn"),
        RolloutItem::ResponseItem(target),
        completed_client_user_message("target-turn", "target-client-id"),
        RolloutItem::ResponseItem(target_result),
        turn_complete("target-turn"),
        correction_intent(correction_id, "target-client-id", "Tori should be Tauri"),
        RolloutItem::ResponseItem(correction),
        RolloutItem::ResponseItem(appendix_result),
        corrections_sampled(&[correction_id]),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert!(reconstructed.history.is_empty());
    assert!(reconstructed.surviving_client_user_message_ids.is_empty());
    assert!(reconstructed.correction_receipts.is_empty());
    assert!(reconstructed.processed_correction_ids.is_empty());
}

#[tokio::test]
async fn correction_replay_drops_physical_frame_when_carrier_target_is_missing() {
    let (session, turn_context) = make_session_and_context().await;
    let correction_id = Uuid::new_v4();
    let rollout_items = vec![
        correction_intent(correction_id, "missing-client-id", "Tori should be Tauri"),
        RolloutItem::ResponseItem(correction_message(correction_id, "Tori should be Tauri")),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(reconstructed.correction_intents.len(), 1);
    assert!(reconstructed.correction_receipts.is_empty());
    assert!(reconstructed.history.is_empty());
}

#[tokio::test]
async fn correction_replay_leaves_unprocessed_summarized_frame_pending_for_rematerialization() {
    let (session, turn_context) = make_session_and_context().await;
    let correction_id = Uuid::new_v4();
    let correction = correction_message(correction_id, "Tori should be Tauri");
    let summary = assistant_message("The terminology correction was incorporated.");
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("dictated text")),
        completed_client_user_message("turn-1", "target-client-id"),
        correction_intent(correction_id, "target-client-id", "Tori should be Tauri"),
        RolloutItem::ResponseItem(correction.clone()),
        RolloutItem::Compacted(CompactedItem {
            message: "compacted".to_string(),
            replacement_history: Some(vec![summary.clone()]),
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed
            .correction_receipts
            .get(correction.id().expect("correction stable id")),
        Some(&correction)
    );
    assert!(reconstructed.processed_correction_ids.is_empty());
    assert_eq!(reconstructed.history, vec![summary.clone()]);
}

#[tokio::test]
async fn correction_replay_preserves_sampled_proof_across_surviving_compaction() {
    let (session, turn_context) = make_session_and_context().await;
    let correction_id = Uuid::new_v4();
    let correction = correction_message(correction_id, "Tori should be Tauri");
    let summary = assistant_message("The terminology correction was incorporated.");
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("dictated text")),
        completed_client_user_message("turn-1", "target-client-id"),
        correction_intent(correction_id, "target-client-id", "Tori should be Tauri"),
        RolloutItem::ResponseItem(correction.clone()),
        corrections_sampled(&[correction_id]),
        RolloutItem::Compacted(CompactedItem {
            message: "compacted".to_string(),
            replacement_history: Some(vec![summary.clone()]),
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.processed_correction_ids,
        HashSet::from([correction.id().expect("correction stable id").to_string()])
    );
    assert_eq!(reconstructed.history, vec![summary.clone()]);
    assert_eq!(
        correction::project_correction_acks(
            reconstructed.history.clone(),
            &reconstructed.processed_correction_ids,
        ),
        vec![summary]
    );
}

#[tokio::test]
async fn correction_replay_without_sampled_marker_remains_unprocessed_after_abort() {
    let (session, turn_context) = make_session_and_context().await;
    let correction_id = Uuid::new_v4();
    let correction = correction_message(correction_id, "Tori should be Tauri");
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("dictated text")),
        completed_client_user_message("turn-1", "target-client-id"),
        correction_intent(correction_id, "target-client-id", "Tori should be Tauri"),
        RolloutItem::ResponseItem(correction),
        RolloutItem::EventMsg(EventMsg::TurnAborted(
            codex_protocol::protocol::TurnAbortedEvent {
                turn_id: Some("turn-1".to_string()),
                reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
                completed_at: None,
                duration_ms: None,
            },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert!(reconstructed.processed_correction_ids.is_empty());
}

#[tokio::test]
async fn correction_ack_without_sampled_proof_is_removed_from_model_projection() {
    let (session, turn_context) = make_session_and_context().await;
    let correction_id = Uuid::new_v4();
    let user = user_message("dictated text");
    let correction = correction_message(correction_id, "Tori should be Tauri");
    let rollout_items = vec![
        RolloutItem::ResponseItem(user.clone()),
        completed_client_user_message("turn-1", "target-client-id"),
        correction_intent(correction_id, "target-client-id", "Tori should be Tauri"),
        RolloutItem::ResponseItem(correction.clone()),
        RolloutItem::ResponseItem(correction::correction_ack_frame(correction_id)),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert!(reconstructed.processed_correction_ids.is_empty());
    assert_eq!(
        correction::project_correction_acks(
            reconstructed.history,
            &reconstructed.processed_correction_ids,
        ),
        vec![user, correction]
    );
}

#[tokio::test]
async fn correction_replay_preserves_existing_eligible_frame_chronology() {
    let (session, turn_context) = make_session_and_context().await;
    let correction_id = Uuid::new_v4();
    let initial = user_message("initial dictated text");
    let correction = correction_message(correction_id, "Tori should be Tauri");
    let assistant = assistant_message("I incorporated the correction.");
    let later = user_message("later user input");
    let rollout_items = vec![
        RolloutItem::ResponseItem(initial.clone()),
        completed_client_user_message("turn-1", "target-client-id"),
        correction_intent(correction_id, "target-client-id", "Tori should be Tauri"),
        RolloutItem::ResponseItem(correction.clone()),
        RolloutItem::ResponseItem(assistant.clone()),
        RolloutItem::ResponseItem(later.clone()),
        completed_client_user_message("turn-2", "later-client-id"),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.history,
        vec![initial, correction, assistant, later]
    );
}

#[tokio::test]
async fn correction_intent_replay_preserves_multiple_pending_intent_order() {
    let (session, turn_context) = make_session_and_context().await;
    let first_id = Uuid::new_v4();
    let second_id = Uuid::new_v4();
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("dictated text")),
        completed_client_user_message("turn-1", "target-client-id"),
        correction_intent(first_id, "target-client-id", "first correction"),
        correction_intent(second_id, "target-client-id", "second correction"),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed
            .correction_intents
            .iter()
            .map(|intent| intent.correction_id.as_str())
            .collect::<Vec<_>>(),
        vec![first_id.to_string(), second_id.to_string()]
    );
}

#[tokio::test]
async fn correction_intent_identity_conflict_survives_target_rollback() {
    let (session, turn_context) = make_session_and_context().await;
    let correction_id = Uuid::new_v4();
    let first_intent =
        correction_intent(correction_id, "rolled-target-client-id", "first correction");
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("rolled target")),
        completed_client_user_message("turn-1", "rolled-target-client-id"),
        first_intent,
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
        RolloutItem::ResponseItem(user_message("surviving target")),
        completed_client_user_message("turn-2", "surviving-target-client-id"),
        correction_intent(
            correction_id,
            "surviving-target-client-id",
            "changed correction",
        ),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;
    let stable_id = correction::correction_frame_stable_id(correction_id);

    assert_eq!(reconstructed.correction_intents.len(), 1);
    assert_eq!(
        reconstructed.correction_intents[0].expected_client_user_message_id,
        "rolled-target-client-id"
    );
    assert!(
        reconstructed
            .correction_receipt_conflicts
            .contains(&stable_id)
    );
    assert!(
        reconstructed
            .correction_integrity_failure
            .as_deref()
            .is_some_and(
                |message| message.contains("correction identity reused with different data")
            )
    );
}

#[tokio::test]
async fn correction_integrity_failure_reports_invalid_durable_intent() {
    let (session, turn_context) = make_session_and_context().await;
    let invalid_intent = RolloutItem::EventMsg(EventMsg::RawResponseItem(
        codex_protocol::protocol::RawResponseItemEvent::correction_intent(
            codex_protocol::protocol::CorrectionIntent {
                correction_id: "not-a-uuid".to_string(),
                expected_client_user_message_id: "target-client-id".to_string(),
                payload: "Tori should be Tauri".to_string(),
            },
        ),
    ));
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("dictated text")),
        completed_client_user_message("turn-1", "target-client-id"),
        invalid_intent,
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert!(reconstructed.correction_intents.is_empty());
    assert!(
        reconstructed
            .correction_integrity_failure
            .as_deref()
            .is_some_and(|message| message.contains("invalid durable intent"))
    );
}

#[tokio::test]
async fn correction_integrity_failure_includes_replacement_history_conflict() {
    let (session, turn_context) = make_session_and_context().await;
    let correction_id = Uuid::new_v4();
    let first = correction_message(correction_id, "Tori should be Tauri");
    let conflicting = correction_message(correction_id, "Tori should be Torii");
    let stable_id = correction::correction_frame_stable_id(correction_id);
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("dictated text")),
        completed_client_user_message("turn-1", "target-client-id"),
        correction_intent(correction_id, "target-client-id", "Tori should be Tauri"),
        RolloutItem::ResponseItem(first.clone()),
        corrections_sampled(&[correction_id]),
        RolloutItem::Compacted(CompactedItem {
            message: "compacted".to_string(),
            replacement_history: Some(vec![first, conflicting]),
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert!(
        reconstructed
            .correction_receipt_conflicts
            .contains(&stable_id)
    );
    assert!(!reconstructed.processed_correction_ids.contains(&stable_id));
    assert!(
        reconstructed
            .history
            .iter()
            .all(|item| correction::correction_frame_id(item).is_none())
    );
    assert!(
        reconstructed
            .correction_integrity_failure
            .as_deref()
            .is_some_and(|message| message.contains("conflicting reconstructed correction frame"))
    );
}

#[test]
fn correction_receipt_replay_is_compaction_independent_and_semantically_idempotent() {
    let id = Uuid::new_v4();
    let correction = correction_message(id, "replace parakeat with Parakeet");
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("dictated text")),
        RolloutItem::ResponseItem(correction.clone()),
        RolloutItem::Compacted(CompactedItem {
            message: "compacted".to_string(),
            replacement_history: Some(vec![user_message("summary without correction frame")]),
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
        RolloutItem::ResponseItem(correction.clone()),
    ];

    let (receipts, conflicts) =
        rollout_reconstruction::reconstruct_correction_receipts(&rollout_items);
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts.get(correction.id().unwrap()), Some(&correction));
    assert!(conflicts.is_empty());

    let mut history_conflicts = HashSet::new();
    let deduplicated = rollout_reconstruction::deduplicate_correction_frames(
        vec![correction.clone(), correction],
        &mut history_conflicts,
    );
    assert_eq!(deduplicated.len(), 1);
    assert!(history_conflicts.is_empty());
}

#[test]
fn correction_receipt_replay_conflicts_only_for_surviving_frames() {
    let id = Uuid::new_v4();
    let first = correction_message(id, "first payload");
    let second = correction_message(id, "different payload");
    let surviving = vec![
        RolloutItem::ResponseItem(user_message("dictated text")),
        RolloutItem::ResponseItem(first.clone()),
        RolloutItem::ResponseItem(second),
    ];
    let (_, conflicts) = rollout_reconstruction::reconstruct_correction_receipts(&surviving);
    assert_eq!(conflicts, HashSet::from([first.id().unwrap().to_string()]));

    let rolled_back = surviving
        .into_iter()
        .chain(std::iter::once(RolloutItem::EventMsg(
            EventMsg::ThreadRolledBack(codex_protocol::protocol::ThreadRolledBackEvent {
                num_turns: 1,
            }),
        )))
        .collect::<Vec<_>>();
    let (receipts, conflicts) =
        rollout_reconstruction::reconstruct_correction_receipts(&rolled_back);
    assert!(receipts.is_empty());
    assert!(conflicts.is_empty());
}

#[test]
fn correction_receipt_replay_restores_surviving_segment_after_rollback() {
    let id = Uuid::new_v4();
    let correction = correction_message(id, "payload");
    let rollback = || {
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        ))
    };
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("u1")),
        RolloutItem::ResponseItem(user_message("u2")),
        RolloutItem::ResponseItem(correction.clone()),
        rollback(),
        RolloutItem::ResponseItem(correction),
        rollback(),
    ];

    let (receipts, conflicts) =
        rollout_reconstruction::reconstruct_correction_receipts(&rollout_items);
    assert!(receipts.is_empty());
    assert!(conflicts.is_empty());
}

#[tokio::test]
async fn reconstruct_history_rollback_then_reapply_keeps_one_correction_frame_and_receipt() {
    let (session, turn_context) = make_session_and_context().await;
    let id = Uuid::new_v4();
    let correction = correction_message(id, "replace parakeat with Parakeet");
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("dictated text")),
        RolloutItem::ResponseItem(correction.clone()),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
        RolloutItem::ResponseItem(correction.clone()),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;
    assert_eq!(reconstructed.history, vec![correction.clone()]);
    assert_eq!(
        reconstructed
            .correction_receipts
            .get(correction.id().unwrap()),
        Some(&correction)
    );
    assert!(reconstructed.correction_receipt_conflicts.is_empty());
}

fn inter_agent_assistant_message(text: &str) -> ResponseItem {
    let communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root().join("worker").unwrap(),
        Vec::new(),
        text.to_string(),
        /*trigger_turn*/ true,
    );
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: serde_json::to_string(&communication).unwrap(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn completed_user_turn_rollout(
    turn_context_item: TurnContextItem,
    items: Vec<RolloutItem>,
) -> Vec<RolloutItem> {
    let turn_id = turn_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let mut rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(turn_context_item),
    ];
    rollout_items.extend(items);
    rollout_items.push(RolloutItem::EventMsg(EventMsg::TurnComplete(
        codex_protocol::protocol::TurnCompleteEvent {
            turn_id,
            last_agent_message: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        },
    )));
    rollout_items
}

#[tokio::test]
async fn record_initial_history_reconstructs_typed_inter_agent_message() {
    let (session, _turn_context) = make_session_and_context().await;
    let communication = InterAgentCommunication::new(
        AgentPath::root().join("worker").expect("worker path"),
        AgentPath::root(),
        Vec::new(),
        "child done".to_string(),
        /*trigger_turn*/ false,
    );

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(vec![RolloutItem::InterAgentCommunication(
                communication.clone(),
            )]),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(
        session.state.lock().await.clone_history().raw_items(),
        &[communication.to_model_input_item()]
    );
}

#[tokio::test]
async fn record_initial_history_restores_world_state_baseline() {
    let (session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let world_state = build_world_state_from_turn_context(&session, &turn_context).await;
    let rollout_items = completed_user_turn_rollout(
        turn_context.to_turn_context_item(),
        vec![RolloutItem::WorldState(WorldStateItem::full(
            world_state.snapshot().into_value(),
        ))],
    );

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    session
        .record_context_updates_and_set_reference_context_item(&step_context)
        .await;

    assert_eq!(session.clone_history().await.raw_items(), &[]);
}

#[tokio::test]
async fn record_initial_history_resumed_bare_turn_context_does_not_hydrate_previous_turn_settings()
{
    let (session, turn_context) = make_session_and_context().await;
    let previous_model = "previous-rollout-model";
    let previous_context_item = TurnContextItem {
        turn_id: Some(turn_context.sub_id.clone()),
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy.value(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: previous_model.to_string(),
        comp_hash: None,
        personality: turn_context.personality,
        collaboration_mode: Some(turn_context.collaboration_mode.clone()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        effort: turn_context.reasoning_effort.clone(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };
    let rollout_items = vec![RolloutItem::TurnContext(previous_context_item)];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;
    assert_eq!(reconstructed.world_state_baseline, None);

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(session.previous_turn_settings().await, None);
    assert!(session.reference_context_item().await.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_hydrates_previous_turn_settings_from_lifecycle_turn_with_missing_turn_context_id()
 {
    let (session, turn_context) = make_session_and_context().await;
    let previous_model = "previous-rollout-model";
    let mut previous_context_item = TurnContextItem {
        turn_id: Some(turn_context.sub_id.clone()),
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy.value(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: previous_model.to_string(),
        comp_hash: Some("comp-hash-a".to_string()),
        personality: turn_context.personality,
        collaboration_mode: Some(turn_context.collaboration_mode.clone()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        effort: turn_context.reasoning_effort.clone(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };
    let turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    previous_context_item.turn_id = None;

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: previous_model.to_string(),
            comp_hash: Some("comp-hash-a".to_string()),
            realtime_active: Some(turn_context.realtime_active),
        })
    );
}

#[tokio::test]
async fn reconstruct_history_rollback_keeps_history_and_metadata_in_sync_for_completed_turns() {
    let (session, turn_context) = make_session_and_context().await;
    let first_context_item = turn_context.to_turn_context_item();
    let first_turn_id = first_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let mut rolled_back_context_item = first_context_item.clone();
    rolled_back_context_item.turn_id = Some("rolled-back-turn".to_string());
    rolled_back_context_item.model = "rolled-back-model".to_string();
    let rolled_back_turn_id = rolled_back_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let turn_one_user = user_message("turn 1 user");
    let turn_one_assistant = assistant_message("turn 1 assistant");
    let turn_two_user = user_message("turn 2 user");
    let turn_two_assistant = assistant_message("turn 2 assistant");

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: first_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 1 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(first_context_item.clone()),
        RolloutItem::WorldState(WorldStateItem::full(json!({
            "test": {"environment": "first"}
        }))),
        RolloutItem::ResponseItem(turn_one_user.clone()),
        RolloutItem::ResponseItem(turn_one_assistant.clone()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: first_turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: rolled_back_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 2 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(rolled_back_context_item),
        RolloutItem::WorldState(WorldStateItem::patch(json!({
            "test": {"environment": "rolled-back"}
        }))),
        RolloutItem::ResponseItem(turn_two_user),
        RolloutItem::ResponseItem(turn_two_assistant),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: rolled_back_turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.history,
        vec![turn_one_user, turn_one_assistant]
    );
    assert_eq!(
        reconstructed.previous_turn_settings,
        Some(PreviousTurnSettings {
            model: turn_context.model_info.slug.clone(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(reconstructed.reference_context_item)
            .expect("serialize reconstructed reference context item"),
        serde_json::to_value(Some(first_context_item))
            .expect("serialize expected reference context item")
    );
    assert_eq!(
        serde_json::to_value(reconstructed.world_state_baseline)
            .expect("serialize reconstructed world state"),
        json!({"test": {"environment": "first"}})
    );
}

#[tokio::test]
async fn reconstruct_history_rollback_keeps_history_and_metadata_in_sync_for_incomplete_turn() {
    let (session, turn_context) = make_session_and_context().await;
    let first_context_item = turn_context.to_turn_context_item();
    let first_turn_id = first_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let incomplete_turn_id = "incomplete-rolled-back-turn".to_string();
    let turn_one_user = user_message("turn 1 user");
    let turn_one_assistant = assistant_message("turn 1 assistant");
    let turn_two_user = user_message("turn 2 user");

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: first_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 1 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(first_context_item.clone()),
        RolloutItem::ResponseItem(turn_one_user.clone()),
        RolloutItem::ResponseItem(turn_one_assistant.clone()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: first_turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: incomplete_turn_id,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 2 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::ResponseItem(turn_two_user),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.history,
        vec![turn_one_user, turn_one_assistant]
    );
    assert_eq!(
        reconstructed.previous_turn_settings,
        Some(PreviousTurnSettings {
            model: turn_context.model_info.slug.clone(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(reconstructed.reference_context_item)
            .expect("serialize reconstructed reference context item"),
        serde_json::to_value(Some(first_context_item))
            .expect("serialize expected reference context item")
    );
}

#[tokio::test]
async fn reconstruct_history_rollback_skips_non_user_turns_for_history_and_metadata() {
    let (session, turn_context) = make_session_and_context().await;
    let first_context_item = turn_context.to_turn_context_item();
    let first_turn_id = first_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let second_turn_id = "rolled-back-user-turn".to_string();
    let standalone_turn_id = "standalone-turn".to_string();
    let turn_one_user = user_message("turn 1 user");
    let turn_one_assistant = assistant_message("turn 1 assistant");
    let turn_two_user = user_message("turn 2 user");
    let turn_two_assistant = assistant_message("turn 2 assistant");
    let standalone_assistant = assistant_message("standalone assistant");

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: first_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 1 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(first_context_item.clone()),
        RolloutItem::ResponseItem(turn_one_user.clone()),
        RolloutItem::ResponseItem(turn_one_assistant.clone()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: first_turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: second_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 2 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::ResponseItem(turn_two_user),
        RolloutItem::ResponseItem(turn_two_assistant),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: second_turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: standalone_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::ResponseItem(standalone_assistant),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: standalone_turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.history,
        vec![turn_one_user, turn_one_assistant]
    );
    assert_eq!(
        reconstructed.previous_turn_settings,
        Some(PreviousTurnSettings {
            model: turn_context.model_info.slug.clone(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(reconstructed.reference_context_item)
            .expect("serialize reconstructed reference context item"),
        serde_json::to_value(Some(first_context_item))
            .expect("serialize expected reference context item")
    );
}

#[tokio::test]
async fn reconstruct_history_rollback_counts_inter_agent_assistant_turns() {
    let (session, turn_context) = make_session_and_context().await;
    let first_context_item = turn_context.to_turn_context_item();
    let first_turn_id = first_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let assistant_turn_id = "assistant-instruction-turn".to_string();
    let assistant_turn_context = TurnContextItem {
        turn_id: Some(assistant_turn_id.clone()),
        ..first_context_item.clone()
    };
    let assistant_instruction = inter_agent_assistant_message("continue");
    let assistant_reply = assistant_message("worker reply");

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: first_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 1 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(first_context_item.clone()),
        RolloutItem::ResponseItem(user_message("turn 1 user")),
        RolloutItem::ResponseItem(assistant_message("turn 1 assistant")),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: first_turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: assistant_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::TurnContext(assistant_turn_context),
        RolloutItem::ResponseItem(assistant_instruction),
        RolloutItem::ResponseItem(assistant_reply),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: assistant_turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.history,
        vec![
            user_message("turn 1 user"),
            assistant_message("turn 1 assistant")
        ]
    );
    assert_eq!(
        reconstructed.previous_turn_settings,
        Some(PreviousTurnSettings {
            model: turn_context.model_info.slug.clone(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(reconstructed.reference_context_item)
            .expect("serialize reconstructed reference context item"),
        serde_json::to_value(Some(first_context_item))
            .expect("serialize expected reference context item")
    );
}

#[tokio::test]
async fn reconstruct_history_rollback_clears_history_and_metadata_when_exceeding_user_turns() {
    let (session, turn_context) = make_session_and_context().await;
    let only_context_item = turn_context.to_turn_context_item();
    let only_turn_id = only_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: only_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "only user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(only_context_item),
        RolloutItem::ResponseItem(user_message("only user")),
        RolloutItem::ResponseItem(assistant_message("only assistant")),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: only_turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 99 },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(reconstructed.history, Vec::new());
    assert_eq!(reconstructed.previous_turn_settings, None);
    assert!(reconstructed.reference_context_item.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_rollback_skips_only_user_turns() {
    let (session, turn_context) = make_session_and_context().await;
    let previous_context_item = turn_context.to_turn_context_item();
    let user_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let standalone_turn_id = "standalone-task-turn".to_string();
    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: user_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: user_turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        // Standalone task turn (no UserMessage) should not consume rollback skips.
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: standalone_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: standalone_turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(session.previous_turn_settings().await, None);
    assert!(session.reference_context_item().await.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_rollback_drops_incomplete_user_turn_compaction_metadata() {
    let (session, turn_context) = make_session_and_context().await;
    let previous_context_item = turn_context.to_turn_context_item();
    let previous_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let incomplete_turn_id = "incomplete-compacted-user-turn".to_string();

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: previous_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item.clone()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: previous_turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: incomplete_turn_id,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "rolled back".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: turn_context.model_info.slug.clone(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(session.reference_context_item().await)
            .expect("serialize seeded reference context item"),
        serde_json::to_value(Some(previous_context_item))
            .expect("serialize expected reference context item")
    );
}

#[tokio::test]
async fn record_initial_history_resumed_bare_turn_context_does_not_seed_reference_context_item() {
    let (session, turn_context) = make_session_and_context().await;
    let previous_context_item = turn_context.to_turn_context_item();
    let rollout_items = vec![RolloutItem::TurnContext(previous_context_item.clone())];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert!(session.reference_context_item().await.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_does_not_seed_reference_context_item_after_compaction() {
    let (session, turn_context) = make_session_and_context().await;
    let previous_context_item = turn_context.to_turn_context_item();
    let rollout_items = vec![
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(session.previous_turn_settings().await, None);
    assert!(session.reference_context_item().await.is_none());
}

#[tokio::test]
async fn reconstruct_history_restores_initial_window_from_session_meta() {
    let (session, turn_context) = make_session_and_context().await;
    let thread_id = ThreadId::default();
    let initial_window_id = Uuid::now_v7();
    let rollout_items = vec![RolloutItem::SessionMeta(SessionMetaLine {
        meta: SessionMeta {
            session_id: thread_id.into(),
            id: thread_id,
            context_window: Some(SessionContextWindow {
                window_id: initial_window_id.to_string(),
            }),
            ..SessionMeta::default()
        },
        git: None,
    })];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(reconstructed.window_number, 0);
    assert_eq!(reconstructed.first_window_id, Some(initial_window_id));
    assert_eq!(reconstructed.previous_window_id, None);
    assert_eq!(reconstructed.window_id, Some(initial_window_id));
}

#[tokio::test]
async fn reconstruct_history_prefers_compacted_window_over_session_meta() {
    let (session, turn_context) = make_session_and_context().await;
    let thread_id = ThreadId::default();
    let initial_window_id = Uuid::now_v7();
    let compacted_first_window_id = Uuid::now_v7();
    let compacted_previous_window_id = Uuid::now_v7();
    let compacted_window_id = Uuid::now_v7();
    let rollout_items = vec![
        RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id: thread_id.into(),
                id: thread_id,
                context_window: Some(SessionContextWindow {
                    window_id: initial_window_id.to_string(),
                }),
                ..SessionMeta::default()
            },
            git: None,
        }),
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            window_number: Some(2),
            first_window_id: Some(compacted_first_window_id.to_string()),
            previous_window_id: Some(compacted_previous_window_id.to_string()),
            window_id: Some(compacted_window_id.to_string()),
        }),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(reconstructed.window_number, 2);
    assert_eq!(
        reconstructed.first_window_id,
        Some(compacted_first_window_id)
    );
    assert_eq!(
        reconstructed.previous_window_id,
        Some(compacted_previous_window_id)
    );
    assert_eq!(reconstructed.window_id, Some(compacted_window_id));
}

#[tokio::test]
async fn reconstruct_history_replays_world_state_from_latest_compaction_window() {
    let (session, turn_context) = make_session_and_context().await;
    let rollout_items = completed_user_turn_rollout(
        turn_context.to_turn_context_item(),
        vec![
            RolloutItem::WorldState(WorldStateItem::full(json!({
                "environment": {"status": "old"}
            }))),
            RolloutItem::Compacted(CompactedItem {
                message: String::new(),
                replacement_history: Some(Vec::new()),
                window_number: Some(1),
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
            }),
            RolloutItem::WorldState(WorldStateItem::full(json!({
                "environment": {"status": "starting", "cwd": "/workspace"}
            }))),
            RolloutItem::WorldState(WorldStateItem::patch(json!({
                "environment": {"status": "ready"}
            }))),
        ],
    );

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        serde_json::to_value(reconstructed.world_state_baseline)
            .expect("serialize reconstructed world state"),
        json!({
            "environment": {"status": "ready", "cwd": "/workspace"}
        })
    );
}

#[tokio::test]
async fn reconstruct_history_preserves_legacy_compaction_count_with_session_meta_window() {
    let (session, turn_context) = make_session_and_context().await;
    let thread_id = ThreadId::default();
    let initial_window_id = Uuid::now_v7();
    let rollout_items = vec![
        RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id: thread_id.into(),
                id: thread_id,
                context_window: Some(SessionContextWindow {
                    window_id: initial_window_id.to_string(),
                }),
                ..SessionMeta::default()
            },
            git: None,
        }),
        RolloutItem::Compacted(CompactedItem {
            message: "legacy summary".to_string(),
            replacement_history: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(reconstructed.window_number, 1);
    assert_eq!(reconstructed.first_window_id, None);
    assert_eq!(reconstructed.previous_window_id, None);
    assert_eq!(reconstructed.window_id, None);
}

#[tokio::test]
async fn reconstruct_history_legacy_compaction_without_replacement_history_does_not_inject_current_initial_context()
 {
    let (session, turn_context) = make_session_and_context().await;
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("before compact")),
        RolloutItem::ResponseItem(assistant_message("assistant reply")),
        RolloutItem::Compacted(CompactedItem {
            message: "legacy summary".to_string(),
            replacement_history: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.history,
        vec![
            user_message("before compact"),
            user_message("legacy summary"),
        ]
    );
    assert!(reconstructed.reference_context_item.is_none());
}

#[tokio::test]
async fn reconstruct_history_legacy_compaction_without_replacement_history_clears_later_reference_context_item()
 {
    let (session, turn_context) = make_session_and_context().await;
    let current_context_item = turn_context.to_turn_context_item();
    let current_turn_id = current_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("before compact")),
        RolloutItem::Compacted(CompactedItem {
            message: "legacy summary".to_string(),
            replacement_history: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: current_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "after legacy compact".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(current_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: current_turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert!(reconstructed.reference_context_item.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_turn_context_after_compaction_reestablishes_reference_context_item()
 {
    let (session, turn_context) = make_session_and_context().await;
    let previous_model = "previous-rollout-model";
    let previous_context_item = TurnContextItem {
        turn_id: Some(turn_context.sub_id.clone()),
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy.value(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: previous_model.to_string(),
        comp_hash: None,
        personality: turn_context.personality,
        collaboration_mode: Some(turn_context.collaboration_mode.clone()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        effort: turn_context.reasoning_effort.clone(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };
    let previous_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: previous_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        // Compaction clears baseline until a later TurnContextItem re-establishes it.
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: previous_turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: previous_model.to_string(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(session.reference_context_item().await)
            .expect("serialize seeded reference context item"),
        serde_json::to_value(Some(TurnContextItem {
            turn_id: Some(turn_context.sub_id.clone()),
            #[allow(deprecated)]
            cwd: turn_context.cwd.clone(),
            workspace_roots: None,
            current_date: turn_context.current_date.clone(),
            timezone: turn_context.timezone.clone(),
            approval_policy: turn_context.approval_policy.value(),
            approvals_reviewer: None,
            sandbox_policy: turn_context.sandbox_policy(),
            permission_profile: None,
            network: None,
            file_system_sandbox_policy: None,
            model: previous_model.to_string(),
            comp_hash: None,
            personality: turn_context.personality,
            collaboration_mode: Some(turn_context.collaboration_mode.clone()),
            multi_agent_version: None,
            multi_agent_mode: None,
            realtime_active: Some(turn_context.realtime_active),
            effort: turn_context.reasoning_effort.clone(),
            summary: codex_protocol::config_types::ReasoningSummary::Auto,
        }))
        .expect("serialize expected reference context item")
    );
}

#[tokio::test]
async fn record_initial_history_resumed_aborted_turn_without_id_clears_active_turn_for_compaction_accounting()
 {
    let (session, turn_context) = make_session_and_context().await;
    let previous_model = "previous-rollout-model";
    let previous_context_item = TurnContextItem {
        turn_id: Some(turn_context.sub_id.clone()),
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy.value(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: previous_model.to_string(),
        comp_hash: None,
        personality: turn_context.personality,
        collaboration_mode: Some(turn_context.collaboration_mode.clone()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        effort: turn_context.reasoning_effort.clone(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };
    let previous_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let aborted_turn_id = "aborted-turn-without-id".to_string();

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: previous_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: previous_turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: aborted_turn_id,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "aborted".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnAborted(
            codex_protocol::protocol::TurnAbortedEvent {
                turn_id: None,
                reason: TurnAbortReason::Interrupted,
                completed_at: None,
                duration_ms: None,
            },
        )),
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: previous_model.to_string(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert!(session.reference_context_item().await.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_unmatched_abort_preserves_active_turn_for_later_turn_context()
 {
    let (session, turn_context) = make_session_and_context().await;
    let previous_context_item = turn_context.to_turn_context_item();
    let previous_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let current_model = "current-rollout-model";
    let current_turn_id = "current-turn".to_string();
    let unmatched_abort_turn_id = "other-turn".to_string();
    let current_context_item = TurnContextItem {
        turn_id: Some(current_turn_id.clone()),
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy.value(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: current_model.to_string(),
        comp_hash: None,
        personality: turn_context.personality,
        collaboration_mode: Some(turn_context.collaboration_mode.clone()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        effort: turn_context.reasoning_effort.clone(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: previous_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: previous_turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: current_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "current".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnAborted(
            codex_protocol::protocol::TurnAbortedEvent {
                turn_id: Some(unmatched_abort_turn_id),
                reason: TurnAbortReason::Interrupted,
                completed_at: None,
                duration_ms: None,
            },
        )),
        RolloutItem::TurnContext(current_context_item.clone()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: current_turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: current_model.to_string(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(session.reference_context_item().await)
            .expect("serialize seeded reference context item"),
        serde_json::to_value(Some(current_context_item))
            .expect("serialize expected reference context item")
    );
}

#[tokio::test]
async fn record_initial_history_resumed_trailing_incomplete_turn_compaction_clears_reference_context_item()
 {
    let (session, turn_context) = make_session_and_context().await;
    let previous_model = "previous-rollout-model";
    let previous_context_item = TurnContextItem {
        turn_id: Some(turn_context.sub_id.clone()),
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy.value(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: previous_model.to_string(),
        comp_hash: None,
        personality: turn_context.personality,
        collaboration_mode: Some(turn_context.collaboration_mode.clone()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        effort: turn_context.reasoning_effort.clone(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };
    let previous_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let incomplete_turn_id = "trailing-incomplete-turn".to_string();

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: previous_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: previous_turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: incomplete_turn_id,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "incomplete".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: previous_model.to_string(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert!(session.reference_context_item().await.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_trailing_incomplete_turn_preserves_turn_context_item() {
    let (session, turn_context) = make_session_and_context().await;
    let current_context_item = turn_context.to_turn_context_item();
    let current_turn_id = current_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: current_turn_id,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "incomplete".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(current_context_item.clone()),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: turn_context.model_info.slug.clone(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(session.reference_context_item().await)
            .expect("serialize seeded reference context item"),
        serde_json::to_value(Some(current_context_item))
            .expect("serialize expected reference context item")
    );
}

#[tokio::test]
async fn record_initial_history_resumed_replaced_incomplete_compacted_turn_clears_reference_context_item()
 {
    let (session, turn_context) = make_session_and_context().await;
    let previous_model = "previous-rollout-model";
    let previous_context_item = TurnContextItem {
        turn_id: Some(turn_context.sub_id.clone()),
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy.value(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: previous_model.to_string(),
        comp_hash: None,
        personality: turn_context.personality,
        collaboration_mode: Some(turn_context.collaboration_mode.clone()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        effort: turn_context.reasoning_effort.clone(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };
    let previous_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let compacted_incomplete_turn_id = "compacted-incomplete-turn".to_string();
    let replacing_turn_id = "replacing-turn".to_string();

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: previous_turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: previous_turn_id,
                last_agent_message: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: compacted_incomplete_turn_id,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "compacted".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
        // A newer TurnStarted replaces the incomplete compacted turn without a matching
        // completion/abort for the old one.
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: replacing_turn_id,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: previous_model.to_string(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert!(session.reference_context_item().await.is_none());
}

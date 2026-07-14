use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::GuardianAssessmentAction;
use codex_protocol::protocol::GuardianAssessmentDecisionSource;
use codex_protocol::protocol::GuardianAssessmentEvent;
use codex_protocol::protocol::GuardianAssessmentStatus;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::NetworkApprovalProtocol;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;

use super::*;
use crate::protocol::v2::CommandExecutionStatus;
use crate::protocol::v2::GuardianApprovalReviewStatus;
use crate::protocol::v2::ThreadItem;
use crate::protocol::v2::TurnError;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;

#[test]
fn projects_turn_lifecycle_without_prior_builder_state() {
    let started = project(RolloutItem::EventMsg(EventMsg::TurnStarted(
        TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            trace_id: None,
            started_at: Some(10),
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        },
    )));
    let completed = project(RolloutItem::EventMsg(EventMsg::TurnComplete(
        TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: None,
            error: None,
            started_at: Some(10),
            completed_at: Some(20),
            duration_ms: Some(10_000),
            time_to_first_token_ms: None,
        },
    )));

    assert_eq!(started.changed_turns.len(), 1);
    assert_eq!(started.changed_turns[0].turn_id, "turn-1");
    assert_eq!(started.changed_turns[0].status, TurnStatus::InProgress);
    assert_eq!(started.changed_turns[0].started_at, Some(10));
    assert_eq!(
        completed,
        ThreadHistoryChangeSet {
            changed_turns: vec![ThreadHistoryTurnChange {
                turn_id: "turn-1".to_string(),
                status: TurnStatus::Completed,
                error: None,
                started_at: Some(10),
                completed_at: Some(20),
                duration_ms: Some(10_000),
            }],
            ..Default::default()
        }
    );
}

#[test]
fn projects_failed_turn_completion_as_snapshot() {
    let error = ErrorEvent {
        message: "request failed".to_string(),
        codex_error_info: None,
    };

    let changes = project(RolloutItem::EventMsg(EventMsg::TurnComplete(
        TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: None,
            error: Some(error),
            started_at: Some(10),
            completed_at: Some(20),
            duration_ms: Some(10_000),
            time_to_first_token_ms: None,
        },
    )));

    assert_eq!(
        changes,
        ThreadHistoryChangeSet {
            changed_turns: vec![ThreadHistoryTurnChange {
                turn_id: "turn-1".to_string(),
                status: TurnStatus::Failed,
                error: Some(TurnError {
                    message: "request failed".to_string(),
                    codex_error_info: None,
                    additional_details: None,
                }),
                started_at: Some(10),
                completed_at: Some(20),
                duration_ms: Some(10_000),
            }],
            ..Default::default()
        }
    );
}

#[test]
fn projects_completed_canonical_turn_items() {
    let thread_id = ThreadId::default();
    let user_item = TurnItem::UserMessage(UserMessageItem {
        id: "user-1".to_string(),
        client_id: None,
        content: vec![UserInput::Text {
            text: "hello".to_string(),
            text_elements: Vec::new(),
        }],
    });
    let agent_item = TurnItem::AgentMessage(AgentMessageItem {
        id: "agent-1".to_string(),
        content: vec![AgentMessageContent::Text {
            text: "done".to_string(),
        }],
        phase: None,
        memory_citation: None,
    });

    let user_changes = project(item_completed(thread_id, "turn-1", user_item.clone()));
    let agent_changes = project(item_completed(thread_id, "turn-1", agent_item.clone()));

    assert_eq!(
        user_changes.changed_items,
        vec![ThreadHistoryItemChange {
            turn_id: "turn-1".to_string(),
            item: ThreadItem::from(user_item),
        }]
    );
    assert_eq!(
        agent_changes.changed_items,
        vec![ThreadHistoryItemChange {
            turn_id: "turn-1".to_string(),
            item: ThreadItem::from(agent_item),
        }]
    );
}

#[test]
fn projects_persisted_reviewer_unavailable_reviews_without_losing_provenance() {
    let rationale = "automatic reviewer temporarily unavailable; not a risk denial; retry with \
        backoff";
    let network_changes = project(RolloutItem::EventMsg(EventMsg::GuardianAssessment(
        GuardianAssessmentEvent {
            id: "review-network".to_string(),
            target_item_id: None,
            turn_id: "turn-1".to_string(),
            started_at_ms: 10,
            completed_at_ms: Some(20),
            status: GuardianAssessmentStatus::ReviewerUnavailable,
            risk_level: None,
            user_authorization: None,
            rationale: Some(rationale.to_string()),
            decision_source: Some(GuardianAssessmentDecisionSource::Agent),
            action: GuardianAssessmentAction::NetworkAccess {
                target: "https://api.example.com:443".to_string(),
                host: "api.example.com".to_string(),
                protocol: NetworkApprovalProtocol::Https,
                port: 443,
            },
        },
    )));
    assert_eq!(network_changes.changed_items.len(), 1);
    let ThreadItem::GuardianApprovalReview(review) = &network_changes.changed_items[0].item else {
        panic!("network review should project as a durable review item");
    };
    assert_eq!(
        review.review.status,
        GuardianApprovalReviewStatus::ReviewerUnavailable
    );
    assert_eq!(review.review.rationale.as_deref(), Some(rationale));

    let command_changes = project(RolloutItem::EventMsg(EventMsg::GuardianAssessment(
        GuardianAssessmentEvent {
            id: "review-command".to_string(),
            target_item_id: Some("command-1".to_string()),
            turn_id: "turn-1".to_string(),
            started_at_ms: 10,
            completed_at_ms: Some(20),
            status: GuardianAssessmentStatus::ReviewerUnavailable,
            risk_level: None,
            user_authorization: None,
            rationale: Some(rationale.to_string()),
            decision_source: Some(GuardianAssessmentDecisionSource::Agent),
            action: GuardianAssessmentAction::Command {
                source: codex_protocol::protocol::GuardianCommandSource::Shell,
                command: "curl https://api.example.com".to_string(),
                cwd: test_path_buf("/tmp").abs(),
            },
        },
    )));
    assert_eq!(command_changes.changed_items.len(), 2);
    assert!(matches!(
        &command_changes.changed_items[0].item,
        ThreadItem::CommandExecution {
            status: CommandExecutionStatus::Failed,
            ..
        }
    ));
    assert!(matches!(
        &command_changes.changed_items[1].item,
        ThreadItem::GuardianApprovalReview(review)
            if review.review.status == GuardianApprovalReviewStatus::ReviewerUnavailable
                && review.review.rationale.as_deref() == Some(rationale)
    ));
}

#[test]
fn ignores_legacy_abort_without_turn_id_and_context_only_records() {
    let aborted = project(RolloutItem::EventMsg(EventMsg::TurnAborted(
        TurnAbortedEvent {
            turn_id: None,
            reason: TurnAbortReason::Interrupted,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        },
    )));
    let compacted = project(RolloutItem::Compacted(CompactedItem {
        message: String::new(),
        replacement_history: None,
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
    }));

    assert!(aborted.is_empty());
    assert!(compacted.is_empty());
}

#[test]
fn projects_identified_turn_aborts() {
    let changes = project(RolloutItem::EventMsg(EventMsg::TurnAborted(
        TurnAbortedEvent {
            turn_id: Some("turn-1".to_string()),
            reason: TurnAbortReason::Interrupted,
            started_at: Some(10),
            completed_at: Some(20),
            duration_ms: Some(10_000),
        },
    )));

    assert_eq!(
        changes,
        ThreadHistoryChangeSet {
            changed_turns: vec![ThreadHistoryTurnChange {
                turn_id: "turn-1".to_string(),
                status: TurnStatus::Interrupted,
                error: None,
                started_at: Some(10),
                completed_at: Some(20),
                duration_ms: Some(10_000),
            }],
            ..Default::default()
        }
    );
}

fn project(item: RolloutItem) -> ThreadHistoryChangeSet {
    project_rollout_line(&RolloutLine {
        timestamp: "2026-07-09T00:00:00.000Z".to_string(),
        ordinal: Some(7),
        item,
    })
}

fn item_completed(thread_id: ThreadId, turn_id: &str, item: TurnItem) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id,
        turn_id: turn_id.to_string(),
        item,
        completed_at_ms: 123,
    }))
}

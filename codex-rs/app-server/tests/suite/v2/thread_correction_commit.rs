use anyhow::Context;
use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadCorrectionCommitParams;
use codex_app_server_protocol::ThreadCorrectionCommitResponse;
use codex_app_server_protocol::ThreadCorrectionCommitStatus;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadRollbackParams;
use codex_app_server_protocol::ThreadRollbackResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartedNotification;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_core::RolloutRecorder;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::RolloutItem;
use core_test_support::responses;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::time::sleep;
use tokio::time::timeout;
use uuid::Uuid;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
async fn thread_correction_commit_is_durable_and_idempotent() -> Result<()> {
    let server = responses::start_mock_server().await;
    let first_response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-target"),
            responses::ev_assistant_message("msg-target-answer", "Ready"),
            responses::ev_completed("resp-target"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, app.initialize()).await??;

    let thread_request = app
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let thread_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_stream_until_response_message(RequestId::Integer(thread_request)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_response)?;

    let target_client_user_message_id = "client-message-to-correct".to_string();
    let target_request = app
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            client_user_message_id: Some(target_client_user_message_id.clone()),
            input: vec![V2UserInput::Text {
                text: "Use parakeat".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_stream_until_response_message(RequestId::Integer(target_request)),
    )
    .await??;
    timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    assert_eq!(first_response_mock.requests().len(), 1);
    app.clear_message_buffer();

    let correction_response = responses::sse_response(responses::sse(vec![
        responses::ev_response_created("resp-correction"),
        responses::ev_assistant_message("msg-correction-answer", "Done"),
        responses::ev_completed("resp-correction"),
    ]))
    .set_delay(std::time::Duration::from_millis(500));
    let response_mock = responses::mount_response_once(&server, correction_response).await;

    let correction_id = Uuid::new_v4().to_string();
    let correction_frame_id = format!(
        "msg_correction_{}",
        Uuid::parse_str(&correction_id)?.simple()
    );
    let payload = "mechanically replace parakeat with Parakeet".to_string();
    let params = ThreadCorrectionCommitParams {
        thread_id: thread.id.clone(),
        correction_id: correction_id.clone(),
        expected_client_user_message_id: target_client_user_message_id.clone(),
        payload: payload.clone(),
    };
    let first_request = app
        .send_thread_correction_commit_request(params.clone())
        .await?;
    let first_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_stream_until_response_message(RequestId::Integer(first_request)),
    )
    .await??;
    assert_eq!(
        to_response::<ThreadCorrectionCommitResponse>(first_response)?.status,
        ThreadCorrectionCommitStatus::Queued
    );

    let retry_request = app
        .send_thread_correction_commit_request(params.clone())
        .await?;
    let retry_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_stream_until_response_message(RequestId::Integer(retry_request)),
    )
    .await??;
    assert_eq!(
        to_response::<ThreadCorrectionCommitResponse>(retry_response)?.status,
        ThreadCorrectionCommitStatus::Queued
    );

    let conflict_request = app
        .send_thread_correction_commit_request(ThreadCorrectionCommitParams {
            thread_id: thread.id.clone(),
            correction_id: correction_id.clone(),
            expected_client_user_message_id: target_client_user_message_id.clone(),
            payload: "different payload".to_string(),
        })
        .await?;
    let conflict: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_stream_until_error_message(RequestId::Integer(conflict_request)),
    )
    .await??;
    assert_eq!(conflict.error.code, -32600);

    timeout(DEFAULT_READ_TIMEOUT, async {
        while response_mock.requests().is_empty() {
            sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .context("timed out waiting for correction sampling request")?;
    assert_eq!(
        response_mock.requests().len(),
        1,
        "correction must be sampled exactly once"
    );
    let request = response_mock.single_request();
    let request_body = request.body_json();
    assert!(
        request_body
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|tools| !tools.is_empty()),
        "ordinary correction turn must advertise the normal nonempty tool set"
    );
    let expected_frame_text = format!("<{correction_frame_id}>{payload}</{correction_frame_id}>");
    let expected_content = serde_json::json!([{
        "type": "input_text",
        "text": expected_frame_text,
    }]);
    let request_input = request.input();
    assert_eq!(
        request_input
            .iter()
            .filter(|item| {
                item.get("role").and_then(serde_json::Value::as_str) == Some("developer")
                    && item.get("content") == Some(&expected_content)
            })
            .count(),
        1
    );
    assert!(
        !request_input
            .iter()
            .filter(|item| item.get("role").and_then(serde_json::Value::as_str) == Some("user"))
            .any(|item| {
                let serialized = item.to_string();
                serialized.contains(&correction_frame_id) || serialized.contains(&payload)
            }),
        "correction context must not invent a user message"
    );

    let started_notification = timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_stream_until_notification_message("turn/started"),
    )
    .await??;
    let started: TurnStartedNotification = serde_json::from_value(
        started_notification
            .params
            .context("turn/started notification must include params")?,
    )?;
    assert_eq!(started.thread_id, thread.id);
    assert_eq!(started.turn.status, TurnStatus::InProgress);
    let correction_turn_id = started.turn.id.clone();

    let completed_notification = timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    let completed: TurnCompletedNotification = serde_json::from_value(
        completed_notification
            .params
            .context("turn/completed notification must include params")?,
    )?;
    assert_eq!(completed.thread_id, thread.id);
    assert_eq!(completed.turn.id, correction_turn_id);
    assert_eq!(completed.turn.status, TurnStatus::Completed);

    let rollout_path = thread.path.as_ref().context("thread path missing")?;
    let rollout_items = wait_for_sampled_rollout(rollout_path, correction_id.as_str()).await?;
    assert_eq!(
        responses_request_count(&server).await?,
        2,
        "target and ordinary correction turns must make exactly two model requests"
    );
    let correction_intents = rollout_items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| match item {
            RolloutItem::EventMsg(EventMsg::RawResponseItem(event)) => event
                .persisted_correction_intent()
                .filter(|intent| intent.correction_id.as_str() == correction_id)
                .map(|intent| (index, intent)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        correction_intents.len(),
        1,
        "idempotent retries must persist exactly one correction intent"
    );
    let (correction_intent_index, correction_intent) = correction_intents[0];
    assert_eq!(
        correction_intent.expected_client_user_message_id,
        target_client_user_message_id
    );
    assert_eq!(correction_intent.payload, payload);

    let expected_frame_content = [ContentItem::InputText {
        text: expected_frame_text,
    }];
    let lifecycle_projection = rollout_items
        .iter()
        .enumerate()
        .skip(correction_intent_index + 1)
        .filter_map(|(index, item)| match item {
            RolloutItem::EventMsg(EventMsg::TurnStarted(event))
                if event.turn_id == correction_turn_id =>
            {
                Some((index, "started"))
            }
            RolloutItem::ResponseItem(ResponseItem::Message {
                id: Some(id),
                role,
                content,
                ..
            }) if id == &correction_frame_id
                && role == "developer"
                && content.as_slice() == expected_frame_content.as_slice() =>
            {
                Some((index, "frame"))
            }
            RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. })
                if role == "assistant"
                    && matches!(
                        content.as_slice(),
                        [ContentItem::OutputText { text }] if text == "Done"
                    ) =>
            {
                Some((index, "assistant"))
            }
            RolloutItem::EventMsg(EventMsg::RawResponseItem(event))
                if event.persisted_corrections_sampled().is_some_and(|proof| {
                    proof.correction_ids.as_slice() == [correction_id.as_str()]
                }) =>
            {
                Some((index, "sampled"))
            }
            RolloutItem::EventMsg(EventMsg::TurnComplete(event))
                if event.turn_id == correction_turn_id =>
            {
                Some((index, "complete"))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        lifecycle_projection
            .iter()
            .map(|(_, marker)| *marker)
            .collect::<Vec<_>>(),
        ["started", "frame", "assistant", "sampled", "complete"],
        "durable lifecycle must contain each marker once and in execution order"
    );
    let turn_started_index = lifecycle_projection[0].0;
    let turn_complete_index = lifecycle_projection[4].0;
    assert!(
        rollout_items[turn_started_index + 1..turn_complete_index]
            .iter()
            .all(|item| !is_user_turn_boundary(item)),
        "ordinary correction lifecycle must not invent an actual user boundary"
    );

    let visible_thread = read_thread(&mut app, thread.id.as_str()).await?;
    assert!(
        matches!(
            visible_thread.turns.as_slice(),
            [target_turn, correction_turn]
                if matches!(
                target_turn.items.as_slice(),
                [
                    ThreadItem::UserMessage { client_id, .. },
                    ThreadItem::AgentMessage { text, .. }
                ] if client_id.as_deref() == Some(target_client_user_message_id.as_str())
                    && text == "Ready"
            ) && correction_turn.id == correction_turn_id
                && matches!(
                    correction_turn.items.as_slice(),
                    [ThreadItem::AgentMessage { text, .. }] if text == "Done"
                )
        ),
        "ordinary correction must add a visible agent-only turn after the target"
    );

    let rollback_request = app
        .send_thread_rollback_request(ThreadRollbackParams {
            thread_id: thread.id.clone(),
            num_turns: 1,
        })
        .await?;
    let rollback_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_stream_until_response_message(RequestId::Integer(rollback_request)),
    )
    .await??;
    let ThreadRollbackResponse {
        thread: rolled_back_thread,
    } = to_response::<ThreadRollbackResponse>(rollback_response)?;
    assert!(
        rolled_back_thread.turns.is_empty(),
        "rollback 1 must remove the newest actual user boundary and its later correction turn"
    );
    let rolled_back_read = read_thread(&mut app, thread.id.as_str()).await?;
    assert!(
        rolled_back_read.turns.is_empty(),
        "thread/read must durably reflect the target rollback"
    );

    let model_request_count = responses_request_count(&server).await?;
    let recommit_request = app.send_thread_correction_commit_request(params).await?;
    let recommit_error: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_stream_until_error_message(RequestId::Integer(recommit_request)),
    )
    .await??;
    assert_eq!(recommit_error.error.code, -32600);
    assert!(
        recommit_error
            .error
            .message
            .contains("does not identify a surviving user message"),
        "recommit must be rejected against the rolled-back target: {}",
        recommit_error.error.message
    );
    sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        responses_request_count(&server).await?,
        model_request_count,
        "rejected recommit must not resample the correction"
    );

    Ok(())
}

async fn wait_for_sampled_rollout(
    rollout_path: &Path,
    correction_id: &str,
) -> Result<Arc<Vec<RolloutItem>>> {
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let InitialHistory::Resumed(history) =
                RolloutRecorder::get_rollout_history(rollout_path).await?
            else {
                anyhow::bail!("expected resumed rollout history");
            };
            if history.history.iter().any(|item| {
                matches!(
                    item,
                    RolloutItem::EventMsg(EventMsg::RawResponseItem(event))
                        if event.persisted_corrections_sampled().is_some_and(|proof| {
                            proof.correction_ids.iter().any(|id| id == correction_id)
                        })
                )
            }) {
                let observed_len = history.history.len();
                sleep(std::time::Duration::from_millis(50)).await;
                let InitialHistory::Resumed(stable_history) =
                    RolloutRecorder::get_rollout_history(rollout_path).await?
                else {
                    anyhow::bail!("expected resumed rollout history");
                };
                if stable_history.history.len() == observed_len {
                    return Ok(stable_history.history);
                }
            }
            sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .context("timed out waiting for durable correction sampled proof")?
}

async fn responses_request_count(server: &wiremock::MockServer) -> Result<usize> {
    let requests = server
        .received_requests()
        .await
        .context("wiremock did not record requests")?;
    Ok(requests
        .iter()
        .filter(|request| request.method == "POST" && request.url.path().ends_with("/responses"))
        .count())
}

async fn read_thread(app: &mut TestAppServer, thread_id: &str) -> Result<Thread> {
    let read_request = app
        .send_thread_read_request(ThreadReadParams {
            thread_id: thread_id.to_string(),
            include_turns: true,
        })
        .await?;
    let read_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_stream_until_response_message(RequestId::Integer(read_request)),
    )
    .await??;
    Ok(to_response::<ThreadReadResponse>(read_response)?.thread)
}

fn is_user_turn_boundary(item: &RolloutItem) -> bool {
    match item {
        RolloutItem::EventMsg(EventMsg::UserMessage(_)) => true,
        RolloutItem::InterAgentCommunication(_) => true,
        RolloutItem::ResponseItem(ResponseItem::AgentMessage { .. }) => true,
        RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. }) => {
            role == "user"
                || (role == "assistant"
                    && InterAgentCommunication::is_message_content(content.as_slice()))
        }
        _ => false,
    }
}

fn create_config_toml(codex_home: &std::path::Path, base_url: &str) -> Result<()> {
    std::fs::write(
        codex_home.join("config.toml"),
        format!(
            r#"
model = "mock-model"
model_provider = "mock"

[model_providers.mock]
name = "mock"
base_url = "{base_url}/v1"
wire_api = "responses"
"#
        ),
    )?;
    Ok(())
}

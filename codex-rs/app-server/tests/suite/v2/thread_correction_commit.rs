use anyhow::Context;
use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadCorrectionCommitParams;
use codex_app_server_protocol::ThreadCorrectionCommitResponse;
use codex_app_server_protocol::ThreadCorrectionCommitStatus;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_core::RolloutRecorder;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::RolloutItem;
use core_test_support::responses;
use tempfile::TempDir;
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

    let retry_request = app.send_thread_correction_commit_request(params).await?;
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
            correction_id,
            expected_client_user_message_id: target_client_user_message_id,
            payload: "different payload".to_string(),
        })
        .await?;
    let conflict: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_stream_until_error_message(RequestId::Integer(conflict_request)),
    )
    .await??;
    assert_eq!(conflict.error.code, -32600);

    timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    let request = response_mock.single_request();
    let expected_content = serde_json::json!([{
        "type": "input_text",
        "text": format!("<{correction_frame_id}>{payload}</{correction_frame_id}>"),
    }]);
    let correction_messages = request
        .input()
        .into_iter()
        .filter(|item| {
            item.get("role").and_then(serde_json::Value::as_str) == Some("developer")
                && item.get("content") == Some(&expected_content)
        })
        .collect::<Vec<_>>();
    assert_eq!(correction_messages.len(), 1);
    assert_eq!(
        correction_messages[0]
            .get("role")
            .and_then(serde_json::Value::as_str),
        Some("developer")
    );
    assert_eq!(
        correction_messages[0].get("content"),
        Some(&expected_content)
    );
    let correction_user_messages = request
        .input()
        .into_iter()
        .filter(|item| item.get("role").and_then(serde_json::Value::as_str) == Some("user"))
        .filter(|item| {
            let serialized = item.to_string();
            serialized.contains(&correction_frame_id) || serialized.contains(&payload)
        })
        .count();
    assert_eq!(
        correction_user_messages, 0,
        "correction context must not invent a user message"
    );

    let rollout_path = thread.path.as_ref().context("thread path missing")?;
    let InitialHistory::Resumed(history) =
        RolloutRecorder::get_rollout_history(rollout_path).await?
    else {
        anyhow::bail!("expected resumed rollout history");
    };
    let correction_frames = history
        .history
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(item)
                if item
                    .id()
                    .is_some_and(|id| id.starts_with("msg_correction_")) =>
            {
                Some(item)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(correction_frames.len(), 1);
    assert!(
        serde_json::to_string(correction_frames[0])?.contains(&payload),
        "persisted correction must retain the exact payload"
    );

    Ok(())
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

use anyhow::Result;
use codex_config::types::ModelSettingsPolicy;
use codex_config::types::ServerModelValidation;
use codex_protocol::models::ContentItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ModelRerouteReason;
use codex_protocol::protocol::ModelVerification;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_model_verification_metadata;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once;
use core_test_support::responses::mount_response_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_completed;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use wiremock::ResponseTemplate;

const SERVER_MODEL: &str = "gpt-5.2";
const REQUESTED_MODEL: &str = "gpt-5.3-codex";
const TRUSTED_ACCESS_FOR_CYBER_VERIFICATION: &str = "trusted_access_for_cyber";

const CYBER_POLICY_MESSAGE: &str =
    "This request has been flagged for potentially high-risk cyber activity.";

fn disabled_text_turn(test: &TestCodex, text: &str) -> Op {
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.cwd_path());
    Op::UserInput {
        items: vec![UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }],
        final_output_json_schema: None,
        responsesapi_client_metadata: None,
        additional_context: Default::default(),
        thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
            environments: Some(local_selections(test.config.cwd.clone())),
            approval_policy: Some(AskForApproval::Never),
            sandbox_policy: Some(sandbox_policy),
            permission_profile,
            collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                mode: codex_protocol::config_types::ModeKind::Default,
                settings: codex_protocol::config_types::Settings {
                    model: test.session_configured.model.clone(),
                    reasoning_effort: test.config.model_reasoning_effort.clone(),
                    developer_instructions: None,
                },
            }),
            ..Default::default()
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn locked_new_thread_persists_exact_model_settings_anchor() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex()
        .with_model(REQUESTED_MODEL)
        .with_config(|config| {
            config.model_settings_policy = ModelSettingsPolicy::Locked;
        });
    let test = builder.build(&server).await?;
    test.codex.ensure_rollout_materialized().await;
    test.codex.flush_rollout().await?;

    let history = test.codex.load_history(/*include_archived*/ false).await?;
    let anchor = history.items.iter().find_map(|item| match item {
        RolloutItem::SessionMeta(meta) if meta.meta.id == test.session_configured.thread_id => {
            meta.meta.model_settings.as_ref()
        }
        _ => None,
    });
    let anchor = anchor.expect("locked thread should persist a model settings anchor");
    assert_eq!(anchor.model, test.session_configured.model);
    assert_eq!(
        anchor.reasoning_effort,
        test.session_configured.reasoning_effort
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutable_new_thread_omits_model_settings_anchor() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_model(REQUESTED_MODEL);
    let test = builder.build(&server).await?;
    test.codex.ensure_rollout_materialized().await;
    test.codex.flush_rollout().await?;

    let history = test.codex.load_history(/*include_archived*/ false).await?;
    let anchor = history.items.iter().find_map(|item| match item {
        RolloutItem::SessionMeta(meta) if meta.meta.id == test.session_configured.thread_id => {
            Some(meta.meta.model_settings.as_ref())
        }
        _ => None,
    });
    assert_eq!(anchor, Some(None));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_model_header_mismatch_emits_warning_event() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response =
        sse_response(sse_completed("resp-1")).insert_header("OpenAI-Model", SERVER_MODEL);
    let _mock = mount_response_once(&server, response).await;

    let mut builder = test_codex().with_model(REQUESTED_MODEL);
    let test = builder.build(&server).await?;

    test.codex
        .submit(disabled_text_turn(&test, "trigger safety check"))
        .await?;

    let reroute = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ModelReroute(_))
    })
    .await;
    let EventMsg::ModelReroute(reroute) = reroute else {
        panic!("expected model reroute event");
    };
    assert_eq!(reroute.from_model, REQUESTED_MODEL);
    assert_eq!(reroute.to_model, SERVER_MODEL);
    assert_eq!(reroute.reason, ModelRerouteReason::HighRiskCyberActivity);

    let warning = wait_for_event(&test.codex, |event| matches!(event, EventMsg::Warning(_))).await;
    let EventMsg::Warning(warning) = warning else {
        panic!("expected warning event");
    };
    assert!(warning.message.contains(REQUESTED_MODEL));
    assert!(warning.message.contains(SERVER_MODEL));

    let _ = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn required_server_model_match_replays_matching_response() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut created = ev_response_created("resp-1");
    created["response"]["headers"] = serde_json::json!({
        "OpenAI-Model": REQUESTED_MODEL.to_ascii_uppercase()
    });
    let response = sse_response(sse(vec![
        created,
        ev_assistant_message("msg-1", "validated"),
        core_test_support::responses::ev_completed("resp-1"),
    ]))
    .insert_header("OpenAI-Model", REQUESTED_MODEL);
    let _mock = mount_response_once(&server, response).await;

    let mut builder = test_codex()
        .with_model(REQUESTED_MODEL)
        .with_config(|config| {
            config.server_model_validation = ServerModelValidation::RequireMatch;
        });
    let test = builder.build(&server).await?;

    test.codex
        .submit(disabled_text_turn(&test, "matching attestation"))
        .await?;

    let message = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::AgentMessage(_))
    })
    .await;
    let EventMsg::AgentMessage(message) = message else {
        panic!("expected agent message");
    };
    assert_eq!(message.message, "validated");
    let _ = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn required_server_model_match_rejects_late_mismatch_before_tool_execution() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let marker_name = "server-model-mismatch-tool-ran";
    let tool_args = serde_json::json!({
        "command": format!("touch {marker_name}"),
        "timeout_ms": 1_000
    });
    let response = sse_response(sse(vec![
        ev_response_created("resp-1"),
        ev_assistant_message("msg-1", "must not escape validation buffer"),
        ev_function_call(
            "call-1",
            "shell_command",
            &serde_json::to_string(&tool_args)?,
        ),
        serde_json::json!({
            "type": "response.created",
            "response": {
                "id": "resp-1-late",
                "headers": { "OpenAI-Model": SERVER_MODEL }
            }
        }),
        core_test_support::responses::ev_completed("resp-1"),
    ]))
    .insert_header("OpenAI-Model", REQUESTED_MODEL);
    let _mock = mount_response_once(&server, response).await;

    let mut builder = test_codex()
        .with_model(REQUESTED_MODEL)
        .with_config(|config| {
            config.server_model_validation = ServerModelValidation::RequireMatch;
        });
    let test = builder.build(&server).await?;

    test.codex
        .submit(disabled_text_turn(&test, "late mismatching attestation"))
        .await?;

    let error = loop {
        let event = wait_for_event(&test.codex, |_| true).await;
        match event {
            EventMsg::Error(error) => break error,
            EventMsg::AgentMessage(_)
            | EventMsg::AgentMessageContentDelta(_)
            | EventMsg::RawResponseItem(_)
            | EventMsg::ItemStarted(_)
            | EventMsg::ItemCompleted(_) => {
                panic!("buffered server output escaped before model validation failed")
            }
            _ => {}
        }
    };
    assert!(error.message.contains("server model validation failed"));
    assert!(!test.config.cwd.join(marker_name).exists());
    let _ = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    test.codex.flush_rollout().await?;
    let history = test.codex.load_history(/*include_archived*/ false).await?;
    assert!(!history.items.iter().any(|item| match item {
        RolloutItem::ResponseItem(ResponseItem::FunctionCall { .. }) => true,
        RolloutItem::ResponseItem(ResponseItem::Message { role, .. }) => role == "assistant",
        _ => false,
    }));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn required_server_model_match_rejects_first_mismatch_before_output() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = sse_response(sse(vec![
        ev_response_created("resp-1"),
        ev_assistant_message("msg-1", "must remain buffered"),
        core_test_support::responses::ev_completed("resp-1"),
    ]))
    .insert_header("OpenAI-Model", SERVER_MODEL);
    let _mock = mount_response_once(&server, response).await;

    let mut builder = test_codex()
        .with_model(REQUESTED_MODEL)
        .with_config(|config| {
            config.server_model_validation = ServerModelValidation::RequireMatch;
        });
    let test = builder.build(&server).await?;

    test.codex
        .submit(disabled_text_turn(&test, "first attestation mismatches"))
        .await?;

    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::Error(error) => {
                assert!(error.message.contains("server model validation failed"));
                break;
            }
            EventMsg::AgentMessage(_)
            | EventMsg::AgentMessageContentDelta(_)
            | EventMsg::RawResponseItem(_)
            | EventMsg::ItemStarted(_)
            | EventMsg::ItemCompleted(_) => {
                panic!("buffered server output escaped before model validation failed")
            }
            _ => {}
        }
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn required_server_model_match_rejects_missing_attestation() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let _mock = mount_response_once(&server, sse_response(sse_completed("resp-1"))).await;

    let mut builder = test_codex()
        .with_model(REQUESTED_MODEL)
        .with_config(|config| {
            config.server_model_validation = ServerModelValidation::RequireMatch;
        });
    let test = builder.build(&server).await?;

    test.codex
        .submit(disabled_text_turn(&test, "missing model attestation"))
        .await?;

    let error = wait_for_event(&test.codex, |event| matches!(event, EventMsg::Error(_))).await;
    let EventMsg::Error(error) = error else {
        panic!("expected model validation error");
    };
    assert!(error.message.contains("did not attest"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cyber_policy_response_emits_typed_error_without_retry() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = ResponseTemplate::new(400).set_body_json(serde_json::json!({
        "error": {
            "message": CYBER_POLICY_MESSAGE,
            "type": "invalid_request",
            "param": null,
            "code": "cyber_policy"
        }
    }));
    let mock = mount_response_once(&server, response).await;

    let mut builder = test_codex().with_model(REQUESTED_MODEL);
    let test = builder.build(&server).await?;

    test.codex
        .submit(disabled_text_turn(&test, "trigger cyber policy error"))
        .await?;

    let error = wait_for_event(&test.codex, |event| matches!(event, EventMsg::Error(_))).await;
    let EventMsg::Error(error) = error else {
        panic!("expected error event");
    };
    assert_eq!(error.message, CYBER_POLICY_MESSAGE);
    assert_eq!(error.codex_error_info, Some(CodexErrorInfo::CyberPolicy));

    mock.single_request();

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_model_field_mismatch_emits_warning_when_header_matches_requested() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = sse_response(sse(vec![
        serde_json::json!({
            "type": "response.created",
            "response": {
                "id": "resp-1",
                "headers": {
                    "OpenAI-Model": SERVER_MODEL
                }
            }
        }),
        core_test_support::responses::ev_completed("resp-1"),
    ]))
    .insert_header("OpenAI-Model", REQUESTED_MODEL);
    let _mock = mount_response_once(&server, response).await;

    let mut builder = test_codex().with_model(REQUESTED_MODEL);
    let test = builder.build(&server).await?;

    test.codex
        .submit(disabled_text_turn(&test, "trigger response model check"))
        .await?;

    let reroute = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ModelReroute(_))
    })
    .await;
    let EventMsg::ModelReroute(reroute) = reroute else {
        panic!("expected model reroute event");
    };
    assert_eq!(reroute.from_model, REQUESTED_MODEL);
    assert_eq!(reroute.to_model, SERVER_MODEL);
    assert_eq!(reroute.reason, ModelRerouteReason::HighRiskCyberActivity);

    let warning = wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::Warning(warning)
                if warning
                    .message
                    .contains("flagged for potentially high-risk cyber activity")
        )
    })
    .await;
    let EventMsg::Warning(warning) = warning else {
        panic!("expected warning event");
    };
    assert!(warning.message.contains(REQUESTED_MODEL));
    assert!(warning.message.contains(SERVER_MODEL));

    let _ = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_model_header_mismatch_only_emits_one_warning_per_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let tool_args = serde_json::json!({
        "command": "echo hello",
        "timeout_ms": 1_000
    });

    let first_response = sse_response(sse(vec![
        ev_response_created("resp-1"),
        ev_function_call(
            "call-1",
            "shell_command",
            &serde_json::to_string(&tool_args)?,
        ),
        core_test_support::responses::ev_completed("resp-1"),
    ]))
    .insert_header("OpenAI-Model", SERVER_MODEL);
    let second_response = sse_response(sse(vec![
        ev_response_created("resp-2"),
        ev_assistant_message("msg-1", "done"),
        core_test_support::responses::ev_completed("resp-2"),
    ]))
    .insert_header("OpenAI-Model", SERVER_MODEL);
    let _mock = mount_response_sequence(&server, vec![first_response, second_response]).await;

    let mut builder = test_codex().with_model(REQUESTED_MODEL);
    let test = builder.build(&server).await?;

    test.codex
        .submit(disabled_text_turn(&test, "trigger follow-up turn"))
        .await?;

    let mut warning_count = 0;
    loop {
        let event = wait_for_event(&test.codex, |_| true).await;
        match event {
            EventMsg::Warning(warning)
                if warning
                    .message
                    .contains("flagged for potentially high-risk cyber activity") =>
            {
                warning_count += 1;
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert_eq!(warning_count, 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_model_header_casing_only_mismatch_does_not_warn() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let requested_header = REQUESTED_MODEL.to_ascii_uppercase();
    let response = sse_response(sse_completed("resp-1"))
        .insert_header("OpenAI-Model", requested_header.as_str());
    let _mock = mount_response_once(&server, response).await;

    let mut builder = test_codex().with_model(REQUESTED_MODEL);
    let test = builder.build(&server).await?;

    test.codex
        .submit(disabled_text_turn(&test, "trigger casing check"))
        .await?;

    let mut reroute_count = 0;
    let mut warning_count = 0;
    loop {
        let event = wait_for_event(&test.codex, |_| true).await;
        match event {
            EventMsg::ModelReroute(_) => reroute_count += 1,
            EventMsg::Warning(warning)
                if warning
                    .message
                    .contains("flagged for potentially high-risk cyber activity") =>
            {
                warning_count += 1;
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert_eq!(reroute_count, 0);
    assert_eq!(warning_count, 0);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_verification_emits_structured_event_without_reroute_or_warning() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = sse_response(sse(vec![
        ev_response_created("resp-1"),
        ev_model_verification_metadata("resp-1", vec![TRUSTED_ACCESS_FOR_CYBER_VERIFICATION]),
        core_test_support::responses::ev_completed("resp-1"),
    ]));
    let _mock = mount_response_once(&server, response).await;

    let mut builder = test_codex().with_model(SERVER_MODEL);
    let test = builder.build(&server).await?;

    test.codex
        .submit(disabled_text_turn(&test, "trigger model verification"))
        .await?;

    let mut verification_count = 0;
    let mut reroute_count = 0;
    let mut warning_count = 0;
    let mut warning_item_count = 0;
    loop {
        let event = wait_for_event(&test.codex, |_| true).await;
        match event {
            EventMsg::ModelVerification(event) => {
                assert_eq!(
                    event.verifications,
                    vec![ModelVerification::TrustedAccessForCyber]
                );
                verification_count += 1;
            }
            EventMsg::Warning(_) => warning_count += 1,
            EventMsg::ModelReroute(_) => reroute_count += 1,
            EventMsg::RawResponseItem(raw)
                if matches!(
                    &raw.item,
                    ResponseItem::Message { content, .. }
                        if content.iter().any(|item| matches!(
                            item,
                            ContentItem::InputText { text } if text.starts_with("Warning: ")
                        ))
                ) =>
            {
                warning_item_count += 1;
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert_eq!(verification_count, 1);
    assert_eq!(reroute_count, 0);
    assert_eq!(warning_count, 0);
    assert_eq!(warning_item_count, 0);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_verification_only_emits_once_per_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let tool_args = serde_json::json!({
        "command": "echo hello",
        "timeout_ms": 1_000
    });

    let first_response = sse_response(sse(vec![
        ev_response_created("resp-1"),
        ev_function_call(
            "call-1",
            "shell_command",
            &serde_json::to_string(&tool_args)?,
        ),
        ev_model_verification_metadata("resp-1", vec![TRUSTED_ACCESS_FOR_CYBER_VERIFICATION]),
        core_test_support::responses::ev_completed("resp-1"),
    ]));
    let second_response = sse_response(sse(vec![
        ev_response_created("resp-2"),
        ev_model_verification_metadata("resp-2", vec![TRUSTED_ACCESS_FOR_CYBER_VERIFICATION]),
        ev_assistant_message("msg-1", "done"),
        core_test_support::responses::ev_completed("resp-2"),
    ]));
    let _mock = mount_response_sequence(&server, vec![first_response, second_response]).await;

    let mut builder = test_codex().with_model(SERVER_MODEL);
    let test = builder.build(&server).await?;

    test.codex
        .submit(disabled_text_turn(
            &test,
            "trigger follow-up model verification",
        ))
        .await?;

    let mut verification_count = 0;
    loop {
        let event = wait_for_event(&test.codex, |_| true).await;
        match event {
            EventMsg::ModelVerification(_) => verification_count += 1,
            EventMsg::Warning(warning) if warning.message.contains("high-risk cyber activity") => {
                panic!("model verification should not emit a warning event");
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert_eq!(verification_count, 1);

    Ok(())
}

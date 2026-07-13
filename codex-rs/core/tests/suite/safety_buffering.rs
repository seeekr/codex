use anyhow::Ok;
use codex_config::types::ModelSettingsPolicy;
use codex_config::types::ServerModelValidation;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SafetyBufferingEvent;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::sync::oneshot;

const FASTER_MODEL: &str = "faster-model";
const STRICT_MODEL: &str = "gpt-5.3-codex";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emits_safety_buffering_with_the_header_fallback_model() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut created = ev_response_created("resp-1");
    created["safety_buffering"] = json!({
        "use_cases": ["cyber"],
        "reasons": ["policy-check"],
    });
    mount_response_once(
        &server,
        sse_response(sse(vec![created, ev_completed("resp-1")]))
            .insert_header("x-codex-safety-buffering-enabled", "true")
            .insert_header("x-codex-safety-buffering-faster-model", FASTER_MODEL),
    )
    .await;

    let test = test_codex().build(&server).await?;
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Check this request".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let event = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::SafetyBuffering(event) => Some(event.clone()),
        _ => None,
    })
    .await;
    assert_eq!(
        event,
        SafetyBufferingEvent {
            model: test.session_configured.model.clone(),
            use_cases: vec!["cyber".to_string()],
            reasons: vec!["policy-check".to_string()],
            show_buffering_ui: true,
            faster_model: Some(FASTER_MODEL.to_string()),
        }
    );
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emits_safety_buffering_with_the_responses_api_model_without_header_gating()
-> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut created = ev_response_created("resp-1");
    created["safety_buffering"] = json!({
        "use_cases": ["cyber"],
        "reasons": ["policy-check"],
        "retry_model": FASTER_MODEL,
    });
    mount_response_once(
        &server,
        sse_response(sse(vec![created, ev_completed("resp-1")])),
    )
    .await;

    let test = test_codex().build(&server).await?;
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Check this request".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let event = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::SafetyBuffering(event) => Some(event.clone()),
        _ => None,
    })
    .await;
    assert_eq!(
        event,
        SafetyBufferingEvent {
            model: test.session_configured.model.clone(),
            use_cases: vec!["cyber".to_string()],
            reasons: vec!["policy-check".to_string()],
            show_buffering_ui: true,
            faster_model: Some(FASTER_MODEL.to_string()),
        }
    );
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn locked_model_settings_remove_safety_buffering_retry_model() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut created = ev_response_created("resp-1");
    created["safety_buffering"] = json!({
        "use_cases": ["cyber"],
        "reasons": ["policy-check"],
        "retry_model": FASTER_MODEL,
    });
    mount_response_once(
        &server,
        sse_response(sse(vec![created, ev_completed("resp-1")])),
    )
    .await;

    let test = test_codex()
        .with_config(|config| {
            config.model_settings_policy = ModelSettingsPolicy::Locked;
        })
        .build(&server)
        .await?;
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Check this request".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let event = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::SafetyBuffering(event) => Some(event.clone()),
        _ => None,
    })
    .await;
    assert_eq!(event.faster_model, None);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn required_server_model_match_preserves_safety_retry_when_model_settings_are_mutable()
-> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let mut created = ev_response_created("resp-1");
    created["response"]["headers"] = json!({ "OpenAI-Model": STRICT_MODEL });
    created["safety_buffering"] = json!({
        "use_cases": ["cyber"],
        "reasons": ["policy-check"],
        "retry_model": FASTER_MODEL,
    });
    let (complete_tx, complete_rx) = oneshot::channel();
    let chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse(vec![created]),
        },
        StreamingSseChunk {
            gate: Some(complete_rx),
            body: sse(vec![ev_completed("resp-1")]),
        },
    ];
    let (server, _completions) = start_streaming_sse_server(vec![chunks]).await;

    let test = test_codex()
        .with_model(STRICT_MODEL)
        .with_config(|config| {
            config.server_model_validation = ServerModelValidation::RequireMatch;
        })
        .build_with_streaming_server(&server)
        .await?;
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Check this request".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    // The completion chunk is still gated, so receiving this proves strict validation does not
    // leave the client blank while it waits for the full response and final attestations.
    let event = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::SafetyBuffering(event) => Some(event.clone()),
        _ => None,
    })
    .await;
    assert_eq!(
        event,
        SafetyBufferingEvent {
            model: STRICT_MODEL.to_string(),
            use_cases: vec!["cyber".to_string()],
            reasons: vec!["policy-check".to_string()],
            show_buffering_ui: true,
            faster_model: Some(FASTER_MODEL.to_string()),
        }
    );

    complete_tx
        .send(())
        .expect("release the response completion chunk");
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::SafetyBuffering(_) => panic!("strict validation replayed duplicate status"),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    server.shutdown().await;

    Ok(())
}

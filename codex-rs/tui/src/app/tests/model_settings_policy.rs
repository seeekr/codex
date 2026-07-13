use super::*;
use codex_config::types::ApprovalsReviewerPolicy;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn locked_policy_rejects_direct_safety_retry_before_side_effects() -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;

    app.chat_widget.set_model("gpt-5.4");
    app.chat_widget
        .set_reasoning_effort(Some(ReasoningEffortConfig::Ultra));
    app.config.model_settings_policy = ModelSettingsPolicy::Locked;
    app.chat_widget
        .set_model_settings_policy_for_tests(ModelSettingsPolicy::Locked);

    let thread_id = ThreadId::new();
    app.chat_widget.handle_thread_session(ThreadSessionState {
        model: "gpt-5.4".to_string(),
        reasoning_effort: Some(ReasoningEffortConfig::Ultra),
        ..test_thread_session(thread_id, test_path_buf("/tmp/project"))
    });
    app.active_thread_id = Some(thread_id);

    while app_event_rx.try_recv().is_ok() {}
    let turn = Op::user_turn(
        Vec::new(),
        test_path_buf("/tmp/project"),
        AskForApproval::Never,
        /*active_permission_profile*/ None,
        "gpt-5.4".to_string(),
        Some(ReasoningEffortConfig::Ultra),
        /*summary*/ None,
        /*service_tier*/ None,
        /*final_output_json_schema*/ None,
        /*collaboration_mode*/ None,
        /*personality*/ None,
    );

    let turn_id = "locked-safety-buffered-turn";
    app.chat_widget
        .record_safety_buffering_turn(turn_id.to_string(), &turn);
    app.chat_widget.handle_server_notification(
        ServerNotification::TurnStarted(TurnStartedNotification {
            thread_id: thread_id.to_string(),
            turn: Turn {
                id: turn_id.to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: TurnStatus::InProgress,
                error: None,
                started_at: Some(0),
                completed_at: None,
                duration_ms: None,
            },
        }),
        /*replay_kind*/ None,
    );
    app.chat_widget.handle_server_notification(
        ServerNotification::ModelSafetyBufferingUpdated(
            codex_app_server_protocol::ModelSafetyBufferingUpdatedNotification {
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
                model: "gpt-5.4".to_string(),
                use_cases: Vec::new(),
                reasons: Vec::new(),
                show_buffering_ui: true,
                faster_model: Some("gpt-5.4-mini".to_string()),
            },
        ),
        /*replay_kind*/ None,
    );
    assert!(app.chat_widget.can_retry_safety_buffered_turn(turn_id));

    while app_event_rx.try_recv().is_ok() {}
    let model_before = app.chat_widget.current_model().to_string();
    let effort_before = app.chat_widget.current_reasoning_effort();
    let transcript_len_before = app.transcript_cells.len();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let mut app_server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let request_id_before = app_server.next_request_id_for_tests();

    let control = Box::pin(app.handle_event(
        &mut tui,
        &mut app_server,
        AppEvent::RetrySafetyBufferedTurn {
            thread_id,
            turn_id: turn_id.to_string(),
            model: "gpt-5.4-mini".to_string(),
            turn,
        },
    ))
    .await?;

    assert!(matches!(control, AppRunControl::Continue));
    assert_eq!(app_server.next_request_id_for_tests(), request_id_before);
    assert_eq!(app.chat_widget.current_model(), model_before);
    assert_eq!(app.chat_widget.current_reasoning_effort(), effort_before);
    assert_eq!(app.transcript_cells.len(), transcript_len_before);
    assert!(app.chat_widget.can_retry_safety_buffered_turn(turn_id));
    let events = std::iter::from_fn(|| app_event_rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        events.iter().all(|event| !matches!(
            event,
            AppEvent::UpdateModel(_) | AppEvent::UpdateReasoningEffort(_)
        )),
        "direct retry changed locked model settings: {events:?}"
    );

    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn locked_reviewer_policy_rejects_direct_tui_update_without_persisting() -> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let codex_home = tempdir()?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.approvals_reviewer = ApprovalsReviewer::AutoReview;
    app.config.approvals_reviewer_policy = ApprovalsReviewerPolicy::Locked;
    app.chat_widget
        .set_approvals_reviewer(ApprovalsReviewer::AutoReview);
    app.chat_widget
        .set_approvals_reviewer_policy_for_tests(ApprovalsReviewerPolicy::Locked);
    std::fs::write(
        codex_home.path().join("config.toml"),
        "approvals_reviewer = \"auto_review\"\napprovals_reviewer_policy = \"locked\"\n",
    )?;

    let mut tui = crate::tui::test_support::make_test_tui()?;
    let mut app_server = start_config_write_test_app_server(&app).await?;
    let request_id_before = app_server.next_request_id_for_tests();

    let control = app
        .handle_event(
            &mut tui,
            &mut app_server,
            AppEvent::UpdateApprovalsReviewer(ApprovalsReviewer::User),
        )
        .await?;

    assert!(matches!(control, AppRunControl::Continue));
    assert_eq!(app_server.next_request_id_for_tests(), request_id_before);
    assert_eq!(app.config.approvals_reviewer, ApprovalsReviewer::AutoReview);
    assert_eq!(
        app.chat_widget.config_ref().approvals_reviewer,
        ApprovalsReviewer::AutoReview
    );
    let config = std::fs::read_to_string(codex_home.path().join("config.toml"))?;
    assert!(config.contains("approvals_reviewer = \"auto_review\""));
    assert!(config.contains("approvals_reviewer_policy = \"locked\""));

    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn locked_reviewer_policy_preserves_reviewer_across_permission_profile_selection()
-> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let codex_home = tempdir()?;
    let selected_config = codex_home.path().join("work.config.toml");
    std::fs::write(
        &selected_config,
        r#"
default_permissions = "locked-down"

[permissions.locked-down.filesystem]
":minimal" = "read"
"#,
    )?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.approvals_reviewer = ApprovalsReviewer::AutoReview;
    app.config.approvals_reviewer_policy = ApprovalsReviewerPolicy::Locked;
    app.chat_widget
        .set_approvals_reviewer(ApprovalsReviewer::AutoReview);
    app.chat_widget
        .set_approvals_reviewer_policy_for_tests(ApprovalsReviewerPolicy::Locked);
    app.loader_overrides.user_config_path = Some(selected_config.abs());

    assert!(
        app.apply_permission_profile_selection(PermissionProfileSelection {
            profile_id: "locked-down".to_string(),
            approval_policy: None,
            approvals_reviewer: Some(ApprovalsReviewer::User),
            display_label: "locked-down".to_string(),
        })
        .await
    );

    assert_eq!(app.config.approvals_reviewer, ApprovalsReviewer::AutoReview);
    assert_eq!(
        app.chat_widget.config_ref().approvals_reviewer,
        ApprovalsReviewer::AutoReview
    );
    let op = match app_event_rx.try_recv() {
        Ok(AppEvent::CodexOp(op)) => op,
        other => panic!("expected CodexOp event, got {other:?}"),
    };
    assert!(matches!(
        op,
        Op::OverrideTurnContext {
            approvals_reviewer: None,
            permission_profile: Some(_),
            ..
        }
    ));
    Ok(())
}

#[tokio::test]
async fn locked_reviewer_policy_survives_guardian_feature_disable() -> Result<()> {
    Box::pin(async {
        let (mut app, _app_event_rx, mut op_rx) = make_test_app_with_channels().await;
        let codex_home = tempdir()?;
        app.config.codex_home = codex_home.path().to_path_buf().abs();
        let config_toml_path = codex_home.path().join("config.toml").abs();
        let config_toml = "approvals_reviewer = \"auto_review\"\napprovals_reviewer_policy = \"locked\"\n\n[features]\nguardian_approval = true\n";
        std::fs::write(config_toml_path.as_path(), config_toml)?;
        let user_config = toml::from_str::<TomlValue>(config_toml)?;
        app.config.config_layer_stack = app
            .config
            .config_layer_stack
            .with_user_config(&config_toml_path, user_config);
        app.config.approvals_reviewer = ApprovalsReviewer::AutoReview;
        app.config.approvals_reviewer_policy = ApprovalsReviewerPolicy::Locked;
        app.config
            .features
            .set_enabled(Feature::GuardianApproval, /*enabled*/ true)?;
        app.chat_widget
            .set_approvals_reviewer(ApprovalsReviewer::AutoReview);
        app.chat_widget
            .set_approvals_reviewer_policy_for_tests(ApprovalsReviewerPolicy::Locked);
        app.chat_widget
            .set_feature_enabled(Feature::GuardianApproval, /*enabled*/ true);
        let mut app_server = start_config_write_test_app_server(&app).await?;

        app.update_feature_flags(&mut app_server, vec![(Feature::GuardianApproval, false)])
            .await;

        assert!(!app.config.features.enabled(Feature::GuardianApproval));
        assert_eq!(app.config.approvals_reviewer, ApprovalsReviewer::AutoReview);
        assert_eq!(
            app.chat_widget.config_ref().approvals_reviewer,
            ApprovalsReviewer::AutoReview
        );
        assert!(op_rx.try_recv().is_err());
        let config = std::fs::read_to_string(config_toml_path)?;
        assert!(config.contains("approvals_reviewer = \"auto_review\""));
        assert!(config.contains("approvals_reviewer_policy = \"locked\""));

        app_server.shutdown().await?;
        Ok(())
    })
    .await
}

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn guardian_cwd_preserves_drive_shaped_local_posix_path() {
    let native_cwd = AbsolutePathBuf::try_from(std::path::PathBuf::from("/C:/workspace"))
        .expect("drive-shaped POSIX path should be absolute");
    let cwd = PathUri::from_abs_path(&native_cwd);

    assert_eq!(
        guardian_cwd(codex_exec_server::LOCAL_ENVIRONMENT_ID, cwd)
            .expect("local cwd should retain the host path convention"),
        native_cwd
    );
}

#[test]
fn guardian_cwd_rejects_foreign_remote_path() {
    let cwd = PathUri::parse("file:///C:/workspace").expect("valid Windows path URI");

    assert!(guardian_cwd(codex_exec_server::REMOTE_ENVIRONMENT_ID, cwd).is_err());
}

#[test]
fn delegated_reviewer_unavailability_keeps_retry_guidance_and_automated_source() {
    let resolution = normalize_user_rejection(ApprovalResolution {
        decision: ReviewDecision::ReviewerUnavailable,
        rejection: None,
        source: ApprovalResolutionSource::User,
    });

    assert_eq!(resolution.source, ApprovalResolutionSource::Guardian);
    let message = resolution
        .rejection
        .expect("reviewer unavailability must include retry guidance");
    assert!(message.contains("temporarily unavailable"));
    assert!(message.contains("not a risk denial"));
    assert!(message.contains("Retry with backoff"));
    assert!(message.contains("do not treat this as disapproval"));
}

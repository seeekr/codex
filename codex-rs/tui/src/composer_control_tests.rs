use std::collections::HashMap;
use std::ops::Range;
use std::time::Instant;

use super::*;

struct FakeTarget {
    available: bool,
    thread_id: String,
    text: String,
    cursor: usize,
    next_lease: u64,
    leases: HashMap<u64, Range<usize>>,
}

impl FakeTarget {
    fn new(text: &str) -> Self {
        Self {
            available: true,
            thread_id: "synthetic-thread".to_string(),
            text: text.to_string(),
            cursor: text.len(),
            next_lease: 1,
            leases: HashMap::new(),
        }
    }

    fn user_insert(&mut self, position: usize, text: &str) {
        let position = position.min(self.text.len());
        self.apply_insert_to_leases(position, text.len());
        self.text.insert_str(position, text);
        if self.cursor >= position {
            self.cursor += text.len();
        }
    }

    fn apply_insert_to_leases(&mut self, position: usize, inserted_len: usize) {
        self.leases.retain(|_, range| {
            if position <= range.start {
                range.start += inserted_len;
                range.end += inserted_len;
                true
            } else {
                position >= range.end
            }
        });
    }

    fn apply_replace_to_leases(&mut self, edit_range: Range<usize>, replacement_len: usize) {
        let removed_len = edit_range.end - edit_range.start;
        let delta = replacement_len as isize - removed_len as isize;
        self.leases.retain(|_, range| {
            if edit_range.end <= range.start {
                range.start = range.start.saturating_add_signed(delta);
                range.end = range.end.saturating_add_signed(delta);
                true
            } else {
                edit_range.start >= range.end
            }
        });
    }
}

impl ComposerControlTarget for FakeTarget {
    type Lease = u64;

    fn thread_id(&self) -> Option<String> {
        Some(self.thread_id.clone())
    }

    fn snapshot(&self) -> Option<ComposerSnapshot> {
        self.available
            .then(|| ComposerSnapshot::new(self.thread_id.clone(), self.text.clone(), self.cursor))
    }

    fn insert_owned_text(&mut self, text: &str) -> Result<Self::Lease, ComposerLeaseError> {
        if text.is_empty() {
            return Err(ComposerLeaseError::EmptyText);
        }
        let start = self.cursor.min(self.text.len());
        self.apply_insert_to_leases(start, text.len());
        self.text.insert_str(start, text);
        self.cursor = start + text.len();
        let lease = self.next_lease;
        self.next_lease += 1;
        self.leases.insert(lease, start..self.cursor);
        Ok(lease)
    }

    fn verify_owned_text(
        &self,
        lease: Self::Lease,
        expected: &str,
    ) -> Result<(), ComposerLeaseError> {
        let range = self
            .leases
            .get(&lease)
            .ok_or(ComposerLeaseError::LeaseUnavailable)?;
        if self.text.get(range.clone()) != Some(expected) {
            return Err(ComposerLeaseError::ExpectedTextMismatch);
        }
        Ok(())
    }

    fn keep_owned_text(&mut self, lease: Self::Lease) -> Result<(), ComposerLeaseError> {
        self.leases
            .remove(&lease)
            .map(|_| ())
            .ok_or(ComposerLeaseError::LeaseUnavailable)
    }

    fn replace_owned_text(
        &mut self,
        lease: Self::Lease,
        expected: &str,
        replacement: &str,
    ) -> Result<(), ComposerLeaseError> {
        let Some(range) = self.leases.remove(&lease) else {
            return Err(ComposerLeaseError::LeaseUnavailable);
        };
        if self.text.get(range.clone()) != Some(expected) {
            return Err(ComposerLeaseError::ExpectedTextMismatch);
        }
        if self.cursor >= range.start && self.cursor < range.end {
            return Err(ComposerLeaseError::CursorConflicts);
        }
        self.apply_replace_to_leases(range.clone(), replacement.len());
        self.text.replace_range(range.clone(), replacement);
        let removed_len = range.end - range.start;
        self.cursor = if self.cursor < range.start {
            self.cursor
        } else if self.cursor <= range.end {
            range.start + replacement.len()
        } else {
            self.cursor - removed_len + replacement.len()
        };
        Ok(())
    }
}

fn capture<L: Copy, T: ComposerControlTarget<Lease = L>>(
    state: &mut ComposerControlState<L>,
    target: &mut T,
) -> Uuid {
    match state.execute(
        ComposerCommand::Capture,
        target,
        /*app_overlay_active*/ false,
    ) {
        WireResult::Captured { capture_id } => capture_id,
        _ => panic!("synthetic capture should succeed"),
    }
}

fn insert<L: Copy, T: ComposerControlTarget<Lease = L>>(
    state: &mut ComposerControlState<L>,
    target: &mut T,
    capture_id: Uuid,
    text: &str,
) -> Uuid {
    match state.execute(
        ComposerCommand::Insert {
            capture_id,
            text: text.to_string(),
        },
        target,
        /*app_overlay_active*/ false,
    ) {
        WireResult::Inserted { lease_id } => lease_id,
        _ => panic!("synthetic insert should succeed"),
    }
}

#[test]
fn capture_insert_verify_keep_and_replace_are_serialized() {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("draft");

    let first_capture = capture(&mut state, &mut target);
    let first_lease = insert(&mut state, &mut target, first_capture, " fast");
    let second_capture = capture(&mut state, &mut target);
    let second_lease = insert(&mut state, &mut target, second_capture, " pass");

    target.user_insert(/*position*/ 0, "prefix ");
    assert!(matches!(
        state.execute(
            ComposerCommand::Verify {
                lease_id: first_lease,
                expected: " fast".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Verified
    ));
    assert!(matches!(
        state.execute(
            ComposerCommand::Replace {
                lease_id: first_lease,
                expected: " fast".to_string(),
                replacement: " final".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Replaced
    ));
    assert!(matches!(
        state.execute(
            ComposerCommand::Verify {
                lease_id: second_lease,
                expected: " pass".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Verified
    ));
    assert!(matches!(
        state.execute(
            ComposerCommand::Replace {
                lease_id: second_lease,
                expected: " pass".to_string(),
                replacement: " done".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Replaced
    ));

    let kept_capture = capture(&mut state, &mut target);
    let kept_lease = insert(&mut state, &mut target, kept_capture, " kept");
    assert!(matches!(
        state.execute(
            ComposerCommand::Keep {
                lease_id: kept_lease,
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Kept
    ));
    assert_eq!(target.text, "prefix draft final done kept");
}

#[test]
fn capture_compare_and_swap_rejects_intervening_input_and_snapshot_changes() {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("draft");
    let input_capture = capture(&mut state, &mut target);
    state.note_tui_event(&TuiEvent::Paste("synthetic".to_string()));
    assert!(matches!(
        state.execute(
            ComposerCommand::Insert {
                capture_id: input_capture,
                text: " fast".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Error {
            code: ErrorCode::CaptureChanged,
            outcome: MutationOutcome::NotApplied,
        }
    ));

    let cursor_capture = capture(&mut state, &mut target);
    target.cursor = 0;
    assert!(matches!(
        state.execute(
            ComposerCommand::Insert {
                capture_id: cursor_capture,
                text: "fast ".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Error {
            code: ErrorCode::CaptureChanged,
            ..
        }
    ));

    target.cursor = target.text.len();
    let thread_capture = capture(&mut state, &mut target);
    target.thread_id = "other-synthetic-thread".to_string();
    assert!(matches!(
        state.execute(
            ComposerCommand::Insert {
                capture_id: thread_capture,
                text: " fast".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Error {
            code: ErrorCode::CaptureChanged,
            ..
        }
    ));
}

#[test]
fn verify_mismatch_is_retryable_but_replace_mismatch_is_terminal() {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("draft");
    let capture_id = capture(&mut state, &mut target);
    let lease_id = insert(&mut state, &mut target, capture_id, " fast");
    let before = target.text.clone();

    assert!(matches!(
        state.execute(
            ComposerCommand::Verify {
                lease_id,
                expected: "wrong".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Error {
            code: ErrorCode::ExpectedMismatch,
            outcome: MutationOutcome::NotApplied,
        }
    ));
    assert_eq!(target.text, before);
    assert!(matches!(
        state.execute(
            ComposerCommand::Verify {
                lease_id,
                expected: " fast".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Verified
    ));

    assert!(matches!(
        state.execute(
            ComposerCommand::Replace {
                lease_id,
                expected: "wrong".to_string(),
                replacement: "final".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Error {
            code: ErrorCode::ExpectedMismatch,
            outcome: MutationOutcome::NotApplied,
        }
    ));
    assert_eq!(target.text, before);
    assert!(matches!(
        state.execute(
            ComposerCommand::Verify {
                lease_id,
                expected: " fast".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Error {
            code: ErrorCode::LeaseUnavailable,
            ..
        }
    ));
    assert!(target.leases.is_empty());
}

#[test]
fn unavailable_surfaces_and_expired_ui_requests_do_not_mutate() {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("draft");
    assert!(matches!(
        state.execute(
            ComposerCommand::Capture,
            &mut target,
            /*app_overlay_active*/ true,
        ),
        WireResult::Error {
            code: ErrorCode::ComposerUnavailable,
            ..
        }
    ));

    let (reply, reply_rx) = std::sync::mpsc::sync_channel(1);
    state.handle_ui_request(
        ComposerControlRequest {
            command: ComposerCommand::Capture,
            deadline: Instant::now() - Duration::from_millis(1),
            reply,
        },
        &mut target,
        /*app_overlay_active*/ false,
    );
    assert!(matches!(
        reply_rx.recv().expect("UI reply"),
        WireResult::Error {
            code: ErrorCode::UiTimeout,
            outcome: MutationOutcome::NotApplied,
        }
    ));
    assert_eq!(target.text, "draft");
}

#[test]
fn wire_validation_is_strict_and_rejects_submit_shaped_text() {
    let instance_id = Uuid::new_v4();
    let capture_id = Uuid::new_v4();
    let request: WireRequest = serde_json::from_value(serde_json::json!({
        "protocolVersion": PROTOCOL_VERSION,
        "instanceId": instance_id,
        "op": "insert",
        "captureId": capture_id,
        "text": "line\nsubmit"
    }))
    .expect("wire request");
    assert!(matches!(
        request.into_command(instance_id),
        Err(ErrorCode::InvalidText)
    ));

    assert!(
        serde_json::from_value::<WireRequest>(serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "instanceId": instance_id,
            "op": "capture",
            "unexpected": true
        }))
        .is_err()
    );

    let wrong_version: WireRequest = serde_json::from_value(serde_json::json!({
        "protocolVersion": 9,
        "instanceId": instance_id,
        "op": "capture"
    }))
    .expect("wire request");
    assert!(matches!(
        wrong_version.into_command(instance_id),
        Err(ErrorCode::UnsupportedVersion)
    ));
}

#[cfg(unix)]
#[test]
fn iterm_identity_maps_to_bounded_instance_socket_path() {
    let guid = Uuid::new_v4();
    let instance_id = Uuid::new_v4();
    let parsed =
        session_guid_from_value(&format!("w0t0p0:{guid}")).expect("iTerm session GUID suffix");
    assert_eq!(parsed, guid.to_string());
    let path = socket_path(
        std::path::Path::new("/tmp/codex-cc"),
        &parsed,
        u32::MAX,
        instance_id,
    );
    assert_eq!(
        path,
        std::path::Path::new("/tmp/codex-cc")
            .join(guid.to_string())
            .join(format!("{}-{instance_id}.sock", u32::MAX))
    );
    assert!(path.as_os_str().len() <= 103);
}

#[cfg(unix)]
#[test]
fn unix_socket_round_trip_returns_only_metadata() {
    use std::io::BufRead;
    use std::io::BufReader;
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    let root = tempfile::Builder::new()
        .prefix("cc-")
        .tempdir_in("/tmp")
        .expect("short temporary root");
    let session_guid = Uuid::new_v4().to_string();
    let instance_id = Uuid::new_v4();
    let (request_tx, mut request_rx) = unbounded_channel();
    let server = ComposerControlServer::bind(root.path(), &session_guid, instance_id, request_tx)
        .expect("bind synthetic composer-control listener");

    let responder = std::thread::spawn(move || {
        let request = request_rx
            .blocking_recv()
            .expect("listener should dispatch one UI request");
        let mut state = ComposerControlState::<u64>::new();
        let mut target = FakeTarget::new("private synthetic draft");
        state.handle_ui_request(request, &mut target, /*app_overlay_active*/ false);
    });

    let mut stream = UnixStream::connect(&server.socket_path).expect("connect synthetic client");
    let request = serde_json::json!({
        "protocolVersion": PROTOCOL_VERSION,
        "instanceId": instance_id,
        "op": "capture"
    });
    serde_json::to_writer(&mut stream, &request).expect("write request");
    stream.write_all(b"\n").expect("terminate request");

    let mut response_line = String::new();
    BufReader::new(stream)
        .read_line(&mut response_line)
        .expect("read response");
    let response: serde_json::Value = serde_json::from_str(&response_line).expect("response JSON");
    let object = response.as_object().expect("response object");
    assert_eq!(object.get("protocolVersion"), Some(&serde_json::json!(1)));
    assert_eq!(
        object.get("instanceId"),
        Some(&serde_json::json!(instance_id))
    );
    assert_eq!(object.get("status"), Some(&serde_json::json!("captured")));
    assert!(object.get("captureId").is_some());
    assert_eq!(object.len(), 4);
    assert!(!response_line.contains("private synthetic draft"));

    responder.join().expect("UI responder");
    drop(server);
}

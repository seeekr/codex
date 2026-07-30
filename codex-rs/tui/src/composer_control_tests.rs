use std::collections::HashMap;
use std::collections::HashSet;
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
    submission_keys: Vec<crossterm::event::KeyEvent>,
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
            submission_keys: vec![
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Enter,
                    crossterm::event::KeyModifiers::NONE,
                ),
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Tab,
                    crossterm::event::KeyModifiers::NONE,
                ),
            ],
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

    fn is_submission_event(&self, event: &TuiEvent) -> bool {
        let TuiEvent::Key(key) = event else {
            return false;
        };
        self.submission_keys.iter().any(|binding| {
            binding.code == key.code
                && binding.modifiers == key.modifiers
                && binding.kind == key.kind
        })
    }
}

fn capture<L: Copy + Eq, T: ComposerControlTarget<Lease = L>>(
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

fn insert<L: Copy + Eq, T: ComposerControlTarget<Lease = L>>(
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

fn mark_submission_pending(
    state: &mut ComposerControlState<u64>,
    target: &FakeTarget,
    lease_ids: &[Uuid],
) -> Uuid {
    let native_leases = lease_ids
        .iter()
        .map(|lease_id| {
            let lease = state.leases.get(lease_id).expect("external lease");
            match lease.state {
                ExternalLeaseState::Draft { native } => (
                    native,
                    target
                        .leases
                        .get(&native)
                        .expect("native lease range")
                        .clone(),
                ),
                _ => panic!("lease should still be draft-owned"),
            }
        })
        .collect::<Vec<_>>();
    let submission_id = Uuid::new_v4();
    state.note_submission_pending_by_native(
        "synthetic-thread",
        submission_id,
        Arc::from(target.text.as_str()),
        &native_leases,
    );
    submission_id
}

fn mark_submitted(
    state: &mut ComposerControlState<u64>,
    target: &FakeTarget,
    lease_ids: &[Uuid],
) -> Uuid {
    let submission_id = mark_submission_pending(state, target, lease_ids);
    state.note_submission_committed_id("synthetic-thread", submission_id);
    submission_id
}

fn pending_correction(
    state: &mut ComposerControlState<u64>,
    target: &mut FakeTarget,
    lease_id: Uuid,
    expected: &str,
    replacement: &str,
) -> PreparedCorrection {
    match state.prepare_command(
        ComposerCommand::Replace {
            lease_id,
            expected: expected.to_string(),
            replacement: replacement.to_string(),
        },
        target,
        /*app_overlay_active*/ false,
    ) {
        CommandExecution::Correct(correction) => correction,
        CommandExecution::Complete(_) => panic!("submitted replacement should dispatch correction"),
        CommandExecution::Submit(_) => panic!("replacement must not dispatch a submission"),
    }
}

fn start_submit(
    state: &mut ComposerControlState<u64>,
    target: &mut FakeTarget,
    lease_id: Uuid,
    expected: &str,
) -> (
    PendingComposerSubmission<u64>,
    std::sync::mpsc::Receiver<WireResult>,
) {
    let (reply, reply_rx) = std::sync::mpsc::sync_channel(1);
    let action = state
        .handle_ui_request(
            ComposerControlRequest {
                command: ComposerCommand::Submit {
                    lease_id,
                    expected: expected.to_string(),
                },
                deadline: Instant::now() + Duration::from_secs(1),
                reply,
            },
            target,
            /*app_overlay_active*/ false,
        )
        .expect("exact draft should produce a pending submit action");
    let PendingComposerControlAction::Submission(pending) = action else {
        panic!("submit request must not produce a correction action");
    };
    (pending, reply_rx)
}

fn inserted_draft(expected: &str) -> (ComposerControlState<u64>, FakeTarget, Uuid) {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("");
    let capture_id = capture(&mut state, &mut target);
    let lease_id = insert(&mut state, &mut target, capture_id, expected);
    (state, target, lease_id)
}

fn synthetic_receipt(text: &str) -> SubmittedLeaseReceipt {
    SubmittedLeaseReceipt {
        submission_id: Uuid::new_v4(),
        submitted_text: Arc::from(text),
        range: 0..text.len(),
    }
}

fn stored_lease(
    state: ExternalLeaseState<u64>,
    thread_id: &str,
    submit_attempt: Option<SubmitAttempt>,
) -> ExternalLease<u64> {
    ExternalLease {
        state,
        thread_id: thread_id.to_string(),
        expected_hash: text_hash("pinned"),
        draft_witness: DraftWitness {
            text_hash: text_hash("pinned"),
            cursor: "pinned".len(),
            input_epoch: 0,
        },
        submit_attempt,
    }
}

fn push_stored_lease(state: &mut ComposerControlState<u64>, lease: ExternalLease<u64>) -> Uuid {
    let lease_id = Uuid::new_v4();
    state.leases.insert(lease_id, lease);
    state.lease_order.push_back(lease_id);
    lease_id
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
fn lease_capacity_reclaims_oldest_confirmed_terminal_even_after_unknown_dispatch() {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("human draft");
    let mut oldest_id = None;
    for index in 0..MAX_LEASE_COUNT {
        let text = format!("submitted-{index}");
        let lease_id = push_stored_lease(
            &mut state,
            stored_lease(
                ExternalLeaseState::SubmittedIntact {
                    receipt: synthetic_receipt(&text),
                },
                &format!("settled-{index}"),
                Some(SubmitAttempt::Resolved {
                    result: if index == 0 {
                        WireResult::Unknown
                    } else {
                        WireResult::SendAccepted
                    },
                    fence: None,
                }),
            ),
        );
        oldest_id.get_or_insert(lease_id);
    }

    let capture_id = capture(&mut state, &mut target);
    let inserted_id = insert(&mut state, &mut target, capture_id, " + dictated");
    let oldest_id = oldest_id.expect("terminal lease");
    assert_eq!(target.text, "human draft + dictated");
    assert_eq!(target.leases.len(), 1);
    assert!(!state.leases.contains_key(&oldest_id));
    assert!(state.leases.contains_key(&inserted_id));
    assert_eq!(state.leases.len(), MAX_LEASE_COUNT);
    assert!(matches!(
        state.execute(
            ComposerCommand::Verify {
                lease_id: oldest_id,
                expected: "submitted-0".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Error {
            code: ErrorCode::LeaseUnavailable,
            outcome: MutationOutcome::NotApplied,
        }
    ));
}

#[test]
fn lease_capacity_reclaims_oldest_settled_draft_without_removing_its_text() {
    let mut state = ComposerControlState::<u64>::new();
    let settled_text = "x".repeat(MAX_LEASE_COUNT);
    let mut target = FakeTarget::new(&settled_text);
    target.next_lease = MAX_LEASE_COUNT as u64 + 1;
    let mut oldest_id = None;
    for index in 0..MAX_LEASE_COUNT {
        let native = index as u64 + 1;
        target.leases.insert(native, index..index + 1);
        let lease_id = push_stored_lease(
            &mut state,
            ExternalLease {
                state: ExternalLeaseState::Draft { native },
                thread_id: target.thread_id.clone(),
                expected_hash: text_hash("x"),
                draft_witness: DraftWitness {
                    text_hash: text_hash(&settled_text),
                    cursor: settled_text.len(),
                    input_epoch: 0,
                },
                submit_attempt: None,
            },
        );
        oldest_id.get_or_insert(lease_id);
    }

    let capture_id = capture(&mut state, &mut target);
    let inserted_id = insert(&mut state, &mut target, capture_id, "new");
    let oldest_id = oldest_id.expect("settled draft lease");
    assert_eq!(target.text, format!("{settled_text}new"));
    assert!(!target.leases.contains_key(&1));
    assert!(!state.leases.contains_key(&oldest_id));
    assert!(state.leases.contains_key(&inserted_id));
    assert_eq!(state.leases.len(), MAX_LEASE_COUNT);
    assert_eq!(target.leases.len(), MAX_LEASE_COUNT);
}

#[test]
fn lease_capacity_keeps_unknown_and_correction_pending_leases_pinned() {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("");
    let unknown_id = push_stored_lease(
        &mut state,
        stored_lease(
            ExternalLeaseState::SubmittedIntact {
                receipt: synthetic_receipt("submitted"),
            },
            "unknown-thread",
            Some(SubmitAttempt::Resolved {
                result: WireResult::Unknown,
                fence: Some(SubmitFence::new()),
            }),
        ),
    );
    let correction_id = push_stored_lease(
        &mut state,
        stored_lease(
            ExternalLeaseState::CorrectionPending {
                receipt: synthetic_receipt("submitted"),
                correction_id: Uuid::new_v4(),
                expected: "submitted".to_string(),
                replacement: "corrected".to_string(),
                payload: "correction".to_string(),
            },
            "correction-thread",
            Some(SubmitAttempt::Resolved {
                result: WireResult::SendAccepted,
                fence: None,
            }),
        ),
    );
    let mut oldest_terminal = None;
    for index in 2..MAX_LEASE_COUNT {
        let lease_id = push_stored_lease(
            &mut state,
            stored_lease(
                if index % 2 == 0 {
                    ExternalLeaseState::SubmissionAbandoned {
                        receipt: synthetic_receipt("submitted"),
                    }
                } else {
                    ExternalLeaseState::SubmittedIntact {
                        receipt: synthetic_receipt("submitted"),
                    }
                },
                &format!("settled-{index}"),
                Some(SubmitAttempt::Resolved {
                    result: if index % 2 == 0 {
                        WireResult::SubmissionAbandoned
                    } else {
                        WireResult::SendAccepted
                    },
                    fence: None,
                }),
            ),
        );
        oldest_terminal.get_or_insert(lease_id);
    }

    let capture_id = capture(&mut state, &mut target);
    let inserted_id = insert(&mut state, &mut target, capture_id, "new");
    assert!(state.leases.contains_key(&unknown_id));
    assert!(state.leases.contains_key(&correction_id));
    assert!(
        !state
            .leases
            .contains_key(&oldest_terminal.expect("terminal lease"))
    );
    assert!(state.leases.contains_key(&inserted_id));
    assert_eq!(state.leases.len(), MAX_LEASE_COUNT);
    assert_eq!(target.text, "new");
}

#[test]
fn lease_capacity_fails_visibly_without_mutating_when_every_lease_is_pinned() {
    let mut all_pinned = ComposerControlState::<u64>::new();
    let mut untouched = FakeTarget::new("");
    for index in 0..MAX_LEASE_COUNT {
        let lease = match index {
            0 => stored_lease(
                ExternalLeaseState::Draft { native: 10_000 },
                "other-thread",
                None,
            ),
            1 => stored_lease(
                ExternalLeaseState::Draft { native: 10_001 },
                "dispatching-thread",
                Some(SubmitAttempt::Dispatching {
                    fence: SubmitFence::new(),
                }),
            ),
            2 => stored_lease(
                ExternalLeaseState::Draft { native: 10_002 },
                "synthetic-thread",
                Some(SubmitAttempt::Resolved {
                    result: WireResult::Unknown,
                    fence: None,
                }),
            ),
            3 => stored_lease(
                ExternalLeaseState::CorrectionPending {
                    receipt: synthetic_receipt("submitted"),
                    correction_id: Uuid::new_v4(),
                    expected: "submitted".to_string(),
                    replacement: "corrected".to_string(),
                    payload: "correction".to_string(),
                },
                "correction-thread",
                Some(SubmitAttempt::Resolved {
                    result: WireResult::SendAccepted,
                    fence: None,
                }),
            ),
            _ => stored_lease(
                ExternalLeaseState::SubmissionPending {
                    receipt: synthetic_receipt("submitted"),
                },
                &format!("pending-{index}"),
                Some(SubmitAttempt::Resolved {
                    result: WireResult::SendAccepted,
                    fence: None,
                }),
            ),
        };
        push_stored_lease(&mut all_pinned, lease);
    }
    let pinned_ids = all_pinned.leases.keys().copied().collect::<HashSet<_>>();
    let blocked_capture = capture(&mut all_pinned, &mut untouched);
    assert!(matches!(
        all_pinned.execute(
            ComposerCommand::Insert {
                capture_id: blocked_capture,
                text: "must not appear".to_string(),
            },
            &mut untouched,
            /*app_overlay_active*/ false,
        ),
        WireResult::Error {
            code: ErrorCode::LeaseUnavailable,
            outcome: MutationOutcome::NotApplied,
        }
    ));
    assert_eq!(untouched.text, "");
    assert_eq!(untouched.cursor, 0);
    assert_eq!(untouched.next_lease, 1);
    assert!(untouched.leases.is_empty());
    assert!(all_pinned.captures.contains_key(&blocked_capture));
    assert_eq!(all_pinned.leases.len(), MAX_LEASE_COUNT);
    assert_eq!(
        all_pinned.leases.keys().copied().collect::<HashSet<_>>(),
        pinned_ids
    );
}

#[test]
fn captures_rebase_only_across_acknowledged_native_mutations() {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("draft");

    let capture_a = capture(&mut state, &mut target);
    let capture_b = capture(&mut state, &mut target);
    let lease_a = insert(&mut state, &mut target, capture_a, " A");
    let lease_b = insert(&mut state, &mut target, capture_b, " B");
    assert_eq!(target.text, "draft A B");

    let capture_c = capture(&mut state, &mut target);
    let capture_d = capture(&mut state, &mut target);
    let lease_c = insert(&mut state, &mut target, capture_c, " C");
    state.note_tui_event(&TuiEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('x'),
        crossterm::event::KeyModifiers::NONE,
    )));
    assert!(matches!(
        state.execute(
            ComposerCommand::Insert {
                capture_id: capture_d,
                text: " D".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Error {
            code: ErrorCode::CaptureChanged,
            outcome: MutationOutcome::NotApplied,
        }
    ));

    let capture_e = capture(&mut state, &mut target);
    assert!(matches!(
        state.execute(
            ComposerCommand::Replace {
                lease_id: lease_c,
                expected: " C".to_string(),
                replacement: " corrected C".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Replaced
    ));
    let _lease_e = insert(&mut state, &mut target, capture_e, " E");
    assert_eq!(target.text, "draft A B corrected C E");

    assert!(state.leases.contains_key(&lease_a));
    assert!(state.leases.contains_key(&lease_b));
}

#[test]
fn accepted_submission_survives_composer_clear_for_verify_and_keep() {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("");
    let capture_id = capture(&mut state, &mut target);
    let lease_id = insert(&mut state, &mut target, capture_id, "transcribed");
    mark_submitted(&mut state, &target, &[lease_id]);

    target.thread_id = "different-thread".to_string();
    target.text.clear();
    target.cursor = 0;
    target.leases.clear();
    assert!(matches!(
        state.execute(
            ComposerCommand::Verify {
                lease_id,
                expected: "transcribed".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::SubmittedIntact
    ));
    assert!(matches!(
        state.execute(
            ComposerCommand::Keep { lease_id },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Kept
    ));
    assert!(target.text.is_empty());
}

#[test]
fn dispatching_submit_consumes_default_and_custom_queue_bindings() {
    let (mut state, mut target, lease_id) = inserted_draft("dictated");
    let custom_queue = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('q'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    target.submission_keys.push(custom_queue);
    let (pending, reply_rx) = start_submit(&mut state, &mut target, lease_id, "dictated");
    let dispatch_fence = pending.fence();

    let default_queue = TuiEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Tab,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(
        state.prepare_tui_event_during_submission(&default_queue, &target, &dispatch_fence),
        TuiEventDisposition::BlockDispatchingSubmission { disclose: true }
    );
    let ordinary_input = TuiEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('x'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(
        state.prepare_tui_event_during_submission(&ordinary_input, &target, &dispatch_fence),
        TuiEventDisposition::Allow
    );
    assert_eq!(
        state.prepare_tui_event_during_submission(
            &TuiEvent::Key(custom_queue),
            &target,
            &dispatch_fence,
        ),
        TuiEventDisposition::BlockDispatchingSubmission { disclose: false }
    );

    state.finish_submission(
        pending,
        SubmissionDispatchOutcome::NotApplied(ErrorCode::SubmissionUnavailable),
    );
    assert_eq!(
        reply_rx.recv().expect("submit response"),
        WireResult::not_applied(ErrorCode::SubmissionUnavailable)
    );
}

#[test]
fn explicit_submit_refusal_preserves_the_draft_and_consumes_queued_submit_events() {
    let (mut state, mut target, lease_id) = inserted_draft("dictated");
    let (pending, reply_rx) = start_submit(&mut state, &mut target, lease_id, "dictated");
    let dispatch_fence = pending.fence();

    state.finish_submission(
        pending,
        SubmissionDispatchOutcome::NotApplied(ErrorCode::SubmissionUnavailable),
    );
    assert_eq!(
        reply_rx.recv().expect("submit response"),
        WireResult::not_applied(ErrorCode::SubmissionUnavailable)
    );
    assert_eq!(target.text, "dictated");

    let enter = TuiEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(
        state.prepare_tui_event_during_submission(&enter, &target, &dispatch_fence),
        TuiEventDisposition::BlockDispatchingSubmission { disclose: true }
    );
    assert_eq!(
        state.prepare_tui_event_during_submission(&enter, &target, &dispatch_fence),
        TuiEventDisposition::BlockDispatchingSubmission { disclose: false }
    );
    assert_eq!(
        state.prepare_tui_event(&enter, &target),
        TuiEventDisposition::Allow
    );

    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Submit {
                lease_id,
                expected: "dictated".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::Error {
            code: ErrorCode::SubmissionUnavailable,
            outcome: MutationOutcome::NotApplied,
        })
    ));
    assert!(matches!(
        state.execute(
            ComposerCommand::Verify {
                lease_id,
                expected: "dictated".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Verified
    ));
}

#[test]
fn accepted_submit_consumes_dispatch_window_enter_after_the_draft_is_cleared() {
    let (mut state, mut target, lease_id) = inserted_draft("dictated");
    let (pending, reply_rx) = start_submit(&mut state, &mut target, lease_id, "dictated");
    let dispatch_fence = pending.fence();
    mark_submission_pending(&mut state, &target, &[lease_id]);

    target.text.clear();
    target.cursor = 0;
    target.leases.clear();
    state.finish_submission(pending, SubmissionDispatchOutcome::Accepted);
    assert_eq!(
        reply_rx.recv().expect("submit response"),
        WireResult::SendAccepted
    );

    let enter = TuiEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(
        state.prepare_tui_event_during_submission(&enter, &target, &dispatch_fence),
        TuiEventDisposition::BlockDispatchingSubmission { disclose: true }
    );
}

#[test]
fn dispatch_window_enter_is_consumed_after_an_earlier_buffered_edit() {
    let (mut state, mut target, lease_id) = inserted_draft("dictated");
    let (pending, _reply_rx) = start_submit(&mut state, &mut target, lease_id, "dictated");
    let dispatch_fence = pending.fence();
    state.finish_submission(
        pending,
        SubmissionDispatchOutcome::NotApplied(ErrorCode::SubmissionUnavailable),
    );

    let edit = TuiEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('x'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(
        state.prepare_tui_event_during_submission(&edit, &target, &dispatch_fence),
        TuiEventDisposition::Allow
    );
    state.note_tui_event(&edit);
    target.user_insert(target.text.len(), "x");
    state.finish_tui_event(&target);

    let enter = TuiEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(
        state.prepare_tui_event_during_submission(&enter, &target, &dispatch_fence),
        TuiEventDisposition::BlockDispatchingSubmission { disclose: true }
    );
}

#[test]
fn accepted_submit_is_pending_until_commit_and_retries_observe_stable_state() {
    let (mut state, mut target, lease_id) = inserted_draft("dictated");
    let (pending, reply_rx) = start_submit(&mut state, &mut target, lease_id, "dictated");

    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Submit {
                lease_id,
                expected: "dictated".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::Unknown)
    ));

    let submission_id = mark_submission_pending(&mut state, &target, &[lease_id]);
    state.finish_submission(pending, SubmissionDispatchOutcome::Accepted);
    assert_eq!(
        reply_rx.recv().expect("submit response"),
        WireResult::SendAccepted
    );
    assert!(matches!(
        state.execute(
            ComposerCommand::Verify {
                lease_id,
                expected: "dictated".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::SubmissionPending
    ));
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Submit {
                lease_id,
                expected: "dictated".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::SendAccepted)
    ));

    state.note_submission_committed_id("synthetic-thread", submission_id);
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Submit {
                lease_id,
                expected: "dictated".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::SendAccepted)
    ));
}

#[test]
fn unknown_submit_fences_submit_and_queue_bindings_until_the_user_edits() {
    let (mut state, mut target, lease_id) = inserted_draft("dictated");
    let custom_queue = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('q'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    target.submission_keys.push(custom_queue);
    let (pending, reply_rx) = start_submit(&mut state, &mut target, lease_id, "dictated");
    let fence = pending.fence();
    mark_submission_pending(&mut state, &target, &[lease_id]);
    state.finish_submission(pending, SubmissionDispatchOutcome::AcceptedButUncommitted);
    assert_eq!(
        reply_rx.recv().expect("submit response"),
        WireResult::Unknown
    );

    let default_queue = TuiEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Tab,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(
        state.prepare_tui_event(&default_queue, &target),
        TuiEventDisposition::BlockUnknownSubmission { disclose: true }
    );
    assert_eq!(
        state.prepare_tui_event(&TuiEvent::Key(custom_queue), &target),
        TuiEventDisposition::BlockUnknownSubmission { disclose: false }
    );
    let enter = TuiEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(matches!(
        state.prepare_tui_event(&enter, &target),
        TuiEventDisposition::BlockUnknownSubmission { disclose: false }
    ));
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Submit {
                lease_id,
                expected: "dictated".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::Unknown)
    ));

    let edit = TuiEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('x'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(
        state.prepare_tui_event(&edit, &target),
        TuiEventDisposition::Allow
    );
    assert!(!fence.is_relinquished());
    state.note_tui_event(&edit);
    target.user_insert(target.text.len(), "x");
    state.finish_tui_event(&target);
    assert!(fence.is_relinquished());
    assert_eq!(
        state.prepare_tui_event(&enter, &target),
        TuiEventDisposition::Allow
    );
    assert_eq!(
        state.prepare_tui_event(&default_queue, &target),
        TuiEventDisposition::Allow
    );
    assert_eq!(
        state.prepare_tui_event(&TuiEvent::Key(custom_queue), &target),
        TuiEventDisposition::Allow
    );
    assert!(matches!(
        state.execute(
            ComposerCommand::Verify {
                lease_id,
                expected: "dictated".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Unknown
    ));
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Submit {
                lease_id,
                expected: "dictated".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::Unknown)
    ));
}

#[test]
fn unknown_submit_latch_blocks_native_mutation_and_survives_malformed_retries() {
    let (mut state, mut target, lease_id) = inserted_draft("dictated");
    let (native, range) = match &state.leases.get(&lease_id).expect("lease").state {
        ExternalLeaseState::Draft { native } => (
            *native,
            target.leases.get(native).expect("native range").clone(),
        ),
        _ => panic!("new lease must own the draft"),
    };
    let (pending, reply_rx) = start_submit(&mut state, &mut target, lease_id, "dictated");
    let fence = pending.fence();
    state.finish_submission(pending, SubmissionDispatchOutcome::Unknown);
    assert_eq!(
        reply_rx.recv().expect("submit response"),
        WireResult::Unknown
    );

    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Submit {
                lease_id,
                expected: "wrong".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::Unknown)
    ));
    let enter = TuiEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(matches!(
        state.prepare_tui_event(&enter, &target),
        TuiEventDisposition::BlockUnknownSubmission { .. }
    ));

    let second_capture = capture(&mut state, &mut target);
    assert_eq!(
        state.execute(
            ComposerCommand::Insert {
                capture_id: second_capture,
                text: " more".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Unknown
    );
    assert_eq!(target.text, "dictated");
    assert!(state.captures.contains_key(&second_capture));
    assert_eq!(
        state.execute(
            ComposerCommand::Keep { lease_id },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Unknown
    );
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Replace {
                lease_id,
                expected: "dictated".to_string(),
                replacement: "corrected".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::Unknown)
    ));
    assert_eq!(target.text, "dictated");

    let edit = TuiEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('x'),
        crossterm::event::KeyModifiers::NONE,
    ));
    state.note_tui_event(&edit);
    target.user_insert(target.text.len(), "x");
    state.finish_tui_event(&target);
    assert!(fence.is_relinquished());
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Replace {
                lease_id,
                expected: "dictated".to_string(),
                replacement: "corrected".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::Unknown)
    ));
    let recovery_capture = capture(&mut state, &mut target);
    let recovery_lease = insert(&mut state, &mut target, recovery_capture, " new recording");
    assert_eq!(target.text, "dictatedx new recording");
    assert!(state.leases.contains_key(&recovery_lease));

    let submission_id = Uuid::new_v4();
    state.note_submission_pending_by_native(
        "synthetic-thread",
        submission_id,
        Arc::from("dictated"),
        &[(native, range)],
    );
    state.note_submission_committed_id("synthetic-thread", submission_id);
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Replace {
                lease_id,
                expected: "dictated".to_string(),
                replacement: "corrected".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Correct(_)
    ));
}

#[test]
fn live_unknown_draft_does_not_block_an_independent_submitted_correction() {
    let (mut state, mut target, submitted_lease) = inserted_draft("first");
    mark_submitted(&mut state, &target, &[submitted_lease]);
    target.text.clear();
    target.cursor = 0;
    target.leases.clear();

    let unknown_capture = capture(&mut state, &mut target);
    let unknown_lease = insert(&mut state, &mut target, unknown_capture, "second");
    let (pending, reply_rx) = start_submit(&mut state, &mut target, unknown_lease, "second");
    state.finish_submission(pending, SubmissionDispatchOutcome::Unknown);
    assert_eq!(
        reply_rx.recv().expect("submit response"),
        WireResult::Unknown
    );

    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Replace {
                lease_id: submitted_lease,
                expected: "first".to_string(),
                replacement: "corrected first".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Correct(_)
    ));
}

#[test]
fn pending_submission_cannot_be_retired_before_commit_truth() {
    let (mut state, mut target, lease_id) = inserted_draft("dictated");
    let (pending, reply_rx) = start_submit(&mut state, &mut target, lease_id, "dictated");
    let submission_id = mark_submission_pending(&mut state, &target, &[lease_id]);
    state.finish_submission(pending, SubmissionDispatchOutcome::Accepted);
    assert_eq!(
        reply_rx.recv().expect("submit response"),
        WireResult::SendAccepted
    );

    assert_eq!(
        state.execute(
            ComposerCommand::Keep { lease_id },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::SubmissionPending
    );
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Replace {
                lease_id,
                expected: "dictated".to_string(),
                replacement: "dictated".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::SubmissionPending)
    ));
    assert!(state.leases.contains_key(&lease_id));

    state.note_submission_committed_id("synthetic-thread", submission_id);
    assert_eq!(
        state.execute(
            ComposerCommand::Keep { lease_id },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Kept
    );
}

#[test]
fn manual_enter_before_socket_submit_is_unknown_until_commit_notification() {
    let (mut state, mut target, lease_id) = inserted_draft("dictated");
    let (native, range) = match &state.leases.get(&lease_id).expect("lease").state {
        ExternalLeaseState::Draft { native } => (
            *native,
            target.leases.get(native).expect("native range").clone(),
        ),
        _ => panic!("new lease must own the draft"),
    };
    let enter = TuiEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));

    assert_eq!(
        state.prepare_tui_event(&enter, &target),
        TuiEventDisposition::Allow
    );
    state.note_tui_event(&enter);
    target.text.clear();
    target.cursor = 0;
    target.leases.clear();
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Submit {
                lease_id,
                expected: "dictated".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::Unknown)
    ));

    let submission_id = Uuid::new_v4();
    state.note_submission_pending_by_native(
        "synthetic-thread",
        submission_id,
        Arc::from("dictated"),
        &[(native, range)],
    );
    state.note_submission_committed_id("synthetic-thread", submission_id);
    assert!(matches!(
        state.execute(
            ComposerCommand::Verify {
                lease_id,
                expected: "dictated".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::SubmittedIntact
    ));
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Submit {
                lease_id,
                expected: "dictated".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::Unknown)
    ));
}

#[test]
fn rejected_manual_enter_does_not_masquerade_as_an_in_flight_send() {
    let (mut state, mut target, lease_id) = inserted_draft("dictated");
    let enter = TuiEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    state.note_tui_event(&enter);

    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Submit {
                lease_id,
                expected: "dictated".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::Error {
            code: ErrorCode::CaptureChanged,
            outcome: MutationOutcome::NotApplied,
        })
    ));
    assert_eq!(target.text, "dictated");
}

#[test]
fn unrelated_or_rejected_actions_do_not_relinquish_an_unknown_fence() {
    let (mut state, mut target, lease_id) = inserted_draft("dictated");
    let (pending, reply_rx) = start_submit(&mut state, &mut target, lease_id, "dictated");
    state.finish_submission(pending, SubmissionDispatchOutcome::Unknown);
    assert_eq!(
        reply_rx.recv().expect("submit response"),
        WireResult::Unknown
    );

    assert!(matches!(
        state.execute(
            ComposerCommand::Insert {
                capture_id: Uuid::new_v4(),
                text: "ignored".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Error {
            code: ErrorCode::CaptureUnavailable,
            ..
        }
    ));
    assert!(matches!(
        state.execute(
            ComposerCommand::Keep {
                lease_id: Uuid::new_v4(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Error {
            code: ErrorCode::LeaseUnavailable,
            ..
        }
    ));
    assert!(matches!(
        state.execute(
            ComposerCommand::Replace {
                lease_id,
                expected: "wrong".to_string(),
                replacement: "ignored".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Unknown
    ));

    let shortcut = TuiEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('?'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
    assert_eq!(
        state.prepare_tui_event(&shortcut, &target),
        TuiEventDisposition::Allow
    );
    state.note_tui_event(&shortcut);
    target.cursor = 0;
    state.finish_tui_event(&target);

    let enter = TuiEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(matches!(
        state.prepare_tui_event(&enter, &target),
        TuiEventDisposition::BlockUnknownSubmission { .. }
    ));
}

#[test]
fn another_thread_cannot_relinquish_an_unknown_draft_fence() {
    let (mut state, mut target, lease_id) = inserted_draft("dictated");
    let (pending, reply_rx) = start_submit(&mut state, &mut target, lease_id, "dictated");
    state.finish_submission(pending, SubmissionDispatchOutcome::Unknown);
    assert_eq!(
        reply_rx.recv().expect("submit response"),
        WireResult::Unknown
    );

    target.thread_id = "other-thread".to_string();
    target.text = "other draft".to_string();
    target.cursor = target.text.len();
    let other_edit = TuiEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('x'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(
        state.prepare_tui_event(&other_edit, &target),
        TuiEventDisposition::Allow
    );
    state.note_tui_event(&other_edit);
    state.finish_tui_event(&target);

    target.thread_id = "synthetic-thread".to_string();
    target.text = "dictated".to_string();
    target.cursor = target.text.len();
    let enter = TuiEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(matches!(
        state.prepare_tui_event(&enter, &target),
        TuiEventDisposition::BlockUnknownSubmission { .. }
    ));
}

#[test]
fn submit_admission_requires_the_exact_inserted_draft_witness() {
    let (mut state, mut target, lease_id) = inserted_draft("dictated");
    target.user_insert(0, "changed ");
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Submit {
                lease_id,
                expected: "dictated".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::Error {
            code: ErrorCode::CaptureChanged,
            outcome: MutationOutcome::NotApplied,
        })
    ));

    let (mut state, mut target, lease_id) = inserted_draft("dictated");
    target.cursor = 0;
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Submit {
                lease_id,
                expected: "dictated".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::Error {
            code: ErrorCode::CaptureChanged,
            ..
        })
    ));

    let (mut state, mut target, lease_id) = inserted_draft("dictated");
    state.note_tui_event(&TuiEvent::Paste("intervening".to_string()));
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Submit {
                lease_id,
                expected: "dictated".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::Error {
            code: ErrorCode::CaptureChanged,
            ..
        })
    ));

    let (mut state, mut target, lease_id) = inserted_draft("dictated");
    target.thread_id = "other-thread".to_string();
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Submit {
                lease_id,
                expected: "dictated".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::Unknown)
    ));

    let (mut state, mut target, lease_id) = inserted_draft("dictated");
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Submit {
                lease_id,
                expected: "different".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::Error {
            code: ErrorCode::ExpectedMismatch,
            outcome: MutationOutcome::NotApplied,
        })
    ));
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Submit {
                lease_id,
                expected: "dictated".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::Error {
            code: ErrorCode::ExpectedMismatch,
            outcome: MutationOutcome::NotApplied,
        })
    ));
}

#[test]
fn submission_stays_pending_until_matching_user_message_commit() {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("");
    let capture_id = capture(&mut state, &mut target);
    let lease_id = insert(&mut state, &mut target, capture_id, "parakeat");
    let submission_id = mark_submission_pending(&mut state, &target, &[lease_id]);

    target.thread_id = "different-thread".to_string();
    target.text.clear();
    target.cursor = 0;
    target.leases.clear();
    assert!(matches!(
        state.execute(
            ComposerCommand::Verify {
                lease_id,
                expected: "parakeat".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::SubmissionPending
    ));
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Replace {
                lease_id,
                expected: "parakeat".to_string(),
                replacement: "Parakeet".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::SubmissionPending)
    ));

    state.note_submission_committed_id("synthetic-thread", submission_id);
    assert!(matches!(
        state.execute(
            ComposerCommand::Verify {
                lease_id,
                expected: "parakeat".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::SubmittedIntact
    ));
}

#[test]
fn rollback_invalidates_uncommitted_state_but_preserves_surviving_submission_leases() {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("");
    let first_capture = capture(&mut state, &mut target);
    let first_lease = insert(&mut state, &mut target, first_capture, "first");
    let submission_id = mark_submission_pending(&mut state, &target, &[first_lease]);
    state.note_submission_abandoned_id("synthetic-thread", submission_id);
    assert!(matches!(
        state.leases.get(&first_lease).map(|lease| &lease.state),
        Some(ExternalLeaseState::SubmissionAbandoned { .. })
    ));
    assert!(matches!(
        state.execute(
            ComposerCommand::Verify {
                lease_id: first_lease,
                expected: "first".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::SubmissionAbandoned
    ));

    let second_capture = capture(&mut state, &mut target);
    let second_lease = insert(&mut state, &mut target, second_capture, " second");
    let second_submission_id = mark_submitted(&mut state, &target, &[second_lease]);
    let surviving = HashSet::from([second_submission_id]);
    state.note_thread_rolled_back("synthetic-thread", &surviving);
    assert!(state.captures.is_empty());
    assert!(matches!(
        state.leases.get(&second_lease).map(|lease| &lease.state),
        Some(ExternalLeaseState::SubmittedIntact { .. })
    ));

    let replacement = || ComposerCommand::Replace {
        lease_id: second_lease,
        expected: " second".to_string(),
        replacement: " Second".to_string(),
    };
    let CommandExecution::Correct(pending) = state.prepare_command(
        replacement(),
        &mut target,
        /*app_overlay_active*/ false,
    ) else {
        panic!("surviving submitted lease should still dispatch its correction");
    };
    assert_eq!(pending.thread_id, "synthetic-thread");

    state.note_thread_rolled_back("synthetic-thread", &surviving);
    assert!(matches!(
        state.prepare_command(replacement(), &mut target, /*app_overlay_active*/ false),
        CommandExecution::Correct(retry)
            if retry.thread_id == "synthetic-thread"
                && retry.correction_id == pending.correction_id
    ));
}

#[test]
fn rollback_survivor_keeps_the_local_clear_fence_until_disposition() {
    let (mut state, mut target, lease_id) = inserted_draft("retained");
    let (pending, reply_rx) = start_submit(&mut state, &mut target, lease_id, "retained");
    let submission_id = mark_submission_pending(&mut state, &target, &[lease_id]);
    state.finish_submission(pending, SubmissionDispatchOutcome::AcceptedButUncommitted);
    assert_eq!(
        reply_rx.recv().expect("submit response"),
        WireResult::Unknown
    );

    state.note_thread_rolled_back("synthetic-thread", &HashSet::from([submission_id]));
    assert!(matches!(
        state.leases.get(&lease_id).map(|lease| &lease.state),
        Some(ExternalLeaseState::SubmittedIntact { .. })
    ));

    let enter = TuiEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(matches!(
        state.prepare_tui_event(&enter, &target),
        TuiEventDisposition::BlockUnknownSubmission { .. }
    ));
    let insert_capture = capture(&mut state, &mut target);
    assert_eq!(
        state.execute(
            ComposerCommand::Insert {
                capture_id: insert_capture,
                text: " duplicate".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Unknown
    );
    assert_eq!(
        state.execute(
            ComposerCommand::Keep { lease_id },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Unknown
    );
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Replace {
                lease_id,
                expected: "retained".to_string(),
                replacement: "corrected".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::Unknown)
    ));
    assert_eq!(target.text, "retained");

    target.text.clear();
    target.cursor = 0;
    target.leases.clear();
    state.note_submission_committed_id("synthetic-thread", submission_id);
    let post_commit_capture = capture(&mut state, &mut target);
    let inserted_id = insert(&mut state, &mut target, post_commit_capture, "after commit");
    assert!(state.leases.contains_key(&inserted_id));
    assert_eq!(target.text, "after commit");
}

#[test]
fn rollback_marks_a_removed_committed_submission_abandoned() {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("");
    let capture_id = capture(&mut state, &mut target);
    let lease_id = insert(&mut state, &mut target, capture_id, "removed");
    mark_submitted(&mut state, &target, &[lease_id]);

    state.note_thread_rolled_back("synthetic-thread", &HashSet::new());
    assert!(matches!(
        state.execute(
            ComposerCommand::Verify {
                lease_id,
                expected: "removed".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::SubmissionAbandoned
    ));
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Replace {
                lease_id,
                expected: "removed".to_string(),
                replacement: "replacement".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::SubmissionAbandoned)
    ));
}

#[test]
fn submission_receipt_is_xml_safe_and_unambiguously_names_the_following_message() {
    let submission = NativeComposerSubmission::new(
        "synthetic-thread".to_string(),
        "owned",
        vec![SubmittedComposerLease {
            id: ComposerLeaseId::for_test(1),
            range: 0..5,
        }],
    )
    .expect("submission");
    let (key, value) = submission.receipt_context();

    assert!(key.starts_with("koenig_transcription_receipt_"));
    assert!(!key.contains('/'));
    assert!(
        key.chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    );
    assert!(value.contains(&submission.client_user_message_id()));
    assert!(value.contains("immediately following user message"));
}

#[test]
fn merged_submission_rebases_image_placeholder_edits_outside_owned_text() {
    let original = "[Image #9] parakeat";
    let owned_start = original.find("parakeat").expect("owned text");
    let submission = NativeComposerSubmission::new(
        "synthetic-thread".to_string(),
        original,
        vec![SubmittedComposerLease {
            id: ComposerLeaseId::for_test(1),
            range: owned_start..owned_start + "parakeat".len(),
        }],
    )
    .expect("submission");
    let remapped = "[Image #10] parakeat";

    let merged =
        NativeComposerSubmission::merge_parts([(remapped.to_string(), Some(submission))], remapped)
            .expect("placeholder edit outside the lease should preserve provenance");
    let rebased = &merged.leases[0].range;
    assert_eq!(merged.submitted_text.as_ref(), remapped);
    assert_eq!(merged.submitted_text.get(rebased.clone()), Some("parakeat"));
    assert_eq!(rebased.start, owned_start + 1);

    let placeholder_submission = NativeComposerSubmission::new(
        "synthetic-thread".to_string(),
        original,
        vec![SubmittedComposerLease {
            id: ComposerLeaseId::for_test(2),
            range: 0.."[Image #9]".len(),
        }],
    )
    .expect("placeholder submission");
    assert!(
        NativeComposerSubmission::merge_parts(
            [(remapped.to_string(), Some(placeholder_submission))],
            remapped,
        )
        .is_none(),
        "normalization intersecting owned text must fail closed"
    );
}

#[test]
fn cleared_or_edited_draft_without_acceptance_is_not_submitted() {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("");
    let capture_id = capture(&mut state, &mut target);
    let lease_id = insert(&mut state, &mut target, capture_id, "transcribed");

    target.text.clear();
    target.cursor = 0;
    target.leases.clear();
    assert!(matches!(
        state.execute(
            ComposerCommand::Verify {
                lease_id,
                expected: "transcribed".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Error {
            code: ErrorCode::LeaseUnavailable,
            outcome: MutationOutcome::NotApplied,
        }
    ));
}

#[test]
fn submitted_replace_is_same_thread_application_correction_with_ack_semantics() {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("");
    let capture_id = capture(&mut state, &mut target);
    let lease_id = insert(&mut state, &mut target, capture_id, "parakeat result");
    let submission_id = mark_submitted(&mut state, &target, &[lease_id]);
    target.text.clear();
    target.cursor = 0;
    target.leases.clear();

    let correction = pending_correction(
        &mut state,
        &mut target,
        lease_id,
        "parakeat result",
        "Parakeet result",
    );
    assert_eq!(correction.thread_id, "synthetic-thread");
    assert_eq!(correction.correction_id.get_version_num(), 4);
    assert!(
        correction
            .payload
            .contains(&format!("koenig-composer-{submission_id}"))
    );
    assert!(
        correction
            .payload
            .contains("application context, not a user message")
    );

    let (reply, reply_rx) = std::sync::mpsc::sync_channel(1);
    state.finish_correction(
        PendingComposerCorrection {
            lease_id: correction.lease_id,
            thread_id: correction.thread_id,
            correction_id: correction.correction_id,
            expected_client_user_message_id: correction.expected_client_user_message_id,
            payload: correction.payload,
            reply,
        },
        CorrectionDispatchOutcome::Accepted,
    );
    assert!(matches!(
        reply_rx.recv().expect("correction reply"),
        WireResult::CorrectionQueued
    ));
    assert!(matches!(
        state.execute(
            ComposerCommand::Verify {
                lease_id,
                expected: "parakeat result".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        WireResult::Error {
            code: ErrorCode::LeaseUnavailable,
            ..
        }
    ));
}

#[test]
fn submitted_correction_retries_definite_rejection_or_ambiguity_without_identity_loss() {
    for (outcome, remains_submitted) in [
        (CorrectionDispatchOutcome::NotApplied, true),
        (CorrectionDispatchOutcome::Unknown, false),
    ] {
        let mut state = ComposerControlState::<u64>::new();
        let mut target = FakeTarget::new("");
        let capture_id = capture(&mut state, &mut target);
        let lease_id = insert(&mut state, &mut target, capture_id, "parakeat");
        mark_submitted(&mut state, &target, &[lease_id]);
        target.thread_id = "different-thread".to_string();
        target.text.clear();
        target.cursor = 0;
        target.leases.clear();
        let correction =
            pending_correction(&mut state, &mut target, lease_id, "parakeat", "Parakeet");
        let original_correction_id = correction.correction_id;
        let original_payload = correction.payload.clone();
        let (reply, reply_rx) = std::sync::mpsc::sync_channel(1);
        state.finish_correction(
            PendingComposerCorrection {
                lease_id: correction.lease_id,
                thread_id: correction.thread_id,
                correction_id: correction.correction_id,
                expected_client_user_message_id: correction.expected_client_user_message_id,
                payload: correction.payload,
                reply,
            },
            outcome,
        );
        let reply = reply_rx.recv().expect("correction reply");
        if remains_submitted {
            assert!(matches!(
                reply,
                WireResult::Error {
                    code: ErrorCode::CorrectionUnavailable,
                    outcome: MutationOutcome::NotApplied,
                }
            ));
        } else {
            assert!(matches!(reply, WireResult::CorrectionPending));
        }
        let verify = state.execute(
            ComposerCommand::Verify {
                lease_id,
                expected: "parakeat".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        );
        if remains_submitted {
            assert!(matches!(verify, WireResult::SubmittedIntact));
        } else {
            assert!(matches!(
                verify,
                WireResult::Error {
                    code: ErrorCode::LeaseUnavailable,
                    ..
                }
            ));
            let retry =
                pending_correction(&mut state, &mut target, lease_id, "parakeat", "Parakeet");
            assert_eq!(retry.correction_id, original_correction_id);
            assert_eq!(retry.payload, original_payload);
            assert!(matches!(
                state.prepare_command(
                    ComposerCommand::Replace {
                        lease_id,
                        expected: "parakeat".to_string(),
                        replacement: "different correction".to_string(),
                    },
                    &mut target,
                    /*app_overlay_active*/ false,
                ),
                CommandExecution::Complete(WireResult::Error {
                    code: ErrorCode::LeaseUnavailable,
                    outcome: MutationOutcome::NotApplied,
                })
            ));
        }
    }
}

#[test]
fn stale_correction_completion_does_not_mutate_a_newer_generation() {
    for stale_outcome in [
        CorrectionDispatchOutcome::Accepted,
        CorrectionDispatchOutcome::NotApplied,
    ] {
        let mut state = ComposerControlState::<u64>::new();
        let mut target = FakeTarget::new("");
        let capture_id = capture(&mut state, &mut target);
        let lease_id = insert(&mut state, &mut target, capture_id, "parakeat");
        mark_submitted(&mut state, &target, &[lease_id]);
        target.text.clear();
        target.cursor = 0;
        target.leases.clear();

        let first = pending_correction(&mut state, &mut target, lease_id, "parakeat", "Parakeet");
        let first_thread_id = first.thread_id.clone();
        let first_correction_id = first.correction_id;
        let first_expected_client_user_message_id = first.expected_client_user_message_id.clone();
        let first_payload = first.payload.clone();
        let (first_reply, first_reply_rx) = std::sync::mpsc::sync_channel(1);
        state.finish_correction(
            PendingComposerCorrection {
                lease_id,
                thread_id: first.thread_id,
                correction_id: first.correction_id,
                expected_client_user_message_id: first.expected_client_user_message_id,
                payload: first.payload,
                reply: first_reply,
            },
            CorrectionDispatchOutcome::NotApplied,
        );
        let _ = first_reply_rx.recv().expect("first correction reply");

        let second =
            pending_correction(&mut state, &mut target, lease_id, "parakeat", "Parakeet v2");
        assert_ne!(second.correction_id, first_correction_id);
        let second_correction_id = second.correction_id;
        let second_payload = second.payload.clone();

        let (stale_reply, stale_reply_rx) = std::sync::mpsc::sync_channel(1);
        state.finish_correction(
            PendingComposerCorrection {
                lease_id,
                thread_id: first_thread_id,
                correction_id: first_correction_id,
                expected_client_user_message_id: first_expected_client_user_message_id,
                payload: first_payload,
                reply: stale_reply,
            },
            stale_outcome,
        );
        let _ = stale_reply_rx.recv().expect("stale correction reply");

        let retry =
            pending_correction(&mut state, &mut target, lease_id, "parakeat", "Parakeet v2");
        assert_eq!(retry.correction_id, second_correction_id);
        assert_eq!(retry.payload, second_payload);
    }
}

#[test]
fn late_acceptance_removes_same_generation_after_definite_rejection() {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("");
    let capture_id = capture(&mut state, &mut target);
    let lease_id = insert(&mut state, &mut target, capture_id, "parakeat");
    mark_submitted(&mut state, &target, &[lease_id]);
    target.text.clear();
    target.cursor = 0;
    target.leases.clear();

    let correction = pending_correction(&mut state, &mut target, lease_id, "parakeat", "Parakeet");
    let thread_id = correction.thread_id.clone();
    let correction_id = correction.correction_id;
    let expected_client_user_message_id = correction.expected_client_user_message_id.clone();
    let payload = correction.payload.clone();
    let (rejected_reply, rejected_reply_rx) = std::sync::mpsc::sync_channel(1);
    state.finish_correction(
        PendingComposerCorrection {
            lease_id,
            thread_id: correction.thread_id,
            correction_id,
            expected_client_user_message_id: correction.expected_client_user_message_id,
            payload: correction.payload,
            reply: rejected_reply,
        },
        CorrectionDispatchOutcome::NotApplied,
    );
    let _ = rejected_reply_rx.recv().expect("rejected correction reply");

    let (accepted_reply, accepted_reply_rx) = std::sync::mpsc::sync_channel(1);
    state.finish_correction(
        PendingComposerCorrection {
            lease_id,
            thread_id,
            correction_id,
            expected_client_user_message_id,
            payload,
            reply: accepted_reply,
        },
        CorrectionDispatchOutcome::Accepted,
    );
    assert!(matches!(
        accepted_reply_rx.recv().expect("accepted correction reply"),
        WireResult::CorrectionQueued
    ));
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Replace {
                lease_id,
                expected: "parakeat".to_string(),
                replacement: "Parakeet v2".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Complete(WireResult::Error {
            code: ErrorCode::LeaseUnavailable,
            outcome: MutationOutcome::NotApplied,
        })
    ));
}

#[test]
fn submitted_replace_routes_by_origin_after_display_thread_switch() {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("");
    let capture_id = capture(&mut state, &mut target);
    let lease_id = insert(&mut state, &mut target, capture_id, "parakeat");
    mark_submitted(&mut state, &target, &[lease_id]);
    target.thread_id = "different-thread".to_string();
    target.text.clear();
    target.cursor = 0;

    let CommandExecution::Correct(correction) = state.prepare_command(
        ComposerCommand::Replace {
            lease_id,
            expected: "parakeat".to_string(),
            replacement: "Parakeet".to_string(),
        },
        &mut target,
        /*app_overlay_active*/ false,
    ) else {
        panic!("submitted correction should route by its stored origin");
    };
    assert_eq!(correction.thread_id, "synthetic-thread");
    assert!(matches!(
        state.prepare_command(
            ComposerCommand::Replace {
                lease_id,
                expected: "parakeat".to_string(),
                replacement: "Parakeet".to_string(),
            },
            &mut target,
            /*app_overlay_active*/ false,
        ),
        CommandExecution::Correct(retry)
            if retry.thread_id == "synthetic-thread"
                && retry.correction_id == correction.correction_id
    ));
}

#[test]
fn submitted_locator_is_global_and_never_claims_unowned_prefix_suffix_or_other_lease() {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("prefix parakeat | ");
    let first_capture = capture(&mut state, &mut target);
    let first_lease = insert(&mut state, &mut target, first_capture, "parakeat");
    target.user_insert(target.text.len(), " | middle parakeat | ");
    let second_capture = capture(&mut state, &mut target);
    let second_lease = insert(&mut state, &mut target, second_capture, "parakeat");
    target.user_insert(target.text.len(), " | suffix");

    let submitted_text = target.text.clone();
    let first_native = match state.leases[&first_lease].state {
        ExternalLeaseState::Draft { native } => native,
        _ => unreachable!(),
    };
    let second_native = match state.leases[&second_lease].state {
        ExternalLeaseState::Draft { native } => native,
        _ => unreachable!(),
    };
    let first_range = target.leases[&first_native].clone();
    let second_range = target.leases[&second_native].clone();
    mark_submitted(&mut state, &target, &[first_lease, second_lease]);
    target.text.clear();
    target.cursor = 0;
    target.leases.clear();

    let first = pending_correction(&mut state, &mut target, first_lease, "parakeat", "Parakeet");
    let second = pending_correction(
        &mut state,
        &mut target,
        second_lease,
        "parakeat",
        "Parakeet",
    );
    assert!(first.payload.contains(&format!(
        "Koenig-owned submitted-message bytes {}..{}",
        first_range.start, first_range.end
    )));
    assert!(first.payload.contains(&format!(
        "Hunk 1: old bytes {}..{}",
        first_range.start, first_range.end
    )));
    assert!(second.payload.contains(&format!(
        "Koenig-owned submitted-message bytes {}..{}",
        second_range.start, second_range.end
    )));
    assert!(second.payload.contains("occurrence 4"));
    assert!(!first.payload.contains("prefix parakeat"));
    assert!(!first.payload.contains("| suffix"));
    assert_eq!(
        submitted_text.get(first_range),
        Some("parakeat"),
        "first locator must resolve only the first Koenig-owned lease"
    );
    assert_eq!(
        submitted_text.get(second_range),
        Some("parakeat"),
        "second locator must resolve only the second Koenig-owned lease"
    );
}

#[test]
fn correction_hunks_are_compact_unique_and_deterministically_located() {
    let repeated_prefix = (0..14)
        .map(|index| format!("before{index}"))
        .collect::<Vec<_>>()
        .join(" ");
    let repeated_suffix = (0..14)
        .map(|index| format!("after{index}"))
        .collect::<Vec<_>>()
        .join(" ");
    let segment = format!("{repeated_prefix} parakeat {repeated_suffix}");
    let expected_repeated = format!("{segment} | {segment} | {segment}");
    let second_error_start = expected_repeated
        .match_indices("parakeat")
        .nth(1)
        .map(|(position, _)| position)
        .expect("second repeated error");
    let mut replacement_repeated = expected_repeated.clone();
    replacement_repeated.replace_range(
        second_error_start..second_error_start + "parakeat".len(),
        "Parakeet",
    );
    let repeated_context = mechanical_correction_context(
        Uuid::new_v4(),
        &expected_repeated,
        0..expected_repeated.len(),
        &expected_repeated,
        &replacement_repeated,
    )
    .expect("bounded repeated correction context");
    assert!(
        repeated_context.contains("occurrence 2"),
        "{repeated_context}"
    );

    let filler = (0..80)
        .map(|index| format!("filler{index}"))
        .collect::<Vec<_>>()
        .join(" ");
    let expected = format!("start parakeat {filler} second eror end");
    let replacement = format!("start Parakeet {filler} second error end");
    let first = mechanical_correction_context(
        Uuid::new_v4(),
        &expected,
        0..expected.len(),
        &expected,
        &replacement,
    )
    .expect("bounded multi-hunk correction context");
    let second = mechanical_correction_context(
        Uuid::new_v4(),
        &expected,
        0..expected.len(),
        &expected,
        &replacement,
    )
    .expect("bounded multi-hunk correction context");
    assert!(first.contains("Hunk 1:"));
    assert!(first.contains("Hunk 2:"));
    assert!(!first.contains(&filler));
    assert_ne!(
        first.lines().find(|line| line.contains("koenig-composer-")),
        second
            .lines()
            .find(|line| line.contains("koenig-composer-"))
    );
}

#[test]
fn long_correction_context_uses_only_the_changed_core_or_fails_closed() {
    let shared_prefix = "unchanged context ".repeat(30_000);
    let expected = format!("{shared_prefix}late parakeat tail");
    let replacement = format!("{shared_prefix}late Parakeet tail");
    let context = mechanical_correction_context(
        Uuid::new_v4(),
        &expected,
        0..expected.len(),
        &expected,
        &replacement,
    )
    .expect("a tiny correction in a long submission remains representable");

    assert!(context.len() < 4_096, "context was {} bytes", context.len());
    assert!(context.contains("parakeat"));
    assert!(context.contains("Parakeet"));
    assert!(!context.contains(&"unchanged context ".repeat(100)));

    let oversized_old = "a".repeat(20_000);
    let oversized_new = "b".repeat(20_000);
    assert!(
        mechanical_correction_context(
            Uuid::new_v4(),
            &oversized_old,
            0..oversized_old.len(),
            &oversized_old,
            &oversized_new,
        )
        .is_none(),
        "an unbounded changed core must be rejected instead of reproduced"
    );
}

#[test]
fn many_small_hunks_fail_before_the_real_additional_context_budget_would_truncate() {
    let spacer = (0..16)
        .map(|index| format!("stable{index}"))
        .collect::<Vec<_>>()
        .join(" ");
    let expected = (0..80)
        .map(|index| format!("bad{index} {spacer}"))
        .collect::<Vec<_>>()
        .join(" ");
    let replacement = (0..80)
        .map(|index| format!("good{index} {spacer}"))
        .collect::<Vec<_>>()
        .join(" ");

    assert!(
        mechanical_correction_context(
            Uuid::new_v4(),
            &expected,
            0..expected.len(),
            &expected,
            &replacement,
        )
        .is_none(),
        "many bounded hunks must fail closed before AdditionalContext silently truncates at the \
         shared token boundary"
    );
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
fn wire_validation_is_strict_and_accepts_literal_multiline_composer_text() {
    let instance_id = Uuid::new_v4();
    let capture_id = Uuid::new_v4();
    let lease_id = Uuid::new_v4();
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
        Ok(ComposerCommand::Insert { text, .. }) if text == "line\nsubmit"
    ));

    let request: WireRequest = serde_json::from_value(serde_json::json!({
        "protocolVersion": PROTOCOL_VERSION,
        "instanceId": instance_id,
        "op": "submit",
        "leaseId": lease_id,
        "expected": "line\nsubmit"
    }))
    .expect("wire request");
    assert!(matches!(
        request.into_command(instance_id),
        Ok(ComposerCommand::Submit {
            lease_id: parsed_lease,
            expected,
        }) if parsed_lease == lease_id && expected == "line\nsubmit"
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

    assert_eq!(
        serde_json::to_value(WireResponse {
            protocol_version: PROTOCOL_VERSION,
            instance_id,
            result: WireResult::SubmissionPending,
        })
        .expect("wire response"),
        serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "instanceId": instance_id,
            "status": "submission_pending",
        }),
        "slow-final clients receive a typed retryable status, not a target error"
    );
    assert_eq!(
        serde_json::to_value(WireResponse {
            protocol_version: PROTOCOL_VERSION,
            instance_id,
            result: WireResult::CorrectionQueued,
        })
        .expect("wire response"),
        serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "instanceId": instance_id,
            "status": "correction_queued",
        }),
        "acceptance only claims that the correction was queued"
    );
    for (result, status) in [
        (WireResult::SendAccepted, "send_accepted"),
        (WireResult::SubmissionAbandoned, "submission_abandoned"),
        (WireResult::Unknown, "unknown"),
    ] {
        assert_eq!(
            serde_json::to_value(WireResponse {
                protocol_version: PROTOCOL_VERSION,
                instance_id,
                result,
            })
            .expect("wire response")
            .get("status"),
            Some(&serde_json::json!(status))
        );
    }
}

#[test]
fn insert_submit_and_replace_can_have_unknown_ui_timeout_effects() {
    let capture = ComposerCommand::Capture;
    let insert = ComposerCommand::Insert {
        capture_id: Uuid::new_v4(),
        text: "fast".to_string(),
    };
    let verify = ComposerCommand::Verify {
        lease_id: Uuid::new_v4(),
        expected: "fast".to_string(),
    };
    let submit = ComposerCommand::Submit {
        lease_id: Uuid::new_v4(),
        expected: "fast".to_string(),
    };
    let keep = ComposerCommand::Keep {
        lease_id: Uuid::new_v4(),
    };
    let replace = ComposerCommand::Replace {
        lease_id: Uuid::new_v4(),
        expected: "fast".to_string(),
        replacement: "final".to_string(),
    };

    assert!(!capture.may_have_effect());
    assert!(insert.may_have_effect());
    assert!(!verify.may_have_effect());
    assert!(submit.may_have_effect());
    assert!(!keep.may_have_effect());
    assert!(replace.may_have_effect());
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

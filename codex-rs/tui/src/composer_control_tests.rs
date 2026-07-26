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
fn submission_stays_pending_until_matching_user_message_commit() {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("");
    let capture_id = capture(&mut state, &mut target);
    let lease_id = insert(&mut state, &mut target, capture_id, "parakeat");
    let submission_id = mark_submission_pending(&mut state, &target, &[lease_id]);

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
fn abandoned_submission_and_rollback_invalidate_owned_state() {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("");
    let first_capture = capture(&mut state, &mut target);
    let first_lease = insert(&mut state, &mut target, first_capture, "first");
    let submission_id = mark_submission_pending(&mut state, &target, &[first_lease]);
    state.note_submission_abandoned_id("synthetic-thread", submission_id);
    assert!(!state.leases.contains_key(&first_lease));

    let second_capture = capture(&mut state, &mut target);
    let second_lease = insert(&mut state, &mut target, second_capture, " second");
    mark_submitted(&mut state, &target, &[second_lease]);
    state.invalidate_thread("synthetic-thread");
    assert!(state.captures.is_empty());
    assert!(state.leases.is_empty());
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
    assert!(
        correction
            .context_key
            .starts_with("koenig_transcription_correction_")
    );
    assert!(
        correction
            .context_value
            .contains(&format!("koenig-composer-{submission_id}"))
    );
    assert!(
        correction
            .context_value
            .contains("application context, not a user message")
    );

    let (reply, reply_rx) = std::sync::mpsc::sync_channel(1);
    state.finish_correction(
        PendingComposerCorrection {
            lease_id: correction.lease_id,
            thread_id: correction.thread_id,
            context_key: correction.context_key,
            context_value: correction.context_value,
            reply,
        },
        CorrectionDispatchOutcome::Acknowledged,
    );
    assert!(matches!(
        reply_rx.recv().expect("correction reply"),
        WireResult::Corrected
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
fn submitted_correction_known_rejection_is_retryable_but_ack_loss_is_terminal() {
    for (outcome, expected_outcome, remains_submitted) in [
        (
            CorrectionDispatchOutcome::NotApplied,
            MutationOutcome::NotApplied,
            true,
        ),
        (
            CorrectionDispatchOutcome::Unknown,
            MutationOutcome::Unknown,
            false,
        ),
    ] {
        let mut state = ComposerControlState::<u64>::new();
        let mut target = FakeTarget::new("");
        let capture_id = capture(&mut state, &mut target);
        let lease_id = insert(&mut state, &mut target, capture_id, "parakeat");
        mark_submitted(&mut state, &target, &[lease_id]);
        target.text.clear();
        target.cursor = 0;
        target.leases.clear();
        let correction =
            pending_correction(&mut state, &mut target, lease_id, "parakeat", "Parakeet");
        let (reply, reply_rx) = std::sync::mpsc::sync_channel(1);
        state.finish_correction(
            PendingComposerCorrection {
                lease_id: correction.lease_id,
                thread_id: correction.thread_id,
                context_key: correction.context_key,
                context_value: correction.context_value,
                reply,
            },
            outcome,
        );
        assert!(matches!(
            reply_rx.recv().expect("correction reply"),
            WireResult::Error {
                code: ErrorCode::CorrectionUnavailable,
                outcome,
            } if std::mem::discriminant(&outcome) == std::mem::discriminant(&expected_outcome)
        ));
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
        }
    }
}

#[test]
fn submitted_replace_rejects_wrong_thread_without_effect() {
    let mut state = ComposerControlState::<u64>::new();
    let mut target = FakeTarget::new("");
    let capture_id = capture(&mut state, &mut target);
    let lease_id = insert(&mut state, &mut target, capture_id, "parakeat");
    mark_submitted(&mut state, &target, &[lease_id]);
    target.thread_id = "different-thread".to_string();
    target.text.clear();
    target.cursor = 0;

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
        CommandExecution::Complete(WireResult::Error {
            code: ErrorCode::ComposerUnavailable,
            outcome: MutationOutcome::NotApplied,
        })
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
    assert!(first.context_value.contains(&format!(
        "Koenig-owned submitted-message bytes {}..{}",
        first_range.start, first_range.end
    )));
    assert!(first.context_value.contains(&format!(
        "Hunk 1: old bytes {}..{}",
        first_range.start, first_range.end
    )));
    assert!(second.context_value.contains(&format!(
        "Koenig-owned submitted-message bytes {}..{}",
        second_range.start, second_range.end
    )));
    assert!(second.context_value.contains("occurrence 4"));
    assert!(!first.context_value.contains("prefix parakeat"));
    assert!(!first.context_value.contains("| suffix"));
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

#[test]
fn only_insert_and_replace_can_have_unknown_ui_timeout_text_effects() {
    let capture = ComposerCommand::Capture;
    let insert = ComposerCommand::Insert {
        capture_id: Uuid::new_v4(),
        text: "fast".to_string(),
    };
    let verify = ComposerCommand::Verify {
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

    assert!(!capture.may_change_text());
    assert!(insert.may_change_text());
    assert!(!verify.may_change_text());
    assert!(!keep.may_change_text());
    assert!(replace.may_change_text());
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

//! Bounded lifecycle for composer-free Koenig sends.
//!
//! A reservation binds one process-local lease to the active thread and user-chronology epoch.
//! Its first submit latches exact content and a receiver-generated client user-message identity.
//! No state in this module mutates or previews the visible composer.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;

use uuid::Uuid;

use super::CorrectionDispatchOutcome;
use super::CorrectionOrigin;
use super::ErrorCode;
use super::MutationOutcome;
use super::NativeComposerSubmission;
use super::PreparedCorrection;
use super::SubmissionDispatchOutcome;
use super::SubmittedLeaseReceipt;
use super::WireResult;
use super::mechanical_correction_context;
use super::text_hash;

pub(super) const MAX_DIRECT_SEND_RESERVATIONS: usize = 256;
const MAX_DIRECT_SEND_KEEP_TOMBSTONES: usize = 256;
const MAX_DIRECT_SEND_UNKNOWN_ACKNOWLEDGEMENT_TOMBSTONES: usize = 256;

#[derive(Clone)]
enum DirectSendState {
    Reserved,
    Dispatching {
        submission: NativeComposerSubmission,
    },
    Resolved {
        result: WireResult,
        submission: Option<NativeComposerSubmission>,
    },
    SubmissionPending {
        receipt: SubmittedLeaseReceipt,
    },
    SubmittedIntact {
        receipt: SubmittedLeaseReceipt,
    },
    SubmissionAbandoned {
        receipt: SubmittedLeaseReceipt,
    },
    CorrectionPending {
        receipt: SubmittedLeaseReceipt,
        correction_id: Uuid,
        expected: String,
        replacement: String,
        payload: String,
    },
}

#[derive(Clone)]
struct DirectSendReservation {
    thread_id: String,
    user_chronology_epoch: u64,
    submission_id: Uuid,
    content_hash: Option<[u8; 32]>,
    release_on_settle: bool,
    state: DirectSendState,
}

/// Transcript-bearing direct submission ready for app-loop dispatch.
///
/// Deliberately has no `Debug` implementation.
pub(super) struct DirectPreparedSubmission {
    pub(super) reservation_id: Uuid,
    pub(super) submission: NativeComposerSubmission,
    pub(super) expected: String,
}

pub(super) enum DirectReplaceOutcome {
    Complete(WireResult),
    Correct(PreparedCorrection),
}

pub(super) struct DirectSendReservations {
    reservations: HashMap<Uuid, DirectSendReservation>,
    order: VecDeque<Uuid>,
    kept: HashSet<Uuid>,
    kept_order: VecDeque<Uuid>,
    unknown_acknowledged: HashSet<Uuid>,
    unknown_acknowledged_order: VecDeque<Uuid>,
}

impl DirectSendReservations {
    pub(super) fn new() -> Self {
        Self {
            reservations: HashMap::new(),
            order: VecDeque::new(),
            kept: HashSet::new(),
            kept_order: VecDeque::new(),
            unknown_acknowledged: HashSet::new(),
            unknown_acknowledged_order: VecDeque::new(),
        }
    }

    pub(super) fn contains(&self, lease_id: Uuid) -> bool {
        self.reservations.contains_key(&lease_id)
    }

    pub(super) fn contains_keep_id(&self, lease_id: Uuid) -> bool {
        self.reservations.contains_key(&lease_id) || self.kept.contains(&lease_id)
    }

    pub(super) fn contains_acknowledgement_id(&self, lease_id: Uuid) -> bool {
        self.reservations.contains_key(&lease_id) || self.unknown_acknowledged.contains(&lease_id)
    }

    pub(super) fn acquire(&mut self, thread_id: String, user_chronology_epoch: u64) -> WireResult {
        self.retire_terminal();
        if self.reservations.len() >= MAX_DIRECT_SEND_RESERVATIONS {
            return WireResult::not_applied(ErrorCode::LeaseUnavailable);
        }
        let lease_id = loop {
            let candidate = Uuid::new_v4();
            if !self.reservations.contains_key(&candidate)
                && !self.kept.contains(&candidate)
                && !self.unknown_acknowledged.contains(&candidate)
            {
                break candidate;
            }
        };
        self.reservations.insert(
            lease_id,
            DirectSendReservation {
                thread_id,
                user_chronology_epoch,
                submission_id: Uuid::new_v4(),
                content_hash: None,
                release_on_settle: false,
                state: DirectSendState::Reserved,
            },
        );
        self.order.push_back(lease_id);
        WireResult::Reserved { lease_id }
    }

    pub(super) fn prepare_submission(
        &mut self,
        lease_id: Uuid,
        expected: String,
        current_thread_id: Option<&str>,
        current_user_chronology_epoch: u64,
    ) -> Result<DirectPreparedSubmission, WireResult> {
        let Some(reservation) = self.reservations.get_mut(&lease_id) else {
            return Err(WireResult::not_applied(ErrorCode::LeaseUnavailable));
        };
        let expected_hash = text_hash(&expected);
        if let Some(content_hash) = reservation.content_hash {
            if content_hash != expected_hash {
                return Err(WireResult::not_applied(ErrorCode::ExpectedMismatch));
            }
        } else {
            reservation.content_hash = Some(expected_hash);
        }

        match &reservation.state {
            DirectSendState::Dispatching { .. } => return Err(WireResult::Unknown),
            DirectSendState::Resolved { result, .. } => return Err(result.clone()),
            DirectSendState::SubmissionPending { .. } => return Err(WireResult::SendAccepted),
            DirectSendState::SubmittedIntact { .. } | DirectSendState::CorrectionPending { .. } => {
                return Err(WireResult::SubmittedIntact);
            }
            DirectSendState::SubmissionAbandoned { .. } => {
                return Err(WireResult::SubmissionAbandoned);
            }
            DirectSendState::Reserved => {}
        }

        if current_thread_id != Some(reservation.thread_id.as_str())
            || current_user_chronology_epoch != reservation.user_chronology_epoch
        {
            let result = WireResult::not_applied(ErrorCode::ReservationChanged);
            reservation.state = DirectSendState::Resolved {
                result: result.clone(),
                submission: None,
            };
            return Err(result);
        }

        let submission = NativeComposerSubmission::new_direct(
            reservation.submission_id,
            lease_id,
            reservation.thread_id.clone(),
            &expected,
        );
        reservation.state = DirectSendState::Dispatching {
            submission: submission.clone(),
        };
        Ok(DirectPreparedSubmission {
            reservation_id: lease_id,
            submission,
            expected,
        })
    }

    pub(super) fn finish_submission(
        &mut self,
        lease_id: Uuid,
        submission_id: Uuid,
        outcome: SubmissionDispatchOutcome,
    ) -> WireResult {
        let Some(reservation) = self.reservations.get_mut(&lease_id) else {
            return WireResult::not_applied(ErrorCode::LeaseUnavailable);
        };
        let DirectSendState::Dispatching { submission } = &reservation.state else {
            return state_wire_result(&reservation.state);
        };
        if submission.submission_id() != submission_id {
            return WireResult::not_applied(ErrorCode::ExpectedMismatch);
        }
        let submission = submission.clone();
        match outcome {
            SubmissionDispatchOutcome::Accepted => {
                reservation.state = DirectSendState::SubmissionPending {
                    receipt: direct_receipt(&submission),
                };
                WireResult::SendAccepted
            }
            SubmissionDispatchOutcome::AcceptedButUncommitted
            | SubmissionDispatchOutcome::Unknown => {
                reservation.state = DirectSendState::Resolved {
                    result: WireResult::Unknown,
                    submission: Some(submission),
                };
                WireResult::Unknown
            }
            SubmissionDispatchOutcome::NotApplied(code) => {
                let result = WireResult::not_applied(code);
                reservation.state = DirectSendState::Resolved {
                    result: result.clone(),
                    submission: None,
                };
                result
            }
        }
    }

    pub(super) fn verify(
        &self,
        lease_id: Uuid,
        expected: &str,
        current_thread_id: Option<&str>,
        current_user_chronology_epoch: u64,
    ) -> WireResult {
        let Some(reservation) = self.reservations.get(&lease_id) else {
            return WireResult::not_applied(ErrorCode::LeaseUnavailable);
        };
        if reservation
            .content_hash
            .is_some_and(|content_hash| content_hash != text_hash(expected))
        {
            return WireResult::not_applied(ErrorCode::ExpectedMismatch);
        }
        match &reservation.state {
            DirectSendState::Reserved => {
                if current_thread_id == Some(reservation.thread_id.as_str())
                    && current_user_chronology_epoch == reservation.user_chronology_epoch
                {
                    WireResult::Verified
                } else {
                    WireResult::not_applied(ErrorCode::ReservationChanged)
                }
            }
            state => state_wire_result(state),
        }
    }

    pub(super) fn keep(&mut self, lease_id: Uuid) -> WireResult {
        let Some(state) = self
            .reservations
            .get(&lease_id)
            .map(|reservation| reservation.state.clone())
        else {
            return if self.kept.contains(&lease_id) {
                WireResult::Kept
            } else {
                WireResult::not_applied(ErrorCode::LeaseUnavailable)
            };
        };
        match state {
            DirectSendState::Reserved
            | DirectSendState::SubmittedIntact { .. }
            | DirectSendState::SubmissionAbandoned { .. }
            | DirectSendState::Resolved {
                result:
                    WireResult::Error {
                        outcome: MutationOutcome::NotApplied,
                        ..
                    },
                ..
            } => {
                self.remove_kept(lease_id);
                WireResult::Kept
            }
            DirectSendState::SubmissionPending { .. } => {
                if let Some(reservation) = self.reservations.get_mut(&lease_id) {
                    reservation.release_on_settle = true;
                }
                WireResult::Kept
            }
            DirectSendState::Dispatching { .. }
            | DirectSendState::Resolved {
                result: WireResult::Unknown,
                ..
            } => {
                if let Some(reservation) = self.reservations.get_mut(&lease_id) {
                    reservation.release_on_settle = true;
                }
                WireResult::Unknown
            }
            DirectSendState::CorrectionPending { .. } => {
                WireResult::not_applied(ErrorCode::LeaseUnavailable)
            }
            DirectSendState::Resolved { result, .. } => result,
        }
    }

    pub(super) fn acknowledge_unknown(&mut self, lease_id: Uuid) -> WireResult {
        let Some(state) = self
            .reservations
            .get(&lease_id)
            .map(|reservation| reservation.state.clone())
        else {
            return if self.unknown_acknowledged.contains(&lease_id) {
                WireResult::UnknownAcknowledged
            } else {
                WireResult::not_applied(ErrorCode::LeaseUnavailable)
            };
        };
        match state {
            DirectSendState::Resolved {
                result: WireResult::Unknown,
                ..
            } => {
                self.remove_unknown_acknowledged(lease_id);
                WireResult::UnknownAcknowledged
            }
            state => state_wire_result(&state),
        }
    }

    pub(super) fn replace(
        &mut self,
        lease_id: Uuid,
        expected: String,
        replacement: String,
    ) -> DirectReplaceOutcome {
        let Some(reservation) = self.reservations.get(&lease_id).cloned() else {
            return DirectReplaceOutcome::Complete(WireResult::not_applied(
                ErrorCode::LeaseUnavailable,
            ));
        };
        if reservation
            .content_hash
            .is_none_or(|content_hash| content_hash != text_hash(&expected))
        {
            return DirectReplaceOutcome::Complete(WireResult::not_applied(
                ErrorCode::ExpectedMismatch,
            ));
        }
        match reservation.state {
            DirectSendState::Reserved => {
                DirectReplaceOutcome::Complete(WireResult::not_applied(ErrorCode::LeaseUnavailable))
            }
            DirectSendState::Dispatching { .. }
            | DirectSendState::Resolved {
                result: WireResult::Unknown,
                ..
            } => DirectReplaceOutcome::Complete(WireResult::Unknown),
            DirectSendState::Resolved { result, .. } => DirectReplaceOutcome::Complete(result),
            DirectSendState::SubmissionPending { .. } => {
                DirectReplaceOutcome::Complete(WireResult::SubmissionPending)
            }
            DirectSendState::SubmissionAbandoned { .. } => {
                DirectReplaceOutcome::Complete(WireResult::SubmissionAbandoned)
            }
            DirectSendState::SubmittedIntact { receipt } => {
                if expected == replacement {
                    self.remove(lease_id);
                    return DirectReplaceOutcome::Complete(WireResult::Kept);
                }
                if receipt
                    .submitted_text
                    .get(receipt.range.clone())
                    .is_none_or(|submitted| submitted != expected)
                {
                    return DirectReplaceOutcome::Complete(WireResult::not_applied(
                        ErrorCode::ExpectedMismatch,
                    ));
                }
                let correction_id = Uuid::new_v4();
                let Some(payload) = mechanical_correction_context(
                    receipt.submission_id,
                    &receipt.submitted_text,
                    receipt.range.clone(),
                    &expected,
                    &replacement,
                ) else {
                    return DirectReplaceOutcome::Complete(WireResult::not_applied(
                        ErrorCode::CorrectionUnavailable,
                    ));
                };
                if let Some(stored) = self.reservations.get_mut(&lease_id) {
                    stored.state = DirectSendState::CorrectionPending {
                        receipt: receipt.clone(),
                        correction_id,
                        expected,
                        replacement,
                        payload: payload.clone(),
                    };
                }
                DirectReplaceOutcome::Correct(PreparedCorrection {
                    lease_id,
                    origin: CorrectionOrigin::Direct,
                    thread_id: reservation.thread_id,
                    correction_id,
                    expected_client_user_message_id: format!(
                        "koenig-composer-{}",
                        receipt.submission_id
                    ),
                    payload,
                })
            }
            DirectSendState::CorrectionPending {
                receipt,
                correction_id,
                expected: pending_expected,
                replacement: pending_replacement,
                payload,
            } => {
                if expected != pending_expected || replacement != pending_replacement {
                    return DirectReplaceOutcome::Complete(WireResult::not_applied(
                        ErrorCode::LeaseUnavailable,
                    ));
                }
                DirectReplaceOutcome::Correct(PreparedCorrection {
                    lease_id,
                    origin: CorrectionOrigin::Direct,
                    thread_id: reservation.thread_id,
                    correction_id,
                    expected_client_user_message_id: format!(
                        "koenig-composer-{}",
                        receipt.submission_id
                    ),
                    payload,
                })
            }
        }
    }

    pub(super) fn finish_correction(
        &mut self,
        lease_id: Uuid,
        correction_id: Uuid,
        outcome: CorrectionDispatchOutcome,
    ) -> WireResult {
        match outcome {
            CorrectionDispatchOutcome::Accepted => {
                let is_current = self.reservations.get(&lease_id).is_some_and(|reservation| {
                    matches!(
                        &reservation.state,
                        DirectSendState::CorrectionPending {
                            correction_id: current,
                            ..
                        } if *current == correction_id
                    )
                });
                if is_current {
                    self.remove(lease_id);
                }
                WireResult::CorrectionQueued
            }
            CorrectionDispatchOutcome::NotApplied => {
                if let Some(reservation) = self.reservations.get_mut(&lease_id)
                    && let DirectSendState::CorrectionPending {
                        receipt,
                        correction_id: current,
                        ..
                    } = &reservation.state
                    && *current == correction_id
                {
                    reservation.state = DirectSendState::SubmittedIntact {
                        receipt: receipt.clone(),
                    };
                }
                WireResult::not_applied(ErrorCode::CorrectionUnavailable)
            }
            CorrectionDispatchOutcome::Unknown => WireResult::CorrectionPending,
        }
    }

    pub(super) fn note_submission_pending(&mut self, submission: &NativeComposerSubmission) {
        let Some(lease_id) = submission.direct_lease_id() else {
            return;
        };
        let Some(reservation) = self.reservations.get_mut(&lease_id) else {
            return;
        };
        if reservation.submission_id != submission.submission_id()
            || reservation.content_hash != Some(text_hash(submission.submitted_text()))
        {
            return;
        }
        if !matches!(
            &reservation.state,
            DirectSendState::Dispatching { .. }
                | DirectSendState::Resolved {
                    result: WireResult::Unknown,
                    ..
                }
                | DirectSendState::SubmissionPending { .. }
        ) {
            return;
        }
        reservation.state = DirectSendState::SubmissionPending {
            receipt: direct_receipt(submission),
        };
    }

    pub(super) fn note_submission_committed(&mut self, submission: &NativeComposerSubmission) {
        self.note_submission_pending(submission);
        let Some(lease_id) = submission.direct_lease_id() else {
            return;
        };
        if self
            .reservations
            .get(&lease_id)
            .is_some_and(|reservation| reservation.release_on_settle)
        {
            self.remove_kept(lease_id);
        } else if let Some(reservation) = self.reservations.get_mut(&lease_id)
            && let DirectSendState::SubmissionPending { receipt } = &reservation.state
        {
            reservation.state = DirectSendState::SubmittedIntact {
                receipt: receipt.clone(),
            };
        }
    }

    pub(super) fn note_submission_abandoned(&mut self, submission: &NativeComposerSubmission) {
        let Some(lease_id) = submission.direct_lease_id() else {
            return;
        };
        if self.reservations.get(&lease_id).is_some_and(|reservation| {
            reservation.submission_id == submission.submission_id() && reservation.release_on_settle
        }) {
            self.remove_kept(lease_id);
            return;
        }
        let Some(reservation) = self.reservations.get_mut(&lease_id) else {
            return;
        };
        if reservation.submission_id == submission.submission_id() {
            reservation.state = DirectSendState::SubmissionAbandoned {
                receipt: direct_receipt(submission),
            };
        }
    }

    pub(super) fn note_thread_rolled_back(
        &mut self,
        thread_id: &str,
        surviving_submission_ids: &HashSet<Uuid>,
    ) {
        let mut released = Vec::new();
        for (lease_id, reservation) in self
            .reservations
            .iter_mut()
            .filter(|(_, reservation)| reservation.thread_id == thread_id)
        {
            let (submission_id, receipt) = match &reservation.state {
                DirectSendState::Dispatching { submission }
                | DirectSendState::Resolved {
                    result: WireResult::Unknown,
                    submission: Some(submission),
                } => (submission.submission_id(), Some(direct_receipt(submission))),
                DirectSendState::SubmissionPending { receipt }
                | DirectSendState::SubmittedIntact { receipt }
                | DirectSendState::SubmissionAbandoned { receipt }
                | DirectSendState::CorrectionPending { receipt, .. } => {
                    (receipt.submission_id, Some(receipt.clone()))
                }
                DirectSendState::Reserved | DirectSendState::Resolved { .. } => {
                    (reservation.submission_id, None)
                }
            };
            let Some(receipt) = receipt else {
                continue;
            };
            if !surviving_submission_ids.contains(&submission_id) {
                reservation.state = DirectSendState::SubmissionAbandoned { receipt };
            } else if matches!(
                &reservation.state,
                DirectSendState::Dispatching { .. }
                    | DirectSendState::Resolved {
                        result: WireResult::Unknown,
                        ..
                    }
                    | DirectSendState::SubmissionPending { .. }
                    | DirectSendState::SubmissionAbandoned { .. }
            ) {
                reservation.state = DirectSendState::SubmittedIntact { receipt };
            }
            if reservation.release_on_settle
                && matches!(
                    &reservation.state,
                    DirectSendState::SubmittedIntact { .. }
                        | DirectSendState::SubmissionAbandoned { .. }
                )
            {
                released.push(*lease_id);
            }
        }
        for lease_id in released {
            self.remove_kept(lease_id);
        }
    }

    fn retire_terminal(&mut self) {
        while self.reservations.len() >= MAX_DIRECT_SEND_RESERVATIONS {
            let Some(index) = self.order.iter().position(|lease_id| {
                self.reservations.get(lease_id).is_some_and(|reservation| {
                    matches!(
                        &reservation.state,
                        DirectSendState::SubmissionAbandoned { .. }
                            | DirectSendState::Resolved {
                                result: WireResult::Error {
                                    outcome: MutationOutcome::NotApplied,
                                    ..
                                },
                                ..
                            }
                    )
                })
            }) else {
                return;
            };
            let Some(lease_id) = self.order.remove(index) else {
                return;
            };
            self.reservations.remove(&lease_id);
        }
    }

    fn remove(&mut self, lease_id: Uuid) {
        self.reservations.remove(&lease_id);
        self.order.retain(|queued| *queued != lease_id);
    }

    fn remove_kept(&mut self, lease_id: Uuid) {
        self.remove(lease_id);
        if self.kept.insert(lease_id) {
            self.kept_order.push_back(lease_id);
        }
        while self.kept.len() > MAX_DIRECT_SEND_KEEP_TOMBSTONES {
            let Some(expired) = self.kept_order.pop_front() else {
                break;
            };
            self.kept.remove(&expired);
        }
    }

    fn remove_unknown_acknowledged(&mut self, lease_id: Uuid) {
        self.remove(lease_id);
        if self.unknown_acknowledged.insert(lease_id) {
            self.unknown_acknowledged_order.push_back(lease_id);
        }
        while self.unknown_acknowledged.len() > MAX_DIRECT_SEND_UNKNOWN_ACKNOWLEDGEMENT_TOMBSTONES {
            let Some(expired) = self.unknown_acknowledged_order.pop_front() else {
                break;
            };
            self.unknown_acknowledged.remove(&expired);
        }
    }
}

fn state_wire_result(state: &DirectSendState) -> WireResult {
    match state {
        DirectSendState::Reserved => WireResult::not_applied(ErrorCode::SubmissionUnavailable),
        DirectSendState::Dispatching { .. } => WireResult::Unknown,
        DirectSendState::Resolved { result, .. } => result.clone(),
        DirectSendState::SubmissionPending { .. } => WireResult::SubmissionPending,
        DirectSendState::SubmittedIntact { .. } | DirectSendState::CorrectionPending { .. } => {
            WireResult::SubmittedIntact
        }
        DirectSendState::SubmissionAbandoned { .. } => WireResult::SubmissionAbandoned,
    }
}

fn direct_receipt(submission: &NativeComposerSubmission) -> SubmittedLeaseReceipt {
    SubmittedLeaseReceipt {
        submission_id: submission.submission_id(),
        submitted_text: submission.submitted_text_arc(),
        range: 0..submission.submitted_text().len(),
    }
}

//! Native, content-opaque control plane for the active composer.
//!
//! The listener never uses [`crate::app_event::AppEvent`]: requests enter the main UI loop through
//! a dedicated channel, so transcript-bearing payloads are neither session-logged nor formatted
//! for diagnostics. All capture checks and mutations therefore share the same serialized order as
//! terminal input.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::sync::mpsc::SyncSender;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use similar::ChangeTag;
use similar::TextDiff;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::unbounded_channel;
use uuid::Uuid;

use crate::bottom_pane::ComposerLeaseError;
use crate::bottom_pane::ComposerLeaseId;
use crate::bottom_pane::SubmittedComposerLease;
use crate::chatwidget::ChatWidget;
use crate::tui::TuiEvent;

const PROTOCOL_VERSION: u8 = 1;
const MAX_CAPTURE_COUNT: usize = 64;
const MAX_LEASE_COUNT: usize = 256;
const MAX_TEXT_BYTES: usize = 256 * 1024;
const UI_REQUEST_DEADLINE: Duration = Duration::from_secs(2);

#[cfg(unix)]
const MAX_WIRE_REQUEST_BYTES: usize = 512 * 1024;
#[cfg(unix)]
const SOCKET_READ_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(unix)]
const UI_REPLY_TIMEOUT: Duration = Duration::from_millis(2250);

/// Snapshot of the active native composer. It intentionally has no `Debug` implementation because
/// it temporarily carries the draft while the UI loop computes a digest.
pub(crate) struct ComposerSnapshot {
    thread_id: String,
    text: String,
    cursor: usize,
}

impl ComposerSnapshot {
    pub(crate) fn new(thread_id: String, text: String, cursor: usize) -> Self {
        Self {
            thread_id,
            text,
            cursor,
        }
    }
}

/// Content-opaque provenance carried with one composer submission until app-server acceptance.
///
/// The submitted text is retained only in memory so later correction locators can be computed
/// against the exact accepted user message. Its custom `Debug` implementation never exposes text.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct NativeComposerSubmission {
    submission_id: Uuid,
    thread_id: String,
    submitted_text: Arc<str>,
    leases: Vec<NativeSubmittedLease>,
}

#[derive(Clone, PartialEq, Eq)]
struct NativeSubmittedLease {
    native: ComposerLeaseId,
    range: Range<usize>,
}

impl fmt::Debug for NativeComposerSubmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeComposerSubmission")
            .field("submission_id", &self.submission_id)
            .field("thread_id", &self.thread_id)
            .field("submitted_text_bytes", &self.submitted_text.len())
            .field("lease_count", &self.leases.len())
            .finish()
    }
}

impl NativeComposerSubmission {
    pub(crate) fn new(
        thread_id: String,
        submitted_text: &str,
        leases: Vec<SubmittedComposerLease>,
    ) -> Option<Self> {
        let leases = leases
            .into_iter()
            .filter_map(|lease| {
                submitted_text
                    .get(lease.range.clone())
                    .map(|_| NativeSubmittedLease {
                        native: lease.id,
                        range: lease.range,
                    })
            })
            .collect::<Vec<_>>();
        (!leases.is_empty()).then(|| Self {
            submission_id: Uuid::new_v4(),
            thread_id,
            submitted_text: Arc::from(submitted_text),
            leases,
        })
    }

    pub(crate) fn client_user_message_id(&self) -> String {
        format!("koenig-composer-{}", self.submission_id)
    }

    pub(crate) fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub(crate) fn merge_parts(
        parts: impl IntoIterator<Item = (String, Option<Self>)>,
        merged_text: &str,
    ) -> Option<Self> {
        let mut thread_id: Option<String> = None;
        let mut leases = Vec::new();
        let mut offset = 0;
        for (index, (text, submission)) in parts.into_iter().enumerate() {
            if index > 0 {
                if merged_text.as_bytes().get(offset) != Some(&b'\n') {
                    return None;
                }
                offset += 1;
            }
            if merged_text.get(offset..offset + text.len()) != Some(text.as_str()) {
                return None;
            }
            if let Some(submission) = submission {
                if submission.submitted_text.as_ref() != text {
                    return None;
                }
                match thread_id.as_deref() {
                    None => thread_id = Some(submission.thread_id.clone()),
                    Some(existing) if existing == submission.thread_id => {}
                    Some(_) => return None,
                }
                for lease in submission.leases {
                    leases.push(NativeSubmittedLease {
                        native: lease.native,
                        range: lease.range.start + offset..lease.range.end + offset,
                    });
                }
            }
            offset += text.len();
        }
        if offset != merged_text.len() {
            return None;
        }
        let thread_id = thread_id?;
        (!leases.is_empty()).then(|| Self {
            submission_id: Uuid::new_v4(),
            thread_id,
            submitted_text: Arc::from(merged_text),
            leases,
        })
    }

    fn submission_id(&self) -> Uuid {
        self.submission_id
    }

    fn submitted_text_arc(&self) -> Arc<str> {
        Arc::clone(&self.submitted_text)
    }

    fn submitted_leases(&self) -> &[NativeSubmittedLease] {
        &self.leases
    }
}

pub(crate) trait ComposerControlTarget {
    type Lease: Copy;

    fn thread_id(&self) -> Option<String>;
    fn snapshot(&self) -> Option<ComposerSnapshot>;
    fn insert_owned_text(&mut self, text: &str) -> Result<Self::Lease, ComposerLeaseError>;
    fn verify_owned_text(
        &self,
        lease: Self::Lease,
        expected: &str,
    ) -> Result<(), ComposerLeaseError>;
    fn keep_owned_text(&mut self, lease: Self::Lease) -> Result<(), ComposerLeaseError>;
    fn replace_owned_text(
        &mut self,
        lease: Self::Lease,
        expected: &str,
        replacement: &str,
    ) -> Result<(), ComposerLeaseError>;
}

impl ComposerControlTarget for ChatWidget {
    type Lease = ComposerLeaseId;

    fn thread_id(&self) -> Option<String> {
        self.thread_id().map(|thread_id| thread_id.to_string())
    }

    fn snapshot(&self) -> Option<ComposerSnapshot> {
        if !self.composer_control_available() {
            return None;
        }
        Some(ComposerSnapshot::new(
            self.thread_id()?.to_string(),
            self.composer_text(),
            self.composer_cursor(),
        ))
    }

    fn insert_owned_text(&mut self, text: &str) -> Result<Self::Lease, ComposerLeaseError> {
        self.insert_composer_owned_text(text)
    }

    fn verify_owned_text(
        &self,
        lease: Self::Lease,
        expected: &str,
    ) -> Result<(), ComposerLeaseError> {
        self.verify_composer_owned_text(lease, expected)
    }

    fn keep_owned_text(&mut self, lease: Self::Lease) -> Result<(), ComposerLeaseError> {
        self.keep_composer_owned_text(lease)
    }

    fn replace_owned_text(
        &mut self,
        lease: Self::Lease,
        expected: &str,
        replacement: &str,
    ) -> Result<(), ComposerLeaseError> {
        self.replace_composer_owned_text(lease, expected, replacement)
    }
}

/// A request from the socket listener to the UI loop. This type deliberately has no `Debug`
/// implementation because its command can carry transcription text.
pub(crate) struct ComposerControlRequest {
    command: ComposerCommand,
    deadline: Instant,
    reply: SyncSender<WireResult>,
}

/// Application-context correction ready for the async app-server acknowledgement boundary.
///
/// This type carries transcript-derived context and therefore deliberately has no `Debug`
/// implementation.
pub(crate) struct PendingComposerCorrection {
    lease_id: Uuid,
    thread_id: String,
    context_key: String,
    context_value: String,
    reply: SyncSender<WireResult>,
}

impl PendingComposerCorrection {
    pub(crate) fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub(crate) fn context_key(&self) -> &str {
        &self.context_key
    }

    pub(crate) fn context_value(&self) -> &str {
        &self.context_value
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CorrectionDispatchOutcome {
    Acknowledged,
    NotApplied,
    Unknown,
}

/// Transcript-bearing command. Do not derive or implement `Debug`.
enum ComposerCommand {
    Capture,
    Insert {
        capture_id: Uuid,
        text: String,
    },
    Verify {
        lease_id: Uuid,
        expected: String,
    },
    Keep {
        lease_id: Uuid,
    },
    Replace {
        lease_id: Uuid,
        expected: String,
        replacement: String,
    },
}

struct PreparedCorrection {
    lease_id: Uuid,
    thread_id: String,
    context_key: String,
    context_value: String,
}

enum CommandExecution {
    Complete(WireResult),
    Correct(PreparedCorrection),
}

impl ComposerCommand {
    fn may_change_text(&self) -> bool {
        matches!(self, Self::Insert { .. } | Self::Replace { .. })
    }
}

struct Capture {
    thread_id: String,
    text_hash: [u8; 32],
    cursor: usize,
    input_epoch: u64,
}

enum ExternalLeaseState<L> {
    Draft { native: L },
    SubmittedIntact { receipt: SubmittedLeaseReceipt },
    CorrectionPending { receipt: SubmittedLeaseReceipt },
}

#[derive(Clone)]
struct SubmittedLeaseReceipt {
    submission_id: Uuid,
    submitted_text: Arc<str>,
    range: Range<usize>,
}

impl<L: Copy> Clone for ExternalLeaseState<L> {
    fn clone(&self) -> Self {
        match self {
            Self::Draft { native } => Self::Draft { native: *native },
            Self::SubmittedIntact { receipt } => Self::SubmittedIntact {
                receipt: receipt.clone(),
            },
            Self::CorrectionPending { receipt } => Self::CorrectionPending {
                receipt: receipt.clone(),
            },
        }
    }
}

struct ExternalLease<L> {
    state: ExternalLeaseState<L>,
    thread_id: String,
    expected_hash: [u8; 32],
}

impl<L: Copy> Clone for ExternalLease<L> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            thread_id: self.thread_id.clone(),
            expected_hash: self.expected_hash,
        }
    }
}

pub(crate) struct ComposerControlState<L> {
    input_epoch: u64,
    captures: HashMap<Uuid, Capture>,
    capture_order: VecDeque<Uuid>,
    leases: HashMap<Uuid, ExternalLease<L>>,
    lease_order: VecDeque<Uuid>,
}

pub(crate) type NativeComposerControlState = ComposerControlState<ComposerLeaseId>;

impl<L: Copy + Eq> ComposerControlState<L> {
    pub(crate) fn new() -> Self {
        Self {
            input_epoch: 0,
            captures: HashMap::new(),
            capture_order: VecDeque::new(),
            leases: HashMap::new(),
            lease_order: VecDeque::new(),
        }
    }

    /// Conservatively invalidate capture compare-and-swap tokens before any terminal input is
    /// routed. Even a key handled by another surface advances the epoch; false rejection is safer
    /// than inserting against an input order the producer did not capture.
    pub(crate) fn note_tui_event(&mut self, event: &TuiEvent) {
        if matches!(event, TuiEvent::Key(_) | TuiEvent::Paste(_)) {
            self.input_epoch = self.input_epoch.wrapping_add(1);
        }
    }

    pub(crate) fn handle_ui_request<T>(
        &mut self,
        request: ComposerControlRequest,
        target: &mut T,
        app_overlay_active: bool,
    ) -> Option<PendingComposerCorrection>
    where
        T: ComposerControlTarget<Lease = L>,
    {
        let ComposerControlRequest {
            command,
            deadline,
            reply,
        } = request;
        let execution = if Instant::now() > deadline {
            CommandExecution::Complete(WireResult::error(
                ErrorCode::UiTimeout,
                MutationOutcome::NotApplied,
            ))
        } else {
            self.prepare_command(command, target, app_overlay_active)
        };
        match execution {
            CommandExecution::Complete(result) => {
                let _ = reply.send(result);
                None
            }
            CommandExecution::Correct(correction) => Some(PendingComposerCorrection {
                lease_id: correction.lease_id,
                thread_id: correction.thread_id,
                context_key: correction.context_key,
                context_value: correction.context_value,
                reply,
            }),
        }
    }

    pub(crate) fn finish_correction(
        &mut self,
        pending: PendingComposerCorrection,
        outcome: CorrectionDispatchOutcome,
    ) {
        let result = match outcome {
            CorrectionDispatchOutcome::Acknowledged => {
                self.leases.remove(&pending.lease_id);
                self.lease_order
                    .retain(|queued| *queued != pending.lease_id);
                WireResult::Corrected
            }
            CorrectionDispatchOutcome::NotApplied => {
                if let Some(lease) = self.leases.get_mut(&pending.lease_id)
                    && let ExternalLeaseState::CorrectionPending { receipt } = &lease.state
                {
                    lease.state = ExternalLeaseState::SubmittedIntact {
                        receipt: receipt.clone(),
                    };
                }
                WireResult::not_applied(ErrorCode::CorrectionUnavailable)
            }
            CorrectionDispatchOutcome::Unknown => {
                self.leases.remove(&pending.lease_id);
                self.lease_order
                    .retain(|queued| *queued != pending.lease_id);
                WireResult::error(ErrorCode::CorrectionUnavailable, MutationOutcome::Unknown)
            }
        };
        let _ = pending.reply.send(result);
    }

    #[cfg(test)]
    fn execute<T>(
        &mut self,
        command: ComposerCommand,
        target: &mut T,
        app_overlay_active: bool,
    ) -> WireResult
    where
        T: ComposerControlTarget<Lease = L>,
    {
        match self.prepare_command(command, target, app_overlay_active) {
            CommandExecution::Complete(result) => result,
            CommandExecution::Correct(correction) => {
                if let Some(lease) = self.leases.get_mut(&correction.lease_id)
                    && let ExternalLeaseState::CorrectionPending { receipt } = &lease.state
                {
                    lease.state = ExternalLeaseState::SubmittedIntact {
                        receipt: receipt.clone(),
                    };
                }
                WireResult::not_applied(ErrorCode::CorrectionUnavailable)
            }
        }
    }

    fn prepare_command<T>(
        &mut self,
        command: ComposerCommand,
        target: &mut T,
        app_overlay_active: bool,
    ) -> CommandExecution
    where
        T: ComposerControlTarget<Lease = L>,
    {
        match command {
            ComposerCommand::Capture => {
                CommandExecution::Complete(self.capture(target, app_overlay_active))
            }
            ComposerCommand::Insert { capture_id, text } => CommandExecution::Complete(
                self.insert(target, app_overlay_active, capture_id, text),
            ),
            ComposerCommand::Verify { lease_id, expected } => CommandExecution::Complete(
                self.verify(target, app_overlay_active, lease_id, expected),
            ),
            ComposerCommand::Keep { lease_id } => {
                CommandExecution::Complete(self.keep(target, app_overlay_active, lease_id))
            }
            ComposerCommand::Replace {
                lease_id,
                expected,
                replacement,
            } => self.replace(target, app_overlay_active, lease_id, expected, replacement),
        }
    }

    fn capture<T>(&mut self, target: &T, app_overlay_active: bool) -> WireResult
    where
        T: ComposerControlTarget<Lease = L>,
    {
        let Some(snapshot) = available_snapshot(target, app_overlay_active) else {
            return WireResult::not_applied(ErrorCode::ComposerUnavailable);
        };
        let capture_id = fresh_id(&self.captures);
        self.captures.insert(
            capture_id,
            Capture {
                thread_id: snapshot.thread_id,
                text_hash: text_hash(&snapshot.text),
                cursor: snapshot.cursor,
                input_epoch: self.input_epoch,
            },
        );
        self.capture_order.push_back(capture_id);
        evict_oldest(
            &mut self.captures,
            &mut self.capture_order,
            MAX_CAPTURE_COUNT,
        );
        WireResult::Captured { capture_id }
    }

    fn insert<T>(
        &mut self,
        target: &mut T,
        app_overlay_active: bool,
        capture_id: Uuid,
        text: String,
    ) -> WireResult
    where
        T: ComposerControlTarget<Lease = L>,
    {
        let Some(capture) = self.captures.remove(&capture_id) else {
            return WireResult::not_applied(ErrorCode::CaptureUnavailable);
        };
        self.capture_order.retain(|queued| *queued != capture_id);
        let Some(snapshot) = available_snapshot(target, app_overlay_active) else {
            return WireResult::not_applied(ErrorCode::ComposerUnavailable);
        };
        if capture.thread_id != snapshot.thread_id
            || capture.text_hash != text_hash(&snapshot.text)
            || capture.cursor != snapshot.cursor
            || capture.input_epoch != self.input_epoch
        {
            return WireResult::not_applied(ErrorCode::CaptureChanged);
        }

        self.retire_excess_leases(target, &snapshot);
        let expected_hash = text_hash(&text);
        let native = match target.insert_owned_text(&text) {
            Ok(native) => native,
            Err(err) => return WireResult::not_applied(map_lease_error(err)),
        };
        self.rebase_matching_captures(target, &snapshot);
        let lease_id = fresh_id(&self.leases);
        self.leases.insert(
            lease_id,
            ExternalLease {
                state: ExternalLeaseState::Draft { native },
                thread_id: snapshot.thread_id,
                expected_hash,
            },
        );
        self.lease_order.push_back(lease_id);
        WireResult::Inserted { lease_id }
    }

    fn rebase_matching_captures<T>(&mut self, target: &T, before: &ComposerSnapshot)
    where
        T: ComposerControlTarget<Lease = L>,
    {
        let Some(after) = target.snapshot() else {
            return;
        };
        if after.thread_id != before.thread_id {
            return;
        }
        let before_hash = text_hash(&before.text);
        let after_hash = text_hash(&after.text);
        for capture in self.captures.values_mut() {
            if capture.input_epoch == self.input_epoch
                && capture.thread_id == before.thread_id
                && capture.text_hash == before_hash
                && capture.cursor == before.cursor
            {
                capture.text_hash = after_hash;
                capture.cursor = after.cursor;
            }
        }
    }

    fn verify<T>(
        &mut self,
        target: &T,
        app_overlay_active: bool,
        lease_id: Uuid,
        expected: String,
    ) -> WireResult
    where
        T: ComposerControlTarget<Lease = L>,
    {
        let Some(lease) = self.leases.get(&lease_id) else {
            return WireResult::not_applied(ErrorCode::LeaseUnavailable);
        };
        if lease.expected_hash != text_hash(&expected) {
            return WireResult::not_applied(ErrorCode::ExpectedMismatch);
        }
        match &lease.state {
            ExternalLeaseState::Draft { native } => {
                let Some(snapshot) = available_snapshot(target, app_overlay_active) else {
                    return WireResult::not_applied(ErrorCode::ComposerUnavailable);
                };
                if lease.thread_id != snapshot.thread_id {
                    return WireResult::not_applied(ErrorCode::ComposerUnavailable);
                }
                match target.verify_owned_text(*native, &expected) {
                    Ok(()) => WireResult::Verified,
                    Err(err) => WireResult::not_applied(map_lease_error(err)),
                }
            }
            ExternalLeaseState::SubmittedIntact { .. } => {
                if target.thread_id().as_deref() == Some(lease.thread_id.as_str()) {
                    WireResult::SubmittedIntact
                } else {
                    WireResult::not_applied(ErrorCode::ComposerUnavailable)
                }
            }
            ExternalLeaseState::CorrectionPending { .. } => {
                WireResult::not_applied(ErrorCode::LeaseUnavailable)
            }
        }
    }

    fn keep<T>(&mut self, target: &mut T, app_overlay_active: bool, lease_id: Uuid) -> WireResult
    where
        T: ComposerControlTarget<Lease = L>,
    {
        let Some(lease) = self.leases.get(&lease_id).cloned() else {
            return WireResult::not_applied(ErrorCode::LeaseUnavailable);
        };
        match lease.state {
            ExternalLeaseState::Draft { native } => {
                let Some(snapshot) = available_snapshot(target, app_overlay_active) else {
                    return WireResult::not_applied(ErrorCode::ComposerUnavailable);
                };
                if lease.thread_id != snapshot.thread_id {
                    return WireResult::not_applied(ErrorCode::ComposerUnavailable);
                }
                self.leases.remove(&lease_id);
                self.lease_order.retain(|queued| *queued != lease_id);
                match target.keep_owned_text(native) {
                    Ok(()) => WireResult::Kept,
                    Err(err) => WireResult::not_applied(map_lease_error(err)),
                }
            }
            ExternalLeaseState::SubmittedIntact { .. } => {
                if target.thread_id().as_deref() != Some(lease.thread_id.as_str()) {
                    return WireResult::not_applied(ErrorCode::ComposerUnavailable);
                }
                self.leases.remove(&lease_id);
                self.lease_order.retain(|queued| *queued != lease_id);
                WireResult::Kept
            }
            ExternalLeaseState::CorrectionPending { .. } => {
                WireResult::not_applied(ErrorCode::LeaseUnavailable)
            }
        }
    }

    fn replace<T>(
        &mut self,
        target: &mut T,
        app_overlay_active: bool,
        lease_id: Uuid,
        expected: String,
        replacement: String,
    ) -> CommandExecution
    where
        T: ComposerControlTarget<Lease = L>,
    {
        let Some(lease) = self.leases.get(&lease_id).cloned() else {
            return CommandExecution::Complete(WireResult::not_applied(
                ErrorCode::LeaseUnavailable,
            ));
        };
        if lease.expected_hash != text_hash(&expected) {
            self.leases.remove(&lease_id);
            self.lease_order.retain(|queued| *queued != lease_id);
            if target.thread_id().as_deref() == Some(lease.thread_id.as_str())
                && let ExternalLeaseState::Draft { native } = lease.state
            {
                let _ = target.keep_owned_text(native);
            }
            return CommandExecution::Complete(WireResult::not_applied(
                ErrorCode::ExpectedMismatch,
            ));
        }
        match lease.state {
            ExternalLeaseState::Draft { native } => {
                let Some(snapshot) = available_snapshot(target, app_overlay_active) else {
                    return CommandExecution::Complete(WireResult::not_applied(
                        ErrorCode::ComposerUnavailable,
                    ));
                };
                if lease.thread_id != snapshot.thread_id {
                    return CommandExecution::Complete(WireResult::not_applied(
                        ErrorCode::ComposerUnavailable,
                    ));
                }
                self.leases.remove(&lease_id);
                self.lease_order.retain(|queued| *queued != lease_id);
                match target.replace_owned_text(native, &expected, &replacement) {
                    Ok(()) => {
                        self.rebase_matching_captures(target, &snapshot);
                        CommandExecution::Complete(WireResult::Replaced)
                    }
                    Err(err) => {
                        CommandExecution::Complete(WireResult::not_applied(map_lease_error(err)))
                    }
                }
            }
            ExternalLeaseState::SubmittedIntact { receipt } => {
                if target.thread_id().as_deref() != Some(lease.thread_id.as_str()) {
                    return CommandExecution::Complete(WireResult::not_applied(
                        ErrorCode::ComposerUnavailable,
                    ));
                }
                if expected == replacement {
                    self.leases.remove(&lease_id);
                    self.lease_order.retain(|queued| *queued != lease_id);
                    return CommandExecution::Complete(WireResult::Kept);
                }
                if receipt
                    .submitted_text
                    .get(receipt.range.clone())
                    .is_none_or(|submitted| submitted != expected)
                {
                    self.leases.remove(&lease_id);
                    self.lease_order.retain(|queued| *queued != lease_id);
                    return CommandExecution::Complete(WireResult::not_applied(
                        ErrorCode::ExpectedMismatch,
                    ));
                }
                let correction_id = Uuid::new_v4();
                let context_key = format!("koenig_transcription_correction/{correction_id}");
                let Some(context_value) = mechanical_correction_context(
                    receipt.submission_id,
                    &receipt.submitted_text,
                    receipt.range.clone(),
                    &expected,
                    &replacement,
                ) else {
                    return CommandExecution::Complete(WireResult::not_applied(
                        ErrorCode::CorrectionUnavailable,
                    ));
                };
                if let Some(stored) = self.leases.get_mut(&lease_id) {
                    stored.state = ExternalLeaseState::CorrectionPending { receipt };
                }
                CommandExecution::Correct(PreparedCorrection {
                    lease_id,
                    thread_id: lease.thread_id,
                    context_key,
                    context_value,
                })
            }
            ExternalLeaseState::CorrectionPending { .. } => {
                CommandExecution::Complete(WireResult::not_applied(ErrorCode::LeaseUnavailable))
            }
        }
    }

    fn retire_excess_leases<T>(&mut self, target: &mut T, snapshot: &ComposerSnapshot)
    where
        T: ComposerControlTarget<Lease = L>,
    {
        while self.leases.len() >= MAX_LEASE_COUNT {
            let Some(oldest_id) = self.lease_order.pop_front() else {
                break;
            };
            let Some(oldest) = self.leases.remove(&oldest_id) else {
                continue;
            };
            if oldest.thread_id == snapshot.thread_id
                && let ExternalLeaseState::Draft { native } = oldest.state
            {
                let _ = target.keep_owned_text(native);
            }
        }
    }

    fn note_submission_accepted_by_native(
        &mut self,
        thread_id: &str,
        submission_id: Uuid,
        submitted_text: Arc<str>,
        native_leases: &[(L, Range<usize>)],
    ) {
        for lease in self.leases.values_mut() {
            if lease.thread_id != thread_id {
                continue;
            }
            let ExternalLeaseState::Draft { native } = &lease.state else {
                continue;
            };
            let Some((_, range)) = native_leases
                .iter()
                .find(|(submitted_native, _)| submitted_native == native)
            else {
                continue;
            };
            if submitted_text
                .get(range.clone())
                .is_some_and(|submitted| text_hash(submitted) == lease.expected_hash)
            {
                lease.state = ExternalLeaseState::SubmittedIntact {
                    receipt: SubmittedLeaseReceipt {
                        submission_id,
                        submitted_text: Arc::clone(&submitted_text),
                        range: range.clone(),
                    },
                };
            }
        }
    }
}

impl NativeComposerControlState {
    pub(crate) fn note_submission_accepted(&mut self, submission: &NativeComposerSubmission) {
        let native_leases = submission
            .submitted_leases()
            .iter()
            .map(|lease| (lease.native, lease.range.clone()))
            .collect::<Vec<_>>();
        self.note_submission_accepted_by_native(
            submission.thread_id(),
            submission.submission_id(),
            submission.submitted_text_arc(),
            &native_leases,
        );
    }
}

fn mechanical_correction_context(
    submission_id: Uuid,
    submitted_text: &str,
    lease_range: Range<usize>,
    expected: &str,
    replacement: &str,
) -> Option<String> {
    const CONTEXT_TOKENS: usize = 6;
    const OLD_SNIPPET_MAX_CHARS: usize = 640;
    const MAX_HUNK_LITERAL_BYTES: usize = 16 * 1024;
    const MAX_CONTEXT_BYTES: usize = 48 * 1024;

    let mut config = TextDiff::configure();
    config.timeout(Duration::from_millis(100));
    let diff = config.diff_words(expected, replacement);
    let mut hunks = Vec::new();
    for group in diff.grouped_ops(CONTEXT_TOKENS) {
        let mut old = String::new();
        let mut new = String::new();
        let mut changed = false;
        for operation in &group {
            for change in diff.iter_changes(operation) {
                match change.tag() {
                    ChangeTag::Equal => {
                        old.push_str(change.value());
                        new.push_str(change.value());
                    }
                    ChangeTag::Delete => {
                        changed = true;
                        old.push_str(change.value());
                    }
                    ChangeTag::Insert => {
                        changed = true;
                        new.push_str(change.value());
                    }
                }
            }
        }
        if !changed {
            continue;
        }
        let old_slice_start = group
            .first()
            .map(|operation| operation.old_range().start)
            .expect("grouped diff hunk cannot be empty");
        let old_start = diff.old_slices()[..old_slice_start]
            .iter()
            .map(|slice| slice.len())
            .sum::<usize>();
        hunks.push((old_start, old, new));
    }

    if hunks
        .iter()
        .any(|(_, old, new)| old.len() + new.len() > MAX_HUNK_LITERAL_BYTES)
    {
        let (old_start, old, new) = common_affix_hunk(expected, replacement);
        if old.len() + new.len() > MAX_HUNK_LITERAL_BYTES {
            return None;
        }
        hunks = vec![(old_start, old.to_string(), new.to_string())];
    }

    let mut rendered = format!(
        "Automated mechanical transcription correction (application context, not a user \
         message).\nTarget: the same-thread user submission acknowledged under internal receipt \
         koenig-composer-{submission_id}, specifically Koenig-owned submitted-message bytes \
         {}..{}. The receipt is audit metadata; the byte coordinates and exact old text are \
         authoritative. Do not alter text outside that owned range.\nTreat only the exact \
         replacements below as corrections to the prior message. The receiving agent may discern \
         their semantic impact; Koenig asserts no edit beyond the enumerated mechanical \
         replacements.",
        lease_range.start, lease_range.end
    );

    for (index, (old_start, old, new)) in hunks.into_iter().enumerate() {
        let local_old_end = old_start + old.len();
        let submitted_old_start = lease_range.start + old_start;
        let submitted_old_end = lease_range.start + local_old_end;
        let occurrence = (!old.is_empty()).then(|| {
            submitted_text
                .match_indices(&old)
                .take_while(|(position, _)| *position <= submitted_old_start)
                .count()
        });
        let old_char_count = old.chars().count();
        let old_rendered = compact_old_snippet(&old, OLD_SNIPPET_MAX_CHARS);
        let old_rendered =
            serde_json::to_string(&old_rendered).expect("serializing a string cannot fail");
        let new = serde_json::to_string(&new).expect("serializing a string cannot fail");
        if old.is_empty() {
            rendered.push_str(&format!(
                "\n\nHunk {}: insert at submitted-message byte {submitted_old_start}.\n- \
                 \"\"\n+ {new}",
                index + 1,
            ));
        } else if old_char_count <= OLD_SNIPPET_MAX_CHARS {
            rendered.push_str(&format!(
                "\n\nHunk {}: old bytes {old_start}..{old_end}; occurrence {occurrence} of this \
                 exact old text in the submitted message.\n- {old_rendered}\n+ {new}",
                index + 1,
                old_start = submitted_old_start,
                old_end = submitted_old_end,
                occurrence = occurrence.expect("non-empty old text has an occurrence"),
            ));
        } else {
            rendered.push_str(&format!(
                "\n\nHunk {}: old bytes {old_start}..{old_end}; occurrence {occurrence} of this \
                 old range in the submitted message. The old rendering elides its middle; the \
                 byte range plus both anchors is authoritative.\n- {old_rendered}\n+ {new}",
                index + 1,
                old_start = submitted_old_start,
                old_end = submitted_old_end,
                occurrence = occurrence.expect("non-empty old text has an occurrence"),
            ));
        }
        if rendered.len() > MAX_CONTEXT_BYTES {
            return None;
        }
    }

    Some(rendered)
}

fn common_affix_hunk<'a>(old: &'a str, new: &'a str) -> (usize, &'a str, &'a str) {
    let mut prefix = old
        .as_bytes()
        .iter()
        .zip(new.as_bytes())
        .take_while(|(old_byte, new_byte)| old_byte == new_byte)
        .count();
    while !old.is_char_boundary(prefix) || !new.is_char_boundary(prefix) {
        prefix = prefix.saturating_sub(1);
    }

    let max_suffix = old.len().min(new.len()).saturating_sub(prefix);
    let mut suffix = old
        .as_bytes()
        .iter()
        .rev()
        .zip(new.as_bytes().iter().rev())
        .take(max_suffix)
        .take_while(|(old_byte, new_byte)| old_byte == new_byte)
        .count();
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix = suffix.saturating_sub(1);
    }

    (
        prefix,
        &old[prefix..old.len() - suffix],
        &new[prefix..new.len() - suffix],
    )
}

fn compact_old_snippet(value: &str, max_chars: usize) -> String {
    let char_count = value.chars().count();
    if char_count <= max_chars {
        return value.to_string();
    }
    let side = max_chars.saturating_sub(3) / 2;
    let head = value.chars().take(side).collect::<String>();
    let tail = value
        .chars()
        .rev()
        .take(side)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{head} … {tail}")
}

fn available_snapshot<T>(target: &T, app_overlay_active: bool) -> Option<ComposerSnapshot>
where
    T: ComposerControlTarget,
{
    if app_overlay_active {
        None
    } else {
        target.snapshot()
    }
}

fn evict_oldest<T>(values: &mut HashMap<Uuid, T>, order: &mut VecDeque<Uuid>, limit: usize) {
    while values.len() > limit {
        let Some(oldest) = order.pop_front() else {
            break;
        };
        values.remove(&oldest);
    }
}

fn fresh_id<T>(values: &HashMap<Uuid, T>) -> Uuid {
    loop {
        let id = Uuid::new_v4();
        if !values.contains_key(&id) {
            return id;
        }
    }
}

fn text_hash(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

fn map_lease_error(error: ComposerLeaseError) -> ErrorCode {
    match error {
        ComposerLeaseError::EmptyText => ErrorCode::InvalidText,
        ComposerLeaseError::LeaseUnavailable => ErrorCode::LeaseUnavailable,
        ComposerLeaseError::ExpectedTextMismatch => ErrorCode::ExpectedMismatch,
        ComposerLeaseError::CursorConflicts => ErrorCode::CursorConflict,
    }
}

/// Start the iTerm-affine listener, if this process has a parseable iTerm session GUID.
///
/// The returned guard must stay alive for as long as the receiver is polled.
pub(crate) fn start() -> (
    UnboundedReceiver<ComposerControlRequest>,
    Option<ComposerControlServer>,
) {
    let (request_tx, request_rx) = unbounded_channel();
    #[cfg(unix)]
    let server = session_guid_from_env().and_then(|session_guid| {
        let instance_id = Uuid::new_v4();
        match ComposerControlServer::bind(
            std::path::Path::new("/tmp/codex-cc"),
            &session_guid,
            instance_id,
            request_tx,
        ) {
            Ok(server) => Some(server),
            Err(err) => {
                tracing::warn!(error = %err, "native composer control listener unavailable");
                None
            }
        }
    });
    #[cfg(not(unix))]
    let server = {
        drop(request_tx);
        None
    };
    (request_rx, server)
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum WireRequest {
    Capture {
        #[serde(rename = "protocolVersion")]
        protocol_version: u8,
        #[serde(rename = "instanceId")]
        instance_id: Uuid,
    },
    Insert {
        #[serde(rename = "protocolVersion")]
        protocol_version: u8,
        #[serde(rename = "instanceId")]
        instance_id: Uuid,
        #[serde(rename = "captureId")]
        capture_id: Uuid,
        text: String,
    },
    Verify {
        #[serde(rename = "protocolVersion")]
        protocol_version: u8,
        #[serde(rename = "instanceId")]
        instance_id: Uuid,
        #[serde(rename = "leaseId")]
        lease_id: Uuid,
        expected: String,
    },
    Keep {
        #[serde(rename = "protocolVersion")]
        protocol_version: u8,
        #[serde(rename = "instanceId")]
        instance_id: Uuid,
        #[serde(rename = "leaseId")]
        lease_id: Uuid,
    },
    Replace {
        #[serde(rename = "protocolVersion")]
        protocol_version: u8,
        #[serde(rename = "instanceId")]
        instance_id: Uuid,
        #[serde(rename = "leaseId")]
        lease_id: Uuid,
        expected: String,
        replacement: String,
    },
}

impl WireRequest {
    fn into_command(self, expected_instance_id: Uuid) -> Result<ComposerCommand, ErrorCode> {
        let (protocol_version, instance_id, command) = match self {
            Self::Capture {
                protocol_version,
                instance_id,
            } => (protocol_version, instance_id, ComposerCommand::Capture),
            Self::Insert {
                protocol_version,
                instance_id,
                capture_id,
                text,
            } => {
                validate_inserted_text(&text, /*allow_empty*/ false)?;
                (
                    protocol_version,
                    instance_id,
                    ComposerCommand::Insert { capture_id, text },
                )
            }
            Self::Verify {
                protocol_version,
                instance_id,
                lease_id,
                expected,
            } => {
                validate_inserted_text(&expected, /*allow_empty*/ false)?;
                (
                    protocol_version,
                    instance_id,
                    ComposerCommand::Verify { lease_id, expected },
                )
            }
            Self::Keep {
                protocol_version,
                instance_id,
                lease_id,
            } => (
                protocol_version,
                instance_id,
                ComposerCommand::Keep { lease_id },
            ),
            Self::Replace {
                protocol_version,
                instance_id,
                lease_id,
                expected,
                replacement,
            } => {
                validate_inserted_text(&expected, /*allow_empty*/ false)?;
                validate_inserted_text(&replacement, /*allow_empty*/ true)?;
                (
                    protocol_version,
                    instance_id,
                    ComposerCommand::Replace {
                        lease_id,
                        expected,
                        replacement,
                    },
                )
            }
        };
        if protocol_version != PROTOCOL_VERSION {
            return Err(ErrorCode::UnsupportedVersion);
        }
        if instance_id != expected_instance_id {
            return Err(ErrorCode::WrongInstance);
        }
        Ok(command)
    }
}

fn validate_inserted_text(text: &str, allow_empty: bool) -> Result<(), ErrorCode> {
    if (!allow_empty && text.is_empty()) || text.len() > MAX_TEXT_BYTES {
        return Err(ErrorCode::InvalidText);
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ErrorCode {
    InvalidRequest,
    UnsupportedVersion,
    WrongInstance,
    RequestTooLarge,
    InvalidText,
    ComposerUnavailable,
    CaptureUnavailable,
    CaptureChanged,
    LeaseUnavailable,
    ExpectedMismatch,
    CursorConflict,
    CorrectionUnavailable,
    UiUnavailable,
    UiTimeout,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum MutationOutcome {
    NotApplied,
    Unknown,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum WireResult {
    Captured {
        #[serde(rename = "captureId")]
        capture_id: Uuid,
    },
    Inserted {
        #[serde(rename = "leaseId")]
        lease_id: Uuid,
    },
    Verified,
    SubmittedIntact,
    Kept,
    Replaced,
    Corrected,
    Error {
        code: ErrorCode,
        outcome: MutationOutcome,
    },
}

impl WireResult {
    fn error(code: ErrorCode, outcome: MutationOutcome) -> Self {
        Self::Error { code, outcome }
    }

    fn not_applied(code: ErrorCode) -> Self {
        Self::error(code, MutationOutcome::NotApplied)
    }
}

#[derive(Serialize)]
struct WireResponse {
    #[serde(rename = "protocolVersion")]
    protocol_version: u8,
    #[serde(rename = "instanceId")]
    instance_id: Uuid,
    #[serde(flatten)]
    result: WireResult,
}

#[cfg(unix)]
pub(crate) struct ComposerControlServer {
    socket_path: std::path::PathBuf,
    session_dir: std::path::PathBuf,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    listener_thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(not(unix))]
pub(crate) struct ComposerControlServer;

#[cfg(unix)]
impl ComposerControlServer {
    fn bind(
        root: &std::path::Path,
        session_guid: &str,
        instance_id: Uuid,
        request_tx: UnboundedSender<ComposerControlRequest>,
    ) -> std::io::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;

        let session_dir = root.join(session_guid);
        std::fs::create_dir_all(&session_dir)?;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o700))?;

        let socket_path = socket_path(root, session_guid, std::process::id(), instance_id);
        let listener = UnixListener::bind(&socket_path)?;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;

        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let listener_shutdown = shutdown.clone();
        let listener_thread = match std::thread::Builder::new()
            .name("codex-composer-control".to_string())
            .spawn(move || {
                listener_loop(listener, instance_id, request_tx, listener_shutdown);
            }) {
            Ok(thread) => thread,
            Err(err) => {
                let _ = std::fs::remove_file(&socket_path);
                return Err(err);
            }
        };
        Ok(Self {
            socket_path,
            session_dir,
            shutdown,
            listener_thread: Some(listener_thread),
        })
    }
}

#[cfg(unix)]
impl Drop for ComposerControlServer {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;

        self.shutdown.store(true, Ordering::Release);
        let _ = std::os::unix::net::UnixStream::connect(&self.socket_path);
        if let Some(thread) = self.listener_thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_dir(&self.session_dir);
    }
}

#[cfg(unix)]
fn listener_loop(
    listener: std::os::unix::net::UnixListener,
    instance_id: Uuid,
    request_tx: UnboundedSender<ComposerControlRequest>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;

    while !shutdown.load(Ordering::Acquire) {
        let Ok((mut stream, _address)) = listener.accept() else {
            if shutdown.load(Ordering::Acquire) {
                break;
            }
            continue;
        };
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let _ = stream.set_read_timeout(Some(SOCKET_READ_TIMEOUT));
        let _ = stream.set_write_timeout(Some(SOCKET_READ_TIMEOUT));
        handle_connection(&mut stream, instance_id, &request_tx);
    }
}

#[cfg(unix)]
fn handle_connection(
    stream: &mut std::os::unix::net::UnixStream,
    instance_id: Uuid,
    request_tx: &UnboundedSender<ComposerControlRequest>,
) {
    let request = match read_wire_request(stream) {
        Ok(request) => request,
        Err(code) => {
            write_response(stream, instance_id, WireResult::not_applied(code));
            return;
        }
    };
    let command = match request.into_command(instance_id) {
        Ok(command) => command,
        Err(code) => {
            write_response(stream, instance_id, WireResult::not_applied(code));
            return;
        }
    };
    let may_change_text = command.may_change_text();

    let (reply, reply_rx) = std::sync::mpsc::sync_channel(1);
    let ui_request = ComposerControlRequest {
        command,
        deadline: Instant::now() + UI_REQUEST_DEADLINE,
        reply,
    };
    if request_tx.send(ui_request).is_err() {
        write_response(
            stream,
            instance_id,
            WireResult::not_applied(ErrorCode::UiUnavailable),
        );
        return;
    }
    let result = match reply_rx.recv_timeout(UI_REPLY_TIMEOUT) {
        Ok(result) => result,
        Err(_) if may_change_text => {
            WireResult::error(ErrorCode::UiTimeout, MutationOutcome::Unknown)
        }
        Err(_) => WireResult::not_applied(ErrorCode::UiTimeout),
    };
    write_response(stream, instance_id, result);
}

#[cfg(unix)]
fn read_wire_request(stream: &std::os::unix::net::UnixStream) -> Result<WireRequest, ErrorCode> {
    use std::io::BufRead;
    use std::io::BufReader;
    use std::io::Read;

    let cloned = stream.try_clone().map_err(|_| ErrorCode::InvalidRequest)?;
    let reader = BufReader::new(cloned);
    let mut limited = reader.take((MAX_WIRE_REQUEST_BYTES + 2) as u64);
    let mut line = Vec::new();
    limited
        .read_until(b'\n', &mut line)
        .map_err(|_| ErrorCode::InvalidRequest)?;
    let has_newline = line.last() == Some(&b'\n');
    let payload_len = line.len().saturating_sub(usize::from(has_newline));
    if payload_len > MAX_WIRE_REQUEST_BYTES {
        return Err(ErrorCode::RequestTooLarge);
    }
    if !has_newline {
        return Err(ErrorCode::InvalidRequest);
    }
    line.pop();
    serde_json::from_slice(&line).map_err(|_| ErrorCode::InvalidRequest)
}

#[cfg(unix)]
fn write_response(
    stream: &mut std::os::unix::net::UnixStream,
    instance_id: Uuid,
    result: WireResult,
) {
    use std::io::Write;

    let response = WireResponse {
        protocol_version: PROTOCOL_VERSION,
        instance_id,
        result,
    };
    if serde_json::to_writer(&mut *stream, &response).is_ok() {
        let _ = stream.write_all(b"\n");
        let _ = stream.flush();
    }
}

#[cfg(unix)]
fn session_guid_from_env() -> Option<String> {
    ["TERM_SESSION_ID", "ITERM_SESSION_ID"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .find_map(|value| session_guid_from_value(&value))
}

#[cfg(unix)]
fn session_guid_from_value(value: &str) -> Option<String> {
    let suffix = value.rsplit(':').next()?;
    Uuid::parse_str(suffix).ok().map(|guid| guid.to_string())
}

#[cfg(unix)]
fn socket_path(
    root: &std::path::Path,
    session_guid: &str,
    pid: u32,
    instance_id: Uuid,
) -> std::path::PathBuf {
    root.join(session_guid)
        .join(format!("{pid}-{instance_id}.sock"))
}

#[cfg(test)]
#[path = "composer_control_tests.rs"]
mod tests;

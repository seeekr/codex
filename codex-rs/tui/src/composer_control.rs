//! Native, content-opaque control plane for the active composer.
//!
//! The listener never uses [`crate::app_event::AppEvent`]: requests enter the main UI loop through
//! a dedicated channel, so transcript-bearing payloads are neither session-logged nor formatted
//! for diagnostics. All capture checks and mutations therefore share the same serialized order as
//! terminal input.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc::SyncSender;
use std::time::Duration;
use std::time::Instant;

use codex_protocol::protocol::MAX_ADDITIONAL_CONTEXT_VALUE_TOKENS;
use codex_utils_string::truncate_middle_with_token_budget;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use similar::ChangeTag;
use similar::DiffTag;
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

mod direct_send;

use self::direct_send::DirectPreparedSubmission;
use self::direct_send::DirectReplaceOutcome;
use self::direct_send::DirectSendReservations;

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

/// Content-opaque composer state frozen before a bounded semantic submit begins.
///
/// Terminal events are deferred while the submit is in flight. Acquire/capture requests that
/// arrive first can therefore be answered against this exact snapshot without borrowing the
/// live widget from the dispatch future.
pub(crate) struct FrozenComposerAcquisition {
    snapshot: Option<ComposerSnapshot>,
    direct_send_available: bool,
    plain_draft_send_available: bool,
    input_epoch: u64,
    nonempty_capture_allowed: bool,
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
    direct_lease_id: Option<Uuid>,
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
            .field("direct", &self.direct_lease_id.is_some())
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
            direct_lease_id: None,
        })
    }

    pub(crate) fn new_direct(
        submission_id: Uuid,
        lease_id: Uuid,
        thread_id: String,
        submitted_text: &str,
    ) -> Self {
        Self {
            submission_id,
            thread_id,
            submitted_text: Arc::from(submitted_text),
            leases: Vec::new(),
            direct_lease_id: Some(lease_id),
        }
    }

    pub(crate) fn client_user_message_id(&self) -> String {
        format!("koenig-composer-{}", self.submission_id)
    }

    pub(crate) fn receipt_context(&self) -> (String, String) {
        let receipt = self.client_user_message_id();
        (
            format!(
                "koenig_transcription_receipt_{}",
                self.submission_id.simple()
            ),
            format!(
                "Koenig submission receipt {receipt}. The immediately following user message \
                 contains the Koenig-owned transcription associated with this receipt. A later \
                 Application-context correction naming this receipt mechanically amends only the \
                 explicitly identified owned range in that message."
            ),
        )
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
                if submission.direct_lease_id.is_some() {
                    return None;
                }
                match thread_id.as_deref() {
                    None => thread_id = Some(submission.thread_id.clone()),
                    Some(existing) if existing == submission.thread_id => {}
                    Some(_) => return None,
                }
                for lease in rebase_submission_leases(&submission, &text)? {
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
            direct_lease_id: None,
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

    pub(crate) fn is_direct(&self) -> bool {
        self.direct_lease_id.is_some()
    }

    pub(crate) fn submitted_text(&self) -> &str {
        &self.submitted_text
    }

    fn direct_lease_id(&self) -> Option<Uuid> {
        self.direct_lease_id
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
    fn direct_send_acquisition_available(&self) -> bool;
    fn plain_draft_send_acquisition_available(&self) -> bool;
    fn is_submission_event(&self, event: &TuiEvent) -> bool;
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

    fn direct_send_acquisition_available(&self) -> bool {
        self.direct_send_acquisition_available()
    }

    fn plain_draft_send_acquisition_available(&self) -> bool {
        self.plain_draft_send_acquisition_available()
    }

    fn is_submission_event(&self, event: &TuiEvent) -> bool {
        self.is_composer_submission_event(event)
    }
}

/// A request from the socket listener to the UI loop. This type deliberately has no `Debug`
/// implementation because its command can carry transcription text.
pub(crate) struct ComposerControlRequest {
    command: ComposerCommand,
    deadline: Instant,
    reply: SyncSender<WireResult>,
}

#[cfg(test)]
impl ComposerControlRequest {
    pub(crate) fn acquire_send_for_test(deadline: Instant) -> Self {
        let (reply, _reply_rx) = std::sync::mpsc::sync_channel(1);
        Self {
            command: ComposerCommand::AcquireSend,
            deadline,
            reply,
        }
    }

    pub(crate) fn keep_for_test(deadline: Instant) -> Self {
        let (reply, _reply_rx) = std::sync::mpsc::sync_channel(1);
        Self {
            command: ComposerCommand::Keep {
                lease_id: Uuid::new_v4(),
            },
            deadline,
            reply,
        }
    }
}

/// Correction ready for the async app-server acknowledgement boundary.
///
/// This type carries transcript-derived context and therefore deliberately has no `Debug`
/// implementation.
pub(crate) struct PendingComposerCorrection {
    lease_id: Uuid,
    origin: CorrectionOrigin,
    thread_id: String,
    correction_id: Uuid,
    expected_client_user_message_id: String,
    payload: String,
    reply: SyncSender<WireResult>,
}

/// One exact native composer submission awaiting the bounded app-server acceptance boundary.
///
/// Transcript text is intentionally retained only in memory and this type has no `Debug`
/// implementation.
pub(crate) struct PendingComposerSubmission<L> {
    lease_id: Uuid,
    native: L,
    expected: String,
    fence: SubmitFence,
    reply: SyncSender<WireResult>,
}

impl<L: Copy> PendingComposerSubmission<L> {
    pub(crate) fn native(&self) -> L {
        self.native
    }

    pub(crate) fn expected(&self) -> &str {
        &self.expected
    }

    pub(crate) fn fence(&self) -> SubmitFence {
        self.fence.clone()
    }
}

/// One direct reservation submission awaiting the bounded app-server acceptance boundary.
///
/// Transcript text is intentionally retained only in memory and this type has no `Debug`
/// implementation.
pub(crate) struct PendingDirectSubmission {
    reservation_id: Uuid,
    submission: NativeComposerSubmission,
    expected: String,
    reply: SyncSender<WireResult>,
}

impl PendingDirectSubmission {
    pub(crate) fn submission(&self) -> &NativeComposerSubmission {
        &self.submission
    }

    pub(crate) fn expected(&self) -> &str {
        &self.expected
    }
}

pub(crate) enum PendingComposerControlAction<L> {
    Correction(PendingComposerCorrection),
    Submission(PendingComposerSubmission<L>),
    DirectSubmission(PendingDirectSubmission),
}

impl PendingComposerCorrection {
    pub(crate) fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub(crate) fn correction_id(&self) -> Uuid {
        self.correction_id
    }

    pub(crate) fn expected_client_user_message_id(&self) -> &str {
        &self.expected_client_user_message_id
    }

    pub(crate) fn payload(&self) -> &str {
        &self.payload
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CorrectionDispatchOutcome {
    Accepted,
    NotApplied,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubmissionDispatchOutcome {
    Accepted,
    AcceptedButUncommitted,
    NotApplied(ErrorCode),
    Unknown,
}

#[derive(Clone)]
pub(crate) struct SubmitFence {
    inner: Arc<SubmitFenceInner>,
}

struct SubmitFenceInner {
    relinquished: AtomicBool,
    disclosed: AtomicBool,
}

impl SubmitFence {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(SubmitFenceInner {
                relinquished: AtomicBool::new(false),
                disclosed: AtomicBool::new(false),
            }),
        }
    }

    pub(crate) fn is_relinquished(&self) -> bool {
        self.inner.relinquished.load(Ordering::Acquire)
    }

    pub(crate) fn relinquish(&self) {
        self.inner.relinquished.store(true, Ordering::Release);
    }

    fn disclose_once(&self) -> bool {
        !self.inner.disclosed.swap(true, Ordering::AcqRel)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TuiEventDisposition {
    Allow,
    BlockUnknownSubmission { disclose: bool },
    BlockDispatchingSubmission { disclose: bool },
}

/// Transcript-bearing command. Do not derive or implement `Debug`.
enum ComposerCommand {
    AcquireSend,
    Capture,
    Insert {
        capture_id: Uuid,
        text: String,
    },
    Verify {
        lease_id: Uuid,
        expected: String,
    },
    Submit {
        lease_id: Uuid,
        expected: String,
    },
    Keep {
        lease_id: Uuid,
    },
    AcknowledgeUnknown {
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
    origin: CorrectionOrigin,
    thread_id: String,
    correction_id: Uuid,
    expected_client_user_message_id: String,
    payload: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CorrectionOrigin {
    Composer,
    Direct,
}

enum CommandExecution<L> {
    Complete(WireResult),
    Correct(PreparedCorrection),
    Submit(PreparedSubmission<L>),
    SubmitDirect(DirectPreparedSubmission),
}

struct PreparedSubmission<L> {
    lease_id: Uuid,
    native: L,
    expected: String,
    fence: SubmitFence,
}

impl ComposerCommand {
    fn may_have_effect(&self) -> bool {
        matches!(
            self,
            Self::AcquireSend
                | Self::Insert { .. }
                | Self::Submit { .. }
                | Self::AcknowledgeUnknown { .. }
                | Self::Replace { .. }
        )
    }
}

#[derive(Clone)]
struct Capture {
    thread_id: String,
    text_hash: [u8; 32],
    cursor: usize,
    input_epoch: u64,
}

#[derive(Clone)]
struct DraftWitness {
    text_hash: [u8; 32],
    cursor: usize,
    input_epoch: u64,
}

#[derive(Clone)]
enum SubmitAttempt {
    Dispatching {
        fence: SubmitFence,
    },
    Resolved {
        result: WireResult,
        fence: Option<SubmitFence>,
    },
}

enum ExternalLeaseState<L> {
    Draft {
        native: L,
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
struct SubmittedLeaseReceipt {
    submission_id: Uuid,
    submitted_text: Arc<str>,
    range: Range<usize>,
}

impl<L: Copy> Clone for ExternalLeaseState<L> {
    fn clone(&self) -> Self {
        match self {
            Self::Draft { native } => Self::Draft { native: *native },
            Self::SubmissionPending { receipt } => Self::SubmissionPending {
                receipt: receipt.clone(),
            },
            Self::SubmittedIntact { receipt } => Self::SubmittedIntact {
                receipt: receipt.clone(),
            },
            Self::SubmissionAbandoned { receipt } => Self::SubmissionAbandoned {
                receipt: receipt.clone(),
            },
            Self::CorrectionPending {
                receipt,
                correction_id,
                expected,
                replacement,
                payload,
            } => Self::CorrectionPending {
                receipt: receipt.clone(),
                correction_id: *correction_id,
                expected: expected.clone(),
                replacement: replacement.clone(),
                payload: payload.clone(),
            },
        }
    }
}

struct ExternalLease<L> {
    state: ExternalLeaseState<L>,
    thread_id: String,
    expected_hash: [u8; 32],
    draft_witness: DraftWitness,
    submit_attempt: Option<SubmitAttempt>,
}

impl<L: Copy> Clone for ExternalLease<L> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            thread_id: self.thread_id.clone(),
            expected_hash: self.expected_hash,
            draft_witness: self.draft_witness.clone(),
            submit_attempt: self.submit_attempt.clone(),
        }
    }
}

fn relinquish_unknown_submit<L>(lease: &mut ExternalLease<L>) {
    let Some(SubmitAttempt::Resolved {
        result: WireResult::Unknown,
        fence: Some(fence),
    }) = lease.submit_attempt.clone()
    else {
        return;
    };
    fence.relinquish();
    lease.submit_attempt = Some(SubmitAttempt::Resolved {
        result: WireResult::Unknown,
        fence: None,
    });
}

fn has_live_submit_fence<L>(lease: &ExternalLease<L>) -> bool {
    match &lease.submit_attempt {
        Some(SubmitAttempt::Dispatching { fence }) => !fence.is_relinquished(),
        Some(SubmitAttempt::Resolved {
            result: WireResult::Unknown,
            fence: Some(fence),
        }) => !fence.is_relinquished(),
        _ => false,
    }
}

fn has_unresolved_draft_provenance<L>(lease: &ExternalLease<L>) -> bool {
    matches!(&lease.state, ExternalLeaseState::Draft { .. })
        && matches!(
            &lease.submit_attempt,
            Some(
                SubmitAttempt::Dispatching { .. }
                    | SubmitAttempt::Resolved {
                        result: WireResult::Unknown,
                        ..
                    }
            )
        )
}

pub(crate) struct ComposerControlState<L> {
    input_epoch: u64,
    user_chronology_epoch: Arc<AtomicU64>,
    captures: HashMap<Uuid, Capture>,
    capture_order: VecDeque<Uuid>,
    leases: HashMap<Uuid, ExternalLease<L>>,
    lease_order: VecDeque<Uuid>,
    kept_leases: HashSet<Uuid>,
    kept_lease_order: VecDeque<Uuid>,
    unknown_acknowledged_leases: HashSet<Uuid>,
    unknown_acknowledged_lease_order: VecDeque<Uuid>,
    direct_sends: DirectSendReservations,
}

pub(crate) type NativeComposerControlState = ComposerControlState<ComposerLeaseId>;

impl<L: Copy + Eq> ComposerControlState<L> {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::with_user_chronology_epoch(Arc::new(AtomicU64::new(0)))
    }

    pub(crate) fn with_user_chronology_epoch(user_chronology_epoch: Arc<AtomicU64>) -> Self {
        Self {
            input_epoch: 0,
            user_chronology_epoch,
            captures: HashMap::new(),
            capture_order: VecDeque::new(),
            leases: HashMap::new(),
            lease_order: VecDeque::new(),
            kept_leases: HashSet::new(),
            kept_lease_order: VecDeque::new(),
            unknown_acknowledged_leases: HashSet::new(),
            unknown_acknowledged_lease_order: VecDeque::new(),
            direct_sends: DirectSendReservations::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn note_user_chronology_change(&self) {
        self.user_chronology_epoch.fetch_add(1, Ordering::AcqRel);
    }

    fn current_user_chronology_epoch(&self) -> u64 {
        self.user_chronology_epoch.load(Ordering::Acquire)
    }

    pub(crate) fn freeze_acquisition<T>(
        &self,
        target: &T,
        app_overlay_active: bool,
        nonempty_capture_allowed: bool,
    ) -> FrozenComposerAcquisition
    where
        T: ComposerControlTarget<Lease = L>,
    {
        FrozenComposerAcquisition {
            snapshot: available_snapshot(target, app_overlay_active),
            direct_send_available: target.direct_send_acquisition_available(),
            plain_draft_send_available: target.plain_draft_send_acquisition_available(),
            input_epoch: self.input_epoch,
            nonempty_capture_allowed,
        }
    }

    /// Service only Stop-time acquisition operations while another semantic submit owns the live
    /// widget. Other operations are returned unchanged for normal serialized processing.
    pub(crate) fn handle_frozen_acquisition_request(
        &mut self,
        request: ComposerControlRequest,
        frozen: &FrozenComposerAcquisition,
        tainted_by_prior_terminal_input: bool,
    ) -> Option<ComposerControlRequest> {
        if !matches!(
            &request.command,
            ComposerCommand::AcquireSend | ComposerCommand::Capture
        ) {
            return Some(request);
        }
        let ComposerControlRequest {
            command,
            deadline,
            reply,
        } = request;
        let result = if Instant::now() > deadline {
            WireResult::not_applied(ErrorCode::UiTimeout)
        } else if tainted_by_prior_terminal_input {
            WireResult::not_applied(ErrorCode::SubmissionUnavailable)
        } else {
            match command {
                ComposerCommand::AcquireSend => self.acquire_send_frozen(frozen),
                ComposerCommand::Capture => self.capture_frozen(frozen),
                _ => unreachable!("acquisition operations checked above"),
            }
        };
        let _ = reply.send(result);
        None
    }

    /// Conservatively invalidate capture compare-and-swap tokens before any terminal input is
    /// routed. Even a key handled by another surface advances the epoch; false rejection is safer
    /// than inserting against an input order the producer did not capture.
    pub(crate) fn note_tui_event(&mut self, event: &TuiEvent) {
        if matches!(event, TuiEvent::Key(_) | TuiEvent::Paste(_)) {
            self.input_epoch = self.input_epoch.wrapping_add(1);
        }
    }

    /// Fence a still-exact draft whose semantic submission has an unknown outcome. A submit key
    /// is consumed so the user cannot duplicate a turn that may already exist. Other input is
    /// observed after routing by [`Self::finish_tui_event`], so global shortcuts and thread
    /// switches cannot accidentally relinquish a draft they did not change.
    pub(crate) fn prepare_tui_event<T>(
        &mut self,
        event: &TuiEvent,
        target: &T,
    ) -> TuiEventDisposition
    where
        T: ComposerControlTarget<Lease = L>,
    {
        self.prepare_tui_event_inner(event, target, None)
    }

    /// Apply the submission identity captured when an input event arrived while the bounded
    /// semantic dispatch was still in flight. Such a submit event belongs to that dispatch and
    /// must never become a second ordinary composer dispatch, even if the server later refuses.
    pub(crate) fn prepare_tui_event_during_submission<T>(
        &mut self,
        event: &TuiEvent,
        target: &T,
        dispatch_fence: &SubmitFence,
    ) -> TuiEventDisposition
    where
        T: ComposerControlTarget<Lease = L>,
    {
        self.prepare_tui_event_inner(event, target, Some(dispatch_fence))
    }

    fn prepare_tui_event_inner<T>(
        &mut self,
        event: &TuiEvent,
        target: &T,
        dispatch_fence: Option<&SubmitFence>,
    ) -> TuiEventDisposition
    where
        T: ComposerControlTarget<Lease = L>,
    {
        if !matches!(event, TuiEvent::Key(_) | TuiEvent::Paste(_)) {
            return TuiEventDisposition::Allow;
        }
        let is_submission_event = target.is_submission_event(event);
        if is_submission_event && let Some(dispatch_fence) = dispatch_fence {
            return TuiEventDisposition::BlockDispatchingSubmission {
                disclose: dispatch_fence.disclose_once(),
            };
        }
        let Some(snapshot) = target.snapshot() else {
            return TuiEventDisposition::Allow;
        };
        for lease in self.leases.values_mut() {
            let Some(SubmitAttempt::Resolved {
                result: WireResult::Unknown,
                fence: Some(fence),
            }) = lease.submit_attempt.clone()
            else {
                continue;
            };
            if lease.thread_id != snapshot.thread_id {
                continue;
            }
            if fence.is_relinquished() {
                relinquish_unknown_submit(lease);
                continue;
            }
            let exact = lease.draft_witness.text_hash == text_hash(&snapshot.text);
            if exact && is_submission_event {
                return TuiEventDisposition::BlockUnknownSubmission {
                    disclose: fence.disclose_once(),
                };
            }
            if !exact {
                relinquish_unknown_submit(lease);
            }
        }
        TuiEventDisposition::Allow
    }

    /// Resolve ambiguity only after an allowed event actually changed the active composer. The
    /// app calls this after routing the event through every global and local key handler.
    pub(crate) fn finish_tui_event<T>(&mut self, target: &T)
    where
        T: ComposerControlTarget<Lease = L>,
    {
        let Some(snapshot) = target.snapshot() else {
            return;
        };
        let snapshot_hash = text_hash(&snapshot.text);
        for lease in self.leases.values_mut() {
            if lease.thread_id != snapshot.thread_id {
                continue;
            }
            let unresolved = matches!(
                &lease.submit_attempt,
                Some(SubmitAttempt::Resolved {
                    result: WireResult::Unknown,
                    fence: Some(_),
                })
            );
            let still_exact = lease.draft_witness.text_hash == snapshot_hash;
            if unresolved && !still_exact {
                relinquish_unknown_submit(lease);
            }
        }
    }

    pub(crate) fn handle_ui_request<T>(
        &mut self,
        request: ComposerControlRequest,
        target: &mut T,
        app_overlay_active: bool,
    ) -> Option<PendingComposerControlAction<L>>
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
            CommandExecution::Correct(correction) => Some(
                PendingComposerControlAction::Correction(PendingComposerCorrection {
                    lease_id: correction.lease_id,
                    origin: correction.origin,
                    thread_id: correction.thread_id,
                    correction_id: correction.correction_id,
                    expected_client_user_message_id: correction.expected_client_user_message_id,
                    payload: correction.payload,
                    reply,
                }),
            ),
            CommandExecution::Submit(submission) => Some(PendingComposerControlAction::Submission(
                PendingComposerSubmission {
                    lease_id: submission.lease_id,
                    native: submission.native,
                    expected: submission.expected,
                    fence: submission.fence,
                    reply,
                },
            )),
            CommandExecution::SubmitDirect(submission) => Some(
                PendingComposerControlAction::DirectSubmission(PendingDirectSubmission {
                    reservation_id: submission.reservation_id,
                    submission: submission.submission,
                    expected: submission.expected,
                    reply,
                }),
            ),
        }
    }

    pub(crate) fn finish_submission(
        &mut self,
        pending: PendingComposerSubmission<L>,
        outcome: SubmissionDispatchOutcome,
    ) {
        let result = match outcome {
            SubmissionDispatchOutcome::Accepted => WireResult::SendAccepted,
            SubmissionDispatchOutcome::AcceptedButUncommitted
            | SubmissionDispatchOutcome::Unknown => WireResult::Unknown,
            SubmissionDispatchOutcome::NotApplied(code) => WireResult::not_applied(code),
        };
        if let Some(lease) = self.leases.get_mut(&pending.lease_id) {
            let fence = matches!(result, WireResult::Unknown).then_some(pending.fence.clone());
            lease.submit_attempt = Some(SubmitAttempt::Resolved {
                result: result.clone(),
                fence,
            });
        }
        let _ = pending.reply.send(result);
    }

    pub(crate) fn finish_direct_submission(
        &mut self,
        pending: PendingDirectSubmission,
        outcome: SubmissionDispatchOutcome,
    ) {
        let result = self.direct_sends.finish_submission(
            pending.reservation_id,
            pending.submission.submission_id(),
            outcome,
        );
        let _ = pending.reply.send(result);
    }

    pub(crate) fn finish_correction(
        &mut self,
        pending: PendingComposerCorrection,
        outcome: CorrectionDispatchOutcome,
    ) {
        if pending.origin == CorrectionOrigin::Direct {
            let result = self.direct_sends.finish_correction(
                pending.lease_id,
                pending.correction_id,
                outcome,
            );
            let _ = pending.reply.send(result);
            return;
        }
        let result = match outcome {
            CorrectionDispatchOutcome::Accepted => {
                let is_superseded = self.leases.get(&pending.lease_id).is_some_and(|lease| {
                    matches!(
                        &lease.state,
                        ExternalLeaseState::CorrectionPending {
                            correction_id,
                            ..
                        } if *correction_id != pending.correction_id
                    )
                });
                if !is_superseded {
                    self.leases.remove(&pending.lease_id);
                    self.lease_order
                        .retain(|queued| *queued != pending.lease_id);
                }
                WireResult::CorrectionQueued
            }
            CorrectionDispatchOutcome::NotApplied => {
                if let Some(lease) = self.leases.get_mut(&pending.lease_id)
                    && let ExternalLeaseState::CorrectionPending {
                        receipt,
                        correction_id,
                        ..
                    } = &lease.state
                    && *correction_id == pending.correction_id
                {
                    lease.state = ExternalLeaseState::SubmittedIntact {
                        receipt: receipt.clone(),
                    };
                }
                WireResult::not_applied(ErrorCode::CorrectionUnavailable)
            }
            CorrectionDispatchOutcome::Unknown => WireResult::CorrectionPending,
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
                if correction.origin == CorrectionOrigin::Direct {
                    return self.direct_sends.finish_correction(
                        correction.lease_id,
                        correction.correction_id,
                        CorrectionDispatchOutcome::NotApplied,
                    );
                }
                if let Some(lease) = self.leases.get_mut(&correction.lease_id)
                    && let ExternalLeaseState::CorrectionPending { receipt, .. } = &lease.state
                {
                    lease.state = ExternalLeaseState::SubmittedIntact {
                        receipt: receipt.clone(),
                    };
                }
                WireResult::not_applied(ErrorCode::CorrectionUnavailable)
            }
            CommandExecution::Submit(_) => {
                WireResult::not_applied(ErrorCode::SubmissionUnavailable)
            }
            CommandExecution::SubmitDirect(_) => {
                WireResult::not_applied(ErrorCode::SubmissionUnavailable)
            }
        }
    }

    fn prepare_command<T>(
        &mut self,
        command: ComposerCommand,
        target: &mut T,
        app_overlay_active: bool,
    ) -> CommandExecution<L>
    where
        T: ComposerControlTarget<Lease = L>,
    {
        match command {
            ComposerCommand::AcquireSend => {
                CommandExecution::Complete(self.acquire_send(target, app_overlay_active))
            }
            ComposerCommand::Capture => {
                CommandExecution::Complete(self.capture(target, app_overlay_active))
            }
            ComposerCommand::Insert { capture_id, text } => CommandExecution::Complete(
                self.insert(target, app_overlay_active, capture_id, text),
            ),
            ComposerCommand::Verify { lease_id, expected } => CommandExecution::Complete(
                self.verify(target, app_overlay_active, lease_id, expected),
            ),
            ComposerCommand::Submit { lease_id, expected } => {
                self.submit(target, app_overlay_active, lease_id, expected)
            }
            ComposerCommand::Keep { lease_id } => {
                CommandExecution::Complete(self.keep(target, app_overlay_active, lease_id))
            }
            ComposerCommand::AcknowledgeUnknown { lease_id } => {
                CommandExecution::Complete(self.acknowledge_unknown(target, lease_id))
            }
            ComposerCommand::Replace {
                lease_id,
                expected,
                replacement,
            } => self.replace(target, app_overlay_active, lease_id, expected, replacement),
        }
    }

    fn acquire_send<T>(&mut self, target: &T, app_overlay_active: bool) -> WireResult
    where
        T: ComposerControlTarget<Lease = L>,
    {
        let Some(snapshot) = available_snapshot(target, app_overlay_active) else {
            return WireResult::not_applied(ErrorCode::ComposerUnavailable);
        };
        if snapshot.text.is_empty() {
            if !target.direct_send_acquisition_available() {
                return WireResult::not_applied(ErrorCode::SubmissionUnavailable);
            }
            let user_chronology_epoch = self.current_user_chronology_epoch();
            return self
                .direct_sends
                .acquire(snapshot.thread_id, user_chronology_epoch);
        }
        if !target.plain_draft_send_acquisition_available() {
            return WireResult::not_applied(ErrorCode::SubmissionUnavailable);
        }
        self.capture(target, app_overlay_active)
    }

    fn acquire_send_frozen(&mut self, frozen: &FrozenComposerAcquisition) -> WireResult {
        let Some(snapshot) = frozen.snapshot.as_ref() else {
            return WireResult::not_applied(ErrorCode::ComposerUnavailable);
        };
        if snapshot.text.is_empty() {
            if !frozen.direct_send_available {
                return WireResult::not_applied(ErrorCode::SubmissionUnavailable);
            }
            return self.direct_sends.acquire(
                snapshot.thread_id.clone(),
                self.current_user_chronology_epoch(),
            );
        }
        if !frozen.nonempty_capture_allowed || !frozen.plain_draft_send_available {
            return WireResult::not_applied(ErrorCode::SubmissionUnavailable);
        }
        self.capture_snapshot(snapshot, frozen.input_epoch)
    }

    fn capture<T>(&mut self, target: &T, app_overlay_active: bool) -> WireResult
    where
        T: ComposerControlTarget<Lease = L>,
    {
        let Some(snapshot) = available_snapshot(target, app_overlay_active) else {
            return WireResult::not_applied(ErrorCode::ComposerUnavailable);
        };
        self.capture_snapshot(&snapshot, self.input_epoch)
    }

    fn capture_frozen(&mut self, frozen: &FrozenComposerAcquisition) -> WireResult {
        let Some(snapshot) = frozen.snapshot.as_ref() else {
            return WireResult::not_applied(ErrorCode::ComposerUnavailable);
        };
        if !snapshot.text.is_empty() && !frozen.nonempty_capture_allowed {
            return WireResult::not_applied(ErrorCode::SubmissionUnavailable);
        }
        self.capture_snapshot(snapshot, frozen.input_epoch)
    }

    fn capture_snapshot(&mut self, snapshot: &ComposerSnapshot, input_epoch: u64) -> WireResult {
        let capture_id = fresh_id(&self.captures);
        self.captures.insert(
            capture_id,
            Capture {
                thread_id: snapshot.thread_id.clone(),
                text_hash: text_hash(&snapshot.text),
                cursor: snapshot.cursor,
                input_epoch,
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
        let Some(capture) = self.captures.get(&capture_id).cloned() else {
            return WireResult::not_applied(ErrorCode::CaptureUnavailable);
        };
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
        if self
            .leases
            .values()
            .any(|lease| lease.thread_id == snapshot.thread_id && has_live_submit_fence(lease))
        {
            return WireResult::Unknown;
        }

        if !self.retire_excess_leases(target, &snapshot) {
            return WireResult::not_applied(ErrorCode::LeaseUnavailable);
        }
        self.captures.remove(&capture_id);
        self.capture_order.retain(|queued| *queued != capture_id);
        let expected_hash = text_hash(&text);
        let native = match target.insert_owned_text(&text) {
            Ok(native) => native,
            Err(err) => return WireResult::not_applied(map_lease_error(err)),
        };
        let Some(after) = target.snapshot() else {
            let _ = target.keep_owned_text(native);
            return WireResult::not_applied(ErrorCode::ComposerUnavailable);
        };
        if after.thread_id != snapshot.thread_id {
            let _ = target.keep_owned_text(native);
            return WireResult::not_applied(ErrorCode::ComposerUnavailable);
        }
        self.rebase_matching_captures(target, &snapshot);
        let lease_id = loop {
            let candidate = Uuid::new_v4();
            if !self.leases.contains_key(&candidate)
                && !self.kept_leases.contains(&candidate)
                && !self.unknown_acknowledged_leases.contains(&candidate)
            {
                break candidate;
            }
        };
        self.leases.insert(
            lease_id,
            ExternalLease {
                state: ExternalLeaseState::Draft { native },
                thread_id: snapshot.thread_id,
                expected_hash,
                draft_witness: DraftWitness {
                    text_hash: text_hash(&after.text),
                    cursor: after.cursor,
                    input_epoch: self.input_epoch,
                },
                submit_attempt: None,
            },
        );
        self.lease_order.push_back(lease_id);
        WireResult::Inserted { lease_id }
    }

    fn submit<T>(
        &mut self,
        target: &T,
        app_overlay_active: bool,
        lease_id: Uuid,
        expected: String,
    ) -> CommandExecution<L>
    where
        T: ComposerControlTarget<Lease = L>,
    {
        if self.direct_sends.contains(lease_id) {
            let current_thread_id = target.thread_id();
            let current_user_chronology_epoch = self.current_user_chronology_epoch();
            return match self.direct_sends.prepare_submission(
                lease_id,
                expected,
                current_thread_id.as_deref(),
                current_user_chronology_epoch,
            ) {
                Ok(submission) => CommandExecution::SubmitDirect(submission),
                Err(result) => CommandExecution::Complete(result),
            };
        }
        let Some(lease) = self.leases.get(&lease_id).cloned() else {
            return CommandExecution::Complete(WireResult::not_applied(
                ErrorCode::LeaseUnavailable,
            ));
        };
        if let Some(attempt) = &lease.submit_attempt {
            return CommandExecution::Complete(match attempt {
                SubmitAttempt::Dispatching { .. } => WireResult::Unknown,
                SubmitAttempt::Resolved { result, .. } => result.clone(),
            });
        }
        if lease.expected_hash != text_hash(&expected) {
            let result = WireResult::not_applied(ErrorCode::ExpectedMismatch);
            if let Some(stored) = self.leases.get_mut(&lease_id) {
                stored.submit_attempt = Some(SubmitAttempt::Resolved {
                    result: result.clone(),
                    fence: None,
                });
            }
            return CommandExecution::Complete(result);
        }
        match &lease.state {
            ExternalLeaseState::SubmittedIntact { .. }
            | ExternalLeaseState::CorrectionPending { .. } => {
                return CommandExecution::Complete(WireResult::SubmittedIntact);
            }
            ExternalLeaseState::SubmissionAbandoned { .. } => {
                return CommandExecution::Complete(WireResult::SubmissionAbandoned);
            }
            ExternalLeaseState::Draft { .. } | ExternalLeaseState::SubmissionPending { .. } => {}
        }
        if matches!(&lease.state, ExternalLeaseState::SubmissionPending { .. }) {
            return CommandExecution::Complete(WireResult::SendAccepted);
        }
        let ExternalLeaseState::Draft { native } = lease.state else {
            unreachable!("terminal and pending states checked above")
        };
        let Some(snapshot) = available_snapshot(target, app_overlay_active) else {
            let result = if target.thread_id().as_deref() == Some(lease.thread_id.as_str()) {
                WireResult::not_applied(ErrorCode::ComposerUnavailable)
            } else {
                WireResult::Unknown
            };
            if let Some(stored) = self.leases.get_mut(&lease_id) {
                stored.submit_attempt = Some(SubmitAttempt::Resolved {
                    result: result.clone(),
                    fence: None,
                });
            }
            return CommandExecution::Complete(result);
        };
        if lease.thread_id != snapshot.thread_id {
            if let Some(stored) = self.leases.get_mut(&lease_id) {
                stored.submit_attempt = Some(SubmitAttempt::Resolved {
                    result: WireResult::Unknown,
                    fence: None,
                });
            }
            return CommandExecution::Complete(WireResult::Unknown);
        }
        if lease.draft_witness.text_hash != text_hash(&snapshot.text)
            || lease.draft_witness.cursor != snapshot.cursor
            || lease.draft_witness.input_epoch != self.input_epoch
        {
            let result = if matches!(
                target.verify_owned_text(native, &expected),
                Err(ComposerLeaseError::LeaseUnavailable)
            ) {
                WireResult::Unknown
            } else {
                WireResult::not_applied(ErrorCode::CaptureChanged)
            };
            if let Some(stored) = self.leases.get_mut(&lease_id) {
                stored.submit_attempt = Some(SubmitAttempt::Resolved {
                    result: result.clone(),
                    fence: None,
                });
            }
            return CommandExecution::Complete(result);
        }
        if let Err(error) = target.verify_owned_text(native, &expected) {
            let result = WireResult::not_applied(map_lease_error(error));
            if let Some(stored) = self.leases.get_mut(&lease_id) {
                stored.submit_attempt = Some(SubmitAttempt::Resolved {
                    result: result.clone(),
                    fence: None,
                });
            }
            return CommandExecution::Complete(result);
        }

        if let Some(existing_fence) = self.leases.values().find_map(|candidate| {
            if candidate.thread_id != lease.thread_id {
                return None;
            }
            candidate
                .submit_attempt
                .as_ref()
                .and_then(|attempt| match attempt {
                    SubmitAttempt::Dispatching { fence }
                    | SubmitAttempt::Resolved {
                        result: WireResult::Unknown,
                        fence: Some(fence),
                    } => Some(fence.clone()),
                    SubmitAttempt::Resolved { .. } => None,
                })
        }) {
            if let Some(stored) = self.leases.get_mut(&lease_id) {
                stored.submit_attempt = Some(SubmitAttempt::Resolved {
                    result: WireResult::Unknown,
                    fence: Some(existing_fence),
                });
            }
            return CommandExecution::Complete(WireResult::Unknown);
        }

        let fence = SubmitFence::new();
        if let Some(stored) = self.leases.get_mut(&lease_id) {
            stored.submit_attempt = Some(SubmitAttempt::Dispatching {
                fence: fence.clone(),
            });
        }
        CommandExecution::Submit(PreparedSubmission {
            lease_id,
            native,
            expected,
            fence,
        })
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
        if self.direct_sends.contains(lease_id) {
            let current_thread_id = target.thread_id();
            return self.direct_sends.verify(
                lease_id,
                &expected,
                current_thread_id.as_deref(),
                self.current_user_chronology_epoch(),
            );
        }
        let Some(lease) = self.leases.get(&lease_id) else {
            return WireResult::not_applied(ErrorCode::LeaseUnavailable);
        };
        if lease.expected_hash != text_hash(&expected) {
            return WireResult::not_applied(ErrorCode::ExpectedMismatch);
        }
        match &lease.state {
            ExternalLeaseState::SubmittedIntact { .. } => WireResult::SubmittedIntact,
            ExternalLeaseState::SubmissionAbandoned { .. } => WireResult::SubmissionAbandoned,
            ExternalLeaseState::CorrectionPending { .. } => {
                WireResult::not_applied(ErrorCode::LeaseUnavailable)
            }
            ExternalLeaseState::SubmissionPending { .. }
                if matches!(
                    &lease.submit_attempt,
                    Some(SubmitAttempt::Resolved {
                        result: WireResult::Unknown,
                        fence: None,
                    })
                ) =>
            {
                WireResult::Unknown
            }
            ExternalLeaseState::Draft { .. }
                if matches!(
                    &lease.submit_attempt,
                    Some(SubmitAttempt::Resolved {
                        result: WireResult::Unknown | WireResult::SubmissionAbandoned,
                        ..
                    })
                ) =>
            {
                match &lease.submit_attempt {
                    Some(SubmitAttempt::Resolved { result, .. }) => result.clone(),
                    _ => unreachable!("guard matched a resolved submit attempt"),
                }
            }
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
            ExternalLeaseState::SubmissionPending { .. } => WireResult::SubmissionPending,
        }
    }

    fn keep<T>(&mut self, target: &mut T, app_overlay_active: bool, lease_id: Uuid) -> WireResult
    where
        T: ComposerControlTarget<Lease = L>,
    {
        if self.direct_sends.contains_keep_id(lease_id) {
            return self.direct_sends.keep(lease_id);
        }
        let Some(lease) = self.leases.get(&lease_id).cloned() else {
            return if self.kept_leases.contains(&lease_id) {
                WireResult::Kept
            } else {
                WireResult::not_applied(ErrorCode::LeaseUnavailable)
            };
        };
        if has_live_submit_fence(&lease) {
            return WireResult::Unknown;
        }
        if matches!(&lease.state, ExternalLeaseState::SubmissionPending { .. }) {
            return WireResult::SubmissionPending;
        }
        let draft_mutation_blocked = has_unresolved_draft_provenance(&lease)
            || self.leases.values().any(|candidate| {
                candidate.thread_id == lease.thread_id && has_live_submit_fence(candidate)
            });
        if matches!(&lease.state, ExternalLeaseState::Draft { .. }) && draft_mutation_blocked {
            return WireResult::Unknown;
        }
        match lease.state {
            ExternalLeaseState::Draft { native } => {
                let Some(snapshot) = available_snapshot(target, app_overlay_active) else {
                    return WireResult::not_applied(ErrorCode::ComposerUnavailable);
                };
                if lease.thread_id != snapshot.thread_id {
                    return WireResult::not_applied(ErrorCode::ComposerUnavailable);
                }
                match target.keep_owned_text(native) {
                    Ok(()) => {
                        self.remove_lease_kept(lease_id);
                        WireResult::Kept
                    }
                    Err(err) => WireResult::not_applied(map_lease_error(err)),
                }
            }
            ExternalLeaseState::SubmissionPending { .. } => unreachable!("handled above"),
            ExternalLeaseState::SubmittedIntact { .. } => {
                self.remove_lease_kept(lease_id);
                WireResult::Kept
            }
            ExternalLeaseState::SubmissionAbandoned { .. } => {
                self.remove_lease_kept(lease_id);
                WireResult::Kept
            }
            ExternalLeaseState::CorrectionPending { .. } => {
                WireResult::not_applied(ErrorCode::LeaseUnavailable)
            }
        }
    }

    fn acknowledge_unknown<T>(&mut self, target: &mut T, lease_id: Uuid) -> WireResult
    where
        T: ComposerControlTarget<Lease = L>,
    {
        if self.direct_sends.contains_acknowledgement_id(lease_id) {
            return self.direct_sends.acknowledge_unknown(lease_id);
        }
        let Some(lease) = self.leases.get(&lease_id).cloned() else {
            return if self.unknown_acknowledged_leases.contains(&lease_id) {
                WireResult::UnknownAcknowledged
            } else {
                WireResult::not_applied(ErrorCode::LeaseUnavailable)
            };
        };
        match &lease.state {
            ExternalLeaseState::SubmissionPending { .. } => {
                return WireResult::SubmissionPending;
            }
            ExternalLeaseState::SubmittedIntact { .. }
            | ExternalLeaseState::CorrectionPending { .. } => {
                return WireResult::SubmittedIntact;
            }
            ExternalLeaseState::SubmissionAbandoned { .. } => {
                return WireResult::SubmissionAbandoned;
            }
            ExternalLeaseState::Draft { .. } => {}
        }
        let ExternalLeaseState::Draft { native } = lease.state else {
            unreachable!("terminal and pending states checked above")
        };
        match lease.submit_attempt {
            Some(SubmitAttempt::Dispatching { .. }) => WireResult::Unknown,
            Some(SubmitAttempt::Resolved {
                result: WireResult::Unknown,
                fence,
            }) => {
                if let Some(fence) = fence {
                    fence.relinquish();
                }
                // A missing native lease already means ownership was relinquished by another
                // serialized composer change. Either way, explicit acknowledgement preserves
                // the visible text and retires this producer's local edit right.
                let _ = target.keep_owned_text(native);
                self.remove_lease_unknown_acknowledged(lease_id);
                WireResult::UnknownAcknowledged
            }
            Some(SubmitAttempt::Resolved { result, .. }) => result,
            None => WireResult::not_applied(ErrorCode::LeaseUnavailable),
        }
    }

    fn remove_lease_kept(&mut self, lease_id: Uuid) {
        self.leases.remove(&lease_id);
        self.lease_order.retain(|queued| *queued != lease_id);
        if self.kept_leases.insert(lease_id) {
            self.kept_lease_order.push_back(lease_id);
        }
        while self.kept_leases.len() > MAX_LEASE_COUNT {
            let Some(expired) = self.kept_lease_order.pop_front() else {
                break;
            };
            self.kept_leases.remove(&expired);
        }
    }

    fn remove_lease_unknown_acknowledged(&mut self, lease_id: Uuid) {
        self.leases.remove(&lease_id);
        self.lease_order.retain(|queued| *queued != lease_id);
        if self.unknown_acknowledged_leases.insert(lease_id) {
            self.unknown_acknowledged_lease_order.push_back(lease_id);
        }
        while self.unknown_acknowledged_leases.len() > MAX_LEASE_COUNT {
            let Some(expired) = self.unknown_acknowledged_lease_order.pop_front() else {
                break;
            };
            self.unknown_acknowledged_leases.remove(&expired);
        }
    }

    fn replace<T>(
        &mut self,
        target: &mut T,
        app_overlay_active: bool,
        lease_id: Uuid,
        expected: String,
        replacement: String,
    ) -> CommandExecution<L>
    where
        T: ComposerControlTarget<Lease = L>,
    {
        if self.direct_sends.contains(lease_id) {
            return match self.direct_sends.replace(lease_id, expected, replacement) {
                DirectReplaceOutcome::Complete(result) => CommandExecution::Complete(result),
                DirectReplaceOutcome::Correct(correction) => CommandExecution::Correct(correction),
            };
        }
        let Some(lease) = self.leases.get(&lease_id).cloned() else {
            return CommandExecution::Complete(WireResult::not_applied(
                ErrorCode::LeaseUnavailable,
            ));
        };
        if has_live_submit_fence(&lease) {
            return CommandExecution::Complete(WireResult::Unknown);
        }
        if matches!(&lease.state, ExternalLeaseState::SubmissionPending { .. }) {
            return CommandExecution::Complete(WireResult::SubmissionPending);
        }
        let draft_mutation_blocked = has_unresolved_draft_provenance(&lease)
            || self.leases.values().any(|candidate| {
                candidate.thread_id == lease.thread_id && has_live_submit_fence(candidate)
            });
        if matches!(&lease.state, ExternalLeaseState::Draft { .. }) && draft_mutation_blocked {
            return CommandExecution::Complete(WireResult::Unknown);
        }
        if lease.expected_hash != text_hash(&expected) {
            if matches!(lease.state, ExternalLeaseState::CorrectionPending { .. }) {
                return CommandExecution::Complete(WireResult::not_applied(
                    ErrorCode::LeaseUnavailable,
                ));
            }
            if matches!(
                &lease.submit_attempt,
                Some(SubmitAttempt::Resolved {
                    result: WireResult::Unknown,
                    fence: Some(_),
                })
            ) {
                return CommandExecution::Complete(WireResult::not_applied(
                    ErrorCode::ExpectedMismatch,
                ));
            }
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
            ExternalLeaseState::SubmissionPending { .. } => unreachable!("handled above"),
            ExternalLeaseState::SubmittedIntact { receipt } => {
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
                let expected_client_user_message_id =
                    format!("koenig-composer-{}", receipt.submission_id);
                let Some(payload) = mechanical_correction_context(
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
                    stored.state = ExternalLeaseState::CorrectionPending {
                        receipt,
                        correction_id,
                        expected,
                        replacement,
                        payload: payload.clone(),
                    };
                }
                CommandExecution::Correct(PreparedCorrection {
                    lease_id,
                    origin: CorrectionOrigin::Composer,
                    thread_id: lease.thread_id,
                    correction_id,
                    expected_client_user_message_id,
                    payload,
                })
            }
            ExternalLeaseState::SubmissionAbandoned { .. } => {
                CommandExecution::Complete(WireResult::SubmissionAbandoned)
            }
            ExternalLeaseState::CorrectionPending {
                receipt,
                correction_id,
                expected: pending_expected,
                replacement: pending_replacement,
                payload,
                ..
            } => {
                if expected != pending_expected || replacement != pending_replacement {
                    return CommandExecution::Complete(WireResult::not_applied(
                        ErrorCode::LeaseUnavailable,
                    ));
                }
                CommandExecution::Correct(PreparedCorrection {
                    lease_id,
                    origin: CorrectionOrigin::Composer,
                    thread_id: lease.thread_id,
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

    fn retire_excess_leases<T>(&mut self, target: &mut T, snapshot: &ComposerSnapshot) -> bool
    where
        T: ComposerControlTarget<Lease = L>,
    {
        while self.leases.len() >= MAX_LEASE_COUNT {
            let Some(index) = self.lease_order.iter().position(|lease_id| {
                self.leases
                    .get(lease_id)
                    .is_some_and(|lease| match &lease.state {
                        ExternalLeaseState::Draft { .. } => {
                            lease.thread_id == snapshot.thread_id
                                && !has_unresolved_draft_provenance(lease)
                                && !has_live_submit_fence(lease)
                        }
                        ExternalLeaseState::SubmittedIntact { .. }
                        | ExternalLeaseState::SubmissionAbandoned { .. } => {
                            !has_live_submit_fence(lease)
                        }
                        ExternalLeaseState::SubmissionPending { .. }
                        | ExternalLeaseState::CorrectionPending { .. } => false,
                    })
            }) else {
                return false;
            };
            let Some(oldest_id) = self.lease_order.remove(index) else {
                return false;
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
        true
    }

    fn note_submission_pending_by_native(
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
                lease.state = ExternalLeaseState::SubmissionPending {
                    receipt: SubmittedLeaseReceipt {
                        submission_id,
                        submitted_text: Arc::clone(&submitted_text),
                        range: range.clone(),
                    },
                };
            }
        }
    }

    fn note_submission_committed_id(&mut self, thread_id: &str, submission_id: Uuid) {
        for lease in self.leases.values_mut() {
            if lease.thread_id != thread_id {
                continue;
            }
            let matching_receipt = match &lease.state {
                ExternalLeaseState::SubmissionPending { receipt }
                | ExternalLeaseState::SubmittedIntact { receipt }
                | ExternalLeaseState::CorrectionPending { receipt, .. } => {
                    receipt.submission_id == submission_id
                }
                ExternalLeaseState::Draft { .. }
                | ExternalLeaseState::SubmissionAbandoned { .. } => false,
            };
            if !matching_receipt {
                continue;
            }
            if let ExternalLeaseState::SubmissionPending { receipt } = &lease.state {
                lease.state = ExternalLeaseState::SubmittedIntact {
                    receipt: receipt.clone(),
                };
            }
            relinquish_unknown_submit(lease);
        }
    }

    fn note_submission_abandoned_id(&mut self, thread_id: &str, submission_id: Uuid) {
        for lease in self.leases.values_mut() {
            if lease.thread_id != thread_id {
                continue;
            }
            let ExternalLeaseState::SubmissionPending { receipt } = &lease.state else {
                continue;
            };
            if receipt.submission_id == submission_id {
                lease.state = ExternalLeaseState::SubmissionAbandoned {
                    receipt: receipt.clone(),
                };
                lease.submit_attempt = Some(SubmitAttempt::Resolved {
                    result: WireResult::SubmissionAbandoned,
                    fence: None,
                });
            }
        }
    }

    pub(crate) fn note_thread_rolled_back(
        &mut self,
        thread_id: &str,
        surviving_submission_ids: &HashSet<Uuid>,
    ) {
        self.direct_sends
            .note_thread_rolled_back(thread_id, surviving_submission_ids);
        self.captures
            .retain(|_, capture| capture.thread_id != thread_id);
        self.capture_order
            .retain(|capture_id| self.captures.contains_key(capture_id));
        for lease in self
            .leases
            .values_mut()
            .filter(|lease| lease.thread_id == thread_id)
        {
            let receipt = match &lease.state {
                ExternalLeaseState::SubmissionPending { receipt }
                | ExternalLeaseState::SubmittedIntact { receipt }
                | ExternalLeaseState::SubmissionAbandoned { receipt }
                | ExternalLeaseState::CorrectionPending { receipt, .. } => receipt.clone(),
                ExternalLeaseState::Draft { .. } => continue,
            };
            if surviving_submission_ids.contains(&receipt.submission_id) {
                if matches!(&lease.state, ExternalLeaseState::SubmissionPending { .. }) {
                    lease.state = ExternalLeaseState::SubmittedIntact { receipt };
                }
            } else {
                lease.state = ExternalLeaseState::SubmissionAbandoned { receipt };
                lease.submit_attempt = Some(SubmitAttempt::Resolved {
                    result: WireResult::SubmissionAbandoned,
                    fence: None,
                });
            }
        }
        self.leases.retain(|_, lease| {
            lease.thread_id != thread_id
                || matches!(
                    &lease.state,
                    ExternalLeaseState::SubmittedIntact { .. }
                        | ExternalLeaseState::SubmissionAbandoned { .. }
                        | ExternalLeaseState::CorrectionPending { .. }
                )
        });
        self.lease_order
            .retain(|lease_id| self.leases.contains_key(lease_id));
    }
}

impl NativeComposerControlState {
    pub(crate) fn note_submission_pending(&mut self, submission: &NativeComposerSubmission) {
        self.direct_sends.note_submission_pending(submission);
        let native_leases = submission
            .submitted_leases()
            .iter()
            .map(|lease| (lease.native, lease.range.clone()))
            .collect::<Vec<_>>();
        self.note_submission_pending_by_native(
            submission.thread_id(),
            submission.submission_id(),
            submission.submitted_text_arc(),
            &native_leases,
        );
    }

    pub(crate) fn note_submission_committed(&mut self, submission: &NativeComposerSubmission) {
        self.note_submission_pending(submission);
        self.direct_sends.note_submission_committed(submission);
        self.note_submission_committed_id(submission.thread_id(), submission.submission_id());
    }

    pub(crate) fn note_submission_abandoned(&mut self, submission: &NativeComposerSubmission) {
        self.note_submission_pending(submission);
        self.direct_sends.note_submission_abandoned(submission);
        self.note_submission_abandoned_id(submission.thread_id(), submission.submission_id());
    }
}

/// Rebase owned ranges across deterministic message normalization such as image-placeholder
/// renumbering. Any edit that intersects owned text fails closed, and each rebased range must
/// still contain the exact originally owned bytes.
fn rebase_submission_leases(
    submission: &NativeComposerSubmission,
    remapped_text: &str,
) -> Option<Vec<NativeSubmittedLease>> {
    if submission.submitted_text.as_ref() == remapped_text {
        return Some(submission.leases.clone());
    }

    #[derive(Clone)]
    struct Edit {
        old: Range<usize>,
        new_len: usize,
    }

    let diff = TextDiff::from_chars(submission.submitted_text.as_ref(), remapped_text);
    let old_slices = diff.old_slices();
    let new_slices = diff.new_slices();
    let old_prefix = old_slices
        .iter()
        .scan(0usize, |offset, slice| {
            let current = *offset;
            *offset += slice.len();
            Some(current)
        })
        .chain(std::iter::once(submission.submitted_text.len()))
        .collect::<Vec<_>>();
    let new_prefix = new_slices
        .iter()
        .scan(0usize, |offset, slice| {
            let current = *offset;
            *offset += slice.len();
            Some(current)
        })
        .chain(std::iter::once(remapped_text.len()))
        .collect::<Vec<_>>();
    let edits = diff
        .ops()
        .iter()
        .filter(|operation| operation.tag() != DiffTag::Equal)
        .map(|operation| {
            let old = operation.old_range();
            let new = operation.new_range();
            Edit {
                old: old_prefix[old.start]..old_prefix[old.end],
                new_len: new_prefix[new.end] - new_prefix[new.start],
            }
        })
        .collect::<Vec<_>>();

    submission
        .leases
        .iter()
        .map(|lease| {
            let mut shift = 0isize;
            for edit in &edits {
                if edit.old.end <= lease.range.start {
                    shift += edit.new_len as isize
                        - (edit.old.end.saturating_sub(edit.old.start)) as isize;
                } else if edit.old.start < lease.range.end {
                    return None;
                }
            }
            let range = lease.range.start.checked_add_signed(shift)?
                ..lease.range.end.checked_add_signed(shift)?;
            let expected = submission.submitted_text.get(lease.range.clone())?;
            (remapped_text.get(range.clone()) == Some(expected)).then_some(NativeSubmittedLease {
                native: lease.native,
                range,
            })
        })
        .collect()
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
        let Some(old_slice_start) = group.first().map(|operation| operation.old_range().start)
        else {
            continue;
        };
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
         {}..{}. The model-visible Application receipt immediately preceding that user message \
         identifies the target; the byte coordinates and exact old text identify its owned \
         range. Do not alter text outside that owned range.\nTreat only the exact \
         replacements below as corrections to the prior message. The receiving agent may discern \
         their semantic impact; Koenig asserts no edit beyond the enumerated mechanical \
         replacements.",
        lease_range.start, lease_range.end
    );

    for (index, (old_start, old, new)) in hunks.into_iter().enumerate() {
        let local_old_end = old_start + old.len();
        let submitted_old_start = lease_range.start + old_start;
        let submitted_old_end = lease_range.start + local_old_end;
        let occurrence = if old.is_empty() {
            0
        } else {
            submitted_text
                .match_indices(&old)
                .take_while(|(position, _)| *position <= submitted_old_start)
                .count()
        };
        let old_char_count = old.chars().count();
        let old_rendered = compact_old_snippet(&old, OLD_SNIPPET_MAX_CHARS);
        let Ok(old_rendered) = serde_json::to_string(&old_rendered) else {
            return None;
        };
        let Ok(new) = serde_json::to_string(&new) else {
            return None;
        };
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
                occurrence = occurrence,
            ));
        } else {
            rendered.push_str(&format!(
                "\n\nHunk {}: old bytes {old_start}..{old_end}; occurrence {occurrence} of this \
                 old range in the submitted message. The old rendering elides its middle; the \
                 byte range plus both anchors is authoritative.\n- {old_rendered}\n+ {new}",
                index + 1,
                old_start = submitted_old_start,
                old_end = submitted_old_end,
                occurrence = occurrence,
            ));
        }
        if truncate_middle_with_token_budget(&rendered, MAX_ADDITIONAL_CONTEXT_VALUE_TOKENS)
            .1
            .is_some()
        {
            return None;
        }
    }

    truncate_middle_with_token_budget(&rendered, MAX_ADDITIONAL_CONTEXT_VALUE_TOKENS)
        .1
        .is_none()
        .then_some(rendered)
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
    AcquireSend {
        #[serde(rename = "protocolVersion")]
        protocol_version: u8,
        #[serde(rename = "instanceId")]
        instance_id: Uuid,
    },
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
    Submit {
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
    AcknowledgeUnknown {
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
            Self::AcquireSend {
                protocol_version,
                instance_id,
            } => (protocol_version, instance_id, ComposerCommand::AcquireSend),
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
            Self::Submit {
                protocol_version,
                instance_id,
                lease_id,
                expected,
            } => {
                validate_inserted_text(&expected, /*allow_empty*/ false)?;
                (
                    protocol_version,
                    instance_id,
                    ComposerCommand::Submit { lease_id, expected },
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
            Self::AcknowledgeUnknown {
                protocol_version,
                instance_id,
                lease_id,
            } => (
                protocol_version,
                instance_id,
                ComposerCommand::AcknowledgeUnknown { lease_id },
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorCode {
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
    SubmissionUnavailable,
    ReservationChanged,
    UiUnavailable,
    UiTimeout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MutationOutcome {
    NotApplied,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum WireResult {
    Reserved {
        #[serde(rename = "leaseId")]
        lease_id: Uuid,
    },
    Captured {
        #[serde(rename = "captureId")]
        capture_id: Uuid,
    },
    Inserted {
        #[serde(rename = "leaseId")]
        lease_id: Uuid,
    },
    Verified,
    /// App-server accepted the exact turn or steer. The matching user-message item has not yet
    /// committed to model history.
    SendAccepted,
    /// The matching user message has not committed to model history yet. No lease state or text
    /// was changed; retry the same operation with the same lease after the commit transition.
    SubmissionPending,
    /// The dedicated correction commit may have reached durable model history, but its
    /// acknowledgement was ambiguous. Retry the exact same replacement with this lease; the
    /// retained correction identity makes that retry idempotent.
    CorrectionPending,
    /// The correction commit was accepted for the originating thread. This does not claim that
    /// the model has sampled or semantically applied the correction.
    CorrectionQueued,
    SubmittedIntact,
    SubmissionAbandoned,
    /// Dispatch may have reached app-server. Submit is never retried under this lease.
    Unknown,
    /// The producer explicitly accepted an unknowable one-shot dispatch outcome and relinquished
    /// local ownership. This is not cancellation; app-server may still commit the original send.
    UnknownAcknowledged,
    Kept,
    Replaced,
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
        let Ok((stream, _address)) = listener.accept() else {
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
        enqueue_connection(stream, instance_id, &request_tx);
    }
}

#[cfg(unix)]
fn enqueue_connection(
    mut stream: std::os::unix::net::UnixStream,
    instance_id: Uuid,
    request_tx: &UnboundedSender<ComposerControlRequest>,
) {
    let request = match read_wire_request(&stream) {
        Ok(request) => request,
        Err(code) => {
            write_response(&mut stream, instance_id, WireResult::not_applied(code));
            return;
        }
    };
    let command = match request.into_command(instance_id) {
        Ok(command) => command,
        Err(code) => {
            write_response(&mut stream, instance_id, WireResult::not_applied(code));
            return;
        }
    };
    let may_have_effect = command.may_have_effect();

    let (reply, reply_rx) = std::sync::mpsc::sync_channel(1);
    let ui_request = ComposerControlRequest {
        command,
        deadline: Instant::now() + UI_REQUEST_DEADLINE,
        reply,
    };
    if request_tx.send(ui_request).is_err() {
        write_response(
            &mut stream,
            instance_id,
            WireResult::not_applied(ErrorCode::UiUnavailable),
        );
        return;
    }
    std::thread::spawn(move || {
        let result = match reply_rx.recv_timeout(UI_REPLY_TIMEOUT) {
            Ok(result) => result,
            Err(_) => ui_reply_timeout_result(may_have_effect),
        };
        write_response(&mut stream, instance_id, result);
    });
}

#[cfg(unix)]
fn ui_reply_timeout_result(may_have_effect: bool) -> WireResult {
    if may_have_effect {
        WireResult::error(ErrorCode::UiTimeout, MutationOutcome::Unknown)
    } else {
        WireResult::not_applied(ErrorCode::UiTimeout)
    }
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

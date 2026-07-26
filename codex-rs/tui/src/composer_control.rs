//! Native, content-opaque control plane for the active composer.
//!
//! The listener never uses [`crate::app_event::AppEvent`]: requests enter the main UI loop through
//! a dedicated channel, so transcript-bearing payloads are neither session-logged nor formatted
//! for diagnostics. All capture checks and mutations therefore share the same serialized order as
//! terminal input.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::mpsc::SyncSender;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::unbounded_channel;
use uuid::Uuid;

use crate::bottom_pane::ComposerLeaseError;
use crate::bottom_pane::ComposerLeaseId;
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

struct ExternalLease<L> {
    native: L,
    thread_id: String,
    expected_hash: [u8; 32],
}

impl<L: Copy> Clone for ExternalLease<L> {
    fn clone(&self) -> Self {
        Self {
            native: self.native,
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

impl<L: Copy> ComposerControlState<L> {
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
    ) where
        T: ComposerControlTarget<Lease = L>,
    {
        let ComposerControlRequest {
            command,
            deadline,
            reply,
        } = request;
        let result = if Instant::now() > deadline {
            WireResult::error(ErrorCode::UiTimeout, MutationOutcome::NotApplied)
        } else {
            self.execute(command, target, app_overlay_active)
        };
        let _ = reply.send(result);
    }

    fn execute<T>(
        &mut self,
        command: ComposerCommand,
        target: &mut T,
        app_overlay_active: bool,
    ) -> WireResult
    where
        T: ComposerControlTarget<Lease = L>,
    {
        match command {
            ComposerCommand::Capture => self.capture(target, app_overlay_active),
            ComposerCommand::Insert { capture_id, text } => {
                self.insert(target, app_overlay_active, capture_id, text)
            }
            ComposerCommand::Verify { lease_id, expected } => {
                self.verify(target, app_overlay_active, lease_id, expected)
            }
            ComposerCommand::Keep { lease_id } => self.keep(target, app_overlay_active, lease_id),
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
        let lease_id = fresh_id(&self.leases);
        self.leases.insert(
            lease_id,
            ExternalLease {
                native,
                thread_id: snapshot.thread_id,
                expected_hash,
            },
        );
        self.lease_order.push_back(lease_id);
        WireResult::Inserted { lease_id }
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
        let Some(snapshot) = available_snapshot(target, app_overlay_active) else {
            return WireResult::not_applied(ErrorCode::ComposerUnavailable);
        };
        if lease.thread_id != snapshot.thread_id {
            return WireResult::not_applied(ErrorCode::ComposerUnavailable);
        }
        match target.verify_owned_text(lease.native, &expected) {
            Ok(()) => WireResult::Verified,
            Err(err) => WireResult::not_applied(map_lease_error(err)),
        }
    }

    fn keep<T>(&mut self, target: &mut T, app_overlay_active: bool, lease_id: Uuid) -> WireResult
    where
        T: ComposerControlTarget<Lease = L>,
    {
        let Some(lease) = self.leases.get(&lease_id).cloned() else {
            return WireResult::not_applied(ErrorCode::LeaseUnavailable);
        };
        let Some(snapshot) = available_snapshot(target, app_overlay_active) else {
            return WireResult::not_applied(ErrorCode::ComposerUnavailable);
        };
        if lease.thread_id != snapshot.thread_id {
            return WireResult::not_applied(ErrorCode::ComposerUnavailable);
        }
        self.leases.remove(&lease_id);
        self.lease_order.retain(|queued| *queued != lease_id);
        match target.keep_owned_text(lease.native) {
            Ok(()) => WireResult::Kept,
            Err(err) => WireResult::not_applied(map_lease_error(err)),
        }
    }

    fn replace<T>(
        &mut self,
        target: &mut T,
        app_overlay_active: bool,
        lease_id: Uuid,
        expected: String,
        replacement: String,
    ) -> WireResult
    where
        T: ComposerControlTarget<Lease = L>,
    {
        let Some(lease) = self.leases.get(&lease_id).cloned() else {
            return WireResult::not_applied(ErrorCode::LeaseUnavailable);
        };
        if lease.expected_hash != text_hash(&expected) {
            self.leases.remove(&lease_id);
            self.lease_order.retain(|queued| *queued != lease_id);
            if target.thread_id().as_deref() == Some(lease.thread_id.as_str()) {
                let _ = target.keep_owned_text(lease.native);
            }
            return WireResult::not_applied(ErrorCode::ExpectedMismatch);
        }
        let Some(snapshot) = available_snapshot(target, app_overlay_active) else {
            return WireResult::not_applied(ErrorCode::ComposerUnavailable);
        };
        if lease.thread_id != snapshot.thread_id {
            return WireResult::not_applied(ErrorCode::ComposerUnavailable);
        }
        self.leases.remove(&lease_id);
        self.lease_order.retain(|queued| *queued != lease_id);
        match target.replace_owned_text(lease.native, &expected, &replacement) {
            Ok(()) => WireResult::Replaced,
            Err(err) => WireResult::not_applied(map_lease_error(err)),
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
            if oldest.thread_id == snapshot.thread_id {
                let _ = target.keep_owned_text(oldest.native);
            }
        }
    }
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

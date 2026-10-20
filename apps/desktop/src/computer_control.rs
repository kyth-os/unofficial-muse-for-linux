//! Pure policy and approval broker for the future Computer Control page.
//!
//! This module intentionally has no GTK, D-Bus, filesystem, browser, or
//! network I/O. It validates and queues immutable desktop actions, binds them
//! to a fresh approval token, and returns an exact action only after approval.
//! A caller owns the actual device session and must stop that session before
//! revoking this broker's authority.

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::io::Read;
use std::rc::Rc;
use std::time::Instant;

/// Maximum number of requests retained for approval at once.
pub const MAX_PENDING_REQUESTS: usize = 32;
/// Request IDs are opaque 128-bit values.
pub const REQUEST_ID_BYTES: usize = 16;
/// Approval tokens are independently generated opaque 256-bit values.
pub const APPROVAL_TOKEN_BYTES: usize = 32;
/// Maximum monotonic lifetime of a request.
pub const MAX_REQUEST_LIFETIME_MS: u64 = 120_000;
/// Screenshot requests are bounded before they enter the queue.
pub const MAX_SCREENSHOT_BYTES: u32 = 8 * 1024 * 1024;
/// Relative motion and smooth scroll bounds, in device units.
pub const MAX_CONTINUOUS_DELTA: f64 = 20_000.0;
/// Discrete scroll bound, in wheel steps per axis.
pub const MAX_DISCRETE_SCROLL: i32 = 100;
/// Linux evdev's key/button code ceiling.
pub const EVDEV_CODE_MAX: u16 = 0x2ff;
/// Linux evdev's button-code floor (`BTN_MISC`).
pub const EVDEV_BUTTON_MIN: u16 = 0x100;
/// X11/XKB keysyms are 29-bit values.
pub const KEYSYM_MAX: u32 = 0x1fff_ffff;

/// The only desktop actions this broker can approve.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Screenshot { max_bytes: u32 },
    RelativePointerMotion { dx: f64, dy: f64 },
    PointerButton { button: u16, state: u8 },
    SmoothScroll { dx: f64, dy: f64 },
    DiscreteScroll { dx: i32, dy: i32 },
    KeyboardKeysym { keysym: u32, state: u8 },
    KeyboardKeycode { keycode: u16, state: u8 },
}

/// Actions intentionally unavailable to this first policy implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnavailableAction {
    TextInjection,
    Clipboard,
    Shell,
    AppLaunch,
    WindowClose,
    BrowserDom,
    PasswordOrPaymentField,
    Macro,
}

/// A blocked policy request is explicit rather than silently ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyOutcome {
    Blocked(UnavailableAction),
}

pub fn blocked(action: UnavailableAction) -> PolicyOutcome {
    PolicyOutcome::Blocked(action)
}

/// Session authority is explicit and never inferred from queue contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Disabled,
    Starting,
    Active,
    Stopping,
    Stopped,
}

/// Typed validation failures for caller-supplied actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionValidationError {
    NonFinite,
    OutOfBounds,
    InvalidButton,
    InvalidState,
    InvalidKeycode,
    InvalidKeysym,
    InvalidScreenshotSize,
    Unsupported,
}

/// Safe metadata available before approval. Exact action parameters remain in
/// the broker and are returned only in [`ApprovedAction`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionMetadata {
    pub name: ActionName,
    pub read_only: bool,
    pub requires_fresh_approval: bool,
}

/// Allow-listed action names used in activity output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionName {
    Screenshot,
    RelativePointerMotion,
    PointerButton,
    SmoothScroll,
    DiscreteScroll,
    KeyboardKeysym,
    KeyboardKeycode,
}

impl fmt::Display for ActionName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Screenshot => "screenshot",
            Self::RelativePointerMotion => "pointer-motion",
            Self::PointerButton => "pointer-button",
            Self::SmoothScroll => "smooth-scroll",
            Self::DiscreteScroll => "discrete-scroll",
            Self::KeyboardKeysym => "keyboard-keysym",
            Self::KeyboardKeycode => "keyboard-keycode",
        };
        formatter.write_str(name)
    }
}

/// Validate all action parameters before any request is queued.
pub fn validate_action(action: &Action) -> Result<ActionMetadata, ActionValidationError> {
    let metadata = match action {
        Action::Screenshot { max_bytes } => {
            if *max_bytes == 0 || *max_bytes > MAX_SCREENSHOT_BYTES {
                return Err(ActionValidationError::InvalidScreenshotSize);
            }
            ActionMetadata {
                name: ActionName::Screenshot,
                read_only: true,
                requires_fresh_approval: true,
            }
        }
        Action::RelativePointerMotion { dx, dy } => {
            validate_continuous(*dx, *dy)?;
            ActionMetadata {
                name: ActionName::RelativePointerMotion,
                read_only: false,
                requires_fresh_approval: true,
            }
        }
        Action::PointerButton { button, state } => {
            if *button < EVDEV_BUTTON_MIN || *button > EVDEV_CODE_MAX {
                return Err(ActionValidationError::InvalidButton);
            }
            validate_state(*state)?;
            ActionMetadata {
                name: ActionName::PointerButton,
                read_only: false,
                requires_fresh_approval: true,
            }
        }
        Action::SmoothScroll { dx, dy } => {
            validate_continuous(*dx, *dy)?;
            ActionMetadata {
                name: ActionName::SmoothScroll,
                read_only: false,
                requires_fresh_approval: true,
            }
        }
        Action::DiscreteScroll { dx, dy } => {
            if dx.unsigned_abs() > MAX_DISCRETE_SCROLL.unsigned_abs()
                || dy.unsigned_abs() > MAX_DISCRETE_SCROLL.unsigned_abs()
            {
                return Err(ActionValidationError::OutOfBounds);
            }
            ActionMetadata {
                name: ActionName::DiscreteScroll,
                read_only: false,
                requires_fresh_approval: true,
            }
        }
        Action::KeyboardKeysym { keysym, state } => {
            if *keysym > KEYSYM_MAX {
                return Err(ActionValidationError::InvalidKeysym);
            }
            validate_state(*state)?;
            ActionMetadata {
                name: ActionName::KeyboardKeysym,
                read_only: false,
                requires_fresh_approval: true,
            }
        }
        Action::KeyboardKeycode { keycode, state } => {
            if *keycode > EVDEV_CODE_MAX {
                return Err(ActionValidationError::InvalidKeycode);
            }
            validate_state(*state)?;
            ActionMetadata {
                name: ActionName::KeyboardKeycode,
                read_only: false,
                requires_fresh_approval: true,
            }
        }
    };
    Ok(metadata)
}

fn validate_continuous(dx: f64, dy: f64) -> Result<(), ActionValidationError> {
    if !dx.is_finite() || !dy.is_finite() {
        return Err(ActionValidationError::NonFinite);
    }
    if dx.abs() > MAX_CONTINUOUS_DELTA || dy.abs() > MAX_CONTINUOUS_DELTA {
        return Err(ActionValidationError::OutOfBounds);
    }
    Ok(())
}

fn validate_state(state: u8) -> Result<(), ActionValidationError> {
    if state > 1 {
        Err(ActionValidationError::InvalidState)
    } else {
        Ok(())
    }
}

/// Randomness source used for request IDs and approval tokens. Production
/// uses two independent sources; tests may inject deterministic sources.
pub trait RandomBytes {
    fn fill_bytes(&mut self, destination: &mut [u8]) -> Result<(), RandomError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RandomError {
    Unavailable,
    InvalidLength,
}

/// A monotonic time source in milliseconds. It performs no wall-clock I/O.
pub trait MonotonicClock {
    fn now_ms(&self) -> u64;
}

/// Deterministic clock useful for policy tests and caller-owned simulations.
#[derive(Clone)]
pub struct ManualClock {
    now: Rc<Cell<u64>>,
}

impl ManualClock {
    pub fn new(now_ms: u64) -> Self {
        Self {
            now: Rc::new(Cell::new(now_ms)),
        }
    }

    pub fn advance_ms(&self, amount: u64) {
        self.now.set(self.now.get().saturating_add(amount));
    }
}

impl MonotonicClock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }
}

struct SystemMonotonicClock {
    origin: Instant,
}

impl SystemMonotonicClock {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl MonotonicClock for SystemMonotonicClock {
    fn now_ms(&self) -> u64 {
        self.origin
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }
}

/// Safe opaque request identifier. Its bytes are never included in activity
/// text, and its debug representation does not reveal them.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId([u8; REQUEST_ID_BYTES]);

impl fmt::Debug for RequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RequestId(..)")
    }
}

/// Single-use capability token. Display and debug are deliberately redacted.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ApprovalToken([u8; APPROVAL_TOKEN_BYTES]);

impl fmt::Debug for ApprovalToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApprovalToken(<redacted>)")
    }
}

impl fmt::Display for ApprovalToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RandomIdKind {
    Request,
    Approval,
}

/// The receipt intentionally exposes no action parameters.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RequestReceipt {
    pub request_id: RequestId,
    pub approval_token: ApprovalToken,
    pub deadline_ms: u64,
    pub epoch: u64,
    pub metadata: ActionMetadata,
}

impl fmt::Debug for RequestReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RequestReceipt")
            .field("request_id", &self.request_id)
            .field("approval_token", &"<redacted>")
            .field("deadline_ms", &self.deadline_ms)
            .field("epoch", &self.epoch)
            .field("metadata", &self.metadata)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApprovedAction {
    pub request_id: RequestId,
    pub action: Action,
}

impl ApprovedAction {
    pub fn activity_event(&self) -> ActivityEvent {
        ActivityEvent::allowed(&self.action)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityOutcome {
    Allowed,
    Denied,
    Cancelled,
    Expired,
    Revoked,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ActionSummary {
    Screenshot { max_bytes: u32 },
    RelativePointerMotion { dx: f64, dy: f64 },
    PointerButton { button: u16, state: u8 },
    SmoothScroll { dx: f64, dy: f64 },
    DiscreteScroll { dx: i32, dy: i32 },
    KeyboardKeysym { keysym: u32, state: u8 },
    KeyboardKeycode { keycode: u16, state: u8 },
}

/// Redacted, allow-listed activity event. It contains only action names and
/// bounded scalar summaries; no tokens, paths, bytes, page text, or secrets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActivityEvent {
    pub action: ActionName,
    pub outcome: ActivityOutcome,
    summary: ActionSummary,
}

impl ActivityEvent {
    fn allowed(action: &Action) -> Self {
        Self {
            action: action_name(action),
            outcome: ActivityOutcome::Allowed,
            summary: action_summary(action),
        }
    }
}

impl fmt::Display for ActivityEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.outcome {
            ActivityOutcome::Allowed => "allowed ",
            ActivityOutcome::Denied => "denied ",
            ActivityOutcome::Cancelled => "cancelled ",
            ActivityOutcome::Expired => "expired ",
            ActivityOutcome::Revoked => "revoked ",
            ActivityOutcome::Stopped => "stopped ",
        })?;
        write!(formatter, "{} ", self.action)?;
        match self.summary {
            ActionSummary::Screenshot { max_bytes } => {
                write!(formatter, "{max_bytes} bytes max")
            }
            ActionSummary::RelativePointerMotion { dx, dy } => {
                write!(formatter, "dx={dx:.3} dy={dy:.3}")
            }
            ActionSummary::PointerButton { button, state } => {
                write!(formatter, "button={button} state={state}")
            }
            ActionSummary::SmoothScroll { dx, dy } => {
                write!(formatter, "dx={dx:.3} dy={dy:.3}")
            }
            ActionSummary::DiscreteScroll { dx, dy } => {
                write!(formatter, "dx={dx} dy={dy}")
            }
            ActionSummary::KeyboardKeysym { keysym, state } => {
                write!(formatter, "keysym={keysym} state={state}")
            }
            ActionSummary::KeyboardKeycode { keycode, state } => {
                write!(formatter, "keycode={keycode} state={state}")
            }
        }
    }
}

fn action_name(action: &Action) -> ActionName {
    match action {
        Action::Screenshot { .. } => ActionName::Screenshot,
        Action::RelativePointerMotion { .. } => ActionName::RelativePointerMotion,
        Action::PointerButton { .. } => ActionName::PointerButton,
        Action::SmoothScroll { .. } => ActionName::SmoothScroll,
        Action::DiscreteScroll { .. } => ActionName::DiscreteScroll,
        Action::KeyboardKeysym { .. } => ActionName::KeyboardKeysym,
        Action::KeyboardKeycode { .. } => ActionName::KeyboardKeycode,
    }
}

fn action_summary(action: &Action) -> ActionSummary {
    match action {
        Action::Screenshot { max_bytes } => ActionSummary::Screenshot {
            max_bytes: *max_bytes,
        },
        Action::RelativePointerMotion { dx, dy } => {
            ActionSummary::RelativePointerMotion { dx: *dx, dy: *dy }
        }
        Action::PointerButton { button, state } => ActionSummary::PointerButton {
            button: *button,
            state: *state,
        },
        Action::SmoothScroll { dx, dy } => ActionSummary::SmoothScroll { dx: *dx, dy: *dy },
        Action::DiscreteScroll { dx, dy } => ActionSummary::DiscreteScroll { dx: *dx, dy: *dy },
        Action::KeyboardKeysym { keysym, state } => ActionSummary::KeyboardKeysym {
            keysym: *keysym,
            state: *state,
        },
        Action::KeyboardKeycode { keycode, state } => ActionSummary::KeyboardKeycode {
            keycode: *keycode,
            state: *state,
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitError {
    Inactive(SessionState),
    Capacity,
    DuplicateRequestId,
    DuplicateApprovalToken,
    InvalidAction(ActionValidationError),
    Randomness(RandomIdKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalError {
    UnknownRequest,
    InvalidApproval,
    AlreadyConsumed,
    Expired,
    StaleEpoch,
    Stopped,
    Inactive(SessionState),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecisionOutcome {
    Denied,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingFailureReason {
    Revoked,
    Stopped,
    Expired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingFailure {
    pub request_id: RequestId,
    pub reason: PendingFailureReason,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StopReport {
    pub state: SessionState,
    pub failures: Vec<PendingFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevokeReport {
    pub state: SessionState,
    pub failures: Vec<PendingFailure>,
}

#[derive(Clone)]
struct PendingRecord {
    action: Action,
    token: ApprovalToken,
    deadline_ms: u64,
    epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecordStatus {
    Pending { epoch: u64 },
    Consumed,
    Denied,
    Cancelled,
    Expired,
    Revoked,
    Stopped,
}

/// Pure in-memory action policy and approval broker.
pub struct Broker {
    state: SessionState,
    epoch: u64,
    id_source: Box<dyn RandomBytes>,
    token_source: Box<dyn RandomBytes>,
    clock: Box<dyn MonotonicClock>,
    pending: HashMap<RequestId, PendingRecord>,
    order: VecDeque<RequestId>,
    history: HashMap<RequestId, RecordStatus>,
    seen_ids: HashSet<RequestId>,
    seen_tokens: HashSet<ApprovalToken>,
}

impl Broker {
    pub fn new() -> Self {
        Self::with_dependencies(
            Box::new(OsRandom::new()),
            Box::new(OsRandom::new()),
            Box::new(SystemMonotonicClock::new()),
        )
    }

    pub fn with_dependencies(
        id_source: Box<dyn RandomBytes>,
        token_source: Box<dyn RandomBytes>,
        clock: Box<dyn MonotonicClock>,
    ) -> Self {
        Self {
            state: SessionState::Disabled,
            epoch: 0,
            id_source,
            token_source,
            clock,
            pending: HashMap::new(),
            order: VecDeque::new(),
            history: HashMap::new(),
            seen_ids: HashSet::new(),
            seen_tokens: HashSet::new(),
        }
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn start(&mut self) -> Result<SessionState, SessionState> {
        match self.state {
            SessionState::Disabled | SessionState::Stopped => {
                self.epoch = self.epoch.wrapping_add(1);
                self.state = SessionState::Starting;
                Ok(self.state)
            }
            state => Err(state),
        }
    }

    pub fn activate(&mut self) -> Result<SessionState, SessionState> {
        if self.state == SessionState::Starting {
            self.state = SessionState::Active;
            Ok(self.state)
        } else {
            Err(self.state)
        }
    }

    /// Stop transitions through `Stopping`, fails all pending requests in
    /// FIFO order, and finishes in `Stopped`.
    pub fn stop(&mut self) -> StopReport {
        self.state = SessionState::Stopping;
        let failures = self.fail_all(PendingFailureReason::Stopped);
        self.state = SessionState::Stopped;
        StopReport {
            state: self.state,
            failures,
        }
    }

    /// Revoke clears pending authority and advances the epoch. The caller
    /// must stop any external device session before invoking this method.
    pub fn revoke(&mut self) -> RevokeReport {
        self.epoch = self.epoch.wrapping_add(1);
        let failures = self.fail_all(PendingFailureReason::Revoked);
        self.state = SessionState::Disabled;
        RevokeReport {
            state: self.state,
            failures,
        }
    }

    pub fn submit(&mut self, action: Action) -> Result<RequestReceipt, SubmitError> {
        if self.state != SessionState::Active {
            return Err(SubmitError::Inactive(self.state));
        }
        if self.pending.len() >= MAX_PENDING_REQUESTS {
            return Err(SubmitError::Capacity);
        }
        let metadata = validate_action(&action).map_err(SubmitError::InvalidAction)?;
        let mut id_bytes = [0u8; REQUEST_ID_BYTES];
        self.id_source
            .fill_bytes(&mut id_bytes)
            .map_err(|_| SubmitError::Randomness(RandomIdKind::Request))?;
        let request_id = RequestId(id_bytes);
        if self.seen_ids.contains(&request_id) {
            return Err(SubmitError::DuplicateRequestId);
        }
        let mut token_bytes = [0u8; APPROVAL_TOKEN_BYTES];
        self.token_source
            .fill_bytes(&mut token_bytes)
            .map_err(|_| SubmitError::Randomness(RandomIdKind::Approval))?;
        let approval_token = ApprovalToken(token_bytes);
        if self.seen_tokens.contains(&approval_token) {
            return Err(SubmitError::DuplicateApprovalToken);
        }
        let deadline_ms = self.clock.now_ms().saturating_add(MAX_REQUEST_LIFETIME_MS);
        let epoch = self.epoch;
        self.pending.insert(
            request_id,
            PendingRecord {
                action,
                token: approval_token,
                deadline_ms,
                epoch,
            },
        );
        self.order.push_back(request_id);
        self.history
            .insert(request_id, RecordStatus::Pending { epoch });
        self.seen_ids.insert(request_id);
        self.seen_tokens.insert(approval_token);
        Ok(RequestReceipt {
            request_id,
            approval_token,
            deadline_ms,
            epoch,
            metadata,
        })
    }

    pub fn allow_once(
        &mut self,
        request_id: &RequestId,
        token: &ApprovalToken,
    ) -> Result<ApprovedAction, ApprovalError> {
        let record = self.take_for_decision(request_id, token)?;
        let action = record.action;
        self.history.insert(*request_id, RecordStatus::Consumed);
        Ok(ApprovedAction {
            request_id: *request_id,
            action,
        })
    }

    pub fn deny(
        &mut self,
        request_id: &RequestId,
        token: &ApprovalToken,
    ) -> Result<DecisionOutcome, ApprovalError> {
        let _ = self.take_for_decision(request_id, token)?;
        self.history.insert(*request_id, RecordStatus::Denied);
        Ok(DecisionOutcome::Denied)
    }

    pub fn cancel(
        &mut self,
        request_id: &RequestId,
        token: &ApprovalToken,
    ) -> Result<DecisionOutcome, ApprovalError> {
        let _ = self.take_for_decision(request_id, token)?;
        self.history.insert(*request_id, RecordStatus::Cancelled);
        Ok(DecisionOutcome::Cancelled)
    }

    /// Expire all requests whose monotonic deadline has passed, in FIFO order.
    pub fn expire_pending(&mut self) -> Vec<PendingFailure> {
        let now = self.clock.now_ms();
        let ids: Vec<RequestId> = self
            .order
            .iter()
            .copied()
            .filter(|id| {
                self.pending
                    .get(id)
                    .is_some_and(|record| record.deadline_ms <= now)
            })
            .collect();
        ids.into_iter()
            .filter_map(|request_id| self.remove_pending(request_id, PendingFailureReason::Expired))
            .collect()
    }

    fn take_for_decision(
        &mut self,
        request_id: &RequestId,
        token: &ApprovalToken,
    ) -> Result<PendingRecord, ApprovalError> {
        if self.state == SessionState::Stopped || self.state == SessionState::Stopping {
            return Err(ApprovalError::Stopped);
        }
        if self.state != SessionState::Active {
            return Err(self.error_for_inactive_request(request_id));
        }
        let Some(record) = self.pending.get(request_id).cloned() else {
            return Err(self.error_for_missing_request(request_id));
        };
        if record.epoch != self.epoch {
            return Err(ApprovalError::StaleEpoch);
        }
        if record.token != *token {
            return Err(ApprovalError::InvalidApproval);
        }
        if record.deadline_ms <= self.clock.now_ms() {
            let _ = self.remove_pending(*request_id, PendingFailureReason::Expired);
            return Err(ApprovalError::Expired);
        }
        self.remove_pending_without_history(*request_id);
        Ok(record)
    }

    fn error_for_inactive_request(&self, request_id: &RequestId) -> ApprovalError {
        match self.history.get(request_id) {
            Some(RecordStatus::Revoked) => ApprovalError::StaleEpoch,
            Some(RecordStatus::Stopped) => ApprovalError::Stopped,
            Some(RecordStatus::Consumed)
            | Some(RecordStatus::Denied)
            | Some(RecordStatus::Cancelled) => ApprovalError::AlreadyConsumed,
            Some(RecordStatus::Expired) => ApprovalError::Expired,
            Some(RecordStatus::Pending { epoch }) if *epoch != self.epoch => {
                ApprovalError::StaleEpoch
            }
            _ => ApprovalError::Inactive(self.state),
        }
    }

    fn error_for_missing_request(&self, request_id: &RequestId) -> ApprovalError {
        match self.history.get(request_id) {
            Some(RecordStatus::Revoked) => ApprovalError::StaleEpoch,
            Some(RecordStatus::Stopped) => ApprovalError::Stopped,
            Some(RecordStatus::Consumed)
            | Some(RecordStatus::Denied)
            | Some(RecordStatus::Cancelled) => ApprovalError::AlreadyConsumed,
            Some(RecordStatus::Expired) => ApprovalError::Expired,
            Some(RecordStatus::Pending { epoch }) if *epoch != self.epoch => {
                ApprovalError::StaleEpoch
            }
            _ => ApprovalError::UnknownRequest,
        }
    }

    fn remove_pending_without_history(&mut self, request_id: RequestId) {
        self.pending.remove(&request_id);
        if let Some(position) = self.order.iter().position(|id| *id == request_id) {
            self.order.remove(position);
        }
    }

    fn remove_pending(
        &mut self,
        request_id: RequestId,
        reason: PendingFailureReason,
    ) -> Option<PendingFailure> {
        if self.pending.remove(&request_id).is_some() {
            if let Some(position) = self.order.iter().position(|id| *id == request_id) {
                self.order.remove(position);
            }
            let status = match reason {
                PendingFailureReason::Revoked => RecordStatus::Revoked,
                PendingFailureReason::Stopped => RecordStatus::Stopped,
                PendingFailureReason::Expired => RecordStatus::Expired,
            };
            self.history.insert(request_id, status);
            Some(PendingFailure { request_id, reason })
        } else {
            None
        }
    }

    fn fail_all(&mut self, reason: PendingFailureReason) -> Vec<PendingFailure> {
        let ids: Vec<RequestId> = self.order.iter().copied().collect();
        ids.into_iter()
            .filter_map(|request_id| self.remove_pending(request_id, reason))
            .collect()
    }
}

impl Default for Broker {
    fn default() -> Self {
        Self::new()
    }
}

struct OsRandom;

impl OsRandom {
    fn new() -> Self {
        Self
    }
}

impl RandomBytes for OsRandom {
    fn fill_bytes(&mut self, destination: &mut [u8]) -> Result<(), RandomError> {
        let mut source =
            std::fs::File::open("/dev/urandom").map_err(|_| RandomError::Unavailable)?;
        source
            .read_exact(destination)
            .map_err(|_| RandomError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::fmt::Write as _;

    #[derive(Default)]
    struct TestBytes {
        values: VecDeque<Vec<u8>>,
    }

    impl TestBytes {
        fn from_values(values: impl IntoIterator<Item = Vec<u8>>) -> Self {
            Self {
                values: values.into_iter().collect(),
            }
        }
    }

    impl RandomBytes for TestBytes {
        fn fill_bytes(&mut self, destination: &mut [u8]) -> Result<(), RandomError> {
            let Some(value) = self.values.pop_front() else {
                return Err(RandomError::Unavailable);
            };
            if value.len() != destination.len() {
                return Err(RandomError::InvalidLength);
            }
            destination.copy_from_slice(&value);
            Ok(())
        }
    }

    fn bytes(value: u8, length: usize) -> Vec<u8> {
        vec![value; length]
    }

    fn broker_with_clock(clock: ManualClock) -> Broker {
        Broker::with_dependencies(
            Box::new(TestBytes::from_values([
                bytes(1, REQUEST_ID_BYTES),
                bytes(2, REQUEST_ID_BYTES),
                bytes(3, REQUEST_ID_BYTES),
                bytes(4, REQUEST_ID_BYTES),
            ])),
            Box::new(TestBytes::from_values([
                bytes(9, APPROVAL_TOKEN_BYTES),
                bytes(8, APPROVAL_TOKEN_BYTES),
                bytes(7, APPROVAL_TOKEN_BYTES),
                bytes(6, APPROVAL_TOKEN_BYTES),
            ])),
            Box::new(clock),
        )
    }

    fn active_broker() -> (Broker, ManualClock) {
        let clock = ManualClock::new(1_000);
        let mut broker = broker_with_clock(clock.clone());
        assert_eq!(broker.start(), Ok(SessionState::Starting));
        assert_eq!(broker.activate(), Ok(SessionState::Active));
        (broker, clock)
    }

    #[test]
    fn approval_keeps_immutable_action_details_until_allow() {
        let (mut broker, _) = active_broker();
        let original = Action::RelativePointerMotion { dx: 12.5, dy: -4.0 };
        let receipt = broker.submit(original.clone()).expect("submit should work");
        let caller_copy = Action::RelativePointerMotion {
            dx: 999.0,
            dy: 999.0,
        };

        let approved = broker
            .allow_once(&receipt.request_id, &receipt.approval_token)
            .expect("approval should work");
        assert_eq!(
            approved.action,
            Action::RelativePointerMotion { dx: 12.5, dy: -4.0 }
        );
        assert_ne!(approved.action, caller_copy);
    }

    #[test]
    fn allow_once_consumes_exactly_one_request() {
        let (mut broker, _) = active_broker();
        let receipt = broker
            .submit(Action::KeyboardKeycode {
                keycode: 30,
                state: 1,
            })
            .expect("submit should work");

        let first = broker.allow_once(&receipt.request_id, &receipt.approval_token);
        let second = broker.allow_once(&receipt.request_id, &receipt.approval_token);
        assert!(first.is_ok());
        assert_eq!(second, Err(ApprovalError::AlreadyConsumed));
    }

    #[test]
    fn deny_cancel_and_expiry_never_return_executable_actions() {
        let (mut broker, clock) = active_broker();
        let denied = broker
            .submit(Action::PointerButton {
                button: 0x110,
                state: 1,
            })
            .expect("submit should work");
        assert_eq!(
            broker.deny(&denied.request_id, &denied.approval_token),
            Ok(DecisionOutcome::Denied)
        );

        let cancelled = broker
            .submit(Action::SmoothScroll { dx: 1.0, dy: -2.0 })
            .expect("submit should work");
        assert_eq!(
            broker.cancel(&cancelled.request_id, &cancelled.approval_token),
            Ok(DecisionOutcome::Cancelled)
        );

        let expired = broker
            .submit(Action::Screenshot { max_bytes: 4096 })
            .expect("submit should work");
        assert_eq!(expired.deadline_ms, 1_000 + MAX_REQUEST_LIFETIME_MS);
        clock.advance_ms(MAX_REQUEST_LIFETIME_MS + 1);
        assert_eq!(
            broker.allow_once(&expired.request_id, &expired.approval_token),
            Err(ApprovalError::Expired)
        );
        assert_eq!(
            broker.deny(&expired.request_id, &expired.approval_token),
            Err(ApprovalError::Expired)
        );
    }

    #[test]
    fn revoke_clears_pending_and_invalidates_epoch() {
        let (mut broker, _) = active_broker();
        let receipt = broker
            .submit(Action::Screenshot { max_bytes: 1024 })
            .expect("submit should work");
        let report = broker.revoke();
        assert_eq!(report.state, SessionState::Disabled);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].reason, PendingFailureReason::Revoked);
        assert_eq!(
            broker.allow_once(&receipt.request_id, &receipt.approval_token),
            Err(ApprovalError::StaleEpoch)
        );
        assert_eq!(broker.pending_len(), 0);
    }

    #[test]
    fn duplicate_ids_and_capacity_are_rejected() {
        let clock = ManualClock::new(0);
        let mut broker = Broker::with_dependencies(
            Box::new(TestBytes::from_values(
                (0..40).map(|_| bytes(1, REQUEST_ID_BYTES)),
            )),
            Box::new(TestBytes::from_values(
                (0..40).map(|_| bytes(2, APPROVAL_TOKEN_BYTES)),
            )),
            Box::new(clock),
        );
        broker.start().expect("start should work");
        broker.activate().expect("activate should work");
        broker
            .submit(Action::Screenshot { max_bytes: 1 })
            .expect("first request should work");
        assert_eq!(
            broker.submit(Action::Screenshot { max_bytes: 1 }),
            Err(SubmitError::DuplicateRequestId)
        );

        let mut capacity_broker = Broker::with_dependencies(
            Box::new(TestBytes::from_values(
                (0..33).map(|n| bytes(n, REQUEST_ID_BYTES)),
            )),
            Box::new(TestBytes::from_values(
                (0..33).map(|n| bytes(n, APPROVAL_TOKEN_BYTES)),
            )),
            Box::new(ManualClock::new(0)),
        );
        capacity_broker.start().expect("start should work");
        capacity_broker.activate().expect("activate should work");
        for _ in 0..MAX_PENDING_REQUESTS {
            capacity_broker
                .submit(Action::Screenshot { max_bytes: 1 })
                .expect("capacity should allow 32 requests");
        }
        assert_eq!(
            capacity_broker.submit(Action::Screenshot { max_bytes: 1 }),
            Err(SubmitError::Capacity)
        );
    }

    #[test]
    fn approval_tokens_are_single_use_capabilities_even_if_source_repeats() {
        let mut broker = Broker::with_dependencies(
            Box::new(TestBytes::from_values([
                bytes(1, REQUEST_ID_BYTES),
                bytes(2, REQUEST_ID_BYTES),
            ])),
            Box::new(TestBytes::from_values([
                bytes(9, APPROVAL_TOKEN_BYTES),
                bytes(9, APPROVAL_TOKEN_BYTES),
            ])),
            Box::new(ManualClock::new(0)),
        );
        broker.start().expect("start should work");
        broker.activate().expect("activate should work");
        broker
            .submit(Action::Screenshot { max_bytes: 1 })
            .expect("first request should work");
        assert_eq!(
            broker.submit(Action::Screenshot { max_bytes: 1 }),
            Err(SubmitError::DuplicateApprovalToken)
        );
    }

    #[test]
    fn stale_epoch_approval_is_rejected_after_restart() {
        let (mut broker, _) = active_broker();
        let receipt = broker
            .submit(Action::DiscreteScroll { dx: 1, dy: 0 })
            .expect("submit should work");
        broker.revoke();
        broker.start().expect("restart should work");
        broker.activate().expect("reactivate should work");
        assert_eq!(
            broker.allow_once(&receipt.request_id, &receipt.approval_token),
            Err(ApprovalError::StaleEpoch)
        );
    }

    #[test]
    fn invalid_action_inputs_are_rejected_without_io() {
        assert!(matches!(
            validate_action(&Action::RelativePointerMotion {
                dx: f64::NAN,
                dy: 0.0,
            }),
            Err(ActionValidationError::NonFinite)
        ));
        assert!(matches!(
            validate_action(&Action::RelativePointerMotion {
                dx: 20_001.0,
                dy: 0.0
            }),
            Err(ActionValidationError::OutOfBounds)
        ));
        assert!(
            validate_action(&Action::PointerButton {
                button: 0xff,
                state: 1
            })
            .is_err()
        );
        assert!(
            validate_action(&Action::PointerButton {
                button: 0x100,
                state: 2
            })
            .is_err()
        );
        assert!(
            validate_action(&Action::KeyboardKeycode {
                keycode: 0x300,
                state: 1
            })
            .is_err()
        );
        assert!(
            validate_action(&Action::KeyboardKeysym {
                keysym: 0x2000_0000,
                state: 0
            })
            .is_err()
        );
        assert!(
            validate_action(&Action::SmoothScroll {
                dx: 0.0,
                dy: 20_001.0
            })
            .is_err()
        );
        assert!(validate_action(&Action::DiscreteScroll { dx: 101, dy: 0 }).is_err());
        assert!(validate_action(&Action::Screenshot { max_bytes: 0 }).is_err());
        assert!(
            validate_action(&Action::Screenshot {
                max_bytes: MAX_SCREENSHOT_BYTES + 1
            })
            .is_err()
        );
    }

    #[test]
    fn unavailable_actions_are_explicitly_blocked() {
        assert_eq!(
            blocked(UnavailableAction::Clipboard),
            PolicyOutcome::Blocked(UnavailableAction::Clipboard)
        );
        assert_eq!(
            blocked(UnavailableAction::Shell),
            PolicyOutcome::Blocked(UnavailableAction::Shell)
        );
    }

    #[test]
    fn activity_format_is_redacted_and_allow_listed() {
        let (mut broker, _) = active_broker();
        let receipt = broker
            .submit(Action::Screenshot { max_bytes: 4096 })
            .expect("submit should work");
        let event = broker
            .allow_once(&receipt.request_id, &receipt.approval_token)
            .expect("approval should work")
            .activity_event();
        let mut formatted = String::new();
        write!(&mut formatted, "{event}").expect("format should work");
        assert!(formatted.contains("screenshot"));
        assert!(formatted.contains("4096 bytes max"));
        assert!(!formatted.contains(&receipt.approval_token.to_string()));
        assert!(!formatted.contains("/absolute/secret/path"));
        assert!(!formatted.contains("screenshot-bytes"));
        assert!(!formatted.contains("page content"));
    }

    #[test]
    fn stop_fails_pending_requests_and_allows_no_action_afterward() {
        let (mut broker, _) = active_broker();
        let receipt = broker
            .submit(Action::PointerButton {
                button: 0x110,
                state: 0,
            })
            .expect("submit should work");
        let report = broker.stop();
        assert_eq!(report.state, SessionState::Stopped);
        assert_eq!(report.failures[0].reason, PendingFailureReason::Stopped);
        assert_eq!(
            broker.allow_once(&receipt.request_id, &receipt.approval_token),
            Err(ApprovalError::Stopped)
        );
        assert_eq!(
            broker.submit(Action::Screenshot { max_bytes: 1 }),
            Err(SubmitError::Inactive(SessionState::Stopped))
        );
    }
}

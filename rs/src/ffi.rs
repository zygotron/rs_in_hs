#![allow(dead_code)]
//! FFI boundary between Rust and Haskell.
//!
//! Exports `h2r_init`, `h2r_call`, `h2r_cast`, `h2r_subscribe`,
//! `h2r_next_event`, and `h2r_deinit` as C symbols.
//! Haskell callbacks are passed as function pointers at init time —
//! Rust never links against Haskell symbols by name.

use std::ffi::c_void;
use std::sync::{LazyLock, Mutex, RwLock};
use std::thread;

use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

use crate::dispatch;
use crate::worker;

// ---------------------------------------------------------------------------
// Message types (shared with Haskell via CBOR/ciborium serialization)
// ---------------------------------------------------------------------------

/// Configuration passed from Haskell at init time.
/// Serialized as a single CBOR unsigned integer (the max_msg_len value),
/// matching the Haskell `instance Serialise Config`.
#[derive(Serialize, Deserialize, Debug)]
#[serde(transparent)]
pub struct Config {
    pub max_msg_len: u32,
}

/// Variant order must match the Haskell `data MessageBody = Ping | Pong` exactly.
/// Serialized as a single CBOR unsigned integer (variant index).
#[derive(Serialize_repr, Deserialize_repr, Debug)]
#[repr(u8)]
pub enum MessageBody {
    Ping = 0,
    Pong = 1,
}

impl MessageBody {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::into_writer(self, &mut buf).expect("CBOR serialization failed");
        buf
    }
}

/// Variant order must match the Haskell `data EventBody = CastReceived | Heartbeat` exactly.
/// Serialized as a single CBOR unsigned integer (variant index).
#[derive(Serialize_repr, Deserialize_repr, Debug)]
#[repr(u8)]
pub enum EventBody {
    CastReceived = 0,
    Heartbeat = 1,
}

impl EventBody {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::into_writer(self, &mut buf).expect("CBOR serialization failed");
        buf
    }
}

// ---------------------------------------------------------------------------
// Call/cast/event message wrappers (Rust-side only)
// ---------------------------------------------------------------------------

pub struct CallMessage {
    pub token: CallToken,
    pub body: MessageBody,
}

pub struct CastMessage {
    pub body: MessageBody,
}

pub struct ResponseMessage {
    pub token: CallToken,
    pub body: MessageBody,
}

pub struct EventMessage {
    pub body: EventBody,
}

// ---------------------------------------------------------------------------
// CallToken — one-shot response token wrapping a Haskell StablePtr (MVar)
// ---------------------------------------------------------------------------

pub struct CallToken {
    inner: Option<*mut c_void>,
}

// SAFETY: The *mut c_void is a Haskell StablePtr (GC root). The MVar it
// references is thread-safe, and the StablePtr remains valid until
// freeStablePtr is called on the Haskell side.
unsafe impl Send for CallToken {}

impl CallToken {
    pub fn new(stable_ptr: *mut c_void) -> Self {
        Self {
            inner: Some(stable_ptr),
        }
    }

    /// Extract the StablePtr, setting inner to None so Drop is clean.
    pub fn take(&mut self) -> *mut c_void {
        self.inner.take().expect("CallToken already consumed")
    }
}

impl Drop for CallToken {
    fn drop(&mut self) {
        if self.inner.is_some() {
            eprintln!("error invariant broken: CallToken dropped without response");
        }
    }
}

// ---------------------------------------------------------------------------
// CallResponseCallback — wraps the Haskell callResponse function pointer
// ---------------------------------------------------------------------------

/// C function pointer type for the Haskell `callResponse` callback.
/// Signature: `callResponse(stable_ptr, data, len)`
pub type CallResponseFn = unsafe extern "C" fn(*mut c_void, *const u8, usize);

pub struct CallResponseCallback {
    response_fn: CallResponseFn,
}

// SAFETY: The function pointer is a statically-linked address of a Haskell
// `foreign export ccall` function. With `-threaded`, the GHC RTS handles
// concurrent calls from foreign threads safely.
unsafe impl Send for CallResponseCallback {}

impl CallResponseCallback {
    pub fn new(response_fn: CallResponseFn) -> Self {
        Self { response_fn }
    }

    /// Send a response back to Haskell, consuming the CallToken.
    ///
    /// # Safety
    /// - `data` must point to `len` valid bytes.
    /// - The StablePtr inside `token` must still be valid.
    /// - GHC RTS must be running with `-threaded`.
    pub unsafe fn respond(&self, token: &mut CallToken, data: *const u8, len: usize) {
        let stable_ptr = token.take();
        unsafe {
            (self.response_fn)(stable_ptr, data, len);
        }
    }
}

// ---------------------------------------------------------------------------
// Global static context — no pointer crosses the FFI boundary
// ---------------------------------------------------------------------------

// Senders are accessed concurrently by h2r_call/h2r_cast (read lock).
// Separated from shutdown state because callers only need senders.
struct Senders {
    call_tx: flume::Sender<CallMessage>,
    cast_tx: flume::Sender<CastMessage>,
    event_tx: flume::Sender<EventMessage>,
    config: Config,
}

// Thread handles are only accessed by h2r_deinit (mutex, single access).
struct ShutdownState {
    worker_handle: thread::JoinHandle<()>,
    dispatch_handle: thread::JoinHandle<()>,
}

// Reinit after deinit (tests, engine restart) is supported because the
// actual state is Option<_> inside the lock — init writes Some, deinit
// writes None. LazyLock just initializes the lock wrapper itself once.
// RwLock: readers (call/cast) do not contend with each other.
// Write lock only taken at deinit.
static SENDERS: LazyLock<RwLock<Option<Senders>>> = LazyLock::new(|| RwLock::new(None));
static SHUTDOWN: LazyLock<Mutex<Option<ShutdownState>>> = LazyLock::new(|| Mutex::new(None));

// Stored globally at init so h2r_next_event can call back into Haskell.
static CALL_RESPONSE_FN: LazyLock<Mutex<Option<CallResponseFn>>> =
    LazyLock::new(|| Mutex::new(None));

// Subscription receivers, indexed by subscription ID.
// Each entry is a Receiver<EventMessage> from a bounded channel.
static SUBSCRIPTIONS: LazyLock<Mutex<Vec<Option<flume::Receiver<EventMessage>>>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

// ---------------------------------------------------------------------------
// Exported C functions
// ---------------------------------------------------------------------------

/// Initialize the Rust side. Called once by Haskell at startup.
///
/// Spawns a worker thread and a dispatch thread. State is stored in
/// module-level statics — no pointer crosses the FFI boundary.
/// # Safety
/// - `config_data` must point to `config_len` valid bytes (serialized Config).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn h2r_init(
    call_response_fn: CallResponseFn,
    config_data: *const u8,
    config_len: usize,
) {
    // catch_unwind: panicking across extern "C" is UB. We abort on panic
    // so AssertUnwindSafe is justified — no recovery, no inconsistent state.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let config_bytes = unsafe { std::slice::from_raw_parts(config_data, config_len) };
        let config: Config =
            ciborium::from_reader(config_bytes).expect("h2r_init: deserialize Config");

        /// Channel capacity for control messages. Sized for bursts without
        /// unbounded growth. send() blocks when full — natural backpressure.
        const CHANNEL_CAPACITY: usize = 256;

        let (call_tx, call_rx) = flume::bounded::<CallMessage>(CHANNEL_CAPACITY);
        let (cast_tx, cast_rx) = flume::bounded::<CastMessage>(CHANNEL_CAPACITY);
        let (response_tx, response_rx) = flume::bounded::<ResponseMessage>(CHANNEL_CAPACITY);
        let (event_tx, event_rx) = flume::bounded::<EventMessage>(CHANNEL_CAPACITY);

        let worker_event_tx = event_tx.clone();
        let worker_handle =
            thread::spawn(move || worker::run(call_rx, cast_rx, response_tx, worker_event_tx));

        let cb = CallResponseCallback::new(call_response_fn);
        let dispatch_handle = thread::spawn(move || dispatch::run(response_rx, cb));

        // Store callback globally for h2r_next_event.
        *CALL_RESPONSE_FN
            .lock()
            .expect("CALL_RESPONSE_FN lock poisoned") = Some(call_response_fn);

        // Pre-create subscription 0 with the event receiver.
        // h2r_subscribe will return this for now; later the dispatcher
        // will create per-topic channels.
        *SUBSCRIPTIONS.lock().expect("SUBSCRIPTIONS lock poisoned") = vec![Some(event_rx)];

        *SENDERS.write().expect("SENDERS lock poisoned") = Some(Senders {
            call_tx,
            cast_tx,
            event_tx,
            config,
        });
        *SHUTDOWN.lock().expect("SHUTDOWN lock poisoned") = Some(ShutdownState {
            worker_handle,
            dispatch_handle,
        });
    }));
    if let Err(e) = result {
        eprintln!("h2r_init panicked: {:?}", e);
        std::process::abort();
    }
}

/// Shut down the Rust side. Blocks until all threads have exited.
///
/// Write-locks SENDERS and sets to None (drops senders, disconnects channels).
/// Then locks SHUTDOWN, takes thread handles, and joins them.
#[unsafe(no_mangle)]
pub extern "C" fn h2r_deinit() {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Drop senders to disconnect channels, signalling threads to exit.
        *SENDERS.write().expect("SENDERS lock poisoned") = None;
        // Clear subscriptions (drops receivers).
        SUBSCRIPTIONS
            .lock()
            .expect("SUBSCRIPTIONS lock poisoned")
            .clear();
        // Clear stored callback.
        *CALL_RESPONSE_FN
            .lock()
            .expect("CALL_RESPONSE_FN lock poisoned") = None;
        // Take thread handles and join.
        if let Some(state) = SHUTDOWN.lock().expect("SHUTDOWN lock poisoned").take() {
            state.worker_handle.join().expect("worker thread panicked");
            state
                .dispatch_handle
                .join()
                .expect("dispatch thread panicked");
        }
    }));
    if let Err(e) = result {
        eprintln!("h2r_deinit panicked: {:?}", e);
        std::process::abort();
    }
}

/// Send a call (request-response) from Haskell to Rust.
///
/// Deserialization happens here, on the Haskell RTS worker thread that
/// invoked this FFI call. The deserialized CallMessage is then sent to
/// the Rust worker thread via a flume channel.
///
/// # Safety
/// - `data` must point to `len` valid bytes (serialized request).
/// - `stable_ptr` must be a valid Haskell StablePtr (MVar ByteString).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn h2r_call(stable_ptr: *mut c_void, data: *const u8, len: usize) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let guard = SENDERS.read().expect("SENDERS lock poisoned");
        let senders = guard.as_ref().expect("h2r_call: not initialized");
        // Defense-in-depth: Haskell validates before calling, but assert here too.
        assert!(
            len <= senders.config.max_msg_len as usize,
            "h2r_call: len ({len}) exceeds max_msg_len ({})",
            senders.config.max_msg_len,
        );
        let bytes = unsafe { std::slice::from_raw_parts(data, len) };
        let body: MessageBody = ciborium::from_reader(bytes as &[u8]).expect("h2r_call: deserialize");
        let msg = CallMessage {
            token: CallToken::new(stable_ptr),
            body,
        };
        senders.call_tx.send(msg).expect("h2r_call: send");
    }));
    if let Err(e) = result {
        eprintln!("h2r_call panicked: {:?}", e);
        std::process::abort();
    }
}

/// Send a cast (fire-and-forget) from Haskell to Rust.
///
/// # Safety
/// - `data` must point to `len` valid bytes (serialized request).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn h2r_cast(data: *const u8, len: usize) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let guard = SENDERS.read().expect("SENDERS lock poisoned");
        let senders = guard.as_ref().expect("h2r_cast: not initialized");
        // Defense-in-depth: Haskell validates before calling, but assert here too.
        assert!(
            len <= senders.config.max_msg_len as usize,
            "h2r_cast: len ({len}) exceeds max_msg_len ({})",
            senders.config.max_msg_len,
        );
        let bytes = unsafe { std::slice::from_raw_parts(data, len) };
        let body: MessageBody = ciborium::from_reader(bytes as &[u8]).expect("h2r_cast: deserialize");
        let msg = CastMessage { body };
        senders.cast_tx.send(msg).expect("h2r_cast: send");
    }));
    if let Err(e) = result {
        eprintln!("h2r_cast panicked: {:?}", e);
        std::process::abort();
    }
}

/// Subscribe to an event topic. Returns a subscription ID (>= 0).
///
/// For now, ignores the topic and returns subscription 0 (the single
/// pre-created event channel). When the dispatcher is added, this will
/// create a per-topic channel and return a new ID.
///
/// # Safety
/// - `topic_ptr` must point to `topic_len` valid bytes (UTF-8 topic string).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn h2r_subscribe(_topic_ptr: *const u8, _topic_len: usize) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // TODO: when dispatcher exists, deserialize topic, create channel,
        // register with dispatcher, store receiver in SUBSCRIPTIONS.
        0i32
    }));
    match result {
        Ok(id) => id,
        Err(e) => {
            eprintln!("h2r_subscribe panicked: {:?}", e);
            std::process::abort();
        }
    }
}

/// Block until the next event arrives on the given subscription.
///
/// Returns 0 on success (event bytes written via callResponse callback),
/// or 1 on shutdown (channel disconnected).
///
/// Reuses the `callResponse` callback stored at init time — Rust serializes
/// the event, calls `callResponse(stable_ptr, data, len)`, and Haskell's
/// callback copies the bytes into the MVar.
///
/// # Safety
/// - `stable_ptr` must be a valid Haskell StablePtr (MVar ByteString).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn h2r_next_event(sub_id: i32, stable_ptr: *mut c_void) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let rx = {
            let subs = SUBSCRIPTIONS.lock().expect("SUBSCRIPTIONS lock poisoned");
            let slot = subs
                .get(sub_id as usize)
                .expect("h2r_next_event: invalid subscription ID");
            slot.as_ref()
                .expect("h2r_next_event: subscription closed")
                .clone()
        };

        match rx.recv() {
            Ok(EventMessage { body }) => {
                let response_fn = CALL_RESPONSE_FN
                    .lock()
                    .expect("CALL_RESPONSE_FN lock poisoned")
                    .expect("h2r_next_event: callResponse not initialized");
                let bytes = body.to_bytes();
                unsafe {
                    response_fn(stable_ptr, bytes.as_ptr(), bytes.len());
                }
                0
            }
            Err(_) => 1, // channel disconnected (shutdown)
        }
    }));
    match result {
        Ok(status) => status,
        Err(e) => {
            eprintln!("h2r_next_event panicked: {:?}", e);
            std::process::abort();
        }
    }
}

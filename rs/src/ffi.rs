#![allow(dead_code)]
//! FFI boundary between Rust and Haskell.
//!
//! Exports `h2r_init`, `h2r_call`, `h2r_cast`, and `h2r_deinit` as C symbols.
//! Haskell callbacks are passed as function pointers at init time —
//! Rust never links against Haskell symbols by name.

use std::ffi::c_void;
use std::sync::{LazyLock, Mutex, RwLock};
use std::thread;

use serde::{Deserialize, Serialize};

use crate::dispatch;
use crate::worker;

// ---------------------------------------------------------------------------
// Message types (shared with Haskell via store/serde_store serialization)
// ---------------------------------------------------------------------------

/// Configuration passed from Haskell at init time.
/// Field order must match Haskell's `newtype Config` exactly (serde_store layout).
#[derive(Serialize, Deserialize, Debug)]
pub struct Config {
    pub max_msg_len: u32,
}

/// Variant order must match the Haskell `data MessageBody = Ping | Pong` exactly.
#[derive(Serialize, Deserialize, Debug)]
pub enum MessageBody {
    Ping,
    Pong,
}

impl MessageBody {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_store::to_bytes(self).expect("serialize MessageBody")
    }
}

// ---------------------------------------------------------------------------
// Call/cast message wrappers (Rust-side only)
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
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let config_bytes = unsafe { std::slice::from_raw_parts(config_data, config_len) };
            let config: Config =
                serde_store::from_bytes(config_bytes).expect("h2r_init: deserialize Config");

            /// Channel capacity for control messages. Sized for bursts without
            /// unbounded growth. send() blocks when full — natural backpressure.
            const CHANNEL_CAPACITY: usize = 256;

            let (call_tx, call_rx) = flume::bounded::<CallMessage>(CHANNEL_CAPACITY);
            let (cast_tx, cast_rx) = flume::bounded::<CastMessage>(CHANNEL_CAPACITY);
            let (response_tx, response_rx) =
                flume::bounded::<ResponseMessage>(CHANNEL_CAPACITY);

            let worker_handle =
                thread::spawn(move || worker::run(call_rx, cast_rx, response_tx));

            let cb = CallResponseCallback::new(call_response_fn);
            let dispatch_handle = thread::spawn(move || dispatch::run(response_rx, cb));

            *SENDERS.write().expect("SENDERS lock poisoned") =
                Some(Senders { call_tx, cast_tx, config });
            *SHUTDOWN.lock().expect("SHUTDOWN lock poisoned") =
                Some(ShutdownState { worker_handle, dispatch_handle });
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
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Drop senders to disconnect channels, signalling threads to exit.
            *SENDERS.write().expect("SENDERS lock poisoned") = None;
            // Take thread handles and join.
            if let Some(state) = SHUTDOWN.lock().expect("SHUTDOWN lock poisoned").take() {
                state.worker_handle.join().expect("worker thread panicked");
                state.dispatch_handle.join().expect("dispatch thread panicked");
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
pub unsafe extern "C" fn h2r_call(
    stable_ptr: *mut c_void,
    data: *const u8,
    len: usize,
) {
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let guard = SENDERS.read().expect("SENDERS lock poisoned");
            let senders = guard.as_ref().expect("h2r_call: not initialized");
            // Defense-in-depth: Haskell validates before calling, but assert here too.
            assert!(
                len <= senders.config.max_msg_len as usize,
                "h2r_call: len ({len}) exceeds max_msg_len ({})",
                senders.config.max_msg_len,
            );
            let bytes = unsafe { std::slice::from_raw_parts(data, len) };
            let body: MessageBody =
                serde_store::from_bytes(bytes).expect("h2r_call: deserialize");
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
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let guard = SENDERS.read().expect("SENDERS lock poisoned");
            let senders = guard.as_ref().expect("h2r_cast: not initialized");
            // Defense-in-depth: Haskell validates before calling, but assert here too.
            assert!(
                len <= senders.config.max_msg_len as usize,
                "h2r_cast: len ({len}) exceeds max_msg_len ({})",
                senders.config.max_msg_len,
            );
            let bytes = unsafe { std::slice::from_raw_parts(data, len) };
            let body: MessageBody =
                serde_store::from_bytes(bytes).expect("h2r_cast: deserialize");
            let msg = CastMessage { body };
            senders.cast_tx.send(msg).expect("h2r_cast: send");
        }));
    if let Err(e) = result {
        eprintln!("h2r_cast panicked: {:?}", e);
        std::process::abort();
    }
}

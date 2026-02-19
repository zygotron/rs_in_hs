# FFI Safety Fixes

## Overview

Safety audit of the Haskell↔Rust FFI bridge identified 11 issues across memory safety,
correctness, and hardening. This document describes the fixes, grouped by theme. Each
section explains the problem, why it matters, and the concrete change.

All new and modified code should be well commented — safety invariants, preconditions,
and non-obvious design choices must be documented inline.

---

## 1. Global Static Context

### Problem

The Rust `Context` (channel senders + thread handles) is heap-allocated via `Box::into_raw`
and passed to Haskell as an opaque `Ptr ()`. Haskell passes this pointer back on every
`h2r_call`, `h2r_cast`, and `h2r_deinit`. This creates two classes of UB:

- **Use-after-free**: `forkRust` spawns Haskell threads that capture the pointer. If the
  main `withRust` action returns while forked threads are in-flight, `finally` calls
  `h2r_deinit` (`Box::from_raw`), freeing the context while forked threads still hold
  the pointer.
- **Null/stale dereference**: All exported Rust functions blindly cast and dereference
  `ctx_ptr`. No validation is possible beyond a null check.

### Fix

Move context ownership to a Rust module-level static. No pointer crosses the FFI boundary.

**Rust (`rs/src/ffi.rs`):**

Remove `Context` struct. Split into two statics:

```rust
use std::sync::{LazyLock, Mutex, RwLock};

// Senders are accessed concurrently by h2r_call/h2r_cast (read lock).
// Separated from shutdown state because callers only need senders.
struct Senders {
    call_tx: flume::Sender<CallMessage>,
    cast_tx: flume::Sender<CastMessage>,
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
```

- `h2r_init`: Populate both statics. Takes no `ctx_ptr`. Returns nothing.
- `h2r_call` / `h2r_cast`: Read-lock `SENDERS`, send on channel. No `ctx_ptr` param.
- `h2r_deinit`: Write-lock `SENDERS`, set to `None` (drops senders → channels disconnect).
  Then lock `SHUTDOWN`, take handles, join threads. No `ctx_ptr` param.

**Haskell (`hs/src/FFI.hs`):**

Remove `Ptr ()` from all foreign import/export signatures:

```haskell
foreign import ccall safe "h2r_init"
  h2rInit :: FunPtr (...) -> IO ()

foreign import ccall safe "h2r_deinit"
  h2rDeinit :: IO ()

foreign import ccall safe "h2r_call"
  h2rCall :: StablePtr (MVar ByteString) -> Ptr () -> CSize -> IO ()

foreign import ccall safe "h2r_cast"
  h2rCast :: Ptr () -> CSize -> IO ()
```

`h2rInit` and `h2rDeinit` are not exported from the module — only `withRust` exposes them.

**Haskell (`hs/src/RustBridge.hs`):**

`RustEnv` no longer holds a pointer. Currently empty, will grow with app state.

```haskell
data RustEnv = RustEnv  -- placeholder for future app monad state

withRust :: RustT IO a -> IO a
withRust (RustT action) = do
  h2rInit callResponseFunPtr
  let env = RustEnv
  runReaderT action env `finally` h2rDeinit
```

---

## 2. Structured Concurrency via `async`

### Problem

`forkRust` spawns threads that can outlive the `withRust` scope. When the main action
returns, `finally` calls `h2r_deinit` while forked threads may still be calling
`h2r_call`/`h2r_cast`. The current benchmark works around this with a manual `MVar`
barrier, but this is not enforced structurally.

### Fix

Replace `forkRust` (returns `ThreadId`, fire-and-forget) with `async`-based primitives.
Add `async` package as a dependency.

```haskell
-- | Spawn an async in the RustT environment. Caller manages the handle.
asyncRust :: RustT IO a -> RustT IO (Async a)
asyncRust (RustT action) = do
  env <- ask
  liftIO $ async $ runReaderT action env

-- | Scoped async — cancelled automatically when the scope exits.
-- This is the preferred concurrency primitive. Guarantees the child
-- does not outlive the parent scope, preventing use-after-deinit.
withAsyncRust :: RustT IO a -> (Async a -> RustT IO b) -> RustT IO b
withAsyncRust (RustT action) inner = do
  env <- ask
  RustT $ ReaderT $ \e ->
    withAsync (runReaderT action e) $
      \a -> case inner a of RustT r -> runReaderT r e
```

Remove `forkRust`. Export `asyncRust` and `withAsyncRust`.

**`hs/app/Main.hs`:** Update benchmark to use `asyncRust` or `forConcurrently_`
(from `async` package) instead of `forkRust`.

**`hs/rs-in-hs.cabal`:** Add `async` to `build-depends`.

---

## 3. Unsigned Length Type + Config via `h2r_init`

### Problem

Byte lengths cross the FFI as `c_int` (signed). A negative value wraps to a huge `usize`
in the `len as usize` cast, causing `std::slice::from_raw_parts` to read out of bounds —
immediate UB. On the response path, `bytes.len() as c_int` silently truncates if the
serialized response exceeds `c_int::MAX`.

Additionally, configuration values (like max message length) should flow from Haskell to
Rust at init time rather than being duplicated constants on each side.

### Fix

**Unsigned lengths:** Change all length parameters from `c_int` / `CInt` to `size_t` /
`CSize` (unsigned). Eliminates the negative-length class of bugs by type.

**Config type:** Add a `Config` struct serialized with `store`/`serde_store`, passed from
Haskell to Rust at init time. Haskell is the single source of truth for configuration.

**Haskell (`hs/src/Types.hs`):**

```haskell
data Config = Config
  { maxMsgLen :: Word32
  } deriving (Generic, Show)

instance Store Config
```

**Rust (`rs/src/ffi.rs`):**

```rust
/// Configuration passed from Haskell at init time.
/// Variant/field order must match Haskell's `data Config` exactly.
#[derive(Serialize, Deserialize, Debug)]
pub struct Config {
    pub max_msg_len: u32,
}
```

Store the deserialized `Config` in the global static alongside senders:

```rust
struct Senders {
    call_tx: flume::Sender<CallMessage>,
    cast_tx: flume::Sender<CastMessage>,
    config: Config,
}
```

**`h2r_init` signature change:**

Rust:
```rust
#[unsafe(no_mangle)]
pub unsafe extern "C" fn h2r_init(
    call_response_fn: CallResponseFn,
    config_data: *const u8,
    config_len: usize,
) { ... }
```

Haskell (`hs/src/FFI.hs`):
```haskell
foreign import ccall safe "h2r_init"
  h2rInit :: FunPtr (...) -> Ptr () -> CSize -> IO ()
```

Haskell (`hs/src/RustBridge.hs`) — `withRust` serializes and passes the config:
```haskell
withRust :: Config -> RustT IO a -> IO a
withRust config (RustT action) = do
  let bs = encode config
  useAsCStringLen bs $ \(ptr, len) ->
    h2rInit callResponseFunPtr (castPtr ptr) (fromIntegral len)
  let env = RustEnv
  runReaderT action env `finally` h2rDeinit
```

Validate `len <= config.max_msg_len` in `callRust`/`castRust` on the Haskell side.
Rust side also asserts as defense-in-depth in `h2r_call`/`h2r_cast`.

**Files:** `rs/src/ffi.rs`, `rs/src/dispatch.rs`, `hs/src/FFI.hs`, `hs/src/RustBridge.hs`,
`hs/src/Types.hs`

---

## 4. `callRustTimeout`

### Problem

If a `CallToken` is dropped without `respond()` being called (worker panic, lost message),
`CallToken::Drop` prints to stderr but Haskell's `takeMVar` blocks forever. This is an
invariant that should never break, but a timeout variant provides defense-in-depth.

### Fix

Add a timeout variant in `hs/src/RustBridge.hs`. Leave `callRust` as-is (blocking,
invariant should hold).

```haskell
-- | Like 'callRust' but with an explicit timeout in microseconds.
-- Returns 'Nothing' if the timeout fires before Rust responds.
callRustTimeout :: (Store req, Store resp, MonadIO m)
                => Int -> req -> RustT m (Maybe resp)
```

Wraps `takeMVar` in `System.Timeout.timeout`.

**Required supporting change in `hs/src/FFI.hs`:** Change `putMVar` → `tryPutMVar` in
`callResponse`. If a timeout fires and Haskell abandons the MVar, Rust will eventually
call `callResponse` which does `deRefStablePtr` + `freeStablePtr` + `putMVar`. The
`putMVar` would block forever on a full/GC'd MVar. `tryPutMVar` absorbs the late write
harmlessly. Safe for the non-timeout path too — the MVar is always empty when the
response arrives under normal conditions.

Note: the invariant is that every call receives a response. If this breaks (worker crash),
the application is in catastrophic failure — a leaked StablePtr is not a concern at that
point.

```haskell
callResponse sptr ptr len = do
  mvar <- deRefStablePtr sptr
  freeStablePtr sptr
  bs <- packCStringLen (castPtr ptr, fromIntegral len)
  void $ tryPutMVar mvar bs  -- tryPutMVar: absorbs late writes after timeout
```

---

## 5. `decodeEx` → `decode`

### Problem

`decodeEx` is partial — throws on decode failure. If Rust sends corrupted bytes, Haskell
gets an unhandled exception with no context. Not memory-unsafe, but a crash with a bad
error message.

### Fix

In `hs/src/RustBridge.hs`, replace `decodeEx` with `decode`:

```haskell
case decode respBs of
  Right v  -> pure v
  Left err -> throwIO err  -- structured error, catchable
```

---

## 6. Bounded Channels

### Problem

All three flume channels are unbounded. If Haskell sends faster than Rust processes,
memory grows without limit. For a real-time audio control path this is an OOM risk
under sustained load.

### Fix

In `rs/src/ffi.rs`, replace `flume::unbounded()` with `flume::bounded()`:

```rust
/// Channel capacity for control messages. Sized for bursts without
/// unbounded growth. send() blocks when full — natural backpressure.
const CHANNEL_CAPACITY: usize = 256;

let (call_tx, call_rx) = flume::bounded::<CallMessage>(CHANNEL_CAPACITY);
let (cast_tx, cast_rx) = flume::bounded::<CastMessage>(CHANNEL_CAPACITY);
let (response_tx, response_rx) = flume::bounded::<ResponseMessage>(CHANNEL_CAPACITY);
```

Blocking on `send()` is safe here because `h2r_call`/`h2r_cast` are declared `safe` in
the Haskell FFI imports. A `safe` foreign call releases the GHC capability, so other
Haskell threads continue running while the caller blocks on a full channel. This is the
correct backpressure mechanism.

---

## 7. `catch_unwind` in FFI Exports

### Problem

Panicking inside `extern "C"` functions is UB — unwinding across the FFI boundary.
The `expect()` calls in `h2r_call`, `h2r_cast`, `h2r_deinit` (deserialization failure,
channel disconnect, thread join on panicked thread) all trigger this.

### Fix

Wrap each `extern "C"` function body in `std::panic::catch_unwind`. On caught panic,
log to stderr and abort. Prevents unwinding across FFI.

```rust
#[unsafe(no_mangle)]
pub unsafe extern "C" fn h2r_call(...) {
    let result = std::panic::catch_unwind(|| {
        // ... function body ...
    });
    if let Err(e) = result {
        eprintln!("h2r_call panicked: {:?}", e);
        std::process::abort();
    }
}
```

Apply to: `h2r_init`, `h2r_deinit`, `h2r_call`, `h2r_cast`.

Note: `catch_unwind` only catches unwind panics (the default `panic = "unwind"` strategy).
If `panic = "abort"` were set in `Cargo.toml`, `catch_unwind` would be a no-op. Do not
set `panic = "abort"` for this crate.

---

## 8. `-threaded` on Library

### Problem

`-threaded` is only set on the executable, not the library. If the library is linked
into a non-threaded program, Rust's dispatch thread calling back into Haskell via
`callResponse` is UB per GHC's FFI spec (foreign threads calling into a non-threaded RTS).

### Fix

In `hs/rs-in-hs.cabal`, add `-threaded` to the library's `ghc-options`:

```
library
  ghc-options: -Wall -threaded
```

---

## Not Fixing

- **Serialization format fragility** (audit #7): Rust `serde_store` and Haskell `Store`
  must agree on variant order. Known limitation. No schema check or version tag. Leave
  for now.

---

## Files to Modify

| File | Changes |
|------|---------|
| `rs/src/ffi.rs` | Global static (`LazyLock<RwLock>`), remove `Context`/`ctx_ptr`, `Config` struct + deserialization at init, `size_t` lengths, bounded channels, `catch_unwind` |
| `rs/src/dispatch.rs` | `size_t` length type |
| `hs/src/Types.hs` | Add `Config` type with `Store` instance |
| `hs/src/FFI.hs` | Remove `Ptr ()` params, `CSize` lengths, `tryPutMVar`, config bytes in `h2rInit` signature |
| `hs/src/RustBridge.hs` | Empty `RustEnv`, `withRust` takes `Config`, `asyncRust`/`withAsyncRust`, remove `forkRust`, `callRustTimeout`, `decode`, max msg len check |
| `hs/rs-in-hs.cabal` | `-threaded` on library, add `async` dependency |
| `hs/app/Main.hs` | Replace `forkRust` with `asyncRust`/`forConcurrently_`, pass `Config` to `withRust` |

## Verification

1. `just build-rs && just build-hs` — both compile clean.
2. `just run` — benchmark completes, prints call count and timing.
3. Manual test: verify `withAsyncRust` scope exit cancels child async before deinit.

---

## Appendix: `hint` Integration (Future)

The compiled host application will eventually load and execute user scripts at runtime
via `hint` (the GHC interpreter). Users will not call `callRust`/`castRust` directly —
they will use ergonomic abstractions in the app monad that call these behind the scenes.

The global static design (section 1) is essential for this. Interpreted code runs in the
same process and hits the process-wide Rust static naturally — no pointer injection into
the interpreter environment needed.

Script lifecycle and reload will require cancellation of all work from the previous
execution. The `async`-based structured concurrency (section 2) provides this: each
script evaluation runs inside a scoped async. Re-running cancels the previous scope,
cascading cancellation to all child asyncs. Rust side is unaffected — it just stops
receiving messages from cancelled threads, and late `callResponse` callbacks are
absorbed by `tryPutMVar`.

`RustT` (evolving into the app monad) serves as the capability boundary for interpreted
code — it determines what operations are available to user scripts.

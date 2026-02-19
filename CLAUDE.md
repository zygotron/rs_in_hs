# rs_in_hs

Haskell-to-Rust FFI bridge starter template. Haskell controls the application lifecycle; Rust runs worker and dispatch threads. Messages are serialized with `store`/`serde_store` (binary, zero-copy).

## Layout

```
hs/   — Haskell (Cabal, GHC2021, GHC 9.6)
  src/FFI.hs        — raw foreign imports/exports
  src/RustBridge.hs — RustT monad, public API
  src/Types.hs      — shared types (MessageBody, EventBody, Config)
  app/Main.hs       — benchmark / demo entry point
rs/   — Rust (staticlib, Edition 2024)
  src/ffi.rs        — exported C symbols, global statics
  src/worker.rs     — worker thread (processes calls/casts)
  src/dispatch.rs   — dispatch thread (routes responses back to Haskell)
```

## Build

Requires GHC, Cabal, Rust toolchain, `just`.

```
just build   # cargo build --release, then cabal build -O2
just run     # build then cabal run with +RTS -N16
just clean   # clean both
```

`just setup` writes `hs/cabal.project.local` pointing `extra-lib-dirs` at `rs/target/release/`. Run it once or after a `just clean`.

## Key Design Points

**No pointer crosses the FFI boundary.** Rust state lives in `LazyLock<RwLock<Option<Senders>>>` and `LazyLock<Mutex<Option<ShutdownState>>>` process-wide statics.

**Messaging patterns:**
- `callRust req` — request-response; blocks on `MVar` until Rust calls `callResponse`.
- `callRustTimeout usec req` — same, with `System.Timeout.timeout`.
- `castRust req` — fire-and-forget.
- `subscribe topic` — returns `TQueue event`; Haskell reader thread loops `h2r_next_event`.

**Structured concurrency:** Use `withAsyncRust` (preferred, scoped) or `asyncRust` (caller manages handle). Do not use bare `forkIO` inside `RustT` — threads must not outlive `withRust`.

**Serialization invariant:** Variant/field order in Haskell `data`/`newtype` must match the corresponding Rust `enum`/`struct` exactly. `serde_store` and `store` use the same binary layout; there is no schema check.

**Channel capacity:** All flume channels are bounded at 256. `h2r_call`/`h2r_cast` are declared `safe`, so GHC releases the capability while blocked — backpressure is safe.

**Panic safety:** Every `extern "C"` function body is wrapped in `catch_unwind`. Caught panics print to stderr and `abort()`.

**Config:** `withRust (Config { maxMsgLen = N })` serializes config and passes it to Rust at init. Both sides enforce `maxMsgLen` on each message.

## Extending

To add a new message variant: add to `data MessageBody` in `Types.hs` and `enum MessageBody` in `ffi.rs`, keeping variant order identical. Handle the new variant in `worker.rs`.

To add a new event topic: currently `h2r_subscribe` ignores the topic string and returns subscription 0 (the single pre-created channel). Real per-topic dispatch is a TODO.

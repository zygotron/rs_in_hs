# rs_in_hs

Haskell-to-Rust FFI bridge starter project template using message passing over flume channels.

Haskell controls the application lifecycle. Rust runs worker and dispatch threads, communicating via serialized messages (`store`/`serde_store`). Two messaging patterns: `callRust` (request-response, blocks on MVar) and `castRust` (fire-and-forget).

## Structure

- `hs/` — Haskell side (Cabal). Entry point, FFI imports, `RustBridge` monad transformer.
- `rs/` — Rust side (Cargo, static lib). FFI exports, worker thread, dispatch thread.

## Build & Run

Requires GHC, Cabal, Rust toolchain, and [just](https://github.com/casey/just).

```
just build   # build both Rust and Haskell
just run     # build Rust, then cabal run (rebuilds Haskell if needed)
just clean   # clean both build artifacts
```
## License

[WTFPL](https://www.wtfpl.net/)
# Refactor: store/serde_store → serialise/ciborium

## Context

Replace the binary serialization layer (`store` on Haskell, `serde_store` on Rust) with CBOR-based serialization (`serialise` on Haskell, `ciborium` on Rust). Only serialization/deserialization changes — FFI boundary mechanics (pointer+length passing, StablePtr, channels) remain untouched. Cabal and Cargo configs are already updated.

## CBOR Format Convention

Both sides must agree on an identical CBOR encoding. Since the generic-derived instances of `Serialise` (Haskell) and serde+ciborium (Rust) produce incompatible CBOR by default, **manual instances are required on both sides**.

| Type | CBOR Encoding |
|------|---------------|
| C-like enums (`MessageBody`, `EventBody`) | Single CBOR unsigned integer (variant index: 0, 1, …) |
| `Config` (single-field record/struct) | Single CBOR unsigned integer (the `maxMsgLen`/`max_msg_len` value) |

This is the simplest possible encoding and preserves the current invariant that variant/field order must match across languages. Future variants with payloads can extend to CBOR arrays `[tag, field1, …]`.

### Dependency note

Rust side requires `serde_repr` crate (for `Serialize_repr`/`Deserialize_repr` on C-like enums). Confirm it is in the updated `Cargo.toml`; if not, add it.

---

## Haskell Changes

### 1. `hs/src/Types.hs`

- Remove: `import Data.Store (Store)` and all `instance Store` declarations.
- Add: imports from `Codec.Serialise.Class`, `Codec.CBOR.Encoding`, `Codec.CBOR.Decoding`.
- Write manual `Serialise` instances:

```haskell
instance Serialise MessageBody where
  encode Ping = encodeWord 0
  encode Pong = encodeWord 1
  decode = do
    tag <- decodeWord
    case tag of
      0 -> pure Ping
      1 -> pure Pong
      _ -> fail "unknown MessageBody tag"

instance Serialise EventBody where
  encode CastReceived = encodeWord 0
  encode Heartbeat    = encodeWord 1
  decode = do
    tag <- decodeWord
    case tag of
      0 -> pure CastReceived
      1 -> pure Heartbeat
      _ -> fail "unknown EventBody tag"

instance Serialise Config where
  encode (Config n) = encodeWord32 n
  decode = Config <$> decodeWord32
```

- Update the doc comment (line ~15) to reference CBOR/serialise instead of store.

### 2. `hs/src/RustBridge.hs`

- Remove: `import Data.Store (Store, decode, encode)`.
- Add:
  ```haskell
  import Codec.Serialise (Serialise, serialise, deserialiseOrFail)
  import qualified Data.ByteString.Lazy as LBS
  ```
- Replace all type constraints: `Store req` → `Serialise req`, `Store resp` → `Serialise resp`, `Store event` → `Serialise event`.
- Replace all `encode` calls (lines ~83, 110, 130, 146):
  ```haskell
  -- before:
  let bs = encode req
  -- after:
  let bs = LBS.toStrict (serialise req)
  ```
- Replace all `decode` calls (lines ~115, 137, 178):
  ```haskell
  -- before:
  case decode respBs of
    Left err  -> throwIO err
    Right val -> ...
  -- after:
  case deserialiseOrFail (LBS.fromStrict respBs) of
    Left err  -> throwIO err
    Right val -> ...
  ```
  Note: `deserialiseOrFail` returns `Either DeserialiseFailure a`. The current `decode` returns `Either PeekException a`. Error handling pattern stays the same, only the exception type changes.

### 3. `hs/src/FFI.hs`

- No code changes needed (no serialization logic; only raw pointer passing).
- Update the doc comment (line ~22) to reference CBOR/serialise instead of serde_store.

### 4. `hs/app/Main.hs`

- No changes needed (uses high-level RustBridge API).

---

## Rust Changes

### 1. `rs/src/ffi.rs`

- Remove: `serde_store` import/usage.
- Add: `use serde_repr::{Serialize_repr, Deserialize_repr};`.
- Change derive macros on `MessageBody` and `EventBody`:
  ```rust
  // before:
  #[derive(Serialize, Deserialize, Debug)]
  pub enum MessageBody { Ping, Pong }

  // after:
  #[derive(Serialize_repr, Deserialize_repr, Debug)]
  #[repr(u8)]
  pub enum MessageBody { Ping = 0, Pong = 1 }
  ```
  Same pattern for `EventBody`.
- Change `Config` derive/serialization:
  ```rust
  #[derive(Serialize, Deserialize, Debug)]
  #[serde(transparent)]
  pub struct Config { pub max_msg_len: u32 }
  ```
  `#[serde(transparent)]` makes the struct serialize as its single field (a CBOR unsigned int), matching the Haskell instance.
- Replace `to_bytes()` helper methods:
  ```rust
  // before:
  pub fn to_bytes(&self) -> Vec<u8> { serde_store::to_bytes(self) }
  // after:
  pub fn to_bytes(&self) -> Vec<u8> {
      let mut buf = Vec::new();
      ciborium::into_writer(self, &mut buf).expect("CBOR serialization failed");
      buf
  }
  ```
- Replace all `serde_store::from_bytes(bytes)` calls (lines ~204, 303, 332):
  ```rust
  // before:
  let msg: MessageBody = serde_store::from_bytes(bytes).expect("...");
  // after:
  let msg: MessageBody = ciborium::from_reader(bytes as &[u8]).expect("...");
  ```

### 2. `rs/src/dispatch.rs`

- No direct changes needed — calls `body.to_bytes()` which is updated above.

### 3. `rs/src/worker.rs`

- No changes needed (handles deserialized message bodies, no serialization).

---

## Verification

1. `just build` — confirms both sides compile.
2. `just run` — runs the demo/benchmark in `Main.hs`, exercising call/cast/subscribe paths end-to-end.
3. Manual check: confirm CBOR output size for `Ping` is 1 byte (CBOR unsigned int 0 = `0x00`), `Config { maxMsgLen = 1048576 }` is 5 bytes (CBOR uint32).

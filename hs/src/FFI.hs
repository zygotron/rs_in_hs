{-# LANGUAGE ForeignFunctionInterface #-}

-- | FFI boundary between Haskell and Rust.
--
-- == Exported to C (called by Rust via function pointer)
--
-- * @callResponse@ — Rust's dispatch thread calls this to deliver a response
--   for a pending 'MVar'.
--
-- == Imported from Rust (called by Haskell)
--
-- * @h2r_init@ — Called once at startup. Passes the @callResponse@ function
--   pointer to Rust. Context is stored in Rust-side statics.
-- * @h2r_deinit@ — Shuts down the Rust side, blocking until all threads exit.
-- * @h2r_call@ — Sends a serialized call (request-response) to Rust.
-- * @h2r_cast@ — Sends a serialized cast (fire-and-forget) to Rust.
module FFI
  ( callResponseFunPtr,
    h2rInit,
    h2rDeinit,
    h2rCall,
    h2rCast,
  )
where

import Control.Concurrent.MVar (MVar, tryPutMVar)
import Control.Monad (void)
import Data.ByteString (ByteString, packCStringLen)
import Foreign.C.Types (CSize (..))
import Foreign.Ptr (FunPtr, Ptr, castPtr)
import Foreign.StablePtr (StablePtr, deRefStablePtr, freeStablePtr)

-- ---------------------------------------------------------------------------
-- Exported to C
-- ---------------------------------------------------------------------------

-- | Callback exported to C. Rust's dispatch thread calls this to deliver a
-- call response back to Haskell.
--
-- Receives:
--
-- * @sptr@ — 'StablePtr' to @MVar ByteString@, created per-call by
--   'callRust'. Single-use: freed here immediately after dereferencing.
-- * @ptr@ — pointer to serialized response bytes.
-- * @len@ — byte length.
--
-- Stores raw bytes in the 'MVar'; the caller decodes with the expected type.
foreign export ccall callResponse :: StablePtr (MVar ByteString) -> Ptr () -> CSize -> IO ()

callResponse :: StablePtr (MVar ByteString) -> Ptr () -> CSize -> IO ()
callResponse sptr ptr len = do
  mvar <- deRefStablePtr sptr
  freeStablePtr sptr -- single-use: free immediately
  bs <- packCStringLen (castPtr ptr, fromIntegral len)
  void $ tryPutMVar mvar bs  -- tryPutMVar: absorbs late writes after timeout

-- | Address of the exported @callResponse@ symbol as a 'FunPtr'.
foreign import ccall "&callResponse"
  callResponseFunPtr :: FunPtr (StablePtr (MVar ByteString) -> Ptr () -> CSize -> IO ())

-- ---------------------------------------------------------------------------
-- Imported from Rust
-- ---------------------------------------------------------------------------

-- | Initialize the Rust side. Called once at startup.
--
-- Passes the @callResponse@ function pointer and a serialized 'Config'.
-- Context is stored in Rust-side module-level statics — no pointer returned.
--
-- Declared @safe@ because Rust spawns threads that call back into Haskell.
foreign import ccall safe "h2r_init"
  h2rInit :: FunPtr (StablePtr (MVar ByteString) -> Ptr () -> CSize -> IO ()) -> Ptr () -> CSize -> IO ()

-- | Shut down the Rust side. Blocks until all Rust threads have exited.
--
-- Declared @safe@ because it blocks on thread joins.
foreign import ccall safe "h2r_deinit"
  h2rDeinit :: IO ()

-- | Send a call (request-response) to Rust.
--
-- Declared @safe@ — the dispatch thread will call back into Haskell.
foreign import ccall safe "h2r_call"
  h2rCall :: StablePtr (MVar ByteString) -> Ptr () -> CSize -> IO ()

-- | Send a cast (fire-and-forget) to Rust.
--
-- Declared @safe@ as a conservative default.
foreign import ccall safe "h2r_cast"
  h2rCast :: Ptr () -> CSize -> IO ()

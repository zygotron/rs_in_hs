{-# LANGUAGE GeneralizedNewtypeDeriving #-}

-- | Typesafe interface for Haskell → Rust communication via call\/cast.
--
-- All FFI internals (pointers, StablePtr, serialization) are hidden behind the
-- 'RustT' monad transformer. User code runs inside 'withRust' and interacts
-- with Rust exclusively through 'callRust' and 'castRust'.
module RustBridge
  ( -- * Monad transformer
    RustT,
    Rust,
    withRust,

    -- * Operations
    callRust,
    callRustTimeout,
    castRust,

    -- * Concurrency
    asyncRust,
    withAsyncRust,

    -- * Configuration
    Config (..),

    -- * Message types
    MessageBody (..),
  )
where

import Control.Concurrent.Async (Async, async, withAsync)
import Control.Concurrent.MVar (MVar, newEmptyMVar, takeMVar)
import Control.Exception (finally, throwIO)
import Control.Monad.IO.Class (MonadIO, liftIO)
import Control.Monad (when)
import Control.Monad.Reader (MonadReader, ReaderT (..), ask, asks)
import Data.ByteString (ByteString, useAsCStringLen)
import Data.Store (Store, decode, encode)
import System.Timeout (timeout)
import FFI (callResponseFunPtr, h2rCall, h2rCast, h2rDeinit, h2rInit)
import Foreign.Ptr (castPtr)
import Foreign.StablePtr (newStablePtr)
import Types (Config (..), MessageBody (..))

-- | Internal environment. Holds config values needed for validation.
-- Context is stored in Rust-side statics — no FFI pointer held here.
newtype RustEnv = RustEnv
  { envMaxMsgLen :: Int  -- ^ max serialized message length in bytes
  }

-- | Monad transformer for Rust bridge operations.
-- The constructor is not exported — the only way to run 'RustT' is via 'withRust'.
newtype RustT m a = RustT (ReaderT RustEnv m a)
  deriving (Functor, Applicative, Monad, MonadIO, MonadReader RustEnv)

-- | Convenience alias for 'RustT' over 'IO'.
type Rust = RustT IO

-- | Initialize the Rust bridge, run an action in 'RustT'.
--
-- Calls @h2r_init_rust@ with the @callResponse@ function pointer to set up
-- the Rust worker and dispatch threads. On exit (normal or exception),
-- calls @h2r_deinit@ to shut down all Rust threads and block until they
-- have exited.
--
-- @
-- main = withRust $ do
--   resp <- callRust Ping
--   liftIO $ print resp
-- @
withRust :: Config -> RustT IO a -> IO a
withRust config (RustT action) = do
  let configBs = encode config
  useAsCStringLen configBs $ \(ptr, len) ->
    h2rInit callResponseFunPtr (castPtr ptr) (fromIntegral len)
  let env = RustEnv { envMaxMsgLen = fromIntegral (maxMsgLen config) }
  runReaderT action env `finally` h2rDeinit

-- | Send a call (request-response) to Rust. Blocks until Rust responds.
--
-- Creates an 'MVar', passes its 'StablePtr' to Rust alongside the serialized
-- request. Rust's dispatch thread will call @callResponse@ to fill the 'MVar'.
callRust :: (Store req, Store resp, MonadIO m) => req -> RustT m resp
callRust req = do
  maxLen <- asks envMaxMsgLen
  liftIO $ do
    mvar <- newEmptyMVar :: IO (MVar ByteString)
    sptr <- newStablePtr mvar
    let bs = encode req
    useAsCStringLen bs $ \(ptr, len) -> do
      checkMsgLen "callRust" maxLen len
      h2rCall sptr (castPtr ptr) (fromIntegral len)
    respBs <- takeMVar mvar
    case decode respBs of
      Right v  -> pure v
      Left err -> throwIO err

-- | Like 'callRust' but with an explicit timeout in microseconds.
-- Returns 'Nothing' if the timeout fires before Rust responds.
-- Uses 'decode' (non-partial) so decode failures throw a structured
-- 'PeekException' rather than an opaque error.
callRustTimeout :: (Store req, Store resp, MonadIO m)
                => Int -> req -> RustT m (Maybe resp)
callRustTimeout usec req = do
  maxLen <- asks envMaxMsgLen
  liftIO $ do
    mvar <- newEmptyMVar :: IO (MVar ByteString)
    sptr <- newStablePtr mvar
    let bs = encode req
    useAsCStringLen bs $ \(ptr, len) -> do
      checkMsgLen "callRustTimeout" maxLen len
      h2rCall sptr (castPtr ptr) (fromIntegral len)
    result <- timeout usec (takeMVar mvar)
    case result of
      Nothing -> pure Nothing
      Just respBs -> case decode respBs of
        Right v  -> pure (Just v)
        Left err -> throwIO err

-- | Send a cast (fire-and-forget) to Rust. Returns immediately.
castRust :: (Store req, MonadIO m) => req -> RustT m ()
castRust req = do
  maxLen <- asks envMaxMsgLen
  liftIO $ do
    let bs = encode req
    useAsCStringLen bs $ \(ptr, len) -> do
      checkMsgLen "castRust" maxLen len
      h2rCast (castPtr ptr) (fromIntegral len)

-- | Spawn an async in the RustT environment. Caller manages the handle.
asyncRust :: RustT IO a -> RustT IO (Async a)
asyncRust (RustT action) = do
  env <- ask
  liftIO $ async $ runReaderT action env

-- | Scoped async — cancelled automatically when the scope exits.
-- This is the preferred concurrency primitive. Guarantees the child
-- does not outlive the parent scope, preventing use-after-deinit.
withAsyncRust :: RustT IO a -> (Async a -> RustT IO b) -> RustT IO b
withAsyncRust (RustT action) inner =
  RustT $ ReaderT $ \env ->
    withAsync (runReaderT action env) $
      \a -> case inner a of RustT r -> runReaderT r env

-- | Assert that a serialized message does not exceed the configured maximum.
-- Throws an 'IOError' if the length exceeds 'maxMsgLen'.
checkMsgLen :: String -> Int -> Int -> IO ()
checkMsgLen label maxLen len =
  when (len > maxLen) $
    ioError $ userError $
      label ++ ": message length (" ++ show len
        ++ ") exceeds max_msg_len (" ++ show maxLen ++ ")"

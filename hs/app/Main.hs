module Main where

import Control.Concurrent.STM (atomically, readTQueue)
import Control.Monad (replicateM_)
import Control.Monad.IO.Class (liftIO)
import RustBridge

numCasts :: Int
numCasts = 10

main :: IO ()
main = withRust (Config { maxMsgLen = 1048576 }) $ do
  eventQueue <- subscribe "/demo/casts" :: Rust (TQueue EventBody)

  -- Send some casts — each will produce a CastReceived event on the Rust side.
  replicateM_ numCasts $ castRust Ping

  -- Read back the events.
  liftIO $ replicateM_ numCasts $ do
    event <- atomically $ readTQueue eventQueue
    putStrLn $ "Event: " ++ show event

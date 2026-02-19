module Main where

import Control.Concurrent.Async (wait)
import Control.Concurrent.STM (atomically, readTQueue)
import Control.Monad (replicateM, replicateM_)
import Control.Monad.IO.Class (liftIO)
import Data.Time.Clock (diffUTCTime, getCurrentTime)
import RustBridge

numCalls :: Int
numCalls = 100000

numCasts :: Int
numCasts = 10

main :: IO ()
main = withRust (Config { maxMsgLen = 1048576 }) $ do
  -- Event stream demo: subscribe and send some casts.
  eventQueue <- subscribe "/demo/casts" :: Rust (TQueue EventBody)
  replicateM_ numCasts $ castRust Ping
  liftIO $ replicateM_ numCasts $ do
    event <- atomically $ readTQueue eventQueue
    putStrLn $ "Event: " ++ show event

  -- Call benchmark.
  start <- liftIO getCurrentTime

  handles <- replicateM numCalls $
    asyncRust $ do
      _resp <- callRust Ping :: Rust MessageBody
      pure ()

  liftIO $ mapM_ wait handles

  liftIO $ do
    end <- getCurrentTime
    let elapsed = diffUTCTime end start
    putStrLn $ "Sent:      " ++ show numCalls
    putStrLn $ "Responded: " ++ show numCalls
    putStrLn $ "Time:      " ++ show elapsed

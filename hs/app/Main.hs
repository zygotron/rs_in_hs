module Main where

import Control.Concurrent.Async (wait)
import Control.Monad (replicateM)
import Control.Monad.IO.Class (liftIO)
import Data.Time.Clock (diffUTCTime, getCurrentTime)
import RustBridge

numCalls :: Int
numCalls = 100000

main :: IO ()
main = withRust (Config { maxMsgLen = 1048576 }) $ do
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

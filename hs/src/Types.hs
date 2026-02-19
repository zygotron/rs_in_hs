{-# LANGUAGE DeriveGeneric #-}

module Types
  ( MessageBody (..),
    EventBody (..),
    Config (..),
  )
where

import Data.Store (Store)
import Data.Word (Word32)
import GHC.Generics (Generic)

-- | Variant order must match Rust's @enum MessageBody { Ping, Pong }@ exactly.
-- Serialized as a single @Word8@ variant tag (0 = Ping, 1 = Pong), no payload.
data MessageBody = Ping | Pong
  deriving (Generic, Show)

instance Store MessageBody

-- | Configuration passed from Haskell to Rust at init time.
-- Field order must match Rust's @struct Config@ exactly (serde_store layout).
newtype Config = Config
  { maxMsgLen :: Word32
  }
  deriving (Generic, Show)

instance Store Config

-- | Variant order must match Rust's @enum EventBody { CastReceived, Heartbeat }@ exactly.
data EventBody = CastReceived | Heartbeat
  deriving (Generic, Show)

instance Store EventBody

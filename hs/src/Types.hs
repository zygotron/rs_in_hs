{-# LANGUAGE DeriveGeneric #-}

module Types
  ( MessageBody (..),
    EventBody (..),
    Config (..),
  )
where

import Codec.CBOR.Decoding (decodeWord, decodeWord32)
import Codec.CBOR.Encoding (encodeWord, encodeWord32)
import Codec.Serialise.Class (Serialise (..))
import Data.Word (Word32)
import GHC.Generics (Generic)

-- | Variant order must match Rust's @enum MessageBody { Ping, Pong }@ exactly.
-- Serialized as a single CBOR unsigned integer (0 = Ping, 1 = Pong), no payload.
data MessageBody = Ping | Pong
  deriving (Generic, Show)

instance Serialise MessageBody where
  encode Ping = encodeWord 0
  encode Pong = encodeWord 1
  decode = do
    tag <- decodeWord
    case tag of
      0 -> pure Ping
      1 -> pure Pong
      _ -> fail "unknown MessageBody tag"

-- | Configuration passed from Haskell to Rust at init time.
-- Serialized as a single CBOR unsigned integer (the maxMsgLen value).
newtype Config = Config
  { maxMsgLen :: Word32
  }
  deriving (Generic, Show)

instance Serialise Config where
  encode (Config n) = encodeWord32 n
  decode = Config <$> decodeWord32

-- | Variant order must match Rust's @enum EventBody { CastReceived, Heartbeat }@ exactly.
-- Serialized as a single CBOR unsigned integer (0 = CastReceived, 1 = Heartbeat).
data EventBody = CastReceived | Heartbeat
  deriving (Generic, Show)

instance Serialise EventBody where
  encode CastReceived = encodeWord 0
  encode Heartbeat = encodeWord 1
  decode = do
    tag <- decodeWord
    case tag of
      0 -> pure CastReceived
      1 -> pure Heartbeat
      _ -> fail "unknown EventBody tag"

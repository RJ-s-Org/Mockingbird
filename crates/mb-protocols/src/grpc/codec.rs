//! A tonic codec for [`DynamicMessage`]s of runtime descriptors.

use prost::Message;
use prost_reflect::{DynamicMessage, MessageDescriptor, ReflectMessage};
use tonic::Status;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};

/// Encodes and decodes dynamic messages of two message types: a server
/// uses `new(output, input)`, a client `new(input, output)`.
#[derive(Clone, Debug)]
pub struct DynamicCodec {
    encode: MessageDescriptor,
    decode: MessageDescriptor,
}

impl DynamicCodec {
    /// A codec writing `encode` messages and reading `decode` messages.
    pub fn new(encode: MessageDescriptor, decode: MessageDescriptor) -> Self {
        Self { encode, decode }
    }
}

impl Codec for DynamicCodec {
    type Encode = DynamicMessage;
    type Decode = DynamicMessage;
    type Encoder = DynamicEncoder;
    type Decoder = DynamicDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        DynamicEncoder(self.encode.clone())
    }

    fn decoder(&mut self) -> Self::Decoder {
        DynamicDecoder(self.decode.clone())
    }
}

/// Writes messages of one type.
#[derive(Debug)]
pub struct DynamicEncoder(MessageDescriptor);

impl Encoder for DynamicEncoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn encode(&mut self, item: DynamicMessage, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        if item.descriptor() != self.0 {
            return Err(Status::internal(format!(
                "expected a {} message, got {}",
                self.0.full_name(),
                item.descriptor().full_name()
            )));
        }
        item.encode(dst)
            .map_err(|e| Status::internal(format!("cannot encode message: {e}")))
    }
}

/// Reads messages of one type.
#[derive(Debug)]
pub struct DynamicDecoder(MessageDescriptor);

impl Decoder for DynamicDecoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<DynamicMessage>, Status> {
        DynamicMessage::decode(self.0.clone(), src)
            .map(Some)
            .map_err(|e| Status::invalid_argument(format!("cannot decode message: {e}")))
    }
}

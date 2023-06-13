use crate::interfaces::{DocumentId, Message, NetworkError, RepoId, RepoMessage};
use crate::repo::RepoHandle;
use bytes::{Buf, BytesMut};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::ToSocketAddrs;
use tokio_util::codec::{Decoder, Encoder};

/// Which direction a connection passed to [`Repo::connect`] is going
pub enum ConnDirection {
    Incoming,
    Outgoing,
}

impl RepoHandle {
    /// Connect a tokio io object
    pub async fn connect_tokio_io<Io, Source>(
        &self,
        _source: Source,
        io: Io,
        direction: ConnDirection,
    ) -> Result<(), CodecError>
    where
        Io: AsyncRead + AsyncWrite + Send + 'static,
        Source: ToSocketAddrs,
    {
        let codec = Codec::new();
        let framed = tokio_util::codec::Framed::new(io, codec);
        let (mut sink, mut stream) = framed.split();

        let other_id = match direction {
            ConnDirection::Incoming => {
                if let Some(msg) = stream.next().await {
                    let other_id = match msg {
                        Ok(Message::Join(other_id)) => other_id,
                        _ => return Err(NetworkError::Error.into()),
                    };
                    let msg = Message::Joined(self.get_repo_id().clone());
                    sink.send(msg).await?;
                    other_id
                } else {
                    return Err(NetworkError::Error.into());
                }
            }
            ConnDirection::Outgoing => {
                let msg = Message::Join(self.get_repo_id().clone());
                sink.send(msg).await?;
                if let Some(Ok(Message::Joined(other_id))) = stream.next().await {
                    other_id
                } else {
                    return Err(NetworkError::Error.into());
                }
            }
        };

        let stream = stream.map(|msg| match msg {
            Ok(Message::Repo(repo_msg)) => Ok(repo_msg),
            _ => Err(NetworkError::Error),
        });

        let sink = sink.with(|msg: Result<RepoMessage, NetworkError>| match msg {
            Ok(repo_msg) => futures::future::ready(Ok(Message::Repo(repo_msg))),
            Err(err) => futures::future::ready(Err(err)),
        });

        self.new_remote_repo(other_id, Box::new(stream), Box::new(sink));

        Ok(())
    }
}

/// A simple length prefixed codec over `crate::Message` for use over stream oriented transports
pub(crate) struct Codec;

impl Codec {
    pub(crate) fn new() -> Self {
        Self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error(transparent)]
    Network(#[from] NetworkError),
}

impl From<CodecError> for NetworkError {
    fn from(_err: CodecError) -> Self {
        NetworkError::Error
    }
}

impl Decoder for Codec {
    type Item = Message;

    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < 4 {
            return Ok(None);
        }
        // Read the length prefix
        let mut len_bytes = [0u8; 4];
        len_bytes.copy_from_slice(&src[..4]);
        let len = u32::from_be_bytes(len_bytes) as usize;

        // Check if we have enough data for this message
        if src.len() < len + 4 {
            src.reserve(len + 4 - src.len());
            return Ok(None);
        }

        // Parse the message
        let data = src[4..len + 4].to_vec();
        src.advance(len + 4);
        Message::decode(&data).map(Some).map_err(Into::into)
    }
}

impl Encoder<Message> for Codec {
    type Error = CodecError;

    fn encode(&mut self, msg: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let encoded = msg.encode();
        let len = encoded.len() as u32;
        let len_slice = len.to_be_bytes();
        dst.reserve(4 + len as usize);
        dst.extend_from_slice(&len_slice);
        dst.extend_from_slice(&encoded);
        Ok(())
    }
}

impl Message {
    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let mut decoder = minicbor::Decoder::new(data);
        let mut sender_id: Option<RepoId> = None;
        let mut target_id: Option<RepoId> = None;
        let mut document_id: Option<DocumentId> = None;
        let mut type_name: Option<&str> = None;
        let mut message: Option<Vec<u8>> = None;
        let len = decoder.map()?.ok_or(DecodeError::MissingLen)?;
        for _ in 0..len {
            match decoder.str()? {
                "senderId" => sender_id = Some(decoder.str()?.into()),
                "targetId" => target_id = Some(decoder.str()?.into()),
                "documentId" => document_id = Some(decoder.str()?.into()),
                "type" => type_name = Some(decoder.str()?),
                "message" => message = Some(decoder.bytes()?.to_vec()),
                _ => decoder.skip()?,
            }
        }
        match type_name {
            None => Err(DecodeError::MissingType),
            Some("join") => Ok(Self::Join(sender_id.ok_or(DecodeError::MissingSenderId)?)),
            Some("message") => Ok(Self::Repo(RepoMessage::Sync {
                from_repo_id: sender_id.ok_or(DecodeError::MissingSenderId)?,
                to_repo_id: target_id.ok_or(DecodeError::MissingTargetId)?,
                document_id: document_id.ok_or(DecodeError::MissingDocumentId)?,
                message: message.ok_or(DecodeError::MissingData)?,
            })),
            Some("joined") => Ok(Self::Joined(sender_id.ok_or(DecodeError::MissingSenderId)?)),
            Some(other) => Err(DecodeError::UnknownType(other.to_string())),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let out: Vec<u8> = Vec::new();
        let mut encoder = minicbor::Encoder::new(out);
        match self {
            Self::Join(repo_id) => {
                encoder.map(2).unwrap();
                encoder.str("type").unwrap();
                encoder.str("join").unwrap();
                encoder.str("senderId").unwrap();
                encoder.str(repo_id.0.as_str()).unwrap();
            }
            Self::Repo(RepoMessage::Sync {
                from_repo_id,
                to_repo_id,
                document_id,
                message,
            }) => {
                encoder.map(5).unwrap();
                encoder.str("type").unwrap();
                encoder.str("message").unwrap();
                encoder.str("senderId").unwrap();
                encoder.str(from_repo_id.0.as_str()).unwrap();
                encoder.str("targetId").unwrap();
                encoder.str(to_repo_id.0.as_str()).unwrap();
                encoder.str("documentId").unwrap();
                encoder.str(document_id.0.as_str()).unwrap();
                encoder.str("message").unwrap();
                encoder.bytes(message.as_slice()).unwrap();
            }
            Self::Joined(repo_id) => {
                encoder.map(2).unwrap();
                encoder.str("type").unwrap();
                encoder.str("joined").unwrap();
                encoder.str("senderId").unwrap();
                encoder.str(repo_id.0.as_str()).unwrap();
            }
            _ => todo!(),
        }
        encoder.into_writer()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("missing len")]
    MissingLen,
    #[error("{0}")]
    Minicbor(String),
    #[error("no type field")]
    MissingType,
    #[error("no channel_id field")]
    MissingChannelId,
    #[error("no sender_id field")]
    MissingSenderId,
    #[error("no target_id field")]
    MissingTargetId,
    #[error("no document_id field")]
    MissingDocumentId,
    #[error("no data field")]
    MissingData,
    #[error("no broadcast field")]
    MissingBroadcast,
    #[error("unknown type {0}")]
    UnknownType(String),
}

impl From<minicbor::decode::Error> for DecodeError {
    fn from(e: minicbor::decode::Error) -> Self {
        Self::Minicbor(e.to_string())
    }
}
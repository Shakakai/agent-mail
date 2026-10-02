//! The `agent-mail/2` wire protocol: length-prefixed JSON frames on IROH
//! QUIC streams. One bidirectional stream per exchange: the initiator writes
//! exactly one frame, half-closes, and reads exactly one response frame.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::util::now_secs;

pub const ALPN: &[u8] = b"agent-mail/2";
pub const PROTOCOL_V: u8 = 2;
pub const CAP_MAIL: &str = "mail";
pub const MAX_FRAME_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub thread_id: String,
    pub in_reply_to: Option<String>,
    pub from: String,
    pub to: String,
    pub created_at: u64,
    pub content_type: String,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotAllowed,
    IdentityMismatch,
    TooLarge,
    Malformed,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FrameBody {
    Hello {
        id: String,
        agent: AgentInfo,
        caps: Vec<String>,
        since: u64,
    },
    Send {
        id: String,
        msg: Message,
    },
    Ack {
        id: String,
        of: String,
        received_at: u64,
    },
    Error {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        of: Option<String>,
        code: ErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub v: u8,
    #[serde(flatten)]
    pub body: FrameBody,
}

impl Frame {
    pub fn hello() -> Self {
        Frame {
            v: PROTOCOL_V,
            body: FrameBody::Hello {
                id: ulid::Ulid::generate().to_string(),
                agent: AgentInfo {
                    name: "agent-mail".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                },
                caps: vec![CAP_MAIL.to_string()],
                since: now_secs(),
            },
        }
    }

    pub fn send(msg: &Message) -> Self {
        Frame {
            v: PROTOCOL_V,
            body: FrameBody::Send {
                id: ulid::Ulid::generate().to_string(),
                msg: msg.clone(),
            },
        }
    }

    #[allow(dead_code)]
    pub fn ack(of: &str) -> Self {
        Frame {
            v: PROTOCOL_V,
            body: FrameBody::Ack {
                id: ulid::Ulid::generate().to_string(),
                of: of.to_string(),
                received_at: now_secs(),
            },
        }
    }

    pub fn error(code: ErrorCode, of: Option<String>, message: impl Into<String>) -> Self {
        Frame {
            v: PROTOCOL_V,
            body: FrameBody::Error {
                id: ulid::Ulid::generate().to_string(),
                of,
                code,
                message: message.into(),
            },
        }
    }
}

/// Write one length-prefixed JSON frame. Does not finish the stream —
/// QUIC send streams must be finished by the caller so the peer's reader
/// observes the end of the exchange.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> Result<()> {
    let payload = serde_json::to_vec(frame)?;
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| anyhow::anyhow!("frame exceeds u32 length prefix"))?;
    w.write_all(&len.to_le_bytes()).await?;
    w.write_all(&payload).await?;
    Ok(())
}

/// Read one length-prefixed JSON frame. Enforces the max frame size and
/// the protocol version.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R, max_bytes: usize) -> Result<Frame> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > max_bytes {
        bail!("frame of {len} bytes exceeds limit of {max_bytes}");
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    let frame: Frame = serde_json::from_slice(&payload)?;
    if frame.v != PROTOCOL_V {
        bail!("unsupported protocol version {}", frame.v);
    }
    Ok(frame)
}

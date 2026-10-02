//! The `agent-mail/3` wire protocol: length-prefixed JSON frames on IROH
//! QUIC streams. One bidirectional stream per exchange: the initiator writes
//! exactly one frame, half-closes, and reads exactly one response frame.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::util::now_secs;

pub const ALPN: &[u8] = b"agent-mail/3";
pub const PROTOCOL_V: u8 = 3;
pub const CAP_MAIL: &str = "mail";
pub const CAP_ATTACHMENTS: &str = "attachments";
pub const MAX_FRAME_BYTES: usize = 1 << 20;
/// Hard cap on any single wire frame (a `send` carrying attachments).
/// 64 MiB comfortably covers the 1 MiB default body plus several
/// 20 MiB attachments in base64.
pub const MAX_WIRE_BYTES: usize = 64 << 20;
/// Maximum size of one attachment, raw (before base64), over the wire.
pub const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub content_type: String,
    /// Raw (pre-base64) byte size of the payload.
    pub size: u64,
    /// Payload, base64-encoded. Empty in metadata-only contexts.
    #[serde(default)]
    pub data_base64: String,
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
    #[serde(default)]
    pub attachments: Vec<Attachment>,
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
                caps: vec![CAP_MAIL.to_string(), CAP_ATTACHMENTS.to_string()],
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

#[cfg(test)]
mod tests {
    use super::*;

    fn prefixed(payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(payload);
        buf
    }

    #[tokio::test]
    async fn hello_frame_round_trips() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Frame::hello()).await.unwrap();
        let mut slice: &[u8] = &buf;
        let back = read_frame(&mut slice, MAX_FRAME_BYTES).await.unwrap();
        match back.body {
            FrameBody::Hello { agent, caps, .. } => {
                assert_eq!(agent.name, "agent-mail");
                assert!(caps.iter().any(|c| c == CAP_MAIL));
                assert!(caps.iter().any(|c| c == CAP_ATTACHMENTS));
            }
            other => panic!("expected hello, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_frame_with_attachment_round_trips() {
        let msg = Message {
            id: "m1".into(),
            thread_id: "t1".into(),
            in_reply_to: Some("m0".into()),
            from: "a".into(),
            to: "b".into(),
            created_at: 42,
            content_type: "text/plain".into(),
            body: "hi".into(),
            attachments: vec![Attachment {
                name: "f.bin".into(),
                content_type: "application/octet-stream".into(),
                size: 3,
                data_base64: "AQID".into(),
            }],
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &Frame::send(&msg)).await.unwrap();
        let mut slice: &[u8] = &buf;
        let back = read_frame(&mut slice, MAX_WIRE_BYTES).await.unwrap();
        match back.body {
            FrameBody::Send { msg: m, .. } => {
                assert_eq!(m, msg);
                assert_eq!(m.attachments[0].data_base64, "AQID");
            }
            other => panic!("expected send, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn oversize_length_prefix_rejected_before_alloc() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&((MAX_FRAME_BYTES as u32 + 1).to_le_bytes()));
        let mut slice: &[u8] = &buf;
        assert!(read_frame(&mut slice, MAX_FRAME_BYTES).await.is_err());
    }

    #[tokio::test]
    async fn version_mismatch_rejected() {
        let payload = br#"{"v": 2, "type": "hello", "id": "x", "agent": {"name": "a", "version": "0"}, "caps": [], "since": 0}"#;
        let mut slice: &[u8] = &prefixed(payload);
        assert!(read_frame(&mut slice, MAX_FRAME_BYTES).await.is_err());
    }

    #[tokio::test]
    async fn malformed_json_rejected() {
        let mut slice: &[u8] = &prefixed(b"this is not json");
        assert!(read_frame(&mut slice, MAX_FRAME_BYTES).await.is_err());
    }

    #[tokio::test]
    async fn truncated_stream_errors() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Frame::hello()).await.unwrap();
        buf.truncate(buf.len() - 3); // cut the payload short
        let mut slice: &[u8] = &buf;
        assert!(read_frame(&mut slice, MAX_FRAME_BYTES).await.is_err());
    }

    #[test]
    fn message_serde_defaults_missing_attachments() {
        let json = r#"{"id":"m","thread_id":"t","in_reply_to":null,"from":"a","to":"b",
                       "created_at":1,"content_type":"text/plain","body":"x"}"#;
        let m: Message = serde_json::from_str(json).unwrap();
        assert!(m.attachments.is_empty());
        let wire = serde_json::to_string(&m).unwrap();
        assert!(wire.contains("\"body\":\"x\""));
    }

    #[test]
    fn error_frame_serializes_of_field() {
        let f = Frame::error(ErrorCode::TooLarge, Some("m1".into()), "too big");
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["of"], "m1");
        assert_eq!(v["code"], "too_large");
    }
}

//! Outbound side of the protocol: dial a peer, perform the `hello`
//! handshake, then deliver a `send` frame and await `ack`/`error`.
//! Used by interactive `send`/`reply` and by the daemon's outbox retry loop.

use anyhow::{Result, bail};
use iroh::{Endpoint, EndpointAddr, endpoint::Connection};

use crate::proto::{
    ErrorCode, Frame, FrameBody, Message, read_frame, write_frame, ALPN,
};
use crate::util::de;

/// Dial a peer and perform the symmetric `hello` exchange: both sides open
/// a stream, write a `hello` frame, and read the peer's `hello`. Fails with
/// the peer's `error` frame if the handshake is rejected.
pub async fn handshake(endpoint: &Endpoint, peer: impl Into<EndpointAddr>) -> Result<Connection> {
    let conn = endpoint.connect(peer, ALPN).await.map_err(de)?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(de)?;
    write_frame(&mut send, &Frame::hello()).await?;
    send.finish().map_err(de)?;
    match read_frame(&mut recv, crate::proto::MAX_FRAME_BYTES).await?.body {
        FrameBody::Hello { .. } => Ok(conn),
        FrameBody::Error { code, message, .. } => {
            bail!("peer rejected handshake [{code:?}]: {message}")
        }
        other => bail!("unexpected frame during handshake: {other:?}"),
    }
}

/// Deliver a message on an established (handshaken) connection. Returns the
/// recipient's `received_at` timestamp from the ack.
pub async fn deliver_on(conn: &Connection, msg: &Message) -> Result<u64> {
    let (mut send, mut recv) = conn.open_bi().await.map_err(de)?;
    write_frame(&mut send, &Frame::send(msg)).await?;
    send.finish().map_err(de)?;
    match read_frame(&mut recv, crate::proto::MAX_FRAME_BYTES).await?.body {
        FrameBody::Ack { received_at, .. } => Ok(received_at),
        FrameBody::Error { code, message, .. } => {
            if code == ErrorCode::NotAllowed {
                bail!("peer does not allow our node id (add us to their allowlist)")
            }
            bail!("peer error [{code:?}]: {message}")
        }
        other => bail!("unexpected response frame: {other:?}"),
    }
}

/// Convenience: handshake + deliver. Returns `received_at` on success.
pub async fn deliver(endpoint: &Endpoint, peer: impl Into<EndpointAddr>, msg: &Message) -> Result<u64> {
    let conn = handshake(endpoint, peer).await?;
    deliver_on(&conn, msg).await
}

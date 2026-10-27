//! Framing and message types shared by pairing, sessions and transfers.
//!
//! Every message is a 4-byte big-endian length followed by a postcard body.
//! Bulk payload bytes are the exception: after a fetch header they stream raw
//! until the sender finishes the QUIC stream.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::clip::WireClip;
use crate::error::{CtrlError, Result};

pub const PROTOCOL_VERSION: u16 = 1;
pub const ALPN_SESSION: &[u8] = b"ctrl/1";
pub const ALPN_PAIR: &[u8] = b"ctrl-pair/1";

/// Upper bound on one framed message. A clip with several inline
/// representations at [`crate::clip::INLINE_LIMIT`] fits comfortably.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// The first byte of every bidirectional stream after the control stream.
pub const STREAM_FETCH: u8 = 0x02;

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, body: &[u8]) -> Result<()> {
    if body.len() > MAX_FRAME {
        return Err(CtrlError::Protocol(format!("frame of {} bytes is too large", body.len())));
    }
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(body).await?;
    Ok(())
}

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(CtrlError::Protocol(format!("frame of {len} bytes is too large")));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(body)
}

pub async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> Result<()> {
    write_frame(w, &postcard::to_stdvec(msg)?).await
}

pub async fn read_msg<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> Result<T> {
    Ok(postcard::from_bytes(&read_frame(r).await?)?)
}

/// Messages on a session's control stream, after the Noise handshake.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum Control {
    Clip(WireClip),
    /// The peer changed its own display name.
    Renamed {
        name: String,
    },
    /// The peer unpaired us. We drop its trust record too.
    Unpaired,
}

/// Opens a fetch stream: which payload, and from which byte to resume.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct FetchRequest {
    pub token: String,
    pub offset: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum FetchResponse {
    /// Followed by `total - offset` raw bytes.
    Ok {
        total: u64,
    },
    NotFound,
    BadOffset {
        total: u64,
    },
}

/// Identity details exchanged inside encrypted handshake payloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Hello {
    pub protocol: u16,
    pub device_id: String,
    pub name: String,
}

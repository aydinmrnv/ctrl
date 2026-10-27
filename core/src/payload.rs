//! Lazy payloads: large representations stay on the origin device until a
//! peer pastes them. Each fetch runs on its own QUIC stream and can resume
//! from any byte offset.

use std::collections::{HashMap, VecDeque};
use std::io::SeekFrom;
use std::sync::Mutex;

use quinn::{Connection, RecvStream, SendStream};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::clip::LocalPayload;
use crate::error::{CtrlError, Result};
use crate::wire::{read_msg, write_msg, FetchRequest, FetchResponse, STREAM_FETCH};

pub(crate) const CHUNK: usize = 1024 * 1024;

/// Payloads this device is offering, oldest first. Only the most recent
/// `capacity` stay fetchable.
pub(crate) struct Registry {
    capacity: usize,
    inner: Mutex<(HashMap<String, LocalPayload>, VecDeque<String>)>,
}

impl Registry {
    pub fn new(capacity: usize) -> Self {
        Self { capacity, inner: Mutex::new((HashMap::new(), VecDeque::new())) }
    }

    pub fn insert(&self, token: String, payload: LocalPayload) {
        let mut g = self.inner.lock().unwrap();
        let (map, order) = &mut *g;
        map.insert(token.clone(), payload);
        order.push_back(token);
        while order.len() > self.capacity {
            if let Some(old) = order.pop_front() {
                map.remove(&old);
            }
        }
    }

    pub fn get(&self, token: &str) -> Option<LocalPayload> {
        self.inner.lock().unwrap().0.get(token).cloned()
    }
}

/// Answers one fetch stream. The [`STREAM_FETCH`] tag has already been read.
pub(crate) async fn serve(registry: &Registry, mut send: SendStream, mut recv: RecvStream) -> Result<()> {
    let req: FetchRequest = read_msg(&mut recv).await?;
    let Some(payload) = registry.get(&req.token) else {
        write_msg(&mut send, &FetchResponse::NotFound).await?;
        send.finish()?;
        return Ok(());
    };

    match payload {
        LocalPayload::Bytes(bytes) => {
            let total = bytes.len() as u64;
            if req.offset > total {
                write_msg(&mut send, &FetchResponse::BadOffset { total }).await?;
            } else {
                write_msg(&mut send, &FetchResponse::Ok { total }).await?;
                for chunk in bytes[req.offset as usize..].chunks(CHUNK) {
                    send.write_all(chunk).await?;
                }
            }
        }
        LocalPayload::File(path) => {
            let mut file = match tokio::fs::File::open(&path).await {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("payload file {} is gone: {e}", path.display());
                    write_msg(&mut send, &FetchResponse::NotFound).await?;
                    send.finish()?;
                    return Ok(());
                }
            };
            let total = file.metadata().await?.len();
            if req.offset > total {
                write_msg(&mut send, &FetchResponse::BadOffset { total }).await?;
            } else {
                write_msg(&mut send, &FetchResponse::Ok { total }).await?;
                file.seek(SeekFrom::Start(req.offset)).await?;
                let mut buf = vec![0u8; CHUNK];
                loop {
                    let n = file.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    send.write_all(&buf[..n]).await?;
                }
            }
        }
    }
    send.finish()?;
    Ok(())
}

/// Requests a payload from `offset`. Returns its total size and the stream
/// the remaining bytes arrive on.
pub(crate) async fn open(conn: &Connection, token: &str, offset: u64) -> Result<(u64, RecvStream)> {
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&[STREAM_FETCH]).await?;
    write_msg(&mut send, &FetchRequest { token: token.to_owned(), offset }).await?;
    send.flush().await?;
    send.finish()?;
    match read_msg(&mut recv).await? {
        FetchResponse::Ok { total } => Ok((total, recv)),
        FetchResponse::NotFound => Err(CtrlError::PayloadNotFound),
        FetchResponse::BadOffset { total } => {
            Err(CtrlError::InvalidArgument(format!("offset {offset} is past the end ({total} bytes)")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn registry_drops_the_oldest_payloads() {
        let r = Registry::new(2);
        for t in ["a", "b", "c"] {
            r.insert(t.into(), LocalPayload::Bytes(Arc::new(vec![])));
        }
        assert!(r.get("a").is_none());
        assert!(r.get("b").is_some());
        assert!(r.get("c").is_some());
    }
}

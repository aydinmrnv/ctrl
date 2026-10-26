//! Mutual authentication of an established QUIC connection between paired devices.
//!
//! QUIC's TLS layer encrypts everything, but its certificates are throwaway
//! and never verified. Authentication comes from a Noise `IK` handshake with
//! the long-term static keys exchanged at pairing. The prologue includes a TLS
//! exporter secret of the connection. That binds the Noise result to this
//! exact TLS session, so once the handshake succeeds every stream on the
//! connection is known to come from the paired peer. Payloads therefore rely
//! on QUIC's own encryption and pay no second encryption cost.

use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::{CtrlError, Result};
use crate::trust::{Identity, PeerRecord, NOISE_KEY_PARAMS};
use crate::wire::{read_frame, write_frame, Hello, PROTOCOL_VERSION};

fn prologue(binding: &[u8]) -> Vec<u8> {
    [b"ctrl/1".as_slice(), binding].concat()
}

fn hello(identity: &Identity) -> Result<Vec<u8>> {
    Ok(postcard::to_stdvec(&Hello {
        protocol: PROTOCOL_VERSION,
        device_id: identity.device_id.clone(),
        name: identity.name.clone(),
    })?)
}

/// An empty frame tells the initiator we refused, so it doesn't wait for a timeout.
async fn reject<W: AsyncWrite + Unpin>(w: &mut W) {
    let _ = write_frame(w, &[]).await;
}

fn handshake_err(e: snow::Error) -> CtrlError {
    CtrlError::Protocol(format!("session handshake error: {e}"))
}

/// Dials a known peer. Returns the peer's [`Hello`] once both sides are authenticated.
pub(crate) async fn initiate<R, W>(
    r: &mut R,
    w: &mut W,
    identity: &Identity,
    peer: &PeerRecord,
    binding: &[u8],
) -> Result<Hello>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let prologue = prologue(binding);
    let mut hs = snow::Builder::new(NOISE_KEY_PARAMS.parse().unwrap())
        .local_private_key(&identity.private_key)
        .and_then(|b| b.remote_public_key(&peer.public_key))
        .and_then(|b| b.prologue(&prologue))
        .and_then(|b| b.build_initiator())
        .map_err(handshake_err)?;
    let mut buf = vec![0u8; 65535];

    // -> e, es, s, ss, Hello
    let n = hs.write_message(&hello(identity)?, &mut buf).map_err(handshake_err)?;
    write_frame(w, &buf[..n]).await?;

    // <- e, ee, se, Hello. The responder sends an empty frame when it doesn't
    // trust us, which fails here as well.
    let m2 = read_frame(r).await?;
    let n = hs.read_message(&m2, &mut buf).map_err(|_| CtrlError::UnknownPeer)?;
    let theirs: Hello = postcard::from_bytes(&buf[..n])?;
    if theirs.device_id != peer.device_id {
        return Err(CtrlError::UnknownPeer);
    }
    Ok(theirs)
}

/// Accepts a dial. `lookup` maps the initiator's static key to a trusted peer.
pub(crate) async fn respond<R, W>(
    r: &mut R,
    w: &mut W,
    identity: &Identity,
    binding: &[u8],
    lookup: impl Fn(&[u8]) -> Option<PeerRecord>,
) -> Result<(PeerRecord, Hello)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let prologue = prologue(binding);
    let mut hs = snow::Builder::new(NOISE_KEY_PARAMS.parse().unwrap())
        .local_private_key(&identity.private_key)
        .and_then(|b| b.prologue(&prologue))
        .and_then(|b| b.build_responder())
        .map_err(handshake_err)?;
    let mut buf = vec![0u8; 65535];

    let m1 = read_frame(r).await?;
    let n = match hs.read_message(&m1, &mut buf) {
        Ok(n) => n,
        Err(_) => {
            reject(w).await;
            return Err(CtrlError::UnknownPeer);
        }
    };
    let remote = hs.get_remote_static().map(<[u8]>::to_vec).unwrap_or_default();
    let theirs: Hello = postcard::from_bytes(&buf[..n])?;
    let Some(peer) = lookup(&remote).filter(|p| p.device_id == theirs.device_id) else {
        reject(w).await;
        return Err(CtrlError::UnknownPeer);
    };

    let n = hs.write_message(&hello(identity)?, &mut buf).map_err(handshake_err)?;
    write_frame(w, &buf[..n]).await?;
    Ok((peer, theirs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::tests::identity;
    use tokio::io::{duplex, split, AsyncWriteExt};

    fn record(id: &Identity) -> PeerRecord {
        PeerRecord {
            device_id: id.device_id.clone(),
            name: id.name.clone(),
            alias: None,
            public_key: id.public_key.clone(),
            paired_at_ms: 0,
            last_address: None,
        }
    }

    async fn run(
        initiator: Identity,
        responder: Identity,
        responder_as_seen: PeerRecord,
        trusted: Vec<PeerRecord>,
        bindings: (&[u8], &[u8]),
    ) -> (Result<Hello>, Result<(PeerRecord, Hello)>) {
        let (a, b) = duplex(1 << 16);
        let ((mut ar, mut aw), (mut br, mut bw)) = (split(a), split(b));
        let (ib, rb) = (bindings.0.to_vec(), bindings.1.to_vec());
        let i = tokio::spawn(async move { initiate(&mut ar, &mut aw, &initiator, &responder_as_seen, &ib).await });
        let r = tokio::spawn(async move {
            let r = respond(&mut br, &mut bw, &responder, &rb, |k| trusted.iter().find(|p| p.public_key == k).cloned())
                .await;
            let _ = bw.shutdown().await;
            r
        });
        (i.await.unwrap(), r.await.unwrap())
    }

    #[tokio::test]
    async fn paired_devices_authenticate() {
        let (mac, pc) = (identity("Mac"), identity("PC"));
        let (i, r) = run(mac.clone(), pc.clone(), record(&pc), vec![record(&mac)], (b"x", b"x")).await;
        assert_eq!(i.unwrap().device_id, pc.device_id);
        assert_eq!(r.unwrap().0.device_id, mac.device_id);
    }

    #[tokio::test]
    async fn unknown_peer_is_rejected() {
        let (mac, pc, stranger) = (identity("Mac"), identity("PC"), identity("Stranger"));
        // The stranger knows the PC's public key (it is not secret), but the PC never paired with it.
        let (i, r) = run(stranger, pc.clone(), record(&pc), vec![record(&mac)], (b"x", b"x")).await;
        assert!(matches!(i, Err(CtrlError::UnknownPeer)), "{i:?}");
        assert!(matches!(r, Err(CtrlError::UnknownPeer)), "{r:?}");
    }

    #[tokio::test]
    async fn trusted_key_with_a_different_device_id_is_rejected() {
        let (mac, pc) = (identity("Mac"), identity("PC"));
        let mut wrong = record(&mac);
        wrong.device_id = "someone-else".into();
        let (_, r) = run(mac, pc.clone(), record(&pc), vec![wrong], (b"x", b"x")).await;
        assert!(matches!(r, Err(CtrlError::UnknownPeer)), "{r:?}");
    }

    #[tokio::test]
    async fn impostor_responder_is_rejected() {
        // We dial what we think is the PC, but a different device answers.
        let (mac, pc, impostor) = (identity("Mac"), identity("PC"), identity("Impostor"));
        let (i, r) = run(mac.clone(), impostor, record(&pc), vec![record(&mac)], (b"x", b"x")).await;
        assert!(i.is_err());
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn handshake_is_bound_to_the_connection() {
        let (mac, pc) = (identity("Mac"), identity("PC"));
        let (i, r) = run(mac.clone(), pc.clone(), record(&pc), vec![record(&mac)], (b"leg one", b"leg two")).await;
        assert!(i.is_err());
        assert!(matches!(r, Err(CtrlError::UnknownPeer)), "{r:?}");
    }

    #[tokio::test]
    async fn replayed_first_message_is_useless_on_another_connection() {
        let (mac, pc) = (identity("Mac"), identity("PC"));
        // Capture the initiator's first message.
        let (a, b) = duplex(1 << 16);
        let ((mut ar, mut aw), (mut br, _bw)) = (split(a), split(b));
        let pc_rec = record(&pc);
        let mac2 = mac.clone();
        let dial = tokio::spawn(async move { initiate(&mut ar, &mut aw, &mac2, &pc_rec, b"original").await });
        let captured = read_frame(&mut br).await.unwrap();
        dial.abort();

        // Replay it on a fresh connection.
        let (c, d) = duplex(1 << 16);
        let ((_cr, mut cw), (mut dr, mut dw)) = (split(c), split(d));
        write_frame(&mut cw, &captured).await.unwrap();
        let trusted = [record(&mac)];
        let r =
            respond(&mut dr, &mut dw, &pc, b"replayed", |k| trusted.iter().find(|p| p.public_key == k).cloned()).await;
        assert!(matches!(r, Err(CtrlError::UnknownPeer)), "{r:?}");
    }
}

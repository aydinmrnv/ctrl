//! One-time pairing with a 6-digit code.
//!
//! The *responder* shows the code and the *initiator* types it. The protocol
//! runs over a QUIC stream whose TLS certificates are not trusted. Everything
//! that matters is authenticated by the code instead:
//!
//! 1. **SPAKE2** (Ed25519 group) turns the low-entropy code into a strong
//!    shared key `K`. Each attempt gives an active attacker one guess, and a
//!    passive one learns nothing.
//! 2. `psk = HKDF-SHA256(K, salt = binding)`, where `binding` is a TLS exporter
//!    secret of this QUIC connection.
//! 3. **Noise `XXpsk0`** keyed with `psk`, with the binding also in the
//!    prologue, exchanges and authenticates both long-term X25519 static keys
//!    plus each side's [`Hello`]. Because the PSK is mixed in before the first
//!    message, a wrong code fails at the very first message on both sides.
//! 4. The responder sends an encrypted confirmation, so the initiator knows the
//!    responder stored it as trusted.
//!
//! The channel binding stops a man in the middle from relaying a pairing
//! between two different TLS connections. The fresh SPAKE2 randomness on each
//! side stops replays.

use hkdf::Hkdf;
use rand::Rng;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity as SpakeId, Password, Spake2};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::{CtrlError, Result};
use crate::trust::Identity;
use crate::wire::{read_frame, write_frame, Hello, PROTOCOL_VERSION};

const PAIR_PARAMS: &str = "Noise_XXpsk0_25519_ChaChaPoly_SHA256";
const SPAKE_ID_INITIATOR: &[u8] = b"ctrl-pair-initiator";
const SPAKE_ID_RESPONDER: &[u8] = b"ctrl-pair-responder";
const PSK_INFO: &[u8] = b"ctrl pair psk v1";
const CONFIRM: &[u8] = b"paired";
const MAX_NAME: usize = 64;

/// What each side learns about the other from a successful pairing.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PairOutcome {
    pub hello: Hello,
    pub public_key: Vec<u8>,
}

/// A uniformly random 6-digit code.
pub fn generate_code() -> String {
    format!("{:06}", rand::rngs::OsRng.gen_range(0..1_000_000u32))
}

/// Accepts "123456", "123 456" and "123-456".
pub fn normalize_code(input: &str) -> Result<String> {
    let digits: String = input.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    if digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_digit()) {
        Ok(digits)
    } else {
        Err(CtrlError::InvalidArgument("the pairing code is six digits".into()))
    }
}

fn derive_psk(spake_key: &[u8], binding: &[u8]) -> [u8; 32] {
    let mut psk = [0u8; 32];
    Hkdf::<Sha256>::new(Some(binding), spake_key)
        .expand(PSK_INFO, &mut psk)
        .expect("32 bytes is a valid HKDF-SHA256 length");
    psk
}

fn prologue(binding: &[u8]) -> Vec<u8> {
    [b"ctrl-pair/1".as_slice(), binding].concat()
}

fn noise_err(e: snow::Error) -> CtrlError {
    CtrlError::PairingFailed(format!("handshake error: {e}"))
}

fn hello_for(identity: &Identity) -> Result<Vec<u8>> {
    Ok(postcard::to_stdvec(&Hello {
        protocol: PROTOCOL_VERSION,
        device_id: identity.device_id.clone(),
        name: identity.name.clone(),
    })?)
}

fn parse_hello(bytes: &[u8], identity: &Identity, remote_key: &[u8]) -> Result<Hello> {
    let mut hello: Hello =
        postcard::from_bytes(bytes).map_err(|_| CtrlError::PairingFailed("malformed hello".into()))?;
    if hello.protocol != PROTOCOL_VERSION {
        return Err(CtrlError::PairingFailed(format!("unsupported protocol version {}", hello.protocol)));
    }
    let id_ok = !hello.device_id.is_empty()
        && hello.device_id.len() <= 64
        && hello.device_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if !id_ok {
        return Err(CtrlError::PairingFailed("invalid device id".into()));
    }
    if hello.device_id == identity.device_id || remote_key == identity.public_key.as_slice() {
        return Err(CtrlError::PairingFailed("a device cannot pair with itself".into()));
    }
    if hello.name.chars().count() > MAX_NAME {
        hello.name = hello.name.chars().take(MAX_NAME).collect();
    }
    Ok(hello)
}

/// The side that typed the code.
pub(crate) async fn initiate<R, W>(
    r: &mut R,
    w: &mut W,
    code: &str,
    identity: &Identity,
    binding: &[u8],
) -> Result<PairOutcome>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (spake, msg) = Spake2::<Ed25519Group>::start_a(
        &Password::new(code.as_bytes()),
        &SpakeId::new(SPAKE_ID_INITIATOR),
        &SpakeId::new(SPAKE_ID_RESPONDER),
    );
    write_frame(w, &msg).await?;
    let theirs = read_frame(r).await?;
    let key = spake.finish(&theirs).map_err(|_| CtrlError::PairingFailed("bad SPAKE2 message".into()))?;
    let psk = derive_psk(&key, binding);
    let prologue = prologue(binding);

    let mut hs = snow::Builder::new(PAIR_PARAMS.parse().unwrap())
        .local_private_key(&identity.private_key)
        .and_then(|b| b.psk(0, &psk))
        .and_then(|b| b.prologue(&prologue))
        .and_then(|b| b.build_initiator())
        .map_err(noise_err)?;
    let mut buf = vec![0u8; 65535];

    // -> psk, e
    let n = hs.write_message(&[], &mut buf).map_err(noise_err)?;
    write_frame(w, &buf[..n]).await?;

    // <- e, ee, s, es, Hello. A wrong code makes this fail to decrypt, and so
    // does the empty frame a responder sends when it rejected our first message.
    let m2 = read_frame(r).await?;
    let n = hs.read_message(&m2, &mut buf).map_err(|_| CtrlError::WrongCode)?;
    let remote_key = hs.get_remote_static().ok_or_else(|| noise_err(snow::Error::Input))?.to_vec();
    let hello = parse_hello(&buf[..n], identity, &remote_key)?;

    // -> s, se, Hello
    let n = hs.write_message(&hello_for(identity)?, &mut buf).map_err(noise_err)?;
    write_frame(w, &buf[..n]).await?;

    let mut transport = hs.into_transport_mode().map_err(noise_err)?;
    let confirm = read_frame(r).await.map_err(|_| CtrlError::PairingFailed("the other device cancelled".into()))?;
    let n = transport
        .read_message(&confirm, &mut buf)
        .map_err(|_| CtrlError::PairingFailed("the other device rejected the pairing".into()))?;
    if &buf[..n] != CONFIRM {
        return Err(CtrlError::PairingFailed("unexpected confirmation".into()));
    }
    Ok(PairOutcome { hello, public_key: remote_key })
}

/// The side that showed the code. `accept` runs once the initiator is
/// authenticated and before we confirm, so the peer is already trusted by the
/// time the initiator dials its first session.
pub(crate) async fn respond<R, W>(
    r: &mut R,
    w: &mut W,
    code: &str,
    identity: &Identity,
    binding: &[u8],
    accept: impl FnOnce(&PairOutcome) -> Result<()>,
) -> Result<PairOutcome>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let theirs = read_frame(r).await?;
    let (spake, msg) = Spake2::<Ed25519Group>::start_b(
        &Password::new(code.as_bytes()),
        &SpakeId::new(SPAKE_ID_INITIATOR),
        &SpakeId::new(SPAKE_ID_RESPONDER),
    );
    write_frame(w, &msg).await?;
    let key = spake.finish(&theirs).map_err(|_| CtrlError::PairingFailed("bad SPAKE2 message".into()))?;
    let psk = derive_psk(&key, binding);
    let prologue = prologue(binding);

    let mut hs = snow::Builder::new(PAIR_PARAMS.parse().unwrap())
        .local_private_key(&identity.private_key)
        .and_then(|b| b.psk(0, &psk))
        .and_then(|b| b.prologue(&prologue))
        .and_then(|b| b.build_responder())
        .map_err(noise_err)?;
    let mut buf = vec![0u8; 65535];

    let m1 = read_frame(r).await?;
    if hs.read_message(&m1, &mut buf).is_err() {
        // Tell the initiator explicitly so it doesn't wait for a timeout.
        let _ = write_frame(w, &[]).await;
        return Err(CtrlError::WrongCode);
    }

    let n = hs.write_message(&hello_for(identity)?, &mut buf).map_err(noise_err)?;
    write_frame(w, &buf[..n]).await?;

    let m3 = read_frame(r).await?;
    let n = hs.read_message(&m3, &mut buf).map_err(|_| CtrlError::WrongCode)?;
    let remote_key = hs.get_remote_static().ok_or_else(|| noise_err(snow::Error::Input))?.to_vec();
    let hello = parse_hello(&buf[..n], identity, &remote_key)?;
    let outcome = PairOutcome { hello, public_key: remote_key };
    accept(&outcome)?;

    let mut transport = hs.into_transport_mode().map_err(noise_err)?;
    let n = transport.write_message(CONFIRM, &mut buf).map_err(noise_err)?;
    write_frame(w, &buf[..n]).await?;
    Ok(outcome)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{duplex, split, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};

    pub(crate) fn identity(name: &str) -> Identity {
        let kp = snow::Builder::new(crate::trust::NOISE_KEY_PARAMS.parse().unwrap()).generate_keypair().unwrap();
        Identity {
            device_id: uuid::Uuid::new_v4().simple().to_string(),
            name: name.into(),
            private_key: kp.private,
            public_key: kp.public,
        }
    }

    type Half = (ReadHalf<DuplexStream>, WriteHalf<DuplexStream>);

    fn pipe() -> (Half, Half) {
        let (a, b) = duplex(1 << 20);
        (split(a), split(b))
    }

    async fn run(
        initiator_code: &str,
        responder_code: &str,
        init_binding: &[u8],
        resp_binding: &[u8],
    ) -> (Result<PairOutcome>, Result<PairOutcome>) {
        let (mac, pc) = (identity("Mac"), identity("PC"));
        let ((mut ar, mut aw), (mut br, mut bw)) = pipe();
        let (ic, rc) = (initiator_code.to_owned(), responder_code.to_owned());
        let (ib, rb) = (init_binding.to_vec(), resp_binding.to_vec());
        let init = tokio::spawn(async move { initiate(&mut ar, &mut aw, &ic, &mac, &ib).await });
        let resp = tokio::spawn(async move {
            let r = respond(&mut br, &mut bw, &rc, &pc, &rb, |_| Ok(())).await;
            // Closing our end lets a waiting initiator fail instead of hanging.
            let _ = bw.shutdown().await;
            r
        });
        (init.await.unwrap(), resp.await.unwrap())
    }

    #[tokio::test]
    async fn matching_codes_exchange_static_keys() {
        let (i, r) = run("123456", "123456", b"binding", b"binding").await;
        let (i, r) = (i.unwrap(), r.unwrap());
        assert_eq!(i.hello.name, "PC");
        assert_eq!(r.hello.name, "Mac");
        assert_eq!(i.public_key.len(), 32);
        assert_ne!(i.public_key, r.public_key);
    }

    #[tokio::test]
    async fn wrong_code_fails_on_both_sides() {
        let (i, r) = run("123456", "654321", b"binding", b"binding").await;
        assert!(matches!(i, Err(CtrlError::WrongCode)), "{i:?}");
        assert!(matches!(r, Err(CtrlError::WrongCode)), "{r:?}");
    }

    #[tokio::test]
    async fn mismatched_channel_binding_fails() {
        // A relaying man in the middle sees a different TLS exporter on each leg.
        let (i, r) = run("123456", "123456", b"leg one", b"leg two").await;
        assert!(i.is_err());
        assert!(matches!(r, Err(CtrlError::WrongCode)), "{r:?}");
    }

    /// Records every frame the initiator sends.
    struct Tap<W> {
        inner: W,
        log: Arc<Mutex<Vec<u8>>>,
    }

    impl<W: AsyncWrite + Unpin> AsyncWrite for Tap<W> {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let p = std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
            if let std::task::Poll::Ready(Ok(n)) = p {
                self.log.lock().unwrap().extend_from_slice(&buf[..n]);
            }
            p
        }
        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.inner).poll_flush(cx)
        }
        fn poll_shutdown(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    fn frames(mut bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while bytes.len() >= 4 {
            let len = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
            out.push(bytes[4..4 + len].to_vec());
            bytes = &bytes[4 + len..];
        }
        out
    }

    async fn replay(frames: Vec<Vec<u8>>, code: &str, binding: &[u8]) -> Result<PairOutcome> {
        let ((mut ar, mut aw), (mut br, mut bw)) = pipe();
        let attacker = tokio::spawn(async move {
            for f in frames {
                if write_frame(&mut aw, &f).await.is_err() {
                    break;
                }
                // Read and discard whatever the victim answers.
                if read_frame(&mut ar).await.is_err() {
                    break;
                }
            }
        });
        let victim = identity("Victim");
        let r = respond(&mut br, &mut bw, code, &victim, binding, |_| Ok(())).await;
        drop((br, bw));
        attacker.abort();
        r
    }

    #[tokio::test]
    async fn replayed_pairing_is_rejected() {
        // Record a legitimate pairing.
        let (mac, pc) = (identity("Mac"), identity("PC"));
        let ((mut ar, aw), (mut br, mut bw)) = pipe();
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut tap = Tap { inner: aw, log: log.clone() };
        let init = tokio::spawn(async move { initiate(&mut ar, &mut tap, "246810", &mac, b"b1").await });
        respond(&mut br, &mut bw, "246810", &pc, b"b1", |_| Ok(())).await.unwrap();
        init.await.unwrap().unwrap();
        let recorded = frames(&log.lock().unwrap());
        assert_eq!(recorded.len(), 3, "spake2, noise 1, noise 3");

        // Replaying on a new connection fails: the binding differs.
        let r = replay(recorded.clone(), "246810", b"b2").await;
        assert!(matches!(r, Err(CtrlError::WrongCode)), "{r:?}");
        // Even with the same binding and the same code, the responder's fresh
        // SPAKE2 secret makes the recorded Noise messages useless.
        let r = replay(recorded, "246810", b"b1").await;
        assert!(matches!(r, Err(CtrlError::WrongCode)), "{r:?}");
    }

    #[test]
    fn codes() {
        for _ in 0..1000 {
            let c = generate_code();
            assert_eq!(normalize_code(&c).unwrap(), c);
        }
        assert_eq!(normalize_code("123 456").unwrap(), "123456");
        assert_eq!(normalize_code("123-456").unwrap(), "123456");
        assert!(normalize_code("12345").is_err());
        assert!(normalize_code("12345a").is_err());
        assert!(normalize_code("1234567").is_err());
    }
}

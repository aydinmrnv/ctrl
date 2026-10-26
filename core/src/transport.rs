//! The QUIC endpoint.
//!
//! TLS here provides encryption only. Certificates are generated at startup
//! and never verified, because authentication happens one layer up in
//! [`crate::session`] and [`crate::pairing`], bound to each connection through
//! [`binding`].

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{ClientConfig, Connection, Endpoint, EndpointConfig, IdleTimeout, ServerConfig, TransportConfig, VarInt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

use crate::error::{CtrlError, Result};
use crate::wire::{ALPN_PAIR, ALPN_SESSION};

const EXPORTER_LABEL: &[u8] = b"EXPORTER-ctrl-channel-binding";
/// Large enough for a gigabit link with tens of milliseconds of delay.
const STREAM_WINDOW: u32 = 16 * 1024 * 1024;
const CONNECTION_WINDOW: u32 = 64 * 1024 * 1024;
const SOCKET_BUFFER: usize = 8 * 1024 * 1024;

/// Application error codes used when closing connections.
pub(crate) mod close {
    use quinn::VarInt;
    pub const NORMAL: VarInt = VarInt::from_u32(0);
    pub const DUPLICATE: VarInt = VarInt::from_u32(1);
    pub const REJECTED: VarInt = VarInt::from_u32(2);
    pub const UNPAIRED: VarInt = VarInt::from_u32(3);
}

pub(crate) struct Transport {
    pub endpoint: Endpoint,
    session_client: ClientConfig,
    pair_client: ClientConfig,
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn tls_err(e: impl std::fmt::Display) -> CtrlError {
    CtrlError::Network(format!("TLS setup failed: {e}"))
}

fn transport_config() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.stream_receive_window(VarInt::from_u32(STREAM_WINDOW))
        .receive_window(VarInt::from_u32(CONNECTION_WINDOW))
        .send_window(CONNECTION_WINDOW as u64)
        .max_concurrent_bidi_streams(VarInt::from_u32(64))
        .max_concurrent_uni_streams(VarInt::from_u32(0))
        .keep_alive_interval(Some(Duration::from_secs(5)))
        .max_idle_timeout(Some(IdleTimeout::from(VarInt::from_u32(20_000))));
    Arc::new(t)
}

fn client_config(alpn: &[u8]) -> Result<ClientConfig> {
    let provider = provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(tls_err)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![alpn.to_vec()];
    let mut cfg = ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls).map_err(tls_err)?));
    cfg.transport_config(transport_config());
    Ok(cfg)
}

fn server_config() -> Result<ServerConfig> {
    let cert = rcgen::generate_simple_self_signed(vec!["ctrl".to_owned()]).map_err(tls_err)?;
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(tls_err)?
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key)
        .map_err(tls_err)?;
    tls.alpn_protocols = vec![ALPN_SESSION.to_vec(), ALPN_PAIR.to_vec()];
    let mut cfg = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls).map_err(tls_err)?));
    cfg.transport_config(transport_config());
    Ok(cfg)
}

fn udp_socket(addr: SocketAddr) -> Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    // Bigger kernel buffers avoid drops at gigabit rates. macOS refuses
    // anything over its limit outright rather than clamping, so step down
    // until a size is accepted.
    for size in [SOCKET_BUFFER, SOCKET_BUFFER / 2, SOCKET_BUFFER / 4] {
        if socket.set_recv_buffer_size(size).is_ok() {
            break;
        }
    }
    for size in [SOCKET_BUFFER, SOCKET_BUFFER / 2, SOCKET_BUFFER / 4] {
        if socket.set_send_buffer_size(size).is_ok() {
            break;
        }
    }
    socket.bind(&addr.into())?;
    Ok(socket.into())
}

impl Transport {
    /// Must be called from inside the tokio runtime.
    pub fn bind(addr: SocketAddr) -> Result<Self> {
        let endpoint = Endpoint::new(
            EndpointConfig::default(),
            Some(server_config()?),
            udp_socket(addr)?,
            Arc::new(quinn::TokioRuntime),
        )?;
        Ok(Self { endpoint, session_client: client_config(ALPN_SESSION)?, pair_client: client_config(ALPN_PAIR)? })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.endpoint.local_addr()?)
    }

    pub async fn connect(&self, addr: SocketAddr, pairing: bool) -> Result<Connection> {
        let cfg = if pairing { &self.pair_client } else { &self.session_client };
        let connecting = self.endpoint.connect_with(cfg.clone(), addr, "ctrl")?;
        tokio::time::timeout(Duration::from_secs(5), connecting)
            .await
            .map_err(|_| CtrlError::Unreachable(addr.to_string()))?
            .map_err(CtrlError::from)
    }
}

/// The ALPN the connection negotiated.
pub(crate) fn alpn(conn: &Connection) -> Option<Vec<u8>> {
    conn.handshake_data()?.downcast::<quinn::crypto::rustls::HandshakeData>().ok()?.protocol
}

/// A secret unique to this TLS session, known to both endpoints and to no one
/// else, not even a man in the middle who terminates TLS on each side.
pub(crate) fn binding(conn: &Connection) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    conn.export_keying_material(&mut out, EXPORTER_LABEL, b"")
        .map_err(|_| CtrlError::Network("could not derive channel binding".into()))?;
    Ok(out)
}

/// Accepts any server certificate. Safe only because every connection is
/// authenticated afterwards by a Noise handshake bound to [`binding`].
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

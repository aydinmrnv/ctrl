//! [`CtrlNode`], the single object a shell talks to.
//!
//! Every exported method is synchronous, so the API works the same way from
//! Swift and from C#. Methods that wait on the network block the calling
//! thread, so call them off the UI thread. Events arrive on the shell's
//! listener from one dedicated callback thread, in order, and never from
//! inside the core's runtime. Calling back into the node from a listener is
//! therefore safe.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use quinn::{Connection, RecvStream, SendStream};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::clip::{now_ms, prepare, Clip, LazyHandle, LocalPayload, OutgoingClip, INLINE_LIMIT};
use crate::discovery::{Discovery, Seen, Sighting};
use crate::error::{CtrlError, Result};
use crate::history::{History, Retention};
use crate::pairing::{self, generate_code, normalize_code};
use crate::payload::{self, Registry, CHUNK};
use crate::session;
use crate::transport::{self, close, Transport};
use crate::trust::{fingerprint, PeerRecord, SecretStore, TrustStore};
use crate::wire::{read_msg, write_msg, Control, Hello, ALPN_PAIR, ALPN_SESSION, STREAM_FETCH};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const PAIRING_TIMEOUT: Duration = Duration::from_secs(30);
const PAIRING_WINDOW: Duration = Duration::from_secs(120);
const PAIRING_ATTEMPTS: u8 = 3;
const RECONNECT_INTERVAL: Duration = Duration::from_secs(15);
/// Two connections to the same peer opened within this window are a
/// simultaneous dial, and a fixed rule picks one. After it, a new connection
/// means the peer lost the old one, so the new one wins.
const DIAL_RACE_WINDOW: Duration = Duration::from_secs(2);
/// Payloads stay fetchable for this many recent copies.
const REGISTRY_CAPACITY: usize = 64;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, uniffi::Record)]
pub struct NodeConfig {
    /// Used the first time this device runs. After that the stored name
    /// wins, and you change it with [`CtrlNode::set_device_name`].
    pub device_name: String,
    /// Where history lives. The core creates it if needed.
    pub data_dir: String,
    /// The UDP address to listen on, such as `0.0.0.0:0`. Port 0 picks one.
    pub listen_address: Option<String>,
    /// Advertise and browse over mDNS. Turn it off in tests or when only
    /// explicit addresses are used.
    pub discovery: bool,
    pub retention: Retention,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct DeviceInfo {
    pub device_id: String,
    /// The paired device's name. Unpaired devices don't broadcast a name.
    pub name: Option<String>,
    pub fingerprint: String,
    pub trusted: bool,
    pub connected: bool,
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum DeviceEvent {
    Found { device: DeviceInfo },
    Lost { device_id: String },
    Paired { device: DeviceInfo },
    Unpaired { device_id: String },
    Renamed { device: DeviceInfo },
    Connected { device_id: String },
    Disconnected { device_id: String },
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PairingCode {
    pub code: String,
    pub expires_at_ms: i64,
}

/// Implemented by the shell. Called on the core's callback thread.
#[uniffi::export(callback_interface)]
pub trait NodeListener: Send + Sync {
    fn on_device_event(&self, event: DeviceEvent);
    /// A peer copied something.
    fn on_clip(&self, clip: Clip);
    /// Someone tried to pair with the code this device is showing and failed.
    fn on_pairing_failed(&self, reason: String);
}

/// Progress for [`CtrlNode::fetch_to_file`]. Called on the core's callback thread.
#[uniffi::export(callback_interface)]
pub trait TransferListener: Send + Sync {
    fn on_progress(&self, received: u64, total: u64);
    fn on_complete(&self, path: String);
    fn on_failed(&self, message: String, cancelled: bool);
}

/// A running [`CtrlNode::fetch_to_file`]. Cancelling keeps the partial file,
/// so fetching to the same path again resumes where it stopped.
#[derive(uniffi::Object)]
pub struct Transfer {
    cancel: CancellationToken,
}

#[uniffi::export]
impl Transfer {
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

/// A lazy payload streaming in. Read it with `next_chunk` until it returns nothing.
#[derive(uniffi::Object)]
pub struct FetchStream {
    total: u64,
    offset: u64,
    rx: Mutex<mpsc::Receiver<Result<Vec<u8>>>>,
    cancel: CancellationToken,
}

#[uniffi::export]
impl FetchStream {
    pub fn total_size(&self) -> u64 {
        self.total
    }

    /// The offset this stream started at, as passed to `fetch`.
    pub fn start_offset(&self) -> u64 {
        self.offset
    }

    /// Blocks until the next chunk arrives. Returns `None` once every byte has arrived.
    pub fn next_chunk(&self) -> Result<Option<Vec<u8>>> {
        if self.cancel.is_cancelled() {
            return Err(CtrlError::Cancelled);
        }
        match self.rx.lock().unwrap().blocking_recv() {
            Some(Ok(chunk)) => Ok(Some(chunk)),
            Some(Err(e)) => Err(e),
            None if self.cancel.is_cancelled() => Err(CtrlError::Cancelled),
            None => Ok(None),
        }
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

type Job = Box<dyn FnOnce() + Send>;

/// Runs listener callbacks in order on one thread outside the runtime.
#[derive(Clone)]
struct Callbacks(std::sync::mpsc::Sender<Job>);

impl Callbacks {
    fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("ctrl-callbacks".into())
            .spawn(move || {
                for job in rx {
                    job();
                }
            })
            .expect("spawning the callback thread");
        Self(tx)
    }

    fn run(&self, job: impl FnOnce() + Send + 'static) {
        let _ = self.0.send(Box::new(job));
    }
}

#[derive(Clone)]
struct PeerConn {
    conn: Connection,
    control: mpsc::UnboundedSender<Control>,
    /// Device ID of whoever dialed this connection.
    initiator: String,
    since: Instant,
}

impl PeerConn {
    fn alive(&self) -> bool {
        self.conn.close_reason().is_none()
    }
}

struct PairingWindow {
    code: String,
    expires: Instant,
    attempts_left: u8,
}

struct Inner {
    trust: TrustStore,
    history: History,
    registry: Registry,
    transport: Transport,
    listener: Arc<dyn NodeListener>,
    callbacks: Callbacks,
    peers: Mutex<HashMap<String, PeerConn>>,
    dial_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    sightings: Mutex<HashMap<String, Sighting>>,
    pairing: Mutex<Option<PairingWindow>>,
    pair_busy: tokio::sync::Mutex<()>,
    discovery: Mutex<Option<Discovery>>,
    shutdown: CancellationToken,
    clips_sent: AtomicU64,
}

#[derive(uniffi::Object)]
pub struct CtrlNode {
    inner: Arc<Inner>,
    rt: Mutex<Option<tokio::runtime::Runtime>>,
    handle: tokio::runtime::Handle,
}

#[uniffi::export]
impl CtrlNode {
    /// Starts the node: opens history, binds the QUIC endpoint, starts
    /// discovery if enabled and reconnects to known peers.
    #[uniffi::constructor]
    pub fn new(
        config: NodeConfig,
        secrets: Box<dyn SecretStore>,
        listener: Box<dyn NodeListener>,
    ) -> Result<Arc<Self>> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("ctrl-core")
            .enable_all()
            .build()?;
        let handle = rt.handle().clone();

        let trust = TrustStore::open(Arc::from(secrets), &config.device_name)?;
        let history = History::open(&PathBuf::from(&config.data_dir), config.retention)?;
        let listen: SocketAddr = config
            .listen_address
            .as_deref()
            .unwrap_or("0.0.0.0:0")
            .parse()
            .map_err(|e| CtrlError::InvalidArgument(format!("listen address: {e}")))?;
        let transport = {
            let _guard = rt.enter();
            Transport::bind(listen)?
        };

        let inner = Arc::new(Inner {
            trust,
            history,
            registry: Registry::new(REGISTRY_CAPACITY),
            transport,
            listener: Arc::from(listener),
            callbacks: Callbacks::new(),
            peers: Mutex::default(),
            dial_locks: Mutex::default(),
            sightings: Mutex::default(),
            pairing: Mutex::default(),
            pair_busy: tokio::sync::Mutex::new(()),
            discovery: Mutex::default(),
            shutdown: CancellationToken::new(),
            clips_sent: AtomicU64::new(0),
        });

        handle.spawn(inner.clone().accept_loop());
        handle.spawn(inner.clone().reconnect_loop());

        if config.discovery {
            let id = inner.trust.identity();
            let port = inner.transport.local_addr()?.port();
            let weak: Weak<Inner> = Arc::downgrade(&inner);
            let h = handle.clone();
            let discovery = Discovery::start(&id.device_id, &fingerprint(&id.public_key), port, move |seen| {
                if let Some(inner) = weak.upgrade() {
                    h.spawn(async move { inner.on_sighting(seen) });
                }
            })?;
            *inner.discovery.lock().unwrap() = Some(discovery);
        }

        Ok(Arc::new(Self { inner, rt: Mutex::new(Some(rt)), handle }))
    }

    pub fn device_id(&self) -> String {
        self.inner.trust.device_id()
    }

    pub fn device_name(&self) -> String {
        self.inner.trust.identity().name
    }

    /// Fingerprint of this device's static key, for showing next to its name.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.inner.trust.identity().public_key)
    }

    /// The address peers can reach this node on, with the actual port.
    pub fn listen_address(&self) -> Result<String> {
        Ok(self.inner.transport.local_addr()?.to_string())
    }

    /// Renames this device and tells connected peers.
    pub fn set_device_name(&self, name: String) -> Result<()> {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Err(CtrlError::InvalidArgument("the name is empty".into()));
        }
        self.inner.trust.set_name(&name)?;
        for peer in self.inner.peers.lock().unwrap().values() {
            let _ = peer.control.send(Control::Renamed { name: name.clone() });
        }
        Ok(())
    }

    /// Trusted devices plus untrusted ones seen on the LAN, trusted first.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        let mut ids: Vec<String> = self.inner.trust.peers().into_iter().map(|p| p.device_id).collect();
        for id in self.inner.sightings.lock().unwrap().keys() {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        ids.iter().filter_map(|id| self.inner.device_info(id)).collect()
    }

    /// Opens a pairing window and returns the code to show. Another device
    /// pairs by calling [`pair_with`](Self::pair_with) with this code. Three
    /// wrong attempts or two minutes close the window.
    pub fn begin_pairing(&self) -> PairingCode {
        let code = generate_code();
        *self.inner.pairing.lock().unwrap() = Some(PairingWindow {
            code: code.clone(),
            expires: Instant::now() + PAIRING_WINDOW,
            attempts_left: PAIRING_ATTEMPTS,
        });
        PairingCode { code, expires_at_ms: now_ms() + PAIRING_WINDOW.as_millis() as i64 }
    }

    pub fn cancel_pairing(&self) {
        *self.inner.pairing.lock().unwrap() = None;
    }

    /// Pairs with the device showing `code`. `target` is a device ID seen
    /// through discovery, or an `ip:port` address. Blocks until pairing
    /// finishes.
    pub fn pair_with(&self, target: String, code: String) -> Result<DeviceInfo> {
        let inner = self.inner.clone();
        self.block_on(async move { inner.pair_with(&target, &code).await })
    }

    /// Removes a device's trust on both sides (the peer is told, if it is reachable).
    pub fn unpair(&self, device_id: String) -> Result<()> {
        let inner = self.inner.clone();
        self.block_on(async move { inner.unpair(&device_id).await })
    }

    /// Sets a local name for a paired device. An empty name restores the device's own name.
    pub fn rename_device(&self, device_id: String, name: String) -> Result<DeviceInfo> {
        let alias = Some(name.trim().to_owned()).filter(|n| !n.is_empty());
        self.inner.trust.update_peer(&device_id, |p| p.alias = alias)?.ok_or(CtrlError::UnknownPeer)?;
        let info = self.inner.device_info(&device_id).ok_or(CtrlError::UnknownPeer)?;
        self.inner.emit(DeviceEvent::Renamed { device: info.clone() });
        Ok(info)
    }

    /// Shares a clip with every paired device and returns right away.
    /// Representations over 256 KB are only announced, and their bytes move
    /// when a peer fetches them.
    ///
    /// Returns `None` for a concealed clip, which is neither sent nor recorded.
    pub fn publish(&self, clip: OutgoingClip) -> Result<Option<Clip>> {
        if clip.flags.concealed {
            return Ok(None);
        }
        let prepared = prepare(clip, &self.device_id(), INLINE_LIMIT)?;
        for (token, payload) in &prepared.payloads {
            self.inner.registry.insert(token.clone(), payload.clone());
        }
        if let Err(e) = self.inner.history.insert(&prepared.local, &prepared.payloads) {
            tracing::warn!("could not record clip in history: {e}");
        }
        for peer in self.inner.trust.peers() {
            let inner = self.inner.clone();
            let wire = prepared.wire.clone();
            self.handle.spawn(async move {
                match inner.ensure_connected(&peer.device_id).await {
                    Ok(pc) => {
                        let _ = pc.control.send(Control::Clip(wire));
                    }
                    Err(e) => tracing::debug!("not sending to {}: {e}", peer.device_id),
                }
            });
        }
        Ok(Some(prepared.local))
    }

    /// Starts streaming a lazy payload from `offset`. Blocks until the origin
    /// device answers.
    pub fn fetch(&self, handle: LazyHandle, offset: u64) -> Result<Arc<FetchStream>> {
        let inner = self.inner.clone();
        let cancel = self.inner.shutdown.child_token();
        let task_cancel = cancel.clone();
        let (total, rx) = self.block_on(async move {
            let (total, mut source) = inner.open_source(&handle, offset).await?;
            let (tx, rx) = mpsc::channel(8);
            tokio::spawn(async move {
                let mut received = offset;
                loop {
                    let mut buf = vec![0u8; CHUNK];
                    let n = tokio::select! {
                        _ = task_cancel.cancelled() => return,
                        n = source.read(&mut buf) => n,
                    };
                    match n {
                        Ok(0) => {
                            if received != total {
                                let _ = tx.send(Err(CtrlError::Network("transfer ended early".into()))).await;
                            }
                            return;
                        }
                        Ok(n) => {
                            received += n as u64;
                            buf.truncate(n);
                            if tx.send(Ok(buf)).await.is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(CtrlError::Network(e.to_string()))).await;
                            return;
                        }
                    }
                }
            });
            Ok::<_, CtrlError>((total, rx))
        })?;
        Ok(Arc::new(FetchStream { total, offset, rx: Mutex::new(rx), cancel }))
    }

    /// Fetches a whole payload into memory. Blocks until it arrives.
    pub fn fetch_bytes(&self, handle: LazyHandle) -> Result<Vec<u8>> {
        let inner = self.inner.clone();
        self.block_on(async move {
            let (total, mut source) = inner.open_source(&handle, 0).await?;
            let mut out = Vec::with_capacity(total.min(64 * 1024 * 1024) as usize);
            source.read_to_end(&mut out).await.map_err(|e| CtrlError::Network(e.to_string()))?;
            if out.len() as u64 != total {
                return Err(CtrlError::Network("transfer ended early".into()));
            }
            Ok(out)
        })
    }

    /// Downloads a payload to `path` in the background. Bytes go to
    /// `<path>.ctrlpart` first. That file is renamed when the transfer
    /// completes and kept if it fails, so the next call resumes from it.
    pub fn fetch_to_file(
        &self,
        handle: LazyHandle,
        path: String,
        listener: Box<dyn TransferListener>,
    ) -> Result<Arc<Transfer>> {
        let dest = PathBuf::from(&path);
        if dest.file_name().is_none() {
            return Err(CtrlError::InvalidArgument("path has no file name".into()));
        }
        let cancel = self.inner.shutdown.child_token();
        let listener: Arc<dyn TransferListener> = Arc::from(listener);
        let inner = self.inner.clone();
        let token = cancel.clone();
        self.handle.spawn(async move {
            let result = inner.download(&handle, &dest, &token, &listener).await;
            let callbacks = inner.callbacks.clone();
            callbacks.run(move || match result {
                Ok(()) => listener.on_complete(dest.to_string_lossy().into_owned()),
                Err(CtrlError::Cancelled) => listener.on_failed("cancelled".into(), true),
                Err(e) => listener.on_failed(e.to_string(), false),
            });
        });
        Ok(Arc::new(Transfer { cancel }))
    }

    /// History, newest first.
    pub fn history(&self, limit: u32, offset: u32) -> Result<Vec<Clip>> {
        self.inner.history.list(limit, offset)
    }

    pub fn history_item(&self, clip_id: String) -> Result<Option<Clip>> {
        self.inner.history.get(&clip_id)
    }

    pub fn delete_history_item(&self, clip_id: String) -> Result<bool> {
        self.inner.history.delete(&clip_id)
    }

    pub fn clear_history(&self) -> Result<()> {
        self.inner.history.clear()
    }

    pub fn set_retention(&self, retention: Retention) -> Result<()> {
        self.inner.history.set_retention(retention)
    }

    /// Stops discovery and closes every connection. The node is unusable afterwards.
    pub fn shutdown(&self) {
        if self.inner.shutdown.is_cancelled() {
            return;
        }
        self.inner.shutdown.cancel();
        if let Some(d) = self.inner.discovery.lock().unwrap().take() {
            d.stop();
        }
        let endpoint = self.inner.transport.endpoint.clone();
        endpoint.close(close::NORMAL, b"shutdown");
        let _ = self.block_on(async move {
            let _ = timeout(Duration::from_secs(1), endpoint.wait_idle()).await;
            Ok(())
        });
    }
}

impl CtrlNode {
    fn block_on<T>(&self, fut: impl std::future::Future<Output = Result<T>> + Send + 'static) -> Result<T>
    where
        T: Send + 'static,
    {
        if self.inner.shutdown.is_cancelled() {
            return Err(CtrlError::ShutDown);
        }
        // Run the future as a task so it isn't tied to the caller's thread,
        // then wait for it there.
        let (tx, rx) = std::sync::mpsc::channel();
        self.handle.spawn(async move {
            let _ = tx.send(fut.await);
        });
        rx.recv().map_err(|_| CtrlError::ShutDown)?
    }

    /// How many clips this node has written to peers. For tests.
    #[doc(hidden)]
    pub fn debug_clips_sent(&self) -> u64 {
        self.inner.clips_sent.load(Ordering::SeqCst)
    }
}

impl Drop for CtrlNode {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(rt) = self.rt.lock().unwrap().take() {
            rt.shutdown_timeout(Duration::from_secs(1));
        }
    }
}

/// Lets an `Arc<Vec<u8>>` back a `Cursor` without copying it.
struct SharedBytes(Arc<Vec<u8>>);

impl AsRef<[u8]> for SharedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Inner {
    fn emit(&self, event: DeviceEvent) {
        let listener = self.listener.clone();
        self.callbacks.run(move || listener.on_device_event(event));
    }

    fn pairing_failed(&self, reason: String) {
        let listener = self.listener.clone();
        self.callbacks.run(move || listener.on_pairing_failed(reason));
    }

    fn connected(&self, device_id: &str) -> Option<PeerConn> {
        self.peers.lock().unwrap().get(device_id).filter(|p| p.alive()).cloned()
    }

    fn device_info(&self, device_id: &str) -> Option<DeviceInfo> {
        let peer = self.trust.peer(device_id);
        let sighting = self.sightings.lock().unwrap().get(device_id).cloned();
        let fp = match (&peer, &sighting) {
            (Some(p), _) => fingerprint(&p.public_key),
            (None, Some(s)) => s.fingerprint.clone(),
            (None, None) => return None,
        };
        let mut addresses: Vec<String> =
            sighting.map(|s| s.addresses.iter().map(ToString::to_string).collect()).unwrap_or_default();
        if let Some(a) = peer.as_ref().and_then(|p| p.last_address.clone()) {
            if !addresses.contains(&a) {
                addresses.push(a);
            }
        }
        Some(DeviceInfo {
            device_id: device_id.to_owned(),
            name: peer.as_ref().map(|p| p.display_name().to_owned()),
            fingerprint: fp,
            trusted: peer.is_some(),
            connected: self.connected(device_id).is_some(),
            addresses,
        })
    }

    fn on_sighting(self: Arc<Self>, seen: Seen) {
        match seen {
            Seen::Found(s) => {
                let id = s.device_id.clone();
                let previous = self.sightings.lock().unwrap().insert(id.clone(), s.clone());
                // mDNS resolves the same device again as each interface's
                // address arrives. Only a new device or a new key is news.
                if previous.as_ref().is_some_and(|p| p.fingerprint == s.fingerprint) {
                    return;
                }
                let Some(info) = self.device_info(&id) else { return };
                let trusted = self.trust.peer(&id).is_some_and(|p| fingerprint(&p.public_key) == s.fingerprint);
                self.emit(DeviceEvent::Found { device: info });
                if trusted && self.connected(&id).is_none() {
                    // Only the lower ID dials straight away, so both sides
                    // rarely dial at once. The other side waits briefly in case
                    // its peer didn't see it.
                    let delay = if self.trust.device_id() < id { Duration::ZERO } else { DIAL_RACE_WINDOW };
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = self.ensure_connected(&id).await;
                    });
                }
            }
            Seen::Lost(id) => {
                if self.sightings.lock().unwrap().remove(&id).is_some() {
                    self.emit(DeviceEvent::Lost { device_id: id });
                }
            }
        }
    }

    async fn accept_loop(self: Arc<Self>) {
        loop {
            let incoming = tokio::select! {
                _ = self.shutdown.cancelled() => break,
                i = self.transport.endpoint.accept() => i,
            };
            let Some(incoming) = incoming else { break };
            let me = self.clone();
            tokio::spawn(async move {
                let conn = match incoming.await {
                    Ok(c) => c,
                    Err(e) => return tracing::debug!("incoming connection failed: {e}"),
                };
                let result = match transport::alpn(&conn).as_deref() {
                    Some(ALPN_SESSION) => me.accept_session(conn).await,
                    Some(ALPN_PAIR) => me.accept_pairing(conn).await,
                    _ => {
                        conn.close(close::REJECTED, b"unknown protocol");
                        Ok(())
                    }
                };
                if let Err(e) = result {
                    tracing::debug!("incoming connection: {e}");
                }
            });
        }
    }

    async fn reconnect_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(RECONNECT_INTERVAL);
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = tick.tick() => {}
            }
            for peer in self.trust.peers() {
                if self.connected(&peer.device_id).is_some() || self.addresses_for(&peer).is_empty() {
                    continue;
                }
                let me = self.clone();
                tokio::spawn(async move {
                    let _ = me.ensure_connected(&peer.device_id).await;
                });
            }
        }
    }

    fn addresses_for(&self, peer: &PeerRecord) -> Vec<SocketAddr> {
        let fp = fingerprint(&peer.public_key);
        let mut addrs: Vec<SocketAddr> = self
            .sightings
            .lock()
            .unwrap()
            .get(&peer.device_id)
            .filter(|s| s.fingerprint == fp)
            .map(|s| s.addresses.clone())
            .unwrap_or_default();
        if let Some(a) = peer.last_address.as_deref().and_then(|a| a.parse().ok()) {
            if !addrs.contains(&a) {
                addrs.push(a);
            }
        }
        addrs
    }

    /// Returns a live session with a trusted peer, dialing it if needed.
    async fn ensure_connected(self: &Arc<Self>, device_id: &str) -> Result<PeerConn> {
        if let Some(pc) = self.connected(device_id) {
            return Ok(pc);
        }
        let lock = self.dial_locks.lock().unwrap().entry(device_id.to_owned()).or_default().clone();
        let _guard = lock.lock().await;
        if let Some(pc) = self.connected(device_id) {
            return Ok(pc);
        }
        let peer = self.trust.peer(device_id).ok_or(CtrlError::UnknownPeer)?;
        let mut last = CtrlError::Unreachable(peer.display_name().to_owned());
        for addr in self.addresses_for(&peer) {
            match self.dial(&peer, addr).await {
                Ok(()) => break,
                Err(e) => last = e,
            }
        }
        self.connected(device_id).ok_or(last)
    }

    async fn dial(self: &Arc<Self>, peer: &PeerRecord, addr: SocketAddr) -> Result<()> {
        let conn = self.transport.connect(addr, false).await?;
        let result = async {
            let binding = transport::binding(&conn)?;
            let (mut send, mut recv) = conn.open_bi().await?;
            let identity = self.trust.identity();
            let hello = timeout(HANDSHAKE_TIMEOUT, session::initiate(&mut recv, &mut send, &identity, peer, &binding))
                .await
                .map_err(|_| CtrlError::Protocol("session handshake timed out".into()))??;
            Ok::<_, CtrlError>((send, recv, hello))
        }
        .await;
        match result {
            Ok((send, recv, hello)) => {
                self.register(conn, send, recv, peer.clone(), hello, true);
                Ok(())
            }
            Err(e) => {
                conn.close(close::REJECTED, b"handshake failed");
                Err(e)
            }
        }
    }

    async fn accept_session(self: &Arc<Self>, conn: Connection) -> Result<()> {
        let binding = transport::binding(&conn)?;
        let (mut send, mut recv) = timeout(HANDSHAKE_TIMEOUT, conn.accept_bi())
            .await
            .map_err(|_| CtrlError::Protocol("no handshake".into()))??;
        let identity = self.trust.identity();
        let result = timeout(
            HANDSHAKE_TIMEOUT,
            session::respond(&mut recv, &mut send, &identity, &binding, |key| self.trust.peer_by_key(key)),
        )
        .await
        .map_err(|_| CtrlError::Protocol("session handshake timed out".into()))
        .and_then(|r| r);
        match result {
            Ok((peer, hello)) => {
                self.register(conn, send, recv, peer, hello, false);
                Ok(())
            }
            Err(e) => {
                // Give the rejection frame a moment to reach the dialer.
                let _ = send.finish();
                tokio::time::sleep(Duration::from_millis(200)).await;
                conn.close(close::REJECTED, b"not trusted");
                Err(e)
            }
        }
    }

    fn register(
        self: &Arc<Self>,
        conn: Connection,
        send: SendStream,
        recv: RecvStream,
        peer: PeerRecord,
        hello: Hello,
        we_dialed: bool,
    ) {
        let self_id = self.trust.device_id();
        let peer_id = peer.device_id.clone();
        let initiator = if we_dialed { self_id.clone() } else { peer_id.clone() };
        let address = conn.remote_address().to_string();
        let renamed = peer.name != hello.name;
        let _ = self.trust.update_peer(&peer_id, |p| {
            p.name = hello.name.clone();
            p.last_address = Some(address);
        });

        let (tx, rx) = mpsc::unbounded_channel();
        let was_connected = {
            let mut peers = self.peers.lock().unwrap();
            let existing = peers.get(&peer_id).filter(|p| p.alive()).cloned();
            if let Some(existing) = &existing {
                let preferred = std::cmp::min(&self_id, &peer_id);
                if existing.since.elapsed() < DIAL_RACE_WINDOW
                    && existing.initiator == *preferred
                    && initiator != *preferred
                {
                    conn.close(close::DUPLICATE, b"duplicate");
                    return;
                }
                existing.conn.close(close::DUPLICATE, b"replaced");
            }
            peers.insert(
                peer_id.clone(),
                PeerConn { conn: conn.clone(), control: tx, initiator, since: Instant::now() },
            );
            existing.is_some()
        };

        tokio::spawn(self.clone().control_writer(send, rx));
        tokio::spawn(self.clone().control_reader(peer_id.clone(), conn.clone(), recv));
        tokio::spawn(self.clone().serve_fetches(conn.clone()));
        tokio::spawn(self.clone().watch(peer_id.clone(), conn));

        if !was_connected {
            self.emit(DeviceEvent::Connected { device_id: peer_id.clone() });
        }
        if renamed {
            if let Some(info) = self.device_info(&peer_id) {
                self.emit(DeviceEvent::Renamed { device: info });
            }
        }
    }

    async fn control_writer(self: Arc<Self>, mut send: SendStream, mut rx: mpsc::UnboundedReceiver<Control>) {
        while let Some(msg) = rx.recv().await {
            let is_clip = matches!(msg, Control::Clip(_));
            if write_msg(&mut send, &msg).await.is_err() {
                break;
            }
            if is_clip {
                self.clips_sent.fetch_add(1, Ordering::SeqCst);
            }
        }
        let _ = send.finish();
    }

    async fn control_reader(self: Arc<Self>, peer_id: String, conn: Connection, mut recv: RecvStream) {
        while let Ok(msg) = read_msg::<_, Control>(&mut recv).await {
            match msg {
                Control::Clip(wire) => {
                    let clip = wire.into_clip(&peer_id);
                    if let Err(e) = self.history.insert(&clip, &[]) {
                        tracing::warn!("could not record clip in history: {e}");
                    }
                    let listener = self.listener.clone();
                    self.callbacks.run(move || listener.on_clip(clip));
                }
                Control::Renamed { name } => {
                    let name: String = name.chars().take(64).collect();
                    if let Ok(Some(_)) = self.trust.update_peer(&peer_id, |p| p.name = name) {
                        if let Some(info) = self.device_info(&peer_id) {
                            self.emit(DeviceEvent::Renamed { device: info });
                        }
                    }
                }
                Control::Unpaired => {
                    if let Err(e) = self.trust.remove_peer(&peer_id) {
                        tracing::warn!("could not remove peer: {e}");
                    }
                    conn.close(close::UNPAIRED, b"unpaired");
                    self.emit(DeviceEvent::Unpaired { device_id: peer_id.clone() });
                    break;
                }
            }
        }
    }

    async fn serve_fetches(self: Arc<Self>, conn: Connection) {
        while let Ok((send, mut recv)) = conn.accept_bi().await {
            let me = self.clone();
            tokio::spawn(async move {
                let mut tag = [0u8; 1];
                if recv.read_exact(&mut tag).await.is_ok() && tag[0] == STREAM_FETCH {
                    if let Err(e) = payload::serve(&me.registry, send, recv).await {
                        tracing::debug!("fetch stream ended: {e}");
                    }
                }
            });
        }
    }

    async fn watch(self: Arc<Self>, peer_id: String, conn: Connection) {
        let reason = conn.closed().await;
        tracing::debug!("connection to {peer_id} closed: {reason}");
        let removed = {
            let mut peers = self.peers.lock().unwrap();
            if peers.get(&peer_id).is_some_and(|p| p.conn.stable_id() == conn.stable_id()) {
                peers.remove(&peer_id);
                true
            } else {
                false
            }
        };
        if removed && !self.shutdown.is_cancelled() {
            self.emit(DeviceEvent::Disconnected { device_id: peer_id });
        }
    }

    async fn accept_pairing(self: &Arc<Self>, conn: Connection) -> Result<()> {
        let Ok(_busy) = self.pair_busy.try_lock() else {
            conn.close(close::REJECTED, b"busy");
            return Ok(());
        };
        let code = {
            let mut window = self.pairing.lock().unwrap();
            match window.as_ref() {
                Some(w) if w.expires > Instant::now() => w.code.clone(),
                _ => {
                    *window = None;
                    conn.close(close::REJECTED, b"not pairing");
                    return Err(CtrlError::NotPairing);
                }
            }
        };

        let binding = transport::binding(&conn)?;
        let (mut send, mut recv) = timeout(HANDSHAKE_TIMEOUT, conn.accept_bi())
            .await
            .map_err(|_| CtrlError::Protocol("no pairing stream".into()))??;
        let identity = self.trust.identity();
        let remote = conn.remote_address().to_string();
        let accept = |outcome: &pairing::PairOutcome| {
            let alias = self.trust.peer(&outcome.hello.device_id).and_then(|p| p.alias);
            self.trust.save_peer(PeerRecord {
                device_id: outcome.hello.device_id.clone(),
                name: outcome.hello.name.clone(),
                alias,
                public_key: outcome.public_key.clone(),
                paired_at_ms: now_ms(),
                last_address: Some(remote.clone()),
            })
        };
        let result =
            timeout(PAIRING_TIMEOUT, pairing::respond(&mut recv, &mut send, &code, &identity, &binding, accept))
                .await
                .unwrap_or_else(|_| Err(CtrlError::PairingFailed("timed out".into())));
        let _ = send.finish();

        match &result {
            Ok(outcome) => {
                *self.pairing.lock().unwrap() = None;
                if let Some(info) = self.device_info(&outcome.hello.device_id) {
                    self.emit(DeviceEvent::Paired { device: info });
                }
            }
            Err(CtrlError::WrongCode) => {
                let mut window = self.pairing.lock().unwrap();
                let exhausted = window.as_mut().is_none_or(|w| {
                    w.attempts_left = w.attempts_left.saturating_sub(1);
                    w.attempts_left == 0
                });
                if exhausted {
                    *window = None;
                    self.pairing_failed("Too many wrong codes. Start pairing again to get a new code.".into());
                } else {
                    self.pairing_failed("The code entered on the other device was wrong.".into());
                }
            }
            Err(e) => self.pairing_failed(e.to_string()),
        }
        // Let the initiator read our last message, then hang up.
        let _ = timeout(Duration::from_secs(2), conn.closed()).await;
        result.map(|_| ())
    }

    async fn pair_with(self: &Arc<Self>, target: &str, code: &str) -> Result<DeviceInfo> {
        let code = normalize_code(code)?;
        let addrs: Vec<SocketAddr> = match target.parse::<SocketAddr>() {
            Ok(a) => vec![a],
            Err(_) => self
                .sightings
                .lock()
                .unwrap()
                .get(target)
                .map(|s| s.addresses.clone())
                .ok_or_else(|| CtrlError::Unreachable(target.to_owned()))?,
        };

        let mut last = CtrlError::Unreachable(target.to_owned());
        let mut found = None;
        for addr in addrs {
            match self.transport.connect(addr, true).await {
                Ok(c) => {
                    found = Some((c, addr));
                    break;
                }
                Err(e) => last = e,
            }
        }
        let (conn, addr) = found.ok_or(last)?;

        let identity = self.trust.identity();
        let result = async {
            let binding = transport::binding(&conn)?;
            let (mut send, mut recv) = conn.open_bi().await?;
            timeout(PAIRING_TIMEOUT, pairing::initiate(&mut recv, &mut send, &code, &identity, &binding))
                .await
                .map_err(|_| CtrlError::PairingFailed("timed out".into()))?
        }
        .await;
        let outcome = match result {
            Ok(o) => o,
            Err(e) => {
                // The responder closes a connection it isn't pairing on.
                let e = match (conn.close_reason(), e) {
                    (Some(quinn::ConnectionError::ApplicationClosed(c)), _) if c.reason.as_ref() == b"not pairing" => {
                        CtrlError::NotPairing
                    }
                    (_, e) => e,
                };
                conn.close(close::NORMAL, b"pairing failed");
                return Err(e);
            }
        };
        conn.close(close::NORMAL, b"paired");

        let device_id = outcome.hello.device_id.clone();
        let alias = self.trust.peer(&device_id).and_then(|p| p.alias);
        self.trust.save_peer(PeerRecord {
            device_id: device_id.clone(),
            name: outcome.hello.name,
            alias,
            public_key: outcome.public_key,
            paired_at_ms: now_ms(),
            last_address: Some(addr.to_string()),
        })?;
        let info = self.device_info(&device_id).ok_or(CtrlError::UnknownPeer)?;
        self.emit(DeviceEvent::Paired { device: info.clone() });

        let me = self.clone();
        tokio::spawn(async move {
            if let Err(e) = me.ensure_connected(&device_id).await {
                tracing::debug!("first session after pairing failed: {e}");
            }
        });
        Ok(info)
    }

    async fn unpair(self: &Arc<Self>, device_id: &str) -> Result<()> {
        if self.trust.peer(device_id).is_none() {
            return Err(CtrlError::UnknownPeer);
        }
        if let Some(pc) = self.connected(device_id) {
            let _ = pc.control.send(Control::Unpaired);
            // Give the message a moment to go out before closing.
            let _ = timeout(Duration::from_millis(500), pc.conn.closed()).await;
            pc.conn.close(close::UNPAIRED, b"unpaired");
        }
        self.trust.remove_peer(device_id)?;
        self.emit(DeviceEvent::Unpaired { device_id: device_id.to_owned() });
        Ok(())
    }

    /// Where a payload's bytes come from: the origin device over QUIC, or this
    /// device's own registry when we copied it here.
    async fn open_source(
        self: &Arc<Self>,
        handle: &LazyHandle,
        offset: u64,
    ) -> Result<(u64, Box<dyn AsyncRead + Send + Unpin>)> {
        if handle.device_id == self.trust.device_id() {
            let past_end =
                |total: u64| CtrlError::InvalidArgument(format!("offset {offset} is past the end ({total} bytes)"));
            return match self.registry.get(&handle.token).ok_or(CtrlError::PayloadNotFound)? {
                LocalPayload::Bytes(bytes) => {
                    let total = bytes.len() as u64;
                    if offset > total {
                        return Err(past_end(total));
                    }
                    let mut cursor = std::io::Cursor::new(SharedBytes(bytes));
                    cursor.set_position(offset);
                    Ok((total, Box::new(cursor)))
                }
                LocalPayload::File(path) => {
                    let mut file = tokio::fs::File::open(&path).await.map_err(|_| CtrlError::PayloadNotFound)?;
                    let total = file.metadata().await?.len();
                    if offset > total {
                        return Err(past_end(total));
                    }
                    file.seek(std::io::SeekFrom::Start(offset)).await?;
                    Ok((total, Box::new(file)))
                }
            };
        }
        let pc = self.ensure_connected(&handle.device_id).await?;
        let (total, recv) = payload::open(&pc.conn, &handle.token, offset).await?;
        Ok((total, Box::new(recv)))
    }

    async fn download(
        self: &Arc<Self>,
        handle: &LazyHandle,
        dest: &std::path::Path,
        cancel: &CancellationToken,
        listener: &Arc<dyn TransferListener>,
    ) -> Result<()> {
        let mut part_name = dest.file_name().unwrap_or_default().to_os_string();
        part_name.push(".ctrlpart");
        let part = dest.with_file_name(part_name);

        let mut offset = tokio::fs::metadata(&part).await.map(|m| m.len()).unwrap_or(0);
        if offset > handle.size {
            offset = 0;
        }
        let (total, mut source) = tokio::select! {
            _ = cancel.cancelled() => return Err(CtrlError::Cancelled),
            r = self.open_source(handle, offset) => r?,
        };
        if total < offset {
            return Err(CtrlError::Protocol("payload is smaller than the partial download".into()));
        }

        // Not append mode: Windows refuses set_len on an append-only handle.
        let mut file = tokio::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&part).await?;
        file.set_len(offset).await?;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut received = offset;
        let mut buf = vec![0u8; CHUNK];
        let mut last_report = Instant::now() - PROGRESS_INTERVAL;
        loop {
            if last_report.elapsed() >= PROGRESS_INTERVAL {
                last_report = Instant::now();
                let l = listener.clone();
                self.callbacks.run(move || l.on_progress(received, total));
            }
            let n = tokio::select! {
                _ = cancel.cancelled() => {
                    file.flush().await?;
                    return Err(CtrlError::Cancelled);
                }
                n = source.read(&mut buf) => n.map_err(|e| CtrlError::Network(e.to_string()))?,
            };
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).await?;
            received += n as u64;
        }
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        if received != total {
            return Err(CtrlError::Network(format!("transfer ended early at {received} of {total} bytes")));
        }
        tokio::fs::rename(&part, dest).await?;
        let l = listener.clone();
        self.callbacks.run(move || l.on_progress(total, total));
        Ok(())
    }
}

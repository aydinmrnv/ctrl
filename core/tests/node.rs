//! End-to-end tests: real nodes talking over loopback QUIC.

use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ctrl_core::formats::{FILE_URL, IMAGE_PNG, TEXT_PLAIN};
use ctrl_core::*;
use sha2::{Digest, Sha256};

const WAIT: Duration = Duration::from_secs(10);

#[derive(Debug)]
enum Event {
    Device(DeviceEvent),
    Clip(Clip),
    PairingFailed(String),
}

struct Listener(Sender<Event>);

impl NodeListener for Listener {
    fn on_device_event(&self, event: DeviceEvent) {
        let _ = self.0.send(Event::Device(event));
    }
    fn on_clip(&self, clip: Clip) {
        let _ = self.0.send(Event::Clip(clip));
    }
    fn on_pairing_failed(&self, reason: String) {
        let _ = self.0.send(Event::PairingFailed(reason));
    }
}

struct TestNode {
    node: Arc<CtrlNode>,
    events: Receiver<Event>,
    _dir: tempfile::TempDir,
}

impl TestNode {
    fn new(name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (tx, events) = channel();
        let node = CtrlNode::new(
            NodeConfig {
                device_name: name.into(),
                data_dir: dir.path().to_string_lossy().into(),
                listen_address: Some("127.0.0.1:0".into()),
                discovery: false,
                retention: Retention::default(),
            },
            Box::new(MemorySecretStore::default()),
            Box::new(Listener(tx)),
        )
        .unwrap();
        Self { node, events, _dir: dir }
    }

    fn addr(&self) -> String {
        self.node.listen_address().unwrap()
    }

    /// Waits for the first event `f` accepts, skipping the rest.
    fn wait_for<T>(&self, mut f: impl FnMut(Event) -> Option<T>) -> T {
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(e) => {
                    if let Some(t) = f(e) {
                        return t;
                    }
                }
                Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for an event"),
                Err(e) => panic!("{e}"),
            }
        }
    }

    fn next_clip(&self) -> Clip {
        self.wait_for(|e| match e {
            Event::Clip(c) => Some(c),
            _ => None,
        })
    }

    fn wait_connected(&self, id: &str) {
        self.wait_for(|e| match e {
            Event::Device(DeviceEvent::Connected { device_id }) if device_id == id => Some(()),
            _ => None,
        })
    }

    fn assert_no_clip_within(&self, d: Duration) {
        let deadline = Instant::now() + d;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.events.recv_timeout(left) {
                Ok(Event::Clip(c)) => panic!("unexpected clip {c:?}"),
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
}

fn pair() -> (TestNode, TestNode) {
    let (mac, pc) = (TestNode::new("Mac"), TestNode::new("PC"));
    let code = pc.node.begin_pairing().code;
    let info = mac.node.pair_with(pc.addr(), code).unwrap();
    assert_eq!(info.name.as_deref(), Some("PC"));
    assert!(info.trusted);
    mac.wait_connected(&pc.node.device_id());
    pc.wait_connected(&mac.node.device_id());
    (mac, pc)
}

fn text(s: &str) -> OutgoingRepresentation {
    OutgoingRepresentation {
        format: TEXT_PLAIN.into(),
        source: PayloadSource::Bytes { data: s.as_bytes().to_vec() },
        name: None,
        thumbnail_png: None,
    }
}

fn clip(reps: Vec<OutgoingRepresentation>) -> OutgoingClip {
    OutgoingClip { representations: reps, flags: ClipFlags::default() }
}

fn write_random_file(dir: &std::path::Path, name: &str, len: usize) -> (std::path::PathBuf, Vec<u8>) {
    let mut data = vec![0u8; len];
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for chunk in data.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
    }
    let path = dir.join(name);
    std::fs::write(&path, &data).unwrap();
    let digest = Sha256::digest(&data).to_vec();
    (path, digest)
}

#[derive(Clone)]
struct Progress(Sender<Result<String, (String, bool)>>);

impl TransferListener for Progress {
    fn on_progress(&self, _received: u64, _total: u64) {}
    fn on_complete(&self, path: String) {
        let _ = self.0.send(Ok(path));
    }
    fn on_failed(&self, message: String, cancelled: bool) {
        let _ = self.0.send(Err((message, cancelled)));
    }
}

#[test]
fn pairs_and_syncs_text_both_ways() {
    let (mac, pc) = pair();

    let sent = mac.node.publish(clip(vec![text("hello from the mac")])).unwrap().unwrap();
    let got = pc.next_clip();
    assert_eq!(got.id, sent.id);
    assert_eq!(got.origin_device, mac.node.device_id());
    assert_eq!(got.representations[0].inline_bytes.as_deref(), Some(&b"hello from the mac"[..]));

    pc.node.publish(clip(vec![text("and back")])).unwrap();
    let got = mac.next_clip();
    assert_eq!(got.representations[0].inline_bytes.as_deref(), Some(&b"and back"[..]));

    // Both ends keep history.
    assert_eq!(mac.node.history(10, 0).unwrap().len(), 2);
    assert_eq!(pc.node.history(10, 0).unwrap().len(), 2);
}

#[test]
fn large_image_moves_lazily_and_inline_thumbnail_travels_with_it() {
    let (mac, pc) = pair();
    let image: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    mac.node
        .publish(clip(vec![OutgoingRepresentation {
            format: IMAGE_PNG.into(),
            source: PayloadSource::Bytes { data: image.clone() },
            name: None,
            thumbnail_png: Some(vec![1, 2, 3]),
        }]))
        .unwrap();
    let got = pc.next_clip();
    let rep = &got.representations[0];
    assert!(rep.inline_bytes.is_none());
    assert_eq!(rep.thumbnail_png.as_deref(), Some(&[1u8, 2, 3][..]));
    let handle = rep.lazy_handle.clone().unwrap();
    assert_eq!(handle.size, image.len() as u64);

    assert_eq!(pc.node.fetch_bytes(handle.clone()).unwrap(), image);

    // Streaming from an offset returns exactly the tail.
    let stream = pc.node.fetch(handle, 1_000_000).unwrap();
    assert_eq!(stream.total_size(), image.len() as u64);
    let mut tail = Vec::new();
    while let Some(chunk) = stream.next_chunk().unwrap() {
        tail.extend(chunk);
    }
    assert_eq!(tail, image[1_000_000..]);
}

#[test]
fn file_transfer_resumes_after_cancel() {
    let (mac, pc) = pair();
    let src_dir = tempfile::tempdir().unwrap();
    let (path, digest) = write_random_file(src_dir.path(), "movie.mov", 48 * 1024 * 1024);

    let started = Instant::now();
    mac.node
        .publish(clip(vec![OutgoingRepresentation {
            format: FILE_URL.into(),
            source: PayloadSource::File { path: path.to_string_lossy().into() },
            name: Some("movie.mov".into()),
            thumbnail_png: None,
        }]))
        .unwrap();
    assert!(started.elapsed() < Duration::from_millis(200), "publish must not wait for the bytes");
    let got = pc.next_clip();
    let rep = &got.representations[0];
    assert_eq!(rep.name.as_deref(), Some("movie.mov"));
    let handle = rep.lazy_handle.clone().unwrap();

    let dest_dir = tempfile::tempdir().unwrap();
    let dest = dest_dir.path().join("movie.mov");
    let part = dest_dir.path().join("movie.mov.ctrlpart");

    // Pull part of it through a stream and stop, as a cancelled paste would.
    let first = 5 * 1024 * 1024;
    let stream = pc.node.fetch(handle.clone(), 0).unwrap();
    let mut partial = Vec::new();
    while partial.len() < first {
        partial.extend(stream.next_chunk().unwrap().unwrap());
    }
    stream.cancel();
    assert!(matches!(stream.next_chunk(), Err(CtrlError::Cancelled)));
    std::fs::write(&part, &partial).unwrap();

    // fetch_to_file picks up from the partial file.
    let (tx, rx) = channel();
    pc.node.fetch_to_file(handle, dest.to_string_lossy().into(), Box::new(Progress(tx))).unwrap();
    let result = rx.recv_timeout(Duration::from_secs(60)).unwrap();
    assert_eq!(result, Ok(dest.to_string_lossy().into_owned()));
    assert!(!part.exists());
    assert_eq!(Sha256::digest(std::fs::read(&dest).unwrap()).to_vec(), digest);
}

#[test]
fn cancelled_download_keeps_partial_file() {
    let (mac, pc) = pair();
    let src_dir = tempfile::tempdir().unwrap();
    let (path, digest) = write_random_file(src_dir.path(), "big.bin", 256 * 1024 * 1024);
    mac.node
        .publish(clip(vec![OutgoingRepresentation {
            format: FILE_URL.into(),
            source: PayloadSource::File { path: path.to_string_lossy().into() },
            name: Some("big.bin".into()),
            thumbnail_png: None,
        }]))
        .unwrap();
    let handle = pc.next_clip().representations[0].lazy_handle.clone().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();
    let dest = dest_dir.path().join("big.bin");

    let (tx, rx) = channel();
    let transfer =
        pc.node.fetch_to_file(handle.clone(), dest.to_string_lossy().into(), Box::new(Progress(tx))).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    transfer.cancel();
    match rx.recv_timeout(WAIT).unwrap() {
        Err((_, cancelled)) => assert!(cancelled),
        // On a very fast machine the whole file may already be across.
        Ok(_) => return,
    }
    assert!(dest_dir.path().join("big.bin.ctrlpart").exists());
    assert!(!dest.exists());

    let (tx, rx) = channel();
    pc.node.fetch_to_file(handle, dest.to_string_lossy().into(), Box::new(Progress(tx))).unwrap();
    assert!(rx.recv_timeout(Duration::from_secs(120)).unwrap().is_ok());
    assert_eq!(Sha256::digest(std::fs::read(&dest).unwrap()).to_vec(), digest);
}

#[test]
fn concealed_clip_is_never_transmitted_or_recorded() {
    let (mac, pc) = pair();
    // Prove the link works first, so silence below means something.
    mac.node.publish(clip(vec![text("warm-up")])).unwrap();
    pc.next_clip();
    let sent_before = mac.node.debug_clips_sent();

    let result = mac
        .node
        .publish(OutgoingClip {
            representations: vec![text("correct horse battery staple")],
            flags: ClipFlags { concealed: true, transient: false },
        })
        .unwrap();
    assert!(result.is_none());

    pc.assert_no_clip_within(Duration::from_millis(750));
    assert_eq!(mac.node.debug_clips_sent(), sent_before, "nothing was written to the wire");
    let secret = b"correct horse battery staple".to_vec();
    for node in [&mac.node, &pc.node] {
        let leaked = node
            .history(100, 0)
            .unwrap()
            .into_iter()
            .flat_map(|c| c.representations)
            .any(|r| r.inline_bytes.as_ref() == Some(&secret));
        assert!(!leaked, "concealed clip must not be in history");
    }
}

#[test]
fn transient_clip_is_synced_but_not_recorded() {
    let (mac, pc) = pair();
    mac.node
        .publish(OutgoingClip {
            representations: vec![text("one-time code 123456")],
            flags: ClipFlags { concealed: false, transient: true },
        })
        .unwrap();
    assert!(pc.next_clip().transient);
    assert!(mac.node.history(10, 0).unwrap().is_empty());
    assert!(pc.node.history(10, 0).unwrap().is_empty());
}

#[test]
fn wrong_code_fails_and_three_strikes_close_the_window() {
    let (mac, pc) = (TestNode::new("Mac"), TestNode::new("PC"));
    let code = pc.node.begin_pairing().code;
    let wrong = if code == "000000" { "111111" } else { "000000" };

    for attempt in 1..=3 {
        let err = mac.node.pair_with(pc.addr(), wrong.into()).unwrap_err();
        assert!(matches!(err, CtrlError::WrongCode), "attempt {attempt}: {err:?}");
        let reason = pc.wait_for(|e| match e {
            Event::PairingFailed(reason) => Some(reason),
            _ => None,
        });
        assert!(!reason.is_empty());
    }
    // The window is closed, so even the right code is refused now.
    let err = mac.node.pair_with(pc.addr(), code).unwrap_err();
    assert!(matches!(err, CtrlError::NotPairing | CtrlError::Network(_) | CtrlError::Io(_)), "{err:?}");
    assert!(mac.node.devices().is_empty());
    assert!(pc.node.devices().is_empty());
}

#[test]
fn pairing_requires_an_open_window() {
    let (mac, pc) = (TestNode::new("Mac"), TestNode::new("PC"));
    assert!(mac.node.pair_with(pc.addr(), "123456".into()).is_err());
    assert!(matches!(mac.node.pair_with(pc.addr(), "12345".into()), Err(CtrlError::InvalidArgument(_))));
}

#[test]
fn unknown_peer_cannot_send() {
    let (mac, pc) = pair();
    let stranger = TestNode::new("Stranger");
    // The stranger pairs with someone else, so it has a valid identity but no
    // trust relationship with the PC.
    let code = stranger.node.begin_pairing().code;
    let other = TestNode::new("Other");
    other.node.pair_with(stranger.addr(), code).unwrap();
    stranger.node.publish(clip(vec![text("let me in")])).unwrap();
    pc.assert_no_clip_within(Duration::from_millis(500));
    drop(mac);
}

#[test]
fn unpair_removes_trust_on_both_sides_and_rename_is_local() {
    let (mac, pc) = pair();
    let (mac_id, pc_id) = (mac.node.device_id(), pc.node.device_id());

    let renamed = mac.node.rename_device(pc_id.clone(), "Gaming PC".into()).unwrap();
    assert_eq!(renamed.name.as_deref(), Some("Gaming PC"));
    assert_eq!(mac.node.rename_device(pc_id.clone(), "".into()).unwrap().name.as_deref(), Some("PC"));

    pc.node.set_device_name("Desktop".into()).unwrap();
    mac.wait_for(|e| match e {
        Event::Device(DeviceEvent::Renamed { device }) if device.name.as_deref() == Some("Desktop") => Some(()),
        _ => None,
    });

    mac.node.unpair(pc_id.clone()).unwrap();
    pc.wait_for(|e| match e {
        Event::Device(DeviceEvent::Unpaired { device_id }) if device_id == mac_id => Some(()),
        _ => None,
    });
    assert!(mac.node.devices().is_empty());
    assert!(pc.node.devices().is_empty());

    mac.node.publish(clip(vec![text("still there?")])).unwrap();
    pc.assert_no_clip_within(Duration::from_millis(500));
}

#[test]
fn notices_when_a_peer_goes_away() {
    let (mac, pc) = pair();
    let pc_id = pc.node.device_id();
    pc.node.shutdown();
    mac.wait_for(|e| match e {
        Event::Device(DeviceEvent::Disconnected { device_id }) if device_id == pc_id => Some(()),
        _ => None,
    });
    assert!(!mac.node.devices()[0].connected);
}

#[test]
fn history_paste_on_the_origin_device_reads_locally() {
    let (mac, _pc) = pair();
    let image = vec![9u8; 1_000_000];
    let local = mac
        .node
        .publish(clip(vec![OutgoingRepresentation {
            format: IMAGE_PNG.into(),
            source: PayloadSource::Bytes { data: image.clone() },
            name: None,
            thumbnail_png: None,
        }]))
        .unwrap()
        .unwrap();
    let handle = local.representations[0].lazy_handle.clone().unwrap();
    assert_eq!(mac.node.fetch_bytes(handle).unwrap(), image);
    let stored = mac.node.history_item(local.id).unwrap().unwrap();
    let path = stored.representations[0].local_path.clone().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), image);
}

/// Loopback throughput for a lazily fetched file. Run with
/// `cargo test --release -p ctrl-core --test node -- --ignored --nocapture`.
#[test]
#[ignore]
fn throughput_one_gigabyte() {
    let (mac, pc) = pair();
    let src_dir = tempfile::tempdir().unwrap();
    let size = 1024 * 1024 * 1024;
    let (path, digest) = write_random_file(src_dir.path(), "one-gig.bin", size);
    mac.node
        .publish(clip(vec![OutgoingRepresentation {
            format: FILE_URL.into(),
            source: PayloadSource::File { path: path.to_string_lossy().into() },
            name: Some("one-gig.bin".into()),
            thumbnail_png: None,
        }]))
        .unwrap();
    let handle = pc.next_clip().representations[0].lazy_handle.clone().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();
    let dest = dest_dir.path().join("one-gig.bin");
    let (tx, rx) = channel();
    let started = Instant::now();
    pc.node.fetch_to_file(handle, dest.to_string_lossy().into(), Box::new(Progress(tx))).unwrap();
    assert!(rx.recv_timeout(Duration::from_secs(300)).unwrap().is_ok());
    let secs = started.elapsed().as_secs_f64();
    println!("1 GiB in {secs:.2}s = {:.0} MB/s", size as f64 / secs / 1e6);
    assert_eq!(Sha256::digest(std::fs::read(&dest).unwrap()).to_vec(), digest);
}

/// Like [`throughput_one_gigabyte`] but discards the bytes, to separate
/// network cost from disk cost.
#[test]
#[ignore]
fn throughput_network_only() {
    let (mac, pc) = pair();
    let size = 512 * 1024 * 1024;
    mac.node
        .publish(clip(vec![OutgoingRepresentation {
            format: IMAGE_PNG.into(),
            source: PayloadSource::Bytes { data: vec![7u8; size] },
            name: None,
            thumbnail_png: None,
        }]))
        .unwrap();
    let handle = pc.next_clip().representations[0].lazy_handle.clone().unwrap();
    let started = Instant::now();
    let stream = pc.node.fetch(handle, 0).unwrap();
    let mut n = 0;
    while let Some(chunk) = stream.next_chunk().unwrap() {
        n += chunk.len();
    }
    let secs = started.elapsed().as_secs_f64();
    assert_eq!(n, size);
    println!("512 MiB in memory in {secs:.2}s = {:.0} MB/s", size as f64 / secs / 1e6);
}

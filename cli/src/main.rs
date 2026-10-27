//! `ctrl-cli`: a terminal shell for ctrl-core, for trying pairing and
//! transfers between two machines without the GUI apps.
//!
//! ```text
//! mac$ ctrl-cli --name Mac          pc$ ctrl-cli --name PC
//! > pair                             > pair 3f9a1c2e 123456
//! Code: 123 456                      Paired with Mac (3f9a1c2e)
//! > send file ~/Movies/big.mov       > paste
//! ```
//!
//! Secrets go to plain files in the data directory. That is fine for a
//! development tool. The real apps use Keychain and Credential Locker.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use clap::Parser;
use ctrl_core::formats::{self, FILE_URL, IMAGE_PNG, TEXT_PLAIN};
use ctrl_core::{
    Clip, ClipFlags, CtrlError, CtrlNode, DeviceEvent, DeviceInfo, NodeConfig, NodeListener, OutgoingClip,
    OutgoingRepresentation, PayloadSource, Retention, SecretStore, TransferListener,
};

#[derive(Parser)]
#[command(about = "Pair with other Ctrl devices and move clips between them.")]
struct Args {
    /// This device's name, used the first time it runs.
    #[arg(long, default_value = "ctrl-cli")]
    name: String,
    /// Where identity, peers and history are kept. Defaults to ~/.ctrl-cli/<name>.
    #[arg(long)]
    data: Option<PathBuf>,
    /// UDP address to listen on.
    #[arg(long, default_value = "0.0.0.0:0")]
    listen: String,
    /// Don't advertise or browse over mDNS. Pair with an explicit ip:port instead.
    #[arg(long)]
    no_discovery: bool,
}

/// Secrets as files with owner-only permissions. Development use only.
struct FileSecrets(PathBuf);

impl FileSecrets {
    fn path(&self, key: &str) -> PathBuf {
        self.0.join(key)
    }
}

fn store_err(e: std::io::Error) -> CtrlError {
    CtrlError::SecretStore(e.to_string())
}

impl SecretStore for FileSecrets {
    fn get(&self, key: String) -> Result<Option<Vec<u8>>, CtrlError> {
        match std::fs::read(self.path(&key)) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(store_err(e)),
        }
    }

    fn set(&self, key: String, value: Vec<u8>) -> Result<(), CtrlError> {
        let path = self.path(&key);
        std::fs::write(&path, value).map_err(store_err)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(store_err)?;
        }
        Ok(())
    }

    fn delete(&self, key: String) -> Result<(), CtrlError> {
        match std::fs::remove_file(self.path(&key)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(store_err(e)),
            _ => Ok(()),
        }
    }

    fn keys(&self) -> Result<Vec<String>, CtrlError> {
        let mut keys = Vec::new();
        for entry in std::fs::read_dir(&self.0).map_err(store_err)? {
            if let Some(name) = entry.map_err(store_err)?.file_name().to_str() {
                keys.push(name.to_owned());
            }
        }
        Ok(keys)
    }
}

struct Events;

impl NodeListener for Events {
    fn on_device_event(&self, event: DeviceEvent) {
        let line = match event {
            DeviceEvent::Found { device } if device.trusted => return,
            DeviceEvent::Found { device } => {
                format!("Found unpaired device {} ({})", short(&device.device_id), device.fingerprint)
            }
            DeviceEvent::Lost { .. } => return,
            DeviceEvent::Paired { device } => format!("Paired with {}", label(&device)),
            DeviceEvent::Unpaired { device_id } => format!("Unpaired {}", short(&device_id)),
            DeviceEvent::Renamed { device } => format!("{} renamed", label(&device)),
            DeviceEvent::Connected { device_id } => format!("Connected to {}", short(&device_id)),
            DeviceEvent::Disconnected { device_id } => format!("Disconnected from {}", short(&device_id)),
        };
        notice(&line);
    }

    fn on_clip(&self, clip: Clip) {
        notice(&format!("Received {}. Type `paste` to use it.", describe(&clip)));
    }

    fn on_pairing_failed(&self, reason: String) {
        notice(&format!("Pairing failed: {reason}"));
    }
}

fn notice(line: &str) {
    print!("\r\x1b[2K{line}\n> ");
    let _ = std::io::stdout().flush();
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

fn label(d: &DeviceInfo) -> String {
    match &d.name {
        Some(n) => format!("{n} ({})", short(&d.device_id)),
        None => short(&d.device_id).to_owned(),
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn describe(clip: &Clip) -> String {
    let parts: Vec<String> = clip
        .representations
        .iter()
        .map(|r| {
            let what = r.name.clone().unwrap_or_else(|| r.format.clone());
            let how = if r.inline_bytes.is_some() { "" } else { ", lazy" };
            format!("{what} ({}{how})", human_bytes(r.size))
        })
        .collect();
    parts.join(" + ")
}

/// Waits for a download and draws a progress line.
struct Progress {
    started: Instant,
    name: String,
    done: Mutex<Option<Sender<Result<String, String>>>>,
}

impl TransferListener for Progress {
    fn on_progress(&self, received: u64, total: u64) {
        let secs = self.started.elapsed().as_secs_f64().max(0.001);
        let pct = if total == 0 { 100.0 } else { received as f64 * 100.0 / total as f64 };
        print!(
            "\r\x1b[2K{}: {pct:5.1}%  {} of {}  {:.0} MB/s",
            self.name,
            human_bytes(received),
            human_bytes(total),
            received as f64 / secs / 1e6
        );
        let _ = std::io::stdout().flush();
    }

    fn on_complete(&self, path: String) {
        if let Some(tx) = self.done.lock().unwrap().take() {
            let _ = tx.send(Ok(path));
        }
    }

    fn on_failed(&self, message: String, _cancelled: bool) {
        if let Some(tx) = self.done.lock().unwrap().take() {
            let _ = tx.send(Err(message));
        }
    }
}

fn extension(format: &str) -> &'static str {
    match format {
        formats::IMAGE_PNG => "png",
        formats::IMAGE_JPEG => "jpg",
        formats::IMAGE_TIFF => "tiff",
        formats::TEXT_HTML => "html",
        formats::TEXT_RTF => "rtf",
        formats::TEXT_PLAIN => "txt",
        _ => "bin",
    }
}

struct Shell {
    node: Arc<CtrlNode>,
}

impl Shell {
    fn find_device(&self, query: &str) -> Result<DeviceInfo, String> {
        let q = query.to_lowercase();
        let matches: Vec<DeviceInfo> = self
            .node
            .devices()
            .into_iter()
            .filter(|d| d.device_id.starts_with(&q) || d.name.as_deref().is_some_and(|n| n.to_lowercase() == q))
            .collect();
        match matches.len() {
            1 => Ok(matches.into_iter().next().unwrap()),
            0 => Err(format!("no device matches {query:?}; try `devices`")),
            _ => Err(format!("{query:?} matches several devices; use more of the ID")),
        }
    }

    fn run(&self, line: &str) -> Result<bool, String> {
        let words: Vec<&str> = line.split_whitespace().collect();
        let err = |e: CtrlError| e.to_string();
        match words.as_slice() {
            [] => {}
            ["help"] | ["?"] => println!("{HELP}"),
            ["quit"] | ["exit"] => return Ok(false),
            ["id"] => {
                println!("Name:        {}", self.node.device_name());
                println!("Device ID:   {}", self.node.device_id());
                println!("Fingerprint: {}", self.node.fingerprint());
                println!("Listening:   {}", self.node.listen_address().map_err(err)?);
            }
            ["name", name @ ..] if !name.is_empty() => self.node.set_device_name(name.join(" ")).map_err(err)?,
            ["devices"] => {
                let devices = self.node.devices();
                if devices.is_empty() {
                    println!("No devices yet. Run `pair` here, or `pair <device> <code>` on the other machine.");
                }
                for d in devices {
                    let state = match (d.trusted, d.connected) {
                        (true, true) => "paired, connected",
                        (true, false) => "paired",
                        _ => "not paired",
                    };
                    println!("{:<28} {:<18} {}  {}", label(&d), state, d.fingerprint, d.addresses.join(", "));
                }
            }
            ["pair"] => {
                let code = self.node.begin_pairing();
                println!("Code: {} {}", &code.code[..3], &code.code[3..]);
                println!("On the other device run: pair {} {}", short(&self.node.device_id()), code.code);
                let addr: std::net::SocketAddr =
                    self.node.listen_address().map_err(err)?.parse().map_err(|_| "bad address")?;
                let host =
                    if addr.ip().is_unspecified() { "<this machine's IP>".to_owned() } else { addr.ip().to_string() };
                println!("Or, without discovery:   pair {host}:{} {}", addr.port(), code.code);
            }
            ["pair", target, code @ ..] if !code.is_empty() => {
                let target = match target.parse::<std::net::SocketAddr>() {
                    Ok(_) => target.to_string(),
                    Err(_) => self.find_device(target)?.device_id,
                };
                let device = self.node.pair_with(target, code.join("")).map_err(err)?;
                println!("Paired with {} (fingerprint {})", label(&device), device.fingerprint);
            }
            ["unpair", query] => {
                let d = self.find_device(query)?;
                self.node.unpair(d.device_id).map_err(err)?;
            }
            ["rename", query, name @ ..] => {
                let d = self.find_device(query)?;
                let d = self.node.rename_device(d.device_id, name.join(" ")).map_err(err)?;
                println!("Now called {}", label(&d));
            }
            ["send", "text", text @ ..] if !text.is_empty() => self.send(OutgoingRepresentation {
                format: TEXT_PLAIN.into(),
                source: PayloadSource::Bytes { data: text.join(" ").into_bytes() },
                name: None,
                thumbnail_png: None,
            })?,
            ["send", "image", path] => {
                let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
                self.send(OutgoingRepresentation {
                    format: IMAGE_PNG.into(),
                    source: PayloadSource::Bytes { data },
                    name: None,
                    thumbnail_png: None,
                })?
            }
            ["send", "file", path] => {
                let p = Path::new(path);
                self.send(OutgoingRepresentation {
                    format: FILE_URL.into(),
                    source: PayloadSource::File { path: path.to_string() },
                    name: p.file_name().map(|n| n.to_string_lossy().into_owned()),
                    thumbnail_png: None,
                })?
            }
            ["history"] => self.history(20)?,
            ["history", n] => self.history(n.parse().map_err(|_| "history takes a number".to_string())?)?,
            ["paste"] => self.paste(0, Path::new("."))?,
            ["paste", n] => {
                self.paste(n.parse().map_err(|_| "paste takes a history number".to_string())?, Path::new("."))?
            }
            ["paste", n, dir] => {
                self.paste(n.parse().map_err(|_| "paste takes a history number".to_string())?, Path::new(dir))?
            }
            ["clear-history"] => self.node.clear_history().map_err(err)?,
            _ => println!("Unknown command. Type `help`."),
        }
        Ok(true)
    }

    fn send(&self, rep: OutgoingRepresentation) -> Result<(), String> {
        let clip = self
            .node
            .publish(OutgoingClip { representations: vec![rep], flags: ClipFlags::default() })
            .map_err(|e| e.to_string())?
            .expect("only concealed clips are suppressed");
        println!("Sent {}", describe(&clip));
        Ok(())
    }

    fn history(&self, limit: u32) -> Result<(), String> {
        let items = self.node.history(limit, 0).map_err(|e| e.to_string())?;
        if items.is_empty() {
            println!("History is empty.");
        }
        let me = self.node.device_id();
        for (i, clip) in items.iter().enumerate() {
            let from = if clip.origin_device == me { "here".to_owned() } else { short(&clip.origin_device).to_owned() };
            println!("{i:>3}  {:<8}  {}", from, describe(clip));
        }
        Ok(())
    }

    /// Writes history item `n` out: text to the terminal, everything else to
    /// files in `dir`. Lazy payloads are fetched now, which is the point of the
    /// exercise.
    fn paste(&self, n: u32, dir: &Path) -> Result<(), String> {
        let clip = self.node.history(1, n).map_err(|e| e.to_string())?.into_iter().next().ok_or("nothing to paste")?;
        for (i, rep) in clip.representations.iter().enumerate() {
            let file_name =
                rep.name.clone().unwrap_or_else(|| format!("{}-{i}.{}", short(&clip.id), extension(&rep.format)));
            let dest = dir.join(&file_name);

            if let Some(bytes) = &rep.inline_bytes {
                if rep.format == TEXT_PLAIN {
                    println!("{}", String::from_utf8_lossy(bytes));
                } else {
                    std::fs::write(&dest, bytes).map_err(|e| e.to_string())?;
                    println!("Wrote {}", dest.display());
                }
            } else if let Some(handle) = &rep.lazy_handle {
                let (tx, rx) = channel();
                let started = Instant::now();
                let progress = Progress { started, name: file_name.clone(), done: Mutex::new(Some(tx)) };
                self.node
                    .fetch_to_file(handle.clone(), dest.to_string_lossy().into_owned(), Box::new(progress))
                    .map_err(|e| e.to_string())?;
                match rx.recv().map_err(|e| e.to_string())? {
                    Ok(path) => {
                        let secs = started.elapsed().as_secs_f64();
                        println!(
                            "\r\x1b[2KReceived {} in {secs:.1} s ({:.0} MB/s) to {path}",
                            human_bytes(rep.size),
                            rep.size as f64 / secs / 1e6
                        );
                    }
                    Err(e) => return Err(format!("\n{file_name}: {e} (run paste again to resume)")),
                }
            } else if let Some(path) = &rep.local_path {
                println!("{} is on this device at {path}", file_name);
            }
        }
        Ok(())
    }
}

const HELP: &str = "\
id                          Show this device
name <name>                 Rename this device
devices                     List paired and nearby devices
pair                        Show a pairing code for another device to enter
pair <device|ip:port> <code>  Pair with a device that is showing a code
unpair <device>             Forget a paired device (on both sides)
rename <device> <name>      Give a paired device a local name
send text <text>            Share text
send image <file.png>       Share an image (large images move lazily)
send file <path>            Share a file (moves lazily; returns immediately)
history [n]                 List recent clips
paste [n] [dir]             Use clip n (default: newest): print text, save other data,
                            and fetch lazy payloads into dir (default: .)
clear-history               Delete history on this device
quit";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    let data = args.data.unwrap_or_else(|| {
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).unwrap_or_default();
        PathBuf::from(home).join(".ctrl-cli").join(&args.name)
    });
    let secrets = data.join("secrets");
    if let Err(e) = std::fs::create_dir_all(&secrets) {
        eprintln!("{}: {e}", secrets.display());
        std::process::exit(1);
    }

    let node = match CtrlNode::new(
        NodeConfig {
            device_name: args.name,
            data_dir: data.to_string_lossy().into_owned(),
            listen_address: Some(args.listen),
            discovery: !args.no_discovery,
            retention: Retention::default(),
        },
        Box::new(FileSecrets(secrets)),
        Box::new(Events),
    ) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("Could not start: {e}");
            std::process::exit(1);
        }
    };

    println!(
        "{} ({}) listening on {}. Type `help` for commands.",
        node.device_name(),
        short(&node.device_id()),
        node.listen_address().unwrap_or_default()
    );
    let shell = Shell { node: node.clone() };
    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal();
    print!("> ");
    let _ = std::io::stdout().flush();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        match shell.run(line.trim()) {
            Ok(true) => {}
            Ok(false) => {
                node.shutdown();
                return;
            }
            Err(e) => println!("{e}"),
        }
        print!("> ");
        let _ = std::io::stdout().flush();
    }
    if !interactive {
        // Commands came from a pipe. Keep serving lazy payloads until killed.
        println!("\nInput ended. Still serving; press Ctrl-C to stop.");
        loop {
            std::thread::park();
        }
    }
    node.shutdown();
}

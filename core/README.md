# ctrl-core

The Rust library underneath every Ctrl shell. It handles discovery, pairing, encryption, the clip model, lazy transfers, and history. Shells talk to it through one object, `CtrlNode`, using the UniFFI bindings.

## Using it from a shell

```swift
let node = try CtrlNode(config: config, secrets: KeychainStore(), listener: self)
node.publish(clip: clipFromPasteboard())          // returns immediately
// a peer copies something → listener.onClip(clip:)
try node.fetchToFile(handle: rep.lazyHandle!, path: dest, listener: progress)  // on paste
```

- **Threading.** Every method is synchronous. Methods that wait on the network (`pairWith`, `fetch`, `fetchBytes`, `unpair`) block the calling thread, so call them off the main thread. Listener callbacks arrive in order on one dedicated core thread. Calling back into the node from a callback is safe.
- **Secrets.** The shell implements `SecretStore` on top of Keychain or Credential Locker. All keys start with `ctrl.`.
- **Formats.** Clips use canonical format strings (`text/plain`, `image/png`, `public.file-url`, and so on). `canonicalFormatForApple`, `appleTypesForFormat`, `canonicalFormatForWindows` and `windowsFormatsForFormat` map them to and from pasteboard types and Windows clipboard formats. `clipFlagsForApple` and `clipFlagsForWindows` detect concealed and transient markers.
- **Concealed clips.** Pass the flags to `publish`. A concealed clip returns `nil` and is neither sent nor recorded.
- **Files.** Send a copied file as a `public.file-url` representation with a `File { path }` source and its `name`. The receiver gets the file's bytes (lazily, when the file is over 256 KB), writes them to a local file, and puts that file's URL on its own clipboard.

## Protocol

| Layer | What | Why |
|---|---|---|
| Discovery | mDNS `_ctrl._udp`, TXT `id` and `fp` only | Devices find each other without broadcasting names. |
| Transport | QUIC (quinn) over UDP, TLS 1.3 with throwaway certificates | Multiplexed streams and congestion control. TLS provides encryption only. |
| Pairing | SPAKE2 (6-digit code) → HKDF → Noise `XXpsk0` | Code-authenticated exchange of long-term X25519 keys. A wrong code fails at the first message. |
| Sessions | Noise `IK` with the paired static keys | Mutual authentication with no certificates or accounts. |
| Channel binding | Both handshakes put a TLS exporter secret in the prologue | Ties Noise to *this* QUIC connection, so a man in the middle can't splice two TLS sessions, and bulk streams can rely on QUIC's AEAD without a second encryption layer. |

A session has one control stream (clips, renames, unpair) and one bidirectional stream per payload fetch. Each fetch carries a token and a byte offset, which is what makes resume work.

Pairing gives a pairing window three attempts in two minutes. Each attempt gives an online attacker one guess in a million.

## Building

```bash
cargo test --workspace                      # unit and loopback integration tests
cargo test --release -p ctrl-core --test node -- --ignored --nocapture   # throughput
scripts/build-xcframework.sh                # Swift package in bindings/apple (needs the Apple rustup targets)
```

C# bindings come from [uniffi-bindgen-cs](https://github.com/NordSecurity/uniffi-bindgen-cs). Its version has to match the `uniffi` pin in `Cargo.toml`; see `.github/workflows/core.yml`.

## Trying it with `ctrl-cli`

Run `cargo run --release -p ctrl-cli -- --name Mac` on one machine and `--name PC` on the other. Then:

```
mac> pair                        pc> pair <mac-id> <code>
mac> send file ~/big.mov         pc> paste
```

`send` returns as soon as the file is announced. The bytes move when `paste` asks for them.

## Known gaps

- IPv4 only for now.
- Payload handles live in memory. After the sender restarts, older lazy items in a peer's history can no longer be fetched. Inline items and the sender's own history are unaffected.
- A resumed download assumes the source file hasn't changed since the first attempt.

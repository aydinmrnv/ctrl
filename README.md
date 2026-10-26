# Carry

One clipboard across your Mac, PC, and iPhone. You can also send work to another device: copy on one machine and paste on another, open a file on a different computer, or drag a window off your Mac and have the same app open on your PC, on the same page.

> Status: pre-prototype. The issues in the **Prototype** milestone track the work.

---

## Why another clipboard app

Today's shared clipboards fall into two groups. Some are tied to one platform, like Universal Clipboard. The others are cross-platform but rough: they sync plain text through somebody's cloud and look like a settings panel. Carry aims to feel native on every platform it supports and to move more than text.

### What sets Carry apart

| | |
|---|---|
| **Window Handoff** *(optional)* | Drag a window past the edge of your Mac's screen while it's screen-shared to your PC (or onto a configured "handoff edge"). The same app opens on the other machine, in the same state: same browser tab and URL, same document, same folder. It only works when the app is installed on both machines, and it is off by default. |
| **Open on…** | Send a file, URL, or folder to another device and have it **open** there, not just arrive. Right-click → *Open on Windows PC*. |
| **Lazy payloads** | Copying a 4 GB video doesn't send 4 GB. Carry announces *what* was copied and only moves the bytes when you actually paste, then streams them over the LAN at full speed. Small items are sent eagerly so paste stays instant. |
| **Every type, faithfully** | Rich text, HTML, images, files, folders, colors, and URLs keep their original formats. Each side gets the best representation its platform understands (RTF ↔ HTML, TIFF ↔ PNG, file promises ↔ virtual files). |
| **Respects secrets** | Items that password managers mark as concealed (`org.nspasteboard.ConcealedType`, Windows `ExcludeClipboardContentFromMonitorProcessing`) never leave the device and never enter history. |
| **Local-first, end-to-end encrypted** | Devices find each other on the LAN and talk directly. There are no accounts and no server that can read your clipboard. An optional relay carries only ciphertext. |
| **Paste as…** | Paste plain text, paste an image as text using on-device OCR, paste a file as its path or its contents, and paste a URL as a Markdown link. Pick the transform from the menu bar or a modifier key. |

## Platforms

| Platform | Shell | Notes |
|---|---|---|
| macOS 15+ | Menu bar app (SwiftUI + AppKit) | Full feature set. |
| Windows 11 | Notification-area app (WinUI 3) | Full feature set, Fluent / Mica styling. |
| iOS 18+ | App + Share Extension + Widget + Shortcuts | iOS doesn't allow background clipboard access, so sync is user-initiated: paste into Carry, the share sheet, a Control Center control, or a Shortcut. Receiving works in the background through a notification with a copy action. |

## Design principles

- **Native first.** Each platform gets its own UI built with its own toolkit and following its own guidelines (Apple HIG, Windows Fluent). No web views and no shared look across platforms.
- **Quiet by default.** No sounds, no badges, and no notification for every copy. When it works, the clipboard behaves as if it were always shared.
- **System materials, system type, SF Symbols / Segoe Fluent Icons.** No custom chrome, gradients, or invented controls. Dark mode, Increase Contrast, Reduce Motion, VoiceOver, and Narrator are supported from day one.
- **One obvious action per surface.** The menu bar popover shows recent items and devices, and that's all it shows. Everything else is in Settings.

## Architecture

```
┌──────────────────────┐   ┌──────────────────────┐   ┌──────────────────────┐
│  macOS menu bar app   │   │  Windows tray app     │   │  iOS app + extensions │
│  SwiftUI / AppKit     │   │  WinUI 3 / C#         │   │  SwiftUI              │
└──────────┬───────────┘   └──────────┬───────────┘   └──────────┬───────────┘
           │ UniFFI (Swift)            │ UniFFI (C#)               │ UniFFI (Swift)
┌──────────┴───────────────────────────┴───────────────────────────┴───────────┐
│                               carry-core (Rust)                               │
│  discovery (mDNS) · pairing (SPAKE2) · Noise sessions · clip model            │
│  lazy transfer (QUIC streams) · history store (SQLite) · intents (Open on…)   │
└───────────────────────────────────────────────────────────────────────────────┘
```

- **`core/`**: Rust crate that holds all networking, crypto, the data model, and persistence. One implementation, tested once.
- **`apple/`**: Xcode project (XcodeGen) containing the macOS app, the iOS app, and a shared `CarryKit` Swift package.
- **`windows/`**: WinUI 3 solution.

Platform shells handle only the system integration the core can't: pasteboard access, UI, window inspection, and launching apps.

## Roadmap

1. **Prototype** (this milestone): text, images, and files between Mac and Windows over the LAN, plus pairing, history, *Open on…*, and a working Window Handoff for browsers.
2. **iOS**: share extension, widget, Shortcuts actions, and receive notifications.
3. **Polish**: Paste as…, relay for use away from home, Window Handoff for more apps (VS Code, Office, Finder ↔ Explorer, Figma).

## License

Not chosen yet.

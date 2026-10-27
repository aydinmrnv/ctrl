//! The shared format vocabulary and how it maps to platform clipboard types.
//!
//! Clips travel between devices tagged with a *canonical* format string, for
//! example `text/plain` or `image/png`. Each shell converts between canonical
//! formats and its platform's types:
//!
//! - **macOS / iOS**: Uniform Type Identifiers, plus the legacy pasteboard type
//!   names that some apps still write.
//! - **Windows**: clipboard format names. Standard formats use the name of their
//!   `CF_*` constant (`CF_UNICODETEXT`, `CF_HDROP`), and registered formats use
//!   their registered name (`HTML Format`, `PNG`).
//!
//! The table maps type *names* only. Converting the bytes (for example
//! wrapping HTML in the `CF_HTML` header, or UTF-16 for `CF_UNICODETEXT`) is up
//! to the shell.

pub const TEXT_PLAIN: &str = "text/plain";
pub const TEXT_HTML: &str = "text/html";
pub const TEXT_RTF: &str = "text/rtf";
pub const IMAGE_PNG: &str = "image/png";
pub const IMAGE_TIFF: &str = "image/tiff";
pub const IMAGE_JPEG: &str = "image/jpeg";
pub const URL: &str = "text/uri-list";
pub const FILE_URL: &str = "public.file-url";
pub const FILE_LIST: &str = "x-ctrl/file-list";
pub const COLOR: &str = "x-ctrl/color";

struct Mapping {
    canonical: &'static str,
    /// Preferred type first. A shell writing to the pasteboard uses the first
    /// entry, and a shell reading accepts any of them.
    apple: &'static [&'static str],
    windows: &'static [&'static str],
}

const TABLE: &[Mapping] = &[
    Mapping {
        canonical: TEXT_PLAIN,
        apple: &["public.utf8-plain-text", "public.plain-text", "NSStringPboardType"],
        windows: &["CF_UNICODETEXT", "CF_TEXT", "CF_OEMTEXT"],
    },
    Mapping { canonical: TEXT_HTML, apple: &["public.html", "Apple HTML pasteboard type"], windows: &["HTML Format"] },
    Mapping {
        canonical: TEXT_RTF,
        apple: &["public.rtf", "NeXT Rich Text Format v1.0 pasteboard type"],
        windows: &["Rich Text Format"],
    },
    Mapping { canonical: IMAGE_PNG, apple: &["public.png", "Apple PNG pasteboard type"], windows: &["PNG"] },
    Mapping { canonical: IMAGE_TIFF, apple: &["public.tiff", "NeXT TIFF v4.0 pasteboard type"], windows: &["CF_TIFF"] },
    Mapping { canonical: IMAGE_JPEG, apple: &["public.jpeg"], windows: &["JFIF"] },
    Mapping {
        canonical: URL,
        apple: &["public.url", "Apple URL pasteboard type"],
        windows: &["UniformResourceLocatorW", "UniformResourceLocator"],
    },
    Mapping { canonical: FILE_URL, apple: &["public.file-url"], windows: &["FileNameW", "FileName"] },
    Mapping { canonical: FILE_LIST, apple: &["NSFilenamesPboardType"], windows: &["CF_HDROP"] },
    Mapping { canonical: COLOR, apple: &["com.apple.cocoa.pasteboard.color"], windows: &[] },
];

/// Pasteboard markers that mean "never sync this" (see nspasteboard.org).
const APPLE_CONCEALED: &[&str] = &["org.nspasteboard.ConcealedType"];
/// Markers that mean "sync it, but keep it out of history".
const APPLE_TRANSIENT: &[&str] = &["org.nspasteboard.TransientType"];
const WINDOWS_CONCEALED: &[&str] = &["ExcludeClipboardContentFromMonitorProcessing"];
const WINDOWS_TRANSIENT: &[&str] = &["Clipboard Viewer Ignore"];

/// Privacy flags a shell reads off the system clipboard.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, uniffi::Record)]
pub struct ClipFlags {
    /// The clip never leaves this device and is never written to history.
    pub concealed: bool,
    /// The clip is synced but is not written to history.
    pub transient: bool,
}

fn find<'a>(name: &str, side: impl Fn(&Mapping) -> &'a [&'a str]) -> Option<&'static str> {
    TABLE.iter().find(|m| side(m).iter().any(|t| t.eq_ignore_ascii_case(name))).map(|m| m.canonical)
}

fn lookup(canonical: &str) -> Option<&'static Mapping> {
    TABLE.iter().find(|m| m.canonical.eq_ignore_ascii_case(canonical))
}

/// Canonical format for a macOS/iOS pasteboard type, if Ctrl syncs it.
#[uniffi::export]
pub fn canonical_format_for_apple(uti: String) -> Option<String> {
    find(&uti, |m| m.apple).map(str::to_owned)
}

/// Pasteboard types for a canonical format, preferred first.
#[uniffi::export]
pub fn apple_types_for_format(format: String) -> Vec<String> {
    lookup(&format).map(|m| m.apple.iter().map(|s| s.to_string()).collect()).unwrap_or_default()
}

/// Canonical format for a Windows clipboard format name, if Ctrl syncs it.
#[uniffi::export]
pub fn canonical_format_for_windows(format_name: String) -> Option<String> {
    find(&format_name, |m| m.windows).map(str::to_owned)
}

/// Windows clipboard format names for a canonical format, preferred first.
#[uniffi::export]
pub fn windows_formats_for_format(format: String) -> Vec<String> {
    lookup(&format).map(|m| m.windows.iter().map(|s| s.to_string()).collect()).unwrap_or_default()
}

/// Reads the privacy flags from the full list of types on a macOS/iOS pasteboard item.
#[uniffi::export]
pub fn clip_flags_for_apple(types: Vec<String>) -> ClipFlags {
    flags(&types, APPLE_CONCEALED, APPLE_TRANSIENT)
}

/// Reads the privacy flags from the full list of formats on the Windows clipboard.
///
/// Windows also uses `CanIncludeInClipboardHistory` with a DWORD value of 0 to
/// mean "keep out of history". The shell has to read that value itself and set
/// `transient` when it is 0.
#[uniffi::export]
pub fn clip_flags_for_windows(formats: Vec<String>) -> ClipFlags {
    flags(&formats, WINDOWS_CONCEALED, WINDOWS_TRANSIENT)
}

fn flags(types: &[String], concealed: &[&str], transient: &[&str]) -> ClipFlags {
    let has = |set: &[&str]| types.iter().any(|t| set.iter().any(|s| s.eq_ignore_ascii_case(t)));
    ClipFlags { concealed: has(concealed), transient: has(transient) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_canonical_format_is_unique_and_round_trips() {
        for (i, m) in TABLE.iter().enumerate() {
            assert!(TABLE[i + 1..].iter().all(|o| o.canonical != m.canonical), "{} listed twice", m.canonical);
            for uti in m.apple {
                assert_eq!(canonical_format_for_apple(uti.to_string()).as_deref(), Some(m.canonical));
            }
            for fmt in m.windows {
                assert_eq!(canonical_format_for_windows(fmt.to_string()).as_deref(), Some(m.canonical));
            }
            assert_eq!(
                apple_types_for_format(m.canonical.into()).first().map(String::as_str),
                m.apple.first().copied()
            );
            assert_eq!(
                windows_formats_for_format(m.canonical.into()).first().map(String::as_str),
                m.windows.first().copied()
            );
        }
    }

    #[test]
    fn no_platform_type_maps_to_two_formats() {
        let mut seen = std::collections::HashSet::new();
        for m in TABLE {
            for t in m.apple.iter().chain(m.windows) {
                assert!(seen.insert(t.to_ascii_lowercase()), "{t} is mapped twice");
            }
        }
    }

    #[test]
    fn cross_platform_mappings() {
        let win = |uti: &str| windows_formats_for_format(canonical_format_for_apple(uti.into()).unwrap());
        assert_eq!(win("public.utf8-plain-text")[0], "CF_UNICODETEXT");
        assert_eq!(win("public.html")[0], "HTML Format");
        assert_eq!(win("public.rtf")[0], "Rich Text Format");
        assert_eq!(win("public.png")[0], "PNG");
        assert_eq!(win("NSFilenamesPboardType")[0], "CF_HDROP");

        let mac = |f: &str| apple_types_for_format(canonical_format_for_windows(f.into()).unwrap());
        assert_eq!(mac("CF_TEXT")[0], "public.utf8-plain-text");
        assert_eq!(mac("UniformResourceLocatorW")[0], "public.url");
        assert_eq!(mac("FileNameW")[0], "public.file-url");
    }

    #[test]
    fn lookups_ignore_case() {
        assert_eq!(canonical_format_for_apple("PUBLIC.PNG".into()).as_deref(), Some(IMAGE_PNG));
        assert_eq!(canonical_format_for_windows("html format".into()).as_deref(), Some(TEXT_HTML));
        assert_eq!(apple_types_for_format("Text/Plain".into())[0], "public.utf8-plain-text");
    }

    #[test]
    fn unknown_types_are_not_synced() {
        assert_eq!(canonical_format_for_apple("com.adobe.pdf".into()), None);
        assert_eq!(canonical_format_for_windows("CF_BITMAP".into()), None);
        assert!(apple_types_for_format("application/x-nope".into()).is_empty());
        assert!(windows_formats_for_format(COLOR.into()).is_empty());
    }

    #[test]
    fn privacy_markers() {
        let t = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            clip_flags_for_apple(t(&["public.utf8-plain-text", "org.nspasteboard.ConcealedType"])),
            ClipFlags { concealed: true, transient: false }
        );
        assert_eq!(
            clip_flags_for_apple(t(&["org.nspasteboard.TransientType"])),
            ClipFlags { concealed: false, transient: true }
        );
        assert_eq!(
            clip_flags_for_windows(t(&["CF_UNICODETEXT", "ExcludeClipboardContentFromMonitorProcessing"])),
            ClipFlags { concealed: true, transient: false }
        );
        assert_eq!(
            clip_flags_for_windows(t(&["Clipboard Viewer Ignore"])),
            ClipFlags { concealed: false, transient: true }
        );
        assert_eq!(clip_flags_for_apple(t(&["public.png"])), ClipFlags::default());
    }
}

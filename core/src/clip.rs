//! The clip data model: what shells hand to the core, what peers receive, and
//! what goes over the wire.
//!
//! A clip is one copy action. Each of its representations is one way of
//! reading it (plain text, HTML, PNG, and so on). A copied file is a
//! representation with format [`FILE_URL`](crate::formats::FILE_URL) whose
//! bytes are the file's contents and whose `name` is the file name. A copy of
//! several files is a clip with one such representation per file. The
//! receiving shell materializes those bytes into a local file and puts that
//! file's URL on its own clipboard.

use serde::{Deserialize, Serialize};

use crate::error::{CtrlError, Result};
use crate::formats::ClipFlags;

/// Representations up to this size travel inline with the clip. Larger ones
/// travel as a [`LazyHandle`] and are only fetched when someone pastes.
pub const INLINE_LIMIT: u64 = 256 * 1024;

/// Thumbnails larger than this are dropped instead of sent.
pub const THUMBNAIL_LIMIT: usize = 64 * 1024;

/// A clip as the shell sees it, either received from a peer or read from history.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct Clip {
    pub id: String,
    /// The device the clip was copied on. For received clips this is the
    /// authenticated peer, not whatever the peer claims.
    pub origin_device: String,
    /// Unix time in milliseconds.
    pub created_at_ms: i64,
    pub transient: bool,
    pub representations: Vec<Representation>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct Representation {
    /// A canonical format from [`crate::formats`].
    pub format: String,
    pub size: u64,
    pub name: Option<String>,
    pub thumbnail_png: Option<Vec<u8>>,
    /// The bytes, when they travelled inline. A received representation has
    /// either `inline_bytes` or `lazy_handle`.
    pub inline_bytes: Option<Vec<u8>>,
    pub lazy_handle: Option<LazyHandle>,
    /// A file on this device that holds the bytes: the source file of
    /// something copied here, or a history blob. Shells can paste from it
    /// directly instead of fetching.
    pub local_path: Option<String>,
}

/// A reference to bytes that are still on the origin device. Pass it to
/// `CtrlNode::fetch` or `CtrlNode::fetch_to_file`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Record)]
pub struct LazyHandle {
    pub device_id: String,
    pub token: String,
    pub size: u64,
}

/// A clip the shell has just read from the system clipboard.
#[derive(Debug, Clone, uniffi::Record)]
pub struct OutgoingClip {
    pub representations: Vec<OutgoingRepresentation>,
    pub flags: ClipFlags,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct OutgoingRepresentation {
    pub format: String,
    pub source: PayloadSource,
    pub name: Option<String>,
    pub thumbnail_png: Option<Vec<u8>>,
}

/// Where the bytes of an outgoing representation come from.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum PayloadSource {
    Bytes {
        data: Vec<u8>,
    },
    /// A file the core reads on demand. Large files are never loaded into memory.
    File {
        path: String,
    },
}

/// The clip as it travels between peers. Deliberately has no `concealed`
/// field: concealed clips are rejected by [`prepare`] and never reach the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WireClip {
    pub id: String,
    pub created_at_ms: i64,
    pub transient: bool,
    pub representations: Vec<WireRepresentation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WireRepresentation {
    pub format: String,
    pub size: u64,
    pub name: Option<String>,
    #[serde(with = "serde_bytes")]
    pub thumbnail_png: Option<Vec<u8>>,
    pub body: WireBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum WireBody {
    Inline(#[serde(with = "serde_bytes")] Vec<u8>),
    Lazy { token: String },
}

/// A large payload this device is holding until a peer fetches it.
#[derive(Debug, Clone)]
pub(crate) enum LocalPayload {
    Bytes(std::sync::Arc<Vec<u8>>),
    File(std::path::PathBuf),
}

/// The result of turning an [`OutgoingClip`] into something we can send.
pub(crate) struct Prepared {
    /// The clip as this device's own history and shell see it.
    pub local: Clip,
    pub wire: WireClip,
    /// Payloads to register so peers can fetch them.
    pub payloads: Vec<(String, LocalPayload)>,
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or_default()
}

pub(crate) fn new_token() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

/// Validates an outgoing clip, inlines the small representations and assigns
/// handles to the large ones.
///
/// This is the only way to build a [`WireClip`], and it refuses concealed
/// clips, so a concealed clip cannot be transmitted by accident.
pub(crate) fn prepare(clip: OutgoingClip, self_id: &str, inline_limit: u64) -> Result<Prepared> {
    if clip.flags.concealed {
        return Err(CtrlError::Concealed);
    }
    if clip.representations.is_empty() {
        return Err(CtrlError::InvalidArgument("a clip needs at least one representation".into()));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let created_at_ms = now_ms();
    let mut local_reps = Vec::new();
    let mut wire_reps = Vec::new();
    let mut payloads = Vec::new();

    for rep in clip.representations {
        if rep.format.trim().is_empty() {
            return Err(CtrlError::InvalidArgument("representation format is empty".into()));
        }
        let thumbnail_png = rep.thumbnail_png.filter(|t| t.len() <= THUMBNAIL_LIMIT);
        let mut local_path = None;
        let (size, inline, payload) = match rep.source {
            PayloadSource::Bytes { data } => {
                let size = data.len() as u64;
                if size <= inline_limit {
                    (size, Some(data), None)
                } else {
                    (size, None, Some(LocalPayload::Bytes(std::sync::Arc::new(data))))
                }
            }
            PayloadSource::File { path } => {
                let path = std::path::PathBuf::from(path);
                let meta = std::fs::metadata(&path)
                    .map_err(|e| CtrlError::InvalidArgument(format!("{}: {e}", path.display())))?;
                if !meta.is_file() {
                    return Err(CtrlError::InvalidArgument(format!("{} is not a file", path.display())));
                }
                let size = meta.len();
                local_path = Some(path.to_string_lossy().into_owned());
                if size <= inline_limit {
                    (size, Some(std::fs::read(&path)?), None)
                } else {
                    (size, None, Some(LocalPayload::File(path)))
                }
            }
        };

        let (body, handle, inline_bytes) = match (inline, payload) {
            (Some(bytes), _) => (WireBody::Inline(bytes.clone()), None, Some(bytes)),
            (None, Some(payload)) => {
                let token = new_token();
                payloads.push((token.clone(), payload));
                let handle = LazyHandle { device_id: self_id.to_owned(), token: token.clone(), size };
                (WireBody::Lazy { token }, Some(handle), None)
            }
            (None, None) => unreachable!("every source yields inline bytes or a payload"),
        };

        wire_reps.push(WireRepresentation {
            format: rep.format.clone(),
            size,
            name: rep.name.clone(),
            thumbnail_png: thumbnail_png.clone(),
            body,
        });
        local_reps.push(Representation {
            format: rep.format,
            size,
            name: rep.name,
            thumbnail_png,
            inline_bytes,
            lazy_handle: handle,
            local_path,
        });
    }

    Ok(Prepared {
        local: Clip {
            id: id.clone(),
            origin_device: self_id.to_owned(),
            created_at_ms,
            transient: clip.flags.transient,
            representations: local_reps,
        },
        wire: WireClip { id, created_at_ms, transient: clip.flags.transient, representations: wire_reps },
        payloads,
    })
}

impl WireClip {
    /// Converts a received clip into the shell's model. `peer_id` is the
    /// authenticated sender and overrides anything the sender could claim.
    pub(crate) fn into_clip(self, peer_id: &str) -> Clip {
        Clip {
            id: self.id,
            origin_device: peer_id.to_owned(),
            created_at_ms: self.created_at_ms,
            transient: self.transient,
            representations: self
                .representations
                .into_iter()
                .map(|r| {
                    let (inline_bytes, lazy_handle) = match r.body {
                        WireBody::Inline(b) => (Some(b), None),
                        WireBody::Lazy { token } => {
                            (None, Some(LazyHandle { device_id: peer_id.to_owned(), token, size: r.size }))
                        }
                    };
                    Representation {
                        format: r.format,
                        size: r.size,
                        name: r.name,
                        thumbnail_png: r.thumbnail_png,
                        inline_bytes,
                        lazy_handle,
                        local_path: None,
                    }
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::{FILE_URL, TEXT_PLAIN};

    fn text(s: &str) -> OutgoingRepresentation {
        OutgoingRepresentation {
            format: TEXT_PLAIN.into(),
            source: PayloadSource::Bytes { data: s.as_bytes().to_vec() },
            name: None,
            thumbnail_png: None,
        }
    }

    #[test]
    fn concealed_clips_are_rejected_before_they_reach_the_wire() {
        let clip = OutgoingClip {
            representations: vec![text("hunter2")],
            flags: ClipFlags { concealed: true, transient: false },
        };
        assert!(matches!(prepare(clip, "me", INLINE_LIMIT), Err(CtrlError::Concealed)));
    }

    #[test]
    fn small_payloads_inline_and_large_ones_go_lazy() {
        let big = vec![7u8; INLINE_LIMIT as usize + 1];
        let clip = OutgoingClip {
            representations: vec![
                text("hello"),
                OutgoingRepresentation {
                    format: "image/png".into(),
                    source: PayloadSource::Bytes { data: big },
                    name: None,
                    thumbnail_png: Some(vec![1; THUMBNAIL_LIMIT + 1]),
                },
            ],
            flags: ClipFlags::default(),
        };
        let p = prepare(clip, "me", INLINE_LIMIT).unwrap();
        assert_eq!(p.wire.representations[0].body, WireBody::Inline(b"hello".to_vec()));
        assert!(matches!(p.wire.representations[1].body, WireBody::Lazy { .. }));
        assert_eq!(p.wire.representations[1].thumbnail_png, None, "oversized thumbnail is dropped");
        assert_eq!(p.payloads.len(), 1);
        assert_eq!(p.local.representations[1].lazy_handle.as_ref().unwrap().size, INLINE_LIMIT + 1);
    }

    #[test]
    fn inline_limit_is_inclusive() {
        let exact = vec![0u8; INLINE_LIMIT as usize];
        let clip = OutgoingClip {
            representations: vec![OutgoingRepresentation {
                format: "image/png".into(),
                source: PayloadSource::Bytes { data: exact },
                name: None,
                thumbnail_png: None,
            }],
            flags: ClipFlags::default(),
        };
        let p = prepare(clip, "me", INLINE_LIMIT).unwrap();
        assert!(matches!(p.wire.representations[0].body, WireBody::Inline(_)));
    }

    #[test]
    fn large_files_are_not_read_into_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        std::fs::write(&path, vec![1u8; INLINE_LIMIT as usize * 2]).unwrap();
        let clip = OutgoingClip {
            representations: vec![OutgoingRepresentation {
                format: FILE_URL.into(),
                source: PayloadSource::File { path: path.to_string_lossy().into() },
                name: Some("big.bin".into()),
                thumbnail_png: None,
            }],
            flags: ClipFlags::default(),
        };
        let p = prepare(clip, "me", INLINE_LIMIT).unwrap();
        assert!(matches!(&p.payloads[0].1, LocalPayload::File(f) if f == &path));
    }

    #[test]
    fn received_clips_take_their_origin_from_the_authenticated_peer() {
        let p = prepare(
            OutgoingClip { representations: vec![text("x")], flags: ClipFlags::default() },
            "claimed",
            INLINE_LIMIT,
        )
        .unwrap();
        let bytes = postcard::to_stdvec(&p.wire).unwrap();
        let wire: WireClip = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(wire.into_clip("actual").origin_device, "actual");
    }
}

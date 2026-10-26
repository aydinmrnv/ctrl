//! `ctrl-core` owns everything below the UI: discovery, pairing, encryption,
//! the clip model, lazy transfers and history. Shells reach it through the
//! UniFFI bindings and never touch sockets or crypto directly.
//!
//! Start with [`CtrlNode`].

uniffi::setup_scaffolding!();

pub mod clip;
mod discovery;
pub mod error;
pub mod formats;
pub mod history;
mod node;
pub mod pairing;
mod payload;
mod session;
mod transport;
pub mod trust;
mod wire;

pub use clip::{Clip, LazyHandle, OutgoingClip, OutgoingRepresentation, PayloadSource, Representation, INLINE_LIMIT};
pub use error::{CtrlError, Result};
pub use formats::ClipFlags;
pub use history::Retention;
pub use node::{
    CtrlNode, DeviceEvent, DeviceInfo, FetchStream, NodeConfig, NodeListener, PairingCode, Transfer, TransferListener,
};
pub use trust::{fingerprint, MemorySecretStore, SecretStore};

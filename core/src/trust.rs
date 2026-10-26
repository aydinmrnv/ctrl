//! This device's identity and the peers it trusts, kept in the platform keystore.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{CtrlError, Result};

/// Persistent secret storage implemented by the shell: Keychain on Apple
/// platforms, Credential Locker or DPAPI on Windows.
///
/// Values are opaque bytes. Every key starts with `ctrl.`, so the shell can
/// keep them under a single service or resource name.
#[uniffi::export(callback_interface)]
pub trait SecretStore: Send + Sync {
    fn get(&self, key: String) -> Result<Option<Vec<u8>>, CtrlError>;
    fn set(&self, key: String, value: Vec<u8>) -> Result<(), CtrlError>;
    fn delete(&self, key: String) -> Result<(), CtrlError>;
    /// Every key currently stored.
    fn keys(&self) -> Result<Vec<String>, CtrlError>;
}

/// An in-memory [`SecretStore`] for tests and throwaway nodes.
#[derive(Default)]
pub struct MemorySecretStore(Mutex<HashMap<String, Vec<u8>>>);

impl SecretStore for MemorySecretStore {
    fn get(&self, key: String) -> Result<Option<Vec<u8>>> {
        Ok(self.0.lock().unwrap().get(&key).cloned())
    }
    fn set(&self, key: String, value: Vec<u8>) -> Result<()> {
        self.0.lock().unwrap().insert(key, value);
        Ok(())
    }
    fn delete(&self, key: String) -> Result<()> {
        self.0.lock().unwrap().remove(&key);
        Ok(())
    }
    fn keys(&self) -> Result<Vec<String>> {
        Ok(self.0.lock().unwrap().keys().cloned().collect())
    }
}

const IDENTITY_KEY: &str = "ctrl.identity";
const PEER_PREFIX: &str = "ctrl.peer.";

/// The Noise pattern parameters everything in Ctrl uses for static keys.
pub(crate) const NOISE_KEY_PARAMS: &str = "Noise_IK_25519_ChaChaPoly_SHA256";

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Identity {
    pub device_id: String,
    pub name: String,
    pub private_key: Vec<u8>,
    pub public_key: Vec<u8>,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity").field("device_id", &self.device_id).field("name", &self.name).finish()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PeerRecord {
    pub device_id: String,
    /// The name the peer gave itself.
    pub name: String,
    /// A local rename, which takes precedence over `name`.
    pub alias: Option<String>,
    pub public_key: Vec<u8>,
    pub paired_at_ms: i64,
    pub last_address: Option<String>,
}

impl PeerRecord {
    pub fn display_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

/// A short, stable fingerprint of a static public key. It is published over
/// mDNS and shown in the UI so people can tell devices apart.
pub fn fingerprint(public_key: &[u8]) -> String {
    hex::encode(&Sha256::digest(public_key)[..10])
}

pub(crate) struct TrustStore {
    store: Arc<dyn SecretStore>,
    identity: RwLock<Identity>,
    peers: RwLock<HashMap<String, PeerRecord>>,
}

impl TrustStore {
    /// Loads the identity and peers, creating an identity on first run.
    pub fn open(store: Arc<dyn SecretStore>, default_name: &str) -> Result<Self> {
        let identity = match store.get(IDENTITY_KEY.into())? {
            Some(bytes) => postcard::from_bytes::<Identity>(&bytes)
                .map_err(|e| CtrlError::SecretStore(format!("identity is corrupt: {e}")))?,
            None => {
                let kp = snow::Builder::new(NOISE_KEY_PARAMS.parse().unwrap())
                    .generate_keypair()
                    .map_err(|e| CtrlError::SecretStore(format!("key generation failed: {e}")))?;
                let identity = Identity {
                    device_id: uuid::Uuid::new_v4().simple().to_string(),
                    name: default_name.to_owned(),
                    private_key: kp.private,
                    public_key: kp.public,
                };
                store.set(IDENTITY_KEY.into(), postcard::to_stdvec(&identity)?)?;
                identity
            }
        };

        let mut peers = HashMap::new();
        for key in store.keys()? {
            if !key.starts_with(PEER_PREFIX) {
                continue;
            }
            let Some(bytes) = store.get(key.clone())? else { continue };
            match postcard::from_bytes::<PeerRecord>(&bytes) {
                Ok(peer) => {
                    peers.insert(peer.device_id.clone(), peer);
                }
                Err(e) => tracing::warn!("skipping corrupt peer record {key}: {e}"),
            }
        }

        Ok(Self { store, identity: RwLock::new(identity), peers: RwLock::new(peers) })
    }

    pub fn identity(&self) -> Identity {
        self.identity.read().unwrap().clone()
    }

    pub fn device_id(&self) -> String {
        self.identity.read().unwrap().device_id.clone()
    }

    pub fn set_name(&self, name: &str) -> Result<()> {
        let mut id = self.identity.write().unwrap();
        let mut updated = id.clone();
        updated.name = name.to_owned();
        self.store.set(IDENTITY_KEY.into(), postcard::to_stdvec(&updated)?)?;
        *id = updated;
        Ok(())
    }

    pub fn peer(&self, device_id: &str) -> Option<PeerRecord> {
        self.peers.read().unwrap().get(device_id).cloned()
    }

    pub fn peer_by_key(&self, public_key: &[u8]) -> Option<PeerRecord> {
        self.peers.read().unwrap().values().find(|p| p.public_key == public_key).cloned()
    }

    pub fn peers(&self) -> Vec<PeerRecord> {
        let mut v: Vec<_> = self.peers.read().unwrap().values().cloned().collect();
        v.sort_by_key(|p| p.paired_at_ms);
        v
    }

    pub fn save_peer(&self, peer: PeerRecord) -> Result<()> {
        self.store.set(format!("{PEER_PREFIX}{}", peer.device_id), postcard::to_stdvec(&peer)?)?;
        self.peers.write().unwrap().insert(peer.device_id.clone(), peer);
        Ok(())
    }

    /// Applies `f` to a stored peer and persists the result.
    pub fn update_peer(&self, device_id: &str, f: impl FnOnce(&mut PeerRecord)) -> Result<Option<PeerRecord>> {
        let Some(mut peer) = self.peer(device_id) else { return Ok(None) };
        let before = peer.clone();
        f(&mut peer);
        if peer != before {
            self.save_peer(peer.clone())?;
        }
        Ok(Some(peer))
    }

    pub fn remove_peer(&self, device_id: &str) -> Result<bool> {
        self.store.delete(format!("{PEER_PREFIX}{device_id}"))?;
        Ok(self.peers.write().unwrap().remove(device_id).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_created_once_and_reloaded() {
        let store: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
        let a = TrustStore::open(store.clone(), "Mac").unwrap().identity();
        let b = TrustStore::open(store, "ignored").unwrap().identity();
        assert_eq!(a.device_id, b.device_id);
        assert_eq!(a.public_key, b.public_key);
        assert_eq!(b.name, "Mac");
        assert_eq!(a.public_key.len(), 32);
    }

    #[test]
    fn peers_persist_rename_and_remove() {
        let store: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
        let trust = TrustStore::open(store.clone(), "Mac").unwrap();
        trust
            .save_peer(PeerRecord {
                device_id: "pc".into(),
                name: "DESKTOP-1".into(),
                alias: None,
                public_key: vec![9; 32],
                paired_at_ms: 1,
                last_address: None,
            })
            .unwrap();
        trust.update_peer("pc", |p| p.alias = Some("Gaming PC".into())).unwrap();

        let reloaded = TrustStore::open(store.clone(), "Mac").unwrap();
        let pc = reloaded.peer("pc").unwrap();
        assert_eq!(pc.display_name(), "Gaming PC");
        assert_eq!(reloaded.peer_by_key(&[9; 32]).unwrap().device_id, "pc");

        assert!(reloaded.remove_peer("pc").unwrap());
        assert!(TrustStore::open(store, "Mac").unwrap().peer("pc").is_none());
    }
}

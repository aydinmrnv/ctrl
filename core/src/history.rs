//! Clip history in SQLite. Small payloads live in the database. Anything over
//! [`BLOB_THRESHOLD`] is written to a file in `blobs/` next to it, and the
//! database stores only the file name.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

use crate::clip::{now_ms, Clip, LazyHandle, LocalPayload, Representation, INLINE_LIMIT};
use crate::error::Result;

const BLOB_THRESHOLD: usize = 32 * 1024;
const SCHEMA_VERSION: i32 = 1;

/// How much history to keep. Zero means no limit.
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct Retention {
    pub max_items: u32,
    pub max_age_days: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Self { max_items: 200, max_age_days: 30 }
    }
}

pub(crate) struct History {
    db: Mutex<Connection>,
    blobs: PathBuf,
    retention: Mutex<Retention>,
}

impl History {
    pub fn open(dir: &Path, retention: Retention) -> Result<Self> {
        let blobs = dir.join("blobs");
        std::fs::create_dir_all(&blobs)?;
        let db = Connection::open(dir.join("history.sqlite3"))?;
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS clips (
                 id            TEXT PRIMARY KEY,
                 origin_device TEXT NOT NULL,
                 created_at_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS clips_by_time ON clips (created_at_ms);
             CREATE TABLE IF NOT EXISTS representations (
                 clip_id      TEXT NOT NULL REFERENCES clips (id) ON DELETE CASCADE,
                 idx          INTEGER NOT NULL,
                 format       TEXT NOT NULL,
                 size         INTEGER NOT NULL,
                 name         TEXT,
                 thumbnail    BLOB,
                 inline_bytes BLOB,
                 blob_file    TEXT,
                 source_path  TEXT,
                 lazy_device  TEXT,
                 lazy_token   TEXT,
                 PRIMARY KEY (clip_id, idx)
             );",
        )?;
        db.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        let history = Self { db: Mutex::new(db), blobs, retention: Mutex::new(retention) };
        history.prune()?;
        Ok(history)
    }

    pub fn set_retention(&self, retention: Retention) -> Result<()> {
        *self.retention.lock().unwrap() = retention;
        self.prune()
    }

    /// Records a clip. Transient clips are skipped. Concealed clips never get
    /// this far, because [`crate::clip::prepare`] rejects them.
    ///
    /// `local` holds payloads this device is serving, so a large image copied
    /// here is kept in history even after it stops being fetchable.
    pub fn insert(&self, clip: &Clip, local: &[(String, LocalPayload)]) -> Result<()> {
        if clip.transient {
            return Ok(());
        }
        let mut written = Vec::new();
        let result = (|| -> Result<()> {
            let mut db = self.db.lock().unwrap();
            let tx = db.transaction()?;
            tx.execute(
                "INSERT OR REPLACE INTO clips (id, origin_device, created_at_ms) VALUES (?1, ?2, ?3)",
                params![clip.id, clip.origin_device, clip.created_at_ms],
            )?;
            for (idx, rep) in clip.representations.iter().enumerate() {
                let local_payload =
                    rep.lazy_handle.as_ref().and_then(|h| local.iter().find(|(t, _)| *t == h.token)).map(|(_, p)| p);

                let mut inline = rep.inline_bytes.as_deref();
                let mut blob_file = None;
                let mut source_path = rep.local_path.clone();
                let blob_bytes = match (inline, local_payload) {
                    (Some(b), _) if b.len() > BLOB_THRESHOLD => Some(b),
                    (None, Some(LocalPayload::Bytes(b))) => Some(b.as_slice()),
                    _ => None,
                };
                if let Some(bytes) = blob_bytes {
                    let file = format!("{}-{idx}", clip.id);
                    std::fs::write(self.blobs.join(&file), bytes)?;
                    written.push(file.clone());
                    blob_file = Some(file);
                    inline = None;
                }
                if let Some(LocalPayload::File(p)) = local_payload {
                    source_path = Some(p.to_string_lossy().into_owned());
                }
                tx.execute(
                    "INSERT INTO representations
                       (clip_id, idx, format, size, name, thumbnail, inline_bytes, blob_file, source_path, lazy_device, lazy_token)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        clip.id,
                        idx as i64,
                        rep.format,
                        rep.size as i64,
                        rep.name,
                        rep.thumbnail_png,
                        inline,
                        blob_file,
                        source_path,
                        rep.lazy_handle.as_ref().map(|h| &h.device_id),
                        rep.lazy_handle.as_ref().map(|h| &h.token),
                    ],
                )?;
            }
            tx.commit()?;
            Ok(())
        })();
        if result.is_err() {
            for f in written {
                let _ = std::fs::remove_file(self.blobs.join(f));
            }
        }
        result?;
        self.prune()
    }

    /// Newest first.
    pub fn list(&self, limit: u32, offset: u32) -> Result<Vec<Clip>> {
        let ids: Vec<(String, String, i64)> = {
            let db = self.db.lock().unwrap();
            let mut stmt = db.prepare(
                "SELECT id, origin_device, created_at_ms FROM clips
                 ORDER BY created_at_ms DESC, rowid DESC LIMIT ?1 OFFSET ?2",
            )?;
            let rows = stmt.query_map(params![limit, offset], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        ids.into_iter().map(|(id, origin, at)| self.load(id, origin, at)).collect()
    }

    pub fn get(&self, id: &str) -> Result<Option<Clip>> {
        let row: Option<(String, String, i64)> = {
            let db = self.db.lock().unwrap();
            db.query_row("SELECT id, origin_device, created_at_ms FROM clips WHERE id = ?1", params![id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?
        };
        row.map(|(id, origin, at)| self.load(id, origin, at)).transpose()
    }

    fn load(&self, id: String, origin_device: String, created_at_ms: i64) -> Result<Clip> {
        type Row = (
            String,
            i64,
            Option<String>,
            Option<Vec<u8>>,
            Option<Vec<u8>>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let rows: Vec<Row> = {
            let db = self.db.lock().unwrap();
            let mut stmt = db.prepare(
                "SELECT format, size, name, thumbnail, inline_bytes, blob_file, source_path, lazy_device, lazy_token
                 FROM representations WHERE clip_id = ?1 ORDER BY idx",
            )?;
            let rows = stmt.query_map(params![id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };

        let mut representations = Vec::with_capacity(rows.len());
        for (format, size, name, thumbnail_png, mut inline_bytes, blob_file, source_path, lazy_device, lazy_token) in
            rows
        {
            let size = size as u64;
            let mut local_path = source_path;
            if let Some(file) = blob_file {
                let path = self.blobs.join(file);
                if size <= INLINE_LIMIT {
                    inline_bytes = std::fs::read(&path).ok();
                } else {
                    local_path = Some(path.to_string_lossy().into_owned());
                }
            }
            let lazy_handle = match (lazy_device, lazy_token) {
                (Some(device_id), Some(token)) => Some(LazyHandle { device_id, token, size }),
                _ => None,
            };
            representations.push(Representation {
                format,
                size,
                name,
                thumbnail_png,
                inline_bytes,
                lazy_handle,
                local_path,
            });
        }
        Ok(Clip { id, origin_device, created_at_ms, transient: false, representations })
    }

    pub fn delete(&self, id: &str) -> Result<bool> {
        self.delete_where("id = ?1", params![id]).map(|n| n > 0)
    }

    pub fn clear(&self) -> Result<()> {
        self.delete_where("1", params![]).map(|_| ())
    }

    fn prune(&self) -> Result<()> {
        let r = *self.retention.lock().unwrap();
        if r.max_age_days > 0 {
            let cutoff = now_ms() - r.max_age_days as i64 * 86_400_000;
            self.delete_where("created_at_ms < ?1", params![cutoff])?;
        }
        if r.max_items > 0 {
            self.delete_where(
                "id NOT IN (SELECT id FROM clips ORDER BY created_at_ms DESC, rowid DESC LIMIT ?1)",
                params![r.max_items],
            )?;
        }
        Ok(())
    }

    fn delete_where(&self, filter: &str, args: &[&dyn rusqlite::ToSql]) -> Result<usize> {
        let (files, n) = {
            let mut db = self.db.lock().unwrap();
            let tx = db.transaction()?;
            let files: Vec<String> = {
                let mut stmt = tx.prepare(&format!(
                    "SELECT blob_file FROM representations
                     WHERE blob_file IS NOT NULL AND clip_id IN (SELECT id FROM clips WHERE {filter})"
                ))?;
                let rows = stmt.query_map(args, |r| r.get(0))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            let n = tx.execute(&format!("DELETE FROM clips WHERE {filter}"), args)?;
            tx.commit()?;
            (files, n)
        };
        for f in files {
            let _ = std::fs::remove_file(self.blobs.join(f));
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn clip(id: &str, at: i64, reps: Vec<Representation>) -> Clip {
        Clip { id: id.into(), origin_device: "mac".into(), created_at_ms: at, transient: false, representations: reps }
    }

    fn inline(bytes: Vec<u8>) -> Representation {
        Representation {
            format: "text/plain".into(),
            size: bytes.len() as u64,
            name: None,
            thumbnail_png: None,
            inline_bytes: Some(bytes),
            lazy_handle: None,
            local_path: None,
        }
    }

    fn blob_count(dir: &Path) -> usize {
        std::fs::read_dir(dir.join("blobs")).unwrap().count()
    }

    #[test]
    fn round_trips_small_and_large_inline_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::open(dir.path(), Retention::default()).unwrap();
        let big = vec![3u8; BLOB_THRESHOLD + 1];
        let c = clip("a", now_ms(), vec![inline(b"hi".to_vec()), inline(big.clone())]);
        h.insert(&c, &[]).unwrap();
        assert_eq!(blob_count(dir.path()), 1, "large payload is stored as a file");
        assert_eq!(h.get("a").unwrap().unwrap(), c);
    }

    #[test]
    fn transient_clips_are_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::open(dir.path(), Retention::default()).unwrap();
        let mut c = clip("a", now_ms(), vec![inline(b"x".to_vec())]);
        c.transient = true;
        h.insert(&c, &[]).unwrap();
        assert!(h.list(10, 0).unwrap().is_empty());
    }

    #[test]
    fn local_lazy_bytes_are_kept_as_a_blob_file() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::open(dir.path(), Retention::default()).unwrap();
        let big = vec![1u8; INLINE_LIMIT as usize + 10];
        let rep = Representation {
            format: "image/png".into(),
            size: big.len() as u64,
            name: None,
            thumbnail_png: None,
            inline_bytes: None,
            lazy_handle: Some(LazyHandle { device_id: "mac".into(), token: "t".into(), size: big.len() as u64 }),
            local_path: None,
        };
        h.insert(&clip("a", now_ms(), vec![rep]), &[("t".into(), LocalPayload::Bytes(Arc::new(big.clone())))]).unwrap();
        let got = h.get("a").unwrap().unwrap();
        let path = got.representations[0].local_path.clone().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), big);
    }

    #[test]
    fn retention_by_count_and_age_deletes_blob_files() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::open(dir.path(), Retention { max_items: 2, max_age_days: 1 }).unwrap();
        let big = || inline(vec![0u8; BLOB_THRESHOLD + 1]);
        let now = now_ms();
        h.insert(&clip("ancient", now - 2 * 86_400_000, vec![big()]), &[]).unwrap();
        assert!(h.get("ancient").unwrap().is_none(), "older than max age");
        for (i, id) in ["one", "two", "three"].iter().enumerate() {
            h.insert(&clip(id, now + i as i64, vec![big()]), &[]).unwrap();
        }
        let ids: Vec<_> = h.list(10, 0).unwrap().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, ["three", "two"]);
        assert_eq!(blob_count(dir.path()), 2);

        h.set_retention(Retention { max_items: 1, max_age_days: 0 }).unwrap();
        assert_eq!(h.list(10, 0).unwrap().len(), 1);
        assert_eq!(blob_count(dir.path()), 1);
    }

    #[test]
    fn delete_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::open(dir.path(), Retention::default()).unwrap();
        h.insert(&clip("a", now_ms(), vec![inline(vec![0; BLOB_THRESHOLD + 1])]), &[]).unwrap();
        h.insert(&clip("b", now_ms(), vec![inline(b"b".to_vec())]), &[]).unwrap();
        assert!(h.delete("a").unwrap());
        assert!(!h.delete("a").unwrap());
        assert_eq!(blob_count(dir.path()), 0);
        h.clear().unwrap();
        assert!(h.list(10, 0).unwrap().is_empty());
    }

    #[test]
    fn survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        History::open(dir.path(), Retention::default())
            .unwrap()
            .insert(&clip("a", now_ms(), vec![inline(b"x".to_vec())]), &[])
            .unwrap();
        let h = History::open(dir.path(), Retention::default()).unwrap();
        assert_eq!(h.list(10, 0).unwrap().len(), 1);
    }
}

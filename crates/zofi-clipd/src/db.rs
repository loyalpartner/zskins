use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::model::{Entry, Kind, MimeContent};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS items (
    uuid          TEXT    PRIMARY KEY,
    kind          TEXT    NOT NULL CHECK(kind IN ('text', 'image')),
    primary_mime  TEXT    NOT NULL,
    primary_hash  BLOB    NOT NULL UNIQUE,
    preview       TEXT,
    created_at    INTEGER NOT NULL,
    last_used_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS mimes (
    item_uuid  TEXT NOT NULL REFERENCES items(uuid) ON DELETE CASCADE,
    mime       TEXT NOT NULL,
    content    BLOB NOT NULL,
    PRIMARY KEY (item_uuid, mime)
);

CREATE INDEX IF NOT EXISTS idx_items_last_used ON items(last_used_at DESC);
"#;

pub struct Db {
    conn: Connection,
}

pub enum RecordResult {
    Inserted(String),
    Existed(String),
}

impl RecordResult {
    pub fn uuid(self) -> String {
        match self {
            RecordResult::Inserted(u) | RecordResult::Existed(u) => u,
        }
    }
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create db parent dir {parent:?}"))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open db {path:?}"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Record a freshly-synced item. Dedups via the primary mime's content
    /// hash. On insert, all `extras` are stored too. On dedup, missing extras
    /// from the new sync are added (a previous capture might have missed mimes
    /// the source app now offers).
    pub fn record(
        &self,
        kind: Kind,
        primary_mime: &str,
        primary_content: &[u8],
        preview: Option<&str>,
        extras: &[MimeContent],
    ) -> Result<RecordResult> {
        let now = unix_now();
        self.record_with_ts(kind, primary_mime, primary_content, preview, extras, now)
    }

    pub fn record_with_ts(
        &self,
        kind: Kind,
        primary_mime: &str,
        primary_content: &[u8],
        preview: Option<&str>,
        extras: &[MimeContent],
        ts: i64,
    ) -> Result<RecordResult> {
        let hash = blake3::hash(primary_content);
        let hash_bytes = hash.as_bytes();

        let tx = self.conn.unchecked_transaction()?;

        if let Some(uuid) = tx
            .query_row(
                "SELECT uuid FROM items WHERE primary_hash = ?1",
                params![hash_bytes],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            tx.execute(
                "UPDATE items SET last_used_at = ?1 WHERE uuid = ?2",
                params![ts, uuid],
            )?;
            // Merge any new mimes we now see that we didn't before.
            insert_mimes(&tx, &uuid, primary_mime, primary_content, extras)?;
            tx.commit()?;
            return Ok(RecordResult::Existed(uuid));
        }

        let uuid = uuid::Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO items \
                 (uuid, kind, primary_mime, primary_hash, preview, created_at, last_used_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![uuid, kind.as_str(), primary_mime, hash_bytes, preview, ts],
        )?;
        insert_mimes(&tx, &uuid, primary_mime, primary_content, extras)?;
        tx.commit()?;
        Ok(RecordResult::Inserted(uuid))
    }

    pub fn touch(&self, uuid: &str) -> Result<()> {
        let now = unix_now();
        self.conn.execute(
            "UPDATE items SET last_used_at = ?1 WHERE uuid = ?2",
            params![now, uuid],
        )?;
        Ok(())
    }

    pub fn list(&self, limit: usize) -> Result<Vec<Entry>> {
        // Two queries instead of N+1: items page first, then a single mimes
        // query restricted to that page's UUIDs and stitched in memory.
        let mut stmt = self.conn.prepare(
            "SELECT uuid, kind, primary_mime, preview, created_at, last_used_at \
             FROM items ORDER BY last_used_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            let kind_str: String = r.get(1)?;
            let kind = Kind::parse(&kind_str).unwrap_or(Kind::Text);
            Ok(Entry {
                uuid: r.get(0)?,
                kind,
                primary_mime: r.get(2)?,
                preview: r.get(3)?,
                created_at: r.get(4)?,
                last_used_at: r.get(5)?,
                mimes: Vec::new(),
            })
        })?;
        let mut out: Vec<Entry> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        if out.is_empty() {
            return Ok(out);
        }

        let mut by_uuid: std::collections::HashMap<&str, &mut Vec<MimeContent>> =
            out.iter_mut().map(|e| (e.uuid.as_str(), &mut e.mimes)).collect();

        let placeholders = std::iter::repeat_n("?", by_uuid.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT item_uuid, mime, content FROM mimes \
             WHERE item_uuid IN ({placeholders}) ORDER BY mime"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let uuids: Vec<&str> = by_uuid.keys().copied().collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(uuids), |r| {
            Ok((
                r.get::<_, String>(0)?,
                MimeContent {
                    mime: r.get(1)?,
                    content: r.get(2)?,
                },
            ))
        })?;
        for row in rows {
            let (uuid, mc) = row?;
            if let Some(vec) = by_uuid.get_mut(uuid.as_str()) {
                vec.push(mc);
            }
        }
        Ok(out)
    }

    pub fn get(&self, uuid: &str) -> Result<Option<Entry>> {
        let entry = self
            .conn
            .query_row(
                "SELECT uuid, kind, primary_mime, preview, created_at, last_used_at \
                 FROM items WHERE uuid = ?1",
                params![uuid],
                |r| {
                    let kind_str: String = r.get(1)?;
                    let kind = Kind::parse(&kind_str).unwrap_or(Kind::Text);
                    Ok(Entry {
                        uuid: r.get(0)?,
                        kind,
                        primary_mime: r.get(2)?,
                        preview: r.get(3)?,
                        created_at: r.get(4)?,
                        last_used_at: r.get(5)?,
                        mimes: Vec::new(),
                    })
                },
            )
            .optional()?;
        let Some(mut entry) = entry else {
            return Ok(None);
        };
        entry.mimes = self.load_mimes(&entry.uuid)?;
        Ok(Some(entry))
    }

    fn load_mimes(&self, uuid: &str) -> Result<Vec<MimeContent>> {
        let mut stmt = self.conn.prepare(
            "SELECT mime, content FROM mimes WHERE item_uuid = ?1 ORDER BY mime",
        )?;
        let rows = stmt.query_map(params![uuid], |r| {
            Ok(MimeContent {
                mime: r.get(0)?,
                content: r.get(1)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    pub fn prune(&self, keep: usize) -> Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM items WHERE uuid NOT IN (\
                 SELECT uuid FROM items ORDER BY last_used_at DESC LIMIT ?1\
             )",
            params![keep as i64],
        )?;
        Ok(n)
    }
}

fn insert_mimes(
    tx: &rusqlite::Transaction<'_>,
    uuid: &str,
    primary_mime: &str,
    primary_content: &[u8],
    extras: &[MimeContent],
) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO mimes (item_uuid, mime, content) VALUES (?1, ?2, ?3)",
        params![uuid, primary_mime, primary_content],
    )?;
    for extra in extras {
        if extra.mime == primary_mime {
            continue;
        }
        tx.execute(
            "INSERT OR IGNORE INTO mimes (item_uuid, mime, content) VALUES (?1, ?2, ?3)",
            params![uuid, extra.mime, extra.content],
        )?;
    }
    Ok(())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

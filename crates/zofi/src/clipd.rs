//! `zofi clipd` subcommand: run the clipboard daemon (watches selection
//! events, holds the active selection on activate, exposes IPC).

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use zofi_clipd::{daemon, db::Db, model::Kind, paths, pidfile::DaemonLock, preview};

pub fn run() -> Result<()> {
    let pid_path = paths::pid_path();
    let _lock = DaemonLock::acquire(&pid_path).context("acquire daemon lock")?;

    let db_path = paths::db_path()?;
    let db = Db::open(&db_path)?;
    tracing::info!("db={db_path:?} pid={pid_path:?}");

    daemon::run(db)
}

/// Bulk import a clipman-style JSON array of strings into the db. Older items
/// get older `last_used_at` so the most recent ends up at the top of MRU.
pub fn import(path: &PathBuf) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("read {path:?}"))?;
    let items: Vec<String> =
        serde_json::from_slice(&bytes).context("parse JSON array of strings")?;
    if items.is_empty() {
        eprintln!("no entries in {path:?}");
        return Ok(());
    }

    let db = Db::open(&paths::db_path()?)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let mut inserted = 0;
    let mut deduped = 0;
    let mut converted = 0;
    for (rev_ix, raw) in items.iter().rev().enumerate() {
        if raw.is_empty() {
            continue;
        }
        let bytes = raw.as_bytes();
        let normalized = match decode_utf16_if_likely(bytes) {
            Some(s) => {
                converted += 1;
                s.into_bytes()
            }
            None => bytes.to_vec(),
        };
        if normalized.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(&normalized);
        let preview_str = preview::build(&text);
        let ts = now + rev_ix as i64;
        match db.record_with_ts(
            Kind::Text,
            "text/plain;charset=utf-8",
            &normalized,
            Some(&preview_str),
            &[],
            ts,
        ) {
            Ok(zofi_clipd::db::RecordResult::Inserted(_)) => inserted += 1,
            Ok(zofi_clipd::db::RecordResult::Existed(_)) => deduped += 1,
            Err(e) => tracing::warn!("skip entry {rev_ix}: {e:#}"),
        }
    }
    eprintln!(
        "imported {inserted} entries ({deduped} duplicates collapsed, \
         {converted} converted from UTF-16) from {path:?}"
    );
    Ok(())
}

/// Heuristic: clipman occasionally stores UTF-16LE blobs from apps (Firefox,
/// some Java apps). BOM or mostly-zero odd bytes → decode; otherwise leave
/// the raw bytes alone.
fn decode_utf16_if_likely(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 4 || !bytes.len().is_multiple_of(2) {
        return None;
    }
    let stripped = if bytes.starts_with(&[0xFF, 0xFE]) {
        &bytes[2..]
    } else {
        let sample = bytes.len().min(64);
        let pairs = sample / 2;
        if pairs == 0 {
            return None;
        }
        let nuls = (0..pairs).filter(|i| bytes[i * 2 + 1] == 0).count();
        if nuls * 10 < pairs * 7 {
            return None;
        }
        bytes
    };
    let units: Vec<u16> = stripped
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let s = String::from_utf16(&units).ok()?;
    Some(s.trim_end_matches('\0').to_string())
}

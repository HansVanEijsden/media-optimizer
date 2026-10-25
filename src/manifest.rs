//! The SQLite manifest.
//!
//! One row per file that has ever been considered, keyed by absolute path, and
//! carrying the content hashes on both sides of the transformation. That layout
//! is what makes recovery work: after a full restore onto new hardware every
//! inode and mtime changes, but the hashes (and the in-file COM marker) do not.
//!
//! Everything is written in WAL mode with a busy timeout so a manual run and the
//! nightly run cannot corrupt each other; an advisory `flock` in `limits.rs`
//! stops them from overlapping in the first place.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: i64 = 5;

/// How a file ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Decided, not processed yet (a run may have crashed here).
    #[default]
    Pending,
    /// Replaced by a smaller file.
    Changed,
    /// Encoded but not kept: the result was not enough smaller (`no_gain`) or a
    /// verification check failed. Remembered so the work is not repeated.
    Rejected,
    /// Deliberately left alone, with a reason.
    Skipped,
    /// Something went wrong; safe to retry next run.
    Error,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Pending => "pending",
            Outcome::Changed => "changed",
            Outcome::Rejected => "rejected",
            Outcome::Skipped => "skipped",
            Outcome::Error => "error",
        }
    }

    fn from_str(s: &str) -> Outcome {
        match s {
            "changed" => Outcome::Changed,
            "rejected" => Outcome::Rejected,
            "skipped" => Outcome::Skipped,
            "error" => Outcome::Error,
            _ => Outcome::Pending,
        }
    }
}

/// The full state of one file.
#[derive(Debug, Clone, Default)]
pub struct FileRecord {
    pub path: PathBuf,
    pub root: PathBuf,
    pub size_before: u64,
    pub mtime_ns: i64,
    pub inode: u64,
    /// The fingerprint of the file as it is on disk *after* this tool finished
    /// with it. This is what the fast path compares against, so a changed file is
    /// recognised next run even though its original size and mtime are still
    /// recorded for reporting.
    pub fp_size: u64,
    pub fp_mtime_ns: i64,
    pub fp_inode: u64,
    pub size_after: Option<u64>,
    pub sha_before: Option<String>,
    pub sha_after: Option<String>,
    pub content_format: String,
    pub width: u32,
    pub height: u32,
    pub quality: u8,
    pub quality_confident: bool,
    pub action: String,
    pub reason: Option<String>,
    pub ssimu2: Option<f64>,
    pub encoder: Option<String>,
    pub encoder_version: Option<String>,
    pub settings: Option<String>,
    pub outcome: Outcome,
    pub detail: Option<String>,
    pub run_id: Option<i64>,
    /// Digest of the settings that produced this decision. A row whose digest no
    /// longer matches is re-evaluated rather than skipped.
    pub cfg_hash: String,
}

/// The cheap identity of a file, used for the fast path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    pub size: u64,
    pub mtime_ns: i64,
    pub inode: u64,
}

impl Fingerprint {
    /// The fingerprint stored in the manifest for a row.
    pub fn from_record(r: &FileRecord) -> Self {
        Fingerprint {
            size: r.fp_size,
            mtime_ns: r.fp_mtime_ns,
            inode: r.fp_inode,
        }
    }
}

impl From<&FileRecord> for Fingerprint {
    fn from(r: &FileRecord) -> Self {
        Fingerprint::from_record(r)
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunTotals {
    pub files_seen: u64,
    pub files_changed: u64,
    pub files_skipped: u64,
    pub files_rejected: u64,
    pub errors: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

impl RunTotals {
    pub fn bytes_saved(&self) -> u64 {
        self.bytes_before.saturating_sub(self.bytes_after)
    }
}

pub struct Manifest {
    conn: Connection,
    path: PathBuf,
}

impl Manifest {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating state directory {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening manifest {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let manifest = Manifest {
            conn,
            path: path.to_path_buf(),
        };
        manifest.migrate()?;
        Ok(manifest)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runs (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                started      TEXT NOT NULL,
                finished     TEXT,
                host         TEXT,
                tool_version TEXT,
                mode         TEXT,
                files_seen   INTEGER NOT NULL DEFAULT 0,
                files_changed INTEGER NOT NULL DEFAULT 0,
                files_skipped INTEGER NOT NULL DEFAULT 0,
                files_rejected INTEGER NOT NULL DEFAULT 0,
                errors       INTEGER NOT NULL DEFAULT 0,
                bytes_before INTEGER NOT NULL DEFAULT 0,
                bytes_after  INTEGER NOT NULL DEFAULT 0,
                note         TEXT
            );
            CREATE TABLE IF NOT EXISTS files (
                path              TEXT PRIMARY KEY,
                root              TEXT NOT NULL,
                size_before       INTEGER NOT NULL,
                mtime_ns          INTEGER NOT NULL,
                inode             INTEGER NOT NULL,
                fp_size           INTEGER NOT NULL DEFAULT 0,
                fp_mtime_ns       INTEGER NOT NULL DEFAULT 0,
                fp_inode          INTEGER NOT NULL DEFAULT 0,
                size_after        INTEGER,
                sha_before        TEXT,
                sha_after         TEXT,
                content_format    TEXT NOT NULL DEFAULT '',
                width             INTEGER NOT NULL DEFAULT 0,
                height            INTEGER NOT NULL DEFAULT 0,
                quality           INTEGER NOT NULL DEFAULT 0,
                quality_confident INTEGER NOT NULL DEFAULT 0,
                action            TEXT NOT NULL DEFAULT '',
                reason            TEXT,
                ssimu2            REAL,
                encoder           TEXT,
                encoder_version   TEXT,
                settings          TEXT,
                cfg_hash          TEXT NOT NULL DEFAULT '',
                outcome           TEXT NOT NULL DEFAULT 'pending',
                detail            TEXT,
                run_id            INTEGER,
                ts                TEXT NOT NULL
            );
            -- sha_before is no longer written (the input hash would cost a second
            -- full read per file for a value nothing needs), so its index is dead
            -- weight on a table that reaches a million rows.
            -- Only the primary key is indexed. The other indexes existed for
            -- report queries that run once in a while and can afford a scan, and
            -- together they were a third of the file on a six-figure row count.
            DROP INDEX IF EXISTS files_sha_before;
            DROP INDEX IF EXISTS files_sha_after;
            DROP INDEX IF EXISTS files_outcome;
            DROP INDEX IF EXISTS files_root;
            "#,
        )?;
        self.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
        Ok(())
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?)
    }

    pub fn start_run(&self, host: &str, tool_version: &str, mode: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO runs(started, host, tool_version, mode) VALUES (?1, ?2, ?3, ?4)",
            params![Utc::now().to_rfc3339(), host, tool_version, mode],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn finish_run(&self, id: i64, totals: &RunTotals, note: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET finished=?2, files_seen=?3, files_changed=?4, files_skipped=?5,
                             files_rejected=?6, errors=?7, bytes_before=?8, bytes_after=?9, note=?10
             WHERE id=?1",
            params![
                id,
                Utc::now().to_rfc3339(),
                totals.files_seen as i64,
                totals.files_changed as i64,
                totals.files_skipped as i64,
                totals.files_rejected as i64,
                totals.errors as i64,
                totals.bytes_before as i64,
                totals.bytes_after as i64,
                note
            ],
        )?;
        Ok(())
    }

    /// The fingerprint of a known file, if any.
    /// Returns (fingerprint, outcome, config digest) for a known path.
    pub fn fingerprint(&self, path: &Path) -> Result<Option<(Fingerprint, Outcome, String)>> {
        let key = path.to_string_lossy();
        let row = self
            .conn
            .query_row(
                "SELECT fp_size, fp_mtime_ns, fp_inode, outcome, cfg_hash FROM files WHERE path = ?1",
                params![key],
                |r| {
                    Ok((
                        Fingerprint {
                            size: r.get::<_, i64>(0)? as u64,
                            mtime_ns: r.get(1)?,
                            inode: r.get::<_, i64>(2)? as u64,
                        },
                        Outcome::from_str(&r.get::<_, String>(3)?),
                        r.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn get(&self, path: &Path) -> Result<Option<FileRecord>> {
        let key = path.to_string_lossy();
        let row = self
            .conn
            .query_row(
                "SELECT root, size_before, mtime_ns, inode, size_after, sha_before, sha_after,
                        content_format, width, height, quality, quality_confident, action,
                        reason, ssimu2, encoder, encoder_version, settings, outcome, detail, run_id,
                        fp_size, fp_mtime_ns, fp_inode, cfg_hash
                 FROM files WHERE path = ?1",
                params![key],
                |r| {
                    Ok(FileRecord {
                        path: path.to_path_buf(),
                        root: PathBuf::from(r.get::<_, String>(0)?),
                        size_before: r.get::<_, i64>(1)? as u64,
                        mtime_ns: r.get(2)?,
                        inode: r.get::<_, i64>(3)? as u64,
                        size_after: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                        sha_before: r.get(5)?,
                        sha_after: r.get(6)?,
                        content_format: r.get(7)?,
                        width: r.get::<_, i64>(8)? as u32,
                        height: r.get::<_, i64>(9)? as u32,
                        quality: r.get::<_, i64>(10)? as u8,
                        quality_confident: r.get::<_, i64>(11)? != 0,
                        action: r.get(12)?,
                        reason: r.get(13)?,
                        ssimu2: r.get(14)?,
                        encoder: r.get(15)?,
                        encoder_version: r.get(16)?,
                        settings: r.get(17)?,
                        outcome: Outcome::from_str(&r.get::<_, String>(18)?),
                        detail: r.get(19)?,
                        run_id: r.get(20)?,
                        fp_size: r.get::<_, i64>(21)? as u64,
                        fp_mtime_ns: r.get(22)?,
                        fp_inode: r.get::<_, i64>(23)? as u64,
                        cfg_hash: r.get::<_, String>(24)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Find a known path by the hash of its *output*, which is how we recognise
    /// our own work after files are moved or restored.
    pub fn find_by_hash(&self, sha: &str) -> Result<Option<PathBuf>> {
        let row = self
            .conn
            .query_row(
                "SELECT path FROM files WHERE sha_after = ?1 LIMIT 1",
                params![sha],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(row.map(PathBuf::from))
    }

    pub fn upsert(&self, r: &FileRecord) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO files (
                path, root, size_before, mtime_ns, inode, fp_size, fp_mtime_ns, fp_inode,
                size_after, sha_before, sha_after, content_format, width, height, quality,
                quality_confident, action, reason, ssimu2, encoder, encoder_version, settings,
                outcome, detail, run_id, ts, cfg_hash
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27
            )
            ON CONFLICT(path) DO UPDATE SET
                root=excluded.root, size_before=excluded.size_before, mtime_ns=excluded.mtime_ns,
                inode=excluded.inode, size_after=excluded.size_after,
                fp_size=excluded.fp_size, fp_mtime_ns=excluded.fp_mtime_ns,
                fp_inode=excluded.fp_inode,
                sha_before=excluded.sha_before, sha_after=excluded.sha_after,
                content_format=excluded.content_format, width=excluded.width,
                height=excluded.height, quality=excluded.quality,
                quality_confident=excluded.quality_confident, action=excluded.action,
                reason=excluded.reason, ssimu2=excluded.ssimu2, encoder=excluded.encoder,
                encoder_version=excluded.encoder_version, settings=excluded.settings,
                outcome=excluded.outcome, detail=excluded.detail, run_id=excluded.run_id,
                ts=excluded.ts, cfg_hash=excluded.cfg_hash
            "#,
            params![
                r.path.to_string_lossy(),
                r.root.to_string_lossy(),
                r.size_before as i64,
                r.mtime_ns,
                r.inode as i64,
                r.fp_size as i64,
                r.fp_mtime_ns,
                r.fp_inode as i64,
                r.size_after.map(|v| v as i64),
                r.sha_before,
                r.sha_after,
                r.content_format,
                r.width as i64,
                r.height as i64,
                r.quality as i64,
                if r.quality_confident { 1 } else { 0 },
                r.action,
                r.reason,
                r.ssimu2,
                r.encoder,
                r.encoder_version,
                r.settings,
                r.outcome.as_str(),
                r.detail,
                r.run_id,
                Utc::now().to_rfc3339(),
                r.cfg_hash,
            ],
        )?;
        Ok(())
    }

    /// Totals for one root, computed from the manifest rather than from counters,
    /// so an interrupted run still reports the truth.
    pub fn totals_for_root(&self, root: &Path) -> Result<RunTotals> {
        let mut totals = RunTotals::default();
        let mut stmt = self.conn.prepare(
            "SELECT outcome, COUNT(*), COALESCE(SUM(size_before),0),
                    COALESCE(SUM(COALESCE(size_after, size_before)),0)
             FROM files WHERE root = ?1 GROUP BY outcome",
        )?;
        let rows = stmt.query_map(params![root.to_string_lossy()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, i64>(3)? as u64,
            ))
        })?;
        for row in rows {
            let (outcome, count, before, after) = row?;
            totals.files_seen += count;
            match Outcome::from_str(&outcome) {
                Outcome::Changed => {
                    totals.files_changed += count;
                    totals.bytes_before += before;
                    totals.bytes_after += after;
                }
                Outcome::Skipped => totals.files_skipped += count,
                Outcome::Rejected => totals.files_rejected += count,
                Outcome::Error => totals.errors += count,
                Outcome::Pending => {}
            }
        }
        Ok(totals)
    }

    /// Rows changed in a given run, for the report.
    pub fn changed_in_run(&self, run_id: i64) -> Result<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, action, size_before, size_after, quality, ssimu2, width, height
             FROM files WHERE run_id = ?1 AND outcome = 'changed'
             ORDER BY (COALESCE(size_before,0) - COALESCE(size_after,0)) DESC",
        )?;
        let rows = stmt.query_map(params![run_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                r.get::<_, i64>(4)? as u8,
                r.get::<_, Option<f64>>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (path, action, size_before, size_after, quality, ssimu2) = row?;
            out.push(FileRecord {
                path: PathBuf::from(path),
                action,
                size_before,
                size_after,
                quality,
                ssimu2,
                ..Default::default()
            });
        }
        Ok(out)
    }

    pub fn run_summary(&self, run_id: i64) -> Result<Option<RunSummary>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, started, finished, host, tool_version, mode, files_seen,
                        files_changed, files_skipped, files_rejected, errors,
                        bytes_before, bytes_after, note
                 FROM runs WHERE id = ?1",
                params![run_id],
                |r| {
                    Ok(RunSummary {
                        id: r.get(0)?,
                        started: r.get(1)?,
                        finished: r.get(2)?,
                        host: r.get(3)?,
                        tool_version: r.get(4)?,
                        mode: r.get(5)?,
                        files_seen: r.get::<_, i64>(6)? as u64,
                        files_changed: r.get::<_, i64>(7)? as u64,
                        files_skipped: r.get::<_, i64>(8)? as u64,
                        files_rejected: r.get::<_, i64>(9)? as u64,
                        errors: r.get::<_, i64>(10)? as u64,
                        bytes_before: r.get::<_, i64>(11)? as u64,
                        bytes_after: r.get::<_, i64>(12)? as u64,
                        note: r.get(13)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn last_run(&self) -> Result<Option<RunSummary>> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM runs WHERE finished IS NOT NULL ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match id {
            Some(id) => self.run_summary(id),
            None => Ok(None),
        }
    }

    /// Per-root savings, for `mediaopt report`.
    pub fn root_summaries(&self) -> Result<Vec<RootSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT root,
                    COUNT(*) FILTER (WHERE outcome='changed') AS changed,
                    COUNT(*) FILTER (WHERE outcome='rejected') AS rejected,
                    COUNT(*) FILTER (WHERE outcome='skipped') AS skipped,
                    COUNT(*) FILTER (WHERE outcome='error') AS errors,
                    COALESCE(SUM(CASE WHEN outcome='changed' THEN size_before END),0),
                    COALESCE(SUM(CASE WHEN outcome='changed' THEN size_after END),0)
             FROM files GROUP BY root ORDER BY 6 DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(RootSummary {
                root: PathBuf::from(r.get::<_, String>(0)?),
                changed: r.get::<_, i64>(1)? as u64,
                rejected: r.get::<_, i64>(2)? as u64,
                skipped: r.get::<_, i64>(3)? as u64,
                errors: r.get::<_, i64>(4)? as u64,
                bytes_before: r.get::<_, i64>(5)? as u64,
                bytes_after: r.get::<_, i64>(6)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Dump the whole manifest as JSON (used by the nightly export snapshot).
    pub fn export_json(&self) -> Result<String> {
        let mut stmt = self.conn.prepare(
            "SELECT path, root, size_before, mtime_ns, inode, size_after, sha_before,
                    sha_after, content_format, width, height, quality, quality_confident,
                    action, reason, ssimu2, encoder, encoder_version, settings, outcome,
                    detail, run_id, ts, fp_size, fp_mtime_ns, fp_inode, cfg_hash FROM files",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(serde_json::json!({
                "path": r.get::<_, String>(0)?,
                "root": r.get::<_, String>(1)?,
                "size_before": r.get::<_, i64>(2)?,
                "mtime_ns": r.get::<_, i64>(3)?,
                "inode": r.get::<_, i64>(4)?,
                "size_after": r.get::<_, Option<i64>>(5)?,
                "sha_before": r.get::<_, Option<String>>(6)?,
                "sha_after": r.get::<_, Option<String>>(7)?,
                "format": r.get::<_, String>(8)?,
                "width": r.get::<_, i64>(9)?,
                "height": r.get::<_, i64>(10)?,
                "quality": r.get::<_, i64>(11)?,
                "quality_confident": r.get::<_, i64>(12)? != 0,
                "action": r.get::<_, String>(13)?,
                "reason": r.get::<_, Option<String>>(14)?,
                "ssimu2": r.get::<_, Option<f64>>(15)?,
                "encoder": r.get::<_, Option<String>>(16)?,
                "encoder_version": r.get::<_, Option<String>>(17)?,
                "settings": r.get::<_, Option<String>>(18)?,
                "outcome": r.get::<_, String>(19)?,
                "detail": r.get::<_, Option<String>>(20)?,
                "run_id": r.get::<_, Option<i64>>(21)?,
                "ts": r.get::<_, String>(22)?,
            }))
        })?;
        let rows: Vec<serde_json::Value> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(serde_json::to_string(&serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "exported_at": Utc::now().to_rfc3339(),
            "files": rows,
        }))?)
    }

    /// The most recent runs, newest first.
    pub fn recent_runs(&self, limit: i64) -> Result<Vec<RunSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, started, finished, host, tool_version, mode, files_seen,
                    files_changed, files_skipped, files_rejected, errors,
                    bytes_before, bytes_after, note
             FROM runs ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(RunSummary {
                id: r.get(0)?,
                started: r.get(1)?,
                finished: r.get(2)?,
                host: r.get(3)?,
                tool_version: r.get(4)?,
                mode: r.get(5)?,
                files_seen: r.get::<_, i64>(6)? as u64,
                files_changed: r.get::<_, i64>(7)? as u64,
                files_skipped: r.get::<_, i64>(8)? as u64,
                files_rejected: r.get::<_, i64>(9)? as u64,
                errors: r.get::<_, i64>(10)? as u64,
                bytes_before: r.get::<_, i64>(11)? as u64,
                bytes_after: r.get::<_, i64>(12)? as u64,
                note: r.get(13)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// A random sample of files this tool changed, for `mediaopt verify`.
    pub fn sample_changed(&self, limit: i64) -> Result<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, action, size_before, size_after, quality, ssimu2
             FROM files WHERE outcome = 'changed' ORDER BY RANDOM() LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(FileRecord {
                path: PathBuf::from(r.get::<_, String>(0)?),
                action: r.get::<_, String>(1)?,
                size_before: r.get::<_, i64>(2)? as u64,
                size_after: r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                quality: r.get::<_, i64>(4)? as u8,
                ssimu2: r.get::<_, Option<f64>>(5)?,
                ..Default::default()
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn count_files(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?)
    }

    /// Restore rows from an exported snapshot. Existing rows win: a running
    /// manifest is more current than a nightly snapshot.
    pub fn import_json(&self, json: &str) -> Result<usize> {
        let value: serde_json::Value = serde_json::from_str(json)?;
        let Some(files) = value.get("files").and_then(|f| f.as_array()) else {
            anyhow::bail!("snapshot has no \"files\" array");
        };
        let mut imported = 0usize;
        let tx = self.conn.unchecked_transaction()?;
        for file in files {
            let get_str = |key: &str| file.get(key).and_then(|v| v.as_str());
            let get_i64 = |key: &str| file.get(key).and_then(|v| v.as_i64());
            let Some(path) = get_str("path") else {
                continue;
            };
            let record = FileRecord {
                path: PathBuf::from(path),
                root: PathBuf::from(get_str("root").unwrap_or("")),
                size_before: get_i64("size_before").unwrap_or(0).max(0) as u64,
                mtime_ns: get_i64("mtime_ns").unwrap_or(0),
                inode: get_i64("inode").unwrap_or(0).max(0) as u64,
                fp_size: get_i64("fp_size").unwrap_or(0).max(0) as u64,
                fp_mtime_ns: get_i64("fp_mtime_ns").unwrap_or(0),
                fp_inode: get_i64("fp_inode").unwrap_or(0).max(0) as u64,
                size_after: get_i64("size_after").map(|v| v.max(0) as u64),
                sha_before: get_str("sha_before").map(|s| s.to_string()),
                sha_after: get_str("sha_after").map(|s| s.to_string()),
                content_format: get_str("format").unwrap_or("").to_string(),
                width: get_i64("width").unwrap_or(0).max(0) as u32,
                height: get_i64("height").unwrap_or(0).max(0) as u32,
                quality: get_i64("quality").unwrap_or(0).max(0) as u8,
                quality_confident: file
                    .get("quality_confident")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                action: get_str("action").unwrap_or("").to_string(),
                reason: get_str("reason").map(|s| s.to_string()),
                ssimu2: file.get("ssimu2").and_then(|v| v.as_f64()),
                encoder: get_str("encoder").map(|s| s.to_string()),
                encoder_version: get_str("encoder_version").map(|s| s.to_string()),
                settings: get_str("settings").map(|s| s.to_string()),
                outcome: Outcome::from_str(get_str("outcome").unwrap_or("skipped")),
                detail: get_str("detail").map(|s| s.to_string()),
                run_id: get_i64("run_id"),
                cfg_hash: get_str("cfg_hash").unwrap_or("").to_string(),
            };
            // Do not clobber a row that already knows more than the snapshot.
            if self
                .conn
                .query_row(
                    "SELECT 1 FROM files WHERE path = ?1",
                    params![record.path.to_string_lossy()],
                    |_| Ok(()),
                )
                .optional()?
                .is_some()
            {
                continue;
            }
            tx.execute(
                "INSERT INTO files (path, root, size_before, mtime_ns, inode, size_after,
                    sha_before, sha_after, content_format, width, height, quality,
                    quality_confident, action, reason, ssimu2, encoder, encoder_version,
                    settings, outcome, detail, run_id, ts, fp_size, fp_mtime_ns, fp_inode,
                    cfg_hash)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,
                    ?19,?20,?21,?22,?23,?24,?25,?26,?27)",
                params![
                    record.path.to_string_lossy(),
                    record.root.to_string_lossy(),
                    record.size_before as i64,
                    record.mtime_ns,
                    record.inode as i64,
                    record.size_after.map(|v| v as i64),
                    record.sha_before,
                    record.sha_after,
                    record.content_format,
                    record.width as i64,
                    record.height as i64,
                    record.quality as i64,
                    if record.quality_confident { 1 } else { 0 },
                    record.action,
                    record.reason,
                    record.ssimu2,
                    record.encoder,
                    record.encoder_version,
                    record.settings,
                    record.outcome.as_str(),
                    record.detail,
                    record.run_id,
                    Utc::now().to_rfc3339(),
                    record.fp_size as i64,
                    record.fp_mtime_ns,
                    record.fp_inode as i64,
                    record.cfg_hash,
                ],
            )?;
            imported += 1;
        }
        tx.commit()?;
        Ok(imported)
    }

    /// Drop error rows older than `days` so a persistent failure does not grow the
    /// database forever. Deliberately conservative: every other row is a memory
    /// that saves real work.
    pub fn prune_old(&self, days: i64) -> Result<usize> {
        let cutoff = (Utc::now() - chrono::Duration::days(days)).to_rfc3339();
        let removed = self.conn.execute(
            "DELETE FROM files WHERE outcome = 'error' AND ts < ?1",
            params![cutoff],
        )?;
        let runs = self.conn.execute(
            "DELETE FROM runs WHERE finished IS NOT NULL AND started < ?1",
            params![cutoff],
        )?;
        Ok(removed + runs)
    }

    pub fn vacuum(&self) -> Result<()> {
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    /// Housekeeping after a run: let SQLite refresh its statistics and give the
    /// pages back. On a manifest that reaches a hundred megabytes this is worth a
    /// second or two per run, and it keeps the file from fragmenting over months.
    pub fn tidy(&self) -> Result<()> {
        self.conn.execute_batch("PRAGMA optimize; VACUUM;")?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub id: i64,
    pub started: String,
    pub finished: Option<String>,
    pub host: Option<String>,
    pub tool_version: Option<String>,
    pub mode: Option<String>,
    pub files_seen: u64,
    pub files_changed: u64,
    pub files_skipped: u64,
    pub files_rejected: u64,
    pub errors: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub note: Option<String>,
}

impl RunSummary {
    pub fn bytes_saved(&self) -> u64 {
        self.bytes_before.saturating_sub(self.bytes_after)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RootSummary {
    pub root: PathBuf,
    pub changed: u64,
    pub rejected: u64,
    pub skipped: u64,
    pub errors: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

impl RootSummary {
    pub fn bytes_saved(&self) -> u64 {
        self.bytes_before.saturating_sub(self.bytes_after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_manifest() -> (tempfile::TempDir, Manifest) {
        let dir = tempfile::tempdir().unwrap();
        let m = Manifest::open(&dir.path().join("state.db")).unwrap();
        (dir, m)
    }

    fn record(path: &str) -> FileRecord {
        FileRecord {
            path: PathBuf::from(path),
            root: PathBuf::from("/home/example/www"),
            size_before: 1000,
            mtime_ns: 111,
            inode: 42,
            size_after: Some(400),
            sha_before: Some("aa".into()),
            sha_after: Some("bb".into()),
            content_format: "jpeg".into(),
            width: 2048,
            height: 1365,
            quality: 95,
            quality_confident: true,
            action: "jpeg_reencode".into(),
            outcome: Outcome::Changed,
            run_id: Some(1),
            ..Default::default()
        }
    }

    #[test]
    fn round_trip_and_fast_path() {
        let (_dir, m) = temp_manifest();
        let r = record("/home/example/www/a.jpg");
        m.upsert(&r).unwrap();
        let fp = m.fingerprint(&r.path).unwrap().unwrap();
        assert_eq!(fp.0, Fingerprint::from(&r));
        assert_eq!(fp.1, Outcome::Changed);
        assert_eq!(fp.2, "", "a row without a digest must be re-evaluated once");
        let back = m.get(&r.path).unwrap().unwrap();
        assert_eq!(back.size_after, Some(400));
        assert_eq!(back.sha_after.as_deref(), Some("bb"));
    }

    #[test]
    fn changed_fingerprint_is_detected() {
        let (_dir, m) = temp_manifest();
        let r = record("/home/example/www/a.jpg");
        m.upsert(&r).unwrap();
        let mut changed = Fingerprint::from(&r);
        changed.mtime_ns += 1;
        assert_ne!(m.fingerprint(&r.path).unwrap().unwrap().0, changed);
    }

    #[test]
    fn config_digest_round_trips_and_changes() {
        let (_dir, m) = temp_manifest();
        let mut r = record("/home/example/www/a.jpg");
        r.cfg_hash = "abc123".into();
        m.upsert(&r).unwrap();
        assert_eq!(m.fingerprint(&r.path).unwrap().unwrap().2, "abc123");
        let mut changed = r.clone();
        changed.cfg_hash = "def456".into();
        m.upsert(&changed).unwrap();
        assert_eq!(m.fingerprint(&r.path).unwrap().unwrap().2, "def456");
    }

    #[test]
    fn lookup_by_output_hash() {
        let (_dir, m) = temp_manifest();
        m.upsert(&record("/home/example/www/a.jpg")).unwrap();
        assert_eq!(
            m.find_by_hash("bb").unwrap(),
            Some(PathBuf::from("/home/example/www/a.jpg"))
        );
        assert_eq!(m.find_by_hash("zz").unwrap(), None);
    }

    #[test]
    fn totals_only_count_changed_bytes() {
        let (_dir, m) = temp_manifest();
        m.upsert(&record("/home/example/www/a.jpg")).unwrap();
        let mut skipped = record("/home/example/www/b.jpg");
        skipped.outcome = Outcome::Skipped;
        skipped.size_after = None;
        m.upsert(&skipped).unwrap();
        let totals = m.totals_for_root(Path::new("/home/example/www")).unwrap();
        assert_eq!(totals.files_seen, 2);
        assert_eq!(totals.files_changed, 1);
        assert_eq!(totals.files_skipped, 1);
        assert_eq!(totals.bytes_before, 1000);
        assert_eq!(totals.bytes_after, 400);
        assert_eq!(totals.bytes_saved(), 600);
    }

    #[test]
    fn runs_are_recorded_and_latest_is_returned() {
        let (_dir, m) = temp_manifest();
        let id = m.start_run("web-a", "0.1.0", "apply").unwrap();
        m.finish_run(
            id,
            &RunTotals {
                files_seen: 3,
                files_changed: 1,
                bytes_before: 900,
                bytes_after: 300,
                ..Default::default()
            },
            Some("test"),
        )
        .unwrap();
        let last = m.last_run().unwrap().unwrap();
        assert_eq!(last.id, id);
        assert_eq!(last.bytes_saved(), 600);
    }

    #[test]
    fn export_contains_files() {
        let (_dir, m) = temp_manifest();
        m.upsert(&record("/home/example/www/a.jpg")).unwrap();
        let json = m.export_json().unwrap();
        assert!(json.contains("\"schema_version\""));
        assert!(json.contains("/home/example/www/a.jpg"));
    }
}

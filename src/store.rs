//! The SQLite index: files, chunks, FTS5 and vectors.

use crate::types::Kind;
use crate::vectors::{self, VectorIndex};
use anyhow::{ensure, Result};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{DirBuilder, OpenOptions};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 2: FTS prefix index, mtime in nanoseconds.
pub const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS files(
    id INTEGER PRIMARY KEY, path BLOB NOT NULL UNIQUE, kind TEXT NOT NULL,
    mtime INTEGER NOT NULL, size INTEGER NOT NULL, error TEXT);
CREATE TABLE IF NOT EXISTS chunks(
    id INTEGER PRIMARY KEY, file_id INTEGER NOT NULL, ord INTEGER NOT NULL, text TEXT, vec BLOB);
CREATE INDEX IF NOT EXISTS chunks_file ON chunks(file_id);
CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(text, content='chunks', content_rowid='id', prefix='2 3');
CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks WHEN new.text IS NOT NULL BEGIN
    INSERT INTO fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks WHEN old.text IS NOT NULL BEGIN
    INSERT INTO fts(fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
";

const WIPE: &str = "
DROP TABLE IF EXISTS fts;
DROP TABLE IF EXISTS chunks;
DROP TABLE IF EXISTS files;
DROP TABLE IF EXISTS meta;
";

pub struct Store {
    conn: Connection,
}

#[derive(Clone, Debug)]
pub struct FileRow {
    pub id: i64,
    pub path: PathBuf,
    pub kind: Kind,
}

/// One chunk to store. `vec` is the full 768d unit vector.
#[derive(Clone, Debug, Default)]
pub struct NewChunk {
    pub text: Option<String>,
    pub vec: Option<Vec<f32>>,
}

fn path_of(b: Vec<u8>) -> PathBuf {
    PathBuf::from(OsString::from_vec(b))
}

fn file_row(r: &Row) -> rusqlite::Result<FileRow> {
    Ok(FileRow {
        id: r.get(0)?,
        path: path_of(r.get(1)?),
        kind: Kind::parse(&r.get::<_, String>(2)?),
    })
}

fn connect(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        // The index holds extracted document text; new directories get owner-only access.
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    // SQLite gives the -wal and -shm files the mode of the database file.
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

/// The stored meta value. A database that has tables but no `meta` table is not an
/// mmsearch index, and this returns an error so that nothing changes it.
fn meta(conn: &Connection, path: &Path, key: &str) -> Result<Option<String>> {
    let count = |q: &str| conn.query_row(q, [], |r| r.get::<_, i64>(0));
    if count("SELECT count(*) FROM sqlite_master WHERE type = 'table'")? == 0 {
        return Ok(None);
    }
    ensure!(
        count("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'")? == 1,
        "{} is not an mmsearch database",
        path.display()
    );
    let v = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()?;
    Ok(v)
}

fn init(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

/// Upserts the file row and deletes its old chunks. Returns the file id.
fn put_file(
    tx: &Transaction,
    path: &Path,
    kind: Kind,
    mtime: i64,
    size: i64,
    error: Option<&str>,
) -> Result<i64> {
    let id = tx.query_row(
        "INSERT INTO files(path, kind, mtime, size, error) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(path) DO UPDATE SET kind = ?2, mtime = ?3, size = ?4, error = ?5 RETURNING id",
        params![
            path.as_os_str().as_bytes(),
            kind.as_str(),
            mtime,
            size,
            error
        ],
        |r| r.get(0),
    )?;
    tx.execute("DELETE FROM chunks WHERE file_id = ?1", [id])?;
    Ok(id)
}

impl Store {
    /// Opens or creates the index in WAL mode. If the model id or the schema version differs
    /// from the stored one, it deletes all rows and starts empty. It refuses other databases.
    pub fn open(path: &Path, model_id: &str) -> Result<Store> {
        let conn = connect(path)?;
        let version = SCHEMA_VERSION.to_string();
        let current = meta(&conn, path, "model")?.as_deref() == Some(model_id)
            && meta(&conn, path, "schema")?.as_deref() == Some(&version);
        init(&conn)?;
        if !current {
            conn.execute_batch(WIPE)?;
            conn.execute_batch(SCHEMA)?;
            conn.execute(
                "INSERT INTO meta(key, value) VALUES ('model', ?1), ('schema', ?2)",
                params![model_id, version],
            )?;
        }
        Ok(Store { conn })
    }

    /// Opens an index for search. It never creates, wipes or migrates a database, and it
    /// fails if the index was built with another model or schema.
    pub fn open_existing(path: &Path, model_id: &str) -> Result<Store> {
        ensure!(path.exists(), "no index at {}", path.display());
        let conn = connect(path)?;
        let version = SCHEMA_VERSION.to_string();
        ensure!(
            meta(&conn, path, "model")?.as_deref() == Some(model_id)
                && meta(&conn, path, "schema")?.as_deref() == Some(&version),
            "{} was built with another model or schema; run `mmsearch index` or the daemon to rebuild it",
            path.display()
        );
        Ok(Store { conn })
    }

    /// path -> (mtime, size) of every file row, for the startup reconcile.
    pub fn file_states(&self) -> Result<HashMap<PathBuf, (i64, i64)>> {
        let mut st = self.conn.prepare("SELECT path, mtime, size FROM files")?;
        let rows = st.query_map([], |r| Ok((path_of(r.get(0)?), (r.get(1)?, r.get(2)?))))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Replaces all rows of `path` in one transaction. Returns the file id and the new
    /// chunk ids in the order of `chunks`.
    pub fn replace_file(
        &mut self,
        path: &Path,
        kind: Kind,
        mtime: i64,
        size: i64,
        chunks: &[NewChunk],
    ) -> Result<(i64, Vec<i64>)> {
        let tx = self.conn.transaction()?;
        let id = put_file(&tx, path, kind, mtime, size, None)?;
        let mut ids = Vec::with_capacity(chunks.len());
        {
            let mut ins = tx.prepare_cached(
                "INSERT INTO chunks(file_id, ord, text, vec) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (ord, c) in chunks.iter().enumerate() {
                let blob = c.vec.as_deref().map(vectors::to_blob);
                ins.execute(params![id, ord as i64, c.text, blob])?;
                ids.push(tx.last_insert_rowid());
            }
        }
        tx.commit()?;
        Ok((id, ids))
    }

    /// Records a failed extraction: no chunks, the error text, and the mtime and size,
    /// so the reconcile does not retry the file until it changes. Returns the file id.
    pub fn set_error(
        &mut self,
        path: &Path,
        kind: Kind,
        mtime: i64,
        size: i64,
        error: &str,
    ) -> Result<i64> {
        let tx = self.conn.transaction()?;
        let id = put_file(&tx, path, kind, mtime, size, Some(error))?;
        tx.commit()?;
        Ok(id)
    }

    /// Deletes all rows of `path`. Returns the deleted file id, if there was one.
    pub fn delete_file(&mut self, path: &Path) -> Result<Option<i64>> {
        let tx = self.conn.transaction()?;
        let id: Option<i64> = tx
            .query_row(
                "SELECT id FROM files WHERE path = ?1",
                [path.as_os_str().as_bytes()],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = id {
            tx.execute("DELETE FROM chunks WHERE file_id = ?1", [id])?;
            tx.execute("DELETE FROM files WHERE id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// FTS5 BM25 search, best first: (file_id, chunk_id). Each query word is quoted and
    /// used as a prefix term, so no input can break the FTS syntax. Empty query: empty result.
    pub fn fts(&self, query: &str, limit: usize) -> Result<Vec<(i64, i64)>> {
        // A word without a letter or digit has no tokens; its quoted form is an empty phrase.
        let q = query
            .split_whitespace()
            .filter(|w| w.chars().any(char::is_alphanumeric))
            // A one-letter prefix scans most of the index (0.8 s at 300k chunks), so a
            // one-character word matches whole tokens only. `prefix='2 3'` covers the rest.
            .map(|w| {
                let star = if w.chars().count() > 1 { "*" } else { "" };
                format!("\"{}\"{star}", w.replace('"', "\"\""))
            })
            .collect::<Vec<_>>()
            .join(" ");
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let mut st = self.conn.prepare_cached(
            "SELECT chunks.file_id, fts.rowid FROM fts JOIN chunks ON chunks.id = fts.rowid
             WHERE fts MATCH ?1 ORDER BY fts.rank LIMIT ?2",
        )?;
        let rows = st.query_map(params![q, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn file(&self, id: i64) -> Result<Option<FileRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, path, kind FROM files WHERE id = ?1",
                [id],
                file_row,
            )
            .optional()?)
    }

    pub fn file_by_path(&self, path: &Path) -> Result<Option<FileRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, path, kind FROM files WHERE path = ?1",
                [path.as_os_str().as_bytes()],
                file_row,
            )
            .optional()?)
    }

    pub fn chunk_text(&self, chunk_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT text FROM chunks WHERE id = ?1", [chunk_id], |r| {
                r.get(0)
            })
            .optional()?
            .flatten())
    }

    /// The text of the first chunk of a file that has text, for a preview without a chunk hit.
    pub fn first_text(&self, file_id: i64) -> Result<Option<String>> {
        let q =
            "SELECT text FROM chunks WHERE file_id = ?1 AND text IS NOT NULL ORDER BY ord LIMIT 1";
        Ok(self.conn.query_row(q, [file_id], |r| r.get(0)).optional()?)
    }

    /// Every stored vector, cut to the RAM dimension.
    pub fn load_vectors(&self) -> Result<VectorIndex> {
        let mut index = VectorIndex::new();
        let mut st = self
            .conn
            .prepare("SELECT id, file_id, vec FROM chunks WHERE vec IS NOT NULL")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            index.insert(
                r.get(0)?,
                r.get(1)?,
                &vectors::from_blob(&r.get::<_, Vec<u8>>(2)?),
            );
        }
        Ok(index)
    }
}

//! `SQLite` ledger: the durable product of the engine.
//!
//! One database holds every document seen (by content hash), every extraction
//! run keyed by that hash plus the [`BackendIdentity`] that produced it, and
//! the pages, metadata, reference entries and in-text citations of each run.
//! Publication is idempotent: writing a result whose identity key already
//! exists replaces the earlier run inside a single transaction. Positioned
//! evidence (spans, lines), warnings, keywords and the raw `/Info` map are
//! kept as JSON text columns; everything a query would filter on is a plain
//! column. Figures (image and drawing regions) get their own `figures` table,
//! created on open when missing, so ledgers written before it existed gain
//! it without a [`SCHEMA_VERSION`] change.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Params, Row, params};
use thiserror::Error;

use crate::schema::{
    Author, BBox, BackendIdentity, ChunkResult, CitationMarker, ContentHash, Document,
    ExtractionResult, Figure, Metadata, PageText, ReferenceEntry, SCHEMA_VERSION,
    SourceObservation, StageTimings, Status,
};

/// Row id of a run in the `runs` table.
pub type RunId = i64;

/// Errors raised by the ledger.
#[derive(Debug, Error)]
pub enum LedgerError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("ledger schema version {found} but this build expects {expected}")]
    SchemaMismatch { found: u32, expected: u32 },
}

/// A stored run located by its identity key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSummary {
    /// Row id usable with [`Ledger::load_result`].
    pub id: RunId,
    /// Outcome recorded for the run.
    pub status: Status,
    /// Unix seconds at which the run was written.
    pub finished_at: i64,
}

/// Row counts over the whole ledger.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LedgerStats {
    pub documents: u64,
    pub runs: u64,
    pub complete: u64,
    pub partial: u64,
    pub failed: u64,
    pub pages: u64,
    pub references: u64,
    pub citations: u64,
    pub figures: u64,
}

/// Handle on one ledger database.
#[derive(Debug)]
pub struct Ledger {
    conn: Connection,
}

/// Pragmas for an on-disk ledger: WAL so readers never block the writer, a
/// bounded wait on a locked file, and enforced foreign keys (needed for the
/// cascading delete that makes publication idempotent).
const FILE_PRAGMAS: &str = "PRAGMA journal_mode = WAL; \
    PRAGMA synchronous = NORMAL; \
    PRAGMA busy_timeout = 5000; \
    PRAGMA foreign_keys = ON;";

const MEMORY_PRAGMAS: &str = "PRAGMA foreign_keys = ON;";

/// The whole schema. Every table has an explicit primary key; every table
/// below `runs` cascades on delete so removing a run removes its rows.
const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_meta (
    version INTEGER PRIMARY KEY
);
CREATE TABLE IF NOT EXISTS documents (
    hash TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    pages INTEGER NOT NULL DEFAULT 0,
    first_seen INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sources (
    id INTEGER PRIMARY KEY,
    hash TEXT NOT NULL REFERENCES documents(hash) ON DELETE CASCADE,
    path TEXT NOT NULL,
    inode INTEGER,
    device INTEGER,
    mtime_unix INTEGER,
    size INTEGER NOT NULL,
    seen_at INTEGER NOT NULL,
    UNIQUE (hash, path, inode, mtime_unix)
);
CREATE TABLE IF NOT EXISTS runs (
    id INTEGER PRIMARY KEY,
    hash TEXT NOT NULL REFERENCES documents(hash) ON DELETE CASCADE,
    backend_name TEXT NOT NULL,
    backend_version TEXT NOT NULL,
    config_digest TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    status TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    finished_at INTEGER NOT NULL,
    timings_json TEXT NOT NULL,
    warnings_json TEXT NOT NULL,
    UNIQUE (hash, backend_name, backend_version, config_digest, schema_version)
);
CREATE INDEX IF NOT EXISTS runs_hash ON runs(hash);
CREATE TABLE IF NOT EXISTS pages (
    run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    page INTEGER NOT NULL,
    width REAL NOT NULL,
    height REAL NOT NULL,
    rotation INTEGER NOT NULL,
    text TEXT NOT NULL,
    spans_json TEXT NOT NULL,
    lines_json TEXT NOT NULL,
    warnings_json TEXT NOT NULL,
    PRIMARY KEY (run_id, page)
);
CREATE TABLE IF NOT EXISTS chunks (
    run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    chunk_index INTEGER NOT NULL,
    first_page INTEGER NOT NULL,
    last_page INTEGER NOT NULL,
    status TEXT NOT NULL,
    text_sha256 TEXT NOT NULL,
    ms REAL NOT NULL,
    PRIMARY KEY (run_id, chunk_index)
);
CREATE TABLE IF NOT EXISTS metadata (
    run_id INTEGER PRIMARY KEY REFERENCES runs(id) ON DELETE CASCADE,
    title TEXT,
    doi TEXT,
    arxiv_id TEXT,
    year INTEGER,
    venue TEXT,
    abstract_text TEXT,
    keywords_json TEXT NOT NULL,
    info_json TEXT NOT NULL,
    provenance_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS metadata_doi ON metadata(doi);
CREATE TABLE IF NOT EXISTS authors (
    run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    name TEXT NOT NULL,
    affiliation TEXT,
    orcid TEXT,
    email TEXT,
    PRIMARY KEY (run_id, seq)
);
CREATE TABLE IF NOT EXISTS "references" (
    run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    idx INTEGER NOT NULL,
    label TEXT,
    raw TEXT NOT NULL,
    title TEXT,
    year INTEGER,
    venue TEXT,
    volume TEXT,
    issue TEXT,
    pages TEXT,
    doi TEXT,
    arxiv_id TEXT,
    url TEXT,
    page INTEGER NOT NULL,
    PRIMARY KEY (run_id, idx)
);
CREATE INDEX IF NOT EXISTS references_doi ON "references"(doi);
CREATE TABLE IF NOT EXISTS reference_authors (
    run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    ref_idx INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    name TEXT NOT NULL,
    PRIMARY KEY (run_id, ref_idx, seq),
    FOREIGN KEY (run_id, ref_idx) REFERENCES "references"(run_id, idx) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS citations (
    id INTEGER PRIMARY KEY,
    run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    page INTEGER NOT NULL,
    "offset" INTEGER NOT NULL,
    text TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS citations_run ON citations(run_id);
CREATE TABLE IF NOT EXISTS citation_targets (
    citation_id INTEGER NOT NULL REFERENCES citations(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    ref_idx INTEGER NOT NULL,
    PRIMARY KEY (citation_id, seq)
);
"#;

/// The `figures` table, kept apart from [`SCHEMA_SQL`] because it was added
/// after version 1 shipped. It is created `IF NOT EXISTS` on every open, so an
/// existing ledger gains the (empty) table the first time this build opens it
/// and [`SCHEMA_VERSION`] stays unchanged. The bounding box is four nullable
/// columns: all set or all `NULL`.
const FIGURES_SQL: &str = r"
CREATE TABLE IF NOT EXISTS figures (
    run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    page INTEGER NOT NULL,
    idx INTEGER NOT NULL,
    kind TEXT NOT NULL,
    mime TEXT,
    width_px INTEGER,
    height_px INTEGER,
    sha256 TEXT,
    file TEXT,
    caption TEXT,
    x0 REAL,
    y0 REAL,
    x1 REAL,
    y1 REAL,
    PRIMARY KEY (run_id, page, idx)
);
";

// Statement text. `references` and `offset` are SQL keywords and stay quoted.

const SELECT_VERSION: &str = "SELECT version FROM schema_meta";
const INSERT_VERSION: &str = "INSERT INTO schema_meta (version) VALUES (?1)";
const UPSERT_DOCUMENT_SIZE: &str = "INSERT INTO documents (hash, size, pages, first_seen) \
    VALUES (?1, ?2, 0, ?3) ON CONFLICT(hash) DO UPDATE SET size = excluded.size";
const UPSERT_DOCUMENT: &str = "INSERT INTO documents (hash, size, pages, first_seen) \
    VALUES (?1, ?2, ?3, ?4) \
    ON CONFLICT(hash) DO UPDATE SET size = excluded.size, pages = excluded.pages";
const SELECT_SOURCE_ID: &str = "SELECT id FROM sources \
    WHERE hash = ?1 AND path = ?2 AND inode IS ?3 AND mtime_unix IS ?4";
const INSERT_SOURCE: &str = "INSERT INTO sources \
    (hash, path, inode, device, mtime_unix, size, seen_at) \
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)";
const DELETE_RUN: &str = "DELETE FROM runs WHERE hash = ?1 AND backend_name = ?2 \
    AND backend_version = ?3 AND config_digest = ?4 AND schema_version = ?5";
const INSERT_RUN: &str = "INSERT INTO runs (hash, backend_name, backend_version, config_digest, \
    schema_version, status, started_at, finished_at, timings_json, warnings_json) \
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)";
const INSERT_PAGE: &str = "INSERT INTO pages (run_id, page, width, height, rotation, text, \
    spans_json, lines_json, warnings_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)";
const INSERT_FIGURE: &str = "INSERT INTO figures (run_id, page, idx, kind, mime, width_px, \
    height_px, sha256, file, caption, x0, y0, x1, y1) \
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)";
const INSERT_CHUNK: &str = "INSERT INTO chunks (run_id, chunk_index, first_page, last_page, \
    status, text_sha256, ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)";
const INSERT_METADATA: &str = "INSERT INTO metadata (run_id, title, doi, arxiv_id, year, \
    venue, abstract_text, keywords_json, info_json, provenance_json) \
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)";
const INSERT_AUTHOR: &str = "INSERT INTO authors (run_id, seq, name, affiliation, orcid, email) \
    VALUES (?1, ?2, ?3, ?4, ?5, ?6)";
const INSERT_REFERENCE: &str = "INSERT INTO \"references\" (run_id, idx, label, raw, title, \
    year, venue, volume, issue, pages, doi, arxiv_id, url, page) \
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)";
const INSERT_REFERENCE_AUTHOR: &str = "INSERT INTO reference_authors \
    (run_id, ref_idx, seq, name) VALUES (?1, ?2, ?3, ?4)";
const INSERT_CITATION: &str = "INSERT INTO citations (run_id, page, \"offset\", text) \
    VALUES (?1, ?2, ?3, ?4)";
const INSERT_CITATION_TARGET: &str = "INSERT INTO citation_targets (citation_id, seq, ref_idx) \
    VALUES (?1, ?2, ?3)";
const SELECT_RUN_ID: &str = "SELECT id, status, finished_at FROM runs \
    WHERE hash = ?1 AND backend_name = ?2 AND backend_version = ?3 \
    AND config_digest = ?4 AND schema_version = ?5";
const SELECT_LATEST_RUN_BY_PREFIX: &str = "SELECT id FROM runs \
    WHERE substr(hash, 1, ?2) = ?1 ORDER BY finished_at DESC, id DESC LIMIT 1";
const UPDATE_TIMINGS: &str = "UPDATE runs SET timings_json = ?1 WHERE id = ?2";
const SELECT_RUN: &str = "SELECT hash, backend_name, backend_version, config_digest, \
    schema_version, status, timings_json, warnings_json FROM runs WHERE id = ?1";
const SELECT_DOCUMENT: &str = "SELECT size, pages FROM documents WHERE hash = ?1";
const SELECT_SOURCES: &str = "SELECT path, inode, device, mtime_unix, size FROM sources \
    WHERE hash = ?1 ORDER BY id";
const SELECT_PAGES: &str = "SELECT page, width, height, rotation, text, spans_json, lines_json, \
    warnings_json FROM pages WHERE run_id = ?1 ORDER BY page";
const SELECT_FIGURES: &str = "SELECT page, idx, kind, mime, width_px, height_px, sha256, file, \
    caption, x0, y0, x1, y1 FROM figures WHERE run_id = ?1 ORDER BY page, idx";
const SELECT_CHUNKS: &str = "SELECT chunk_index, first_page, last_page, status, text_sha256, ms \
    FROM chunks WHERE run_id = ?1 ORDER BY chunk_index";
const SELECT_METADATA: &str = "SELECT title, doi, arxiv_id, year, venue, abstract_text, \
    keywords_json, info_json, provenance_json FROM metadata WHERE run_id = ?1";
const SELECT_AUTHORS: &str = "SELECT name, affiliation, orcid, email FROM authors \
    WHERE run_id = ?1 ORDER BY seq";
const SELECT_REFERENCES: &str = "SELECT idx, label, raw, title, year, venue, volume, issue, \
    pages, doi, arxiv_id, url, page FROM \"references\" WHERE run_id = ?1 ORDER BY idx";
const SELECT_REFERENCE_AUTHORS: &str = "SELECT ref_idx, name FROM reference_authors \
    WHERE run_id = ?1 ORDER BY ref_idx, seq";
const SELECT_CITATIONS: &str = "SELECT id, page, \"offset\", text FROM citations \
    WHERE run_id = ?1 ORDER BY id";
const SELECT_CITATION_TARGETS: &str = "SELECT t.citation_id, t.ref_idx \
    FROM citation_targets t JOIN citations c ON c.id = t.citation_id \
    WHERE c.run_id = ?1 ORDER BY t.citation_id, t.seq";
const SELECT_STATS: &str = "SELECT \
    (SELECT COUNT(*) FROM documents), \
    (SELECT COUNT(*) FROM runs), \
    (SELECT COUNT(*) FROM runs WHERE status = 'complete'), \
    (SELECT COUNT(*) FROM runs WHERE status = 'partial'), \
    (SELECT COUNT(*) FROM runs WHERE status = 'failed'), \
    (SELECT COUNT(*) FROM pages), \
    (SELECT COUNT(*) FROM \"references\"), \
    (SELECT COUNT(*) FROM citations), \
    (SELECT COUNT(*) FROM figures)";

impl Ledger {
    /// Opens or creates the ledger file at `path` and verifies its schema
    /// version. Uses WAL journaling, `synchronous = NORMAL`, a 5 s busy
    /// timeout and enforced foreign keys.
    pub fn open(path: &Path) -> Result<Self, LedgerError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(FILE_PRAGMAS)?;
        Self::init(conn)
    }

    /// Opens a private in-memory ledger (tests and dry runs).
    pub fn open_in_memory() -> Result<Self, LedgerError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(MEMORY_PRAGMAS)?;
        Self::init(conn)
    }

    /// Creates the schema if missing (including the later `figures` table)
    /// and checks `schema_meta.version`.
    fn init(conn: Connection) -> Result<Self, LedgerError> {
        conn.execute_batch(SCHEMA_SQL)?;
        conn.execute_batch(FIGURES_SQL)?;
        let found = optional_row(&conn, SELECT_VERSION, [], |row| row.get::<_, u32>(0))?;
        match found {
            None => {
                conn.execute(INSERT_VERSION, params![SCHEMA_VERSION])?;
            }
            Some(SCHEMA_VERSION) => {}
            Some(found) => {
                return Err(LedgerError::SchemaMismatch {
                    found,
                    expected: SCHEMA_VERSION,
                });
            }
        }
        Ok(Self { conn })
    }

    /// Records that the bytes hashing to `hash` were seen at `size` bytes
    /// via `obs`. The document row is upserted; an identical observation
    /// (same hash, path, inode and mtime) is stored only once.
    pub fn record_source(
        &mut self,
        hash: &ContentHash,
        size: u64,
        obs: &SourceObservation,
    ) -> Result<(), LedgerError> {
        let now = now_unix();
        let tx = self.conn.transaction()?;
        tx.execute(UPSERT_DOCUMENT_SIZE, params![hash.0, to_i64(size), now])?;
        insert_source(&tx, hash, obs, now)?;
        tx.commit()?;
        Ok(())
    }

    /// Publishes `result` in one transaction. Any earlier run with the same
    /// (hash, backend name, backend version, config digest, schema version)
    /// is deleted first, so writing the same result twice leaves one run.
    pub fn write_result(&mut self, result: &ExtractionResult) -> Result<RunId, LedgerError> {
        let finished_at = now_unix();
        let started_at = finished_at - elapsed_seconds(&result.timings);
        let timings_json = serde_json::to_string(&result.timings)?;
        let warnings_json = serde_json::to_string(&result.warnings)?;
        let document = &result.document;
        let backend = &result.backend;
        let tx = self.conn.transaction()?;
        tx.execute(
            UPSERT_DOCUMENT,
            params![
                document.hash.0,
                to_i64(document.size),
                document.pages,
                finished_at,
            ],
        )?;
        for obs in &document.sources {
            insert_source(&tx, &document.hash, obs, finished_at)?;
        }
        tx.execute(
            DELETE_RUN,
            params![
                document.hash.0,
                backend.name,
                backend.version,
                backend.config_digest,
                result.schema_version,
            ],
        )?;
        tx.execute(
            INSERT_RUN,
            params![
                document.hash.0,
                backend.name,
                backend.version,
                backend.config_digest,
                result.schema_version,
                result.status.as_str(),
                started_at,
                finished_at,
                timings_json,
                warnings_json,
            ],
        )?;
        let run_id = tx.last_insert_rowid();
        insert_pages(&tx, run_id, &result.pages)?;
        insert_figures(&tx, run_id, &result.pages)?;
        insert_chunks(&tx, run_id, &result.chunks)?;
        insert_metadata(&tx, run_id, &result.metadata)?;
        insert_references(&tx, run_id, &result.references)?;
        insert_citations(&tx, run_id, &result.citations)?;
        tx.commit()?;
        Ok(run_id)
    }

    /// Finds the most recently finished run whose document hash starts with
    /// `prefix` (lower-case hex), regardless of backend identity.
    pub fn latest_run_for_prefix(&self, prefix: &str) -> Result<Option<RunId>, LedgerError> {
        let prefix_len = i64::try_from(prefix.len()).unwrap_or(i64::MAX);
        optional_row(
            &self.conn,
            SELECT_LATEST_RUN_BY_PREFIX,
            params![prefix, prefix_len],
            |row| row.get::<_, RunId>(0),
        )
    }

    /// Replaces the stored stage timings of `run`, so the cost of the ledger
    /// write itself can be recorded after [`Ledger::write_result`] returns.
    pub fn update_timings(
        &mut self,
        run: RunId,
        timings: &StageTimings,
    ) -> Result<(), LedgerError> {
        let json = serde_json::to_string(timings)?;
        let changed = self.conn.execute(UPDATE_TIMINGS, params![json, run])?;
        if changed == 0 {
            return Err(LedgerError::NotFound(format!("run {run}")));
        }
        Ok(())
    }

    /// Finds the run for `hash` produced by `backend` at the current
    /// [`SCHEMA_VERSION`], if one has been published.
    pub fn find_run(
        &self,
        hash: &ContentHash,
        backend: &BackendIdentity,
    ) -> Result<Option<RunSummary>, LedgerError> {
        let Some((id, status, finished_at)) = find_run_row(&self.conn, &hash.0, backend)? else {
            return Ok(None);
        };
        let status = parse_status(&status)?;
        Ok(Some(RunSummary {
            id,
            status,
            finished_at,
        }))
    }

    /// Rebuilds the complete [`ExtractionResult`] stored for `run`.
    pub fn load_result(&self, run: RunId) -> Result<ExtractionResult, LedgerError> {
        let conn = &self.conn;
        let header = optional_row(conn, SELECT_RUN, params![run], |row| {
            Ok(RunRow {
                hash: row.get(0)?,
                backend_name: row.get(1)?,
                backend_version: row.get(2)?,
                config_digest: row.get(3)?,
                schema_version: row.get(4)?,
                status: row.get(5)?,
                timings_json: row.get(6)?,
                warnings_json: row.get(7)?,
            })
        })?;
        let Some(header) = header else {
            return Err(LedgerError::NotFound(format!("run {run}")));
        };
        let document = load_document(conn, ContentHash(header.hash))?;
        let status = parse_status(&header.status)?;
        Ok(ExtractionResult {
            schema_version: header.schema_version,
            document,
            backend: BackendIdentity {
                name: header.backend_name,
                version: header.backend_version,
                config_digest: header.config_digest,
            },
            status,
            pages: load_pages(conn, run)?,
            chunks: load_chunks(conn, run)?,
            metadata: load_metadata(conn, run)?,
            references: load_references(conn, run)?,
            citations: load_citations(conn, run)?,
            warnings: serde_json::from_str(&header.warnings_json)?,
            timings: serde_json::from_str(&header.timings_json)?,
        })
    }

    /// Row counts across the ledger.
    pub fn stats(&self) -> Result<LedgerStats, LedgerError> {
        let stats = self.conn.query_row(SELECT_STATS, [], |row| {
            Ok(LedgerStats {
                documents: to_u64(row.get(0)?),
                runs: to_u64(row.get(1)?),
                complete: to_u64(row.get(2)?),
                partial: to_u64(row.get(3)?),
                failed: to_u64(row.get(4)?),
                pages: to_u64(row.get(5)?),
                references: to_u64(row.get(6)?),
                citations: to_u64(row.get(7)?),
                figures: to_u64(row.get(8)?),
            })
        })?;
        Ok(stats)
    }
}

/// The `runs` columns needed to rebuild a result.
struct RunRow {
    hash: String,
    backend_name: String,
    backend_version: String,
    config_digest: String,
    schema_version: u32,
    status: String,
    timings_json: String,
    warnings_json: String,
}

/// One `pages` row before its JSON columns are decoded.
struct PageRow {
    page: u32,
    width: f32,
    height: f32,
    rotation: i32,
    text: String,
    spans_json: String,
    lines_json: String,
    warnings_json: String,
}

/// One `chunks` row before its status is parsed.
struct ChunkRow {
    chunk_index: u32,
    first_page: u32,
    last_page: u32,
    status: String,
    text_sha256: String,
    ms: f64,
}

/// The `metadata` row before its JSON columns are decoded.
struct MetadataRow {
    title: Option<String>,
    doi: Option<String>,
    arxiv_id: Option<String>,
    year: Option<u16>,
    venue: Option<String>,
    abstract_text: Option<String>,
    keywords_json: String,
    info_json: String,
    provenance_json: String,
}

/// Runs a query expected to return at most one row; no row is `None`.
fn optional_row<T, P, F>(
    conn: &Connection,
    sql: &str,
    params: P,
    map: F,
) -> Result<Option<T>, LedgerError>
where
    P: Params,
    F: FnOnce(&Row<'_>) -> rusqlite::Result<T>,
{
    let value = conn.query_row(sql, params, map).optional()?;
    Ok(value)
}

/// Inserts a source observation unless the same one is already stored.
/// `IS` rather than `=` so a missing inode or mtime still dedupes.
fn insert_source(
    conn: &Connection,
    hash: &ContentHash,
    obs: &SourceObservation,
    seen_at: i64,
) -> Result<(), LedgerError> {
    let inode = obs.inode.map(to_i64);
    let device = obs.device.map(to_i64);
    let existing = optional_row(
        conn,
        SELECT_SOURCE_ID,
        params![hash.0, obs.path, inode, obs.mtime_unix],
        |row| row.get::<_, i64>(0),
    )?;
    if existing.is_none() {
        conn.execute(
            INSERT_SOURCE,
            params![
                hash.0,
                obs.path,
                inode,
                device,
                obs.mtime_unix,
                to_i64(obs.size),
                seen_at,
            ],
        )?;
    }
    Ok(())
}

fn insert_pages(conn: &Connection, run_id: RunId, pages: &[PageText]) -> Result<(), LedgerError> {
    let mut stmt = conn.prepare(INSERT_PAGE)?;
    for page in pages {
        let spans_json = serde_json::to_string(&page.spans)?;
        let lines_json = serde_json::to_string(&page.lines)?;
        let warnings_json = serde_json::to_string(&page.warnings)?;
        stmt.execute(params![
            run_id,
            page.page,
            page.width,
            page.height,
            page.rotation,
            page.text,
            spans_json,
            lines_json,
            warnings_json,
        ])?;
    }
    Ok(())
}

/// Stores every page's figures; the bounding box goes into four nullable
/// coordinate columns.
fn insert_figures(conn: &Connection, run_id: RunId, pages: &[PageText]) -> Result<(), LedgerError> {
    let mut stmt = conn.prepare(INSERT_FIGURE)?;
    for page in pages {
        for figure in &page.figures {
            let bbox = figure.bbox;
            stmt.execute(params![
                run_id,
                page.page,
                figure.index,
                figure.kind,
                figure.mime,
                figure.width_px,
                figure.height_px,
                figure.sha256,
                figure.file,
                figure.caption,
                bbox.map(|b| b.x0),
                bbox.map(|b| b.y0),
                bbox.map(|b| b.x1),
                bbox.map(|b| b.y1),
            ])?;
        }
    }
    Ok(())
}

fn insert_chunks(
    conn: &Connection,
    run_id: RunId,
    chunks: &[ChunkResult],
) -> Result<(), LedgerError> {
    let mut stmt = conn.prepare(INSERT_CHUNK)?;
    for chunk in chunks {
        stmt.execute(params![
            run_id,
            chunk.chunk_index,
            chunk.first_page,
            chunk.last_page,
            chunk.status.as_str(),
            chunk.text_sha256,
            chunk.ms,
        ])?;
    }
    Ok(())
}

fn insert_metadata(conn: &Connection, run_id: RunId, meta: &Metadata) -> Result<(), LedgerError> {
    let keywords_json = serde_json::to_string(&meta.keywords)?;
    let info_json = serde_json::to_string(&meta.info)?;
    let provenance_json = serde_json::to_string(&meta.provenance)?;
    conn.execute(
        INSERT_METADATA,
        params![
            run_id,
            meta.title,
            meta.doi,
            meta.arxiv_id,
            meta.year,
            meta.venue,
            meta.abstract_text,
            keywords_json,
            info_json,
            provenance_json,
        ],
    )?;
    let mut stmt = conn.prepare(INSERT_AUTHOR)?;
    for (seq, author) in (0_u32..).zip(&meta.authors) {
        stmt.execute(params![
            run_id,
            seq,
            author.name,
            author.affiliation,
            author.orcid,
            author.email,
        ])?;
    }
    Ok(())
}

fn insert_references(
    conn: &Connection,
    run_id: RunId,
    refs: &[ReferenceEntry],
) -> Result<(), LedgerError> {
    let mut entry_stmt = conn.prepare(INSERT_REFERENCE)?;
    let mut author_stmt = conn.prepare(INSERT_REFERENCE_AUTHOR)?;
    for entry in refs {
        entry_stmt.execute(params![
            run_id,
            entry.index,
            entry.label,
            entry.raw,
            entry.title,
            entry.year,
            entry.venue,
            entry.volume,
            entry.issue,
            entry.pages,
            entry.doi,
            entry.arxiv_id,
            entry.url,
            entry.page,
        ])?;
        for (seq, name) in (0_u32..).zip(&entry.authors) {
            author_stmt.execute(params![run_id, entry.index, seq, name])?;
        }
    }
    Ok(())
}

fn insert_citations(
    conn: &Connection,
    run_id: RunId,
    cites: &[CitationMarker],
) -> Result<(), LedgerError> {
    let mut cite_stmt = conn.prepare(INSERT_CITATION)?;
    let mut target_stmt = conn.prepare(INSERT_CITATION_TARGET)?;
    for cite in cites {
        let cite_id = cite_stmt.insert(params![run_id, cite.page, cite.offset, cite.text])?;
        for (seq, target) in (0_u32..).zip(&cite.targets) {
            target_stmt.execute(params![cite_id, seq, target])?;
        }
    }
    Ok(())
}

fn find_run_row(
    conn: &Connection,
    hash: &str,
    backend: &BackendIdentity,
) -> Result<Option<(RunId, String, i64)>, LedgerError> {
    optional_row(
        conn,
        SELECT_RUN_ID,
        params![
            hash,
            backend.name,
            backend.version,
            backend.config_digest,
            SCHEMA_VERSION,
        ],
        |row| {
            let id: RunId = row.get(0)?;
            let status: String = row.get(1)?;
            let finished_at: i64 = row.get(2)?;
            Ok((id, status, finished_at))
        },
    )
}

fn load_document(conn: &Connection, hash: ContentHash) -> Result<Document, LedgerError> {
    let found = optional_row(conn, SELECT_DOCUMENT, params![hash.0], |row| {
        let size: i64 = row.get(0)?;
        let pages: u32 = row.get(1)?;
        Ok((size, pages))
    })?;
    let Some((size, pages)) = found else {
        return Err(LedgerError::NotFound(format!("document {}", hash.0)));
    };
    let mut stmt = conn.prepare(SELECT_SOURCES)?;
    let rows = stmt.query_map(params![hash.0], |row| {
        let inode: Option<i64> = row.get(1)?;
        let device: Option<i64> = row.get(2)?;
        let bytes: i64 = row.get(4)?;
        Ok(SourceObservation {
            path: row.get(0)?,
            inode: inode.map(to_u64),
            device: device.map(to_u64),
            mtime_unix: row.get(3)?,
            size: to_u64(bytes),
        })
    })?;
    let sources = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Document {
        hash,
        size: to_u64(size),
        pages,
        sources,
    })
}

fn load_pages(conn: &Connection, run: RunId) -> Result<Vec<PageText>, LedgerError> {
    let mut figures_by_page = load_figures(conn, run)?;
    let mut stmt = conn.prepare(SELECT_PAGES)?;
    let rows = stmt.query_map(params![run], |row| {
        Ok(PageRow {
            page: row.get(0)?,
            width: row.get(1)?,
            height: row.get(2)?,
            rotation: row.get(3)?,
            text: row.get(4)?,
            spans_json: row.get(5)?,
            lines_json: row.get(6)?,
            warnings_json: row.get(7)?,
        })
    })?;
    let mut pages = Vec::new();
    for row in rows {
        let row = row?;
        pages.push(PageText {
            page: row.page,
            width: row.width,
            height: row.height,
            rotation: row.rotation,
            spans: serde_json::from_str(&row.spans_json)?,
            figures: figures_by_page.remove(&row.page).unwrap_or_default(),
            lines: serde_json::from_str(&row.lines_json)?,
            text: row.text,
            warnings: serde_json::from_str(&row.warnings_json)?,
        });
    }
    Ok(pages)
}

/// Loads the figures of `run` grouped by page number, each page's figures
/// in ascending index order.
fn load_figures(conn: &Connection, run: RunId) -> Result<BTreeMap<u32, Vec<Figure>>, LedgerError> {
    let mut stmt = conn.prepare(SELECT_FIGURES)?;
    let rows = stmt.query_map(params![run], |row| {
        let page: u32 = row.get(0)?;
        let x0: Option<f32> = row.get(9)?;
        let y0: Option<f32> = row.get(10)?;
        let x1: Option<f32> = row.get(11)?;
        let y1: Option<f32> = row.get(12)?;
        let bbox = match (x0, y0, x1, y1) {
            (Some(x0), Some(y0), Some(x1), Some(y1)) => Some(BBox { x0, y0, x1, y1 }),
            _ => None,
        };
        let figure = Figure {
            index: row.get(1)?,
            bbox,
            kind: row.get(2)?,
            mime: row.get(3)?,
            width_px: row.get(4)?,
            height_px: row.get(5)?,
            sha256: row.get(6)?,
            file: row.get(7)?,
            caption: row.get(8)?,
        };
        Ok((page, figure))
    })?;
    let mut by_page: BTreeMap<u32, Vec<Figure>> = BTreeMap::new();
    for row in rows {
        let (page, figure) = row?;
        by_page.entry(page).or_default().push(figure);
    }
    Ok(by_page)
}

fn load_chunks(conn: &Connection, run: RunId) -> Result<Vec<ChunkResult>, LedgerError> {
    let mut stmt = conn.prepare(SELECT_CHUNKS)?;
    let rows = stmt.query_map(params![run], |row| {
        Ok(ChunkRow {
            chunk_index: row.get(0)?,
            first_page: row.get(1)?,
            last_page: row.get(2)?,
            status: row.get(3)?,
            text_sha256: row.get(4)?,
            ms: row.get(5)?,
        })
    })?;
    let mut chunks = Vec::new();
    for row in rows {
        let row = row?;
        chunks.push(ChunkResult {
            chunk_index: row.chunk_index,
            first_page: row.first_page,
            last_page: row.last_page,
            status: parse_status(&row.status)?,
            text_sha256: row.text_sha256,
            ms: row.ms,
        });
    }
    Ok(chunks)
}

fn load_metadata(conn: &Connection, run: RunId) -> Result<Metadata, LedgerError> {
    let found = optional_row(conn, SELECT_METADATA, params![run], |row| {
        Ok(MetadataRow {
            title: row.get(0)?,
            doi: row.get(1)?,
            arxiv_id: row.get(2)?,
            year: row.get(3)?,
            venue: row.get(4)?,
            abstract_text: row.get(5)?,
            keywords_json: row.get(6)?,
            info_json: row.get(7)?,
            provenance_json: row.get(8)?,
        })
    })?;
    let Some(meta) = found else {
        return Err(LedgerError::NotFound(format!("metadata for run {run}")));
    };
    let mut stmt = conn.prepare(SELECT_AUTHORS)?;
    let rows = stmt.query_map(params![run], |row| {
        Ok(Author {
            name: row.get(0)?,
            affiliation: row.get(1)?,
            orcid: row.get(2)?,
            email: row.get(3)?,
        })
    })?;
    let authors = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Metadata {
        title: meta.title,
        authors,
        doi: meta.doi,
        arxiv_id: meta.arxiv_id,
        year: meta.year,
        venue: meta.venue,
        abstract_text: meta.abstract_text,
        keywords: serde_json::from_str(&meta.keywords_json)?,
        info: serde_json::from_str(&meta.info_json)?,
        provenance: serde_json::from_str(&meta.provenance_json)?,
    })
}

fn load_references(conn: &Connection, run: RunId) -> Result<Vec<ReferenceEntry>, LedgerError> {
    let mut stmt = conn.prepare(SELECT_REFERENCES)?;
    let rows = stmt.query_map(params![run], |row| {
        Ok(ReferenceEntry {
            index: row.get(0)?,
            label: row.get(1)?,
            raw: row.get(2)?,
            authors: Vec::new(),
            title: row.get(3)?,
            year: row.get(4)?,
            venue: row.get(5)?,
            volume: row.get(6)?,
            issue: row.get(7)?,
            pages: row.get(8)?,
            doi: row.get(9)?,
            arxiv_id: row.get(10)?,
            url: row.get(11)?,
            page: row.get(12)?,
        })
    })?;
    let mut entries = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut author_stmt = conn.prepare(SELECT_REFERENCE_AUTHORS)?;
    let author_rows = author_stmt.query_map(params![run], |row| {
        let ref_idx: u32 = row.get(0)?;
        let name: String = row.get(1)?;
        Ok((ref_idx, name))
    })?;
    let mut by_index: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for author_row in author_rows {
        let (ref_idx, name) = author_row?;
        by_index.entry(ref_idx).or_default().push(name);
    }
    for entry in &mut entries {
        if let Some(names) = by_index.remove(&entry.index) {
            entry.authors = names;
        }
    }
    Ok(entries)
}

fn load_citations(conn: &Connection, run: RunId) -> Result<Vec<CitationMarker>, LedgerError> {
    let mut stmt = conn.prepare(SELECT_CITATIONS)?;
    let rows = stmt.query_map(params![run], |row| {
        let id: i64 = row.get(0)?;
        let marker = CitationMarker {
            page: row.get(1)?,
            offset: row.get(2)?,
            text: row.get(3)?,
            targets: Vec::new(),
        };
        Ok((id, marker))
    })?;
    let markers = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut target_stmt = conn.prepare(SELECT_CITATION_TARGETS)?;
    let target_rows = target_stmt.query_map(params![run], |row| {
        let citation_id: i64 = row.get(0)?;
        let ref_idx: u32 = row.get(1)?;
        Ok((citation_id, ref_idx))
    })?;
    let mut by_id: BTreeMap<i64, Vec<u32>> = BTreeMap::new();
    for target_row in target_rows {
        let (citation_id, ref_idx) = target_row?;
        by_id.entry(citation_id).or_default().push(ref_idx);
    }
    let mut out = Vec::with_capacity(markers.len());
    for (id, mut marker) in markers {
        if let Some(targets) = by_id.remove(&id) {
            marker.targets = targets;
        }
        out.push(marker);
    }
    Ok(out)
}

/// Inverse of [`Status::as_str`]; unknown text is an error, never a guess.
fn parse_status(text: &str) -> Result<Status, LedgerError> {
    match text {
        "complete" => Ok(Status::Complete),
        "partial" => Ok(Status::Partial),
        "failed" => Ok(Status::Failed),
        "deferred" => Ok(Status::Deferred),
        other => Err(LedgerError::NotFound(format!("unknown status {other:?}"))),
    }
}

/// Current time as unix seconds; zero if the clock is before the epoch.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| to_i64(elapsed.as_secs()))
}

/// Whole seconds spent across all stages, used to derive `started_at`
/// from the write time.
fn elapsed_seconds(timings: &StageTimings) -> i64 {
    let total_ms = timings.acquire_ms
        + timings.parse_ms
        + timings.order_ms
        + timings.metadata_ms
        + timings.citations_ms
        + timings.write_ms;
    (total_ms / 1000.0).floor() as i64
}

/// Clamps an unsigned count into the signed `INTEGER` range `SQLite` stores.
fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Reads a stored count back as unsigned; negative values never occur.
fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::schema::{Line, Span, config_digest, sha256_hex};

    fn at(x0: f32, y0: f32, x1: f32, y1: f32) -> BBox {
        BBox { x0, y0, x1, y1 }
    }

    fn span(text: &str, bbox: BBox, size: f32, seq: u32) -> Span {
        Span {
            text: text.to_string(),
            bbox: Some(bbox),
            font: Some("F1".to_string()),
            size: Some(size),
            seq,
        }
    }

    fn line(text: &str, bbox: BBox, column: u32, spans: Vec<u32>) -> Line {
        Line {
            text: text.to_string(),
            bbox: Some(bbox),
            column,
            spans,
        }
    }

    fn author(name: &str, affiliation: Option<&str>) -> Author {
        Author {
            name: name.to_string(),
            affiliation: affiliation.map(str::to_string),
            orcid: None,
            email: None,
        }
    }

    fn reference(index: u32, raw: &str, authors: &[&str], year: Option<u16>) -> ReferenceEntry {
        ReferenceEntry {
            index,
            label: Some(format!("[{index}]")),
            raw: raw.to_string(),
            authors: authors.iter().copied().map(String::from).collect(),
            year,
            page: 2,
            ..ReferenceEntry::default()
        }
    }

    fn sample_result() -> ExtractionResult {
        let mut first = PageText::new(1, 612.0, 792.0, 0);
        first.spans = vec![
            span("Deep Ledgers", at(72.0, 700.0, 300.0, 720.0), 18.0, 0),
            span("Ada", at(72.0, 680.0, 100.0, 692.0), 10.0, 1),
            span("Bob", at(110.0, 680.0, 140.0, 692.0), 10.0, 2),
            Span {
                text: "\u{fffd}".to_string(),
                bbox: None,
                font: None,
                size: Some(10.0),
                seq: 3,
            },
        ];
        first.lines = vec![
            line("Deep Ledgers", at(72.0, 700.0, 300.0, 720.0), 0, vec![0]),
            line("Ada Bob", at(72.0, 680.0, 140.0, 692.0), 0, vec![1, 2]),
        ];
        first.text = "Deep Ledgers\nAda Bob".to_string();
        first.warnings = vec!["span 3: undecodable bytes".to_string()];
        first.figures = vec![
            Figure {
                index: 0,
                bbox: Some(at(72.0, 300.0, 540.0, 660.5)),
                kind: "raster".to_string(),
                mime: Some("image/png".to_string()),
                width_px: Some(1200),
                height_px: Some(800),
                sha256: Some(sha256_hex(b"figure bytes")),
                file: Some("figures/p1-0.png".to_string()),
                caption: Some("Figure 1: A ledger.".to_string()),
            },
            Figure {
                index: 1,
                bbox: None,
                kind: "vector".to_string(),
                mime: None,
                width_px: None,
                height_px: None,
                sha256: None,
                file: None,
                caption: None,
            },
        ];

        let mut second = PageText::new(2, 612.0, 792.0, 90);
        let body = at(72.0, 60.0, 400.0, 72.0);
        second.spans = vec![span("See [1], [2, 3].", body, 10.0, 0)];
        second.lines = vec![line("See [1], [2, 3].", body, 0, vec![0])];
        second.text = "See [1], [2, 3].".to_string();

        let chunks = vec![
            ChunkResult {
                chunk_index: 0,
                first_page: 1,
                last_page: 1,
                status: Status::Complete,
                text_sha256: sha256_hex(first.text.as_bytes()),
                ms: 12.5,
            },
            ChunkResult {
                chunk_index: 1,
                first_page: 2,
                last_page: 2,
                status: Status::Partial,
                text_sha256: sha256_hex(second.text.as_bytes()),
                ms: 7.25,
            },
        ];

        let mut info = BTreeMap::new();
        info.insert("Title".to_string(), "Deep Ledgers".to_string());
        info.insert("Producer".to_string(), "pdfTeX".to_string());
        let mut provenance = BTreeMap::new();
        provenance.insert("title".to_string(), "info:Title".to_string());
        provenance.insert("year".to_string(), "doi".to_string());
        let metadata = Metadata {
            title: Some("Deep Ledgers".to_string()),
            authors: vec![author("Ada", Some("Univ. A")), author("Bob", None)],
            doi: Some("10.1000/xyz123".to_string()),
            arxiv_id: Some("2101.00001".to_string()),
            year: Some(2021),
            venue: Some("Journal of Ledgers".to_string()),
            abstract_text: Some("We store things.".to_string()),
            keywords: vec!["ledgers".to_string(), "sqlite".to_string()],
            info,
            provenance,
        };

        let mut first_ref = reference(
            1,
            "[1] Ada, Bob. Deep Ledgers. J. Ledgers 3(2):10-20, 2020.",
            &["Ada", "Bob"],
            Some(2020),
        );
        first_ref.title = Some("Deep Ledgers".to_string());
        first_ref.venue = Some("J. Ledgers".to_string());
        first_ref.volume = Some("3".to_string());
        first_ref.issue = Some("2".to_string());
        first_ref.pages = Some("10-20".to_string());
        first_ref.doi = Some("10.1000/abc".to_string());
        let mut second_ref = reference(2, "[2] Cy. A note. 2019.", &["Cy"], Some(2019));
        second_ref.arxiv_id = Some("1901.00001".to_string());
        let mut third_ref = reference(3, "[3] Dee. https://example.org/p", &["Dee"], None);
        third_ref.url = Some("https://example.org/p".to_string());

        let citations = vec![
            CitationMarker {
                page: 2,
                offset: 4,
                text: "[1]".to_string(),
                targets: vec![1],
            },
            CitationMarker {
                page: 2,
                offset: 9,
                text: "[2, 3]".to_string(),
                targets: vec![2, 3],
            },
        ];

        let mut config = BTreeMap::new();
        config.insert("max_xobject_depth".to_string(), "8".to_string());
        let backend = BackendIdentity {
            name: "lopdf".to_string(),
            version: "0.45".to_string(),
            config_digest: config_digest(&config),
        };
        let source = SourceObservation {
            path: "/papers/sample.pdf".to_string(),
            inode: Some(4242),
            device: Some(7),
            mtime_unix: Some(1_700_000_000),
            size: 16,
        };

        ExtractionResult {
            schema_version: SCHEMA_VERSION,
            document: Document {
                hash: ContentHash(sha256_hex(b"sample pdf bytes")),
                size: 16,
                pages: 2,
                sources: vec![source],
            },
            backend,
            status: Status::Partial,
            pages: vec![first, second],
            chunks,
            metadata,
            references: vec![first_ref, second_ref, third_ref],
            citations,
            warnings: vec!["page 1: undecodable bytes".to_string()],
            timings: StageTimings {
                acquire_ms: 0.5,
                parse_ms: 20.25,
                order_ms: 3.0,
                metadata_ms: 1.5,
                citations_ms: 2.0,
                write_ms: 0.0,
            },
        }
    }

    fn count_sources(ledger: &Ledger) -> i64 {
        let sql = "SELECT COUNT(*) FROM sources";
        let conn = &ledger.conn;
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    fn has_table(conn: &Connection, name: &str) -> bool {
        let sql = "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1";
        let count: i64 = conn
            .query_row(sql, params![name], |row| row.get(0))
            .unwrap();
        count == 1
    }

    #[test]
    fn in_memory_ledger_starts_empty() {
        let ledger = Ledger::open_in_memory().unwrap();
        assert_eq!(ledger.stats().unwrap(), LedgerStats::default());
    }

    #[test]
    fn write_then_load_round_trips() {
        let mut ledger = Ledger::open_in_memory().unwrap();
        let result = sample_result();
        let run = ledger.write_result(&result).unwrap();
        let loaded = ledger.load_result(run).unwrap();
        assert_eq!(loaded.pages[0].figures.len(), 2);
        assert!(loaded.pages[0].figures[1].bbox.is_none());
        assert!(loaded.pages[1].figures.is_empty());
        assert_eq!(loaded, result);
        assert_eq!(ledger.stats().unwrap().figures, 2);
    }

    #[test]
    fn ledger_without_figures_table_gains_it_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_SQL).unwrap();
            conn.execute(INSERT_VERSION, params![SCHEMA_VERSION])
                .unwrap();
            assert!(!has_table(&conn, "figures"));
        }
        let mut ledger = Ledger::open(&path).unwrap();
        assert!(has_table(&ledger.conn, "figures"));
        assert_eq!(ledger.stats().unwrap().figures, 0);
        let result = sample_result();
        let run = ledger.write_result(&result).unwrap();
        assert_eq!(ledger.load_result(run).unwrap(), result);
        assert_eq!(ledger.stats().unwrap().figures, 2);
    }

    #[test]
    fn rewriting_same_identity_replaces_the_run() {
        let mut ledger = Ledger::open_in_memory().unwrap();
        let result = sample_result();
        let first_run = ledger.write_result(&result).unwrap();
        let second_run = ledger.write_result(&result).unwrap();
        assert!(first_run > 0);
        assert!(second_run > 0);
        let stats = ledger.stats().unwrap();
        assert_eq!(stats.documents, 1);
        assert_eq!(stats.runs, 1);
        assert_eq!(stats.partial, 1);
        assert_eq!(stats.pages, 2);
        assert_eq!(stats.references, 3);
        assert_eq!(stats.citations, 2);
        assert_eq!(stats.figures, 2);
        assert_eq!(count_sources(&ledger), 1);
        assert_eq!(ledger.load_result(second_run).unwrap(), result);

        let mut upgraded = result.clone();
        upgraded.backend.version = "0.46".to_string();
        let third_run = ledger.write_result(&upgraded).unwrap();
        assert_ne!(third_run, second_run);
        let stats = ledger.stats().unwrap();
        assert_eq!(stats.documents, 1);
        assert_eq!(stats.runs, 2);
        assert_eq!(stats.references, 6);
        assert_eq!(stats.figures, 4);
        assert_eq!(ledger.load_result(third_run).unwrap(), upgraded);
        assert_eq!(ledger.load_result(second_run).unwrap(), result);
    }

    #[test]
    fn find_run_reports_status() {
        let mut ledger = Ledger::open_in_memory().unwrap();
        let result = sample_result();
        let hash = &result.document.hash;
        let backend = &result.backend;
        assert!(ledger.find_run(hash, backend).unwrap().is_none());
        let run = ledger.write_result(&result).unwrap();
        let summary = ledger.find_run(hash, backend).unwrap().unwrap();
        assert_eq!(summary.id, run);
        assert_eq!(summary.status, Status::Partial);
        assert!(summary.finished_at > 0);
        let other = BackendIdentity {
            name: "other".to_string(),
            ..backend.clone()
        };
        assert!(ledger.find_run(hash, &other).unwrap().is_none());
        assert!(ledger.load_result(run + 1000).is_err());
        assert!(parse_status("bogus").is_err());
    }

    #[test]
    fn file_ledger_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let result = sample_result();
        {
            let mut ledger = Ledger::open(&path).unwrap();
            ledger.write_result(&result).unwrap();
        }
        let ledger = Ledger::open(&path).unwrap();
        let hash = &result.document.hash;
        let summary = ledger.find_run(hash, &result.backend).unwrap();
        let summary = summary.expect("run should persist across reopen");
        assert_eq!(ledger.load_result(summary.id).unwrap(), result);
        assert_eq!(ledger.stats().unwrap().runs, 1);
    }

    #[test]
    fn record_source_dedupes_identical_observations() {
        let mut ledger = Ledger::open_in_memory().unwrap();
        let hash = ContentHash(sha256_hex(b"bytes"));
        let obs = SourceObservation {
            path: "/a/paper.pdf".to_string(),
            inode: Some(1),
            device: Some(2),
            mtime_unix: Some(3),
            size: 5,
        };
        ledger.record_source(&hash, 5, &obs).unwrap();
        ledger.record_source(&hash, 5, &obs).unwrap();
        assert_eq!(count_sources(&ledger), 1);

        let moved = SourceObservation {
            path: "/b/paper.pdf".to_string(),
            ..obs.clone()
        };
        ledger.record_source(&hash, 5, &moved).unwrap();
        assert_eq!(count_sources(&ledger), 2);

        let unstamped = SourceObservation {
            inode: None,
            mtime_unix: None,
            ..obs.clone()
        };
        ledger.record_source(&hash, 5, &unstamped).unwrap();
        ledger.record_source(&hash, 5, &unstamped).unwrap();
        assert_eq!(count_sources(&ledger), 3);
        assert_eq!(ledger.stats().unwrap().documents, 1);
    }
}

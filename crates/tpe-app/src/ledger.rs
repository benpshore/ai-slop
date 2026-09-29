//! Read-only access to committed results in the engine ledger written by
//! `tpe extract --db <file>`.
//!
//! The reader opens the `SQLite` file with `SQLITE_OPEN_READ_ONLY` so the GUI can
//! never modify a ledger, checks `schema_meta.version`, and returns plain rows.
//! Table and column names follow the engine's DDL (`src/ledger.rs` of the root
//! crate); the two SQL keywords `"references"` and `"offset"` stay quoted.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};
use thiserror::Error;

/// Ledger schema version this reader understands (`schema_meta.version`).
pub const LEDGER_SCHEMA_VERSION: u32 = 1;

/// Errors while reading a ledger.
#[derive(Debug, Error)]
pub enum LedgerError {
    /// Any `SQLite` failure, including "no such table" on a foreign database.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The ledger was written by an engine with a different schema version.
    #[error("ledger schema version is {found}, this app understands {expected}")]
    SchemaMismatch {
        /// Version found in `schema_meta`.
        found: u32,
        /// Version this reader supports.
        expected: u32,
    },
    /// `schema_meta` exists but is empty.
    #[error("ledger has no schema_meta row")]
    NoSchema,
    /// No `runs` row with this id.
    #[error("run {0} not found")]
    RunNotFound(i64),
}

/// One document in the corpus list: the `documents` row joined with its latest
/// run (if any) and that run's metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CorpusRow {
    /// Content hash (hex `SHA-256`), the document identity.
    pub hash: String,
    /// Size in bytes.
    pub size: u64,
    /// Page count recorded on the document.
    pub pages: u32,
    /// Latest run id, `None` when the document was recorded but never extracted.
    pub run_id: Option<i64>,
    /// Run status text (`complete`, `partial`, `failed`, `deferred`).
    pub status: Option<String>,
    /// Unix time the latest run finished.
    pub finished_at: Option<i64>,
    /// Extracted title, when metadata found one.
    pub title: Option<String>,
    /// Extracted DOI, when metadata found one.
    pub doi: Option<String>,
}

/// A single sighting of an input file.  Unlike [`CorpusRow`], observations are
/// deliberately not deduplicated by content hash: two paths containing the
/// same bytes remain two rows in the work queue.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObservationRow {
    pub id: i64,
    pub hash: String,
    pub path: String,
    pub seen_at: i64,
    pub size: u64,
    pub attempt: Option<AttemptRow>,
}

/// The latest committed extraction attempt and its final counters.
///
/// Schema-v1 publication is atomic: these values are historical result data,
/// not a source of live worker or scheduler progress.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttemptRow {
    pub id: i64,
    pub status: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub current_stage: Option<String>,
    pub pages_done: u32,
    pub pages_total: Option<u32>,
    pub chunks_done: u32,
    pub chunks_total: Option<u32>,
    pub queue_position: Option<u32>,
    pub warnings: Vec<String>,
    pub retry_history: Vec<RetryRow>,
    pub terminal_error: Option<String>,
}

/// An earlier attempt for the same content.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetryRow {
    pub id: i64,
    pub status: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
}

/// Reading-ordered text of one page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageRow {
    /// 1-based page number.
    pub page: u32,
    /// `pages.text` as written by the reading-order stage.
    pub text: String,
}

/// One bibliography entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReferenceRow {
    /// Position in the reference list (`"references".idx`).
    pub index: u32,
    /// Label as printed, e.g. `[12]` or `12.`.
    pub label: Option<String>,
    /// The entry text as extracted.
    pub raw: String,
    /// Parsed author names, in order.
    pub authors: Vec<String>,
    /// Parsed title.
    pub title: Option<String>,
    /// Parsed year.
    pub year: Option<u16>,
    /// Parsed venue.
    pub venue: Option<String>,
    /// Parsed DOI.
    pub doi: Option<String>,
    /// Page the entry starts on.
    pub page: u32,
}

/// One in-text citation marker.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CitationRow {
    /// Page the marker is on.
    pub page: u32,
    /// Char offset into that page's `PageRow::text`.
    pub offset: u32,
    /// Marker text as printed, e.g. `[4]` or `(Smith, 2020)`.
    pub text: String,
    /// Reference indexes the marker resolved to (empty when unresolved).
    pub targets: Vec<u32>,
}

/// Everything the document view shows for one run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocumentDetail {
    /// Content hash of the document.
    pub hash: String,
    /// Run id the detail was loaded from.
    pub run_id: i64,
    /// Run status text.
    pub status: String,
    /// Extracted title.
    pub title: Option<String>,
    /// Extracted DOI.
    pub doi: Option<String>,
    /// Extracted arXiv id.
    pub arxiv_id: Option<String>,
    /// Extracted year.
    pub year: Option<u16>,
    /// Extracted venue.
    pub venue: Option<String>,
    /// Extracted author names, in order.
    pub authors: Vec<String>,
    /// Extracted abstract.
    pub abstract_text: Option<String>,
    /// Pages in order.
    pub pages: Vec<PageRow>,
    /// Reference entries in order.
    pub references: Vec<ReferenceRow>,
    /// Citation markers in ledger order.
    pub citations: Vec<CitationRow>,
}

const SELECT_VERSION: &str = "SELECT version FROM schema_meta";
const SELECT_CORPUS: &str = "SELECT d.hash, d.size, d.pages, r.id, r.status, r.finished_at, \
    m.title, m.doi FROM documents d \
    LEFT JOIN runs r ON r.id = (SELECT id FROM runs WHERE hash = d.hash \
        ORDER BY finished_at DESC, id DESC LIMIT 1) \
    LEFT JOIN metadata m ON m.run_id = r.id \
    ORDER BY (m.title IS NULL), lower(COALESCE(m.title, d.hash)), d.hash";
const SELECT_OBSERVATIONS: &str = "SELECT s.id, s.hash, s.path, s.seen_at, s.size \
    FROM sources s ORDER BY s.seen_at, s.id";
const SELECT_ATTEMPTS: &str = "SELECT id, status, started_at, finished_at, warnings_json \
    FROM runs WHERE hash = ?1 ORDER BY started_at DESC, id DESC";
const SELECT_RUN: &str = "SELECT hash, status FROM runs WHERE id = ?1";
const SELECT_METADATA: &str = "SELECT title, doi, arxiv_id, year, venue, abstract_text \
    FROM metadata WHERE run_id = ?1";
const SELECT_AUTHORS: &str = "SELECT name FROM authors WHERE run_id = ?1 ORDER BY seq";
const SELECT_PAGES: &str = "SELECT page, text FROM pages WHERE run_id = ?1 ORDER BY page";
const SELECT_REFERENCES: &str = "SELECT idx, label, raw, title, year, venue, doi, page \
    FROM \"references\" WHERE run_id = ?1 ORDER BY idx";
const SELECT_REFERENCE_AUTHORS: &str = "SELECT ref_idx, name FROM reference_authors \
    WHERE run_id = ?1 ORDER BY ref_idx, seq";
const SELECT_CITATIONS: &str = "SELECT id, page, \"offset\", text FROM citations \
    WHERE run_id = ?1 ORDER BY id";
const SELECT_CITATION_TARGETS: &str = "SELECT t.citation_id, t.ref_idx \
    FROM citation_targets t JOIN citations c ON c.id = t.citation_id \
    WHERE c.run_id = ?1 ORDER BY t.citation_id, t.seq";

/// Read-only handle on a ledger file.
pub struct LedgerReader {
    conn: Connection,
}

impl LedgerReader {
    /// Opens `path` read-only and checks the schema version.
    pub fn open(path: &Path) -> Result<Self, LedgerError> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags)?;
        Self::from_connection(conn)
    }

    /// Wraps an already open connection (tests use an in-memory database) and
    /// checks the schema version.
    pub fn from_connection(conn: Connection) -> Result<Self, LedgerError> {
        let found: Option<u32> = conn
            .query_row(SELECT_VERSION, [], |row| row.get(0))
            .optional()?;
        match found {
            None => Err(LedgerError::NoSchema),
            Some(version) if version != LEDGER_SCHEMA_VERSION => Err(LedgerError::SchemaMismatch {
                found: version,
                expected: LEDGER_SCHEMA_VERSION,
            }),
            Some(_) => Ok(Self { conn }),
        }
    }

    /// Every document with its latest run and metadata, sorted by title (or
    /// hash when there is no title).
    pub fn corpus(&self) -> Result<Vec<CorpusRow>, LedgerError> {
        let mut stmt = self.conn.prepare(SELECT_CORPUS)?;
        let rows = stmt.query_map([], |row| {
            Ok(CorpusRow {
                hash: row.get(0)?,
                size: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                pages: row.get(2)?,
                run_id: row.get(3)?,
                status: row.get(4)?,
                finished_at: row.get(5)?,
                title: row.get(6)?,
                doi: row.get(7)?,
            })
        })?;
        let out = rows.collect::<rusqlite::Result<Vec<CorpusRow>>>()?;
        Ok(out)
    }

    /// Returns one row per committed source observation, enriched with its
    /// newest committed attempt. Committed page/chunk rows are final counters; a
    /// percentage is intentionally unavailable when the document page count
    /// is zero because that is an unknown total, not 0%.
    pub fn observations(&self) -> Result<Vec<ObservationRow>, LedgerError> {
        let mut stmt = self.conn.prepare(SELECT_OBSERVATIONS)?;
        let rows = stmt.query_map([], |row| {
            Ok(ObservationRow {
                id: row.get(0)?,
                hash: row.get(1)?,
                path: row.get(2)?,
                seen_at: row.get(3)?,
                size: u64::try_from(row.get::<_, i64>(4)?).unwrap_or(0),
                attempt: None,
            })
        })?;
        let mut observations = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        for observation in &mut observations {
            observation.attempt = self.latest_attempt(&observation.hash)?;
        }
        Ok(observations)
    }

    /// Non-terminal rows written by a producer that persists lifecycle data.
    /// `tpe extract` itself atomically publishes terminal results, so callers
    /// must not use this method as a live view of its workers.
    pub fn active_attempts(&self) -> Result<Vec<AttemptRow>, LedgerError> {
        let mut by_id = BTreeMap::new();
        for row in self.observations()? {
            if let Some(attempt) = row.attempt
                && matches!(
                    attempt.status.as_str(),
                    "queued"
                        | "deferred"
                        | "stabilizing"
                        | "processing"
                        | "active"
                        | "running"
                        | "cancellation_requested"
                )
            {
                by_id.insert(attempt.id, attempt);
            }
        }
        Ok(by_id.into_values().collect())
    }

    /// Latest committed per-document counters, if the hash has an attempt.
    pub fn stage_progress(&self, hash: &str) -> Result<Option<AttemptRow>, LedgerError> {
        self.latest_attempt(hash)
    }

    /// Queue position for a document. Schema-v1 ledgers do not persist the
    /// in-memory scheduler queue, so this is `None` rather than a guessed rank.
    pub fn queue_position(&self, hash: &str) -> Result<Option<u32>, LedgerError> {
        Ok(self
            .latest_attempt(hash)?
            .and_then(|attempt| attempt.queue_position))
    }

    pub fn warnings(&self, hash: &str) -> Result<Vec<String>, LedgerError> {
        Ok(self
            .latest_attempt(hash)?
            .map_or_else(Vec::new, |attempt| attempt.warnings))
    }

    pub fn retry_history(&self, hash: &str) -> Result<Vec<RetryRow>, LedgerError> {
        Ok(self
            .latest_attempt(hash)?
            .map_or_else(Vec::new, |attempt| attempt.retry_history))
    }

    pub fn terminal_error(&self, hash: &str) -> Result<Option<String>, LedgerError> {
        Ok(self
            .latest_attempt(hash)?
            .and_then(|attempt| attempt.terminal_error))
    }

    fn latest_attempt(&self, hash: &str) -> Result<Option<AttemptRow>, LedgerError> {
        let mut stmt = self.conn.prepare(SELECT_ATTEMPTS)?;
        let raw = stmt
            .query_map(params![hash], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let Some((id, status, started_at, finished_at, warnings_json)) = raw.first() else {
            return Ok(None);
        };
        let warnings: Vec<String> = serde_json::from_str(warnings_json).unwrap_or_default();
        let pages_done = self.conn.query_row(
            "SELECT COUNT(*) FROM pages WHERE run_id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        let pages_total: u32 = self.conn.query_row(
            "SELECT pages FROM documents WHERE hash = ?1",
            params![hash],
            |row| row.get(0),
        )?;
        let chunks_done = self.conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE run_id = ?1 AND status = 'complete'",
            params![id],
            |row| row.get(0),
        )?;
        let chunks_total: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE run_id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        let retry_history = raw
            .iter()
            .skip(1)
            .map(|(id, status, started_at, finished_at, warnings)| RetryRow {
                id: *id,
                status: status.clone(),
                started_at: *started_at,
                finished_at: *finished_at,
                error: terminal_error(status, warnings),
            })
            .collect();
        Ok(Some(AttemptRow {
            id: *id,
            status: status.clone(),
            started_at: *started_at,
            finished_at: *finished_at,
            current_stage: infer_stage(status, pages_done, chunks_done),
            pages_done,
            pages_total: (pages_total > 0).then_some(pages_total),
            chunks_done,
            chunks_total: (chunks_total > 0).then_some(chunks_total),
            queue_position: None,
            terminal_error: terminal_error(status, warnings_json),
            warnings,
            retry_history,
        }))
    }

    /// Loads pages, metadata, references and citation markers of one run.
    pub fn document(&self, run_id: i64) -> Result<DocumentDetail, LedgerError> {
        let run: Option<(String, String)> = self
            .conn
            .query_row(SELECT_RUN, params![run_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        let Some((hash, status)) = run else {
            return Err(LedgerError::RunNotFound(run_id));
        };
        let mut detail = DocumentDetail {
            hash,
            run_id,
            status,
            ..DocumentDetail::default()
        };
        self.load_metadata(run_id, &mut detail)?;
        detail.authors = query_vec(&self.conn, SELECT_AUTHORS, run_id, |row| row.get(0))?;
        detail.pages = query_vec(&self.conn, SELECT_PAGES, run_id, |row| {
            Ok(PageRow {
                page: row.get(0)?,
                text: row.get(1)?,
            })
        })?;
        detail.references = self.load_references(run_id)?;
        detail.citations = self.load_citations(run_id)?;
        Ok(detail)
    }

    fn load_metadata(&self, run_id: i64, detail: &mut DocumentDetail) -> Result<(), LedgerError> {
        type MetaTuple = (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<u16>,
            Option<String>,
            Option<String>,
        );
        let meta: Option<MetaTuple> = self
            .conn
            .query_row(SELECT_METADATA, params![run_id], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .optional()?;
        if let Some((title, doi, arxiv_id, year, venue, abstract_text)) = meta {
            detail.title = title;
            detail.doi = doi;
            detail.arxiv_id = arxiv_id;
            detail.year = year;
            detail.venue = venue;
            detail.abstract_text = abstract_text;
        }
        Ok(())
    }

    fn load_references(&self, run_id: i64) -> Result<Vec<ReferenceRow>, LedgerError> {
        let mut entries = query_vec(&self.conn, SELECT_REFERENCES, run_id, |row| {
            Ok(ReferenceRow {
                index: row.get(0)?,
                label: row.get(1)?,
                raw: row.get(2)?,
                authors: Vec::new(),
                title: row.get(3)?,
                year: row.get(4)?,
                venue: row.get(5)?,
                doi: row.get(6)?,
                page: row.get(7)?,
            })
        })?;
        let authors: Vec<(u32, String)> =
            query_vec(&self.conn, SELECT_REFERENCE_AUTHORS, run_id, |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
        let mut by_index: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        for (ref_idx, name) in authors {
            by_index.entry(ref_idx).or_default().push(name);
        }
        for entry in &mut entries {
            if let Some(names) = by_index.remove(&entry.index) {
                entry.authors = names;
            }
        }
        Ok(entries)
    }

    fn load_citations(&self, run_id: i64) -> Result<Vec<CitationRow>, LedgerError> {
        let markers: Vec<(i64, CitationRow)> =
            query_vec(&self.conn, SELECT_CITATIONS, run_id, |row| {
                let id: i64 = row.get(0)?;
                let marker = CitationRow {
                    page: row.get(1)?,
                    offset: row.get(2)?,
                    text: row.get(3)?,
                    targets: Vec::new(),
                };
                Ok((id, marker))
            })?;
        let targets: Vec<(i64, u32)> =
            query_vec(&self.conn, SELECT_CITATION_TARGETS, run_id, |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
        let mut by_id: BTreeMap<i64, Vec<u32>> = BTreeMap::new();
        for (citation_id, ref_idx) in targets {
            by_id.entry(citation_id).or_default().push(ref_idx);
        }
        let mut out = Vec::with_capacity(markers.len());
        for (id, mut marker) in markers {
            if let Some(found) = by_id.remove(&id) {
                marker.targets = found;
            }
            out.push(marker);
        }
        Ok(out)
    }
}

/// Runs a one-parameter query and collects the mapped rows.
fn query_vec<T, F>(conn: &Connection, sql: &str, run_id: i64, map: F) -> Result<Vec<T>, LedgerError>
where
    F: FnMut(&Row<'_>) -> rusqlite::Result<T>,
{
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![run_id], map)?;
    let out = rows.collect::<rusqlite::Result<Vec<T>>>()?;
    Ok(out)
}

fn terminal_error(status: &str, warnings_json: &str) -> Option<String> {
    if status != "failed" {
        return None;
    }
    serde_json::from_str::<Vec<String>>(warnings_json)
        .ok()
        .and_then(|warnings| {
            warnings
                .into_iter()
                .find(|warning| !warning.trim().is_empty())
        })
        .or_else(|| Some(String::from("Extraction failed")))
}

fn infer_stage(status: &str, pages: u32, chunks: u32) -> Option<String> {
    match status {
        "queued" | "deferred" => Some(String::from("Queued")),
        "stabilizing" => Some(String::from("Waiting for file to stabilize")),
        "processing" if chunks > 0 => Some(String::from("Writing chunks")),
        "processing" if pages > 0 => Some(String::from("Processing pages")),
        "processing" => Some(String::from("Acquiring input")),
        "partial" | "failed" | "complete" | "cancelled" => None,
        other if !other.is_empty() => Some(other.to_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine's DDL for the tables this reader touches (root crate
    /// `src/ledger.rs`, `CREATE TABLE` statements), copied verbatim.
    const DDL: &str = r#"
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
    hash TEXT NOT NULL REFERENCES documents(hash),
    path TEXT NOT NULL,
    inode INTEGER,
    device INTEGER,
    mtime_unix INTEGER,
    size INTEGER NOT NULL,
    seen_at INTEGER NOT NULL
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
    run_id INTEGER NOT NULL REFERENCES runs(id),
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
CREATE TABLE IF NOT EXISTS citation_targets (
    citation_id INTEGER NOT NULL REFERENCES citations(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    ref_idx INTEGER NOT NULL,
    PRIMARY KEY (citation_id, seq)
);
"#;

    const FIXTURE: &str = r#"
INSERT INTO schema_meta (version) VALUES (1);
INSERT INTO documents (hash, size, pages, first_seen) VALUES ('bbbb2222', 10, 0, 1);
INSERT INTO documents (hash, size, pages, first_seen) VALUES ('aaaa1111', 2048, 2, 1);
INSERT INTO sources (id, hash, path, size, seen_at) VALUES (1, 'aaaa1111', '/one.pdf', 2048, 2);
INSERT INTO sources (id, hash, path, size, seen_at) VALUES (2, 'aaaa1111', '/copy.pdf', 2048, 3);
INSERT INTO runs (id, hash, backend_name, backend_version, config_digest, schema_version,
    status, started_at, finished_at, timings_json, warnings_json)
    VALUES (7, 'aaaa1111', 'lopdf', '0.45', 'd', 1, 'partial', 5, 6, '{}', '[]');
INSERT INTO runs (id, hash, backend_name, backend_version, config_digest, schema_version,
    status, started_at, finished_at, timings_json, warnings_json)
    VALUES (9, 'aaaa1111', 'lopdf', '0.45', 'e', 1, 'complete', 8, 9, '{}', '[]');
INSERT INTO pages (run_id, page, width, height, rotation, text, spans_json, lines_json, warnings_json)
    VALUES (9, 1, 612, 792, 0, 'Title line' || char(10) || 'Body [1] here', '[]', '[]', '[]');
INSERT INTO pages (run_id, page, width, height, rotation, text, spans_json, lines_json, warnings_json)
    VALUES (9, 2, 612, 792, 0, 'References' || char(10) || '[1] A. Author. T. 2020.', '[]', '[]', '[]');
INSERT INTO metadata (run_id, title, doi, arxiv_id, year, venue, abstract_text,
    keywords_json, info_json, provenance_json)
    VALUES (9, 'Zebra Paper', '10.1000/xyz', NULL, 2021, NULL, 'An abstract.', '[]', '{}', '{}');
INSERT INTO authors (run_id, seq, name) VALUES (9, 0, 'Ada Lovelace');
INSERT INTO authors (run_id, seq, name) VALUES (9, 1, 'Alan Turing');
INSERT INTO "references" (run_id, idx, label, raw, title, year, venue, doi, page)
    VALUES (9, 0, '[1]', '[1] A. Author. T. 2020.', 'T', 2020, NULL, NULL, 2);
INSERT INTO reference_authors (run_id, ref_idx, seq, name) VALUES (9, 0, 0, 'A. Author');
INSERT INTO citations (id, run_id, page, "offset", text) VALUES (3, 9, 1, 16, '[1]');
INSERT INTO citation_targets (citation_id, seq, ref_idx) VALUES (3, 0, 0);
"#;

    fn fixture() -> LedgerReader {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(DDL).unwrap();
        conn.execute_batch(FIXTURE).unwrap();
        LedgerReader::from_connection(conn).unwrap()
    }

    #[test]
    fn corpus_lists_every_document_with_latest_run() {
        let reader = fixture();
        let rows = reader.corpus().unwrap();
        assert_eq!(rows.len(), 2);
        // Untitled documents sort after titled ones, then by hash.
        assert_eq!(rows[0].hash, "aaaa1111");
        assert_eq!(rows[0].run_id, Some(9), "latest run by finished_at wins");
        assert_eq!(rows[0].status.as_deref(), Some("complete"));
        assert_eq!(rows[0].title.as_deref(), Some("Zebra Paper"));
        assert_eq!(rows[0].doi.as_deref(), Some("10.1000/xyz"));
        assert_eq!(rows[0].pages, 2);
        assert_eq!(rows[0].size, 2048);
        assert_eq!(rows[1].hash, "bbbb2222");
        assert_eq!(rows[1].run_id, None);
        assert_eq!(rows[1].status, None);
        assert_eq!(rows[1].title, None);
    }

    #[test]
    fn observations_keep_duplicate_content_paths_and_expose_progress() {
        let rows = fixture().observations().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].path, "/one.pdf");
        assert_eq!(rows[1].path, "/copy.pdf");
        assert_eq!(rows[0].hash, rows[1].hash);
        let attempt = rows[0].attempt.as_ref().unwrap();
        assert_eq!(attempt.id, 9);
        assert_eq!(attempt.pages_done, 2);
        assert_eq!(attempt.pages_total, Some(2));
        assert_eq!(attempt.retry_history.len(), 1);
    }

    #[test]
    fn document_detail_round_trips_pages_references_and_markers() {
        let reader = fixture();
        let detail = reader.document(9).unwrap();
        assert_eq!(detail.hash, "aaaa1111");
        assert_eq!(detail.status, "complete");
        assert_eq!(detail.title.as_deref(), Some("Zebra Paper"));
        assert_eq!(detail.year, Some(2021));
        assert_eq!(detail.abstract_text.as_deref(), Some("An abstract."));
        assert_eq!(detail.authors, vec!["Ada Lovelace", "Alan Turing"]);
        assert_eq!(detail.pages.len(), 2);
        assert_eq!(detail.pages[0].page, 1);
        assert_eq!(detail.pages[0].text, "Title line\nBody [1] here");
        assert_eq!(detail.references.len(), 1);
        assert_eq!(detail.references[0].label.as_deref(), Some("[1]"));
        assert_eq!(detail.references[0].authors, vec!["A. Author"]);
        assert_eq!(detail.references[0].year, Some(2020));
        assert_eq!(detail.references[0].page, 2);
        assert_eq!(detail.citations.len(), 1);
        assert_eq!(detail.citations[0].page, 1);
        assert_eq!(detail.citations[0].offset, 16);
        assert_eq!(detail.citations[0].text, "[1]");
        assert_eq!(detail.citations[0].targets, vec![0]);
    }

    #[test]
    fn run_without_metadata_loads_with_empty_fields() {
        let reader = fixture();
        let detail = reader.document(7).unwrap();
        assert_eq!(detail.status, "partial");
        assert_eq!(detail.title, None);
        assert!(detail.pages.is_empty());
        assert!(detail.references.is_empty());
        assert!(detail.citations.is_empty());
    }

    #[test]
    fn unknown_run_is_an_error() {
        let reader = fixture();
        assert!(matches!(
            reader.document(4242),
            Err(LedgerError::RunNotFound(4242))
        ));
    }

    #[test]
    fn schema_version_is_checked() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(DDL).unwrap();
        assert!(matches!(
            LedgerReader::from_connection(conn),
            Err(LedgerError::NoSchema)
        ));

        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(DDL).unwrap();
        conn.execute_batch("INSERT INTO schema_meta (version) VALUES (99)")
            .unwrap();
        assert!(matches!(
            LedgerReader::from_connection(conn),
            Err(LedgerError::SchemaMismatch {
                found: 99,
                expected: 1
            })
        ));
    }

    #[test]
    fn foreign_database_is_a_sqlite_error() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(matches!(
            LedgerReader::from_connection(conn),
            Err(LedgerError::Sqlite(_))
        ));
    }
}

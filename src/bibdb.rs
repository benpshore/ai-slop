//! Query-friendly `SQLite` store for `tpe --bib --db FILE`: one `papers`
//! row per scanned PDF (keyed by its SHA-256) and one `refs` row per
//! reference entry, with plain columns for everything a query filters on.
//!
//! This is deliberately separate from the full-document [`crate::ledger`]:
//! a bibliography scan covers only the tail of a document and carries no
//! metadata, markers or page text. Writing a paper is one `IMMEDIATE`
//! transaction that deletes the paper's earlier rows and inserts the new
//! ones, so re-running a PDF replaces its rows and two concurrent `tpe
//! --bib` invocations on the same file wait for each other (WAL journal, a
//! 10 s busy timeout) instead of failing.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, TransactionBehavior, params};
use thiserror::Error;

use crate::bibliography::Record;
use crate::schema::ReferenceEntry;

/// Errors raised by the bibliography store.
#[derive(Debug, Error)]
pub enum BibDbError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// The record has no hash (acquisition failed), so it has no key.
    #[error("record for {0} has no sha256 and cannot be stored")]
    NoHash(String),
}

/// Pragmas for the on-disk store: WAL so readers never block the writer,
/// and a 10 s wait on a locked file so concurrent writers queue.
const FILE_PRAGMAS: &str = "PRAGMA journal_mode = WAL; \
    PRAGMA synchronous = NORMAL; \
    PRAGMA busy_timeout = 10000;";

/// The schema; see `docs/CLI.md` for the column meanings.
pub const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS papers (
    sha256 TEXT PRIMARY KEY,
    path TEXT,
    status TEXT,
    total_pages INTEGER,
    pages_scanned INTEGER,
    section_page INTEGER,
    heading TEXT,
    backend TEXT,
    backend_version TEXT,
    elapsed_ms REAL,
    error TEXT,
    warnings TEXT,
    scanned_at TEXT
);
CREATE TABLE IF NOT EXISTS refs (
    sha256 TEXT,
    idx INTEGER,
    label TEXT,
    raw TEXT,
    authors TEXT,
    first_author TEXT,
    title TEXT,
    year INTEGER,
    venue TEXT,
    volume TEXT,
    issue TEXT,
    pages TEXT,
    doi TEXT,
    arxiv_id TEXT,
    url TEXT,
    page INTEGER,
    PRIMARY KEY (sha256, idx)
);
CREATE INDEX IF NOT EXISTS refs_doi ON refs(doi);
CREATE INDEX IF NOT EXISTS refs_year ON refs(year);
CREATE INDEX IF NOT EXISTS refs_title ON refs(title);
CREATE INDEX IF NOT EXISTS refs_first_author ON refs(first_author);
";

const DELETE_PAPER: &str = "DELETE FROM papers WHERE sha256 = ?1";
const DELETE_REFS: &str = "DELETE FROM refs WHERE sha256 = ?1";
const INSERT_PAPER: &str = "INSERT INTO papers (sha256, path, status, total_pages, \
    pages_scanned, section_page, heading, backend, backend_version, elapsed_ms, error, \
    warnings, scanned_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)";
const INSERT_REF: &str = "INSERT INTO refs (sha256, idx, label, raw, authors, first_author, \
    title, year, venue, volume, issue, pages, doi, arxiv_id, url, page) \
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)";

/// Handle on one bibliography database.
#[derive(Debug)]
pub struct BibDb {
    conn: Connection,
}

impl BibDb {
    /// Open or create the store at `path`.
    pub fn open(path: &Path) -> Result<Self, BibDbError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(FILE_PRAGMAS)?;
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(Self { conn })
    }

    /// A private in-memory store (tests).
    pub fn open_in_memory() -> Result<Self, BibDbError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(Self { conn })
    }

    /// Store `record`, replacing any earlier rows for the same SHA-256, in
    /// one transaction. Returns the number of reference rows written.
    pub fn write(&mut self, record: &Record) -> Result<usize, BibDbError> {
        let sha256 = record
            .sha256
            .as_deref()
            .ok_or_else(|| BibDbError::NoHash(record.path.clone()))?;
        let warnings = serde_json::to_string(&record.warnings)?;
        let scanned_at = rfc3339_now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(DELETE_REFS, params![sha256])?;
        tx.execute(DELETE_PAPER, params![sha256])?;
        tx.execute(
            INSERT_PAPER,
            params![
                sha256,
                record.path,
                record.status,
                record.total_pages,
                record.pages_scanned,
                record.section_page,
                record.heading,
                record.backend.name,
                record.backend.version,
                record.elapsed_ms,
                record.error,
                warnings,
                scanned_at,
            ],
        )?;
        for entry in &record.references {
            insert_ref(&tx, sha256, entry)?;
        }
        tx.commit()?;
        Ok(record.references.len())
    }

    /// Number of rows in `papers`.
    pub fn paper_count(&self) -> Result<u64, BibDbError> {
        count_rows(&self.conn, "SELECT COUNT(*) FROM papers")
    }

    /// Number of rows in `refs`.
    pub fn ref_count(&self) -> Result<u64, BibDbError> {
        count_rows(&self.conn, "SELECT COUNT(*) FROM refs")
    }
}

/// The single integer that `sql` selects.
fn count_rows(conn: &Connection, sql: &str) -> Result<u64, BibDbError> {
    let count: u64 = conn.query_row(sql, [], |row| row.get(0))?;
    Ok(count)
}

/// Insert one reference row.
fn insert_ref(
    tx: &rusqlite::Transaction<'_>,
    sha256: &str,
    entry: &ReferenceEntry,
) -> Result<(), BibDbError> {
    let authors = serde_json::to_string(&entry.authors)?;
    let doi = entry.doi.as_deref().map(normalize_doi);
    tx.execute(
        INSERT_REF,
        params![
            sha256,
            entry.index,
            entry.label,
            entry.raw,
            authors,
            entry.authors.first(),
            entry.title,
            entry.year,
            entry.venue,
            entry.volume,
            entry.issue,
            entry.pages,
            doi,
            entry.arxiv_id,
            entry.url,
            entry.page,
        ],
    )?;
    Ok(())
}

/// The DOI as stored: trimmed, without a `doi:` or `https://doi.org/` style
/// prefix, lower-cased. The raw entry text keeps the original spelling.
#[must_use]
pub fn normalize_doi(raw: &str) -> String {
    let mut doi = raw.trim();
    let lower = doi.to_ascii_lowercase();
    for prefix in [
        "https://doi.org/",
        "http://doi.org/",
        "https://dx.doi.org/",
        "http://dx.doi.org/",
        "doi.org/",
        "doi:",
    ] {
        if lower.starts_with(prefix) {
            doi = doi[prefix.len()..].trim_start();
            break;
        }
    }
    doi.to_lowercase()
}

/// The current time as an RFC 3339 UTC timestamp with second precision.
#[must_use]
pub fn rfc3339_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    rfc3339_from_unix(i64::try_from(secs).unwrap_or(i64::MAX))
}

/// Format Unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (proleptic Gregorian, via
/// the days-to-civil algorithm).
#[must_use]
pub fn rfc3339_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Civil date of the day `days` after 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    let month = u32::try_from(month).unwrap_or(1);
    let day = u32::try_from(day).unwrap_or(1);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::BackendIdentity;

    fn identity() -> BackendIdentity {
        BackendIdentity {
            name: "lopdf".to_string(),
            version: "0.45".to_string(),
            config_digest: "abc".to_string(),
        }
    }

    fn entry(index: u32, doi: Option<&str>) -> ReferenceEntry {
        ReferenceEntry {
            index,
            label: Some(format!("[{index}]")),
            raw: format!("[{index}] A. Author, Title, 2020."),
            authors: vec!["A. Author".to_string(), "B. Other".to_string()],
            title: Some("Title".to_string()),
            year: Some(2020),
            venue: None,
            volume: None,
            issue: None,
            pages: None,
            doi: doi.map(str::to_string),
            arxiv_id: None,
            url: None,
            page: 7,
        }
    }

    fn record(sha: &str, refs: Vec<ReferenceEntry>) -> Record {
        Record {
            path: "paper.pdf".to_string(),
            sha256: Some(sha.to_string()),
            backend: identity(),
            status: "found",
            total_pages: Some(9),
            pages_scanned: Some(2),
            section_page: Some(8),
            heading: Some("References".to_string()),
            references: refs,
            warnings: vec!["w".to_string()],
            elapsed_ms: 12.5,
            error: None,
        }
    }

    #[test]
    fn doi_is_lowercased_and_stripped() {
        assert_eq!(normalize_doi("https://doi.org/10.1/ABC"), "10.1/abc");
        assert_eq!(normalize_doi("doi:10.1000/x"), "10.1000/x");
        assert_eq!(normalize_doi(" 10.1000/Y "), "10.1000/y");
        assert_eq!(normalize_doi("http://dx.doi.org/10.1/z"), "10.1/z");
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        assert_eq!(rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_from_unix(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_from_unix(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339_from_unix(-1), "1969-12-31T23:59:59Z");
        assert!(rfc3339_now().ends_with('Z'));
    }

    #[test]
    fn rerun_replaces_rows() {
        let mut db = BibDb::open_in_memory().unwrap();
        let sha = "a".repeat(64);
        let first = record(&sha, vec![entry(1, Some("10.1/ABC")), entry(2, None)]);
        assert_eq!(db.write(&first).unwrap(), 2);
        assert_eq!(db.paper_count().unwrap(), 1);
        assert_eq!(db.ref_count().unwrap(), 2);

        let second = record(&sha, vec![entry(1, Some("doi:10.1/ABC"))]);
        assert_eq!(db.write(&second).unwrap(), 1);
        assert_eq!(db.paper_count().unwrap(), 1);
        assert_eq!(db.ref_count().unwrap(), 1);

        let (doi, first_author, authors): (String, String, String) = db
            .conn
            .query_row(
                "SELECT doi, first_author, authors FROM refs WHERE sha256 = ?1",
                params![sha],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(doi, "10.1/abc");
        assert_eq!(first_author, "A. Author");
        assert_eq!(authors, "[\"A. Author\",\"B. Other\"]");
    }

    #[test]
    fn record_without_hash_is_rejected() {
        let mut db = BibDb::open_in_memory().unwrap();
        let mut failed = record("x", Vec::new());
        failed.sha256 = None;
        let err = db.write(&failed).unwrap_err();
        assert!(matches!(err, BibDbError::NoHash(_)), "{err:?}");
    }

    #[test]
    fn file_store_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bib.sqlite");
        {
            let mut db = BibDb::open(&path).unwrap();
            db.write(&record(&"b".repeat(64), vec![entry(1, None)]))
                .unwrap();
        }
        let db = BibDb::open(&path).unwrap();
        assert_eq!(db.paper_count().unwrap(), 1);
        assert_eq!(db.ref_count().unwrap(), 1);
    }
}

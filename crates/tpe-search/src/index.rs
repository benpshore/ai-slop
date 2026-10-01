//! The on-disk search index: an FTS5 table for lexical (BM25) search plus a
//! vector store for semantic search, built from a `tpe` ledger.
//!
//! Layout of the index directory: `search.sqlite` (tables `meta`, `chunks`
//! and the FTS5 table `chunks_fts`, whose rowid equals `chunks.id`) and the
//! vector file `vectors.flat` or `vectors.usearch`, keyed by `chunks.id`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::SearchError;
use crate::chunker::{Chunk, Chunker};
use crate::embed::Embedder;
use crate::fusion::{RRF_K, reciprocal_rank_fusion};
use crate::permissions::{private_dir, private_file};
use crate::store::{FlatStore, VectorStore};
#[cfg(feature = "usearch")]
use crate::usearch_store::UsearchStore;

const INDEX_DB: &str = "search.sqlite";
const EMBED_BATCH: usize = 64;
const SNIPPET_WORDS: usize = 30;
#[cfg(feature = "usearch")]
const DEFAULT_STORE: &str = "usearch";
#[cfg(not(feature = "usearch"))]
const DEFAULT_STORE: &str = "flat";

const INDEX_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS chunks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    doc_hash TEXT NOT NULL,
    run_id INTEGER NOT NULL,
    page INTEGER NOT NULL,
    idx INTEGER NOT NULL,
    text TEXT NOT NULL,
    title TEXT,
    doi TEXT
);
CREATE INDEX IF NOT EXISTS chunks_doc ON chunks(doc_hash);
CREATE TABLE IF NOT EXISTS indexed_runs (
    doc_hash TEXT PRIMARY KEY,
    run_id INTEGER NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
    text,
    doc_hash UNINDEXED,
    page UNINDEXED,
    idx UNINDEXED
);
";

/// Latest successful run per document, with its metadata (ledger schema of
/// `text-processing-engine/src/ledger.rs`).
const LATEST_RUNS: &str = "SELECT r.id, r.hash, m.title, m.doi, \
    r.backend_name, r.backend_version, r.config_digest, r.finished_at FROM runs r \
    LEFT JOIN metadata m ON m.run_id = r.id \
    WHERE r.id = (SELECT r2.id FROM runs r2 WHERE r2.hash = r.hash \
        AND r2.status IN ('complete', 'partial') \
        ORDER BY r2.finished_at DESC, r2.id DESC LIMIT 1) \
    ORDER BY r.hash";
const RUN_PAGES: &str = "SELECT page, text FROM pages WHERE run_id = ?1 ORDER BY page";
const CHUNK_IDS: &str = "SELECT id FROM chunks WHERE doc_hash = ?1";
const UPSERT_INDEXED_RUN: &str = "INSERT INTO indexed_runs (doc_hash, run_id) VALUES (?1, ?2) \
    ON CONFLICT(doc_hash) DO UPDATE SET run_id = excluded.run_id";
const INSERT_CHUNK: &str = "INSERT INTO chunks (doc_hash, run_id, page, idx, text, title, doi) \
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)";
const INSERT_FTS: &str = "INSERT INTO chunks_fts (rowid, text, doc_hash, page, idx) \
    VALUES (?1, ?2, ?3, ?4, ?5)";
const LEXICAL: &str = "SELECT rowid, bm25(chunks_fts), \
    snippet(chunks_fts, 0, '[', ']', '...', 16) \
    FROM chunks_fts WHERE chunks_fts MATCH ?1 ORDER BY bm25(chunks_fts) LIMIT ?2";
const CHUNK_BY_ID: &str = "SELECT doc_hash, page, idx, text, title, doi FROM chunks WHERE id = ?1";

/// How [`SearchIndex::search`] ranks chunks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SearchMode {
    /// FTS5 BM25 over the chunk text.
    Lexical,
    /// Cosine similarity of embeddings.
    Semantic,
    /// Weighted reciprocal rank fusion: `alpha` weights the semantic list and
    /// `1 - alpha` the lexical one (`alpha` is clamped to `0..=1`).
    Hybrid { alpha: f64 },
}

/// One search result.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    /// Content hash of the document.
    pub doc_hash: String,
    /// Page the chunk came from.
    pub page: u32,
    /// Chunk index within the document.
    pub idx: u32,
    /// Higher is better. Lexical: negated BM25; semantic: cosine similarity;
    /// hybrid: fused RRF score.
    pub score: f64,
    /// FTS5 snippet with matches in `[` `]`, or the chunk's first words.
    pub snippet: String,
    /// Title from the ledger's metadata, when extracted.
    pub title: Option<String>,
    /// DOI from the ledger's metadata, when extracted.
    pub doi: Option<String>,
}

/// What one [`SearchIndex::index_ledger`] call did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexStats {
    /// Documents with a complete or partial run in the ledger.
    pub documents_seen: u64,
    /// Documents (re)chunked and embedded in this call.
    pub documents_indexed: u64,
    /// Documents whose latest run's content fingerprint matches the one
    /// recorded when it was indexed.
    pub documents_unchanged: u64,
    /// Chunks inserted.
    pub chunks_added: u64,
    /// Chunks deleted because their document's latest run changed.
    pub chunks_removed: u64,
    /// The index had been left inconsistent by an interrupted run and was
    /// rebuilt from scratch.
    pub rebuilt: bool,
}

/// Current size and configuration of an index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexSummary {
    /// Distinct documents with at least one chunk.
    pub documents: u64,
    /// Chunks in the FTS table.
    pub chunks: u64,
    /// Vectors in the vector store.
    pub vectors: u64,
    /// Embedding dimensions, once something was indexed.
    pub dim: Option<usize>,
    /// Name of the embedder the index was built with.
    pub embedder: Option<String>,
    /// Vector store kind (`flat` or `usearch`).
    pub store: String,
}

struct LedgerDoc {
    hash: String,
    run_id: i64,
    title: Option<String>,
    doi: Option<String>,
    backend_name: String,
    backend_version: String,
    config_digest: String,
    finished_at: i64,
}

/// A search index stored in one directory.
pub struct SearchIndex {
    dir: PathBuf,
    conn: Connection,
    store: Option<Box<dyn VectorStore>>,
    chunker: Chunker,
}

impl SearchIndex {
    /// Open (creating if needed) the index in `dir`.
    pub fn open(dir: &Path) -> Result<Self, SearchError> {
        private_dir(dir)?;
        let db_path = dir.join(INDEX_DB);
        // Pre-create (or tighten) the database before SQLite can write any
        // extracted document content to it.
        drop(private_file(&db_path, false)?);
        let conn = Connection::open(db_path)?;
        if let Err(e) = conn.execute_batch(INDEX_SCHEMA) {
            let message = e.to_string();
            if message.contains("fts5") {
                return Err(SearchError::Fts5Unavailable(message));
            }
            return Err(SearchError::Sqlite(e));
        }
        let mut index = Self {
            dir: dir.to_path_buf(),
            conn,
            store: None,
            chunker: Chunker::default(),
        };
        index.store = index.load_store()?;
        Ok(index)
    }

    /// Replace the chunker used for documents indexed from now on.
    pub fn set_chunker(&mut self, chunker: Chunker) {
        self.chunker = chunker;
    }

    /// Delete every chunk, vector and setting (for example to switch embedder).
    pub fn reset(&mut self) -> Result<(), SearchError> {
        self.conn
            .execute_batch("DELETE FROM chunks; DELETE FROM chunks_fts; DELETE FROM indexed_runs; DELETE FROM meta;")?;
        for kind in ["flat", "usearch"] {
            let path = store_path(&self.dir, kind);
            if path.exists() {
                fs::remove_file(&path)?;
            }
        }
        self.store = None;
        Ok(())
    }

    /// Index the latest complete or partial run of every document in the
    /// ledger at `ledger_path` (opened read-only). Each indexed run's content
    /// fingerprint (see `run_fingerprint`) is kept in the `meta` table;
    /// documents whose latest run still has that fingerprint are skipped, and
    /// any other document replaces its old chunks. The ledger reuses
    /// `runs.id` when it rewrites a run, so the id alone is not trusted.
    pub fn index_ledger(
        &mut self,
        ledger_path: &Path,
        embedder: &dyn Embedder,
    ) -> Result<IndexStats, SearchError> {
        let ledger = Connection::open_with_flags(
            ledger_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let mut stats = IndexStats::default();
        if get_meta(&self.conn, "dirty")?.as_deref() == Some("1") {
            self.reset()?;
            stats.rebuilt = true;
        }
        self.check_embedder(embedder)?;
        let kind = get_meta(&self.conn, "store")?.unwrap_or_else(|| DEFAULT_STORE.to_string());
        if self.store.is_none() {
            let dim = embedder.dim();
            self.store = Some(open_store(&kind, dim, &store_path(&self.dir, &kind))?);
            set_meta(&self.conn, "store", &kind)?;
            set_meta(&self.conn, "dim", &dim.to_string())?;
            set_meta(&self.conn, "embedder", &embedder.name())?;
        }
        let docs = latest_runs(&ledger)?;
        for doc in &docs {
            stats.documents_seen += 1;
            self.index_document(&ledger, doc, embedder, &mut stats)?;
        }
        if stats.documents_indexed > 0 {
            if let Some(store) = &self.store {
                store.save(&store_path(&self.dir, &kind))?;
            }
            set_meta(&self.conn, "dirty", "0")?;
        }
        Ok(stats)
    }

    fn index_document(
        &mut self,
        ledger: &Connection,
        doc: &LedgerDoc,
        embedder: &dyn Embedder,
        stats: &mut IndexStats,
    ) -> Result<(), SearchError> {
        let pages = run_pages(ledger, doc.run_id)?;
        let fingerprint = run_fingerprint(doc, &pages);
        let fingerprint_key = format!("fp:{}", doc.hash);
        if get_meta(&self.conn, &fingerprint_key)?.as_deref() == Some(fingerprint.as_str()) {
            stats.documents_unchanged += 1;
            return Ok(());
        }
        let existing = chunk_ids(&self.conn, &doc.hash)?;
        let chunks = self.chunk_pages(doc, &pages);
        let vectors = embed_all(embedder, &chunks)?;
        // From here on the SQLite tables and the vector file can disagree
        // until the vector file is saved; an interrupted run is rebuilt.
        set_meta(&self.conn, "dirty", "1")?;
        let tx = self.conn.transaction()?;
        for id in &existing {
            tx.execute("DELETE FROM chunks_fts WHERE rowid = ?1", params![id])?;
        }
        tx.execute("DELETE FROM chunks WHERE doc_hash = ?1", params![doc.hash])?;
        let mut new_ids: Vec<i64> = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            tx.execute(
                INSERT_CHUNK,
                params![
                    chunk.doc_hash,
                    chunk.run_id,
                    chunk.page,
                    chunk.idx,
                    chunk.text,
                    doc.title,
                    doc.doi
                ],
            )?;
            let id = tx.last_insert_rowid();
            tx.execute(
                INSERT_FTS,
                params![id, chunk.text, chunk.doc_hash, chunk.page, chunk.idx],
            )?;
            new_ids.push(id);
        }
        tx.execute(UPSERT_INDEXED_RUN, params![doc.hash, doc.run_id])?;
        set_meta(&tx, &fingerprint_key, &fingerprint)?;
        tx.commit()?;
        let store = self
            .store
            .as_mut()
            .ok_or_else(|| SearchError::Store("vector store not initialised".to_string()))?;
        for id in &existing {
            store.remove(key_of(*id)?)?;
        }
        for (id, vector) in new_ids.iter().zip(&vectors) {
            store.add(key_of(*id)?, vector)?;
        }
        stats.documents_indexed += 1;
        stats.chunks_added += u64::try_from(chunks.len()).unwrap_or(u64::MAX);
        stats.chunks_removed += u64::try_from(existing.len()).unwrap_or(u64::MAX);
        Ok(())
    }

    fn chunk_pages(&self, doc: &LedgerDoc, pages: &[(u32, String)]) -> Vec<Chunk> {
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut next_idx: u32 = 0;
        for (page, text) in pages {
            let page_chunks = self
                .chunker
                .chunk_page(&doc.hash, doc.run_id, *page, next_idx, text);
            let added = u32::try_from(page_chunks.len()).unwrap_or(u32::MAX);
            next_idx = next_idx.saturating_add(added);
            chunks.extend(page_chunks);
        }
        chunks
    }

    /// Search the index. `embedder` must be the one the index was built
    /// with; it is not used in [`SearchMode::Lexical`].
    pub fn search(
        &self,
        query: &str,
        k: usize,
        mode: SearchMode,
        embedder: &dyn Embedder,
    ) -> Result<Vec<Hit>, SearchError> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let mut hits: Vec<Hit> = Vec::new();
        match mode {
            SearchMode::Lexical => {
                for (id, score, snippet) in self.lexical(query, k)? {
                    if let Some(hit) = self.hit(id, score, Some(snippet))? {
                        hits.push(hit);
                    }
                }
            }
            SearchMode::Semantic => {
                for (id, score) in self.semantic(query, k, embedder)? {
                    if let Some(hit) = self.hit(id, f64::from(score), None)? {
                        hits.push(hit);
                    }
                }
            }
            SearchMode::Hybrid { alpha } => {
                let alpha = if alpha.is_nan() {
                    0.5
                } else {
                    alpha.clamp(0.0, 1.0)
                };
                let depth = k.saturating_mul(4).max(50);
                let lexical = self.lexical(query, depth)?;
                let semantic = self.semantic(query, depth, embedder)?;
                let semantic_keys: Vec<u64> = semantic.iter().map(|(id, _)| *id).collect();
                let lexical_keys: Vec<u64> = lexical.iter().map(|(id, _, _)| *id).collect();
                let mut snippets: HashMap<u64, String> =
                    lexical.into_iter().map(|(id, _, s)| (id, s)).collect();
                let lists: [(f64, &[u64]); 2] = [
                    (alpha, semantic_keys.as_slice()),
                    (1.0 - alpha, lexical_keys.as_slice()),
                ];
                for (id, score) in reciprocal_rank_fusion(&lists, RRF_K).into_iter().take(k) {
                    if let Some(hit) = self.hit(id, score, snippets.remove(&id))? {
                        hits.push(hit);
                    }
                }
            }
        }
        Ok(hits)
    }

    /// Size and configuration of the index.
    pub fn stats(&self) -> Result<IndexSummary, SearchError> {
        let (documents, chunks): (i64, i64) = self.conn.query_row(
            "SELECT COUNT(DISTINCT doc_hash), COUNT(*) FROM chunks",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let vectors = self
            .store
            .as_ref()
            .map_or(0, |s| u64::try_from(s.count()).unwrap_or(u64::MAX));
        Ok(IndexSummary {
            documents: u64::try_from(documents).unwrap_or(0),
            chunks: u64::try_from(chunks).unwrap_or(0),
            vectors,
            dim: self.meta_dim()?,
            embedder: get_meta(&self.conn, "embedder")?,
            store: get_meta(&self.conn, "store")?.unwrap_or_else(|| DEFAULT_STORE.to_string()),
        })
    }

    fn lexical(&self, query: &str, limit: usize) -> Result<Vec<(u64, f64, String)>, SearchError> {
        let Some(fts) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare(LEXICAL)?;
        let rows = stmt.query_map(params![fts, limit], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut out: Vec<(u64, f64, String)> = Vec::new();
        for row in rows {
            let (rowid, bm25, snippet) = row?;
            // FTS5's bm25() is lower-is-better; negate so higher is better.
            out.push((key_of(rowid)?, -bm25, snippet));
        }
        Ok(out)
    }

    fn semantic(
        &self,
        query: &str,
        limit: usize,
        embedder: &dyn Embedder,
    ) -> Result<Vec<(u64, f32)>, SearchError> {
        let Some(store) = &self.store else {
            return Ok(Vec::new());
        };
        self.check_embedder(embedder)?;
        let vectors = embedder.embed(&[query])?;
        let Some(q) = vectors.first() else {
            return Ok(Vec::new());
        };
        store.search(q, limit)
    }

    fn hit(
        &self,
        id: u64,
        score: f64,
        snippet: Option<String>,
    ) -> Result<Option<Hit>, SearchError> {
        let rowid = i64::try_from(id).map_err(|e| SearchError::Corrupt(e.to_string()))?;
        let hit = self
            .conn
            .query_row(CHUNK_BY_ID, params![rowid], |row| {
                let text: String = row.get(3)?;
                Ok(Hit {
                    doc_hash: row.get(0)?,
                    page: row.get(1)?,
                    idx: row.get(2)?,
                    score,
                    snippet: snippet.unwrap_or_else(|| leading_words(&text, SNIPPET_WORDS)),
                    title: row.get(4)?,
                    doi: row.get(5)?,
                })
            })
            .optional()?;
        Ok(hit)
    }

    fn check_embedder(&self, embedder: &dyn Embedder) -> Result<(), SearchError> {
        let given = embedder.name();
        if let Some(index) = get_meta(&self.conn, "embedder")?
            && index != given
        {
            return Err(SearchError::EmbedderMismatch { index, given });
        }
        if let Some(dim) = self.meta_dim()?
            && dim != embedder.dim()
        {
            return Err(SearchError::DimensionMismatch {
                expected: dim,
                found: embedder.dim(),
            });
        }
        Ok(())
    }

    fn meta_dim(&self) -> Result<Option<usize>, SearchError> {
        match get_meta(&self.conn, "dim")? {
            Some(value) => value
                .parse::<usize>()
                .map(Some)
                .map_err(|e| SearchError::Corrupt(format!("meta dim `{value}`: {e}"))),
            None => Ok(None),
        }
    }

    fn load_store(&self) -> Result<Option<Box<dyn VectorStore>>, SearchError> {
        let Some(dim) = self.meta_dim()? else {
            return Ok(None);
        };
        let kind = get_meta(&self.conn, "store")?.unwrap_or_else(|| DEFAULT_STORE.to_string());
        open_store(&kind, dim, &store_path(&self.dir, &kind)).map(Some)
    }
}

/// Turn free text into an FTS5 query: every alphanumeric run becomes a
/// quoted term and the terms are OR-ed (BM25 then rewards documents that
/// match more of them). Returns `None` when there is no term.
pub fn fts_query(text: &str) -> Option<String> {
    let terms: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}

fn leading_words(text: &str, n: usize) -> String {
    text.split_whitespace()
        .take(n)
        .collect::<Vec<&str>>()
        .join(" ")
}

fn key_of(id: i64) -> Result<u64, SearchError> {
    u64::try_from(id).map_err(|e| SearchError::Corrupt(format!("chunk id {id}: {e}")))
}

fn store_path(dir: &Path, kind: &str) -> PathBuf {
    dir.join(format!("vectors.{kind}"))
}

fn open_store(kind: &str, dim: usize, path: &Path) -> Result<Box<dyn VectorStore>, SearchError> {
    match kind {
        "flat" => {
            if path.exists() {
                let store = FlatStore::load(path)?;
                if store.dim() != dim {
                    return Err(SearchError::DimensionMismatch {
                        expected: dim,
                        found: store.dim(),
                    });
                }
                Ok(Box::new(store))
            } else {
                Ok(Box::new(FlatStore::new(dim)))
            }
        }
        #[cfg(feature = "usearch")]
        "usearch" => {
            if path.exists() {
                Ok(Box::new(UsearchStore::load(path, dim)?))
            } else {
                Ok(Box::new(UsearchStore::new(dim)?))
            }
        }
        other => Err(SearchError::Store(format!(
            "vector store `{other}` is not compiled in (enable its cargo feature)"
        ))),
    }
}

fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>, SearchError> {
    let value = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value)
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<(), SearchError> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn chunk_ids(conn: &Connection, doc_hash: &str) -> Result<Vec<i64>, SearchError> {
    let mut stmt = conn.prepare(CHUNK_IDS)?;
    let rows = stmt.query_map(params![doc_hash], |row| row.get::<_, i64>(0))?;
    let mut out: Vec<i64> = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn run_pages(ledger: &Connection, run_id: i64) -> Result<Vec<(u32, String)>, SearchError> {
    let mut stmt = ledger.prepare(RUN_PAGES)?;
    let rows = stmt.query_map(params![run_id], |row| {
        Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out: Vec<(u32, String)> = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Content fingerprint of a ledger run: SHA-256 over the length-prefixed run
/// id, document hash, backend name, backend version, config digest,
/// `finished_at`, page count, the SHA-256 of every page (number and
/// length-prefixed text, in page order), and the title and DOI that are
/// copied into the chunk rows. A run rewritten under a reused `runs.id`
/// with different text gets a different fingerprint.
fn run_fingerprint(doc: &LedgerDoc, pages: &[(u32, String)]) -> String {
    let mut page_bytes: Vec<u8> = Vec::new();
    for (page, text) in pages {
        page_bytes.extend_from_slice(&page.to_be_bytes());
        push_field(&mut page_bytes, text.as_bytes());
    }
    let pages_digest = sha256_hex(&page_bytes);
    let page_count = u64::try_from(pages.len()).unwrap_or(u64::MAX);
    let mut input: Vec<u8> = Vec::new();
    input.extend_from_slice(&doc.run_id.to_be_bytes());
    push_field(&mut input, doc.hash.as_bytes());
    push_field(&mut input, doc.backend_name.as_bytes());
    push_field(&mut input, doc.backend_version.as_bytes());
    push_field(&mut input, doc.config_digest.as_bytes());
    input.extend_from_slice(&doc.finished_at.to_be_bytes());
    input.extend_from_slice(&page_count.to_be_bytes());
    push_field(&mut input, pages_digest.as_bytes());
    push_optional(&mut input, doc.title.as_deref());
    push_optional(&mut input, doc.doi.as_deref());
    sha256_hex(&input)
}

fn push_field(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

fn push_optional(out: &mut Vec<u8>, value: Option<&str>) {
    if let Some(text) = value {
        out.push(1);
        push_field(out, text.as_bytes());
    } else {
        out.push(0);
    }
}

fn latest_runs(ledger: &Connection) -> Result<Vec<LedgerDoc>, SearchError> {
    let mut stmt = ledger.prepare(LATEST_RUNS)?;
    let rows = stmt.query_map([], |row| {
        Ok(LedgerDoc {
            run_id: row.get(0)?,
            hash: row.get(1)?,
            title: row.get(2)?,
            doi: row.get(3)?,
            backend_name: row.get(4)?,
            backend_version: row.get(5)?,
            config_digest: row.get(6)?,
            finished_at: row.get(7)?,
        })
    })?;
    let mut out: Vec<LedgerDoc> = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn embed_all(embedder: &dyn Embedder, chunks: &[Chunk]) -> Result<Vec<Vec<f32>>, SearchError> {
    let dim = embedder.dim();
    let mut out: Vec<Vec<f32>> = Vec::with_capacity(chunks.len());
    for batch in chunks.chunks(EMBED_BATCH) {
        let texts: Vec<&str> = batch.iter().map(|c| c.text.as_str()).collect();
        let vectors = embedder.embed(&texts)?;
        if vectors.len() != texts.len() {
            return Err(SearchError::Embed(format!(
                "embedder returned {} vectors for {} texts",
                vectors.len(),
                texts.len()
            )));
        }
        if let Some(bad) = vectors.iter().find(|v| v.len() != dim) {
            return Err(SearchError::DimensionMismatch {
                expected: dim,
                found: bad.len(),
            });
        }
        out.extend(vectors);
    }
    Ok(out)
}

/// Lower-case hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::embed::HashEmbedder;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    // Copied verbatim from `text-processing-engine/src/ledger.rs` (`SCHEMA_SQL`):
    // the four ledger tables the index reads.
    const LEDGER_DDL: &str = r"
CREATE TABLE IF NOT EXISTS documents (
    hash TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    pages INTEGER NOT NULL DEFAULT 0,
    first_seen INTEGER NOT NULL
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
";

    struct Doc<'a> {
        hash: &'a str,
        status: &'a str,
        finished_at: i64,
        title: Option<&'a str>,
        doi: Option<&'a str>,
        pages: &'a [&'a str],
    }

    fn add_run(conn: &Connection, doc: &Doc<'_>) -> i64 {
        let n_pages = i64::try_from(doc.pages.len()).unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO documents (hash, size, pages, first_seen) \
             VALUES (?1, 1000, ?2, 0)",
            params![doc.hash, n_pages],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runs (hash, backend_name, backend_version, config_digest, \
             schema_version, status, started_at, finished_at, timings_json, warnings_json) \
             VALUES (?1, 'lopdf', '0.45', ?2, 1, ?3, 0, ?4, '{}', '[]')",
            params![
                doc.hash,
                format!("cfg{}", doc.finished_at),
                doc.status,
                doc.finished_at
            ],
        )
        .unwrap();
        let run_id = conn.last_insert_rowid();
        for (i, text) in doc.pages.iter().enumerate() {
            let page = i64::try_from(i + 1).unwrap();
            conn.execute(
                "INSERT INTO pages (run_id, page, width, height, rotation, text, spans_json, \
                 lines_json, warnings_json) VALUES (?1, ?2, 612.0, 792.0, 0, ?3, '[]', '[]', '[]')",
                params![run_id, page, text],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO metadata (run_id, title, doi, keywords_json, info_json, provenance_json) \
             VALUES (?1, ?2, ?3, '[]', '{}', '{}')",
            params![run_id, doc.title, doc.doi],
        )
        .unwrap();
        run_id
    }

    fn make_ledger(path: &Path, docs: &[Doc<'_>]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(LEDGER_DDL).unwrap();
        for doc in docs {
            add_run(&conn, doc);
        }
    }

    const PHOTO_P1: &str = "Photosynthesis converts light energy into chemical energy. \
        Chlorophyll in plant leaves absorbs light. Photosynthesis produces oxygen and \
        glucose inside chloroplasts.";
    const PHOTO_P2: &str = "The Calvin cycle fixes carbon dioxide during photosynthesis.";
    const NEURAL: &str = "Neural networks learn representations with gradient descent. \
        Deep neural networks stack many layers of artificial neurons trained by \
        backpropagation.";

    #[cfg(unix)]
    #[test]
    fn open_makes_index_paths_owner_only() {
        let parent = tempfile::tempdir().unwrap();
        let index_dir = parent.path().join("idx");
        fs::create_dir(&index_dir).unwrap();
        fs::set_permissions(&index_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let db_path = index_dir.join(INDEX_DB);
        fs::write(&db_path, []).unwrap();
        fs::set_permissions(&db_path, fs::Permissions::from_mode(0o644)).unwrap();

        let index = SearchIndex::open(&index_dir).unwrap();
        assert_eq!(
            fs::metadata(&index_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&db_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(index);
    }
    const VOLCANO: &str = "Volcanoes erupt magma, ash and gas. Volcanic activity is \
        monitored with seismometers and satellite radar.";

    fn three_docs() -> Vec<Doc<'static>> {
        vec![
            Doc {
                hash: "a1a1a1a1a1a1a1a1",
                status: "complete",
                finished_at: 10,
                title: Some("Photosynthesis in leaves"),
                doi: Some("10.1000/photo"),
                pages: &[PHOTO_P1, PHOTO_P2],
            },
            Doc {
                hash: "b2b2b2b2b2b2b2b2",
                status: "complete",
                finished_at: 10,
                title: Some("Neural networks"),
                doi: None,
                pages: &[NEURAL],
            },
            Doc {
                hash: "c3c3c3c3c3c3c3c3",
                status: "partial",
                finished_at: 10,
                title: None,
                doi: None,
                pages: &[VOLCANO],
            },
        ]
    }

    #[test]
    fn fts_query_quotes_terms() {
        assert_eq!(
            fts_query("C++ \"x\" (y) AND"),
            Some("\"C\" OR \"x\" OR \"y\" OR \"AND\"".to_string())
        );
        assert_eq!(fts_query(" !!! "), None);
    }

    #[test]
    fn fts5_bm25_ranks_by_term_frequency() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.sqlite");
        make_ledger(
            &ledger,
            &[
                Doc {
                    hash: "x",
                    status: "complete",
                    finished_at: 1,
                    title: Some("Graphene"),
                    doi: None,
                    pages: &["graphene graphene graphene sheets conduct well. Graphene is strong."],
                },
                Doc {
                    hash: "y",
                    status: "complete",
                    finished_at: 1,
                    title: None,
                    doi: None,
                    pages: &[
                        "carbon nanotubes and one graphene mention among many other \
                              words about carbon materials and their uses",
                    ],
                },
                Doc {
                    hash: "z",
                    status: "complete",
                    finished_at: 1,
                    title: None,
                    doi: None,
                    pages: &["nothing relevant here at all"],
                },
            ],
        );
        let embedder = HashEmbedder::default();
        let mut index = SearchIndex::open(&dir.path().join("idx")).unwrap();
        index.index_ledger(&ledger, &embedder).unwrap();
        let hits = index
            .search("graphene", 10, SearchMode::Lexical, &embedder)
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].doc_hash, "x");
        assert_eq!(hits[1].doc_hash, "y");
        assert!(hits[0].score > hits[1].score);
        assert!(hits[0].snippet.to_lowercase().contains("[graphene]"));
        assert_eq!(hits[0].title.as_deref(), Some("Graphene"));
        assert!(
            index
                .search("absent", 10, SearchMode::Lexical, &embedder)
                .unwrap()
                .is_empty()
        );
        assert!(
            index
                .search("...", 10, SearchMode::Lexical, &embedder)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn end_to_end_index_and_query() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.sqlite");
        make_ledger(&ledger, &three_docs());
        let idx_dir = dir.path().join("idx");
        let embedder = HashEmbedder::default();
        {
            let mut index = SearchIndex::open(&idx_dir).unwrap();
            let stats = index.index_ledger(&ledger, &embedder).unwrap();
            assert_eq!(stats.documents_seen, 3);
            assert_eq!(stats.documents_indexed, 3);
            assert_eq!(stats.chunks_added, 4);
            assert!(!stats.rebuilt);

            let semantic = index
                .search(
                    "chlorophyll absorbs light in plant leaves",
                    3,
                    SearchMode::Semantic,
                    &embedder,
                )
                .unwrap();
            assert_eq!(semantic[0].doc_hash, "a1a1a1a1a1a1a1a1");
            assert_eq!(semantic[0].page, 1);
            assert_eq!(semantic[0].idx, 0);
            assert_eq!(semantic[0].doi.as_deref(), Some("10.1000/photo"));
            assert!(semantic[0].snippet.starts_with("Photosynthesis converts"));

            let lexical = index
                .search("magma", 5, SearchMode::Lexical, &embedder)
                .unwrap();
            assert_eq!(lexical.len(), 1);
            assert_eq!(lexical[0].doc_hash, "c3c3c3c3c3c3c3c3");
            assert_eq!(lexical[0].title, None);

            let hybrid = index
                .search(
                    "neural networks gradient descent",
                    2,
                    SearchMode::Hybrid { alpha: 0.5 },
                    &embedder,
                )
                .unwrap();
            assert_eq!(hybrid.len(), 2);
            assert_eq!(hybrid[0].doc_hash, "b2b2b2b2b2b2b2b2");
            assert!(hybrid[0].snippet.contains('['));

            let summary = index.stats().unwrap();
            assert_eq!(summary.documents, 3);
            assert_eq!(summary.chunks, 4);
            assert_eq!(summary.vectors, 4);
            assert_eq!(summary.dim, Some(256));
            assert_eq!(summary.embedder.as_deref(), Some("hash-256"));
            assert_eq!(summary.store, DEFAULT_STORE);

            let again = index.index_ledger(&ledger, &embedder).unwrap();
            assert_eq!(again.documents_unchanged, 3);
            assert_eq!(again.documents_indexed, 0);
            assert_eq!(again.chunks_added, 0);
        }
        // Reopen from disk: the vectors and settings persist.
        let index = SearchIndex::open(&idx_dir).unwrap();
        assert_eq!(index.stats().unwrap().vectors, 4);
        let hits = index
            .search("volcanoes erupt magma", 1, SearchMode::Semantic, &embedder)
            .unwrap();
        assert_eq!(hits[0].doc_hash, "c3c3c3c3c3c3c3c3");
    }

    #[test]
    fn newer_run_replaces_chunks_and_failed_runs_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.sqlite");
        make_ledger(&ledger, &three_docs());
        let embedder = HashEmbedder::default();
        let mut index = SearchIndex::open(&dir.path().join("idx")).unwrap();
        index.index_ledger(&ledger, &embedder).unwrap();

        let conn = Connection::open(&ledger).unwrap();
        add_run(
            &conn,
            &Doc {
                hash: "a1a1a1a1a1a1a1a1",
                status: "complete",
                finished_at: 20,
                title: Some("Entanglement"),
                doi: None,
                pages: &["Entirely new text about quantum entanglement."],
            },
        );
        add_run(
            &conn,
            &Doc {
                hash: "b2b2b2b2b2b2b2b2",
                status: "failed",
                finished_at: 30,
                title: None,
                doi: None,
                pages: &[],
            },
        );
        drop(conn);

        let stats = index.index_ledger(&ledger, &embedder).unwrap();
        assert_eq!(stats.documents_seen, 3);
        assert_eq!(stats.documents_indexed, 1);
        assert_eq!(stats.documents_unchanged, 2);
        assert_eq!(stats.chunks_removed, 2);
        assert_eq!(stats.chunks_added, 1);
        assert!(
            index
                .search("chlorophyll", 5, SearchMode::Lexical, &embedder)
                .unwrap()
                .is_empty()
        );
        let hits = index
            .search("entanglement", 5, SearchMode::Lexical, &embedder)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title.as_deref(), Some("Entanglement"));
        let summary = index.stats().unwrap();
        assert_eq!(summary.chunks, 3);
        assert_eq!(summary.vectors, 3);
    }

    #[test]
    fn reused_run_id_with_new_text_is_reindexed() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.sqlite");
        make_ledger(&ledger, &three_docs());
        let embedder = HashEmbedder::default();
        let mut index = SearchIndex::open(&dir.path().join("idx")).unwrap();
        index.index_ledger(&ledger, &embedder).unwrap();

        // Rewrite the run the way `Ledger::write_result` does (delete, then
        // reinsert under the same identity), keeping the same `runs.id`, hash,
        // backend, config digest, `finished_at`, page count, title and DOI:
        // only the page text differs.
        let conn = Connection::open(&ledger).unwrap();
        let old_id: i64 = conn
            .query_row(
                "SELECT id FROM runs WHERE hash = 'a1a1a1a1a1a1a1a1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute_batch(&format!(
            "DELETE FROM pages WHERE run_id = {old_id}; \
             DELETE FROM metadata WHERE run_id = {old_id}; \
             DELETE FROM runs WHERE id = {old_id};"
        ))
        .unwrap();
        conn.execute(
            "INSERT INTO runs (id, hash, backend_name, backend_version, config_digest, \
             schema_version, status, started_at, finished_at, timings_json, warnings_json) \
             VALUES (?1, 'a1a1a1a1a1a1a1a1', 'lopdf', '0.45', 'cfg10', 1, 'complete', 0, 10, \
             '{}', '[]')",
            params![old_id],
        )
        .unwrap();
        for (page, text) in [
            (
                1_i64,
                "Superconductivity appears in cuprates below a critical temperature.",
            ),
            (2_i64, "Meissner effect expels magnetic fields."),
        ] {
            conn.execute(
                "INSERT INTO pages (run_id, page, width, height, rotation, text, spans_json, \
                 lines_json, warnings_json) VALUES (?1, ?2, 612.0, 792.0, 0, ?3, '[]', '[]', '[]')",
                params![old_id, page, text],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO metadata (run_id, title, doi, keywords_json, info_json, provenance_json) \
             VALUES (?1, 'Photosynthesis in leaves', '10.1000/photo', '[]', '{}', '{}')",
            params![old_id],
        )
        .unwrap();
        let new_id: i64 = conn
            .query_row(
                "SELECT id FROM runs WHERE hash = 'a1a1a1a1a1a1a1a1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new_id, old_id);
        drop(conn);

        let stats = index.index_ledger(&ledger, &embedder).unwrap();
        assert_eq!(stats.documents_seen, 3);
        assert_eq!(stats.documents_indexed, 1);
        assert_eq!(stats.documents_unchanged, 2);
        assert_eq!(stats.chunks_removed, 2);
        assert_eq!(stats.chunks_added, 2);
        assert!(
            index
                .search("chlorophyll", 5, SearchMode::Lexical, &embedder)
                .unwrap()
                .is_empty()
        );
        let hits = index
            .search("superconductivity", 5, SearchMode::Lexical, &embedder)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].doc_hash, "a1a1a1a1a1a1a1a1");
        assert_eq!(hits[0].page, 1);
        let summary = index.stats().unwrap();
        assert_eq!(summary.chunks, 4);
        assert_eq!(summary.vectors, 4);

        // Indexing again without further changes is a no-op.
        let again = index.index_ledger(&ledger, &embedder).unwrap();
        assert_eq!(again.documents_unchanged, 3);
        assert_eq!(again.documents_indexed, 0);
    }

    #[test]
    fn embedder_mismatch_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.sqlite");
        make_ledger(&ledger, &three_docs());
        let mut index = SearchIndex::open(&dir.path().join("idx")).unwrap();
        index
            .index_ledger(&ledger, &HashEmbedder::default())
            .unwrap();
        let other = HashEmbedder::new(128);
        assert!(matches!(
            index.search("light", 3, SearchMode::Semantic, &other),
            Err(SearchError::EmbedderMismatch { .. })
        ));
        assert!(matches!(
            index.index_ledger(&ledger, &other),
            Err(SearchError::EmbedderMismatch { .. })
        ));
        // Lexical search does not need the embedder.
        assert!(
            !index
                .search("light", 3, SearchMode::Lexical, &other)
                .unwrap()
                .is_empty()
        );
        // After a reset the index can be rebuilt with the other embedder.
        index.reset().unwrap();
        let stats = index.index_ledger(&ledger, &other).unwrap();
        assert_eq!(stats.documents_indexed, 3);
        assert_eq!(index.stats().unwrap().dim, Some(128));
    }

    #[test]
    fn interrupted_index_is_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.sqlite");
        make_ledger(&ledger, &three_docs());
        let embedder = HashEmbedder::default();
        let mut index = SearchIndex::open(&dir.path().join("idx")).unwrap();
        index.index_ledger(&ledger, &embedder).unwrap();
        set_meta(&index.conn, "dirty", "1").unwrap();
        let stats = index.index_ledger(&ledger, &embedder).unwrap();
        assert!(stats.rebuilt);
        assert_eq!(stats.documents_indexed, 3);
        assert_eq!(index.stats().unwrap().vectors, 4);
    }

    #[test]
    fn documents_without_text_are_not_reindexed() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.sqlite");
        let mut docs = three_docs();
        docs.push(Doc {
            hash: "d4d4d4d4d4d4d4d4",
            status: "complete",
            finished_at: 10,
            title: Some("Scanned"),
            doi: None,
            pages: &["   ", ""],
        });
        make_ledger(&ledger, &docs);
        let embedder = HashEmbedder::default();
        let mut index = SearchIndex::open(&dir.path().join("idx")).unwrap();
        let first = index.index_ledger(&ledger, &embedder).unwrap();
        assert_eq!(first.documents_indexed, 4);
        assert_eq!(first.chunks_added, 4);
        let second = index.index_ledger(&ledger, &embedder).unwrap();
        assert_eq!(second.documents_unchanged, 4);
        assert_eq!(second.documents_indexed, 0);
    }

    #[test]
    fn reads_a_wal_ledger_while_a_writer_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.sqlite");
        make_ledger(&ledger, &three_docs());
        // The engine's ledger runs in WAL mode (`ledger.rs` `FILE_PRAGMAS`).
        let writer = Connection::open(&ledger).unwrap();
        let mode: String = writer
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        add_run(
            &writer,
            &Doc {
                hash: "e5e5e5e5e5e5e5e5",
                status: "complete",
                finished_at: 11,
                title: None,
                doi: None,
                pages: &["Tidal forces shape planetary rings."],
            },
        );
        let embedder = HashEmbedder::default();
        let mut index = SearchIndex::open(&dir.path().join("idx")).unwrap();
        let stats = index.index_ledger(&ledger, &embedder).unwrap();
        assert_eq!(stats.documents_indexed, 4);
        let hits = index
            .search("planetary rings", 1, SearchMode::Lexical, &embedder)
            .unwrap();
        assert_eq!(hits[0].doc_hash, "e5e5e5e5e5e5e5e5");
        drop(writer);
    }

    #[test]
    fn zero_k_and_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let embedder = HashEmbedder::default();
        let index = SearchIndex::open(&dir.path().join("idx")).unwrap();
        for mode in [
            SearchMode::Lexical,
            SearchMode::Semantic,
            SearchMode::Hybrid { alpha: 0.5 },
        ] {
            assert!(
                index
                    .search("anything", 5, mode, &embedder)
                    .unwrap()
                    .is_empty()
            );
            assert!(
                index
                    .search("anything", 0, mode, &embedder)
                    .unwrap()
                    .is_empty()
            );
        }
        let summary = index.stats().unwrap();
        assert_eq!(summary.chunks, 0);
        assert_eq!(summary.dim, None);
    }
}

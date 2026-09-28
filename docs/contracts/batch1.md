# Baseline PR contract (branch feat/engine-baseline)

Repo: <repo>
Crate: lib `tpe` (src/lib.rs), bin `tpe` (src/main.rs). Edition 2024. Rust stable (assume 1.90).
Read first: Cargo.toml, src/lib.rs, src/schema.rs, src/backend/mod.rs. These are FIXED; do not edit them
(if you truly need a change, put a note in your final report; do not make it).
Offline crate sources for exact APIs (read them, never guess an API):
  <scratchpad>/crates/{lopdf-0.45.0,rusqlite-0.40.2,clap-4.5.60}

HARD RULES (this machine is a fanless Raspberry Pi, the user is on an iPhone):
- NEVER run cargo, rustc, rustup, uv, pip, swift, cmake, or any compiler/test. CI compiles. Do not "just check".
- NO network access. No git commands. Do not touch files you do not own.
- Code must pass `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` with `pedantic = warn`
  (lib.rs allows: missing_errors_doc, missing_panics_doc, module_name_repetitions, must_use_candidate,
  cast_precision_loss, cast_possible_truncation, cast_sign_loss, too_many_lines), and `cargo test`.
  Because you cannot compile, be conservative: simple ownership, explicit types, no clever generics,
  no unwrap() outside tests, format!("{var}") inline args (clippy::uninlined_format_args), doc comments
  with backticks around identifiers (clippy::doc_markdown), `#[must_use]` not needed, use `Self`,
  no `&Vec<T>`/`&String` params, no needless `return`, no `.len() == 0`, `f32::from(x)` for widening,
  `u32::try_from` for narrowing where sensible, no wildcard imports, no `pub(crate)` needed.
  rustfmt default style: 4 spaces, max width 100, trailing commas, imports grouped std / external / crate.
- Every public fn gets a `///` doc line. Tests go in `#[cfg(test)] mod tests` in the same file unless told otherwise.
- Never fabricate: a field is None unless evidence supports it. No language-model or dictionary "repair" of text.

## Shared types (src/schema.rs) — use exactly these
ContentHash(String), BackendIdentity{name,version,config_digest}, BBox{x0,y0,x1,y1}: f32,
Span{text,bbox:Option<BBox>,font:Option<String>,size:Option<f32>,seq:u32},
Line{text,bbox,column:u32,spans:Vec<u32>}, PageText{page,width,height,rotation,spans,lines,text,warnings} + PageText::new(page,width,height,rotation),
Author{name,affiliation,orcid,email}, Metadata{title,authors,doi,arxiv_id,year:Option<u16>,venue,abstract_text,keywords,info:BTreeMap,provenance:BTreeMap},
ReferenceEntry{index,label,raw,authors:Vec<String>,title,year:Option<u16>,venue,volume,issue,pages,doi,arxiv_id,url,page:u32} (Default),
CitationMarker{page,offset:u32,text,targets:Vec<u32>}, Status{Complete,Partial,Failed,Deferred} + as_str(),
StageTimings{acquire_ms,parse_ms,order_ms,metadata_ms,citations_ms,write_ms}: f64,
ChunkResult{chunk_index,first_page,last_page,status,text_sha256,ms}, SourceObservation{path,inode,device,mtime_unix,size},
Document{hash,size,pages,sources}, ExtractionResult{schema_version,document,backend,status,pages,chunks,metadata,references,citations,warnings,timings},
Job{path,backend,pages:Option<(u32,u32)>,password,max_bytes}, sha256_hex(&[u8])->String, config_digest(&BTreeMap<String,String>)->String,
SCHEMA_VERSION: u32 = 1, CHUNK_PAGES: u32 = 20.
Backend traits (src/backend/mod.rs): BackendError{Malformed(String),Encrypted(EncryptionProblem),PageRange{page,count},Page{page,message},Unsupported(String),Limit(String)},
EncryptionProblem{PasswordRequired,WrongPassword,UnsupportedCipher},
trait DocumentSession{page_count()->u32; page_text(&mut self,page:u32)->Result<PageText,BackendError>; info()->BTreeMap<String,String>},
trait Extractor: Send+Sync {identity()->BackendIdentity; open(&self,bytes:&[u8],password:Option<&str>)->Result<Box<dyn DocumentSession>,BackendError>},
by_name(&str)->Option<Box<dyn Extractor>> (knows "lopdf" -> lopdf_backend::LopdfBackend::default()).

## Module signatures (each owned by ONE agent)

### src/acquire.rs
pub enum AcquireError { Io(std::io::Error) [#[from]], TooLarge{size:u64,max:u64}, Empty, ChangedDuringRead, NotAFile }  (thiserror)
pub struct Snapshot { pub bytes: Vec<u8>, pub hash: ContentHash, pub source: SourceObservation }
pub fn snapshot(path: &std::path::Path, max_bytes: Option<u64>) -> Result<Snapshot, AcquireError>
  // stat before, read all, stat after; size/mtime mismatch => ChangedDuringRead. inode/device via std::os::unix::fs::MetadataExt under #[cfg(unix)].

### src/backend/lopdf_backend.rs
pub struct LopdfBackend { pub max_xobject_depth: u32 }  impl Default (depth 8)
impl Extractor for LopdfBackend  // identity: name "lopdf", version "0.45", config_digest over {"max_xobject_depth"}
  // open: lopdf::Document::load_mem; encrypted docs: if password given try decrypt else PasswordRequired.
  // page_text: interpret the content stream (BT/ET, Tf, Td, TD, Tm, T*, TL, Tc, Tw, Tz, Ts, Tj, TJ, ', ", q/Q, cm, Do for Form XObjects
  // bounded by max_xobject_depth). One Span per Tj/TJ string piece; text decoded with the page font's Encoding
  // (lopdf: doc.get_page_fonts, Dictionary::get_font_encoding(&doc), Document::decode_text), NFC-normalised.
  // bbox from text matrix × CTM: x from current text position, advance using /Widths (or /W for Type0) else 500/1000 em;
  // y0 = baseline + descent estimate (-0.2*size), y1 = baseline + 0.8*size. Page width/height from CropBox else MediaBox
  // (walk up /Parent), rotation from /Rotate (inherited). Undecodable bytes => U+FFFD and a page warning, never dropped silently.

### src/reading_order.rs
pub fn order_page(page: &mut PageText)   // fills page.lines and page.text from page.spans; idempotent
  // 1) drop empty/whitespace-only spans from consideration; 2) group into lines: same baseline (|y0 diff| <= 0.4*size) and overlapping/near x ranges;
  // 3) within a line sort spans by x0; insert a single space between spans when gap > 0.15*size and neither side already has a space;
  // 4) column detection: recursive XY-cut on the union of line boxes (vertical gaps > 1.0*median line height split into rows;
  //    horizontal gaps > 2.0*median char width split into columns; columns ordered left→right, rows top→bottom); 5) paragraph break
  //    ("\n\n") when vertical gap between consecutive lines in a column > 1.5*median line height, else "\n". No dehyphenation, no repair.
  // Spans without bbox go in content-stream order at the end as their own column with a warning.

### src/metadata.rs
pub fn extract_metadata(info: &BTreeMap<String,String>, pages: &[PageText]) -> Metadata
  // info Title/Author/Subject/Keywords/CreationDate → fields with provenance "info:<Key>"; prefer page-1 evidence when info is empty or generic
  // ("untitled", a filename, "Microsoft Word - ..."); title = largest-font line group near top of page 1; authors = lines between title and
  // abstract split on ",", " and ", "&" with superscript/digit affiliation markers stripped; DOI regex 10\.\d{4,9}/[^\s"<>]+ (trim trailing punctuation);
  // arXiv id \d{4}\.\d{4,5}(v\d+)? or old style; year from DOI/arXiv/info date/page-1 4-digit 19xx/20xx; abstract = text between a line "Abstract"
  // and the next heading (Introduction / 1 / Keywords). Every field set records provenance.

### src/citations.rs
pub struct ReferenceSection { pub first_page: u32, pub first_line: usize, pub heading: String }
pub fn find_reference_section(pages: &[PageText]) -> Option<ReferenceSection>   // heading line matching ^(\d+\.?\s*)?(References|Bibliography|Works Cited|Literature Cited|REFERENCES)\s*$ ; take the LAST such line
pub fn segment_entries(pages: &[PageText], section: &ReferenceSection) -> Vec<ReferenceEntry>   // raw, label, index, page; stops at Appendix/Supplementary heading or end
   // styles: numbered "[n]" / "n." / "n)" ; author-year: new entry starts when a line begins with a capitalised surname + initials pattern and previous line ended with "." or a year;
   // hanging-indent detection via Line.bbox x0 when available (entry start = line whose x0 is less than the following line's x0).
pub fn parse_entry(entry: &mut ReferenceEntry)   // authors (split before year or before quoted/“ title), year (19xx|20xx), title (segment after authors/year up to next ". "),
   // venue (In Proceedings of…, journal name before volume), volume/issue/pages (vol(issue):pages or pp. x–y), doi, arxiv_id, url. Never invent.
pub fn find_citation_markers(pages: &[PageText], refs: &[ReferenceEntry]) -> Vec<CitationMarker>   // numeric: [1], [2,3], [4–6], superscript digits not attempted; author-year: (Smith, 2020), (Smith et al., 2020; Lee and Kim, 2019), Smith (2020). Resolve to refs by number or by (first author surname, year).
pub fn extract_citations(pages: &[PageText]) -> (Vec<ReferenceEntry>, Vec<ReferenceEntry> is wrong) → signature: pub fn extract_citations(pages: &[PageText]) -> (Vec<ReferenceEntry>, Vec<CitationMarker>)  // convenience: find→segment→parse each→markers; only body pages before the section are searched for markers (plus the section pages before the heading line).
   Requires pages[i].text and pages[i].lines to be filled (reading_order ran). Offsets are char offsets into PageText::text.

### src/ledger.rs   (rusqlite, bundled)
pub type RunId = i64;
pub enum LedgerError { Sqlite(rusqlite::Error) [#[from]], Json(serde_json::Error) [#[from]], NotFound(String), SchemaMismatch{found:u32,expected:u32} }
pub struct RunSummary { pub id: RunId, pub status: Status, pub finished_at: i64 }
pub struct LedgerStats { pub documents:u64, pub runs:u64, pub complete:u64, pub partial:u64, pub failed:u64, pub pages:u64, pub references:u64, pub citations:u64 }
pub struct Ledger { conn: rusqlite::Connection }
impl Ledger {
  pub fn open(path: &std::path::Path) -> Result<Self, LedgerError>   // journal_mode=WAL, synchronous=NORMAL, busy_timeout 5000, foreign_keys ON, creates schema, checks schema_meta.version == SCHEMA_VERSION
  pub fn open_in_memory() -> Result<Self, LedgerError>
  pub fn record_source(&mut self, hash:&ContentHash, size:u64, obs:&SourceObservation) -> Result<(), LedgerError>   // upsert documents(hash,size); insert sources row (dedupe on hash+path+inode+mtime)
  pub fn write_result(&mut self, result:&ExtractionResult) -> Result<RunId, LedgerError>   // ONE transaction; delete+reinsert any prior run with same (hash, backend.name, backend.version, backend.config_digest, schema_version) so publication is idempotent
  pub fn find_run(&self, hash:&ContentHash, backend:&BackendIdentity) -> Result<Option<RunSummary>, LedgerError>
  pub fn load_result(&self, run: RunId) -> Result<ExtractionResult, LedgerError>   // full round trip equal to what was written (spans/lines stored as JSON columns)
  pub fn stats(&self) -> Result<LedgerStats, LedgerError>
}
Tables (all with explicit PK/FK, ON DELETE CASCADE from runs): schema_meta(version), documents(hash PK,size,pages,first_seen), sources(id,hash,path,inode,device,mtime_unix,size,seen_at, UNIQUE(hash,path,inode,mtime_unix)),
runs(id PK, hash, backend_name, backend_version, config_digest, schema_version, status, started_at, finished_at, timings_json, warnings_json, UNIQUE(hash,backend_name,backend_version,config_digest,schema_version)),
pages(run_id,page,width,height,rotation,text,spans_json,lines_json,warnings_json, PK(run_id,page)), chunks(run_id,chunk_index,first_page,last_page,status,text_sha256,ms),
metadata(run_id PK,title,doi,arxiv_id,year,venue,abstract_text,keywords_json,info_json,provenance_json), authors(run_id,seq,name,affiliation,orcid,email),
"references"(run_id,idx,label,raw,title,year,venue,volume,issue,pages,doi,arxiv_id,url,page, PK(run_id,idx)), reference_authors(run_id,ref_idx,seq,name),
citations(id PK,run_id,page,offset,text), citation_targets(citation_id,ref_idx). Indexes on references(doi), metadata(doi), documents(hash) implicit.

### src/pipeline.rs
pub enum PipelineError { Acquire(AcquireError) [#[from]], Backend(BackendError) [#[from]], UnknownBackend(String) }
pub fn run_job(job: &Job) -> Result<ExtractionResult, PipelineError>
  // acquire::snapshot → backend::by_name → open → for each page in range: page_text, on BackendError::Page push warning and continue (status Partial);
  // reading_order::order_page each page; chunks = chunk_results(); metadata::extract_metadata(session.info(), &pages); citations::extract_citations(&pages);
  // timings per stage via std::time::Instant; document.pages = page_count; sources = [snapshot.source].
pub fn chunk_results(pages: &[PageText], parse_plus_order_ms: f64) -> Vec<ChunkResult>   // groups of CHUNK_PAGES by page number; ms apportioned by page count; text_sha256 = sha256_hex of page texts joined by "\n\f\n"; status Complete unless any page in chunk has a warning starting with "failed:" → Partial

### src/main.rs (clap derive, bin name tpe)
tpe extract <PATH>... --db <FILE> [--backend lopdf] [--out <DIR>] [--json] [--password <PW>] [--pages a-b] [--jobs N] [--max-bytes N]
   // N worker threads (std::thread::scope + std::sync::mpsc + Mutex<VecDeque<PathBuf>> queue); ledger written only by the main thread; per file prints one line:
   // "<status>\t<hash[..12]>\t<pages>p\t<n_refs> refs\t<n_cites> cites\t<total_ms> ms\t<path>"; --out writes <hash>.json (ExtractionResult) and <hash>.txt (pages joined by \f);
   // nonzero exit if any file Failed. --json prints one JSON object per line instead of the tab line.
tpe stats --db <FILE>        // prints LedgerStats as key: value lines
tpe show --db <FILE> --hash <PREFIX> [--refs] [--meta] [--text]   // loads latest run for hash prefix and prints requested parts
tpe bench <PATH>... [--backend lopdf] [--iterations N=5]   // warm service time per 20-page chunk: runs run_job N times per file (no ledger), reports per-file p50/p95 ms per chunk and pages/s; prints the 30 ms target gap
Also: tests/common/mod.rs and tests/end_to_end.rs (see agent D).

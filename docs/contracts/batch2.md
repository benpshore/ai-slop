# Batch 2 contract: evaluation harness against arXiv ground truth (branch feat/eval-harness)

Same HARD RULES as CONTRACT.md (read it: same directory). No cargo/rustc/compilers, no network, no git; touch only your files;
rustfmt/clippy-pedantic clean; read APIs from the offline crate sources in scratchpad/crates/ (now also ureq-3.4.2, flate2-1.1.10, tar-0.4.46).
The baseline crate is complete and CI-green: read src/schema.rs, src/lib.rs, src/pipeline.rs (run_job), src/citations.rs (public fns only),
src/ledger.rs (public fns only). Do NOT edit any existing file except where your section says so.

Purpose. The product must capture every reference entry and citation of every paper exactly. arXiv gives an independent ground truth for free:
the e-print source contains either a `.bbl` (the typeset bibliography, one `\bibitem` per printed entry) or a `.bib` plus `\cite{...}` commands in the
`.tex`. We measure: exact reference-count match, per-entry recall/precision, DOI/year/title field accuracy, in-text marker resolution, and a
word-alignment diagnostic of body text order. These are diagnostics on real papers, not the final human-checked acceptance protocol.

Sample real sources (read them to design parsers; do not copy whole files into the repo):
  scratchpad/arxiv/2108.04588/main.bbl (31 bibitems) + main.tex ; scratchpad/arxiv/2502.00857/custom.bib + Main.tex ; scratchpad/arxiv/2412.06210/sample-base.bib + sample-sigconf-authordraft.tex
  CC-BY candidate ids with categories: scratchpad/ccby.txt (582 lines "id<TAB>categories").

New Cargo.toml dependencies (the integration owner adds them, nobody else edits Cargo.toml):
  ureq = "3"  (default features: rustls + gzip), flate2 = "1", tar = "0.4"
New modules registered in src/lib.rs by the integration owner: pub mod corpus; pub mod latex_refs; pub mod eval;

### src/corpus.rs  (owner: E1)
pub enum CorpusError { Io(std::io::Error) [#[from]], Json(serde_json::Error) [#[from]], Http(String), HashMismatch{id:String,expected:String,actual:String}, NoSource(String), Unpack(String) }  (thiserror)
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)] pub struct Manifest { pub version: u32, pub items: Vec<ManifestItem> }
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)] pub struct ManifestItem { pub id: String /* "arxiv:2502.00857" */, pub kind: String /* "arxiv" */, pub license: String /* URL */, pub pdf_url: String, pub source_url: Option<String>, pub pdf_sha256: Option<String>, pub source_sha256: Option<String>, pub categories: Vec<String>, pub split: String /* "dev" | "holdout" */, pub notes: Option<String> }
pub fn load_manifest(path: &Path) -> Result<Manifest, CorpusError>
pub fn save_manifest(path: &Path, manifest: &Manifest) -> Result<(), CorpusError>   // pretty JSON, trailing newline
pub struct Fetched { pub pdf_path: PathBuf, pub source_dir: Option<PathBuf>, pub pdf_sha256: String, pub source_sha256: Option<String> }
pub fn cache_paths(cache_dir: &Path, item: &ManifestItem) -> (PathBuf /*pdf*/, PathBuf /*source archive*/, PathBuf /*source dir*/)   // cache_dir/<id with ':' replaced by '_'>/paper.pdf, source.bin, source/
pub fn fetch_item(item: &ManifestItem, cache_dir: &Path, user_agent: &str, offline: bool) -> Result<Fetched, CorpusError>
   // if cached files exist: reuse (no network). offline && missing => Http("offline"). Download with ureq (Agent with user_agent, 60 s timeouts, follow redirects,
   // one retry after 2 s on transport error). Verify sha256 against manifest when present (HashMismatch), else just report. Unpack source into source dir if archive present.
pub fn unpack_source(bytes: &[u8], dest: &Path) -> Result<(), CorpusError>   // detect: gzip magic 1f 8b -> gunzip; then if tar (ustar or a valid header) -> extract with `tar` (reject entries with .. or absolute paths); else write dest/main.tex. If bytes start with %PDF -> Err(NoSource). Bound total unpacked size 200 MB.
pub struct LatexFiles { pub root: PathBuf, pub tex: Vec<PathBuf>, pub bbl: Vec<PathBuf>, pub bib: Vec<PathBuf> }
pub fn find_latex_files(dir: &Path) -> Result<LatexFiles, CorpusError>   // recursive walk, sorted paths
pub fn update_manifest_hashes(manifest: &mut Manifest, id: &str, fetched: &Fetched) -> bool   // fills missing sha256 fields; returns true if changed
Tests: manifest JSON round trip; unpack_source with an in-test-built tar.gz (use tar::Builder + flate2 GzEncoder) containing a.tex and sub/b.bbl -> find_latex_files finds both; bare gzip single file -> main.tex; %PDF bytes -> NoSource; path traversal entry rejected; cache_paths shape; fetch_item offline with pre-seeded cache returns Fetched without network.

### src/latex_refs.rs  (owner: E2)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)] pub enum TruthSource { Bbl, Bib }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)] pub struct TruthReference { pub key: String, pub label: Option<String>, pub text: String, pub authors: Vec<String>, pub title: Option<String>, pub year: Option<u16>, pub doi: Option<String>, pub arxiv_id: Option<String>, pub source: TruthSource }
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)] pub struct TruthCitations { pub cite_commands: u32, pub cited_keys: Vec<String> /* in order, duplicates kept */, pub nocite_keys: Vec<String>, pub nocite_all: bool }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)] pub struct GroundTruth { pub references: Vec<TruthReference>, pub citations: TruthCitations, pub method: String /* "bbl" | "bib-cited" | "bib-all" */ , pub body_text: String /* detexed body for alignment diagnostics; may be empty */ }
pub enum TruthError { Io(std::io::Error) [#[from]], NoBibliography, NoMainTex }
pub fn latex_to_text(s: &str) -> String   // remove comments (% not escaped), \newblock->space, \emph{x}/\textit/\textbf/\textsc/\texttt/\mbox/\text{x}->x, \url{x}->x, \href{u}{t}->t, \natexlab{x}->x, {\em x}->x, ~ -> space, `` '' -> " , -- -> –, --- -> —, \& -> &, \% -> %, \_ -> _, \$ -> $, \{ \} -> { }, accents \'e \`e \^e \"e \~n \c{c} \v{s} \H{o} \k{a} \={e} \.{z} \u{g} \r{a} \ss \o \O \ae \AE \aa \AA \l \L \i (with or without braces), \, and \  -> space, remaining \command{args} -> args, remaining \command -> "", strip braces, collapse whitespace, trim. Keep $...$ contents without the dollars.
pub fn parse_bbl(text: &str) -> Vec<TruthReference>   // split on \bibitem; parse optional [label] (may contain nested braces/\protect) and {key}; text = latex_to_text(rest); year = first (19|20)\d\d in text; doi from \doi{..}, doi: , https://doi.org/ ; arxiv from arXiv:xxxx or /abs/; title = the \newblock segment after the authors when a \newblock structure exists (natbib/plain styles), else None; authors = first segment split on ", " / " and " (best-effort, before the first \newblock).
pub fn parse_bib(text: &str) -> Vec<TruthReference>   // robust bib parser: @type{key, ...} with brace/quote/number values and # concatenation, @string/@preamble/@comment skipped, fields case-insensitive; title -> latex_to_text; author split on " and "; year; doi; eprint(+archivePrefix arXiv) -> arxiv_id; text = "authors. title. journal/booktitle year".
pub fn parse_cites(tex: &str) -> TruthCitations   // \cite \citep \citet \citealp \citealt \citeauthor \citeyear \citeyearpar \parencite \textcite \autocite \footcite \citenum \cite* variants, each with optional [..] args (up to two) then {keys}; \nocite{keys} / \nocite{*}; ignore commented text; keys trimmed, split on ','.
pub fn body_text(main_tex: &str) -> String   // between \begin{document} and \end{document}; drop figure/table/tabular/algorithm/equation/align/lstlisting/verbatim environments, \bibliography{...}, \maketitle etc.; \section{X}->"X\n"; \cite* commands -> "" ; then latex_to_text; paragraphs separated by "\n\n".
pub fn resolve_inputs(root: &Path, main_tex: &str, depth: u32) -> String   // inline \input{f} / \include{f} (add .tex if missing) relative to root, depth <= 5, missing files left as-is
pub fn ground_truth(files: &crate::corpus::LatexFiles) -> Result<GroundTruth, TruthError>
   // main tex = file containing \begin{document} (NoMainTex if none); merged = resolve_inputs; citations = parse_cites(merged);
   // prefer .bbl (all of them concatenated, method "bbl"); else .bib files filtered to cited/nocite keys (method "bib-cited"), or all entries if \nocite{*} ("bib-all"); NoBibliography if neither. body_text from merged.
Tests: latex_to_text table of 15 cases (accents, dashes, quotes, \emph, \url, \href, math); parse_bbl on a 3-item natbib snippet modelled on scratchpad/arxiv/2108.04588/main.bbl (count, keys, labels, years, a DOI, a title); parse_bib on a 4-entry snippet modelled on custom.bib (braces, quotes, month macro, eprint -> arxiv_id, doi, title with nested braces {LLM}); parse_cites with \citep[see][p. 3]{a,b} \citet{c} \nocite{*} and a commented \cite; ground_truth via a tempdir with main.tex+refs.bib (bib-cited filters uncited entries) and with main.bbl (prefers bbl); body_text drops a figure environment and keeps section titles.

### src/eval.rs  (owner: E3)
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)] pub struct RefMatch { pub truth_key: String, pub extracted_index: Option<u32>, pub method: String /* "doi" | "arxiv" | "title" | "author-year" | "none" */, pub score: f32 }
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)] pub struct PaperEval { pub id: String, pub status: String, pub pages: u32, pub truth_method: String,
   pub truth_refs: u32, pub extracted_refs: u32, pub matched_refs: u32, pub ref_count_exact: bool, pub unmatched_truth_keys: Vec<String>, pub spurious_extracted: Vec<u32>,
   pub doi_truth: u32, pub doi_correct: u32, pub year_truth: u32, pub year_correct: u32, pub title_truth: u32, pub title_correct: u32,
   pub truth_cite_commands: u32, pub extracted_markers: u32, pub resolved_markers: u32, pub marker_targets: u32,
   pub body_alignment: Option<f32>, pub ms_total: f64, pub ms_per_chunk: f64, pub chunks: u32, pub warnings: u32, pub matches: Vec<RefMatch> }
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)] pub struct Summary { pub papers: u32, pub failed: u32, pub ref_count_exact_rate: f32, pub ref_recall: f32, pub ref_precision: f32, pub doi_accuracy: f32, pub year_accuracy: f32, pub title_accuracy: f32, pub marker_resolution_rate: f32, pub mean_body_alignment: Option<f32>, pub p50_ms_per_chunk: f64, pub p95_ms_per_chunk: f64, pub target_ms_per_chunk: f64 /* 30.0 */ }
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)] pub struct CorpusReport { pub generated_unix: i64, pub backend: String, pub host: String, pub papers: Vec<PaperEval>, pub summary: Summary }
pub fn normalize_title(s: &str) -> String   // NFKD-free: lowercase, keep alphanumerics (unicode letters/digits) and single spaces
pub fn word_alignment(a: &str, b: &str) -> f32   // tokens = lowercase alnum words; LCS length via DP with O(min) memory; cap each side to 12_000 tokens (take evenly spaced sample beyond); 2*lcs/(n+m); 0 when both empty? -> 1.0 if both empty, 0.0 if one empty
pub fn match_references(truth: &[crate::latex_refs::TruthReference], extracted: &[crate::schema::ReferenceEntry]) -> Vec<RefMatch>
   // greedy one-to-one, in priority: DOI equal (case-insensitive) -> arxiv_id equal (ignore version) -> normalized title equal or Jaccard of title words >= 0.8 -> first-author surname (lowercase, ascii-folded) + year equal. Each extracted index used at most once.
pub fn evaluate(id: &str, result: &crate::schema::ExtractionResult, truth: &crate::latex_refs::GroundTruth) -> PaperEval
   // doi_correct counts matched pairs where truth doi is Some and extracted doi equal; likewise year/title (title via normalize_title equality); marker stats from result.citations; body_alignment = word_alignment(all page texts before the reference section?, truth.body_text) -> simply all page text joined vs body_text when body_text non-empty; ms_per_chunk = sum of stage timings / max(1, chunks)
pub fn failed_paper(id: &str, error: &str) -> PaperEval   // status "failed:<error>", zeros
pub fn summarize(papers: &[PaperEval]) -> Summary   // rates over non-failed papers; recall = sum matched / sum truth_refs; precision = sum matched / sum extracted; percentiles over ms_per_chunk (nearest-rank)
pub fn build_report(backend: &str, host: &str, papers: Vec<PaperEval>) -> CorpusReport
pub fn render_markdown(report: &CorpusReport) -> String   // summary table then per-paper table (id, status, pages, truth/extracted/matched refs, count-exact ✓/✗, doi c/t, year c/t, markers resolved/extracted, align, ms/chunk, warnings), then "Unmatched truth keys" list capped 10 per paper
Tests: normalize_title; word_alignment identical -> 1.0, disjoint -> 0.0, half overlap ~0.5..0.67; match_references by doi/arxiv/title/author-year and one-to-one; evaluate on a hand-built result+truth (3 truth, 3 extracted, 2 matched) yields exact counts; summarize percentiles; render_markdown contains the header and a row per paper.

### Integration owner (E4): src/main.rs (append subcommands), src/lib.rs (register 3 modules), Cargo.toml (3 deps), corpus/manifest.json, .github/workflows/eval.yml, docs/EVAL.md, tests/eval_cli.rs
tpe corpus fetch --manifest corpus/manifest.json --cache <DIR> [--offline] [--update-manifest] [--split all|dev|holdout]   // fetch each item, print one line per item (id, pdf sha[..12], source? yes/no, cached/downloaded), update hashes when asked and save manifest; nonzero exit if any item failed
tpe eval --manifest corpus/manifest.json --cache <DIR> --out <DIR> [--backend lopdf] [--split dev|holdout|all] [--offline] [--db <FILE>]
   // for each item: fetch (cached), run_job(Job{path: pdf, backend, ..}), ground_truth(find_latex_files(source_dir)) -> evaluate; on any error -> failed_paper; write <out>/report.json + <out>/report.md; optional ledger write when --db given; print the summary lines; exit 0 even when metrics are poor (this is a measurement), nonzero only on I/O failures
corpus/manifest.json: version 1, 30 items from scratchpad/ccby.txt choosing diverse categories (cs.CL, cs.LG, cs.CV, math.*, physics.*, stat.*, eess.*, cs.SE, cs.CG…), kind "arxiv", license "http://creativecommons.org/licenses/by/4.0/", pdf_url "https://arxiv.org/pdf/<id>", source_url "https://arxiv.org/e-print/<id>", sha256 null (filled by `tpe corpus fetch --update-manifest` in CI), split: first 20 "dev", last 10 "holdout", notes null. Include 2108.04588, 2502.00857, 2412.06210 in dev.
.github/workflows/eval.yml: name Eval; on workflow_dispatch (inputs: split default dev) and schedule daily 09:00 UTC; permissions contents: read; jobs eval with matrix os [ubuntu-24.04-arm, macos-15]; steps: checkout (pin 3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1, persist-credentials false), rustup component add clippy rustfmt, actions/cache (pin 0057852bfaa89a56745cba8c7296529d2fc39830 # v4.3.0) for cargo and for `.corpus-cache` keyed on hashFiles('corpus/manifest.json'), `cargo build --release`, `./target/release/tpe corpus fetch --manifest corpus/manifest.json --cache .corpus-cache --split ${{ inputs.split || 'dev' }}`, `./target/release/tpe eval ... --out eval-out --split ...`, `cat eval-out/report.md >> "$GITHUB_STEP_SUMMARY"`, actions/upload-artifact (pin: use `actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02 # v4.6.2`) name eval-${{ matrix.os }} path eval-out. Never make it part of the required `ci` job.
docs/EVAL.md: what is measured, why arXiv .bbl/.bib is independent-but-imperfect truth, the exact-count rule, how to run locally and in CI, and that these are diagnostics not the acceptance protocol.
tests/eval_cli.rs: `tpe eval --offline` against an empty cache and a manifest with one item -> exits 0 and writes report.json with 1 failed paper (use std::process::Command with env!("CARGO_BIN_EXE_tpe")); `tpe corpus fetch --offline` on the same -> nonzero exit.

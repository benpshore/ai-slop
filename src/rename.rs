//! `tpe rename`: rename PDFs from their bibliographic metadata, Zotero style.
//!
//! Metadata comes from the engine's own extraction of the first pages. With
//! `--online`, a DOI the engine found is looked up at Crossref and the Crossref
//! record wins field by field (the engine fills what Crossref lacks); Crossref
//! does not index arXiv DOIs (`10.48550/arXiv.*` answers 404, checked
//! 2026-09-30), so an arXiv-only PDF is always named from the engine's metadata.
//!
//! Safety rules, in the order they matter:
//! - nothing is renamed unless the caller asks (`--apply`); the default is a plan;
//! - a name that exists in the directory, ignoring case and Unicode normalisation,
//!   is never reused: the new name gets ` (2)`, ` (3)`, ... and the final step is a hard
//!   link, which fails instead of replacing anything, then the old name is removed;
//! - only regular files are renamed, never a symlink, and only inside their own
//!   directory (the new name is a bare file name);
//! - a journal listing every planned `from -> to` (with the file's SHA-256) is written
//!   before the first rename and never overwritten, so `--undo` can reverse exactly what
//!   happened and refuses a file that has changed since.
//!
//! The hard-link step needs a file system with hard links (not FAT or exFAT); on one
//! without, the rename fails and nothing is changed rather than falling back to an
//! overwriting rename.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tpe_biblio::filename::{FilenameOptions, fold_key, suggest_filename, unique_name};
use tpe_biblio::{BiblioError, PaperRecord, merge_records};
use tpe_common::{normalize_arxiv_id, normalize_doi};

use crate::acquire;
use crate::pipeline;
use crate::schema::{Job, Metadata};

/// Pages read for metadata. Title, authors, year and DOI are on the first pages.
const METADATA_PAGES: (u32, u32) = (1, 3);
/// Larger inputs are skipped rather than read into memory.
const MAX_BYTES: u64 = 1 << 30;
/// Journal format version.
const JOURNAL_VERSION: u32 = 1;

/// Naming metadata for one file and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub record: PaperRecord,
    /// `"crossref"` or `"engine"`.
    pub source: &'static str,
    /// Why a lookup was not used, when it was asked for.
    pub note: Option<String>,
    /// SHA-256 of the file's bytes as extracted.
    pub sha256: String,
}

/// One input file with its metadata, or the reason there is none.
#[derive(Debug)]
pub struct Candidate {
    pub path: PathBuf,
    pub resolved: Result<Resolved, String>,
}

/// What to do with one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Rename {
        to: String,
    },
    /// The file already has the suggested name.
    Unchanged,
    Skip(String),
}

/// One line of a plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planned {
    pub path: PathBuf,
    pub action: Action,
    pub source: &'static str,
    pub note: Option<String>,
    pub sha256: String,
}

/// The engine's record for a PDF: extraction of its first pages, nothing fetched.
pub fn engine_record(path: &Path) -> Result<(PaperRecord, String), String> {
    let job = Job {
        path: path.to_string_lossy().into_owned(),
        backend: "lopdf".to_string(),
        pages: Some(METADATA_PAGES),
        password: None,
        max_bytes: Some(MAX_BYTES),
        figures_dir: None,
    };
    let result = pipeline::run_job(&job).map_err(|e| e.to_string())?;
    Ok((to_record(&result.metadata), result.document.hash.0))
}

/// The engine's metadata as the shared record type; identifiers are normalised
/// and dropped when they do not have the identifier shape.
pub fn to_record(meta: &Metadata) -> PaperRecord {
    PaperRecord {
        title: meta.title.clone().unwrap_or_default(),
        authors: meta.authors.iter().map(|a| a.name.clone()).collect(),
        year: meta.year,
        venue: meta.venue.clone(),
        doi: meta.doi.as_deref().and_then(normalize_doi),
        arxiv_id: meta.arxiv_id.as_deref().and_then(normalize_arxiv_id),
        source: "engine".to_string(),
        ..PaperRecord::default()
    }
}

/// Prefer the Crossref record for the engine's DOI. `lookup` performs the request
/// (a test passes a recorded response); any failure falls back to the engine's
/// record and is reported in `note`.
pub fn refine_online(
    engine: PaperRecord,
    lookup: impl FnOnce(&str) -> Result<Option<PaperRecord>, BiblioError>,
) -> (PaperRecord, &'static str, Option<String>) {
    // Crossref has no arXiv DOIs (10.48550/arXiv.*); do not spend a request on one.
    let doi = engine
        .doi
        .clone()
        .filter(|d| !d.starts_with("10.48550/arxiv."));
    let Some(doi) = doi else {
        let note = (engine.arxiv_id.is_some() || engine.doi.is_some()).then(|| {
            "arXiv id only; Crossref does not index arXiv DOIs, used the PDF's metadata".to_string()
        });
        return (engine, "engine", note);
    };
    match lookup(&doi) {
        Ok(Some(found)) => {
            let merged = merge_records(vec![found, engine]).into_iter().next();
            (merged.unwrap_or_default(), "crossref", None)
        }
        Ok(None) => (
            engine,
            "engine",
            Some(format!("DOI {doi} not found at Crossref")),
        ),
        Err(e) => (
            engine,
            "engine",
            Some(format!("Crossref lookup failed: {e}")),
        ),
    }
}

/// Extract (and, with a client, look up) the metadata of every path. Symlinks
/// and non-files become candidates with an error; nothing is renamed here.
pub fn collect(paths: &[PathBuf], client: Option<&tpe_biblio::Client>) -> Vec<Candidate> {
    paths
        .iter()
        .map(|path| {
            let resolved = match refuse(path) {
                Some(reason) => Err(reason),
                None => engine_record(path).map(|(engine, sha256)| {
                    let (record, source, note) = match client {
                        Some(client) => refine_online(engine, |doi| {
                            tpe_biblio::crossref::fetch_by_doi(client, doi)
                                .map(|found| found.map(|f| f.record))
                        }),
                        None => (engine, "engine", None),
                    };
                    Resolved {
                        record,
                        source,
                        note,
                        sha256,
                    }
                }),
            };
            Candidate {
                path: path.clone(),
                resolved,
            }
        })
        .collect()
}

/// Order and de-duplicate `paths`, and describe each file's action against the
/// current contents of its directory. Reads directories, never files.
pub fn plan(mut candidates: Vec<Candidate>, options: &FilenameOptions) -> Vec<Planned> {
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    candidates.dedup_by(|a, b| a.path == b.path);
    let mut listings: HashMap<PathBuf, Result<HashSet<String>, String>> = HashMap::new();
    candidates
        .into_iter()
        .map(|candidate| {
            let Candidate { path, resolved } = candidate;
            let skip = |reason: String, source, note, sha256| Planned {
                path: path.clone(),
                action: Action::Skip(reason),
                source,
                note,
                sha256,
            };
            let resolved = match resolved {
                Ok(r) => r,
                Err(reason) => return skip(reason, "", None, String::new()),
            };
            let Resolved {
                record,
                source,
                note,
                sha256,
            } = resolved;
            let dir = parent_of(&path);
            let listing = listings
                .entry(dir.clone())
                .or_insert_with(|| list_folded(&dir));
            let names = match listing {
                Ok(names) => names,
                Err(e) => return skip(e.clone(), source, note, sha256),
            };
            if record.title.trim().is_empty() {
                return skip("no title found".to_string(), source, note, sha256);
            }
            let lost = |s: &String| s.contains('\u{fffd}');
            if lost(&record.title) || record.authors.iter().take(2).any(lost) {
                let reason = "extracted text has lost characters (U+FFFD)".to_string();
                return skip(reason, source, note, sha256);
            }
            let Some(own) = path.file_name().and_then(|n| n.to_str()) else {
                return skip("file name is not UTF-8".to_string(), source, note, sha256);
            };
            let own_key = fold_key(own);
            let wanted = suggest_filename(&record, options);
            let free = |candidate: &str| {
                let key = fold_key(candidate);
                key != own_key && names.contains(&key)
            };
            let Some(to) = unique_name(&wanted, free) else {
                return skip("no free name".to_string(), source, note, sha256);
            };
            let action = if to == own {
                Action::Unchanged
            } else if fold_key(&to) == own_key {
                return skip(
                    "new name differs only in case or normalisation".to_string(),
                    source,
                    note,
                    sha256,
                );
            } else {
                names.insert(fold_key(&to));
                Action::Rename { to }
            };
            Planned {
                path,
                action,
                source,
                note,
                sha256,
            }
        })
        .collect()
}

fn parent_of(path: &Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn list_folded(dir: &Path) -> Result<HashSet<String>, String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
    Ok(entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .map(|n| fold_key(&n))
        .collect())
}

/// Why `path` may not be renamed, if so: only regular files, never symlinks.
pub fn refuse(path: &Path) -> Option<String> {
    match fs::symlink_metadata(path) {
        Err(e) => Some(format!("cannot read: {e}")),
        Ok(m) if m.file_type().is_symlink() => Some("is a symlink; not followed".to_string()),
        Ok(m) if !m.is_file() => Some("not a regular file".to_string()),
        Ok(_) => None,
    }
}

/// Human-readable lines for one plan entry.
pub fn render(item: &Planned) -> String {
    let old = item.path.display();
    let note = item
        .note
        .as_ref()
        .map_or_else(String::new, |n| format!("\n      note: {n}"));
    match &item.action {
        Action::Rename { to } => {
            format!("rename  {old}\n    ->  {to}  [{}]{note}", item.source)
        }
        Action::Unchanged => format!("same    {old}  [{}]{note}", item.source),
        Action::Skip(reason) => format!("skip    {old}  ({reason}){note}"),
    }
}

/// The undo journal: intended renames, written before the first one happens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub version: u32,
    pub entries: Vec<Entry>,
}

/// One `from -> to` rename inside `dir` (absolute); both are bare file names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub dir: PathBuf,
    pub from: String,
    pub to: String,
    pub sha256: String,
}

/// The journal for the renames in `plan`, or `None` when there are none.
pub fn journal_for(plan: &[Planned]) -> io::Result<Option<Journal>> {
    let mut entries = Vec::new();
    for item in plan {
        let Action::Rename { to } = &item.action else {
            continue;
        };
        let dir = fs::canonicalize(parent_of(&item.path))?;
        let from = item
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| io::Error::other("file name is not UTF-8"))?;
        entries.push(Entry {
            dir,
            from: from.to_string(),
            to: to.clone(),
            sha256: item.sha256.clone(),
        });
    }
    Ok((!entries.is_empty()).then_some(Journal {
        version: JOURNAL_VERSION,
        entries,
    }))
}

/// Write `journal` to a new file; an existing file is an error, never replaced.
pub fn write_journal(path: &Path, journal: &Journal) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let json = serde_json::to_string_pretty(journal).map_err(io::Error::other)?;
    file.write_all(json.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
}

/// Read a journal and check that every name in it is a bare file name in an absolute directory.
pub fn read_journal(path: &Path) -> Result<Journal, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("cannot read journal: {e}"))?;
    let journal: Journal =
        serde_json::from_str(&text).map_err(|e| format!("invalid journal: {e}"))?;
    if journal.version != JOURNAL_VERSION {
        return Err(format!("unsupported journal version {}", journal.version));
    }
    for e in &journal.entries {
        if !e.dir.is_absolute() || !is_bare_name(&e.from) || !is_bare_name(&e.to) {
            return Err(format!("journal entry is not a bare file name: {e:?}"));
        }
    }
    Ok(journal)
}

fn is_bare_name(name: &str) -> bool {
    !matches!(name, "" | "." | "..") && Path::new(name).file_name() == Some(name.as_ref())
}

/// Rename `from` to `to` in `dir` without ever replacing `to`: hard-link the new
/// name (fails if it exists), then remove the old one. Refuses a symlink.
pub fn rename_no_clobber(dir: &Path, from: &str, to: &str) -> io::Result<()> {
    let (src, dst) = (dir.join(from), dir.join(to));
    if let Some(reason) = refuse(&src) {
        return Err(io::Error::other(reason));
    }
    fs::hard_link(&src, &dst)?;
    if let Err(e) = fs::remove_file(&src) {
        let _ = fs::remove_file(&dst);
        return Err(e);
    }
    Ok(())
}

/// Perform every rename in `journal` (already written to disk). One result per entry.
pub fn apply(journal: &Journal) -> Vec<io::Result<()>> {
    journal
        .entries
        .iter()
        .map(|e| rename_no_clobber(&e.dir, &e.from, &e.to))
        .collect()
}

/// What `--undo` would do with one journal entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UndoAction {
    Restore,
    /// Nothing to reverse: never renamed, or already restored.
    Nothing,
    Skip(String),
}

/// Decide what to do with `entry`: restore only a regular file at the new name
/// whose content is the recorded one, and only if the old name is free.
pub fn plan_undo(entry: &Entry) -> UndoAction {
    let (from, to) = (entry.dir.join(&entry.from), entry.dir.join(&entry.to));
    let from_exists = fs::symlink_metadata(&from).is_ok();
    if fs::symlink_metadata(&to).is_err_and(|e| e.kind() == io::ErrorKind::NotFound) {
        return if from_exists {
            UndoAction::Nothing
        } else {
            UndoAction::Skip(format!("{} is missing", entry.to))
        };
    }
    if let Some(reason) = refuse(&to) {
        return UndoAction::Skip(format!("{}: {reason}", entry.to));
    }
    if from_exists {
        return UndoAction::Skip(format!("{} already exists", entry.from));
    }
    match acquire::snapshot(&to, None) {
        Ok(s) if s.hash.0 == entry.sha256 => UndoAction::Restore,
        Ok(_) => UndoAction::Skip(format!("{} has changed since it was renamed", entry.to)),
        Err(e) => UndoAction::Skip(format!("{}: {e}", entry.to)),
    }
}

/// Reverse one journal entry, or explain why not. Reverse order for the whole journal.
pub fn undo_entry(entry: &Entry) -> Result<UndoAction, String> {
    let action = plan_undo(entry);
    if action == UndoAction::Restore {
        rename_no_clobber(&entry.dir, &entry.to, &entry.from).map_err(|e| e.to_string())?;
    }
    Ok(action)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn record(title: &str) -> PaperRecord {
        PaperRecord {
            title: title.to_string(),
            authors: vec!["Yann LeCun".to_string()],
            year: Some(2015),
            source: "engine".to_string(),
            ..PaperRecord::default()
        }
    }

    fn candidate(path: PathBuf, title: &str, sha: &str) -> Candidate {
        Candidate {
            path,
            resolved: Ok(Resolved {
                record: record(title),
                source: "engine",
                note: None,
                sha256: sha.to_string(),
            }),
        }
    }

    fn touch(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    fn to_of(item: &Planned) -> Option<&str> {
        match &item.action {
            Action::Rename { to } => Some(to),
            _ => None,
        }
    }

    // A trimmed capture of https://api.crossref.org/works/10.1038/nature14539
    // (fetched 2026-09-30): only the fields the parser reads, authors cut to three.
    const CROSSREF_NATURE: &str = r#"{"status":"ok","message-type":"work","message-version":"1.0.0","message":{
        "DOI":"10.1038/nature14539","URL":"https://doi.org/10.1038/nature14539",
        "title":["Deep learning"],"container-title":["Nature"],
        "author":[{"given":"Yann","family":"LeCun","sequence":"first","affiliation":[]},
                  {"given":"Yoshua","family":"Bengio","sequence":"additional","affiliation":[]},
                  {"given":"Geoffrey","family":"Hinton","sequence":"additional","affiliation":[]}],
        "issued":{"date-parts":[[2015,5,27]]}}}"#;

    fn crossref(doi: &str) -> Result<Option<PaperRecord>, BiblioError> {
        assert_eq!(doi, "10.1038/nature14539");
        Ok(tpe_biblio::crossref::parse_crossref(CROSSREF_NATURE)?
            .into_iter()
            .next())
    }

    #[test]
    fn crossref_record_wins_and_engine_fills_gaps() {
        let engine = PaperRecord {
            title: "DEEP LEARNING (as printed)".to_string(),
            authors: vec!["Y. LeCun".to_string()],
            doi: Some("10.1038/nature14539".to_string()),
            venue: None,
            source: "engine".to_string(),
            year: Some(2014),
            ..PaperRecord::default()
        };
        let (record, source, note) = refine_online(engine, crossref);
        assert_eq!(source, "crossref");
        assert_eq!(note, None);
        assert_eq!(record.title, "Deep learning");
        assert_eq!(record.year, Some(2015));
        assert_eq!(record.authors.len(), 3);
        let name = suggest_filename(&record, &FilenameOptions::default());
        assert_eq!(name, "LeCun et al. - 2015 - Deep learning.pdf");

        // Crossref lacks a title: the engine's fills it in.
        let sparse = |_: &str| {
            Ok(Some(PaperRecord {
                title: String::new(),
                year: Some(2015),
                doi: Some("10.1038/nature14539".to_string()),
                source: "crossref".to_string(),
                ..PaperRecord::default()
            }))
        };
        let engine = PaperRecord {
            title: "Printed title".to_string(),
            doi: Some("10.1038/nature14539".to_string()),
            source: "engine".to_string(),
            ..PaperRecord::default()
        };
        let (record, source, _) = refine_online(engine, sparse);
        assert_eq!(
            (record.title.as_str(), source),
            ("Printed title", "crossref")
        );
        assert_eq!(record.year, Some(2015));
    }

    #[test]
    fn lookup_failures_fall_back_to_the_engine_with_a_note() {
        let engine = |doi: Option<&str>, arxiv: Option<&str>| PaperRecord {
            title: "T".to_string(),
            doi: doi.map(str::to_string),
            arxiv_id: arxiv.map(str::to_string),
            source: "engine".to_string(),
            ..PaperRecord::default()
        };
        let (r, source, note) =
            refine_online(engine(Some("10.1/x"), None), |_| Ok::<_, BiblioError>(None));
        assert_eq!((r.title.as_str(), source), ("T", "engine"));
        assert!(note.unwrap().contains("not found at Crossref"));

        let (_, source, note) =
            refine_online(engine(Some("10.1/x"), None), |_| Err(BiblioError::Offline));
        assert_eq!(source, "engine");
        assert!(note.unwrap().contains("offline"));

        // An arXiv DOI is not looked up either.
        let (_, source, note) =
            refine_online(engine(Some("10.48550/arxiv.1706.03762"), None), |_| {
                unreachable!("no lookup")
            });
        assert_eq!(source, "engine");
        assert!(note.unwrap().contains("arXiv"));
        // No DOI: no lookup at all (the closure would panic).
        let (_, source, note) = refine_online(engine(None, Some("1706.03762")), |_| {
            unreachable!("no lookup")
        });
        assert_eq!(source, "engine");
        assert!(note.unwrap().contains("arXiv"));
        let (_, _, note) = refine_online(engine(None, None), |_| unreachable!("no lookup"));
        assert_eq!(note, None);
    }

    #[test]
    fn engine_metadata_maps_to_a_record() {
        let meta = Metadata {
            title: Some("A title".to_string()),
            authors: vec![crate::schema::Author {
                name: "Ada Lovelace".to_string(),
                ..crate::schema::Author::default()
            }],
            doi: Some("https://doi.org/10.1000/ABC.".to_string()),
            arxiv_id: Some("arXiv:1706.03762v5".to_string()),
            year: Some(2017),
            ..Metadata::default()
        };
        let r = to_record(&meta);
        assert_eq!(r.doi.as_deref(), Some("10.1000/abc"));
        assert_eq!(r.arxiv_id.as_deref(), Some("1706.03762"));
        assert_eq!(r.authors, ["Ada Lovelace"]);
        let bad = Metadata {
            doi: Some("not a doi".to_string()),
            ..Metadata::default()
        };
        assert_eq!(to_record(&bad).doi, None);
    }

    #[test]
    fn plan_names_collisions_deterministically() {
        let dir = TempDir::new().unwrap();
        let a = touch(dir.path(), "b.pdf", "1");
        let b = touch(dir.path(), "a.pdf", "2");
        let c = touch(dir.path(), "c.pdf", "3");
        // Already occupied by an unrelated file, differing only in case.
        touch(dir.path(), "lecun - 2015 - deep learning.pdf", "x");
        let make = || {
            vec![
                candidate(a.clone(), "Deep learning", "h1"),
                candidate(b.clone(), "Deep learning", "h2"),
                candidate(c.clone(), "Deep learning", "h3"),
                candidate(a.clone(), "Deep learning", "h1"), // duplicate path
            ]
        };
        let planned = plan(make(), &FilenameOptions::default());
        assert_eq!(planned.len(), 3);
        // Sorted by path: a.pdf, b.pdf, c.pdf.
        assert_eq!(
            to_of(&planned[0]),
            Some("LeCun - 2015 - Deep learning (2).pdf")
        );
        assert_eq!(
            to_of(&planned[1]),
            Some("LeCun - 2015 - Deep learning (3).pdf")
        );
        assert_eq!(
            to_of(&planned[2]),
            Some("LeCun - 2015 - Deep learning (4).pdf")
        );
        assert_eq!(planned, plan(make(), &FilenameOptions::default()));
    }

    #[test]
    fn plan_is_idempotent_and_keeps_existing_suffixes() {
        let dir = TempDir::new().unwrap();
        let first = touch(dir.path(), "LeCun - 2015 - Deep learning.pdf", "1");
        let second = touch(dir.path(), "LeCun - 2015 - Deep learning (2).pdf", "2");
        let planned = plan(
            vec![
                candidate(first, "Deep learning", "h1"),
                candidate(second, "Deep learning", "h2"),
            ],
            &FilenameOptions::default(),
        );
        assert!(
            planned.iter().all(|p| p.action == Action::Unchanged),
            "{planned:?}"
        );
    }

    #[test]
    fn plan_skips_what_it_cannot_name_safely() {
        let dir = TempDir::new().unwrap();
        let untitled = touch(dir.path(), "u.pdf", "1");
        let broken = touch(dir.path(), "broken.pdf", "2");
        let case_only = touch(dir.path(), "lecun - 2015 - deep learning.pdf", "3");
        let lost = touch(dir.path(), "lost.pdf", "4");
        let planned = plan(
            vec![
                candidate(untitled, "  ", "h"),
                Candidate {
                    path: broken,
                    resolved: Err("encrypted".to_string()),
                },
                candidate(case_only, "Deep learning", "h"),
                candidate(lost, "Quality\u{fffd} control", "h"),
            ],
            &FilenameOptions::default(),
        );
        let reasons: Vec<String> = planned
            .iter()
            .map(|p| match &p.action {
                Action::Skip(r) => r.clone(),
                other => format!("{other:?}"),
            })
            .collect();
        // Sorted by path: broken, lecun..., lost, u.
        assert_eq!(reasons[0], "encrypted");
        assert!(reasons[1].contains("differs only in case"), "{reasons:?}");
        assert!(reasons[2].contains("lost characters"), "{reasons:?}");
        assert_eq!(reasons[3], "no title found");
        let missing = plan(
            vec![candidate(dir.path().join("nodir/x.pdf"), "T", "h")],
            &FilenameOptions::default(),
        );
        assert!(matches!(&missing[0].action, Action::Skip(r) if r.contains("cannot list")));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_directories_are_refused() {
        let dir = TempDir::new().unwrap();
        let real = touch(dir.path(), "real.pdf", "1");
        let link = dir.path().join("link.pdf");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(refuse(&link).unwrap().contains("symlink"));
        assert!(refuse(dir.path()).unwrap().contains("not a regular file"));
        assert!(refuse(&real).is_none());
        assert!(refuse(&dir.path().join("missing.pdf")).is_some());
        // The rename primitive refuses too, and changes nothing.
        assert!(rename_no_clobber(dir.path(), "link.pdf", "new.pdf").is_err());
        assert!(!dir.path().join("new.pdf").exists());
    }

    #[test]
    fn rename_never_overwrites() {
        let dir = TempDir::new().unwrap();
        touch(dir.path(), "old.pdf", "old");
        touch(dir.path(), "taken.pdf", "precious");
        let err = rename_no_clobber(dir.path(), "old.pdf", "taken.pdf").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(dir.path().join("taken.pdf")).unwrap(),
            "precious"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("old.pdf")).unwrap(),
            "old"
        );
        rename_no_clobber(dir.path(), "old.pdf", "new.pdf").unwrap();
        assert!(!dir.path().join("old.pdf").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("new.pdf")).unwrap(),
            "old"
        );
    }

    #[test]
    fn journal_round_trips_and_is_never_overwritten() {
        let dir = TempDir::new().unwrap();
        let file = touch(dir.path(), "a.pdf", "1");
        let planned = plan(
            vec![candidate(file, "Deep learning", "abc")],
            &FilenameOptions::default(),
        );
        let journal = journal_for(&planned).unwrap().unwrap();
        assert_eq!(journal.entries.len(), 1);
        assert!(journal.entries[0].dir.is_absolute());
        let path = dir.path().join("journal.json");
        write_journal(&path, &journal).unwrap();
        assert_eq!(read_journal(&path).unwrap(), journal);
        let again = write_journal(&path, &journal).unwrap_err();
        assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(journal_for(&[]).unwrap(), None);
    }

    #[test]
    fn journal_with_path_tricks_is_refused() {
        let dir = TempDir::new().unwrap();
        for (from, to) in [
            ("../x.pdf", "y.pdf"),
            ("x.pdf", "sub/y.pdf"),
            ("x.pdf", ".."),
        ] {
            let journal = Journal {
                version: JOURNAL_VERSION,
                entries: vec![Entry {
                    dir: dir.path().to_path_buf(),
                    from: from.to_string(),
                    to: to.to_string(),
                    sha256: String::new(),
                }],
            };
            let path = dir.path().join("j.json");
            fs::write(&path, serde_json::to_string(&journal).unwrap()).unwrap();
            assert!(read_journal(&path).is_err(), "{from} {to}");
        }
        let path = dir.path().join("v.json");
        fs::write(&path, r#"{"version":9,"entries":[]}"#).unwrap();
        assert!(read_journal(&path).unwrap_err().contains("version"));
        fs::write(&path, "not json").unwrap();
        assert!(read_journal(&path).is_err());
    }

    #[test]
    fn apply_then_undo_restores_exactly() {
        let dir = TempDir::new().unwrap();
        let a = touch(dir.path(), "a.pdf", "content-a");
        let b = touch(dir.path(), "b.pdf", "content-b");
        let sha = |s: &str| crate::schema::sha256_hex(s.as_bytes());
        let planned = plan(
            vec![
                candidate(a, "Deep learning", &sha("content-a")),
                candidate(b, "Deep learning", &sha("content-b")),
            ],
            &FilenameOptions::default(),
        );
        let journal = journal_for(&planned).unwrap().unwrap();
        assert!(apply(&journal).iter().all(Result::is_ok));
        let names = |dir: &Path| {
            let mut n: Vec<String> = fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            n.sort();
            n
        };
        assert_eq!(
            names(dir.path()),
            [
                "LeCun - 2015 - Deep learning (2).pdf",
                "LeCun - 2015 - Deep learning.pdf"
            ]
        );
        for entry in journal.entries.iter().rev() {
            assert_eq!(undo_entry(entry), Ok(UndoAction::Restore));
        }
        assert_eq!(names(dir.path()), ["a.pdf", "b.pdf"]);
        assert_eq!(
            fs::read_to_string(dir.path().join("a.pdf")).unwrap(),
            "content-a"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("b.pdf")).unwrap(),
            "content-b"
        );
        // A second undo finds nothing to do.
        for entry in &journal.entries {
            assert_eq!(undo_entry(entry), Ok(UndoAction::Nothing));
        }
    }

    #[test]
    fn undo_refuses_changed_files_and_occupied_names() {
        let dir = TempDir::new().unwrap();
        let entry = |sha: &str| Entry {
            dir: dir.path().to_path_buf(),
            from: "a.pdf".to_string(),
            to: "New.pdf".to_string(),
            sha256: sha.to_string(),
        };
        // Neither name exists.
        assert!(matches!(plan_undo(&entry("x")), UndoAction::Skip(r) if r.contains("missing")));
        touch(dir.path(), "New.pdf", "edited");
        // The renamed file's content is not the recorded one.
        let recorded = crate::schema::sha256_hex(b"original");
        assert!(
            matches!(plan_undo(&entry(&recorded)), UndoAction::Skip(r) if r.contains("changed"))
        );
        // Content matches but the old name is taken by someone else.
        touch(dir.path(), "a.pdf", "someone else");
        let edited = crate::schema::sha256_hex(b"edited");
        assert!(
            matches!(plan_undo(&entry(&edited)), UndoAction::Skip(r) if r.contains("already exists"))
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("a.pdf")).unwrap(),
            "someone else"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("New.pdf")).unwrap(),
            "edited"
        );
    }

    #[test]
    fn render_is_readable() {
        let item = Planned {
            path: PathBuf::from("dl/paper.pdf"),
            action: Action::Rename {
                to: "LeCun - 2015 - Deep learning.pdf".to_string(),
            },
            source: "crossref",
            note: None,
            sha256: String::new(),
        };
        assert_eq!(
            render(&item),
            "rename  dl/paper.pdf\n    ->  LeCun - 2015 - Deep learning.pdf  [crossref]"
        );
        let skipped = Planned {
            action: Action::Skip("no title found".to_string()),
            note: Some("n".to_string()),
            ..item
        };
        assert_eq!(
            render(&skipped),
            "skip    dl/paper.pdf  (no title found)\n      note: n"
        );
    }
}

//! Evaluation corpus: a JSON manifest of `arXiv` papers, a local download
//! cache and the unpacking of e-print source bundles.
//!
//! An e-print is either gzip of a tar archive (multi-file sources) or gzip
//! of a single `.tex` file; for some papers `arXiv` serves the PDF itself,
//! which means there is no TeX source. Bundles are small, so they are
//! decompressed fully into memory before the tar/single-file decision.

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::schema::sha256_hex;

/// Upper bound on the total unpacked size of one source bundle.
const MAX_UNPACKED_BYTES: u64 = 200 * 1024 * 1024;

/// Upper bound on one downloaded response body.
const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// Per-phase network timeout.
const TIMEOUT: Duration = Duration::from_secs(60);

/// Pause before the single retry of a failed download.
const RETRY_DELAY: Duration = Duration::from_secs(2);

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const TAR_BLOCK: usize = 512;
const TAR_MAGIC_OFFSET: usize = 257;
const TAR_CKSUM_RANGE: std::ops::Range<usize> = 148..156;

/// Why a manifest could not be read or an item could not be fetched.
#[derive(Debug, Error)]
pub enum CorpusError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("http: {0}")]
    Http(String),
    #[error("sha256 mismatch for {id}: expected {expected}, got {actual}")]
    HashMismatch {
        id: String,
        expected: String,
        actual: String,
    },
    #[error("no TeX source: {0}")]
    NoSource(String),
    #[error("unpack: {0}")]
    Unpack(String),
}

/// The list of papers the evaluation runs over.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub version: u32,
    pub items: Vec<ManifestItem>,
}

/// One paper: where its PDF and source live and what their hashes should be.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ManifestItem {
    /// e.g. `arxiv:2502.00857`.
    pub id: String,
    /// e.g. `arxiv`.
    pub kind: String,
    /// License URL.
    pub license: String,
    pub pdf_url: String,
    pub source_url: Option<String>,
    pub pdf_sha256: Option<String>,
    pub source_sha256: Option<String>,
    pub categories: Vec<String>,
    /// `dev` or `holdout`.
    pub split: String,
    pub notes: Option<String>,
}

/// Where a fetched item's files ended up and what they hash to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetched {
    pub pdf_path: PathBuf,
    /// Directory with the unpacked source; `None` when the item has no
    /// source URL or the e-print turned out to be a PDF (no TeX source).
    pub source_dir: Option<PathBuf>,
    pub pdf_sha256: String,
    /// Hash of the source archive as downloaded, when there was one.
    pub source_sha256: Option<String>,
}

/// The TeX-related files found under an unpacked source directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LatexFiles {
    pub root: PathBuf,
    pub tex: Vec<PathBuf>,
    pub bbl: Vec<PathBuf>,
    pub bib: Vec<PathBuf>,
}

/// Read and parse a manifest file.
pub fn load_manifest(path: &Path) -> Result<Manifest, CorpusError> {
    let bytes = fs::read(path)?;
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    Ok(manifest)
}

/// Write a manifest as pretty JSON with a trailing newline.
pub fn save_manifest(path: &Path, manifest: &Manifest) -> Result<(), CorpusError> {
    let mut text = serde_json::to_string_pretty(manifest)?;
    text.push('\n');
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, text)?;
    Ok(())
}

/// Cache locations for an item: `cache_dir/<cache_dir_name(id)>/`
/// `paper.pdf`, `source.bin` (the archive as downloaded) and `source/`.
pub fn cache_paths(cache_dir: &Path, item: &ManifestItem) -> (PathBuf, PathBuf, PathBuf) {
    let dir = cache_dir.join(cache_dir_name(&item.id));
    (
        dir.join("paper.pdf"),
        dir.join("source.bin"),
        dir.join("source"),
    )
}

/// A single safe directory name derived from a manifest id.
///
/// Characters outside `[A-Za-z0-9._-]` (including `:`) become `_`, and runs
/// of `_` collapse to one, so `arxiv:2502.00857` is `arxiv_2502.00857`. An id
/// that contains `/`, `\` or `..`, or whose cleaned form is empty, `.` or
/// `..`, is replaced by `id-<first 16 hex digits of its sha256>`. The result
/// never contains a path separator and never names a parent directory.
pub fn cache_dir_name(id: &str) -> String {
    let hashed = || format!("id-{}", &sha256_hex(id.as_bytes())[..16]);
    if id.contains('/') || id.contains('\\') || id.contains("..") {
        return hashed();
    }
    let mut name = String::with_capacity(id.len());
    for c in id.chars() {
        let keep = c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-';
        let out = if keep { c } else { '_' };
        if out == '_' && name.ends_with('_') {
            continue;
        }
        name.push(out);
    }
    if name.is_empty() || name == "." || name == ".." {
        return hashed();
    }
    name
}

/// Make sure the PDF and (when the item has one) the source of `item` are in
/// the cache, verified against the manifest hashes, and the source unpacked.
///
/// Cached files are reused without touching the network. With `offline`,
/// a missing file is `Http("offline")`. Downloads use `user_agent`, 60 s
/// timeouts, follow redirects and are retried once after 2 s on a transport
/// error. A manifest hash that is present and does not match the bytes is
/// [`CorpusError::HashMismatch`]; a mismatching download is not cached.
pub fn fetch_item(
    item: &ManifestItem,
    cache_dir: &Path,
    user_agent: &str,
    offline: bool,
) -> Result<Fetched, CorpusError> {
    let (pdf_path, archive_path, source_dir) = cache_paths(cache_dir, item);
    if let Some(parent) = pdf_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let pdf_expected = item.pdf_sha256.as_deref();
    let pdf_bytes = obtain(
        &item.id,
        &pdf_path,
        &item.pdf_url,
        pdf_expected,
        user_agent,
        offline,
    )?;
    let pdf_sha256 = sha256_hex(&pdf_bytes);

    let source_url = item.source_url.as_deref();
    let archive_bytes: Option<Vec<u8>> = if archive_path.is_file() || source_url.is_some() {
        let expected = item.source_sha256.as_deref();
        let url = source_url.unwrap_or_default();
        let bytes = obtain(&item.id, &archive_path, url, expected, user_agent, offline)?;
        Some(bytes)
    } else {
        None
    };

    let mut source_sha256 = None;
    let mut unpacked_dir = None;
    if let Some(bytes) = archive_bytes {
        source_sha256 = Some(sha256_hex(&bytes));
        unpacked_dir = ensure_unpacked(&bytes, source_dir)?;
    }

    Ok(Fetched {
        pdf_path,
        source_dir: unpacked_dir,
        pdf_sha256,
        source_sha256,
    })
}

/// Bytes of `path` from the cache, or downloaded from `url`, verified and
/// (only when verified) written to the cache.
fn obtain(
    id: &str,
    path: &Path,
    url: &str,
    expected: Option<&str>,
    user_agent: &str,
    offline: bool,
) -> Result<Vec<u8>, CorpusError> {
    if path.is_file() {
        let bytes = fs::read(path)?;
        verify_hash(id, expected, &bytes)?;
        return Ok(bytes);
    }
    if offline {
        return Err(CorpusError::Http("offline".to_string()));
    }
    let bytes = download(url, user_agent)?;
    verify_hash(id, expected, &bytes)?;
    write_atomic(path, &bytes)?;
    Ok(bytes)
}

fn verify_hash(id: &str, expected: Option<&str>, bytes: &[u8]) -> Result<(), CorpusError> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let actual = sha256_hex(bytes);
    if expected.eq_ignore_ascii_case(&actual) {
        Ok(())
    } else {
        Err(CorpusError::HashMismatch {
            id: id.to_string(),
            expected: expected.to_string(),
            actual,
        })
    }
}

/// Write via a `.part` sibling and rename, so a crash never leaves a
/// truncated file that a later run would trust.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), CorpusError> {
    let part = part_path(path);
    fs::write(&part, bytes)?;
    fs::rename(part, path)?;
    Ok(())
}

fn part_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
}

/// The unpacked source directory for `bytes`: reused when it exists,
/// otherwise created; `None` when the archive is a PDF (no TeX source).
fn ensure_unpacked(bytes: &[u8], dest: PathBuf) -> Result<Option<PathBuf>, CorpusError> {
    if dest.is_dir() {
        return Ok(Some(dest));
    }
    match unpack_fresh(bytes, &dest) {
        Ok(()) => Ok(Some(dest)),
        Err(CorpusError::NoSource(_)) => Ok(None),
        Err(err) => Err(err),
    }
}

/// Unpack into a `.part` sibling directory and rename it into place.
fn unpack_fresh(bytes: &[u8], dest: &Path) -> Result<(), CorpusError> {
    let part = part_path(dest);
    if part.exists() {
        fs::remove_dir_all(&part)?;
    }
    if let Err(err) = unpack_source(bytes, &part) {
        // Best effort: a failed unpack must not leave a half-written tree.
        let _ = fs::remove_dir_all(&part);
        return Err(err);
    }
    fs::rename(part, dest)?;
    Ok(())
}

fn download(url: &str, user_agent: &str) -> Result<Vec<u8>, CorpusError> {
    let config = ureq::Agent::config_builder()
        .user_agent(user_agent)
        .http_status_as_error(true)
        .max_redirects(10)
        .timeout_resolve(Some(TIMEOUT))
        .timeout_connect(Some(TIMEOUT))
        .timeout_send_request(Some(TIMEOUT))
        .timeout_recv_response(Some(TIMEOUT))
        .timeout_recv_body(Some(TIMEOUT))
        .build();
    let agent = ureq::Agent::new_with_config(config);
    match download_once(&agent, url) {
        Ok(bytes) => Ok(bytes),
        Err(first) if is_transient(&first) => {
            std::thread::sleep(RETRY_DELAY);
            download_once(&agent, url).map_err(|err| http_error(url, &err))
        }
        Err(err) => Err(http_error(url, &err)),
    }
}

fn download_once(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, ureq::Error> {
    let mut response = agent.get(url).call()?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD_BYTES)
        .read_to_vec()
}

fn http_error(url: &str, err: &ureq::Error) -> CorpusError {
    CorpusError::Http(format!("{url}: {err}"))
}

/// Errors worth one retry: the transport failed or the server was busy.
fn is_transient(err: &ureq::Error) -> bool {
    match err {
        ureq::Error::StatusCode(code) => *code == 429 || *code >= 500,
        ureq::Error::Io(_)
        | ureq::Error::Timeout(_)
        | ureq::Error::Protocol(_)
        | ureq::Error::HostNotFound
        | ureq::Error::ConnectionFailed
        | ureq::Error::BodyStalled => true,
        _ => false,
    }
}

fn too_big(what: &str) -> CorpusError {
    CorpusError::Unpack(format!("{what} exceeds 200 MB"))
}

/// Unpack an e-print into `dest`.
///
/// Gzip (magic `1f 8b`) is decompressed fully into memory (bounded at
/// 200 MB). If the result is a tar archive (`ustar` magic at offset 257 or
/// a header with a valid checksum) it is extracted with entries whose path
/// is absolute or contains `..` rejected; otherwise it is written to
/// `dest/main.tex`. Bytes starting with `%PDF` (before or after gunzip) are
/// [`CorpusError::NoSource`]; nothing is created in that case.
pub fn unpack_source(bytes: &[u8], dest: &Path) -> Result<(), CorpusError> {
    if bytes.starts_with(b"%PDF") {
        return Err(CorpusError::NoSource("e-print is a PDF".to_string()));
    }
    let payload: Vec<u8> = if bytes.starts_with(&GZIP_MAGIC) {
        gunzip(bytes)?
    } else {
        bytes.to_vec()
    };
    if payload.starts_with(b"%PDF") {
        return Err(CorpusError::NoSource("gzip of a PDF".to_string()));
    }
    if payload.len() as u64 > MAX_UNPACKED_BYTES {
        return Err(too_big("source"));
    }
    fs::create_dir_all(dest)?;
    if looks_like_tar(&payload) {
        unpack_tar(&payload, dest)
    } else {
        fs::write(dest.join("main.tex"), &payload)?;
        Ok(())
    }
}

fn gunzip(bytes: &[u8]) -> Result<Vec<u8>, CorpusError> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut out = Vec::new();
    let read = decoder
        .take(MAX_UNPACKED_BYTES + 1)
        .read_to_end(&mut out)
        .map_err(|err| CorpusError::Unpack(format!("gzip: {err}")))?;
    if read as u64 > MAX_UNPACKED_BYTES {
        return Err(too_big("decompressed source"));
    }
    Ok(out)
}

/// A tar archive starts with a 512-byte header carrying either the `ustar`
/// magic or, for the old format, at least a name and a valid checksum.
fn looks_like_tar(bytes: &[u8]) -> bool {
    if bytes.len() < TAR_BLOCK {
        return false;
    }
    let header = &bytes[..TAR_BLOCK];
    if header[TAR_MAGIC_OFFSET..].starts_with(b"ustar") {
        return true;
    }
    header[0] != 0 && header_checksum_valid(header)
}

fn byte_sum(bytes: &[u8]) -> u32 {
    bytes.iter().map(|byte| u32::from(*byte)).sum()
}

/// The header checksum is the octal sum of all header bytes with the
/// checksum field itself counted as eight spaces.
fn header_checksum_valid(header: &[u8]) -> bool {
    let digits: String = header[TAR_CKSUM_RANGE]
        .iter()
        .copied()
        .filter(u8::is_ascii_digit)
        .map(char::from)
        .collect();
    let Ok(stored) = u32::from_str_radix(&digits, 8) else {
        return false;
    };
    let sum = byte_sum(header) - byte_sum(&header[TAR_CKSUM_RANGE]) + 8 * u32::from(b' ');
    sum == stored
}

fn unpack_tar(bytes: &[u8], dest: &Path) -> Result<(), CorpusError> {
    let mut archive = tar::Archive::new(bytes);
    let mut total: u64 = 0;
    let entries = archive
        .entries()
        .map_err(|err| CorpusError::Unpack(format!("tar: {err}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|err| CorpusError::Unpack(format!("tar entry: {err}")))?;
        let path: PathBuf = entry
            .path()
            .map_err(|err| CorpusError::Unpack(format!("tar entry path: {err}")))?
            .into_owned();
        check_entry_path(&path)?;
        let shown = path.display().to_string();
        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            // Links are never part of a TeX bundle; never create them.
            continue;
        }
        total = total.saturating_add(entry.size());
        if total > MAX_UNPACKED_BYTES {
            return Err(too_big("unpacked archive"));
        }
        let unpacked = entry
            .unpack_in(dest)
            .map_err(|err| CorpusError::Unpack(format!("{shown}: {err}")))?;
        if !unpacked {
            return Err(CorpusError::Unpack(format!("unsafe entry path: {shown}")));
        }
    }
    Ok(())
}

fn check_entry_path(path: &Path) -> Result<(), CorpusError> {
    let shown = path.display();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                return Err(CorpusError::Unpack(format!("absolute path: {shown}")));
            }
            Component::ParentDir => {
                return Err(CorpusError::Unpack(format!("path traversal: {shown}")));
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

/// Recursively list the `.tex`, `.bbl` and `.bib` files under `dir`
/// (extension compared case-insensitively), each list sorted.
pub fn find_latex_files(dir: &Path) -> Result<LatexFiles, CorpusError> {
    let mut files = LatexFiles {
        root: dir.to_path_buf(),
        tex: Vec::new(),
        bbl: Vec::new(),
        bib: Vec::new(),
    };
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Some(ext) = path.extension().and_then(std::ffi::OsStr::to_str) else {
                continue;
            };
            match ext.to_ascii_lowercase().as_str() {
                "tex" => files.tex.push(path),
                "bbl" => files.bbl.push(path),
                "bib" => files.bib.push(path),
                _ => {}
            }
        }
    }
    files.tex.sort();
    files.bbl.sort();
    files.bib.sort();
    Ok(files)
}

/// Fill in the missing `sha256` fields of the item `id` from `fetched`.
/// Existing values are kept. Returns whether anything changed.
pub fn update_manifest_hashes(manifest: &mut Manifest, id: &str, fetched: &Fetched) -> bool {
    let mut changed = false;
    for item in manifest.items.iter_mut().filter(|item| item.id == id) {
        if item.pdf_sha256.is_none() {
            item.pdf_sha256 = Some(fetched.pdf_sha256.clone());
            changed = true;
        }
        if item.source_sha256.is_none()
            && let Some(hash) = &fetched.source_sha256
        {
            item.source_sha256 = Some(hash.clone());
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tempfile::tempdir;

    use super::*;

    fn item(id: &str) -> ManifestItem {
        ManifestItem {
            id: id.to_string(),
            kind: "arxiv".to_string(),
            license: "http://creativecommons.org/licenses/by/4.0/".to_string(),
            pdf_url: "https://arxiv.org/pdf/2502.00857".to_string(),
            source_url: Some("https://arxiv.org/e-print/2502.00857".to_string()),
            pdf_sha256: None,
            source_sha256: None,
            categories: vec!["cs.CL".to_string(), "cs.LG".to_string()],
            split: "dev".to_string(),
            notes: None,
        }
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn build_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder.append_data(&mut header, *name, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    /// A tar with one regular entry whose name is written raw, bypassing the
    /// `..`/absolute validation of `Header::set_path`.
    fn raw_named_tar(name: &[u8]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.as_mut_bytes()[..name.len()].copy_from_slice(name);
        header.set_size(3);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder.append(&header, &b"bad"[..]).unwrap();
        builder.into_inner().unwrap()
    }

    fn sample_tar_gz() -> Vec<u8> {
        let tar_bytes = build_tar(&[
            ("a.tex", b"\\begin{document}\nHi\n\\end{document}\n"),
            ("sub/b.bbl", b"\\bibitem{a} A.\n"),
            ("figure.png", b"\x89PNG"),
        ]);
        gzip(&tar_bytes)
    }

    #[test]
    fn manifest_round_trips_through_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("manifest.json");
        let mut second = item("arxiv:2108.04588");
        second.split = "holdout".to_string();
        second.pdf_sha256 = Some("ab".repeat(32));
        second.notes = Some("bbl present".to_string());
        let manifest = Manifest {
            version: 1,
            items: vec![item("arxiv:2502.00857"), second],
        };

        save_manifest(&path, &manifest).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.ends_with("}\n"), "trailing newline");
        assert!(text.contains("\n  \"version\": 1,\n"));

        let back = load_manifest(&path).unwrap();
        assert_eq!(back, manifest);
    }

    #[test]
    fn missing_manifest_is_io_error() {
        let dir = tempdir().unwrap();
        let err = load_manifest(&dir.path().join("none.json")).unwrap_err();
        assert!(matches!(err, CorpusError::Io(_)), "{err:?}");
    }

    #[test]
    fn cache_paths_have_expected_shape() {
        let cache = Path::new("/cache");
        let (pdf, archive, source) = cache_paths(cache, &item("arxiv:2502.00857"));
        assert_eq!(pdf, Path::new("/cache/arxiv_2502.00857/paper.pdf"));
        assert_eq!(archive, Path::new("/cache/arxiv_2502.00857/source.bin"));
        assert_eq!(source, Path::new("/cache/arxiv_2502.00857/source"));
    }

    #[test]
    fn cache_dir_name_replaces_colon_and_collapses_repeats() {
        assert_eq!(cache_dir_name("arxiv:2502.00857"), "arxiv_2502.00857");
        assert_eq!(cache_dir_name("a::b  c"), "a_b_c");
        assert_eq!(cache_dir_name("x_:y"), "x_y");
    }

    #[test]
    fn cache_dir_name_hashes_unsafe_ids() {
        let cache = Path::new("/cache");
        for id in ["../evil", "/abs", "a/b", "a\\b", "..", ".", ""] {
            let name = cache_dir_name(id);
            let expected = format!("id-{}", &sha256_hex(id.as_bytes())[..16]);
            assert_eq!(name, expected, "{id:?}");
            assert!(!name.contains('/') && !name.contains('\\'), "{id:?}");
            let (pdf, archive, source) = cache_paths(cache, &item(id));
            let dir = cache.join(&name);
            assert_eq!(pdf, dir.join("paper.pdf"), "{id:?}");
            assert_eq!(archive, dir.join("source.bin"), "{id:?}");
            assert_eq!(source, dir.join("source"), "{id:?}");
            assert!(pdf.starts_with(cache), "{id:?}");
            assert_eq!(dir.parent(), Some(cache), "{id:?}");
        }
    }

    #[test]
    fn cache_dir_name_never_escapes_cache_dir() {
        let cache = Path::new("/cache/root");
        for id in [
            "arxiv:2502.00857",
            "../evil",
            "/abs",
            "a/b",
            "ok-id_1.2",
            "x/../y",
        ] {
            let name = cache_dir_name(id);
            assert!(!name.is_empty() && name != "." && name != "..", "{id:?}");
            let joined = cache.join(&name);
            assert!(joined.starts_with(cache), "{id:?}");
            let components: Vec<Component<'_>> = joined.components().collect();
            assert!(
                components
                    .iter()
                    .all(|c| !matches!(c, Component::ParentDir)),
                "{id:?}"
            );
            assert_eq!(joined.parent(), Some(cache), "{id:?}");
        }
    }

    #[test]
    fn tar_gz_bundle_unpacks_and_latex_files_are_found() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("src");
        unpack_source(&sample_tar_gz(), &dest).unwrap();

        assert!(dest.join("a.tex").is_file());
        assert!(dest.join("sub").join("b.bbl").is_file());
        assert!(dest.join("figure.png").is_file());

        let files = find_latex_files(&dest).unwrap();
        assert_eq!(files.root, dest);
        assert_eq!(files.tex, vec![dest.join("a.tex")]);
        assert_eq!(files.bbl, vec![dest.join("sub").join("b.bbl")]);
        assert!(files.bib.is_empty());
    }

    #[test]
    fn find_latex_files_sorts_and_ignores_case_of_extension() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("z")).unwrap();
        fs::write(root.join("z").join("main.tex"), "x").unwrap();
        fs::write(root.join("b.TEX"), "x").unwrap();
        fs::write(root.join("a.tex"), "x").unwrap();
        fs::write(root.join("refs.bib"), "x").unwrap();
        fs::write(root.join("notes.txt"), "x").unwrap();

        let files = find_latex_files(root).unwrap();
        assert_eq!(files.tex.len(), 3);
        assert_eq!(files.tex[0], root.join("a.tex"));
        assert_eq!(files.tex[1], root.join("b.TEX"));
        assert_eq!(files.tex[2], root.join("z").join("main.tex"));
        assert_eq!(files.bib, vec![root.join("refs.bib")]);
        assert!(files.bbl.is_empty());
    }

    #[test]
    fn bare_gzip_single_file_becomes_main_tex() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("src");
        let tex = b"\\documentclass{article}\n\\begin{document}\nHi\n\\end{document}\n";
        unpack_source(&gzip(tex), &dest).unwrap();

        assert_eq!(fs::read(dest.join("main.tex")).unwrap(), tex.to_vec());
        let files = find_latex_files(&dest).unwrap();
        assert_eq!(files.tex, vec![dest.join("main.tex")]);
    }

    #[test]
    fn uncompressed_tar_is_recognised() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("src");
        let tar_bytes = build_tar(&[("paper.tex", b"\\begin{document}")]);
        unpack_source(&tar_bytes, &dest).unwrap();
        assert!(dest.join("paper.tex").is_file());
        assert!(!dest.join("main.tex").exists());
    }

    #[test]
    fn pdf_bytes_are_no_source() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("src");
        let err = unpack_source(b"%PDF-1.5\n%\xe2\xe3\xcf\xd3\n", &dest).unwrap_err();
        assert!(matches!(err, CorpusError::NoSource(_)), "{err:?}");
        assert!(!dest.exists(), "nothing is created for a PDF e-print");

        let err = unpack_source(&gzip(b"%PDF-1.4 gzipped"), &dest).unwrap_err();
        assert!(matches!(err, CorpusError::NoSource(_)), "{err:?}");
        assert!(!dest.exists());
    }

    #[test]
    fn path_traversal_entry_is_rejected() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("inner").join("src");
        fs::create_dir_all(dir.path().join("inner")).unwrap();

        let err = unpack_source(&gzip(&raw_named_tar(b"../evil.tex")), &dest).unwrap_err();
        assert!(matches!(err, CorpusError::Unpack(_)), "{err:?}");
        assert!(!dir.path().join("inner").join("evil.tex").exists());
        assert!(!dest.join("evil.tex").exists());
    }

    #[test]
    fn absolute_entry_is_rejected() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("src");

        let err = unpack_source(&raw_named_tar(b"/tmp/evil.tex"), &dest).unwrap_err();
        assert!(matches!(err, CorpusError::Unpack(_)), "{err:?}");
        assert!(!dest.join("tmp").exists());
    }

    #[test]
    fn corrupt_gzip_is_unpack_error() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("src");
        let mut bytes = gzip(b"hello world, this is some text to compress");
        let keep = bytes.len() - 7;
        bytes.truncate(keep);
        let err = unpack_source(&bytes, &dest).unwrap_err();
        assert!(matches!(err, CorpusError::Unpack(_)), "{err:?}");
    }

    #[test]
    fn tar_detection_by_checksum_without_magic() {
        let mut header = [0u8; TAR_BLOCK];
        header[..5].copy_from_slice(b"a.tex");
        header[124..135].copy_from_slice(b"00000000000");
        let sum = byte_sum(&header) + 8 * u32::from(b' ');
        let text = format!("{sum:06o}\0 ");
        header[148..156].copy_from_slice(text.as_bytes());
        let mut bytes = header.to_vec();
        bytes.extend_from_slice(&[0u8; 1024]);
        assert!(looks_like_tar(&bytes));

        header[0] = b'b';
        assert!(!looks_like_tar(&header));
        assert!(!looks_like_tar(b"\\documentclass{article}"));
        let mut long_text = vec![b'x'; 2 * TAR_BLOCK];
        long_text[0] = b'\\';
        assert!(!looks_like_tar(&long_text));
    }

    #[test]
    fn fetch_item_offline_uses_seeded_cache() {
        let cache = tempdir().unwrap();
        let paper = item("arxiv:2502.00857");
        let (pdf_path, archive_path, source_dir) = cache_paths(cache.path(), &paper);
        fs::create_dir_all(pdf_path.parent().unwrap()).unwrap();
        let pdf_bytes = b"%PDF-1.7 cached paper".to_vec();
        let archive_bytes = sample_tar_gz();
        fs::write(&pdf_path, &pdf_bytes).unwrap();
        fs::write(archive_path, &archive_bytes).unwrap();

        let fetched = fetch_item(&paper, cache.path(), "tpe-test/0", true).unwrap();
        assert_eq!(fetched.pdf_path, pdf_path);
        assert_eq!(fetched.pdf_sha256, sha256_hex(&pdf_bytes));
        assert_eq!(fetched.source_sha256, Some(sha256_hex(&archive_bytes)));
        assert_eq!(fetched.source_dir, Some(source_dir.clone()));
        assert!(source_dir.join("a.tex").is_file());
        assert!(source_dir.join("sub").join("b.bbl").is_file());

        // Second call: the unpacked directory is reused.
        let again = fetch_item(&paper, cache.path(), "tpe-test/0", true).unwrap();
        assert_eq!(again, fetched);
    }

    #[test]
    fn fetch_item_offline_verifies_manifest_hashes() {
        let cache = tempdir().unwrap();
        let mut paper = item("arxiv:2502.00857");
        let (pdf_path, _, _) = cache_paths(cache.path(), &paper);
        fs::create_dir_all(pdf_path.parent().unwrap()).unwrap();
        let pdf_bytes = b"%PDF-1.7 cached paper".to_vec();
        fs::write(pdf_path, &pdf_bytes).unwrap();
        paper.source_url = None;

        paper.pdf_sha256 = Some(sha256_hex(&pdf_bytes).to_uppercase());
        let fetched = fetch_item(&paper, cache.path(), "tpe-test/0", true).unwrap();
        assert_eq!(fetched.source_dir, None);
        assert_eq!(fetched.source_sha256, None);

        paper.pdf_sha256 = Some("00".repeat(32));
        let err = fetch_item(&paper, cache.path(), "tpe-test/0", true).unwrap_err();
        match err {
            CorpusError::HashMismatch {
                id,
                expected,
                actual,
            } => {
                assert_eq!(id, "arxiv:2502.00857");
                assert_eq!(expected, "00".repeat(32));
                assert_eq!(actual, sha256_hex(&pdf_bytes));
            }
            other => panic!("expected HashMismatch, got {other:?}"),
        }
    }

    #[test]
    fn fetch_item_offline_with_empty_cache_is_offline_error() {
        let cache = tempdir().unwrap();
        let paper = item("arxiv:2502.00857");
        let err = fetch_item(&paper, cache.path(), "tpe-test/0", true).unwrap_err();
        match err {
            CorpusError::Http(message) => assert_eq!(message, "offline"),
            other => panic!("expected Http(offline), got {other:?}"),
        }
    }

    #[test]
    fn fetch_item_offline_with_pdf_e_print_has_no_source_dir() {
        let cache = tempdir().unwrap();
        let paper = item("arxiv:2502.00857");
        let (pdf_path, archive_path, source_dir) = cache_paths(cache.path(), &paper);
        fs::create_dir_all(pdf_path.parent().unwrap()).unwrap();
        fs::write(pdf_path, b"%PDF-1.7 paper").unwrap();
        fs::write(archive_path, b"%PDF-1.7 the e-print is the pdf").unwrap();

        let fetched = fetch_item(&paper, cache.path(), "tpe-test/0", true).unwrap();
        assert_eq!(fetched.source_dir, None);
        assert!(fetched.source_sha256.is_some());
        assert!(!source_dir.exists());
    }

    #[test]
    fn update_manifest_hashes_fills_only_missing_fields() {
        let mut manifest = Manifest {
            version: 1,
            items: vec![item("arxiv:2502.00857"), item("arxiv:2108.04588")],
        };
        manifest.items[1].pdf_sha256 = Some("keep".to_string());
        let fetched = Fetched {
            pdf_path: PathBuf::from("paper.pdf"),
            source_dir: None,
            pdf_sha256: "p".repeat(64),
            source_sha256: Some("s".repeat(64)),
        };

        let first = "arxiv:2502.00857";
        let second = "arxiv:2108.04588";
        let missing = "arxiv:none";

        assert!(update_manifest_hashes(&mut manifest, first, &fetched));
        assert_eq!(manifest.items[0].pdf_sha256, Some("p".repeat(64)));
        assert_eq!(manifest.items[0].source_sha256, Some("s".repeat(64)));
        assert!(!update_manifest_hashes(&mut manifest, first, &fetched));

        assert!(update_manifest_hashes(&mut manifest, second, &fetched));
        assert_eq!(manifest.items[1].pdf_sha256, Some("keep".to_string()));
        assert_eq!(manifest.items[1].source_sha256, Some("s".repeat(64)));

        assert!(!update_manifest_hashes(&mut manifest, missing, &fetched));
    }
}

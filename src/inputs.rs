//! Input expansion for `tpe PATH...`: files are taken as given, directories
//! are walked recursively in a sorted, deterministic order, and only `.pdf`
//! files (any case) are selected. Hidden entries (names starting with `.`,
//! which covers AppleDouble `._*` files) are skipped, and symbolic links to
//! directories are not followed so a linked cycle cannot loop the walk.
//!
//! Every selected file carries the path its outputs mirror: the bare file
//! name for a file given directly, and the path relative to the directory
//! argument for a file found under one.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::schema::{Figure, PageText};

/// One selected input file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    /// The file as it will be opened.
    pub path: PathBuf,
    /// Where its outputs go, relative to the output directory: the file
    /// name for a file named on the command line, else the path below the
    /// directory argument that contained it (extension kept).
    pub relative: PathBuf,
}

/// An input with its planned output stem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planned {
    pub input: Input,
    /// `<out>/<relative without extension>`; outputs are `<stem>.txt`,
    /// `<stem>.json` and `<stem>.figures/`.
    pub stem: PathBuf,
    /// An earlier input already owns `stem`, so this one must add a
    /// `-<first 12 hex of its SHA-256>` suffix.
    pub needs_suffix: bool,
}

/// Whether `name` is hidden: it starts with `.` (this includes AppleDouble
/// `._*` resource forks).
fn is_hidden(name: &OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

/// Whether `path` has a `pdf` extension in any case.
pub fn is_pdf(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

/// Walk `dir` recursively, appending every PDF found to `found` with its
/// path relative to `root`. Entries are visited in file-name order.
fn walk(root: &Path, dir: &Path, found: &mut Vec<Input>) -> io::Result<()> {
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        if is_hidden(&name) {
            continue;
        }
        let path = entry.path();
        // `file_type` does not follow links, so a linked directory is a
        // symlink here and is skipped; a linked file still counts as a file.
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk(root, &path, found)?;
        } else if is_pdf(&path) && path.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_or_else(|_| PathBuf::from(&name), Path::to_path_buf);
            found.push(Input { path, relative });
        }
    }
    Ok(())
}

/// Expand the command-line `paths` into input files, in argument order and,
/// within a directory, in sorted walk order. A path that is not a directory
/// is passed through as a file even when it does not exist, so it fails at
/// open time with a per-file error instead of aborting the batch. Reading a
/// directory that cannot be listed is an error.
pub fn expand(paths: &[PathBuf]) -> io::Result<Vec<Input>> {
    let mut found: Vec<Input> = Vec::new();
    for path in paths {
        if path.is_dir() {
            walk(path, path, &mut found)?;
        } else {
            let relative = path
                .file_name()
                .map_or_else(|| PathBuf::from("input"), PathBuf::from);
            found.push(Input {
                path: path.clone(),
                relative,
            });
        }
    }
    Ok(found)
}

/// Assign every input its output stem under `out`, flagging later inputs
/// whose stem an earlier input already claimed.
pub fn plan(inputs: Vec<Input>, out: &Path) -> Vec<Planned> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    inputs
        .into_iter()
        .map(|input| {
            let stem = out.join(input.relative.with_extension(""));
            let needs_suffix = !seen.insert(stem.clone());
            Planned {
                input,
                stem,
                needs_suffix,
            }
        })
        .collect()
}

/// What the leading bytes of an input file say it is. Extensions are never
/// trusted: a `.pdf` that starts like a JPEG is an [`Kind::Image`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `%PDF-` within the first kilobyte.
    Pdf,
    /// JPEG, PNG, GIF, TIFF, HEIC/HEIF or WebP.
    Image,
    /// A ZIP holding `word/...` (Word).
    Docx,
    /// A ZIP holding `xl/...` (Excel).
    Xlsx,
    /// A ZIP holding `ppt/...` (PowerPoint).
    Pptx,
    /// Any other ZIP archive.
    Zip,
    /// Nothing recognisable.
    Unknown,
}

impl Kind {
    /// The kind's name as printed and stored (`pdf`, `image`, `docx`, ...).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Image => "image",
            Self::Docx => "docx",
            Self::Xlsx => "xlsx",
            Self::Pptx => "pptx",
            Self::Zip => "zip",
            Self::Unknown => "unknown",
        }
    }

    /// Why a non-PDF kind is refused.
    #[must_use]
    pub fn reason(self) -> String {
        format!(
            "{} input needs OCR/document conversion, which this build does not include",
            self.name()
        )
    }
}

/// Bytes at the start of a file that classification looks at.
pub const HEAD_BYTES: usize = 4096;
/// Bytes at the end of a ZIP file searched for its central directory.
pub const TAIL_BYTES: u64 = 1024 * 1024;

/// The little-endian `u16` at `at`.
fn u16_at(bytes: &[u8], at: usize) -> Option<usize> {
    let low = *bytes.get(at)?;
    let high = *bytes.get(at + 1)?;
    Some(usize::from(u16::from_le_bytes([low, high])))
}

/// The entry name of a ZIP local file header starting at `at`.
fn zip_local_name(bytes: &[u8], at: usize) -> Option<&[u8]> {
    let len = u16_at(bytes, at + 26)?;
    bytes.get(at + 30..at + 30 + len)
}

/// The entry name of a ZIP central directory header starting at `at`.
fn zip_central_name(bytes: &[u8], at: usize) -> Option<&[u8]> {
    let len = u16_at(bytes, at + 28)?;
    bytes.get(at + 46..at + 46 + len)
}

/// The Office kind an entry name reveals.
fn office_kind(name: &[u8]) -> Option<Kind> {
    if name.starts_with(b"word/") {
        Some(Kind::Docx)
    } else if name.starts_with(b"xl/") {
        Some(Kind::Xlsx)
    } else if name.starts_with(b"ppt/") {
        Some(Kind::Pptx)
    } else {
        None
    }
}

/// Classify a ZIP by its first local header name, then by every central
/// directory entry found in `tail`.
fn classify_zip(head: &[u8], tail: &[u8]) -> Kind {
    if let Some(kind) = zip_local_name(head, 0).and_then(office_kind) {
        return kind;
    }
    let found = tail
        .windows(4)
        .enumerate()
        .filter(|(_, window)| *window == b"PK\x01\x02")
        .find_map(|(at, _)| zip_central_name(tail, at).and_then(office_kind));
    found.unwrap_or(Kind::Zip)
}

/// ISO base media brands (bytes 8..12 after `ftyp`) that mean HEIC/HEIF.
const HEIF_BRANDS: [&[u8]; 5] = [b"heic", b"heix", b"mif1", b"hevc", b"heif"];

/// The ZIP local file header signature.
const ZIP_MAGIC: [u8; 4] = [0x50, 0x4B, 0x03, 0x04];

/// Whether `head` starts a JPEG, PNG, GIF, TIFF, WebP or HEIC/HEIF file.
fn is_image(head: &[u8]) -> bool {
    let webp = head.starts_with(b"RIFF") && head.get(8..12) == Some(&b"WEBP"[..]);
    let brand = head.get(4..8) == Some(&b"ftyp"[..]);
    let heif = brand && head.get(8..12).is_some_and(|b| HEIF_BRANDS.contains(&b));
    head.starts_with(&[0xFF, 0xD8, 0xFF])
        || head.starts_with(&[0x89, 0x50, 0x4E, 0x47])
        || head.starts_with(b"GIF8")
        || head.starts_with(&[0x49, 0x49, 0x2A, 0x00])
        || head.starts_with(&[0x4D, 0x4D, 0x00, 0x2A])
        || webp
        || heif
}

/// Classify a file from its first bytes (`head`, up to [`HEAD_BYTES`]) and,
/// for ZIP archives, its last bytes (`tail`, up to [`TAIL_BYTES`]).
#[must_use]
pub fn classify_bytes(head: &[u8], tail: &[u8]) -> Kind {
    let pdf_window = &head[..head.len().min(1024)];
    if pdf_window.windows(5).any(|window| window == b"%PDF-") {
        return Kind::Pdf;
    }
    if is_image(head) {
        return Kind::Image;
    }
    if head.starts_with(&ZIP_MAGIC) {
        return classify_zip(head, tail);
    }
    Kind::Unknown
}

/// Classify the file at `path` by reading its head (and, for a ZIP, its
/// tail). A file that cannot be read is an error, never a guess.
pub fn classify(path: &Path) -> io::Result<Kind> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = fs::File::open(path)?;
    let mut head: Vec<u8> = Vec::new();
    let mut limited = (&mut file).take(HEAD_BYTES as u64);
    limited.read_to_end(&mut head)?;
    let mut tail: Vec<u8> = Vec::new();
    if head.starts_with(&ZIP_MAGIC) {
        let len = file.metadata()?.len();
        let start = len.saturating_sub(TAIL_BYTES);
        file.seek(SeekFrom::Start(start))?;
        file.read_to_end(&mut tail)?;
    }
    Ok(classify_bytes(&head, &tail))
}

/// Reason recorded for a scanned document.
pub const SCANNED_REASON: &str = "no text layer; OCR is not included in this build";

/// Whether one page is image-only: fewer than 20 non-whitespace characters
/// of text and at least one `raster` figure covering half the page or more.
fn page_is_scanned(page: &PageText) -> bool {
    let characters = page.text.chars().filter(|c| !c.is_whitespace()).count();
    if characters >= 20 {
        return false;
    }
    let page_area = page.width * page.height;
    if page_area <= 0.0 {
        return false;
    }
    for figure in &page.figures {
        if covers_half_page(figure, page_area) {
            return true;
        }
    }
    false
}

/// Whether `figure` is a raster whose box covers at least half of
/// `page_area`.
fn covers_half_page(figure: &Figure, page_area: f32) -> bool {
    if figure.kind != "raster" {
        return false;
    }
    figure.bbox.is_some_and(|bbox| {
        let area = (bbox.x1 - bbox.x0).abs() * (bbox.y1 - bbox.y0).abs();
        area >= 0.5 * page_area
    })
}

/// Whether `pages` look like a scan: at least one page, and 80% or more of
/// them image-only (see [`page_is_scanned`]).
#[must_use]
pub fn looks_scanned(pages: &[PageText]) -> bool {
    if pages.is_empty() {
        return false;
    }
    let scanned = pages.iter().filter(|page| page_is_scanned(page)).count();
    scanned * 5 >= pages.len() * 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::BBox;

    fn raster_page(text: &str, coverage: f32) -> PageText {
        let mut page = PageText::new(1, 600.0, 800.0, 0);
        page.text = text.to_string();
        page.figures.push(Figure {
            index: 0,
            bbox: Some(BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 600.0,
                y1: 800.0 * coverage,
            }),
            kind: "raster".to_string(),
            mime: None,
            width_px: None,
            height_px: None,
            sha256: None,
            file: None,
            caption: None,
        });
        page
    }

    /// A minimal ZIP local file header followed by `name` (no data).
    fn zip_with_local_name(name: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0x50, 0x4B, 0x03, 0x04];
        bytes.extend_from_slice(&[0u8; 22]);
        bytes.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(name);
        bytes
    }

    /// A ZIP central directory header for `name`.
    fn zip_central_entry(name: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0x50, 0x4B, 0x01, 0x02];
        bytes.extend_from_slice(&[0u8; 24]);
        bytes.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(&[0u8; 16]);
        bytes.extend_from_slice(name);
        bytes
    }

    #[test]
    fn magic_bytes_classify_inputs() {
        assert_eq!(classify_bytes(b"%PDF-1.7\n", &[]), Kind::Pdf);
        assert_eq!(classify_bytes(b"junk\n%PDF-1.4", &[]), Kind::Pdf);
        assert_eq!(classify_bytes(&[0xFF, 0xD8, 0xFF], &[]), Kind::Image);
        assert_eq!(classify_bytes(b"\x89PNG\r\n", &[]), Kind::Image);
        assert_eq!(classify_bytes(b"GIF89a", &[]), Kind::Image);
        assert_eq!(classify_bytes(b"II*\0abc", &[]), Kind::Image);
        assert_eq!(classify_bytes(b"MM\0*abc", &[]), Kind::Image);
        assert_eq!(classify_bytes(b"RIFF\0\0\0\0WEBPVP8 ", &[]), Kind::Image);
        assert_eq!(classify_bytes(b"\0\0\0\x18ftypheic", &[]), Kind::Image);
        assert_eq!(classify_bytes(b"\0\0\0\x18ftypmif1", &[]), Kind::Image);
        assert_eq!(classify_bytes(b"hello", &[]), Kind::Unknown);
        assert_eq!(classify_bytes(b"", &[]), Kind::Unknown);
    }

    #[test]
    fn zip_names_pick_the_office_kind() {
        let docx = zip_with_local_name(b"word/document.xml");
        assert_eq!(classify_bytes(&docx, &[]), Kind::Docx);
        let plain = zip_with_local_name(b"[Content_Types].xml");
        assert_eq!(classify_bytes(&plain, &[]), Kind::Zip);
        let central = zip_central_entry(b"xl/workbook.xml");
        assert_eq!(classify_bytes(&plain, &central), Kind::Xlsx);
        let mut tail = zip_central_entry(b"docProps/core.xml");
        tail.extend(zip_central_entry(b"ppt/slides/slide1.xml"));
        assert_eq!(classify_bytes(&plain, &tail), Kind::Pptx);
        assert_eq!(Kind::Docx.name(), "docx");
        assert!(Kind::Image.reason().starts_with("image input needs OCR"));
    }

    #[test]
    fn classify_reads_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let jpeg = dir.path().join("photo.pdf");
        fs::write(&jpeg, [0xFF, 0xD8, 0xFF, 0xDB, 0, 1]).unwrap();
        assert_eq!(classify(&jpeg).unwrap(), Kind::Image);
        let mut docx = zip_with_local_name(b"[Content_Types].xml");
        docx.extend(vec![0u8; 100]);
        docx.extend(zip_central_entry(b"word/document.xml"));
        let path = dir.path().join("report.docx");
        fs::write(&path, &docx).unwrap();
        assert_eq!(classify(&path).unwrap(), Kind::Docx);
        assert!(classify(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn scanned_pages_need_a_big_raster_and_no_text() {
        assert!(!looks_scanned(&[]));
        assert!(looks_scanned(&[raster_page("", 1.0)]));
        assert!(looks_scanned(&[raster_page("  p. 3 ", 0.6)]));
        assert!(!looks_scanned(&[raster_page("", 0.4)]));
        let texty = raster_page("This page carries a real text layer of its own.", 1.0);
        assert!(!looks_scanned(&[texty]));
        let mut text_only = PageText::new(2, 600.0, 800.0, 0);
        text_only.text = "Twenty characters of body text here.".to_string();
        let mostly_scanned: Vec<PageText> = (0..4)
            .map(|_| raster_page("", 1.0))
            .chain(std::iter::once(text_only.clone()))
            .collect();
        assert!(looks_scanned(&mostly_scanned));
        let half: Vec<PageText> = vec![raster_page("", 1.0), text_only];
        assert!(!looks_scanned(&half));
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"%PDF-1.4\n").unwrap();
    }

    #[test]
    fn pdf_extension_is_case_insensitive() {
        assert!(is_pdf(Path::new("a.pdf")));
        assert!(is_pdf(Path::new("a.PDF")));
        assert!(is_pdf(Path::new("a.Pdf")));
        assert!(!is_pdf(Path::new("a.pdf.txt")));
        assert!(!is_pdf(Path::new("pdf")));
        assert!(!is_pdf(Path::new("a")));
    }

    #[test]
    fn directories_are_walked_sorted_and_mirrored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("b.pdf"));
        touch(&root.join("a.PDF"));
        touch(&root.join("sub/deep/c.pdf"));
        touch(&root.join("sub/notes.txt"));
        touch(&root.join(".hidden.pdf"));
        touch(&root.join("._a.pdf"));
        touch(&root.join(".git/x.pdf"));

        let inputs = expand(&[root.to_path_buf()]).unwrap();
        let relative: Vec<&Path> = inputs.iter().map(|i| i.relative.as_path()).collect();
        let expected = [
            Path::new("a.PDF"),
            Path::new("b.pdf"),
            Path::new("sub/deep/c.pdf"),
        ];
        assert_eq!(relative, expected);
        assert_eq!(inputs[2].path, root.join("sub/deep/c.pdf"));
    }

    #[test]
    fn files_are_taken_as_given_and_keep_only_their_name() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x/y/paper.pdf");
        touch(&file);
        let missing = dir.path().join("absent.pdf");
        let inputs = expand(&[file.clone(), missing.clone()]).unwrap();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0].path, file);
        assert_eq!(inputs[0].relative, Path::new("paper.pdf"));
        assert_eq!(inputs[1].path, missing);
        assert_eq!(inputs[1].relative, Path::new("absent.pdf"));
    }

    #[test]
    fn plan_flags_output_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("one/paper.pdf");
        let second = dir.path().join("two/paper.pdf");
        let third = dir.path().join("two/other.pdf");
        touch(&first);
        touch(&second);
        touch(&third);
        let inputs = expand(&[first, second, third]).unwrap();
        let planned = plan(inputs, Path::new("out"));
        assert_eq!(planned[0].stem, Path::new("out/paper"));
        assert!(!planned[0].needs_suffix);
        assert_eq!(planned[1].stem, Path::new("out/paper"));
        assert!(planned[1].needs_suffix);
        assert_eq!(planned[2].stem, Path::new("out/other"));
        assert!(!planned[2].needs_suffix);
    }

    #[test]
    fn plan_mirrors_directory_structure() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("sub/paper.pdf"));
        let inputs = expand(&[dir.path().to_path_buf()]).unwrap();
        let planned = plan(inputs, Path::new("out"));
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].stem, Path::new("out/sub/paper"));
    }

    #[test]
    fn empty_directory_yields_nothing_and_missing_path_passes_through() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        fs::create_dir(&gone).unwrap();
        let listed = expand(&[gone.clone()]).unwrap();
        assert!(listed.is_empty());
        fs::remove_dir(&gone).unwrap();
        // A missing path is not a directory, so it passes through as a file.
        let inputs = expand(&[gone.clone()]).unwrap();
        assert_eq!(inputs[0].path, gone);
    }
}

//! Which PDFs a job may be given.
//!
//! A path must be absolute and name an existing regular file ending in
//! `.pdf`. `.` and `..` segments are refused as written, before anything
//! touches the file system, so a path means what it says. Symbolic links
//! are allowed (macOS itself puts `/tmp` and `/var` behind one) and are
//! resolved once, here: the job reads the resolved file and writes its
//! outputs next to it, and the resolved file must also end in `.pdf`, so a
//! link named `x.pdf` cannot point a job at some other kind of file.
//! Directories, FIFOs, sockets and devices are refused (a FIFO would block
//! the engine forever).

use std::path::{Path, PathBuf};

/// Longest path accepted, in bytes (Linux's `PATH_MAX`).
const MAX_PATH_BYTES: usize = 4096;

/// Why a path was refused; `as_str` is the `reason` in the error body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathProblem {
    /// Empty, too long, or containing a NUL byte.
    Invalid,
    NotAbsolute,
    /// A `.` or `..` segment.
    DotSegment,
    NotFound,
    /// Exists but is not a regular file (a directory, FIFO, device …).
    NotAFile,
    /// The name, or the file a link resolves to, does not end in `.pdf`.
    NotPdf,
    /// The file system refused to resolve it (permissions, a link loop …).
    Unreadable,
}

impl PathProblem {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::NotAbsolute => "not_absolute",
            Self::DotSegment => "dot_segment",
            Self::NotFound => "not_found",
            Self::NotAFile => "not_a_file",
            Self::NotPdf => "not_pdf",
            Self::Unreadable => "unreadable",
        }
    }
}

fn is_pdf(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

/// The resolved path of an acceptable PDF.
pub fn validate(raw: &str) -> Result<PathBuf, PathProblem> {
    if raw.is_empty() || raw.len() > MAX_PATH_BYTES || raw.contains('\0') {
        return Err(PathProblem::Invalid);
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(PathProblem::NotAbsolute);
    }
    // `Path::components` silently drops interior `.` segments, so look at
    // the text itself.
    if raw
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return Err(PathProblem::DotSegment);
    }
    if !is_pdf(path) {
        return Err(PathProblem::NotPdf);
    }
    let resolved = std::fs::canonicalize(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => PathProblem::NotFound,
        _ => PathProblem::Unreadable,
    })?;
    let meta = std::fs::metadata(&resolved).map_err(|_| PathProblem::Unreadable)?;
    if !meta.is_file() {
        return Err(PathProblem::NotAFile);
    }
    if !is_pdf(&resolved) {
        return Err(PathProblem::NotPdf);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::{PathProblem, validate};

    #[test]
    fn only_existing_absolute_pdf_files_pass() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let pdf = root.join("paper.PDF");
        std::fs::write(&pdf, b"%PDF").unwrap();
        std::fs::write(root.join("notes.txt"), b"x").unwrap();
        std::fs::create_dir(root.join("folder.pdf")).unwrap();
        let text = |p: &std::path::Path| p.to_str().unwrap().to_string();

        assert_eq!(validate(&text(&pdf)), Ok(pdf.clone()));
        let cases = [
            (String::new(), PathProblem::Invalid),
            ("a\0b.pdf".to_string(), PathProblem::Invalid),
            (
                "/".to_string() + &"a".repeat(5000) + ".pdf",
                PathProblem::Invalid,
            ),
            ("paper.pdf".to_string(), PathProblem::NotAbsolute),
            (
                format!("{}/../x/paper.PDF", text(&root)),
                PathProblem::DotSegment,
            ),
            (
                format!("{}/./paper.PDF", text(&root)),
                PathProblem::DotSegment,
            ),
            (
                format!("{}/missing.pdf", text(&root)),
                PathProblem::NotFound,
            ),
            (format!("{}/notes.txt", text(&root)), PathProblem::NotPdf),
            (format!("{}/folder.pdf", text(&root)), PathProblem::NotAFile),
            (
                format!("{}/notes.txt/x.pdf", text(&root)),
                PathProblem::NotFound,
            ),
        ];
        for (raw, expected) in cases {
            assert_eq!(validate(&raw), Err(expected), "{raw:?}");
        }
    }

    #[test]
    fn links_resolve_and_must_land_on_a_pdf() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let real = root.join("real.pdf");
        std::fs::write(&real, b"%PDF").unwrap();
        let secret = root.join("secret.key");
        std::fs::write(&secret, b"k").unwrap();
        let good = root.join("alias.pdf");
        std::os::unix::fs::symlink(&real, &good).unwrap();
        let bad = root.join("evil.pdf");
        std::os::unix::fs::symlink(&secret, &bad).unwrap();
        let dangling = root.join("gone.pdf");
        std::os::unix::fs::symlink(root.join("nothing.pdf"), &dangling).unwrap();
        let fifo = root.join("pipe.pdf");
        let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path and a plain mode.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        assert_eq!(validate(good.to_str().unwrap()), Ok(real));
        assert_eq!(validate(bad.to_str().unwrap()), Err(PathProblem::NotPdf));
        assert_eq!(
            validate(dangling.to_str().unwrap()),
            Err(PathProblem::NotFound)
        );
        assert_eq!(validate(fifo.to_str().unwrap()), Err(PathProblem::NotAFile));
    }
}

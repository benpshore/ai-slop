//! Command-line launch arguments shared by the macOS binary and unit tests.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub struct LaunchArgs {
    pub ledger: Option<PathBuf>,
    pub documents: Vec<PathBuf>,
}

/// Parse launch arguments while preserving the original
/// `tpe-app <ledger.sqlite>` contract. PDF positionals are documents; the first
/// other positional is the ledger unless `--ledger` already selected one.
pub fn parse_args(arguments: impl IntoIterator<Item = OsString>) -> Result<LaunchArgs, String> {
    let mut ledger = std::env::var_os("TPE_LEDGER").map(PathBuf::from);
    let mut documents = Vec::new();
    let mut positional_ledger_seen = false;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        if argument == "--ledger" {
            let value = arguments
                .next()
                .ok_or_else(|| String::from("--ledger requires a path"))?;
            ledger = Some(PathBuf::from(value));
            positional_ledger_seen = true;
        } else {
            let path = PathBuf::from(argument);
            if is_pdf(&path) || positional_ledger_seen {
                documents.push(path);
            } else {
                ledger = Some(path);
                positional_ledger_seen = true;
            }
        }
    }
    Ok(LaunchArgs { ledger, documents })
}

fn is_pdf(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_positional_ledger_and_collects_pdfs() {
        let parsed = parse_args([
            OsString::from("corpus.sqlite"),
            OsString::from("one.pdf"),
            OsString::from("TWO.PDF"),
        ])
        .unwrap();
        assert_eq!(parsed.ledger, Some(PathBuf::from("corpus.sqlite")));
        assert_eq!(
            parsed.documents,
            [PathBuf::from("one.pdf"), PathBuf::from("TWO.PDF")]
        );
    }

    #[test]
    fn explicit_ledger_allows_documents_before_or_after_it() {
        let parsed = parse_args([
            OsString::from("first.pdf"),
            OsString::from("--ledger"),
            OsString::from("corpus.sqlite"),
            OsString::from("second.pdf"),
        ])
        .unwrap();
        assert_eq!(parsed.ledger, Some(PathBuf::from("corpus.sqlite")));
        assert_eq!(parsed.documents.len(), 2);
    }
}

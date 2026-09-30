//! Zotero-style file names from bibliographic metadata.
//!
//! [`suggest_filename`] is pure: a [`PaperRecord`] and [`FilenameOptions`] in,
//! a file name out. It never touches the file system; [`unique_name`] picks a
//! collision suffix against a caller-supplied "is this name taken" predicate.
//!
//! The default shape follows Zotero's "rename from parent metadata" default
//! template, `{{ firstCreator suffix=" - " }}{{ year suffix=" - " }}{{ title truncate="100" }}`
//! (`DEFAULT_ATTACHMENT_RENAME_TEMPLATE` in Zotero's `renameFiles.mjs`, and
//! <https://www.zotero.org/support/file_renaming>), so `Smith and Jones - 2020 - A title.pdf`.
//! Read on 2026-09-30 from the `main` branch of `github.com/zotero/zotero`
//! (files `chrome/content/zotero/xpcom/attachments.js`, `file.js`, `data/items.js`):
//! - the first-creator label is one surname, `A and B` for two, `A et al.` for three or more;
//! - the title is cut to 100 characters;
//! - `Zotero.File.getValidFileName` deletes `/ \ ? * : | " < >`, replaces newlines, tabs and
//!   thin spaces, drops control and zero-width characters, normalises to NFC, strips a leading
//!   `.` and never yields an empty name.
//!
//! Deliberate differences from Zotero:
//! - an illegal character becomes a space (then runs of spaces collapse), so `and/or` gives
//!   `and or`, not `andor`;
//! - Windows-reserved device names and trailing dots or spaces are avoided, and the name is
//!   capped at 255 UTF-8 bytes (most file systems' per-component limit), not only in characters;
//! - HTML and JATS markup that Crossref leaves in titles is removed.
//!
//! [`PaperRecord`] keeps authors as display strings, not `family`/`given` pairs, so
//! [`surname`] has to infer the family name: from `Family, Given`, or the last word of
//! `Given Family` extended left over lower-case particles (`van der`, `de la`, ...). It cannot
//! know that a two-word family name without a particle (`García Márquez`) is one name, or which
//! part of an unspaced CJK name is the family name (the whole name is used).

use tpe_common::PaperRecord;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::canonical_combining_class;

use crate::util::{decode_entities, strip_tags};

const EXTENSION: &str = ".pdf";
/// Most file systems (ext4, APFS, NTFS) allow 255 bytes or more per path component.
const MAX_COMPONENT_BYTES: usize = 255;
const SEPARATOR: &str = " - ";
/// A corporate author longer than this would crowd the title out of the name.
const MAX_CREATOR_CHARS: usize = 50;
/// Give up looking for a free name after this many suffixes.
const MAX_SUFFIX: u32 = 9999;

/// One component of the file name, in the order given by [`FilenameOptions::parts`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// First-creator label (see [`CreatorStyle`]).
    Creator,
    /// Four-digit publication year.
    Year,
    /// Title, cut to [`FilenameOptions::title_max_chars`].
    Title,
}

/// How the creator label is built from the author list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreatorStyle {
    /// Zotero: `Smith`, `Smith and Jones`, `Smith et al.`.
    Zotero,
    /// Only the first author's surname.
    First,
}

/// The whole (small) configuration of [`suggest_filename`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilenameOptions {
    /// Components in order, joined by ` - `; empty ones are left out.
    pub parts: Vec<Part>,
    /// Creator label style.
    pub creators: CreatorStyle,
    /// Title length cap in characters (not bytes).
    pub title_max_chars: usize,
    /// Cap on the whole name including `.pdf`, in characters (also capped at 255 bytes).
    pub max_chars: usize,
}

impl Default for FilenameOptions {
    fn default() -> Self {
        Self {
            parts: vec![Part::Creator, Part::Year, Part::Title],
            creators: CreatorStyle::Zotero,
            title_max_chars: 100,
            max_chars: 150,
        }
    }
}

/// A file name (always ending in `.pdf`) for `record`. Missing pieces are left
/// out; a record with nothing usable gives `Untitled.pdf`. The result contains
/// no path separator, control character or Windows-reserved name, is NFC, and
/// is cut on a character boundary.
pub fn suggest_filename(record: &PaperRecord, options: &FilenameOptions) -> String {
    let pieces: Vec<String> = options
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Creator => creator_label(&record.authors, options.creators),
            Part::Year => record
                .year
                .filter(|y| (1000..=2100).contains(y))
                .map(|y| y.to_string()),
            Part::Title => {
                let title = clean(&record.title);
                Some(cut_title(&title, options.title_max_chars).to_string())
            }
        })
        .filter(|piece| !piece.is_empty())
        .collect();
    let mut stem = pieces.join(SEPARATOR);
    if stem.is_empty() {
        stem = "Untitled".to_string();
    }
    let room = options.max_chars.saturating_sub(EXTENSION.len());
    finish_stem(&stem, room, MAX_COMPONENT_BYTES - EXTENSION.len())
}

/// The key under which two names count as the same file on a case- and
/// normalisation-insensitive file system (macOS, Windows): NFC, lower case.
pub fn fold_key(name: &str) -> String {
    name.nfc().flat_map(char::to_lowercase).collect()
}

/// `name` if `taken` says it is free, else `stem (2).pdf`, `stem (3).pdf`, ...
/// (the stem is shortened to keep the 255-byte limit). Deterministic; `None`
/// after 9999 suffixes. `name` is expected to come from [`suggest_filename`].
pub fn unique_name(name: &str, taken: impl Fn(&str) -> bool) -> Option<String> {
    if !taken(name) {
        return Some(name.to_string());
    }
    let stem = name.strip_suffix(EXTENSION).unwrap_or(name);
    (2..=MAX_SUFFIX).find_map(|n| {
        let suffix = format!(" ({n})");
        let room = MAX_COMPONENT_BYTES - EXTENSION.len() - suffix.len();
        let candidate = format!("{}{suffix}{EXTENSION}", cut(stem, usize::MAX, room));
        (!taken(&candidate)).then_some(candidate)
    })
}

/// Cut `stem` to the limits, avoid a reserved device name, add `.pdf`.
fn finish_stem(stem: &str, max_chars: usize, max_bytes: usize) -> String {
    let cut_stem = cut(stem, max_chars, max_bytes).trim_end_matches(['.', ' ']);
    let mut out = cut_stem.to_string();
    if is_reserved_device_name(&out) {
        out.push('_');
    }
    out.push_str(EXTENSION);
    out
}

/// Windows device names are reserved with any extension (`CON.pdf`).
fn is_reserved_device_name(stem: &str) -> bool {
    let head = stem.split('.').next().unwrap_or(stem).trim_end();
    let upper = head.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            upper
                .strip_prefix(prefix)
                .is_some_and(|n| matches!(n.as_bytes(), [b'0'..=b'9']))
        })
}

/// A title cut to `max_chars`, backed up to the last word boundary when that
/// keeps at least 60% of the allowance (Zotero cuts mid-word).
fn cut_title(title: &str, max_chars: usize) -> &str {
    let cut = cut(title, max_chars, usize::MAX);
    if cut.len() == title.len() || title[cut.len()..].starts_with(' ') {
        return cut;
    }
    match cut.rfind(' ') {
        Some(i) if cut[..i].chars().count() * 5 >= max_chars * 3 => cut[..i].trim_end(),
        _ => cut,
    }
}

/// The longest prefix of `s` within both limits that does not end before a
/// combining mark (which would strand it), trimmed of trailing spaces.
fn cut(s: &str, max_chars: usize, max_bytes: usize) -> &str {
    let mut end = s.len();
    for (count, (i, c)) in s.char_indices().enumerate() {
        if count >= max_chars || i + c.len_utf8() > max_bytes {
            end = i;
            break;
        }
    }
    let mut kept = &s[..end];
    if let Some(next) = s[end..].chars().next()
        && canonical_combining_class(next) != 0
        && let Some((i, _)) = kept.char_indices().next_back()
    {
        kept = &kept[..i];
    }
    kept.trim_end()
}

/// Markup and entities out, NFC, one space between words, and no character that
/// is illegal, invisible or a control character on macOS or Windows.
fn clean(raw: &str) -> String {
    let text = if has_tag(raw) {
        strip_tags(raw)
    } else {
        decode_entities(raw)
    };
    let mapped: String = text
        .nfc()
        .filter_map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => Some(' '),
            c if c.is_whitespace() => Some(' '),
            // Controls, soft hyphen, zero-width and bidi controls, BOM, replacement character.
            c if c.is_control()
                || matches!(
                    c,
                    '\u{ad}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
                        | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{feff}' | '\u{fffd}'
                ) =>
            {
                None
            }
            c => Some(c),
        })
        .collect();
    let joined = mapped.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.trim_start_matches('.').to_string()
}

/// True when `s` has something that looks like an HTML/XML tag (`<i>`, `</jats:p>`),
/// so a plain title such as `p < 0.05 and q > 1` is left alone.
fn has_tag(s: &str) -> bool {
    s.match_indices('<').any(|(i, _)| {
        let after = &s[i + 1..];
        after.starts_with(|c: char| c.is_ascii_alphabetic() || c == '/') && after.contains('>')
    })
}

fn creator_label(authors: &[String], style: CreatorStyle) -> Option<String> {
    let names: Vec<String> = authors.iter().filter_map(|a| surname(a)).collect();
    let label = match (style, names.as_slice()) {
        (_, []) => return None,
        (CreatorStyle::First, [first, ..]) | (CreatorStyle::Zotero, [first]) => first.clone(),
        (CreatorStyle::Zotero, [a, b]) => format!("{a} and {b}"),
        (CreatorStyle::Zotero, [first, ..]) => format!("{first} et al."),
    };
    Some(cut(&label, MAX_CREATOR_CHARS, usize::MAX).to_string())
}

/// Lower-case words that belong to the family name when they precede it.
const PARTICLES: [&str; 24] = [
    "van", "von", "der", "den", "de", "del", "della", "degli", "dei", "di", "da", "dal", "dos",
    "das", "du", "la", "le", "el", "al", "bin", "ibn", "ter", "ten", "zu",
];

/// Words that mark an author as an organisation or consortium.
const CORPORATE_WORDS: [&str; 22] = [
    "collaboration",
    "consortium",
    "group",
    "organization",
    "organisation",
    "association",
    "society",
    "institute",
    "university",
    "committee",
    "team",
    "initiative",
    "network",
    "foundation",
    "agency",
    "council",
    "department",
    "ministry",
    "commission",
    "academy",
    "inc",
    "ltd",
];

/// Generational suffixes that are not part of the family name.
const SUFFIXES: [&str; 8] = ["jr", "jr.", "sr", "sr.", "ii", "iii", "iv", "jnr"];

/// Family name (or organisation name) of one author string.
///
/// Understands `Family, Given` and `Given Family`; `Martin Luther King, Jr.` and
/// `Martin Luther King Jr.`; particles (`Ursula von der Leyen`, `Charles de la Cruz`);
/// single names; organisations (kept whole, without a leading `The`); all-caps names
/// of two or more words (`JOHN SMITH` gives `Smith`). `None` when nothing is left after cleaning.
pub fn surname(author: &str) -> Option<String> {
    let mut name = clean(author);
    if name.is_empty() {
        return None;
    }
    if name.split(' ').count() > 1
        && name.chars().any(char::is_uppercase)
        && !name.chars().any(char::is_lowercase)
    {
        name = title_case(&name);
    }
    let lower = name.to_lowercase();
    if lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| CORPORATE_WORDS.contains(&word))
    {
        return Some(name.strip_prefix("The ").unwrap_or(&name).to_string());
    }
    if let Some((family, rest)) = name.split_once(',') {
        let rest = rest.trim().to_lowercase();
        if !SUFFIXES.contains(&rest.as_str()) {
            return non_empty(family.trim());
        }
        name = family.trim().to_string();
    }
    let mut given: Vec<&str> = name.split(' ').collect();
    while given.len() > 1 && SUFFIXES.contains(&given[given.len() - 1].to_lowercase().as_str()) {
        given.pop();
    }
    let mut family = vec![given.pop()?];
    while let Some(&prev) = given.last() {
        let capitalised_first_word = given.len() == 1 && prev.starts_with(char::is_uppercase);
        if !PARTICLES.contains(&prev.to_lowercase().as_str()) || capitalised_first_word {
            break;
        }
        family.insert(0, prev);
        given.pop();
    }
    non_empty(&family.join(" "))
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

/// `SMITH-JONES` to `Smith-Jones`: a letter is upper case when the previous character is not a letter.
fn title_case(s: &str) -> String {
    let mut previous_is_letter = false;
    s.chars()
        .flat_map(|c| {
            let converted: Vec<char> = if previous_is_letter {
                c.to_lowercase().collect()
            } else {
                c.to_uppercase().collect()
            };
            previous_is_letter = c.is_alphabetic();
            converted
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(authors: &[&str], year: Option<u16>, title: &str) -> PaperRecord {
        PaperRecord {
            title: title.to_string(),
            authors: authors.iter().map(|a| (*a).to_string()).collect(),
            year,
            ..PaperRecord::default()
        }
    }

    fn name(authors: &[&str], year: Option<u16>, title: &str) -> String {
        suggest_filename(&record(authors, year, title), &FilenameOptions::default())
    }

    #[test]
    fn default_shape_follows_zotero() {
        let cases: [(&[&str], Option<u16>, &str, &str); 7] = [
            (
                &["Yann LeCun"],
                Some(2015),
                "Deep learning",
                "LeCun - 2015 - Deep learning.pdf",
            ),
            (
                &["Yann LeCun", "Yoshua Bengio"],
                Some(2015),
                "Deep learning",
                "LeCun and Bengio - 2015 - Deep learning.pdf",
            ),
            (
                &["Yann LeCun", "Yoshua Bengio", "Geoffrey Hinton"],
                Some(2015),
                "Deep learning",
                "LeCun et al. - 2015 - Deep learning.pdf",
            ),
            // Missing fields drop out with their separator.
            (
                &["Yann LeCun"],
                None,
                "Deep learning",
                "LeCun - Deep learning.pdf",
            ),
            (&[], Some(2015), "Deep learning", "2015 - Deep learning.pdf"),
            (&["Yann LeCun"], Some(2015), "", "LeCun - 2015.pdf"),
            (&[], None, "", "Untitled.pdf"),
        ];
        for (authors, year, title, expected) in cases {
            assert_eq!(name(authors, year, title), expected, "{authors:?} {title}");
        }
    }

    #[test]
    fn implausible_years_are_left_out() {
        assert_eq!(name(&["A B"], Some(0), "T"), "B - T.pdf");
        assert_eq!(name(&["A B"], Some(65535), "T"), "B - T.pdf");
    }

    #[test]
    fn surnames_of_awkward_real_world_names() {
        let cases = [
            ("Yann LeCun", "LeCun"),
            ("LeCun, Yann", "LeCun"),
            ("Ludwig van Beethoven", "van Beethoven"),
            ("van Beethoven, Ludwig", "van Beethoven"),
            ("Ursula von der Leyen", "von der Leyen"),
            ("Charles de la Cruz", "de la Cruz"),
            ("Vincent Van Gogh", "Van Gogh"),
            ("Osama bin Laden", "bin Laden"),
            ("Ana Paula dos Santos", "dos Santos"),
            // Family name only, particle first: nothing to split off.
            ("van der Berg", "van der Berg"),
            // A capitalised word first is a given name, not a particle.
            ("Van Nguyen", "Nguyen"),
            ("J. R. R. Tolkien", "Tolkien"),
            ("Martin Luther King, Jr.", "King"),
            ("Martin Luther King Jr.", "King"),
            ("Sammy Davis III", "Davis"),
            ("Gabriel García Márquez", "Márquez"),
            ("Patrick O'Brien", "O'Brien"),
            ("Jean-Pierre Smith-Jones", "Smith-Jones"),
            // Single names.
            ("Plato", "Plato"),
            ("Onlyfamily", "Onlyfamily"),
            // CJK without spaces stays whole; nothing is invented.
            ("王小明", "王小明"),
            ("山田太郎", "山田太郎"),
            // Shouting.
            ("JOHN SMITH", "Smith"),
            ("MARY O'BRIEN-JONES", "O'Brien-Jones"),
            ("SMITH, JOHN", "Smith"),
            // Corporate authors.
            ("The ATLAS Collaboration", "ATLAS Collaboration"),
            ("ATLAS COLLABORATION", "Atlas Collaboration"),
            ("World Health Organization", "World Health Organization"),
            ("Deep Learning Consortium", "Deep Learning Consortium"),
            ("CERN", "CERN"),
            // Decomposed input is normalised.
            ("Jose\u{301} Garci\u{301}a", "Garc\u{ed}a"),
        ];
        for (input, expected) in cases {
            assert_eq!(surname(input).as_deref(), Some(expected), "{input}");
        }
        assert_eq!(surname(""), None);
        assert_eq!(surname(" \u{200b}\t"), None);
    }

    #[test]
    fn creator_label_skips_unusable_authors() {
        assert_eq!(
            name(&["", "Ada Lovelace", " "], Some(1843), "Notes"),
            "Lovelace - 1843 - Notes.pdf"
        );
        let options = FilenameOptions {
            creators: CreatorStyle::First,
            ..FilenameOptions::default()
        };
        let r = record(&["A. Aad", "B. Abbott", "C. Abdallah"], Some(2012), "Higgs");
        assert_eq!(suggest_filename(&r, &options), "Aad - 2012 - Higgs.pdf");
    }

    #[test]
    fn long_corporate_author_is_capped() {
        let long = "International Committee of Medical Journal Editors Working Party on Uniform Requirements";
        let n = name(&[long], Some(2013), "Recommendations");
        assert!(n.starts_with("International Committee of Medical Journal Editors "));
        assert!(n.ends_with(" - 2013 - Recommendations.pdf"), "{n}");
        assert!(n.chars().count() < 100);
    }

    #[test]
    fn illegal_characters_become_spaces() {
        let cases = [
            ("Foo: a study of bar", "Foo a study of bar"),
            ("and/or", "and or"),
            ("Back\\slash | pipe", "Back slash pipe"),
            ("Is it 3*4? \"Maybe\"", "Is it 3 4 Maybe"),
            ("tab\tand\nnewline", "tab and newline"),
            ("zero\u{200b}width\u{202e}bidi\u{feff}", "zerowidthbidi"),
            ("nul\0byte\u{7f}", "nulbyte"),
            ("  .hidden title. ", "hidden title"),
            ("thin\u{2009}space\u{a0}nbsp", "thin space nbsp"),
            ("lost \u{fffd} char", "lost char"),
            // Markup Crossref leaves in titles, and entities.
            (
                "Effects of <i>Escherichia coli</i> on H<sub>2</sub>O &amp; more",
                "Effects of Escherichia coli on H 2 O & more",
            ),
            // Not markup: a bare comparison survives (`<` becomes a space).
            ("p < 0.05 and q > 1", "p 0.05 and q 1"),
        ];
        for (title, expected) in cases {
            assert_eq!(
                name(&[], None, title),
                format!("{expected}.pdf"),
                "{title:?}"
            );
        }
    }

    #[test]
    fn no_separator_or_control_survives_any_input() {
        let nasty = "a/b\\c:d*e?f\"g<h>i|j\0k\rl\u{202e}m\u{85}n";
        let n = name(&[nasty], Some(2000), nasty);
        assert!(
            !n.chars()
                .any(|c| "/\\:*?\"<>|".contains(c) || c.is_control()),
            "{n:?}"
        );
        assert_eq!(n.rsplit('.').next(), Some("pdf"));
    }

    #[test]
    fn names_are_nfc() {
        let n = name(&["Jose\u{301} Mu\u{308}ller"], Some(2001), "Cafe\u{301}");
        assert_eq!(n, "M\u{fc}ller - 2001 - Caf\u{e9}.pdf");
        assert_eq!(n, n.nfc().collect::<String>());
    }

    #[test]
    fn reserved_device_names_and_dots() {
        assert_eq!(name(&[], None, "CON"), "CON_.pdf");
        assert_eq!(name(&[], None, "aux"), "aux_.pdf");
        assert_eq!(name(&[], None, "COM1"), "COM1_.pdf");
        assert_eq!(name(&[], None, "lpt9"), "lpt9_.pdf");
        assert_eq!(name(&[], None, "COM10"), "COM10.pdf");
        assert_eq!(name(&[], None, "Console"), "Console.pdf");
        assert_eq!(name(&[], None, "CON.txt"), "CON.txt_.pdf");
        // A trailing dot or space is illegal on Windows.
        assert_eq!(name(&[], None, "Ends with dots..."), "Ends with dots.pdf");
    }

    #[test]
    fn title_is_cut_on_a_character_boundary() {
        // The title cap counts characters, not bytes.
        let n = name(&["Wei Wang"], Some(2020), &"x".repeat(200));
        assert_eq!(n.len(), "Wang - 2020 - .pdf".len() + 100);

        // 200 three-byte characters exceed the 255-byte limit: cut on a boundary.
        let cjk: String = "深".repeat(200);
        let n = name(&["Wei Wang"], Some(2020), &cjk);
        assert_eq!(n.len(), MAX_COMPONENT_BYTES);
        assert!(n.starts_with("Wang - 2020 - 深"));

        // With a generous character cap the byte cap still holds on a boundary.
        let options = FilenameOptions {
            title_max_chars: 1000,
            max_chars: 1000,
            ..FilenameOptions::default()
        };
        let n = suggest_filename(&record(&[], None, &cjk), &options);
        assert!(n.len() <= MAX_COMPONENT_BYTES, "{} bytes", n.len());
        assert_eq!(n.chars().filter(|&c| c == '深').count(), 83);
        assert!(n.ends_with("深.pdf"));

        // Emoji (4 bytes) and a very long ASCII title.
        let n = suggest_filename(&record(&[], None, &"🧬".repeat(300)), &options);
        assert!(n.len() <= MAX_COMPONENT_BYTES);
        let n = name(&["A B"], Some(1999), &"long ".repeat(200));
        assert!(n.chars().count() <= 150);
        assert!(n.ends_with("long.pdf"), "{n}");
    }

    #[test]
    fn long_titles_are_cut_at_a_word_when_that_costs_little() {
        // 95 characters, then a 20-character word that the 100-character cap would split.
        let title = format!("{} supercalifragilistic", "word ".repeat(19));
        assert_eq!(
            name(&[], None, &title),
            format!("{}.pdf", "word ".repeat(19).trim_end())
        );
        // Backing up would lose too much (the only space is early): cut hard.
        let title = format!("ab {}", "x".repeat(200));
        assert_eq!(name(&[], None, &title).len(), 100 + ".pdf".len());
    }

    #[test]
    fn cut_does_not_strand_a_combining_mark() {
        // `q` + combining acute has no precomposed form.
        let s = "abcq\u{301}def";
        assert_eq!(cut(s, 4, usize::MAX), "abc");
        assert_eq!(cut(s, 5, usize::MAX), "abcq\u{301}");
        assert_eq!(cut(s, 100, usize::MAX), s);
    }

    #[test]
    fn all_caps_title_is_left_alone() {
        // Titles carry acronyms (DNA, MRI); only author names are un-shouted.
        assert_eq!(
            name(&["JOHN SMITH"], Some(2001), "DEEP LEARNING FOR MRI"),
            "Smith - 2001 - DEEP LEARNING FOR MRI.pdf"
        );
    }

    #[test]
    fn parts_are_configurable_and_reorderable() {
        let options = FilenameOptions {
            parts: vec![Part::Year, Part::Title],
            ..FilenameOptions::default()
        };
        let r = record(&["Yann LeCun"], Some(2015), "Deep learning");
        assert_eq!(suggest_filename(&r, &options), "2015 - Deep learning.pdf");
        let none = FilenameOptions {
            parts: vec![],
            ..FilenameOptions::default()
        };
        assert_eq!(suggest_filename(&r, &none), "Untitled.pdf");
    }

    #[test]
    fn collision_suffix_is_deterministic_and_case_insensitive() {
        let taken: Vec<String> = ["A - 2020 - T.pdf", "a - 2020 - t (2).pdf"]
            .iter()
            .map(|n| fold_key(n))
            .collect();
        let is_taken = |n: &str| taken.contains(&fold_key(n));
        assert_eq!(
            unique_name("A - 2020 - T.pdf", is_taken).as_deref(),
            Some("A - 2020 - T (3).pdf")
        );
        assert_eq!(
            unique_name("Other.pdf", is_taken).as_deref(),
            Some("Other.pdf")
        );
        // Decomposed and composed spellings are the same file on macOS.
        assert_eq!(fold_key("Cafe\u{301}.PDF"), fold_key("caf\u{e9}.pdf"));
    }

    #[test]
    fn collision_suffix_keeps_the_byte_limit() {
        let long = "深".repeat(90);
        let options = FilenameOptions {
            title_max_chars: 1000,
            max_chars: 1000,
            ..FilenameOptions::default()
        };
        let first = suggest_filename(&record(&[], None, &long), &options);
        let second = unique_name(&first, |n| n == first).unwrap();
        assert_ne!(first, second);
        assert!(second.len() <= MAX_COMPONENT_BYTES, "{}", second.len());
        assert!(second.ends_with(" (2).pdf"));
        // A predicate that is always true terminates with None.
        assert_eq!(unique_name("x.pdf", |_| true), None);
    }
}

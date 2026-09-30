//! Name and title text: one repair for display and one fold for matching.
//!
//! [`repair_display`] mends what PDF text extraction does to accented letters
//! (a spacing accent printed beside its letter, a dotless `ı` under a
//! combining accent, a combining mark before its letter) and never invents a
//! letter; U+FFFD stays where it is, see [`is_damaged`]. [`match_key`] is the
//! only fold used to compare names and titles (engine evaluation, database
//! matching, verification); [`match_keys`] adds transliteration alternates
//! (`ü`/`ue`) for matching only. [`family_name`] picks the surname of a
//! printed personal name.

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::{compose, is_combining_mark};

/// The combining mark a non-ASCII spacing (standalone) accent stands for:
/// the Latin-1 and Spacing Modifier accents as PDF text layers decode the
/// accent glyphs (`acute`, `dieresis`, `circumflex`, `caron`, ...). The
/// ASCII grave, `^` and `~` are left out: in text they are code and math
/// far more often than accents.
pub fn spacing_accent_mark(spacing: char) -> Option<char> {
    Some(match spacing {
        '\u{00A8}' => '\u{0308}',              // ¨ diaeresis
        '\u{00B4}' | '\u{02CA}' => '\u{0301}', // ´ ˊ acute
        '\u{02CB}' => '\u{0300}',              // ˋ grave
        '\u{02C6}' => '\u{0302}',              // ˆ circumflex
        '\u{02DC}' => '\u{0303}',              // ˜ tilde
        '\u{00B8}' => '\u{0327}',              // ¸ cedilla
        '\u{02C7}' => '\u{030C}',              // ˇ caron
        '\u{02D8}' => '\u{0306}',              // ˘ breve
        '\u{02D9}' => '\u{0307}',              // ˙ dot above
        '\u{02DA}' => '\u{030A}',              // ˚ ring above
        '\u{02DD}' => '\u{030B}',              // ˝ double acute
        '\u{00AF}' => '\u{0304}',              // ¯ macron
        '\u{02DB}' => '\u{0328}',              // ˛ ogonek
        _ => return None,
    })
}

/// `base` with `mark` as one precomposed letter, when Unicode has one. The
/// dotless `ı`/`ȷ` take their accent as `i`/`j` do.
fn composed(base: char, mark: char) -> Option<char> {
    let base = match base {
        'ı' => 'i',
        'ȷ' => 'j',
        other => other,
    };
    compose(base, mark)
}

/// The display form of a name or title: accents extraction split from their
/// letters are rejoined, then NFC.
///
/// * A spacing accent ([`spacing_accent_mark`]) next to a letter it
///   composes with, before it (`M¨uller`) or after it (`Mu¨ller`), with or
///   without one space between them, becomes that precomposed letter. The
///   letter after the accent is tried first (the order `LaTeX` fonts draw).
/// * A dotless `ı`/`ȷ` followed by a combining accent is `i`/`j` with it
///   (`Kovařı́k` → `Kovařík`): NFC alone leaves it apart.
/// * A combining mark that cannot sit on the letter before it but composes
///   with the letter after it moves there.
///
/// Anything that composes with nothing is left exactly as it was, and
/// U+FFFD is kept: the lost letter is not guessed.
pub fn repair_display(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(mark) = spacing_accent_mark(c) {
            // The letter after the accent, one space allowed between.
            let next = match chars.get(i + 1) {
                Some(' ') => Some(i + 2),
                Some(_) => Some(i + 1),
                None => None,
            };
            if let Some(k) = next
                && let Some(&base) = chars.get(k)
                && let Some(letter) = composed(base, mark)
            {
                out.push(letter);
                i = k + 1;
                continue;
            }
            // The letter before it, again allowing one space.
            let space_before = out.last() == Some(&' ');
            let at = out.len().checked_sub(1 + usize::from(space_before));
            if let Some(at) = at
                && let Some(letter) = composed(out[at], mark)
            {
                out.truncate(at);
                out.push(letter);
                i += 1;
                continue;
            }
            out.push(c);
            i += 1;
            continue;
        }
        if is_combining_mark(c) {
            if let Some(&base) = out.last()
                && let Some(letter) = composed(base, c)
            {
                out.pop();
                out.push(letter);
                i += 1;
                continue;
            }
            if let Some(&next) = chars.get(i + 1)
                && let Some(letter) = composed(next, c)
            {
                out.push(letter);
                i += 2;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out.into_iter().collect::<String>().nfc().collect()
}

/// Does `s` contain U+FFFD, a character the PDF text layer lost?
pub fn is_damaged(s: &str) -> bool {
    s.contains('\u{FFFD}')
}

/// Letters that keep no mark to strip under NFKD.
fn special_fold(c: char) -> Option<&'static str> {
    Some(match c {
        'ø' | 'Ø' => "o",
        'ß' | 'ẞ' => "ss",
        'æ' | 'Æ' => "ae",
        'œ' | 'Œ' => "oe",
        'đ' | 'Đ' | 'ð' | 'Ð' => "d",
        'ł' | 'Ł' => "l",
        'þ' | 'Þ' => "th",
        'ı' => "i",
        'ȷ' => "j",
        _ => return None,
    })
}

/// Apostrophes vanish in a key (`O’Brien`, `O'Brien` and `OBrien` meet);
/// every other non-alphanumeric run is one space.
fn is_apostrophe(c: char) -> bool {
    matches!(c, '\'' | '’' | 'ʼ' | '‘' | '`' | '´')
}

/// The comparison key of a name or title: [`repair_display`], NFKD,
/// combining marks dropped, lower-cased, the letters of [`special_fold`]
/// spelled out, apostrophes removed and any other run of punctuation or
/// whitespace one space, trimmed. `Kovařı́k`, `Kovařík` and `KOVARIK` share
/// the key `kovarik`.
pub fn match_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut gap = false;
    for c in repair_display(s).nfkd() {
        if is_combining_mark(c) || is_apostrophe(c) {
            continue;
        }
        if !c.is_alphanumeric() {
            gap = true;
            continue;
        }
        if gap && !out.is_empty() {
            out.push(' ');
        }
        gap = false;
        match special_fold(c) {
            Some(folded) => out.push_str(folded),
            None => out.extend(c.to_lowercase()),
        }
    }
    out
}

/// German umlaut transliterations, both ways: `ü` is also spelled `ue`, and
/// a printed `ue` may stand for `ü` (so `Mueller` meets `Muller`).
const TRANSLITERATIONS: [(&str, &str); 3] = [("ü", "ue"), ("ö", "oe"), ("ä", "ae")];

/// [`match_key`] first, then the distinct keys of the umlaut
/// transliterations of `s` ([`TRANSLITERATIONS`]): `Müller`, `Mueller` and
/// `Muller` each share a key with the other two. For matching only; never
/// display an alternate.
pub fn match_keys(s: &str) -> Vec<String> {
    let lower: String = repair_display(s).to_lowercase();
    let mut spelled = lower.clone();
    let mut contracted = lower;
    for (umlaut, digraph) in TRANSLITERATIONS {
        spelled = spelled.replace(umlaut, digraph);
        contracted = contracted.replace(digraph, &digraph[..1]);
    }
    let mut keys = vec![match_key(s)];
    for alternate in [spelled, contracted] {
        let key = match_key(&alternate);
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// Do two names or titles share a key of [`match_keys`]?
pub fn keys_meet(a: &str, b: &str) -> bool {
    let left = match_keys(a);
    match_keys(b).iter().any(|key| left.contains(key))
}

/// Lower-case surname particles that belong to the family name
/// (`van der Berg`, `de la Cruz`, `al-Farabi`).
const PARTICLES: [&str; 22] = [
    "van", "von", "der", "den", "de", "del", "della", "di", "da", "dos", "das", "do", "du", "le",
    "la", "ten", "ter", "bin", "ibn", "al", "el", "zu",
];

/// Generational suffixes that follow a name without being part of it.
const SUFFIXES: [&str; 6] = ["jr", "sr", "ii", "iii", "iv", "jnr"];

fn is_particle(token: &str) -> bool {
    let bare = token.trim_end_matches(['-', '’', '\'']);
    PARTICLES.contains(&bare.to_lowercase().as_str())
}

fn is_suffix(token: &str) -> bool {
    SUFFIXES.contains(&token.trim_end_matches(['.', ',']).to_lowercase().as_str())
}

/// Is `token` a given-name initial (`J.`, `J.-P.`, `Th.`, `JP`)?
fn is_initial(token: &str) -> bool {
    let letters: Vec<char> = token.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.is_empty() {
        return false;
    }
    if token.contains('.') {
        // `J.`, `J.-P.`, and transliterated digraphs `Th.`, `Yu.`, `Zh.`.
        return token
            .split(['.', '-'])
            .filter(|part| !part.is_empty())
            .all(|part| part.chars().count() <= 2 && part.starts_with(char::is_uppercase));
    }
    letters.len() <= 3 && letters.iter().all(|c| c.is_uppercase())
}

/// The family name of a printed personal name, as printed (fold it with
/// [`match_key`] to compare).
///
/// * `Surname, Given` (a comma after the first name part): everything
///   before the comma, generational suffixes dropped (`de la Cruz, María`,
///   `García Márquez, Gabriel`, `King, Jr., Martin Luther`).
/// * `Surname AB` (Vancouver): the words before the trailing initials.
/// * `Given Surname`: the last word, with the lower-case particles before it
///   (`Ludwig van Beethoven` → `van Beethoven`); initials and suffixes are
///   skipped (`J.-P. Serre`, `Sammy Davis Jr.`).
///
/// Without a comma, a given-first double surname (`Gabriel García
/// Márquez`) cannot be told from a middle name, nor a surname-first order
/// (Hungarian `Erdős Pál`, romanised `Zhang Wei`) from a given-first one;
/// the last word is returned. Empty for an empty name.
pub fn family_name(name: &str) -> String {
    let name = name.trim();
    if let Some((before, _)) = name.split_once(',') {
        let tokens: Vec<&str> = before.split_whitespace().collect();
        let surname = tokens
            .iter()
            .copied()
            .filter(|t| !is_suffix(t))
            .collect::<Vec<&str>>()
            .join(" ");
        // `J., Smith` would be initials before the comma; `LI, X.` is not.
        if !surname.is_empty() && !tokens.iter().all(|t| t.contains('.') && is_initial(t)) {
            return surname;
        }
    }
    let tokens: Vec<&str> = name
        .split([' ', ','])
        .filter(|t| !t.is_empty() && !is_suffix(t))
        .collect();
    // Vancouver `Smith AB`, `van der Berg JP`: word(s) then trailing initials.
    if tokens.len() >= 2
        && tokens
            .last()
            .is_some_and(|t| !t.contains('.') && is_initial(t))
        && !is_initial(tokens[0])
    {
        let end = tokens
            .iter()
            .rposition(|t| !is_initial(t))
            .map_or(0, |k| k + 1);
        return tokens[..end].join(" ");
    }
    let Some(last) = tokens.iter().rposition(|t| !is_initial(t)) else {
        return tokens.last().map_or_else(String::new, |t| (*t).to_string());
    };
    let mut start = last;
    while start > 0
        && is_particle(tokens[start - 1])
        && tokens[start - 1].starts_with(char::is_lowercase)
    {
        start -= 1;
    }
    tokens[start..=last].join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repairs_dotless_i_under_a_combining_accent() {
        // Real extraction output seen in the arXiv dev corpus (docs/EVAL.md).
        for (printed, repaired) in [
            ("Kovar\u{030C}\u{0131}\u{0301}k", "Kovařík"),
            ("Beno\u{0131}\u{0302}t", "Benoît"),
            ("Z\u{030C}\u{0131}\u{0301}dek", "Žídek"),
            ("Mar\u{0131}\u{0301}a", "María"),
        ] {
            assert_eq!(repair_display(printed), repaired, "{printed:?}");
        }
    }

    #[test]
    fn joins_spacing_accents_in_either_order_and_across_one_space() {
        for (printed, repaired) in [
            ("M¨uller", "Müller"),
            ("Mu¨ller", "Müller"),
            ("M ¨uller", "M üller"),
            ("Mu ¨ller", "Müller"),
            ("Erd˝os", "Erdős"),
            ("G¨odel", "Gödel"),
            ("Dvoˇr´ak", "Dvořák"),
            ("Fran¸cois", "François"),
            ("Pe˜na", "Peña"),
            ("Lukasiewicz ˛a", "Lukasiewicz ą"),
            ("Ang´el", "Angél"),
            ("Nguy˜ên", "Nguyễn"),
        ] {
            assert_eq!(repair_display(printed), repaired, "{printed:?}");
        }
    }

    #[test]
    fn moves_a_combining_mark_to_the_letter_it_composes_with() {
        // The accent glyph drawn first arrives before its letter.
        assert_eq!(repair_display("M\u{0308}uller"), "Müller");
        assert_eq!(repair_display("Schro\u{0308}dinger"), "Schrödinger");
    }

    #[test]
    fn never_invents_or_drops_characters() {
        for text in [
            "Bory\u{FFFD}lo",
            "x ^ 2 ~ y",
            "O’Brien",
            "Łukasiewicz",
            "Ørsted",
            "`quoted'",
            "`add` and `a`",
            "¨",
            "",
            "Nguyễn Văn Thành",
            "Сергей Брин",
            "Γιώργος",
            "王小明",
        ] {
            let repaired = repair_display(text);
            assert_eq!(repaired, text.nfc().collect::<String>(), "{text:?}");
        }
        assert!(is_damaged(&repair_display("Bory\u{FFFD}lo")));
        assert!(!is_damaged("Boryło"));
    }

    #[test]
    fn match_key_folds_marks_case_letters_and_punctuation() {
        let same = [
            ("Kovar\u{030C}\u{0131}\u{0301}k", "Kovarik"),
            ("KOVAŘÍK", "kovarik"),
            ("Ørsted", "Orsted"),
            ("Łukasiewicz", "Lukasiewicz"),
            ("Straße", "Strasse"),
            ("Æsir", "Aesir"),
            ("Œuvre", "oeuvre"),
            ("Đorđević", "Dordevic"),
            ("Þórður Guðmundsson", "Thordur Gudmundsson"),
            ("O’Brien", "OBrien"),
            ("O'Brien", "O’Brien"),
            ("Jean-Pierre", "Jean Pierre"),
            ("Al-Khwārizmī", "al khwarizmi"),
            ("Erdős", "Erdos"),
            ("Nguyễn Văn Thành", "nguyen van thanh"),
            ("Yıldız", "Yildiz"),
            ("Çağrı Gülçehre", "Cagri Gulcehre"),
            ("João Gonçalves", "Joao Goncalves"),
            ("Ångström", "angstrom"),
        ];
        for (a, b) in same {
            assert_eq!(match_key(a), match_key(b), "{a} / {b}");
        }
        assert_eq!(match_key("  Smith,  J.  "), "smith j");
        assert_eq!(match_key("ﬁne"), "fine");
        assert_ne!(match_key("Muller"), match_key("Mueller"));
    }

    #[test]
    fn umlaut_transliterations_meet_both_ways() {
        for (a, b) in [
            ("Müller", "Mueller"),
            ("Müller", "Muller"),
            ("Mueller", "Muller"),
            ("Schrödinger", "Schroedinger"),
            ("Gödel", "Goedel"),
            ("Bär", "Baer"),
        ] {
            assert!(keys_meet(a, b), "{a} / {b}");
            assert!(keys_meet(b, a), "{b} / {a}");
        }
        assert!(!keys_meet("Müller", "Miller"));
        assert_eq!(match_keys("Smith"), vec!["smith".to_string()]);
    }

    #[test]
    fn family_name_handles_order_particles_initials_and_suffixes() {
        for (name, family) in [
            ("Smith, J.", "Smith"),
            ("J. Smith", "Smith"),
            ("Smith J", "Smith"),
            ("Smith AB", "Smith"),
            ("van der Berg, J.", "van der Berg"),
            ("J. van der Berg", "van der Berg"),
            ("Ludwig van Beethoven", "van Beethoven"),
            ("Guido van Rossum", "van Rossum"),
            ("Charles de Gaulle", "de Gaulle"),
            ("Oscar de la Hoya", "de la Hoya"),
            ("de la Cruz, María", "de la Cruz"),
            ("García Márquez, Gabriel", "García Márquez"),
            ("Gabriel García Márquez", "Márquez"),
            ("J.-P. Serre", "Serre"),
            ("Serre, J.-P.", "Serre"),
            ("Th. Hofmann", "Hofmann"),
            ("Yu. I. Manin", "Manin"),
            ("King, Jr., Martin Luther", "King"),
            ("Martin Luther King Jr.", "King"),
            ("Muhammad ibn Musa al-Khwarizmi", "al-Khwarizmi"),
            ("Mohamed bin Zayed", "bin Zayed"),
            ("Johann Wolfgang von Goethe", "von Goethe"),
            ("Vigdís Finnbogadóttir", "Finnbogadóttir"),
            ("LI, X.", "LI"),
            ("Van Rossum, G.", "Van Rossum"),
            ("", ""),
        ] {
            assert_eq!(family_name(name), family, "{name:?}");
        }
    }
}

//! Bounds applied before lopdf constructs font decoders (including its eager
//! reverse `ToUnicode` map). Charges are component accounting, not RSS estimates.

use super::content::{Halt, hex_value, is_regular, lex_object, skip_space, skip_while};
use super::{Dictionary, Document, LopdfError, Object, Stream, charge, form_decode_policy};

pub(super) const MAX_FONT_CACHE_BYTES: usize = 64 * 1024 * 1024;
pub(super) const MAX_FONT_CACHE_ENTRIES: usize = 512;
pub(super) const MIN_FONT_CHARGE: usize = 16 * 1024;
pub(super) const MAX_FONT_STREAM: usize = 8 * 1024 * 1024;
#[cfg(feature = "pdf-extract")]
pub(super) const MAX_CFF_STREAM: usize = 256 * 1024;
const MAX_FONT_ITEMS: usize = 65_536;
// Cover width/encoding construction, including CID interval sweep scratch:
// two endpoints, one heap entry, and up to two output runs per source run can
// coexist with the parsed input. Charge before constructing those allocations.
const FONT_ITEM_CHARGE: usize = 128;
const MAX_NAME: usize = 1024;
const MAX_CMAP_CODES: u64 = 65_536;
const MAX_CMAP_CHARGE: u64 = 32 * 1024 * 1024;

pub(super) struct FontWork {
    bytes: usize,
    loads: usize,
}

impl Default for FontWork {
    fn default() -> Self {
        Self {
            bytes: 64 * 1024 * 1024,
            loads: 4096,
        }
    }
}

impl FontWork {
    pub(super) fn reserve(&mut self, bytes: usize) -> Result<(), &'static str> {
        charge(&mut self.bytes, bytes)
            .then_some(())
            .ok_or("font allocation/work budget exceeded")
    }

    pub(super) fn load(&mut self) -> Result<(), &'static str> {
        charge(&mut self.loads, 1)
            .then_some(())
            .ok_or("font load count exceeded")
    }
}

/// Filter/predictor checks happen before decoding. Reserve for both this
/// preflight and the bounded decoder's later pass; refunds apply only when
/// intermediates and predictor scratch cannot outgrow the final output.
fn stream_bytes(stream: &Stream, work: &mut FontWork) -> Result<Option<Vec<u8>>, &'static str> {
    let policy = form_decode_policy(stream).ok_or("font stream filter/predictor limit exceeded")?;
    let layers = policy.layers * 2;
    let limit = MAX_FONT_STREAM.min(work.bytes / layers);
    if limit == 0 || stream.content.len() > limit {
        return Err("font encoded stream byte limit exceeded");
    }
    work.reserve(limit * layers)?;
    match stream.get_plain_content_with_limit(limit) {
        Ok(bytes) => {
            if policy.layers == 1 && !policy.uses_predictor {
                work.bytes += (limit - bytes.len().max(stream.content.len())) * layers;
            }
            Ok(Some(bytes))
        }
        Err(LopdfError::Decompress(lopdf::DecompressError::MemoryLimitExceeded { .. })) => {
            Err("font decoded stream byte limit exceeded")
        }
        // Existing malformed-font fallback is retained, but never for a limit.
        Err(_) => Ok(None),
    }
}

fn array_charge(doc: &Document, array: &[Object], work: &mut FontWork) -> Result<(), &'static str> {
    if array.len() > MAX_FONT_ITEMS {
        return Err("font widths/encoding item limit exceeded");
    }
    work.reserve(array.len() * FONT_ITEM_CHARGE)?;
    let mut entries = array.len();
    for item in array {
        let Ok((_, item)) = doc.dereference(item) else {
            continue;
        };
        match item {
            Object::Array(list) => {
                entries = entries.saturating_add(list.len());
                if entries > MAX_FONT_ITEMS {
                    return Err("font widths/encoding item limit exceeded");
                }
                work.reserve(list.len() * FONT_ITEM_CHARGE)?;
            }
            Object::Name(name) if name.len() > MAX_NAME => {
                return Err("font name byte limit exceeded");
            }
            Object::Name(name) => work.reserve(name.len())?,
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn preflight(
    doc: &Document,
    dict: &Dictionary,
    work: &mut FontWork,
) -> Result<usize, &'static str> {
    let before = work.bytes;
    work.reserve(MIN_FONT_CHARGE)?;
    for key in [b"BaseFont".as_slice(), b"Encoding"] {
        if let Ok(Object::Name(name)) = dict.get_deref(key, doc)
            && name.len() > MAX_NAME
        {
            return Err("font name byte limit exceeded");
        }
    }
    let composite = dict
        .get(b"Subtype")
        .and_then(Object::as_name)
        .is_ok_and(|name| name == b"Type0");
    if !composite && let Ok(Object::Array(widths)) = dict.get_deref(b"Widths", doc) {
        array_charge(doc, widths, work)?;
    }
    if composite
        && let Ok(Object::Array(descendants)) = dict.get_deref(b"DescendantFonts", doc)
        && let Some(first) = descendants.first()
        && let Ok((_, Object::Dictionary(cid))) = doc.dereference(first)
        && let Ok(Object::Array(widths)) = cid.get_deref(b"W", doc)
    {
        array_charge(doc, widths, work)?;
    }
    if !composite
        && dict.get(b"ToUnicode").is_err()
        && let Ok(Object::Dictionary(encoding)) = dict.get_deref(b"Encoding", doc)
        && let Ok(Object::Array(differences)) = encoding.get_deref(b"Differences", doc)
    {
        array_charge(doc, differences, work)?;
    }
    // Simple fonts always prefer ToUnicode to their rendering Encoding.
    // Composite decoding uses it only for absent/Identity encodings.
    let encoding = dict.get_deref(b"Encoding", doc);
    let uses_cmap = !composite
        || dict.get(b"Encoding").is_err()
        || encoding
            .is_ok_and(|object| matches!(object.as_name(), Ok(b"Identity-H" | b"Identity-V")));
    if uses_cmap
        && (!composite || dict.has_type(b"Font"))
        && let Ok(Object::Stream(stream)) = dict.get_deref(b"ToUnicode", doc)
        && let Some(bytes) = stream_bytes(stream, work)?
    {
        work.reserve(cmap_charge(&bytes)?)?;
        if !composite && matches!(dict.get(b"ToUnicode"), Ok(Object::Stream(_))) {
            // The bounded upstream parser receives a tiny font dictionary;
            // a direct stream (rather than a reference) is copied into it.
            work.reserve(stream.content.len())?;
        }
    }
    if uses_embedded_program(doc, dict)
        && let Ok(Object::Dictionary(descriptor)) = dict.get_deref(b"FontDescriptor", doc)
        && let Ok(Object::Stream(stream)) = descriptor.get_deref(b"FontFile", doc)
        && let Some(bytes) = stream_bytes(stream, work)?
    {
        // Type1 token slices, copied glyph names, lookup work, and input bytes.
        // The binary eexec section is not tokenized by the existing decoder.
        let clear_end = bytes
            .windows(5)
            .position(|w| w == b"eexec")
            .unwrap_or(bytes.len());
        work.reserve(clear_end.saturating_mul(32))?;
    }
    #[cfg(feature = "pdf-extract")]
    if let Some(stream) = embedded_cff(doc, dict) {
        let policy = form_decode_policy(stream).ok_or("CFF filter limit exceeded")?;
        // CFF is a byte program, not image pixels; predictor scratch is not
        // needed and could exceed the decoded program cap.
        if policy.uses_predictor || stream.content.len() > MAX_CFF_STREAM {
            return Err("CFF encoded stream/predictor limit exceeded");
        }
        work.reserve(MAX_CFF_STREAM * policy.layers * 2)?;
        match stream.get_plain_content_with_limit(MAX_CFF_STREAM) {
            Ok(bytes) => {
                // Two controlled probes, input copies, maps and up to 256
                // lookups through the font's source tables. Charged before
                // constructing the foreign parser or any probe document.
                work.reserve(bytes.len().saturating_mul(128) + 1024 * 1024)?;
            }
            Err(LopdfError::Decompress(lopdf::DecompressError::MemoryLimitExceeded { .. })) => {
                return Err("CFF decoded stream byte limit exceeded");
            }
            Err(_) => {}
        }
    }
    Ok(before - work.bytes)
}

/// Only an actually selected simple Type1 font without an authoritative
/// PDF-level mapping can use its embedded CFF encoding. No source resources,
/// page content or recursive font dictionaries enter the recovery probe.
pub(super) fn embedded_cff<'a>(doc: &'a Document, dict: &'a Dictionary) -> Option<&'a Stream> {
    if dict.get(b"ToUnicode").is_ok()
        || dict.get(b"Encoding").is_ok()
        || !dict
            .get(b"Subtype")
            .and_then(Object::as_name)
            .is_ok_and(|s| s == b"Type1")
    {
        return None;
    }
    let descriptor = dict
        .get_deref(b"FontDescriptor", doc)
        .ok()?
        .as_dict()
        .ok()?;
    let stream = descriptor
        .get_deref(b"FontFile3", doc)
        .ok()?
        .as_stream()
        .ok()?;
    stream
        .dict
        .get(b"Subtype")
        .and_then(Object::as_name)
        .is_ok_and(|s| s == b"Type1C")
        .then_some(stream)
}

/// Follow the same fallback routes as `own_table` and `builtin_entries`.
/// A named encoding, an available `ToUnicode` map, a known Differences base, or
/// a complete built-in TeX table never inspects `FontFile`; checking it anyway
/// would make an irrelevant resource prevent otherwise valid extraction.
fn uses_embedded_program(doc: &Document, dict: &Dictionary) -> bool {
    use super::{BASE_ENCODINGS, TexEncoding, tex_encoding};
    if dict.get(b"ToUnicode").is_ok() || embedded_cff(doc, dict).is_some() {
        return false;
    }
    if matches!(
        dict.get(b"Subtype").and_then(Object::as_name),
        Ok(b"Type0" | b"Type3")
    ) {
        return false;
    }
    match dict.get_deref(b"Encoding", doc) {
        Ok(Object::Dictionary(encoding)) => {
            if encoding
                .get_deref(b"BaseEncoding", doc)
                .and_then(Object::as_name)
                .is_ok_and(|name| BASE_ENCODINGS.contains(&name))
            {
                return false;
            }
        }
        Ok(_) => return false,
        Err(_) => {
            if dict
                .get_deref(b"ToUnicode", doc)
                .and_then(Object::as_stream)
                .is_ok()
            {
                return false;
            }
        }
    }
    !matches!(
        dict.get(b"BaseFont")
            .and_then(Object::as_name)
            .ok()
            .and_then(tex_encoding),
        Some(TexEncoding::Ot1 | TexEncoding::Ot1Italic | TexEncoding::Oml | TexEncoding::Oms)
    )
}

/// A non-allocating scan counts actual mappings, regardless of declared section
/// counts. Source cardinality and Unicode payload are charged before the eager
/// upstream reverse-map constructor can iterate any range. Metadata is skipped
/// with the depth-bounded PDF lexer; names, strings and comments cannot introduce
/// mapping sections. Unknown/malformed mapping syntax fails closed.
fn cmap_charge(bytes: &[u8]) -> Result<usize, &'static str> {
    let mut scan = CmapScan { bytes, pos: 0 };
    let mut codes = 0u64;
    let mut charge = bytes.len() as u64 * 32;
    let mut arrays = ArrayRanges::default();
    while scan.space() < bytes.len() {
        if scan.word(b"beginbfchar") {
            while !scan.word(b"endbfchar") {
                let (code, len) = scan.hex(4)?;
                let (_, target) = scan.hex(512)?;
                account(&mut codes, &mut charge, 1, target)?;
                arrays.insert(code, code, len, 0, &mut charge)?;
            }
        } else if scan.word(b"beginbfrange") {
            while !scan.word(b"endbfrange") {
                let (first, first_len) = scan.hex(4)?;
                let (last, last_len) = scan.hex(4)?;
                if first_len != last_len || first > last {
                    return Err("ToUnicode range preflight rejected invalid syntax");
                }
                let count = u64::from(last) - u64::from(first) + 1;
                if count > MAX_CMAP_CODES - codes {
                    return Err("ToUnicode source-code cardinality limit exceeded");
                }
                let mut array_bytes = 0;
                let max_target = if scan.take(b'[') {
                    let mut max_target = 0;
                    let mut entries = 0;
                    while !scan.take(b']') {
                        let (_, size) = scan.hex(512)?;
                        entries += 1;
                        if entries > MAX_FONT_ITEMS {
                            return Err("ToUnicode target array limit exceeded");
                        }
                        max_target = max_target.max(size);
                        array_bytes += (size + 64) as u64 * 4;
                        charge += (size + 64) as u64 * 4;
                        if charge > MAX_CMAP_CHARGE {
                            return Err("ToUnicode mapping allocation limit exceeded");
                        }
                    }
                    max_target
                } else {
                    scan.hex(512)?.1
                };
                account(&mut codes, &mut charge, count, max_target)?;
                arrays.insert(first, last, first_len, array_bytes, &mut charge)?;
            }
        } else if scan.epilogue() {
            break;
        } else {
            match lex_object(bytes, scan.pos, 16, false, false) {
                Ok((end, _)) => scan.pos = end,
                Err(Halt::Fatal) => return Err("ToUnicode metadata nesting limit exceeded"),
                Err(Halt::Stop) => {
                    let end = skip_while(bytes, scan.pos, is_regular);
                    if end == scan.pos {
                        return Err("ToUnicode preflight rejected unsupported syntax");
                    }
                    scan.pos = end;
                }
            }
        }
    }
    if charge > MAX_CMAP_CHARGE {
        return Err("ToUnicode mapping allocation limit exceeded");
    }
    Ok(charge as usize)
}

/// lopdf's range map clones an entire array-valued target when a later mapping
/// splits its interval. A short succession of overrides can therefore do much
/// more work than its source cardinality suggests. Model the surviving array
/// intervals and reserve for both possible clones before invoking that parser.
/// Adjoining values can also trigger deep equality checks when coalescing, so
/// charge them even without an overlap. The preflight's own scan is bounded.
#[derive(Default)]
struct ArrayRanges {
    ranges: Vec<(u32, u32, usize, u64)>,
    comparisons: usize,
}

impl ArrayRanges {
    fn insert(
        &mut self,
        first: u32,
        last: u32,
        len: usize,
        payload: u64,
        charge: &mut u64,
    ) -> Result<(), &'static str> {
        let before = self.ranges.len();
        let mut split = Vec::new();
        for (lo, hi, code_len, bytes) in &mut self.ranges {
            self.comparisons += 1;
            if self.comparisons > 1_000_000 {
                return Err("ToUnicode interval preflight work limit exceeded");
            }
            if *code_len != len || *hi < first.saturating_sub(1) || last.saturating_add(1) < *lo {
                continue;
            }
            *charge += 2 * *bytes;
            if *charge > MAX_CMAP_CHARGE {
                return Err("ToUnicode array split/copy work limit exceeded");
            }
            if *hi < first || last < *lo {
                continue;
            }
            let left = *lo < first;
            let right = *hi > last;
            match (left, right) {
                (true, true) => {
                    split.push((last + 1, *hi, len, *bytes));
                    *hi = first - 1;
                }
                (true, false) => *hi = first - 1,
                (false, true) => *lo = last + 1,
                (false, false) => *bytes = 0,
            }
        }
        // At most one surviving interval can split for a single insertion.
        // Keeping this as a vector also tolerates adjacent equal values which
        // upstream may coalesce; tracking both pieces only overcharges work.
        debug_assert!(split.len() <= before);
        self.ranges.retain(|range| range.3 != 0);
        self.ranges.extend(split);
        if payload > 0 {
            self.ranges.push((first, last, len, payload));
        }
        Ok(())
    }
}

fn account(
    codes: &mut u64,
    charge: &mut u64,
    count: u64,
    target: usize,
) -> Result<(), &'static str> {
    *codes += count;
    *charge += count * (256 + 4 * target as u64);
    if *codes > MAX_CMAP_CODES {
        Err("ToUnicode source-code cardinality limit exceeded")
    } else if *charge > MAX_CMAP_CHARGE {
        Err("ToUnicode mapping allocation limit exceeded")
    } else {
        Ok(())
    }
}

struct CmapScan<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl CmapScan<'_> {
    fn space(&mut self) -> usize {
        self.pos = skip_space(self.bytes, self.pos);
        self.pos
    }
    fn take(&mut self, byte: u8) -> bool {
        self.space();
        if self.bytes.get(self.pos) == Some(&byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn word(&mut self, word: &[u8]) -> bool {
        self.space();
        let end = skip_while(self.bytes, self.pos, is_regular);
        if &self.bytes[self.pos..end] == word {
            self.pos = end;
            true
        } else {
            false
        }
    }
    /// lopdf's `CMap` parser returns after this complete outer epilogue and does
    /// not consume a trailer. Some real font streams have binary padding there.
    /// Only recognize this at token boundaries outside strings/dictionaries and
    /// mapping sections: every mapping the decoder can reach was charged first.
    /// An incomplete epilogue is not permission to discard unscanned input.
    fn epilogue(&self) -> bool {
        let mut tail = Self {
            bytes: self.bytes,
            pos: self.pos,
        };
        tail.word(b"endcmap")
            && tail.word(b"CMapName")
            && tail.word(b"currentdict")
            && tail.take(b'/')
            && tail.word(b"CMap")
            && tail.word(b"defineresource")
            && tail.word(b"pop")
            && tail.word(b"end")
            && tail.word(b"end")
    }

    /// Returns the source value (when <=4 bytes) and byte length. No target
    /// payload is allocated; long strings are rejected while scanning.
    fn hex(&mut self, max: usize) -> Result<(u32, usize), &'static str> {
        if !self.take(b'<') {
            return Err("ToUnicode mapping preflight rejected invalid syntax");
        }
        let mut digits = 0;
        let mut value = 0u32;
        loop {
            self.space();
            let Some(&byte) = self.bytes.get(self.pos) else {
                return Err("ToUnicode mapping preflight rejected invalid syntax");
            };
            self.pos += 1;
            if byte == b'>' {
                break;
            }
            if !byte.is_ascii_hexdigit() {
                return Err("ToUnicode mapping preflight rejected invalid syntax");
            }
            digits += 1;
            if digits > 2 * max {
                return Err("ToUnicode mapping string byte limit exceeded");
            }
            value = value.wrapping_mul(16) + u32::from(hex_value(byte));
        }
        if digits == 0 || digits % 2 != 0 {
            return Err("ToUnicode mapping preflight rejected invalid syntax");
        }
        Ok((value, digits / 2))
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use super::*;

    #[test]
    fn width_and_differences_items_reserve_construction_space() {
        let mut doc = Document::with_version("1.5");
        let nested = doc.add_object(Object::Array(vec![500.into(), 600.into(), 700.into()]));
        let widths = vec![0.into(), nested.into(), 10.into(), 20.into(), 800.into()];
        let differences = vec![65.into(), Object::Name(b"Aacute".to_vec())];
        for (array, expected) in [(&widths, 8 * 128), (&differences, 2 * 128 + 6)] {
            let mut exact = FontWork {
                bytes: expected,
                loads: 1,
            };
            array_charge(&doc, array, &mut exact).unwrap();
            assert_eq!(exact.bytes, 0);
            let mut short = FontWork {
                bytes: expected - 1,
                loads: 1,
            };
            assert_eq!(
                array_charge(&doc, array, &mut short),
                Err("font allocation/work budget exceeded")
            );
        }
    }

    #[test]
    fn source_ranges_are_counted_before_expansion_even_with_false_counts() {
        for declaration in ["0", "1", "999"] {
            let cmap =
                format!("{declaration} beginbfrange\n<00000000><FFFFFFFF><0020>\nendbfrange");
            assert_eq!(
                cmap_charge(cmap.as_bytes()),
                Err("ToUnicode source-code cardinality limit exceeded")
            );
        }
        assert!(
            cmap_charge(b"1 begincodespacerange <00000000><FFFFFFFF> endcodespacerange").is_ok()
        );
        assert!(cmap_charge(b"1 beginbfrange <0000><FFFF><0020> endbfrange").is_ok());
        assert!(
            cmap_charge(b"1 beginbfrange <0000><FFFF><0020> <00><00><0020> endbfrange").is_err()
        );
    }

    #[test]
    fn names_comments_and_metadata_do_not_open_mapping_sections() {
        let cmap = b"% beginbfrange <00000000><FFFFFFFF><0020>\n/CMapName /beginbfrange def\n<< /Note (beginbfrange) >>\n0 beginbfchar endbfchar\n2 beginbfrange <01><03>[<0041> <0042> <0043>] <04><05><00660069> endbfrange";
        assert!(cmap_charge(cmap).is_ok());
        assert!(cmap_charge(b"1 beginbfrange <01><02>[<0041> [<0042>]] endbfrange").is_err());
        assert!(cmap_charge(b"1 beginbfchar <0000000000><0020> endbfchar").is_err());
    }

    #[test]
    fn only_complete_outer_epilogue_ends_mapping_accounting() {
        let epilogue = "endcmap CMapName currentdict /CMap defineresource pop end end";
        let mapping = "1 beginbfchar <61><0041> endbfchar";
        let base = format!("{mapping} {epilogue}");
        let trailer = b"\r]|a\x0689W\xb1\x8f\xf2f";
        let mut padded = base.as_bytes().to_vec();
        padded.extend_from_slice(trailer);
        assert_eq!(
            cmap_charge(&padded).unwrap(),
            cmap_charge(base.as_bytes()).unwrap() + trailer.len() * 32
        );
        assert!(
            cmap_charge(
                format!("{mapping} endcmap CMapName currentdict /CMap defineresource pop end ]")
                    .as_bytes()
            )
            .is_err()
        );
        let huge = "1 beginbfrange <00000000><FFFFFFFF><0020> endbfrange";
        for input in [
            format!("{huge} {epilogue}"),
            format!("({epilogue}) {huge}"),
            format!("<< /Note ({epilogue}) >> {huge}"),
            format!("[({epilogue})] {huge}"),
            format!("% {epilogue}\n{huge}"),
            format!("1 beginbfrange <01><02>[{epilogue}] endbfrange"),
            format!("1 beginbfchar {epilogue}"),
        ] {
            assert!(cmap_charge(input.as_bytes()).is_err(), "{input}");
        }
    }

    #[test]
    fn reverse_unicode_payload_is_bounded_independently_of_source_count() {
        let cmap = format!(
            "1 beginbfrange <0000><FFFF><{}> endbfrange",
            "0041".repeat(256)
        );
        assert_eq!(
            cmap_charge(cmap.as_bytes()),
            Err("ToUnicode mapping allocation limit exceeded")
        );
    }

    #[test]
    fn repeated_array_splits_are_charged_before_upstream_clones_the_payload() {
        let mut cmap = format!(
            "1 beginbfrange <0000><1FFF>[{}] endbfrange\n1 beginbfchar\n",
            "<0041> ".repeat(8192)
        );
        for code in (1..80).step_by(2) {
            writeln!(cmap, "<{code:04X}><0042>").unwrap();
        }
        cmap.push_str("endbfchar");
        assert_eq!(
            cmap_charge(cmap.as_bytes()),
            Err("ToUnicode array split/copy work limit exceeded")
        );
        assert!(cmap_charge(b"1 beginbfrange <01><05>[<0041> <0042> <0043> <0044> <0045>] endbfrange 1 beginbfchar <03><0061> endbfchar").is_ok());
    }

    #[test]
    fn touching_inserts_clone_arrays_but_distant_inserts_do_not() {
        let array = "<0041> ".repeat(1024);
        let base = format!("1 beginbfrange <0000><03FF>[{array}] endbfrange");
        let distant = format!("{base} 1 beginbfchar <FFFF><0042> endbfchar");
        assert_eq!(
            cmap_charge(distant.as_bytes()).unwrap() - cmap_charge(base.as_bytes()).unwrap(),
            32 * (distant.len() - base.len()) + 264
        );
        let mut adjacent = format!("{base} 1 beginbfchar\n");
        for _ in 0..512 {
            adjacent.push_str("<0400><0042>\n");
        }
        adjacent.push_str("endbfchar");
        assert_eq!(
            cmap_charge(adjacent.as_bytes()),
            Err("ToUnicode array split/copy work limit exceeded")
        );
    }
}

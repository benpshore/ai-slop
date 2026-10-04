//! Compile PDF content bytes into the small, owned program the interpreter needs.
//!
//! This boundary has no document, resource lookup, font decoding, or page CTM.
//! It recognises operand syntax, retains selected operators in source order,
//! and folds a constructed path into a box in the content stream's coordinates.
//! The parent interpreter supplies execution state and maps those boxes to page
//! space; `graphics` then groups the resulting painted boxes into figures.
//!
//! Two passes over each retained operator's operands avoid building objects for
//! discarded operators: the first pass records byte offsets, the second builds
//! only operands that will be consumed. Both passes use the same bounded grammar,
//! including lopdf's distinction between a recoverable stop and a fatal failure.
//! Arrays/dictionaries recurse only with a decreasing nesting budget; strings,
//! sibling operands, path construction, and the operator stream use loops.

use lopdf::{Dictionary, Error as LopdfError, Object, ObjectId, ParseError, StringFormat};

/// How deep `lopdf` lets arrays and dictionaries nest in a content stream
/// (`reader::MAX_NESTING_DEPTH`).
const MAX_NESTING: usize = 100;
/// How deep `lopdf` lets parentheses nest inside a literal string
/// (`reader::MAX_BRACKET`).
const MAX_PAREN_NESTING: usize = 100;

/// The content-stream operators the interpreter acts on, plus the painted
/// paths (`FillPath`, `StrokePath`) the lexer folds path operators into.
/// Every other operator (clipping, colour, line style, `gs`, marked content,
/// shading, Type3 `d0`/`d1`, `ET`, inline images) is lexed and dropped
/// without materialising its operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OpKind {
    /// `q`
    Save,
    /// `Q`
    Restore,
    /// `cm`
    Concat,
    /// `BT`
    BeginText,
    /// `Tf`
    Font,
    /// `Td`
    Move,
    /// `TD`
    MoveSetLeading,
    /// `Tm`
    TextMatrix,
    /// `T*`
    NextLine,
    /// `TL`
    Leading,
    /// `Tc`
    CharSpacing,
    /// `Tw`
    WordSpacing,
    /// `Tz`
    HorizontalScale,
    /// `Ts`
    Rise,
    /// `Tj`
    Show,
    /// `'`
    NextLineShow,
    /// `"`
    SpacingShow,
    /// `TJ`
    ShowArray,
    /// `Do`
    Invoke,
    /// A path painted by `f F f* B B* b b*`; its box is in
    /// [`TextProgram::paths`]. Never returned by [`OpKind::from_operator`].
    FillPath,
    /// A path painted by `S s` only; its box is in [`TextProgram::paths`].
    StrokePath,
}

impl OpKind {
    pub(super) fn from_operator(operator: &[u8]) -> Option<Self> {
        let kind = match operator {
            b"q" => Self::Save,
            b"Q" => Self::Restore,
            b"cm" => Self::Concat,
            b"BT" => Self::BeginText,
            b"Tf" => Self::Font,
            b"Td" => Self::Move,
            b"TD" => Self::MoveSetLeading,
            b"Tm" => Self::TextMatrix,
            b"T*" => Self::NextLine,
            b"TL" => Self::Leading,
            b"Tc" => Self::CharSpacing,
            b"Tw" => Self::WordSpacing,
            b"Tz" => Self::HorizontalScale,
            b"Ts" => Self::Rise,
            b"Tj" => Self::Show,
            b"'" => Self::NextLineShow,
            b"\"" => Self::SpacingShow,
            b"TJ" => Self::ShowArray,
            b"Do" => Self::Invoke,
            _ => return None,
        };
        Some(kind)
    }

    /// Whether this is a painted path, whose `first` indexes
    /// [`TextProgram::paths`] instead of the operands.
    pub(super) fn is_path(self) -> bool {
        matches!(self, Self::FillPath | Self::StrokePath)
    }
}

/// What a path operator does to the path under construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathOp {
    /// `m`, `l` (one point), `v`, `y` (two) or `c` (three): the first
    /// `2 × n` operands are the points.
    Points(usize),
    /// `re x y width height`.
    Rect,
    /// `S s` (`stroke`) or `f F f* B B* b b*`.
    Paint { stroke: bool },
    /// `n`: the path ends unpainted.
    Discard,
}

/// The path operator `operator` names. `h` adds no point and `W`/`W*` only
/// clip, so they are not path operators here.
fn path_op(operator: &[u8]) -> Option<PathOp> {
    let op = match operator {
        b"m" | b"l" => PathOp::Points(1),
        b"v" | b"y" => PathOp::Points(2),
        b"c" => PathOp::Points(3),
        b"re" => PathOp::Rect,
        b"S" | b"s" => PathOp::Paint { stroke: true },
        b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" => PathOp::Paint { stroke: false },
        b"n" => PathOp::Discard,
        _ => return None,
    };
    Some(op)
}

/// One kept operator and the range of its operands in
/// [`TextProgram::operands`].
#[derive(Clone, Copy, Debug)]
pub(super) struct TextOp {
    pub(super) kind: OpKind,
    // `kind` selects the arena: paths use one slot in `paths`, all other
    // operators use the half-open range `first..end` in `operands`. Consumers
    // must use `operands`/`path_box` instead of treating every range alike.
    first: usize,
    end: usize,
}

/// The operators of one content stream that the interpreter acts on, in
/// stream order, with their operands exactly as `Content::decode` yields them,
/// and the painted paths among them.
#[derive(Default)]
pub(super) struct TextProgram {
    pub(super) ops: Vec<TextOp>,
    pub(super) operands: Vec<Object>,
    /// `[x0, y0, x1, y1]` of each painted path, in the stream's coordinates.
    pub(super) paths: Vec<[f32; 4]>,
    /// Balanced empty save/restore operations removed at Form compilation.
    pub(super) elided_ops: usize,
    /// A malformed tail stopped lexing before the full content was consumed.
    pub(super) incomplete: bool,
}

impl TextProgram {
    /// Remove only adjacent empty q/Q pairs (including nested empty pairs).
    /// Every other retained operator is a barrier. Text, paints, transforms,
    /// invocations and unbalanced restores retain their order and multiplicity.
    pub(super) fn fold_empty_saves(&mut self) {
        // The written prefix is an explicit stack. Removing its trailing save
        // exposes an enclosing empty pair without recursion or a second pass.
        // Arena indices remain valid because only the operator list is compacted.
        let mut written = 0;
        for read in 0..self.ops.len() {
            let op = self.ops[read];
            if op.kind == OpKind::Restore
                && written > 0
                && self.ops[written - 1].kind == OpKind::Save
            {
                written -= 1;
                self.elided_ops += 2;
            } else {
                self.ops[written] = op;
                written += 1;
            }
        }
        if written < self.ops.len() {
            self.ops.truncate(written);
            // Charge the compact program; untouched programs keep their capacity.
            self.ops.shrink_to_fit();
        }
    }

    /// Rough size of the program in memory: the vectors' elements plus the
    /// heap bytes of string, name and array operands.
    pub(super) fn estimated_bytes(&self) -> usize {
        // Production operands come only from the depth-bounded lexer below.
        // Walking their nested allocations therefore has the same depth bound;
        // references are scalar IDs and never traverse the document graph here.
        fn heap_bytes(object: &Object) -> usize {
            match object {
                Object::String(bytes, _) | Object::Name(bytes) => bytes.capacity(),
                Object::Array(items) => {
                    items.capacity() * size_of::<Object>()
                        + items.iter().map(heap_bytes).sum::<usize>()
                }
                Object::Dictionary(dict) => {
                    // IndexMap retains both entries and a hash index.
                    dict.as_hashmap().capacity()
                        * (size_of::<(Vec<u8>, Object)>() + 3 * size_of::<usize>())
                        + dict
                            .as_hashmap()
                            .iter()
                            .map(|(key, value)| key.capacity() + heap_bytes(value))
                            .sum::<usize>()
                }
                _ => 0,
            }
        }
        size_of::<Self>()
            + 2 * size_of::<usize>()
            + self.ops.capacity() * size_of::<TextOp>()
            + self.operands.capacity() * size_of::<Object>()
            + self.paths.capacity() * size_of::<[f32; 4]>()
            + self.operands.iter().map(heap_bytes).sum::<usize>()
    }

    /// The operands of `op` (none for a painted path).
    pub(super) fn operands(&self, op: TextOp) -> &[Object] {
        if op.kind.is_path() {
            return &[];
        }
        self.operands.get(op.first..op.end).unwrap_or_default()
    }

    /// The box of a painted path.
    pub(super) fn path_box(&self, op: TextOp) -> Option<[f32; 4]> {
        if op.kind.is_path() {
            self.paths.get(op.first).copied()
        } else {
            None
        }
    }
}

/// Why no object or operation could be read at some position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Halt {
    /// Nothing valid here (a `nom` error): `lopdf` backtracks, and at the
    /// top level it stops and keeps the operations read so far.
    Stop,
    /// `lopdf` rejects the whole content stream (a `nom` failure).
    Fatal,
}

/// The end of a lexed object (before any white space after it) and, in
/// build mode, the object.
pub(super) type Lexed = Result<(usize, Option<Object>), Halt>;

/// White space `lopdf` skips between content-stream tokens.
fn is_content_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

/// PDF white space, skipped inside arrays, dictionaries and hex strings.
pub(super) fn is_pdf_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b'\0' | 0x0C)
}

fn is_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

pub(super) fn is_regular(byte: u8) -> bool {
    !is_pdf_space(byte) && !is_delimiter(byte)
}

fn is_digit(byte: u8) -> bool {
    byte.is_ascii_digit()
}

fn is_operator_char(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'*' | b'\'' | b'"')
}

/// White space around the `EI` that ends an inline image of unknown length.
fn is_ei_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\n' | b'\r')
}

/// The position after the run of bytes from `pos` for which `keep` holds.
pub(super) fn skip_while(bytes: &[u8], pos: usize, keep: fn(u8) -> bool) -> usize {
    let rest = bytes.get(pos..).unwrap_or_default();
    pos + rest.iter().take_while(|&&byte| keep(byte)).count()
}

fn skip_content_space(bytes: &[u8], pos: usize) -> usize {
    skip_while(bytes, pos, is_content_space)
}

/// The end of the end-of-line marker (`\r\n`, `\n` or `\r`) at `pos`.
fn eol_end(bytes: &[u8], pos: usize) -> Option<usize> {
    match bytes.get(pos..)? {
        [b'\r', b'\n', ..] => Some(pos + 2),
        [b'\r' | b'\n', ..] => Some(pos + 1),
        _ => None,
    }
}

/// The end of the `%` comment at `pos`, after its end-of-line marker;
/// `None` when there is no comment there or nothing terminates it.
fn comment_end(bytes: &[u8], pos: usize) -> Option<usize> {
    if bytes.get(pos) != Some(&b'%') {
        return None;
    }
    let eol = skip_while(bytes, pos + 1, |byte| byte != b'\r' && byte != b'\n');
    eol_end(bytes, eol)
}

/// White space and comments, as `lopdf` skips them inside arrays and
/// dictionaries.
pub(super) fn skip_space(bytes: &[u8], mut pos: usize) -> usize {
    loop {
        let next = skip_while(bytes, pos, is_pdf_space);
        match comment_end(bytes, next) {
            Some(end) => pos = end,
            None => return next,
        }
    }
}

pub(super) fn hex_value(digit: u8) -> u8 {
    match digit {
        b'0'..=b'9' => digit - b'0',
        b'a'..=b'f' => digit - b'a' + 10,
        b'A'..=b'F' => digit - b'A' + 10,
        _ => 0,
    }
}

fn parse_ascii<T: std::str::FromStr>(bytes: &[u8], start: usize, end: usize) -> Result<T, Halt> {
    let text = bytes
        .get(start..end)
        .and_then(|slice| std::str::from_utf8(slice).ok());
    text.and_then(|text| text.parse::<T>().ok())
        .ok_or(Halt::Stop)
}

/// The name whose `/` is at `pos`: its end and, in build mode, its bytes
/// with `#xx` escapes decoded. A `#` without two hex digits ends it.
fn lex_name(bytes: &[u8], pos: usize, build: bool) -> (usize, Vec<u8>) {
    let mut name = Vec::new();
    let mut at = pos + 1;
    loop {
        match bytes.get(at..).unwrap_or_default() {
            [b'#', high, low, ..] if high.is_ascii_hexdigit() && low.is_ascii_hexdigit() => {
                if build {
                    name.push((hex_value(*high) << 4) | hex_value(*low));
                }
                at += 3;
            }
            [byte, ..] if *byte != b'#' && is_regular(*byte) => {
                if build {
                    name.push(*byte);
                }
                at += 1;
            }
            _ => return (at, name),
        }
    }
}

/// The escape sequence whose backslash ends at `pos`: its end and the byte
/// it stands for (`None` for a line continuation).
fn lex_escape(bytes: &[u8], pos: usize) -> Result<(usize, Option<u8>), Halt> {
    let Some(&first) = bytes.get(pos) else {
        return Err(Halt::Stop);
    };
    if (b'0'..=b'7').contains(&first) {
        let mut value: u16 = 0;
        let mut end = pos;
        for &digit in bytes.iter().skip(pos).take(3) {
            if !(b'0'..=b'7').contains(&digit) {
                break;
            }
            value = value * 8 + u16::from(digit - b'0');
            end += 1;
        }
        // Overflow past 0o377 is ignored, as the spec (and `lopdf`) say.
        return Ok((end, Some(value as u8)));
    }
    if let Some(end) = eol_end(bytes, pos) {
        return Ok((end, None));
    }
    let value = match first {
        b'n' => b'\n',
        b'r' => b'\r',
        b't' => b'\t',
        b'b' => 0x08,
        b'f' => 0x0C,
        other => other,
    };
    Ok((pos + 1, Some(value)))
}

/// The literal string whose `(` is at `pos`: its end and, in build mode,
/// its bytes. Balanced inner parentheses are kept, raw end-of-line markers
/// are kept as written, and more than [`MAX_PAREN_NESTING`] open inner
/// parentheses make the string unreadable, as in `lopdf`.
fn lex_literal(bytes: &[u8], pos: usize, build: bool) -> Result<(usize, Vec<u8>), Halt> {
    let mut out = Vec::new();
    let mut open: usize = 0;
    let mut at = pos + 1;
    loop {
        let Some(&byte) = bytes.get(at) else {
            return Err(Halt::Stop);
        };
        match byte {
            b')' => {
                at += 1;
                if open == 0 {
                    return Ok((at, out));
                }
                open -= 1;
                if build {
                    out.push(byte);
                }
            }
            b'(' => {
                if open >= MAX_PAREN_NESTING {
                    return Err(Halt::Stop);
                }
                open += 1;
                at += 1;
                if build {
                    out.push(byte);
                }
            }
            b'\\' => {
                let (end, escaped) = lex_escape(bytes, at + 1)?;
                if build && let Some(value) = escaped {
                    out.push(value);
                }
                at = end;
            }
            _ => {
                if build {
                    out.push(byte);
                }
                at += 1;
            }
        }
    }
}

/// The hex string whose `<` is at `pos`: its end and, in build mode, its
/// bytes. White space between digits is ignored and an odd final digit is
/// padded with 0.
fn lex_hex(bytes: &[u8], pos: usize, build: bool) -> Result<(usize, Vec<u8>), Halt> {
    let mut out: Vec<u8> = Vec::new();
    let mut low_next = false;
    let mut at = pos + 1;
    loop {
        let next = skip_while(bytes, at, is_pdf_space);
        match bytes.get(next) {
            Some(&digit) if digit.is_ascii_hexdigit() => {
                if build {
                    let value = hex_value(digit);
                    if low_next {
                        if let Some(last) = out.last_mut() {
                            *last |= value;
                        }
                    } else {
                        out.push(value << 4);
                    }
                }
                low_next = !low_next;
                at = next + 1;
            }
            _ => break,
        }
    }
    let close = skip_while(bytes, at, is_pdf_space);
    if bytes.get(close) == Some(&b'>') {
        Ok((close + 1, out))
    } else {
        Err(Halt::Stop)
    }
}

/// A number at `pos` (`-.5`, `6.`, `+3`): `Ok(None)` when there is none,
/// `Err(Stop)` for an integer outside `i64`, which `lopdf` cannot read.
fn lex_number(
    bytes: &[u8],
    pos: usize,
    build: bool,
) -> Result<Option<(usize, Option<Object>)>, Halt> {
    let mut digits_start = pos;
    if matches!(bytes.get(pos), Some(b'+' | b'-')) {
        digits_start += 1;
    }
    let digits_end = skip_while(bytes, digits_start, is_digit);
    let has_digits = digits_end > digits_start;
    let fraction_start = digits_end + 1;
    let is_real = bytes.get(digits_end) == Some(&b'.')
        && (has_digits || bytes.get(fraction_start).is_some_and(u8::is_ascii_digit));
    if is_real {
        let end = skip_while(bytes, fraction_start, is_digit);
        let value = if build {
            Some(Object::Real(parse_ascii::<f32>(bytes, pos, end)?))
        } else {
            None
        };
        return Ok(Some((end, value)));
    }
    if !has_digits {
        return Ok(None);
    }
    let value: i64 = parse_ascii(bytes, pos, digits_end)?;
    Ok(Some((digits_end, build.then_some(Object::Integer(value)))))
}

/// An indirect reference `n g R` at `pos` (allowed only inside arrays and
/// dictionaries): its end and id.
fn lex_reference(bytes: &[u8], pos: usize) -> Option<(usize, ObjectId)> {
    let id_end = skip_while(bytes, pos, is_digit);
    let id: u32 = parse_ascii(bytes, pos, id_end).ok()?;
    let generation_start = skip_space(bytes, id_end);
    let generation_end = skip_while(bytes, generation_start, is_digit);
    let generation: u16 = parse_ascii(bytes, generation_start, generation_end).ok()?;
    let marker = skip_space(bytes, generation_end);
    (bytes.get(marker) == Some(&b'R')).then_some((marker + 1, (id, generation)))
}

/// One object at `pos`, read as `lopdf` reads a content-stream operand
/// (`direct == false`) or an array element or dictionary value
/// (`direct == true`, where `n g R` references are allowed too). Nested
/// arrays and dictionaries get `depth` as their budget. Nothing is
/// allocated unless `build`.
pub(super) fn lex_object(
    bytes: &[u8],
    pos: usize,
    depth: usize,
    direct: bool,
    build: bool,
) -> Lexed {
    let rest = bytes.get(pos..).unwrap_or_default();
    if rest.starts_with(b"null") {
        return Ok((pos + 4, build.then_some(Object::Null)));
    }
    if rest.starts_with(b"true") {
        return Ok((pos + 4, build.then_some(Object::Boolean(true))));
    }
    if rest.starts_with(b"false") {
        return Ok((pos + 5, build.then_some(Object::Boolean(false))));
    }
    if direct && let Some((end, id)) = lex_reference(bytes, pos) {
        return Ok((end, build.then_some(Object::Reference(id))));
    }
    if let Some(number) = lex_number(bytes, pos, build)? {
        return Ok(number);
    }
    match rest {
        [b'/', ..] => {
            let (end, name) = lex_name(bytes, pos, build);
            Ok((end, build.then_some(Object::Name(name))))
        }
        [b'(', ..] => {
            let (end, text) = lex_literal(bytes, pos, build)?;
            Ok((
                end,
                build.then_some(Object::String(text, StringFormat::Literal)),
            ))
        }
        [b'<', b'<', ..] => lex_dictionary(bytes, pos, depth, build),
        [b'<', ..] => {
            let (end, text) = lex_hex(bytes, pos, build)?;
            let format = StringFormat::Hexadecimal;
            Ok((end, build.then_some(Object::String(text, format))))
        }
        [b'[', ..] => lex_array(bytes, pos, depth, build),
        _ => Err(Halt::Stop),
    }
}

/// An array element or dictionary value and the white space after it. A
/// container with no budget left is fatal, as in `lopdf`.
fn lex_direct(bytes: &[u8], pos: usize, depth: usize, build: bool) -> Lexed {
    // Every recursive edge into an array element or dictionary value passes
    // here. Keep the check even for skip-only lexing: dropping allocations must
    // not change which nesting depths the grammar accepts or rejects.
    if depth == 0 {
        return Err(Halt::Fatal);
    }
    let (end, value) = lex_object(bytes, pos, depth - 1, true, build)?;
    Ok((skip_space(bytes, end), value))
}

/// The array whose `[` is at `pos`.
fn lex_array(bytes: &[u8], pos: usize, depth: usize, build: bool) -> Lexed {
    let mut items: Vec<Object> = Vec::new();
    let mut at = skip_space(bytes, pos + 1);
    loop {
        match lex_direct(bytes, at, depth, build) {
            Ok((end, item)) => {
                items.extend(item);
                at = end;
            }
            Err(Halt::Stop) => break,
            Err(Halt::Fatal) => return Err(Halt::Fatal),
        }
    }
    if bytes.get(at) == Some(&b']') {
        Ok((at + 1, build.then_some(Object::Array(items))))
    } else {
        Err(Halt::Stop)
    }
}

/// `/Key value` pairs from `pos` up to the first position that does not
/// start one: that position and, in build mode, the entries.
fn lex_entries(
    bytes: &[u8],
    pos: usize,
    depth: usize,
    build: bool,
) -> Result<(usize, Dictionary), Halt> {
    let mut dict = Dictionary::new();
    let mut at = pos;
    while bytes.get(at) == Some(&b'/') {
        let (name_end, key) = lex_name(bytes, at, build);
        match lex_direct(bytes, skip_space(bytes, name_end), depth, build) {
            Ok((end, value)) => {
                if let Some(value) = value {
                    dict.set(key, value);
                }
                at = end;
            }
            Err(Halt::Stop) => break,
            Err(Halt::Fatal) => return Err(Halt::Fatal),
        }
    }
    Ok((at, dict))
}

/// The dictionary whose `<<` is at `pos`.
fn lex_dictionary(bytes: &[u8], pos: usize, depth: usize, build: bool) -> Lexed {
    let (at, dict) = lex_entries(bytes, skip_space(bytes, pos + 2), depth, build)?;
    if bytes.get(at..).is_some_and(|rest| rest.starts_with(b">>")) {
        Ok((at + 2, build.then_some(Object::Dictionary(dict))))
    } else {
        Err(Halt::Stop)
    }
}

fn inline_entry<'d>(dict: &'d Dictionary, short: &[u8], long: &[u8]) -> Option<&'d Object> {
    dict.get(short).or_else(|_| dict.get(long)).ok()
}

/// The data length `lopdf` computes for an unfiltered inline image, `None`
/// where it cannot (and scans for `EI` instead).
fn inline_image_length(dict: &Dictionary) -> Option<usize> {
    let width = inline_entry(dict, b"W", b"Width")?.as_i64().ok()? as usize;
    let height = inline_entry(dict, b"H", b"Height")?.as_i64().ok()? as usize;
    let bits = inline_entry(dict, b"BPC", b"BitsPerComponent")?
        .as_i64()
        .ok()? as usize;
    let mask = inline_entry(dict, b"IM", b"ImageMask")
        .is_some_and(|value| matches!(value.as_bool(), Ok(true)));
    let colours: usize = if mask {
        1
    } else {
        match inline_entry(dict, b"CS", b"ColorSpace")?.as_name().ok()? {
            b"DeviceGray" | b"Gray" => 1,
            b"DeviceRGB" | b"RGB" => 3,
            b"DeviceRGBA" | b"RGBA" | b"DeviceCMYK" | b"CMYK" => 4,
            _ => return None,
        }
    };
    if inline_entry(dict, b"F", b"Filter").is_some() {
        return None;
    }
    let stride = width.checked_mul(colours.checked_mul(bits)?)?.div_ceil(8);
    height.checked_mul(stride)
}

/// Skip the inline image whose `BI` ends at `pos`, as `lopdf` reads it:
/// the data length comes from the image dictionary when `lopdf` can compute
/// it (so data bytes that spell `EI` are skipped), otherwise the data runs
/// to the first `EI` with white space on both sides. `None` where `lopdf`
/// rejects the whole content stream, with one exception: an `EI` that ends
/// the stream right after white space is accepted (`lopdf` wants white
/// space after it too), so a stream cut off after its last inline image
/// keeps its text.
fn skip_inline_image(bytes: &[u8], pos: usize) -> Option<usize> {
    let start = skip_content_space(bytes, pos);
    let (at, dict) = lex_entries(bytes, start, MAX_NESTING, true).ok()?;
    if !bytes.get(at..)?.starts_with(b"ID") {
        return None;
    }
    let data = skip_content_space(bytes, at + 2);
    if let Some(length) = inline_image_length(&dict)
        && let Some(data_end) = data.checked_add(length)
        && data_end <= bytes.len()
    {
        let marker = skip_content_space(bytes, data_end);
        if !bytes.get(marker..)?.starts_with(b"EI") {
            return None;
        }
        return Some(skip_content_space(bytes, marker + 2));
    }
    let rest = bytes.get(data..)?;
    let found = rest.windows(4).position(|window| {
        matches!(window, [before, b'E', b'I', after] if is_ei_space(*before) && is_ei_space(*after))
    });
    if let Some(found) = found {
        return Some(skip_content_space(bytes, data + found + 3));
    }
    match rest {
        [.., before, b'E', b'I'] if is_ei_space(*before) => Some(bytes.len()),
        _ => None,
    }
}

fn invalid_content() -> LopdfError {
    LopdfError::Parse(ParseError::InvalidContentStream)
}

/// Read a content stream as `Content::decode` does and keep only the
/// operators [`OpKind`] names, with their operands, and one box per painted
/// path (see [`record_path`]). Everything else is tokenised and dropped
/// without allocating. Like `lopdf`, lexing stops
/// at the first token it cannot read, keeping what came before and recording
/// `incomplete` evidence unless the remainder is only whitespace or a comment,
/// and fails only where `lopdf` rejects the whole stream (an inline image
/// without `ID` or `EI`, arrays or dictionaries nested too deep). The one
/// place it is more lenient is an inline image whose `EI` ends the stream
/// (see [`skip_inline_image`]).
pub(super) fn lex_content(bytes: &[u8]) -> Result<TextProgram, LopdfError> {
    let mut program = TextProgram::default();
    let mut starts: Vec<usize> = Vec::new();
    let mut path: Option<[f32; 4]> = None;
    let mut pos = skip_content_space(bytes, 0);
    loop {
        let mut at = pos;
        while let Some(end) = comment_end(bytes, at) {
            at = skip_content_space(bytes, end);
        }
        if bytes.get(at..).is_some_and(|rest| rest.starts_with(b"BI")) {
            pos = skip_inline_image(bytes, at + 2).ok_or_else(invalid_content)?;
            continue;
        }
        // Offsets borrow nothing from the source and can be reused for every
        // operator. Until its name is known, only validate operand syntax.
        starts.clear();
        loop {
            match lex_object(bytes, at, MAX_NESTING, false, false) {
                Ok((end, _)) => {
                    starts.push(at);
                    at = skip_content_space(bytes, end);
                }
                Err(Halt::Stop) => break,
                Err(Halt::Fatal) => return Err(invalid_content()),
            }
        }
        let end = skip_while(bytes, at, is_operator_char);
        if end == at {
            let mut tail = skip_while(bytes, at, is_pdf_space);
            while bytes.get(tail) == Some(&b'%') {
                tail = skip_while(bytes, tail, |byte| !matches!(byte, b'\r' | b'\n'));
                tail = skip_while(bytes, tail, is_pdf_space);
            }
            program.incomplete = !starts.is_empty() || tail < bytes.len();
            return Ok(program);
        }
        let operator = bytes.get(at..end).unwrap_or_default();
        if let Some(kind) = OpKind::from_operator(operator) {
            // Re-read only retained operands; do not move operators across a
            // paint or transform, since execution and Form reuse depend on the
            // original ordering even when intervening operators are discarded.
            let first = program.operands.len();
            for &start in &starts {
                if let Ok((_, Some(operand))) = lex_object(bytes, start, MAX_NESTING, false, true) {
                    program.operands.push(operand);
                }
            }
            let last = program.operands.len();
            program.ops.push(TextOp {
                kind,
                first,
                end: last,
            });
        } else if let Some(op) = path_op(operator) {
            record_path(&mut program, &mut path, op, bytes, &starts);
        }
        pos = skip_content_space(bytes, end);
    }
}

/// The first `out.len()` operands (starting at `starts`) as numbers; false
/// when there are fewer or one of them is not a number.
fn read_numbers(bytes: &[u8], starts: &[usize], out: &mut [f32]) -> bool {
    if starts.len() < out.len() {
        return false;
    }
    for (slot, &start) in out.iter_mut().zip(starts) {
        *slot = match lex_number(bytes, start, true) {
            Ok(Some((_, Some(Object::Integer(value))))) => value as f32,
            Ok(Some((_, Some(Object::Real(value))))) => value,
            _ => return false,
        };
    }
    true
}

/// Widen `path` (`[x0, y0, x1, y1]`, `None` before its first point) to
/// take in the point `(x, y)`.
fn grow(path: &mut Option<[f32; 4]>, x: f32, y: f32) {
    match path {
        Some(bounds) => {
            bounds[0] = bounds[0].min(x);
            bounds[1] = bounds[1].min(y);
            bounds[2] = bounds[2].max(x);
            bounds[3] = bounds[3].max(y);
        }
        None => *path = Some([x, y, x, y]),
    }
}

/// Apply one path operator: construction widens the box of the current
/// path (curve control points included), painting records it in `program`
/// as one [`OpKind::FillPath`] or [`OpKind::StrokePath`] and starts a new
/// path, `n` drops it. The operands are read from their `starts` into a
/// fixed buffer; an operator whose operands are not numbers adds nothing.
/// `cm`, `q`, `Q`, `Do` and text cannot occur inside a path object, so the
/// box needs no CTM here.
fn record_path(
    program: &mut TextProgram,
    path: &mut Option<[f32; 4]>,
    op: PathOp,
    bytes: &[u8],
    starts: &[usize],
) {
    let mut values: [f32; 6] = [0.0; 6];
    match op {
        PathOp::Points(count) => {
            let Some(slots) = values.get_mut(..count * 2) else {
                return;
            };
            if read_numbers(bytes, starts, slots) {
                for &[x, y] in slots.as_chunks::<2>().0 {
                    grow(path, x, y);
                }
            }
        }
        PathOp::Rect => {
            if read_numbers(bytes, starts, &mut values[..4]) {
                let [x, y, width, height, _, _] = values;
                grow(path, x, y);
                grow(path, x + width, y + height);
            }
        }
        PathOp::Paint { stroke } => {
            // Taking the box resets the path at its paint boundary. Curve
            // control points produce a covering box, not exact curve extrema;
            // the interpreter will transform all four corners at execution.
            if let Some(bounds) = path.take() {
                let kind = if stroke {
                    OpKind::StrokePath
                } else {
                    OpKind::FillPath
                };
                let index = program.paths.len();
                program.paths.push(bounds);
                program.ops.push(TextOp {
                    kind,
                    first: index,
                    end: index + 1,
                });
            }
        }
        PathOp::Discard => *path = None,
    }
}

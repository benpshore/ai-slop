//! Pure-Rust backend built on `lopdf` 0.45. It interprets each page's
//! content stream (text state, graphics state, Form `XObject`s) and yields
//! one positioned [`Span`] per shown string. Nothing is ordered or repaired.

use std::collections::BTreeMap;

use lopdf::content::{Content, Operation};
use lopdf::{
    Dictionary, Document, Encoding, Error as LopdfError, LoadOptions, Object, ObjectId, Stream,
};
use unicode_normalization::UnicodeNormalization;

use crate::backend::{BackendError, DocumentSession, EncryptionProblem, Extractor};
use crate::schema::{BBox, BackendIdentity, PageText, Span, config_digest};

/// Glyph width (in 1/1000 em) assumed when a font declares nothing usable.
const DEFAULT_WIDTH: f32 = 500.0;
/// Descent estimate below the baseline, as a fraction of the font size.
const DESCENT: f32 = -0.2;
/// Ascent estimate above the baseline, as a fraction of the font size.
const ASCENT: f32 = 0.8;
/// Bound on the `/Parent` walk used for inherited page attributes.
const MAX_PARENT_DEPTH: u32 = 64;

/// The `lopdf` extractor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LopdfBackend {
    /// Nesting limit for Form `XObject`s invoked with `Do`.
    pub max_xobject_depth: u32,
}

impl Default for LopdfBackend {
    fn default() -> Self {
        Self {
            max_xobject_depth: 8,
        }
    }
}

impl Extractor for LopdfBackend {
    /// Name `lopdf`, version `0.45`, digest over `max_xobject_depth`.
    fn identity(&self) -> BackendIdentity {
        let mut config = BTreeMap::new();
        config.insert(
            "max_xobject_depth".to_string(),
            self.max_xobject_depth.to_string(),
        );
        BackendIdentity {
            name: "lopdf".to_string(),
            version: "0.45".to_string(),
            config_digest: config_digest(&config),
        }
    }

    /// Parse the bytes; an encrypted file needs `password` or fails with
    /// [`EncryptionProblem::PasswordRequired`].
    fn open(
        &self,
        bytes: &[u8],
        password: Option<&str>,
    ) -> Result<Box<dyn DocumentSession>, BackendError> {
        let doc = load_document(bytes, password)?;
        let pages = doc.get_pages();
        Ok(Box::new(LopdfSession {
            doc,
            pages,
            max_xobject_depth: self.max_xobject_depth,
        }))
    }
}

fn load_document(bytes: &[u8], password: Option<&str>) -> Result<Document, BackendError> {
    let doc = match password {
        Some(password) => {
            let options = LoadOptions::with_password(password);
            let loaded = Document::load_mem_with_options(bytes, options);
            loaded.map_err(map_load_error)?
        }
        None => Document::load_mem(bytes).map_err(map_load_error)?,
    };
    // Without a usable password `load_mem` keeps the file encrypted: the
    // `/Encrypt` entry stays in the trailer and no objects are parsed.
    if doc.is_encrypted() {
        return Err(BackendError::Encrypted(EncryptionProblem::PasswordRequired));
    }
    if doc.catalog().is_err() {
        return Err(BackendError::Malformed("no document catalog".to_string()));
    }
    Ok(doc)
}

fn map_load_error(err: LopdfError) -> BackendError {
    match err {
        LopdfError::InvalidPassword => BackendError::Encrypted(EncryptionProblem::WrongPassword),
        LopdfError::UnsupportedSecurityHandler(_) | LopdfError::Decryption(_) => {
            BackendError::Encrypted(EncryptionProblem::UnsupportedCipher)
        }
        other => BackendError::Malformed(other.to_string()),
    }
}

struct LopdfSession {
    doc: Document,
    pages: BTreeMap<u32, ObjectId>,
    max_xobject_depth: u32,
}

impl DocumentSession for LopdfSession {
    fn page_count(&self) -> u32 {
        u32::try_from(self.pages.len()).unwrap_or(u32::MAX)
    }

    fn page_text(&mut self, page: u32) -> Result<PageText, BackendError> {
        let count = self.page_count();
        let Some(&page_id) = self.pages.get(&page) else {
            return Err(BackendError::PageRange { page, count });
        };
        extract_page(&self.doc, page, page_id, self.max_xobject_depth)
    }

    fn info(&self) -> BTreeMap<String, String> {
        let mut info = BTreeMap::new();
        let Ok(info_ref) = self.doc.trailer.get(b"Info") else {
            return info;
        };
        let Ok((_, info_obj)) = self.doc.dereference(info_ref) else {
            return info;
        };
        let Ok(dict) = info_obj.as_dict() else {
            return info;
        };
        for (key, value) in dict {
            if let Ok(text) = lopdf::decode_text_string(value) {
                info.insert(String::from_utf8_lossy(key).into_owned(), text);
            }
        }
        info
    }
}

fn page_error(page: u32, message: String) -> BackendError {
    BackendError::Page { page, message }
}

fn lossy(name: &[u8]) -> String {
    String::from_utf8_lossy(name).into_owned()
}

/// Look `key` up on a page dictionary, walking `/Parent` for inherited
/// attributes (`MediaBox`, `CropBox`, `Rotate`, `Resources`).
fn inherited<'a>(doc: &'a Document, page: &'a Dictionary, key: &[u8]) -> Option<&'a Object> {
    let mut node = page;
    for _ in 0..MAX_PARENT_DEPTH {
        if let Ok(value) = node.get(key)
            && let Ok((_, value)) = doc.dereference(value)
        {
            return Some(value);
        }
        let Ok(parent) = node.get_deref(b"Parent", doc) else {
            return None;
        };
        let Ok(parent_dict) = parent.as_dict() else {
            return None;
        };
        node = parent_dict;
    }
    None
}

/// `(width, height)` of a rectangle array `[x0 y0 x1 y1]`.
fn rect_size(obj: &Object) -> Option<(f32, f32)> {
    let array = obj.as_array().ok()?;
    if array.len() != 4 {
        return None;
    }
    let mut values: [f32; 4] = [0.0; 4];
    for (slot, item) in values.iter_mut().zip(array) {
        *slot = item.as_float().ok()?;
    }
    let width = (values[2] - values[0]).abs();
    let height = (values[3] - values[1]).abs();
    Some((width, height))
}

/// `/Rotate` normalised to 0/90/180/270; anything else reads as 0.
fn page_rotation(doc: &Document, page: &Dictionary) -> i32 {
    let Some(value) = inherited(doc, page, b"Rotate") else {
        return 0;
    };
    let Ok(rotate_degrees) = value.as_i64() else {
        return 0;
    };
    let Ok(normalised) = u32::try_from(rotate_degrees.rem_euclid(360)) else {
        return 0;
    };
    if normalised.is_multiple_of(90) {
        i32::try_from(normalised).unwrap_or(0)
    } else {
        0
    }
}

/// Row-vector affine matrix `[a b c d e f]` as PDF uses it:
/// `x' = a*x + c*y + e`, `y' = b*x + d*y + f`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Matrix {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Matrix {
    const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    fn translation(tx: f32, ty: f32) -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: tx,
            f: ty,
        }
    }

    /// `self × other`: apply `self` first, then `other`.
    #[must_use]
    fn then(self, other: Self) -> Self {
        Self {
            a: self.a * other.a + self.b * other.c,
            b: self.a * other.b + self.b * other.d,
            c: self.c * other.a + self.d * other.c,
            d: self.c * other.b + self.d * other.d,
            e: self.e * other.a + self.f * other.c + other.e,
            f: self.e * other.b + self.f * other.d + other.f,
        }
    }

    fn apply(self, x: f32, y: f32) -> (f32, f32) {
        let tx = self.a * x + self.c * y + self.e;
        let ty = self.b * x + self.d * y + self.f;
        (tx, ty)
    }

    /// How one unit of text-space height maps to user space.
    fn y_scale(self) -> f32 {
        (self.b * self.b + self.d * self.d).sqrt()
    }
}

fn float_at(operands: &[Object], index: usize) -> Option<f32> {
    operands.get(index)?.as_float().ok()
}

fn string_at(operands: &[Object], index: usize) -> Option<&[u8]> {
    operands.get(index)?.as_str().ok()
}

fn matrix_from_operands(operands: &[Object]) -> Option<Matrix> {
    if operands.len() < 6 {
        return None;
    }
    let mut values: [f32; 6] = [0.0; 6];
    for (slot, item) in values.iter_mut().zip(operands) {
        *slot = item.as_float().ok()?;
    }
    Some(Matrix {
        a: values[0],
        b: values[1],
        c: values[2],
        d: values[3],
        e: values[4],
        f: values[5],
    })
}

fn number(doc: &Document, item: &Object) -> Option<f32> {
    let (_, value) = doc.dereference(item).ok()?;
    value.as_float().ok()
}

fn to_code(value: f32) -> Option<u32> {
    if (0.0..=65_535.0).contains(&value) {
        Some(value as u32)
    } else {
        None
    }
}

/// Glyph widths of a simple (single-byte) font.
struct SimpleWidths {
    first_char: u32,
    widths: Vec<f32>,
    missing: Option<f32>,
}

impl SimpleWidths {
    fn unknown() -> Self {
        Self {
            first_char: 0,
            widths: Vec::new(),
            missing: None,
        }
    }

    fn width(&self, code: u32) -> f32 {
        let fallback = self.missing.unwrap_or(DEFAULT_WIDTH);
        let Some(offset) = code.checked_sub(self.first_char) else {
            return fallback;
        };
        let Ok(index) = usize::try_from(offset) else {
            return fallback;
        };
        self.widths.get(index).copied().unwrap_or(fallback)
    }
}

/// Glyph widths of a composite (Type0) font, keyed by CID.
struct CompositeWidths {
    /// `(first, last, width)` runs from the `/W` array.
    ranges: Vec<(u32, u32, f32)>,
    default_width: f32,
}

impl CompositeWidths {
    fn width(&self, cid: u32) -> f32 {
        for &(first, last, glyph_width) in &self.ranges {
            if (first..=last).contains(&cid) {
                return glyph_width;
            }
        }
        self.default_width
    }
}

enum Widths {
    Simple(SimpleWidths),
    Composite(CompositeWidths),
}

impl Widths {
    fn glyph_width(&self, code: u32) -> f32 {
        match self {
            Self::Simple(simple) => simple.width(code),
            Self::Composite(composite) => composite.width(code),
        }
    }
}

/// How the bytes of a shown string become text.
enum Decode<'a> {
    /// The font's own encoding as resolved by `lopdf`.
    Encoding(Encoding<'a>),
    /// No encoding is available (for the given reason); bytes are Latin-1.
    Latin1(&'static str),
    /// The font cannot be decoded at all; every code becomes U+FFFD.
    Replacement,
}

struct LoadedFont<'a> {
    /// Resource name, for warnings.
    label: String,
    /// `/BaseFont` if present.
    base_font: Option<String>,
    decode: Decode<'a>,
    /// Two-byte codes (Type0), otherwise single-byte.
    composite: bool,
    /// Decoding yields exactly one char per byte, so dropped bytes are detectable.
    one_to_one: bool,
    widths: Widths,
}

impl LoadedFont<'_> {
    fn missing(resource_name: &[u8]) -> Self {
        Self {
            label: lossy(resource_name),
            base_font: None,
            decode: Decode::Latin1("not in resources"),
            composite: false,
            one_to_one: false,
            widths: Widths::Simple(SimpleWidths::unknown()),
        }
    }
}

fn load_font<'a>(doc: &'a Document, resource_name: &[u8], dict: &'a Dictionary) -> LoadedFont<'a> {
    let subtype = dict.get(b"Subtype").and_then(Object::as_name);
    let composite = subtype.is_ok_and(|name| name == b"Type0");
    let base_font = dict.get(b"BaseFont").and_then(Object::as_name).ok();
    let (decode, one_to_one) = if composite {
        (composite_decode(doc, dict), false)
    } else {
        simple_decode(doc, dict)
    };
    let widths = if composite {
        Widths::Composite(composite_widths(doc, dict))
    } else {
        Widths::Simple(simple_widths(doc, dict))
    };
    LoadedFont {
        label: lossy(resource_name),
        base_font: base_font.map(lossy),
        decode,
        composite,
        one_to_one,
        widths,
    }
}

fn simple_decode<'a>(doc: &'a Document, dict: &'a Dictionary) -> (Decode<'a>, bool) {
    match dict.get_font_encoding(doc) {
        Ok(encoding) => {
            let one_to_one = !matches!(encoding, Encoding::UnicodeMapEncoding(_));
            (Decode::Encoding(encoding), one_to_one)
        }
        Err(_) => (Decode::Latin1("no usable encoding"), false),
    }
}

fn composite_decode<'a>(doc: &'a Document, dict: &'a Dictionary) -> Decode<'a> {
    // `lopdf` maps a CID font through its `/ToUnicode` CMap (for
    // `Identity-H`/`Identity-V`, or when `/Encoding` is absent) or through a
    // predefined UTF-16 CMap named in `/Encoding`. Any other shape silently
    // falls back to a one-byte table inside `lopdf`, which would be garbage.
    let to_unicode = dict.get_deref(b"ToUnicode", doc);
    let has_to_unicode = to_unicode.is_ok_and(|obj| obj.as_stream().is_ok());
    let encoding_obj = dict.get_deref(b"Encoding", doc).ok();
    let usable = match encoding_obj.and_then(|obj| obj.as_name().ok()) {
        Some(b"Identity-H" | b"Identity-V") => has_to_unicode,
        Some(_) => true,
        None => encoding_obj.is_none() && has_to_unicode,
    };
    if !usable {
        return Decode::Replacement;
    }
    match dict.get_font_encoding(doc) {
        Ok(encoding) => Decode::Encoding(encoding),
        Err(_) => Decode::Replacement,
    }
}

fn simple_widths(doc: &Document, dict: &Dictionary) -> SimpleWidths {
    let mut first_char = 0;
    if let Ok(value) = dict.get_deref(b"FirstChar", doc)
        && let Ok(first) = value.as_i64()
    {
        first_char = u32::try_from(first).unwrap_or(0);
    }
    let mut widths = Vec::new();
    if let Ok(value) = dict.get_deref(b"Widths", doc)
        && let Ok(array) = value.as_array()
    {
        widths.reserve(array.len());
        for item in array {
            widths.push(number(doc, item).unwrap_or(0.0));
        }
    }
    let mut missing = None;
    if let Ok(descriptor) = dict.get_deref(b"FontDescriptor", doc)
        && let Ok(descriptor) = descriptor.as_dict()
        && let Ok(missing_obj) = descriptor.get_deref(b"MissingWidth", doc)
    {
        missing = missing_obj.as_float().ok();
    }
    SimpleWidths {
        first_char,
        widths,
        missing,
    }
}

fn composite_widths(doc: &Document, dict: &Dictionary) -> CompositeWidths {
    let mut ranges = Vec::new();
    let mut default_width: f32 = 1000.0;
    if let Ok(descendants) = dict.get_deref(b"DescendantFonts", doc)
        && let Ok(descendants) = descendants.as_array()
        && let Some(first) = descendants.first()
        && let Ok((_, cid_font)) = doc.dereference(first)
        && let Ok(cid_font) = cid_font.as_dict()
    {
        if let Ok(dw) = cid_font.get_deref(b"DW", doc)
            && let Ok(value) = dw.as_float()
        {
            default_width = value;
        }
        if let Ok(w) = cid_font.get_deref(b"W", doc)
            && let Ok(array) = w.as_array()
        {
            ranges = parse_w_array(doc, array);
        }
    }
    CompositeWidths {
        ranges,
        default_width,
    }
}

/// Parse a CID font `/W` array, which mixes `c [w1 w2 ...]` and
/// `cfirst clast w` entries.
fn parse_w_array(doc: &Document, array: &[Object]) -> Vec<(u32, u32, f32)> {
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < array.len() {
        let Some(first_obj) = array.get(index) else {
            break;
        };
        let Some(first) = number(doc, first_obj).and_then(to_code) else {
            break;
        };
        let Some(second_obj) = array.get(index + 1) else {
            break;
        };
        let Ok((_, second)) = doc.dereference(second_obj) else {
            break;
        };
        if let Ok(list) = second.as_array() {
            let mut code = first;
            for item in list {
                if let Some(glyph_width) = number(doc, item) {
                    ranges.push((code, code, glyph_width));
                }
                code = code.saturating_add(1);
            }
            index += 2;
        } else {
            let Some(last) = second.as_float().ok().and_then(to_code) else {
                break;
            };
            let Some(third_obj) = array.get(index + 2) else {
                break;
            };
            let Some(glyph_width) = number(doc, third_obj) else {
                break;
            };
            ranges.push((first, last, glyph_width));
            index += 3;
        }
    }
    ranges
}

/// Fonts and resources visible at one nesting level (page or Form `XObject`).
struct Context<'a> {
    fonts: BTreeMap<Vec<u8>, LoadedFont<'a>>,
    resources: Vec<&'a Dictionary>,
}

fn find_font<'c, 'a>(contexts: &'c [Context<'a>], name: &[u8]) -> Option<&'c LoadedFont<'a>> {
    contexts
        .iter()
        .rev()
        .find_map(|layer| layer.fonts.get(name))
}

fn lookup_xobject<'a>(
    doc: &'a Document,
    contexts: &[Context<'a>],
    name: &[u8],
) -> Option<&'a Stream> {
    for layer in contexts.iter().rev() {
        for &resources in &layer.resources {
            if let Ok(xobjects) = resources.get_deref(b"XObject", doc)
                && let Ok(xobjects) = xobjects.as_dict()
                && let Ok(entry) = xobjects.get_deref(name, doc)
                && let Ok(stream) = entry.as_stream()
            {
                return Some(stream);
            }
        }
    }
    None
}

fn load_fonts_from_resources<'a>(
    doc: &'a Document,
    resources: &'a Dictionary,
    fonts: &mut BTreeMap<Vec<u8>, LoadedFont<'a>>,
) {
    let Ok(font_map) = resources.get_deref(b"Font", doc) else {
        return;
    };
    let Ok(font_map) = font_map.as_dict() else {
        return;
    };
    for (name, value) in font_map {
        if let Ok((_, entry)) = doc.dereference(value)
            && let Ok(font_dict) = entry.as_dict()
        {
            fonts.insert(name.clone(), load_font(doc, name, font_dict));
        }
    }
}

/// Graphics state as far as text placement needs it (saved by `q`/`Q`).
#[derive(Clone, Debug)]
struct GState {
    ctm: Matrix,
    /// Resource name set by `Tf`.
    font: Option<Vec<u8>>,
    size: f32,
    char_spacing: f32,
    word_spacing: f32,
    /// `Tz` / 100.
    hscale: f32,
    leading: f32,
    rise: f32,
}

impl Default for GState {
    fn default() -> Self {
        Self {
            ctm: Matrix::IDENTITY,
            font: None,
            size: 0.0,
            char_spacing: 0.0,
            word_spacing: 0.0,
            hscale: 1.0,
            leading: 0.0,
            rise: 0.0,
        }
    }
}

fn replacement_text(composite: bool, bytes: &[u8]) -> String {
    let codes = if composite {
        bytes.len().div_ceil(2)
    } else {
        bytes.len()
    };
    std::iter::repeat_n('\u{FFFD}', codes).collect()
}

struct Interpreter<'a> {
    doc: &'a Document,
    page: PageText,
    state: GState,
    stack: Vec<GState>,
    /// Text matrix.
    tm: Matrix,
    /// Text line matrix.
    tlm: Matrix,
    seq: u32,
    max_depth: u32,
}

impl<'a> Interpreter<'a> {
    fn warn(&mut self, message: String) {
        if !self.page.warnings.contains(&message) {
            self.page.warnings.push(message);
        }
    }

    fn run(&mut self, operations: &[Operation], contexts: &mut Vec<Context<'a>>, depth: u32) {
        for op in operations {
            let operands = op.operands.as_slice();
            match op.operator.as_str() {
                "q" => self.stack.push(self.state.clone()),
                "Q" => {
                    if let Some(state) = self.stack.pop() {
                        self.state = state;
                    }
                }
                "cm" => {
                    if let Some(matrix) = matrix_from_operands(operands) {
                        self.state.ctm = matrix.then(self.state.ctm);
                    }
                }
                "BT" => {
                    self.tm = Matrix::IDENTITY;
                    self.tlm = Matrix::IDENTITY;
                }
                "Tf" => self.set_font(operands),
                "Td" => {
                    if let Some(tx) = float_at(operands, 0)
                        && let Some(ty) = float_at(operands, 1)
                    {
                        self.text_move(tx, ty);
                    }
                }
                "TD" => {
                    if let Some(tx) = float_at(operands, 0)
                        && let Some(ty) = float_at(operands, 1)
                    {
                        self.state.leading = -ty;
                        self.text_move(tx, ty);
                    }
                }
                "Tm" => {
                    if let Some(matrix) = matrix_from_operands(operands) {
                        self.tm = matrix;
                        self.tlm = matrix;
                    }
                }
                "T*" => self.next_line(),
                "TL" => {
                    if let Some(value) = float_at(operands, 0) {
                        self.state.leading = value;
                    }
                }
                "Tc" => {
                    if let Some(value) = float_at(operands, 0) {
                        self.state.char_spacing = value;
                    }
                }
                "Tw" => {
                    if let Some(value) = float_at(operands, 0) {
                        self.state.word_spacing = value;
                    }
                }
                "Tz" => {
                    if let Some(value) = float_at(operands, 0) {
                        self.state.hscale = value / 100.0;
                    }
                }
                "Ts" => {
                    if let Some(value) = float_at(operands, 0) {
                        self.state.rise = value;
                    }
                }
                "Tj" => {
                    if let Some(bytes) = string_at(operands, 0) {
                        self.show(bytes, contexts);
                    }
                }
                "'" => {
                    self.next_line();
                    if let Some(bytes) = string_at(operands, 0) {
                        self.show(bytes, contexts);
                    }
                }
                "\"" => {
                    if let Some(value) = float_at(operands, 0) {
                        self.state.word_spacing = value;
                    }
                    if let Some(value) = float_at(operands, 1) {
                        self.state.char_spacing = value;
                    }
                    self.next_line();
                    if let Some(bytes) = string_at(operands, 2) {
                        self.show(bytes, contexts);
                    }
                }
                "TJ" => self.show_array(operands, contexts),
                "Do" => self.do_xobject(operands, contexts, depth),
                _ => {}
            }
        }
    }

    fn set_font(&mut self, operands: &[Object]) {
        if let Some(name) = operands.first().and_then(|obj| obj.as_name().ok()) {
            self.state.font = Some(name.to_vec());
        }
        if let Some(size) = float_at(operands, 1) {
            self.state.size = size;
        }
    }

    fn text_move(&mut self, tx: f32, ty: f32) {
        self.tlm = Matrix::translation(tx, ty).then(self.tlm);
        self.tm = self.tlm;
    }

    fn next_line(&mut self) {
        let leading = self.state.leading;
        self.text_move(0.0, -leading);
    }

    fn current_font<'c>(&self, contexts: &'c [Context<'a>]) -> Option<&'c LoadedFont<'a>> {
        let name = self.state.font.as_deref()?;
        find_font(contexts, name)
    }

    fn show(&mut self, bytes: &[u8], contexts: &[Context<'a>]) {
        if bytes.is_empty() {
            return;
        }
        let fallback: LoadedFont<'a>;
        let font = if let Some(found) = self.current_font(contexts) {
            found
        } else {
            let name = self.state.font.clone().unwrap_or_default();
            fallback = LoadedFont::missing(&name);
            &fallback
        };
        let text = self.decode(font, bytes);
        let advance = self.advance(font, bytes);
        let base_font = font.base_font.clone();
        self.emit(text, advance, base_font);
    }

    fn show_array(&mut self, operands: &[Object], contexts: &[Context<'a>]) {
        let Some(pieces) = operands.first().and_then(|obj| obj.as_array().ok()) else {
            return;
        };
        for element in pieces {
            if let Object::String(bytes, _) = element {
                self.show(bytes, contexts);
            } else if let Ok(adjust) = element.as_float() {
                let shift = -adjust / 1000.0 * self.state.size * self.state.hscale;
                self.tm = Matrix::translation(shift, 0.0).then(self.tm);
            }
        }
    }

    fn decode(&mut self, font: &LoadedFont<'_>, bytes: &[u8]) -> String {
        let label = &font.label;
        match &font.decode {
            Decode::Encoding(encoding) => self.decode_with(font, encoding, bytes),
            Decode::Latin1(reason) => {
                self.warn(format!("font {label}: {reason}; decoded as Latin-1"));
                bytes.iter().copied().map(char::from).collect()
            }
            Decode::Replacement => {
                self.warn(format!("font {label}: undecodable; U+FFFD substituted"));
                replacement_text(font.composite, bytes)
            }
        }
    }

    fn decode_with(&mut self, font: &LoadedFont<'_>, enc: &Encoding<'_>, bytes: &[u8]) -> String {
        let label = &font.label;
        let Ok(mut text) = Document::decode_text(enc, bytes) else {
            self.warn(format!("font {label}: undecodable string; U+FFFD substituted"));
            return replacement_text(font.composite, bytes);
        };
        if font.one_to_one {
            let decoded = text.chars().count();
            if decoded < bytes.len() {
                let dropped = bytes.len() - decoded;
                text.extend(std::iter::repeat_n('\u{FFFD}', dropped));
                self.warn(format!("font {label}: {dropped} unmapped byte(s); U+FFFD used"));
            }
        } else if text.contains('\u{FFFD}') {
            self.warn(format!("font {label}: unmapped code(s); U+FFFD substituted"));
        }
        text
    }

    /// Horizontal displacement of `bytes` in unscaled text space.
    fn advance(&self, font: &LoadedFont<'_>, bytes: &[u8]) -> f32 {
        let size = self.state.size;
        let mut total: f32 = 0.0;
        if font.composite {
            for pair in bytes.chunks(2) {
                let mut code: u32 = 0;
                for &unit in pair {
                    code = (code << 8) | u32::from(unit);
                }
                let glyph = font.widths.glyph_width(code) / 1000.0 * size;
                total += glyph + self.state.char_spacing;
            }
        } else {
            for &unit in bytes {
                let glyph = font.widths.glyph_width(u32::from(unit)) / 1000.0 * size;
                total += glyph + self.state.char_spacing;
                if unit == 32 {
                    total += self.state.word_spacing;
                }
            }
        }
        total * self.state.hscale
    }

    fn emit(&mut self, text: String, advance: f32, base_font: Option<String>) {
        let full = self.tm.then(self.state.ctm);
        let rise = self.state.rise;
        let size = self.state.size;
        let corners = [
            full.apply(0.0, rise + DESCENT * size),
            full.apply(0.0, rise + ASCENT * size),
            full.apply(advance, rise + DESCENT * size),
            full.apply(advance, rise + ASCENT * size),
        ];
        let mut bbox = BBox {
            x0: f32::MAX,
            y0: f32::MAX,
            x1: f32::MIN,
            y1: f32::MIN,
        };
        for (x, y) in corners {
            bbox.x0 = bbox.x0.min(x);
            bbox.y0 = bbox.y0.min(y);
            bbox.x1 = bbox.x1.max(x);
            bbox.y1 = bbox.y1.max(y);
        }
        let normalised: String = text.nfc().collect();
        self.page.spans.push(Span {
            text: normalised,
            bbox: Some(bbox),
            font: base_font,
            size: Some(size * full.y_scale()),
            seq: self.seq,
        });
        self.seq = self.seq.saturating_add(1);
        self.tm = Matrix::translation(advance, 0.0).then(self.tm);
    }

    fn do_xobject(&mut self, operands: &[Object], contexts: &mut Vec<Context<'a>>, depth: u32) {
        let doc = self.doc;
        let Some(name) = operands.first().and_then(|obj| obj.as_name().ok()) else {
            return;
        };
        let label = lossy(name);
        let Some(stream) = lookup_xobject(doc, contexts, name) else {
            self.warn(format!("XObject {label}: not in resources"));
            return;
        };
        let subtype = stream.dict.get(b"Subtype").and_then(Object::as_name);
        if !subtype.is_ok_and(|kind| kind == b"Form") {
            return;
        }
        if depth >= self.max_depth {
            let limit = self.max_depth;
            self.warn(format!("XObject {label}: nesting deeper than {limit}; skipped"));
            return;
        }
        let content_bytes = match stream.get_plain_content() {
            Ok(bytes) => bytes,
            Err(_) => stream.content.clone(),
        };
        let Ok(content) = Content::decode(&content_bytes) else {
            self.warn(format!("XObject {label}: undecodable content stream"));
            return;
        };
        let matrix = match stream.dict.get(b"Matrix").and_then(Object::as_array) {
            Ok(array) => matrix_from_operands(array).unwrap_or(Matrix::IDENTITY),
            Err(_) => Matrix::IDENTITY,
        };
        let mut form_context = Context {
            fonts: BTreeMap::new(),
            resources: Vec::new(),
        };
        if let Ok(resources) = stream.dict.get_deref(b"Resources", doc)
            && let Ok(resources) = resources.as_dict()
        {
            form_context.resources.push(resources);
            load_fonts_from_resources(doc, resources, &mut form_context.fonts);
        }

        let saved_state = self.state.clone();
        let saved_depth = self.stack.len();
        self.state.ctm = matrix.then(self.state.ctm);
        contexts.push(form_context);
        self.run(&content.operations, contexts, depth + 1);
        contexts.pop();
        self.stack.truncate(saved_depth);
        self.state = saved_state;
    }
}

fn extract_page(
    doc: &Document,
    page: u32,
    page_id: ObjectId,
    max_depth: u32,
) -> Result<PageText, BackendError> {
    let page_dict = match doc.get_dictionary(page_id) {
        Ok(dict) => dict,
        Err(err) => return Err(page_error(page, format!("page dictionary: {err}"))),
    };

    let crop = inherited(doc, page_dict, b"CropBox").and_then(rect_size);
    let media = inherited(doc, page_dict, b"MediaBox").and_then(rect_size);
    let size = crop.or(media);
    let (width, height) = size.unwrap_or((0.0, 0.0));
    let mut page_text = PageText::new(page, width, height, page_rotation(doc, page_dict));
    if size.is_none() {
        let message = "no MediaBox or CropBox: page size unknown".to_string();
        page_text.warnings.push(message);
    }

    let content_bytes = doc.get_page_content(page_id);
    let content = match Content::decode(&content_bytes) {
        Ok(content) => content,
        Err(err) => return Err(page_error(page, format!("content stream: {err}"))),
    };

    let mut page_context = Context {
        fonts: BTreeMap::new(),
        resources: Vec::new(),
    };
    match doc.get_page_fonts(page_id) {
        Ok(fonts) => {
            for (name, dict) in fonts {
                let loaded = load_font(doc, &name, dict);
                page_context.fonts.insert(name, loaded);
            }
        }
        Err(err) => page_text.warnings.push(format!("fonts: {err}")),
    }
    if let Ok((direct, ids)) = doc.get_page_resources(page_id) {
        if let Some(dict) = direct {
            page_context.resources.push(dict);
        }
        for id in ids {
            if let Ok(dict) = doc.get_dictionary(id) {
                page_context.resources.push(dict);
            }
        }
    }

    let mut interpreter = Interpreter {
        doc,
        page: page_text,
        state: GState::default(),
        stack: Vec::new(),
        tm: Matrix::IDENTITY,
        tlm: Matrix::IDENTITY,
        seq: 0,
        max_depth,
    };
    let mut contexts = vec![page_context];
    interpreter.run(&content.operations, &mut contexts, 0);
    Ok(interpreter.page)
}

#[cfg(test)]
mod tests {
    use lopdf::dictionary;

    use super::*;

    fn close(actual: f32, expected: f32) -> bool {
        (actual - expected).abs() < 1e-3
    }

    /// `BT /F1 size Tf x y Td (text) Tj ET`.
    fn text_ops(size: i32, x: i32, y: i32, text: &str) -> Vec<Operation> {
        vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), size.into()]),
            Operation::new("Td", vec![x.into(), y.into()]),
            Operation::new("Tj", vec![Object::string_literal(text)]),
            Operation::new("ET", vec![]),
        ]
    }

    /// `1 0 0 1 tx ty cm`.
    fn cm_translate(tx: i32, ty: i32) -> Operation {
        let matrix = vec![1.into(), 0.into(), 0.into(), 1.into(), tx.into(), ty.into()];
        Operation::new("cm", matrix)
    }

    /// Build a PDF with Helvetica as `/F1`, one page per operation list, and
    /// optionally a Form `XObject` `/X1` holding `form` with the same font.
    fn build_pdf(pages: Vec<Vec<Operation>>, form: Option<Vec<Operation>>) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let tree_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let mut resources = dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        };
        if let Some(operations) = form {
            let form_content = Content { operations }.encode().unwrap();
            let form_dict = dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                "Resources" => dictionary! { "Font" => dictionary! { "F1" => font_id } },
            };
            let form_id = doc.add_object(Stream::new(form_dict, form_content));
            resources.set("XObject", dictionary! { "X1" => form_id });
        }
        let resources_id = doc.add_object(resources);
        let mut kids = Vec::new();
        for operations in pages {
            let content = Content { operations }.encode().unwrap();
            let content_id = doc.add_object(Stream::new(dictionary! {}, content));
            let page_id = doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => tree_id,
                "Contents" => content_id,
                "Resources" => resources_id,
            });
            kids.push(Object::Reference(page_id));
        }
        let count = i64::try_from(kids.len()).unwrap();
        let tree = dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => count,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        };
        doc.objects.insert(tree_id, Object::Dictionary(tree));
        let info_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal("Test Title"),
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => tree_id,
        });
        doc.trailer.set("Root", catalog_id);
        doc.trailer.set("Info", info_id);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn identity_is_stable() {
        let identity = LopdfBackend::default().identity();
        assert_eq!(identity.name, "lopdf");
        assert_eq!(identity.version, "0.45");
        let mut config = BTreeMap::new();
        config.insert("max_xobject_depth".to_string(), "8".to_string());
        assert_eq!(identity.config_digest, config_digest(&config));
    }

    #[test]
    fn two_text_blocks_have_positions_and_sizes() {
        let ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![100.into(), 600.into()]),
            Operation::new("Tj", vec![Object::string_literal("Hello")]),
            Operation::new("Tf", vec!["F1".into(), 24.into()]),
            Operation::new("Td", vec![0.into(), (-40).into()]),
            Operation::new("Tj", vec![Object::string_literal("World")]),
            Operation::new("ET", vec![]),
        ];
        let bytes = build_pdf(vec![ops, text_ops(10, 72, 700, "Page two")], None);
        let backend = LopdfBackend::default();
        let mut session = backend.open(&bytes, None).unwrap();
        assert_eq!(session.page_count(), 2);

        let page = session.page_text(1).unwrap();
        assert_eq!(page.page, 1);
        assert!(close(page.width, 612.0), "width {}", page.width);
        assert!(close(page.height, 792.0), "height {}", page.height);
        assert_eq!(page.rotation, 0);
        assert!(page.warnings.is_empty(), "{:?}", page.warnings);
        assert!(page.lines.is_empty());
        assert!(page.text.is_empty());

        let texts: Vec<&str> = page.spans.iter().map(|span| span.text.as_str()).collect();
        assert_eq!(texts, vec!["Hello", "World"]);

        let hello = &page.spans[0];
        let hello_box = hello.bbox.unwrap();
        assert!(close(hello_box.x0, 100.0), "x0 {}", hello_box.x0);
        assert!(close(hello_box.x1, 130.0), "x1 {}", hello_box.x1);
        assert!(close(hello_box.y0, 597.6), "y0 {}", hello_box.y0);
        assert!(close(hello_box.y1, 609.6), "y1 {}", hello_box.y1);
        assert!(close(hello.size.unwrap(), 12.0));
        assert_eq!(hello.font.as_deref(), Some("Helvetica"));
        assert_eq!(hello.seq, 0);

        let world = &page.spans[1];
        let world_box = world.bbox.unwrap();
        assert!(close(world_box.x0, 100.0), "x0 {}", world_box.x0);
        assert!(close(world_box.x1, 160.0), "x1 {}", world_box.x1);
        assert!(close(world_box.y0, 555.2), "y0 {}", world_box.y0);
        assert!(close(world_box.y1, 579.2), "y1 {}", world_box.y1);
        assert!(close(world.size.unwrap(), 24.0));
        assert_eq!(world.seq, 1);

        let second = session.page_text(2).unwrap();
        assert_eq!(second.page, 2);
        assert_eq!(second.spans.len(), 1);
        assert_eq!(second.spans[0].text, "Page two");
        let second_box = second.spans[0].bbox.unwrap();
        assert!(close(second_box.x0, 72.0), "x0 {}", second_box.x0);
        assert!(close(second_box.y0, 698.0), "y0 {}", second_box.y0);
        assert!(close(second.spans[0].size.unwrap(), 10.0));
    }

    #[test]
    fn info_exposes_string_entries() {
        let bytes = build_pdf(vec![text_ops(12, 10, 10, "x")], None);
        let session = LopdfBackend::default().open(&bytes, None).unwrap();
        let info = session.info();
        assert_eq!(info.get("Title").map(String::as_str), Some("Test Title"));
    }

    #[test]
    fn tj_adjustments_shift_position() {
        let pieces = vec![
            Object::string_literal("A"),
            Object::Integer(-500),
            Object::string_literal("B"),
        ];
        let ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 10.into()]),
            Operation::new("Td", vec![50.into(), 50.into()]),
            Operation::new("TJ", vec![Object::Array(pieces)]),
            Operation::new("ET", vec![]),
        ];
        let bytes = build_pdf(vec![ops], None);
        let mut session = LopdfBackend::default().open(&bytes, None).unwrap();
        let page = session.page_text(1).unwrap();
        assert_eq!(page.spans.len(), 2);
        let left_box = page.spans[0].bbox.unwrap();
        let right_box = page.spans[1].bbox.unwrap();
        assert!(close(left_box.x0, 50.0), "A x0 {}", left_box.x0);
        assert!(close(left_box.x1, 55.0), "A x1 {}", left_box.x1);
        // -500/1000 * 10 pt moves the next glyph 5 pt to the right.
        assert!(close(right_box.x0, 60.0), "B x0 {}", right_box.x0);
    }

    #[test]
    fn form_xobject_is_placed_through_cm() {
        let ops = vec![
            Operation::new("q", vec![]),
            cm_translate(200, 300),
            Operation::new("Do", vec!["X1".into()]),
            Operation::new("Q", vec![]),
        ];
        let form = text_ops(10, 50, 50, "Form");
        let bytes = build_pdf(vec![ops], Some(form));
        let mut session = LopdfBackend::default().open(&bytes, None).unwrap();
        let page = session.page_text(1).unwrap();
        assert!(page.warnings.is_empty(), "{:?}", page.warnings);
        assert_eq!(page.spans.len(), 1);
        assert_eq!(page.spans[0].text, "Form");
        let form_box = page.spans[0].bbox.unwrap();
        assert!(close(form_box.x0, 250.0), "x0 {}", form_box.x0);
        assert!(close(form_box.y0, 348.0), "y0 {}", form_box.y0);
        assert!(close(form_box.y1, 358.0), "y1 {}", form_box.y1);
        assert!(close(page.spans[0].size.unwrap(), 10.0));
    }

    #[test]
    fn xobject_depth_limit_is_enforced() {
        let ops = vec![Operation::new("Do", vec!["X1".into()])];
        let form = text_ops(10, 50, 50, "Form");
        let bytes = build_pdf(vec![ops], Some(form));
        let backend = LopdfBackend {
            max_xobject_depth: 0,
        };
        let mut session = backend.open(&bytes, None).unwrap();
        let page = session.page_text(1).unwrap();
        assert!(page.spans.is_empty());
        assert_eq!(page.warnings.len(), 1);
        assert!(page.warnings[0].contains("nesting"), "{:?}", page.warnings);
    }

    #[test]
    fn missing_font_falls_back_to_latin1_with_warning() {
        let ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F9".into(), 12.into()]),
            Operation::new("Td", vec![10.into(), 10.into()]),
            Operation::new("Tj", vec![Object::string_literal("Hi")]),
            Operation::new("ET", vec![]),
        ];
        let bytes = build_pdf(vec![ops], None);
        let mut session = LopdfBackend::default().open(&bytes, None).unwrap();
        let page = session.page_text(1).unwrap();
        assert_eq!(page.spans.len(), 1);
        assert_eq!(page.spans[0].text, "Hi");
        assert_eq!(page.spans[0].font, None);
        let mentions_f9 = page.warnings.iter().any(|w| w.contains("F9"));
        assert!(mentions_f9, "{:?}", page.warnings);
    }

    #[test]
    fn page_out_of_range_is_reported() {
        let bytes = build_pdf(vec![text_ops(12, 10, 10, "x")], None);
        let mut session = LopdfBackend::default().open(&bytes, None).unwrap();
        let err = session.page_text(2).unwrap_err();
        match err {
            BackendError::PageRange { page, count } => {
                assert_eq!(page, 2);
                assert_eq!(count, 1);
            }
            other => panic!("expected PageRange, got {other:?}"),
        }
        assert!(matches!(session.page_text(0), Err(BackendError::PageRange { .. })));
    }

    #[test]
    fn malformed_bytes_are_rejected() {
        let backend = LopdfBackend::default();
        let err = backend.open(b"not a pdf at all", None).unwrap_err();
        assert!(matches!(err, BackendError::Malformed(_)), "{err}");
    }
}

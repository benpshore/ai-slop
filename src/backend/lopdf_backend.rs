//! Pure-Rust backend built on `lopdf` 0.45. It interprets each page's
//! content stream (text state, graphics state, Form `XObject`s) and yields
//! one positioned [`Span`] per shown string. Nothing is ordered or repaired.
//!
//! Per-document work is cached inside the session: a font dictionary is
//! resolved (encoding, widths, flags) once per `ObjectId` and shared by
//! every page and Form `XObject` that references it, and a Form `XObject`'s
//! content stream is decompressed and parsed once. The caches hold only
//! owned data, so they never borrow the [`Document`] they were built from.
//!
//! Text is normalised, never repaired: every non-ASCII string is put in NFC,
//! and the Latin presentation-form ligatures U+FB00 to U+FB06 (`ﬀ ﬁ ﬂ ﬃ ﬄ ﬅ
//! ﬆ`) are expanded to their letters, which NFC alone keeps. Nothing else
//! gets a compatibility mapping (no general NFKC), so superscripts, vulgar
//! fractions and mathematical alphanumerics survive as written. A page on
//! which any ligature was expanded carries the warning
//! `ligatures expanded: N`. Because the expansion changes the text produced
//! for the same bytes, it is part of the backend identity: the config map
//! behind the digest holds `ligatures=expand`, so runs from before the
//! change never share an identity with runs after it.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use lopdf::content::{Content, Operation};
use lopdf::{
    Dictionary, Document, Encoding, Error as LopdfError, LoadOptions, Object, ObjectId, Stream,
};
use unicode_normalization::UnicodeNormalization;

use crate::backend::{BackendError, DocumentSession, EncryptionProblem, Extractor};
use crate::schema::{BBox, BackendIdentity, PageText, Span, config_digest};

/// The `lopdf` release this backend is built against. It is part of the
/// [`BackendIdentity`], so a dependency bump must change it (a unit test
/// checks it against `Cargo.lock`).
const LOPDF_VERSION: &str = "0.45.0";
/// Glyph width (in 1/1000 em) assumed when a font declares nothing usable.
const DEFAULT_WIDTH: f32 = 500.0;
/// Glyph-space to text-space factor for every font type except Type3.
const THOUSANDTH: f32 = 0.001;
/// Descent estimate below the baseline, as a fraction of the font size.
const DESCENT: f32 = -0.2;
/// Ascent estimate above the baseline, as a fraction of the font size.
const ASCENT: f32 = 0.8;
/// Bound on the `/Parent` walk used for inherited page attributes.
const MAX_PARENT_DEPTH: u32 = 64;
/// Recorded in the identity's config map: see the module documentation.
const LIGATURE_POLICY: &str = "expand";

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
    /// Name `lopdf`, version [`LOPDF_VERSION`], digest over `max_xobject_depth`
    /// and the ligature policy.
    fn identity(&self) -> BackendIdentity {
        let mut config = BTreeMap::new();
        config.insert(
            "max_xobject_depth".to_string(),
            self.max_xobject_depth.to_string(),
        );
        config.insert("ligatures".to_string(), LIGATURE_POLICY.to_string());
        BackendIdentity {
            name: "lopdf".to_string(),
            version: LOPDF_VERSION.to_string(),
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
            cache: SessionCache::default(),
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

/// Work that is identical for every page of one document, computed on first
/// use and kept for the life of the session.
#[derive(Default)]
struct SessionCache {
    /// Resolved font dictionaries, keyed by the indirect object they live in.
    /// Fonts written directly into a resources dictionary have no id and are
    /// resolved on every use.
    fonts: HashMap<ObjectId, Rc<LoadedFont>>,
    /// Decoded content of Form `XObject` streams, keyed by stream id.
    /// Streams that fail to decode are not cached, so their warning recurs
    /// exactly as it would without the cache.
    forms: HashMap<ObjectId, Rc<Vec<Operation>>>,
}

struct LopdfSession {
    doc: Document,
    pages: BTreeMap<u32, ObjectId>,
    max_xobject_depth: u32,
    cache: SessionCache,
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
        extract_page(
            &self.doc,
            &mut self.cache,
            page,
            page_id,
            self.max_xobject_depth,
        )
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
    /// `/Widths`, in glyph space.
    widths: Vec<f32>,
    /// `/MissingWidth`, in glyph space.
    missing: Option<f32>,
    /// Glyph space to text space: 1/1000 for `Type1`/`TrueType`, the horizontal
    /// scale of `/FontMatrix` for Type3.
    glyph_scale: f32,
}

impl SimpleWidths {
    fn unknown() -> Self {
        Self {
            first_char: 0,
            widths: Vec::new(),
            missing: None,
            glyph_scale: THOUSANDTH,
        }
    }

    /// Advance of `code` in text space (1.0 = the font size).
    fn width(&self, code: u32) -> f32 {
        let fallback = match self.missing {
            Some(missing) => missing * self.glyph_scale,
            None => DEFAULT_WIDTH * THOUSANDTH,
        };
        let Some(offset) = code.checked_sub(self.first_char) else {
            return fallback;
        };
        let Ok(index) = usize::try_from(offset) else {
            return fallback;
        };
        match self.widths.get(index) {
            Some(glyph_width) => glyph_width * self.glyph_scale,
            None => fallback,
        }
    }
}

/// Glyph widths of a composite (Type0) font, keyed by CID.
struct CompositeWidths {
    /// `(first, last, width)` runs from the `/W` array.
    ranges: Vec<(u32, u32, f32)>,
    default_width: f32,
}

impl CompositeWidths {
    /// Advance of `cid` in text space (1.0 = the font size).
    fn width(&self, cid: u32) -> f32 {
        for &(first, last, glyph_width) in &self.ranges {
            if (first..=last).contains(&cid) {
                return glyph_width * THOUSANDTH;
            }
        }
        self.default_width * THOUSANDTH
    }
}

enum Widths {
    Simple(SimpleWidths),
    Composite(CompositeWidths),
}

impl Widths {
    /// Advance of `code` in text space (1.0 = the font size).
    fn text_width(&self, code: u32) -> f32 {
        match self {
            Self::Simple(simple) => simple.width(code),
            Self::Composite(composite) => composite.width(code),
        }
    }
}

/// A single-byte encoding flattened into one entry per byte value.
///
/// `lopdf` decodes its `OneByteEncoding` and `Differences` encodings one
/// byte at a time, each byte independently of its neighbours: the byte maps
/// to zero or more chars, or the whole string is rejected. Entry `b` is
/// therefore exactly what `Document::decode_text` produces for the string
/// `[b]`, and `None` where it fails, so decoding through the table yields
/// the same text (and the same failures) as decoding through `lopdf`
/// without touching the encoding's glyph tables or the `Differences` map.
struct ByteTable {
    entries: Vec<Option<String>>,
}

impl ByteTable {
    fn build(encoding: &Encoding<'_>) -> Self {
        let mut entries = Vec::with_capacity(256);
        for byte in 0..=u8::MAX {
            entries.push(Document::decode_text(encoding, &[byte]).ok());
        }
        Self { entries }
    }

    /// The text for `bytes`, or `None` where `lopdf` would have failed.
    fn decode(&self, bytes: &[u8]) -> Option<String> {
        let mut out = String::with_capacity(bytes.len());
        for &byte in bytes {
            let Some(Some(piece)) = self.entries.get(usize::from(byte)) else {
                return None;
            };
            out.push_str(piece);
        }
        Some(out)
    }
}

/// How the bytes of a shown string become text. Owns everything it needs,
/// so a font can outlive the page it was first seen on.
enum Decode {
    /// A single-byte encoding, flattened (see [`ByteTable`]).
    Table(ByteTable),
    /// A predefined `CMap` name `lopdf` handles as `SimpleEncoding`; rebuilt
    /// (a free borrow) for each string.
    Named(Vec<u8>),
    /// A parsed `/ToUnicode` `CMap`, wrapped as `Encoding::UnicodeMapEncoding`.
    UnicodeMap(Encoding<'static>),
    /// No encoding is available (for the given reason); bytes are Latin-1.
    Latin1(&'static str),
    /// The font cannot be decoded at all; every code becomes U+FFFD.
    Replacement,
}

/// Turn the encoding `lopdf` resolved (which borrows the document) into an
/// owned decoder that produces the same text.
fn own_encoding(encoding: Encoding<'_>) -> Decode {
    match encoding {
        Encoding::OneByteEncoding(_) | Encoding::Differences(_) => {
            Decode::Table(ByteTable::build(&encoding))
        }
        Encoding::SimpleEncoding(name) => Decode::Named(name.to_vec()),
        Encoding::UnicodeMapEncoding(cmap) => {
            Decode::UnicodeMap(Encoding::UnicodeMapEncoding(cmap))
        }
    }
}

/// A font dictionary resolved once: everything the interpreter needs to show
/// a string with it. The resource name is not part of it, as one font object
/// may be reachable under different names on different pages.
struct LoadedFont {
    /// `/BaseFont` if present.
    base_font: Option<String>,
    decode: Decode,
    /// Two-byte codes (Type0), otherwise single-byte.
    composite: bool,
    /// Decoding yields exactly one char per byte, so dropped bytes are detectable.
    one_to_one: bool,
    widths: Widths,
}

impl LoadedFont {
    fn missing() -> Self {
        Self {
            base_font: None,
            decode: Decode::Latin1("not in resources"),
            composite: false,
            one_to_one: false,
            widths: Widths::Simple(SimpleWidths::unknown()),
        }
    }
}

fn load_font(doc: &Document, dict: &Dictionary) -> LoadedFont {
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
        base_font: base_font.map(lossy),
        decode,
        composite,
        one_to_one,
        widths,
    }
}

fn simple_decode(doc: &Document, dict: &Dictionary) -> (Decode, bool) {
    match dict.get_font_encoding(doc) {
        Ok(encoding) => {
            let one_to_one = !matches!(encoding, Encoding::UnicodeMapEncoding(_));
            (own_encoding(encoding), one_to_one)
        }
        Err(_) => (Decode::Latin1("no usable encoding"), false),
    }
}

fn composite_decode(doc: &Document, dict: &Dictionary) -> Decode {
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
        Ok(encoding) => own_encoding(encoding),
        Err(_) => Decode::Replacement,
    }
}

/// Horizontal glyph-space scale of a Type3 font's `/FontMatrix`
/// (`[a b c d e f]`, default `[0.001 0 0 0.001 0 0]`): a horizontal advance
/// `w` in glyph space is `w * a` in text space.
fn type3_glyph_scale(doc: &Document, dict: &Dictionary) -> f32 {
    let Ok(value) = dict.get_deref(b"FontMatrix", doc) else {
        return THOUSANDTH;
    };
    let Ok(array) = value.as_array() else {
        return THOUSANDTH;
    };
    if array.len() != 6 {
        return THOUSANDTH;
    }
    let Some(first) = array.first() else {
        return THOUSANDTH;
    };
    match number(doc, first) {
        Some(scale) if scale.is_finite() => scale,
        _ => THOUSANDTH,
    }
}

fn simple_widths(doc: &Document, dict: &Dictionary) -> SimpleWidths {
    let subtype = dict.get(b"Subtype").and_then(Object::as_name);
    let glyph_scale = if subtype.is_ok_and(|name| name == b"Type3") {
        type3_glyph_scale(doc, dict)
    } else {
        THOUSANDTH
    };
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
        glyph_scale,
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
    fonts: BTreeMap<Vec<u8>, Rc<LoadedFont>>,
    resources: Vec<&'a Dictionary>,
}

fn find_font<'c>(contexts: &'c [Context<'_>], name: &[u8]) -> Option<&'c LoadedFont> {
    contexts
        .iter()
        .rev()
        .find_map(|layer| layer.fonts.get(name))
        .map(Rc::as_ref)
}

/// The stream behind `/XObject name`, with the id of the indirect object
/// that holds it (`None` for a stream written directly into the resources).
fn lookup_xobject<'a>(
    doc: &'a Document,
    contexts: &[Context<'a>],
    name: &[u8],
) -> Option<(Option<ObjectId>, &'a Stream)> {
    for layer in contexts.iter().rev() {
        for &resources in &layer.resources {
            if let Ok(xobjects) = resources.get_deref(b"XObject", doc)
                && let Ok(xobjects) = xobjects.as_dict()
                && let Ok(entry) = xobjects.get(name)
                && let Ok((id, entry)) = doc.dereference(entry)
                && let Ok(stream) = entry.as_stream()
            {
                return Some((id, stream));
            }
        }
    }
    None
}

/// The font `value` (an entry of a `/Font` dictionary) denotes, from the
/// cache when it is an indirect object seen before.
fn resolve_font(
    doc: &Document,
    cache: &mut SessionCache,
    value: &Object,
) -> Option<Rc<LoadedFont>> {
    let (id, entry) = doc.dereference(value).ok()?;
    let dict = entry.as_dict().ok()?;
    match id {
        Some(id) => {
            let font = cache
                .fonts
                .entry(id)
                .or_insert_with(|| Rc::new(load_font(doc, dict)));
            Some(Rc::clone(font))
        }
        None => Some(Rc::new(load_font(doc, dict))),
    }
}

/// Add the fonts of one resources dictionary to `fonts`; a name already
/// present wins, matching `Document::get_page_fonts` (page resources before
/// inherited ones).
fn load_fonts_from_resources(
    doc: &Document,
    cache: &mut SessionCache,
    resources: &Dictionary,
    fonts: &mut BTreeMap<Vec<u8>, Rc<LoadedFont>>,
) {
    let Ok(font_map) = resources.get_deref(b"Font", doc) else {
        return;
    };
    let Ok(font_map) = font_map.as_dict() else {
        return;
    };
    for (name, value) in font_map {
        if let Entry::Vacant(slot) = fonts.entry(name.clone())
            && let Some(font) = resolve_font(doc, cache, value)
        {
            slot.insert(font);
        }
    }
}

/// Graphics state as far as text placement needs it (saved by `q`/`Q`).
#[derive(Clone, Debug)]
struct GState {
    ctm: Matrix,
    /// Resource name set by `Tf`; shared so `q` does not copy it.
    font: Option<Rc<[u8]>>,
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

/// The letters a Latin presentation-form ligature (U+FB00 to U+FB06) stands
/// for, as their compatibility decomposition gives them (long s as `s`).
fn ligature_letters(ch: char) -> Option<&'static str> {
    match ch {
        '\u{FB00}' => Some("ff"),
        '\u{FB01}' => Some("fi"),
        '\u{FB02}' => Some("fl"),
        '\u{FB03}' => Some("ffi"),
        '\u{FB04}' => Some("ffl"),
        '\u{FB05}' | '\u{FB06}' => Some("st"),
        _ => None,
    }
}

/// `text` with every Latin ligature expanded, and how many were expanded.
fn expand_ligatures(text: String) -> (String, u32) {
    if !text.chars().any(|ch| ligature_letters(ch).is_some()) {
        return (text, 0);
    }
    let mut out = String::with_capacity(text.len() + 4);
    let mut count: u32 = 0;
    for ch in text.chars() {
        match ligature_letters(ch) {
            Some(letters) => {
                out.push_str(letters);
                count = count.saturating_add(1);
            }
            None => out.push(ch),
        }
    }
    (out, count)
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
    cache: &'a mut SessionCache,
    page: PageText,
    state: GState,
    stack: Vec<GState>,
    /// Text matrix.
    tm: Matrix,
    /// Text line matrix.
    tlm: Matrix,
    seq: u32,
    max_depth: u32,
    /// Ligatures expanded so far on this page.
    ligatures: u32,
}

impl<'a> Interpreter<'a> {
    fn warn(&mut self, message: String) {
        if !self.page.warnings.contains(&message) {
            self.page.warnings.push(message);
        }
    }

    /// The finished page, with the ligature count recorded as one warning.
    fn finish(mut self) -> PageText {
        if self.ligatures > 0 {
            let count = self.ligatures;
            self.page
                .warnings
                .push(format!("ligatures expanded: {count}"));
        }
        self.page
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
            self.state.font = Some(Rc::from(name));
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

    fn show(&mut self, bytes: &[u8], contexts: &[Context<'a>]) {
        if bytes.is_empty() {
            return;
        }
        let font_name = self.state.font.clone();
        let name: &[u8] = font_name.as_deref().unwrap_or_default();
        let fallback: LoadedFont;
        let font = if let Some(found) = find_font(contexts, name) {
            found
        } else {
            fallback = LoadedFont::missing();
            &fallback
        };
        let text = self.decode(name, font, bytes);
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

    /// Decode `bytes` shown with the font resource `name`.
    fn decode(&mut self, name: &[u8], font: &LoadedFont, bytes: &[u8]) -> String {
        match &font.decode {
            Decode::Table(table) => match table.decode(bytes) {
                Some(text) => self.check_unmapped(name, font, text, bytes),
                None => self.undecodable(name, font, bytes),
            },
            Decode::Named(encoding_name) => {
                let encoding = Encoding::SimpleEncoding(encoding_name);
                self.decode_with(name, font, &encoding, bytes)
            }
            Decode::UnicodeMap(encoding) => self.decode_with(name, font, encoding, bytes),
            Decode::Latin1(reason) => {
                let label = lossy(name);
                self.warn(format!("font {label}: {reason}; decoded as Latin-1"));
                bytes.iter().copied().map(char::from).collect()
            }
            Decode::Replacement => {
                let label = lossy(name);
                self.warn(format!("font {label}: undecodable; U+FFFD substituted"));
                replacement_text(font.composite, bytes)
            }
        }
    }

    fn decode_with(
        &mut self,
        name: &[u8],
        font: &LoadedFont,
        enc: &Encoding<'_>,
        bytes: &[u8],
    ) -> String {
        match Document::decode_text(enc, bytes) {
            Ok(text) => self.check_unmapped(name, font, text, bytes),
            Err(_) => self.undecodable(name, font, bytes),
        }
    }

    fn undecodable(&mut self, name: &[u8], font: &LoadedFont, bytes: &[u8]) -> String {
        let label = lossy(name);
        self.warn(format!(
            "font {label}: undecodable string; U+FFFD substituted"
        ));
        replacement_text(font.composite, bytes)
    }

    /// Account for codes the encoding silently dropped or replaced.
    fn check_unmapped(
        &mut self,
        name: &[u8],
        font: &LoadedFont,
        mut text: String,
        bytes: &[u8],
    ) -> String {
        if font.one_to_one {
            let decoded = text.chars().count();
            if decoded < bytes.len() {
                let dropped = bytes.len() - decoded;
                text.extend(std::iter::repeat_n('\u{FFFD}', dropped));
                let label = lossy(name);
                self.warn(format!(
                    "font {label}: {dropped} unmapped byte(s); U+FFFD used"
                ));
            }
        } else if text.contains('\u{FFFD}') {
            let label = lossy(name);
            self.warn(format!(
                "font {label}: unmapped code(s); U+FFFD substituted"
            ));
        }
        text
    }

    /// Horizontal displacement of `bytes` in unscaled text space.
    fn advance(&self, font: &LoadedFont, bytes: &[u8]) -> f32 {
        let size = self.state.size;
        let mut total: f32 = 0.0;
        if font.composite {
            for pair in bytes.chunks(2) {
                let mut code: u32 = 0;
                for &unit in pair {
                    code = (code << 8) | u32::from(unit);
                }
                let glyph = font.widths.text_width(code) * size;
                total += glyph + self.state.char_spacing;
            }
        } else {
            for &unit in bytes {
                let glyph = font.widths.text_width(u32::from(unit)) * size;
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
        // NFC and ligature expansion leave pure ASCII untouched, so the
        // common case skips both and their allocations. Ligatures are
        // expanded before NFC so the result is still NFC when a combining
        // mark follows one (`ﬁ` + U+0301 composes to `fí`); NFC itself never
        // touches them, so the count is the same either way.
        let normalised = if text.is_ascii() {
            text
        } else {
            let (expanded, count) = expand_ligatures(text);
            self.ligatures = self.ligatures.saturating_add(count);
            expanded.nfc().collect()
        };
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
        let Some((stream_id, stream)) = lookup_xobject(doc, contexts, name) else {
            self.warn(format!("XObject {label}: not in resources"));
            return;
        };
        let subtype = stream.dict.get(b"Subtype").and_then(Object::as_name);
        if !subtype.is_ok_and(|kind| kind == b"Form") {
            return;
        }
        if depth >= self.max_depth {
            let limit = self.max_depth;
            self.warn(format!(
                "XObject {label}: nesting deeper than {limit}; skipped"
            ));
            return;
        }
        let cached = stream_id.and_then(|id| self.cache.forms.get(&id).map(Rc::clone));
        let operations = if let Some(operations) = cached {
            operations
        } else {
            let content_bytes = match stream.get_plain_content() {
                Ok(bytes) => bytes,
                Err(_) => stream.content.clone(),
            };
            let Ok(content) = Content::decode(&content_bytes) else {
                self.warn(format!("XObject {label}: undecodable content stream"));
                return;
            };
            let operations = Rc::new(content.operations);
            if let Some(id) = stream_id {
                self.cache.forms.insert(id, Rc::clone(&operations));
            }
            operations
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
            load_fonts_from_resources(doc, self.cache, resources, &mut form_context.fonts);
        }

        let saved_state = self.state.clone();
        let saved_depth = self.stack.len();
        self.state.ctm = matrix.then(self.state.ctm);
        contexts.push(form_context);
        self.run(operations.as_slice(), contexts, depth + 1);
        contexts.pop();
        self.stack.truncate(saved_depth);
        self.state = saved_state;
    }
}

fn extract_page(
    doc: &Document,
    cache: &mut SessionCache,
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
    // Same walk as `Document::get_page_fonts` (the page's direct resources,
    // then the indirect ones up the `/Parent` chain; first name wins), but
    // each font dictionary is resolved through the session cache.
    match doc.get_page_resources(page_id) {
        Ok((direct, ids)) => {
            if let Some(dict) = direct {
                page_context.resources.push(dict);
            }
            for id in ids {
                if let Ok(dict) = doc.get_dictionary(id) {
                    page_context.resources.push(dict);
                }
            }
            for &resources in &page_context.resources {
                load_fonts_from_resources(doc, cache, resources, &mut page_context.fonts);
            }
        }
        Err(err) => page_text.warnings.push(format!("fonts: {err}")),
    }

    let mut interpreter = Interpreter {
        doc,
        cache,
        page: page_text,
        state: GState::default(),
        stack: Vec::new(),
        tm: Matrix::IDENTITY,
        tlm: Matrix::IDENTITY,
        seq: 0,
        max_depth,
        ligatures: 0,
    };
    let mut contexts = vec![page_context];
    interpreter.run(&content.operations, &mut contexts, 0);
    Ok(interpreter.finish())
}

#[cfg(test)]
mod tests {
    use lopdf::{StringFormat, dictionary};

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
        build_pdf_with_font(pages, form, |_| {
            dictionary! {
                "Type" => "Font",
                "Subtype" => "Type1",
                "BaseFont" => "Helvetica",
            }
        })
    }

    /// Like [`build_pdf`] but `/F1` is the font dictionary `make_font`
    /// returns (it may add its own objects to the document first).
    fn build_pdf_with_font<F>(
        pages: Vec<Vec<Operation>>,
        form: Option<Vec<Operation>>,
        make_font: F,
    ) -> Vec<u8>
    where
        F: FnOnce(&mut Document) -> Dictionary,
    {
        let mut doc = Document::with_version("1.5");
        let tree_id = doc.new_object_id();
        let font_dict = make_font(&mut doc);
        let font_id = doc.add_object(font_dict);
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

    /// A session whose caches the test can inspect.
    fn open_session(bytes: &[u8]) -> LopdfSession {
        let doc = load_document(bytes, None).unwrap();
        let pages = doc.get_pages();
        LopdfSession {
            doc,
            pages,
            max_xobject_depth: 8,
            cache: SessionCache::default(),
        }
    }

    /// Every byte value once, in order.
    fn all_bytes() -> Vec<u8> {
        (0..=u8::MAX).collect()
    }

    /// The encoding `lopdf` resolves for `font` inside an otherwise empty
    /// document, and the [`ByteTable`] built from it.
    fn with_encoding<F>(font: &Dictionary, check: F)
    where
        F: FnOnce(&Encoding<'_>, &ByteTable),
    {
        let doc = Document::with_version("1.5");
        let encoding = font.get_font_encoding(&doc).unwrap();
        let table = ByteTable::build(&encoding);
        check(&encoding, &table);
    }

    #[test]
    fn identity_is_stable() {
        let identity = LopdfBackend::default().identity();
        assert_eq!(identity.name, "lopdf");
        assert_eq!(identity.version, LOPDF_VERSION);
        let mut config = BTreeMap::new();
        config.insert("max_xobject_depth".to_string(), "8".to_string());
        config.insert("ligatures".to_string(), "expand".to_string());
        assert_eq!(identity.config_digest, config_digest(&config));
        // The digest before ligature expansion must not be reused.
        config.remove("ligatures");
        assert_ne!(identity.config_digest, config_digest(&config));
    }

    #[test]
    fn lopdf_version_matches_cargo_lock() {
        let lock = include_str!("../../Cargo.lock");
        let mut locked: Option<&str> = None;
        for block in lock.split("[[package]]") {
            let mut name: Option<&str> = None;
            let mut version: Option<&str> = None;
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("name = ") {
                    name = Some(value.trim().trim_matches('"'));
                } else if let Some(value) = line.strip_prefix("version = ") {
                    version = Some(value.trim().trim_matches('"'));
                }
            }
            if name == Some("lopdf") {
                locked = version;
            }
        }
        assert_eq!(
            locked,
            Some(LOPDF_VERSION),
            "Cargo.lock pins another lopdf; update LOPDF_VERSION (the identity key)"
        );
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
    fn type3_widths_go_through_font_matrix() {
        // Code 65 ("A") is glyph /a, 500 units wide in a glyph space where
        // one unit is 0.01 text-space units: 500 * 0.01 * 10 pt = 50 pt.
        let bytes = build_pdf_with_font(vec![text_ops(10, 100, 500, "A")], None, |doc| {
            let glyph_id = doc.add_object(Stream::new(dictionary! {}, Vec::new()));
            dictionary! {
                "Type" => "Font",
                "Subtype" => "Type3",
                "FontBBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
                "FontMatrix" => vec![
                    Object::Real(0.01),
                    0.into(),
                    0.into(),
                    Object::Real(0.01),
                    0.into(),
                    0.into(),
                ],
                "CharProcs" => dictionary! { "a" => glyph_id },
                "Encoding" => dictionary! {
                    "Type" => "Encoding",
                    "Differences" => vec![65.into(), "a".into()],
                },
                "FirstChar" => 65_i64,
                "LastChar" => 65_i64,
                "Widths" => vec![500.into()],
                "Resources" => dictionary! {},
            }
        });
        let mut session = LopdfBackend::default().open(&bytes, None).unwrap();
        let page = session.page_text(1).unwrap();
        assert_eq!(page.spans.len(), 1);
        let glyph_box = page.spans[0].bbox.unwrap();
        assert!(close(glyph_box.x0, 100.0), "x0 {}", glyph_box.x0);
        let width = glyph_box.x1 - glyph_box.x0;
        assert!((width - 50.0).abs() < 0.01, "width {width}");
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
        assert!(matches!(
            session.page_text(0),
            Err(BackendError::PageRange { .. })
        ));
    }

    #[test]
    fn malformed_bytes_are_rejected() {
        let backend = LopdfBackend::default();
        let Err(err) = backend.open(b"not a pdf at all", None) else {
            panic!("expected Malformed for non-PDF bytes");
        };
        assert!(matches!(err, BackendError::Malformed(_)), "{err}");
    }

    #[test]
    fn byte_table_matches_lopdf_for_standard_encoding() {
        let font = dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        };
        with_encoding(&font, |encoding, table| {
            let bytes = all_bytes();
            let expected = Document::decode_text(encoding, &bytes).ok();
            assert_eq!(table.decode(&bytes), expected);
            assert_eq!(
                table.decode(b"Hello, world!").as_deref(),
                Some("Hello, world!")
            );
            // Byte 1 has no glyph in StandardEncoding: silently dropped.
            assert_eq!(table.decode(&[1]).as_deref(), Some(""));
        });
    }

    #[test]
    fn byte_table_matches_lopdf_for_win_ansi_encoding() {
        let font = dictionary! {
            "Type" => "Font",
            "Subtype" => "TrueType",
            "BaseFont" => "Arial",
            "Encoding" => "WinAnsiEncoding",
        };
        with_encoding(&font, |encoding, table| {
            let bytes = all_bytes();
            let expected = Document::decode_text(encoding, &bytes).ok();
            assert!(expected.is_some());
            assert_eq!(table.decode(&bytes), expected);
            assert_eq!(table.decode(&[0xE9]).as_deref(), Some("\u{E9}"));
        });
    }

    #[test]
    fn byte_table_matches_lopdf_for_differences() {
        let font = dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Times-Roman",
            "Encoding" => dictionary! {
                "Type" => "Encoding",
                "BaseEncoding" => "WinAnsiEncoding",
                "Differences" => vec![65.into(), "eacute".into(), "germandbls".into()],
            },
        };
        with_encoding(&font, |encoding, table| {
            let bytes = all_bytes();
            let expected = Document::decode_text(encoding, &bytes).ok();
            assert!(expected.is_some());
            assert_eq!(table.decode(&bytes), expected);
            assert_eq!(table.decode(b"AB").as_deref(), Some("\u{E9}\u{DF}"));
            // Codes outside the differences fall through to the base.
            assert_eq!(table.decode(b"C").as_deref(), Some("C"));
        });
    }

    #[test]
    fn byte_table_rejects_what_lopdf_rejects() {
        // `lopdf` has no table for this predefined CMap on a simple font:
        // every string fails. The table path is not used for it (the name is
        // kept and re-decoded), but the table must agree all the same.
        let font = dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Foo",
            "Encoding" => "90ms-RKSJ-H",
        };
        with_encoding(&font, |encoding, table| {
            assert!(Document::decode_text(encoding, b"x").is_err());
            assert_eq!(table.decode(b"x"), None);
        });
        let named = own_encoding(Encoding::SimpleEncoding(b"90ms-RKSJ-H"));
        assert!(matches!(named, Decode::Named(_)));
    }

    #[test]
    fn differences_font_decodes_through_table_and_flags_unmapped_bytes() {
        let ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![10.into(), 10.into()]),
            Operation::new(
                "Tj",
                vec![Object::String(vec![65, 1], StringFormat::Hexadecimal)],
            ),
            Operation::new("ET", vec![]),
        ];
        let bytes = build_pdf_with_font(vec![ops], None, |_| {
            dictionary! {
                "Type" => "Font",
                "Subtype" => "Type1",
                "BaseFont" => "Times-Roman",
                "Encoding" => dictionary! {
                    "Type" => "Encoding",
                    "Differences" => vec![65.into(), "eacute".into()],
                },
            }
        });
        let mut session = open_session(&bytes);
        let page = session.page_text(1).unwrap();
        assert_eq!(page.spans.len(), 1);
        // Byte 65 is /eacute; byte 1 has no glyph, so it is dropped by the
        // encoding and restored as U+FFFD at the end, with a warning.
        assert_eq!(page.spans[0].text, "\u{E9}\u{FFFD}");
        assert_eq!(
            page.warnings,
            vec!["font F1: 1 unmapped byte(s); U+FFFD used".to_string()]
        );
        let font = session.cache.fonts.values().next().unwrap();
        assert!(matches!(font.decode, Decode::Table(_)));
        assert!(font.one_to_one);
    }

    /// Emit each of `texts` as one span on a fresh page and finish it.
    fn emit_all(texts: &[&str]) -> PageText {
        let doc = Document::with_version("1.5");
        let mut cache = SessionCache::default();
        let mut interpreter = Interpreter {
            doc: &doc,
            cache: &mut cache,
            page: PageText::new(1, 612.0, 792.0, 0),
            state: GState::default(),
            stack: Vec::new(),
            tm: Matrix::IDENTITY,
            tlm: Matrix::IDENTITY,
            seq: 0,
            max_depth: 8,
            ligatures: 0,
        };
        for &text in texts {
            interpreter.emit(text.to_string(), 1.0, None);
        }
        interpreter.finish()
    }

    fn span_texts(page: &PageText) -> Vec<&str> {
        page.spans.iter().map(|span| span.text.as_str()).collect()
    }

    #[test]
    fn non_ascii_text_is_nfc_normalised() {
        // The ASCII fast path must leave text alone and everything else
        // must still go through NFC: U+0065 U+0301 composes to U+00E9.
        // Compatibility characters other than ligatures are kept as written.
        let page = emit_all(&["plain ascii", "e\u{301}", "x\u{B2} \u{BD} \u{1D465}"]);
        assert_eq!(
            span_texts(&page),
            vec!["plain ascii", "\u{E9}", "x\u{B2} \u{BD} \u{1D465}"]
        );
        assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    }

    #[test]
    fn ligatures_are_expanded_with_one_page_warning() {
        let page = emit_all(&[
            "\u{FB01}nd \u{FB02}ow",
            "e\u{FB00}ect, o\u{FB03}ce, ba\u{FB04}e, \u{FB05}\u{FB06}",
            "\u{FB01}\u{301}",
        ]);
        assert_eq!(
            span_texts(&page),
            vec!["find flow", "effect, office, baffle, stst", "f\u{ED}",]
        );
        assert_eq!(page.warnings, vec!["ligatures expanded: 8".to_string()]);
    }

    #[test]
    fn page_without_ligatures_has_no_ligature_warning() {
        let page = emit_all(&["caf\u{E9} fi fl", "plain"]);
        assert_eq!(span_texts(&page), vec!["caf\u{E9} fi fl", "plain"]);
        let mentions = page.warnings.iter().any(|w| w.starts_with("ligatures"));
        assert!(!mentions, "{:?}", page.warnings);
    }

    #[test]
    fn font_cache_hit_yields_identical_spans() {
        // Both pages reference the same indirect font object, so page 2 is
        // decoded with the cached font that page 1 loaded.
        let page_one = text_ops(12, 100, 600, "Hello, cache");
        let page_two = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 9.into()]),
            Operation::new("Td", vec![72.into(), 700.into()]),
            Operation::new("Tj", vec![Object::string_literal("Second page")]),
            Operation::new("Tf", vec!["F1".into(), 14.into()]),
            Operation::new("Td", vec![0.into(), (-20).into()]),
            Operation::new("Tj", vec![Object::string_literal("More text")]),
            Operation::new("ET", vec![]),
        ];
        let bytes = build_pdf(vec![page_one, page_two], None);

        let mut cached = open_session(&bytes);
        assert!(cached.cache.fonts.is_empty());
        let first = cached.page_text(1).unwrap();
        assert_eq!(cached.cache.fonts.len(), 1, "one font object resolved");
        let second_cached = cached.page_text(2).unwrap();
        assert_eq!(cached.cache.fonts.len(), 1, "page 2 reused the cached font");

        // A fresh session extracting page 2 first cannot hit the cache.
        let mut fresh = open_session(&bytes);
        let second_fresh = fresh.page_text(2).unwrap();
        assert_eq!(second_cached, second_fresh);
        assert_eq!(second_cached.spans.len(), 2);
        assert_eq!(second_cached.spans[0].text, "Second page");
        assert_eq!(second_cached.spans[1].text, "More text");
        assert_eq!(second_cached.spans[0].font.as_deref(), Some("Helvetica"));
        assert!(
            second_cached.warnings.is_empty(),
            "{:?}",
            second_cached.warnings
        );

        // Re-extracting page 1 from the warm session is also identical.
        assert_eq!(cached.page_text(1).unwrap(), first);
    }

    #[test]
    fn shared_form_xobject_is_decoded_once_and_yields_identical_spans() {
        let ops = vec![
            Operation::new("q", vec![]),
            cm_translate(200, 300),
            Operation::new("Do", vec!["X1".into()]),
            Operation::new("Q", vec![]),
            Operation::new("q", vec![]),
            cm_translate(20, 30),
            Operation::new("Do", vec!["X1".into()]),
            Operation::new("Q", vec![]),
        ];
        let form = text_ops(10, 50, 50, "Form");
        let bytes = build_pdf(vec![ops.clone(), ops], Some(form));

        let mut cached = open_session(&bytes);
        let first = cached.page_text(1).unwrap();
        assert_eq!(cached.cache.forms.len(), 1, "the form stream is cached");
        assert_eq!(cached.cache.fonts.len(), 1, "page and form share /F1");
        let second_cached = cached.page_text(2).unwrap();
        assert_eq!(cached.cache.forms.len(), 1);

        let mut fresh = open_session(&bytes);
        let second_fresh = fresh.page_text(2).unwrap();
        assert_eq!(second_cached.spans, second_fresh.spans);
        assert_eq!(second_cached.warnings, second_fresh.warnings);
        assert_eq!(first.spans, second_cached.spans);

        // Two invocations on one page: both placed through their own `cm`.
        assert_eq!(first.spans.len(), 2);
        assert_eq!(first.spans[0].text, "Form");
        assert_eq!(first.spans[1].text, "Form");
        let first_box = first.spans[0].bbox.unwrap();
        let second_box = first.spans[1].bbox.unwrap();
        assert!(close(first_box.x0, 250.0), "x0 {}", first_box.x0);
        assert!(close(first_box.y0, 348.0), "y0 {}", first_box.y0);
        assert!(close(second_box.x0, 70.0), "x0 {}", second_box.x0);
        assert!(close(second_box.y0, 78.0), "y0 {}", second_box.y0);
        assert!(first.warnings.is_empty(), "{:?}", first.warnings);
    }
}

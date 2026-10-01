//! Pure-Rust backend built on `lopdf` 0.45. It interprets each page's
//! content stream (text state, graphics state, Form `XObject`s) and yields
//! one positioned [`Span`] per shown string. Nothing is ordered or repaired.
//!
//! Per-document work is cached inside the session: a font dictionary is
//! resolved (encoding, widths, flags) once per `ObjectId` and shared by
//! every page and Form `XObject` that references it, and a Form `XObject`'s
//! content stream is reused through a byte- and entry-bounded FIFO cache.
//! Decoding and execution have separate per-page budgets; exhaustion fails
//! the page explicitly instead of publishing silently truncated text. The caches hold only owned data, so they never borrow the
//! [`Document`] they were built from.
//!
//! Content streams are not parsed with `Content::decode`, which allocates
//! an `Operation` (and every operand) for each of the many path and colour
//! operators a vector figure is made of. A streaming lexer reads the same
//! grammar as `lopdf` (its quirks included: it stops quietly at the first
//! token it cannot read and rejects the stream only where `lopdf` does,
//! except that it accepts an inline image whose `EI` ends the stream) and
//! materialises operands only for the operators the interpreter acts on
//! (see [`OpKind`]); everything else is tokenised and dropped without
//! allocating. A Form runs on a graphics-state stack of its own and its
//! graphics state is restored afterwards, so only the text it shows, the
//! graphics it paints and the text matrix outlive it.
//!
//! Figures are boxes, never bytes. The lexer folds each path's construction
//! operators (`m l c v y re`) into one box in the stream's own coordinates
//! and keeps it only when the path is painted (`S s f F f* B B* b b*`; `n`
//! discards it, clipping is ignored), as one fill or stroke operation; the
//! interpreter maps the box's corners through the CTM (exact for axis-aligned
//! and quarter-turn CTMs, a covering box under skew). An Image `XObject`
//! shown with `Do` is the unit square under the CTM. Each page then gets
//! `rule` figures (thin horizontal or vertical painted boxes), `vector`
//! figures (the other painted boxes merged where they lie within
//! [`CLUSTER_GAP`] of each other) and `raster` figures, in that order.
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
//!
//! A simple font's `/Encoding` dictionary is resolved here, not by `lopdf`
//! (which replaces the whole encoding with `StandardEncoding` when one
//! `/Differences` name is unknown to it), and so is a font with neither
//! `/Encoding` nor `/ToUnicode`, whose built-in encoding is the one inside
//! its font program: the Computer Modern and AMS fonts `TeX` embeds that way
//! have their encodings tabulated (`CMSY` code 50 is `∈`, not `2`), other
//! embedded Type1 programs are read for their encoding array. Glyph names
//! resolve through [`GLYPH_NAMES`], `uniXXXX`/`uXXXX`, then `lopdf`'s own
//! glyph list. The policy is in the identity as `encodings=1`.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;

use lopdf::{
    Dictionary, Document, Encoding, Error as LopdfError, LoadOptions, Object, ObjectId, ParseError,
    Stream, StringFormat,
};
use unicode_normalization::UnicodeNormalization;

use crate::backend::{BackendError, DocumentSession, EncryptionProblem, Extractor};
use crate::schema::{BBox, BackendIdentity, Figure, Link, PageText, Span, config_digest};

/// The `lopdf` release this backend is built against. It is part of the
/// [`BackendIdentity`], so a dependency bump must change it (a unit test
/// checks it against `Cargo.lock`).
const LOPDF_VERSION: &str = "0.45.0";
/// Glyph width (in 1/1000 em) assumed when a font declares nothing usable.
const DEFAULT_WIDTH: f32 = 500.0;
/// Most link annotations read from one page; a hostile file gets no more.
const MAX_LINKS: usize = 4096;

/// The `/Annots` of `page` that are `/Link` annotations with a `/URI`
/// action: their rectangle (normalised) and URI. Missing or malformed
/// entries are skipped; nothing here is fatal for the page.
fn link_annotations(doc: &Document, page: &Dictionary) -> Vec<Link> {
    let mut links = Vec::new();
    let Some(annots) = page
        .get(b"Annots")
        .ok()
        .and_then(|a| resolve_object(doc, a))
        .and_then(|a| a.as_array().ok().cloned())
    else {
        return links;
    };
    for annot in annots.iter().take(MAX_LINKS) {
        let Some(dict) = resolve_object(doc, annot).and_then(|a| a.as_dict().ok().cloned()) else {
            continue;
        };
        let is_link = dict
            .get(b"Subtype")
            .ok()
            .and_then(|s| s.as_name().ok())
            .is_some_and(|name| name == b"Link");
        if !is_link {
            continue;
        }
        let Some(action) = dict
            .get(b"A")
            .ok()
            .and_then(|a| resolve_object(doc, a))
            .and_then(|a| a.as_dict().ok().cloned())
        else {
            continue;
        };
        let Some(uri) = action
            .get(b"URI")
            .ok()
            .and_then(|u| resolve_object(doc, u))
            .and_then(|u| {
                u.as_str()
                    .ok()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
            })
        else {
            continue;
        };
        let bbox = dict
            .get(b"Rect")
            .ok()
            .and_then(|r| resolve_object(doc, r))
            .and_then(|r| r.as_array().ok().cloned())
            .and_then(|r| {
                let v: Vec<f32> = r.iter().filter_map(|o| o.as_float().ok()).collect();
                (v.len() == 4).then(|| BBox {
                    x0: v[0].min(v[2]),
                    y0: v[1].min(v[3]),
                    x1: v[0].max(v[2]),
                    y1: v[1].max(v[3]),
                })
            });
        links.push(Link { bbox, uri });
    }
    links
}

/// `object` itself, or the object it references (one level; a reference to
/// a reference is not followed).
fn resolve_object<'a>(doc: &'a Document, object: &'a Object) -> Option<&'a Object> {
    match object {
        Object::Reference(id) => doc.get_object(*id).ok(),
        other => Some(other),
    }
}

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

/// Revision of the content-stream extraction policy, part of the backend
/// identity so ledger runs from different policies are never confused:
/// 1 = `Content::decode`; 2 = the streaming lexer with an isolated graphics
/// stack per Form; 3 = painted paths and Image `XObject`s become figures;
/// 4 = image placements are bounded per page; 5 = bounded Form decoding,
/// caching and execution, with explicit page errors on resource exhaustion.
const CONTENT_POLICY: &str = "5";

/// A painted box thinner than this (points) and at least [`RULE_LENGTH`]
/// long is a `rule` figure.
const RULE_THICKNESS: f32 = 2.0;
/// Shortest `rule` figure, in points.
const RULE_LENGTH: f32 = 30.0;
/// Painted boxes closer than this (points) belong to one `vector` figure.
const CLUSTER_GAP: f32 = 6.0;
/// A `vector` cluster that fits in a square this wide (points) is dropped.
const MIN_VECTOR_SIDE: f32 = 8.0;
/// Most painted boxes or image placements retained on one page. Beyond this,
/// painted boxes become one covering `vector` figure and images are ignored.
const MAX_CLUSTER_BOXES: usize = 2000;
/// Retained program allocation charge (not a process RSS limit).
const MAX_FORM_CACHE_BYTES: usize = 64 * 1024 * 1024;
/// Also bound map buckets, allocator overhead and the eviction queue.
const MAX_FORM_CACHE_ENTRIES: usize = 4096;
const MIN_FORM_CHARGE: usize = 256;
/// Limit both encoded and decoded bytes before lexing a Form.
const MAX_FORM_DECODE_BYTES: usize = 8 * 1024 * 1024;
/// Per-page limits also cover cache misses after eviction and direct Forms.
const MAX_PAGE_FORM_DECODE_BYTES: usize = 64 * 1024 * 1024;
const MAX_PAGE_FORM_WORK_BYTES: usize = 256 * 1024 * 1024;
const MAX_PAGE_FORM_CALLS: usize = 131_072;
const MAX_FORM_FILTERS: usize = 8;

/// Revision of the simple-font encoding policy, part of the backend identity:
/// 1 = the TeX built-in encodings, embedded Type1 encodings and a
/// `/Differences` parse that tolerates unknown glyph names.
const ENCODING_POLICY: &str = "1";

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
    /// and the ligature, content and encoding policies.
    fn identity(&self) -> BackendIdentity {
        let mut config = BTreeMap::new();
        config.insert(
            "max_xobject_depth".to_string(),
            self.max_xobject_depth.to_string(),
        );
        config.insert("ligatures".to_string(), LIGATURE_POLICY.to_string());
        config.insert("content".to_string(), CONTENT_POLICY.to_string());
        config.insert("encodings".to_string(), ENCODING_POLICY.to_string());
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
    /// FIFO eviction retains normal reuse without an unbounded miss cache.
    forms: HashMap<ObjectId, (Rc<TextProgram>, usize)>,
    form_order: VecDeque<ObjectId>,
    form_bytes: usize,
    #[cfg(test)]
    last_form_work: Option<FormWork>,
}

impl SessionCache {
    fn insert_form(&mut self, id: ObjectId, program: &Rc<TextProgram>) -> bool {
        let bytes = program.estimated_bytes().max(MIN_FORM_CHARGE);
        if bytes > MAX_FORM_CACHE_BYTES {
            return false;
        }
        // An existing immutable PDF object is already represented; do not
        // double-charge it or add duplicate eviction records.
        if self.forms.contains_key(&id) {
            return true;
        }
        while self.forms.len() >= MAX_FORM_CACHE_ENTRIES
            || self.form_bytes.saturating_add(bytes) > MAX_FORM_CACHE_BYTES
        {
            let Some(oldest) = self.form_order.pop_front() else {
                return false;
            };
            if let Some((_, charge)) = self.forms.remove(&oldest) {
                self.form_bytes -= charge;
            }
        }
        self.form_bytes += bytes;
        self.forms.insert(id, (Rc::clone(program), bytes));
        self.form_order.push_back(id);
        true
    }
}

/// Budgets reset for each page, so page order cannot exhaust a session-wide
/// work allowance. These bound Form work, not document parsing or fonts.
#[derive(Clone, Copy)]
struct FormWork {
    decode: usize,
    execute: usize,
    calls: usize,
}

impl Default for FormWork {
    fn default() -> Self {
        Self {
            decode: MAX_PAGE_FORM_DECODE_BYTES,
            execute: MAX_PAGE_FORM_WORK_BYTES,
            calls: MAX_PAGE_FORM_CALLS,
        }
    }
}

fn charge(remaining: &mut usize, bytes: usize) -> bool {
    if let Some(next) = remaining.checked_sub(bytes) {
        *remaining = next;
        true
    } else {
        false
    }
}

struct FormDecodePolicy {
    layers: usize,
    uses_predictor: bool,
}

/// Check attacker-controlled filter metadata before entering lopdf's decoder.
/// Predictor rows and their auxiliary color accumulators have separate bounds.
fn form_decode_policy(stream: &Stream) -> Option<FormDecodePolicy> {
    let layers = match stream.dict.get(b"Filter") {
        Ok(Object::Array(filters)) => filters.len(),
        Ok(_) => 1,
        Err(_) => 0,
    };
    if layers > MAX_FORM_FILTERS {
        return None;
    }
    // Match lopdf's decoder: only Flate/LZW use the dictionary-form
    // parameters, and only TIFF 2 / PNG 10..15 apply prediction.
    let predictor_filter = stream.filters().is_ok_and(|filters| {
        filters
            .iter()
            .any(|filter| matches!(*filter, b"FlateDecode" | b"LZWDecode"))
    });
    let mut uses_predictor = false;
    if predictor_filter
        && let Ok(params) = stream.dict.get(b"DecodeParms").and_then(Object::as_dict)
    {
        let predictor = params
            .get(b"Predictor")
            .and_then(Object::as_i64)
            .unwrap_or(1);
        uses_predictor = predictor == 2 || (10..=15).contains(&predictor);
        if !uses_predictor {
            return Some(FormDecodePolicy {
                layers: layers.max(1),
                uses_predictor,
            });
        }
        let dimension = |key: &[u8], default| {
            usize::try_from(
                params
                    .get(key)
                    .and_then(Object::as_i64)
                    .unwrap_or(default)
                    .max(1),
            )
            .ok()
        };
        let colors = dimension(b"Colors", 1)?;
        let component_bits = dimension(b"BitsPerComponent", 8)?;
        let bits = dimension(b"Columns", 1)?
            .checked_mul(colors)?
            .checked_mul(component_bits)?;
        if bits > MAX_FORM_DECODE_BYTES.checked_mul(8)? {
            return None;
        }
        // Packed rows do not bound the unpacked per-color accumulator in
        // lopdf's reverse_tiff_predictor2_subbyte (Vec<u16>). Independently
        // cap that auxiliary allocation before any decompression takes place.
        if predictor == 2
            && matches!(component_bits, 1 | 2 | 4)
            && colors.checked_mul(size_of::<u16>())? > MAX_FORM_DECODE_BYTES
        {
            return None;
        }
    }
    Some(FormDecodePolicy {
        layers: layers.max(1),
        uses_predictor,
    })
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
    /// `(first, last, width)` runs from the `/W` array: sorted by `first`
    /// when `disjoint`, otherwise in `/W` order.
    ranges: Vec<(u32, u32, f32)>,
    /// No two runs overlap, so at most one contains a CID and a binary
    /// search finds it. Otherwise the first run in `/W` order that contains
    /// the CID wins, found by a linear scan.
    disjoint: bool,
    default_width: f32,
}

impl CompositeWidths {
    /// Index the `/W` runs for lookup. Empty runs (`first > last`) contain
    /// no CID and are dropped from the sorted index.
    fn new(ranges: Vec<(u32, u32, f32)>, default_width: f32) -> Self {
        let mut sorted: Vec<(u32, u32, f32)> = ranges
            .iter()
            .copied()
            .filter(|&(first, last, _)| first <= last)
            .collect();
        sorted.sort_by_key(|&(first, _, _)| first);
        let disjoint = sorted.windows(2).all(|pair| match pair {
            [left, right] => left.1 < right.0,
            _ => true,
        });
        if disjoint {
            Self {
                ranges: sorted,
                disjoint,
                default_width,
            }
        } else {
            Self {
                ranges,
                disjoint,
                default_width,
            }
        }
    }

    /// Advance of `cid` in text space (1.0 = the font size).
    fn width(&self, cid: u32) -> f32 {
        if self.disjoint {
            let after = self.ranges.partition_point(|&(first, _, _)| first <= cid);
            if let Some(&(_, last, glyph_width)) = after
                .checked_sub(1)
                .and_then(|index| self.ranges.get(index))
                && cid <= last
            {
                return glyph_width * THOUSANDTH;
            }
        } else {
            for &(first, last, glyph_width) in &self.ranges {
                if (first..=last).contains(&cid) {
                    return glyph_width * THOUSANDTH;
                }
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

/// The four named base encodings a simple font's `/Encoding` dictionary may
/// give as `/BaseEncoding`; `lopdf` has a table for each.
const BASE_ENCODINGS: [&[u8]; 4] = [
    b"StandardEncoding",
    b"MacRomanEncoding",
    b"MacExpertEncoding",
    b"WinAnsiEncoding",
];

/// Decompression bound for an embedded Type1 font program read for its
/// built-in encoding.
const MAX_FONT_PROGRAM: usize = 8 * 1024 * 1024;

/// The built-in encodings of the Computer Modern and AMS symbol fonts, which
/// `pdfTeX` embeds without `/Encoding` (and usually without `/ToUnicode`): a
/// viewer must use the encoding inside the font program, which `lopdf`
/// replaces with `StandardEncoding`, so `∈` (`CMSY` code 50) came out as `2`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TexEncoding {
    /// `OT1` text fonts (`CMR`, `CMBX`, `CMSS`, ...).
    Ot1,
    /// `OT1` text italics, whose code 36 is `sterling` rather than `dollar`.
    Ot1Italic,
    /// `OML` math italic (`CMMI`, `CMMIB`).
    Oml,
    /// `OMS` math symbols (`CMSY`, `CMBSY`).
    Oms,
    /// `OMX` math extension (`CMEX`).
    Omx,
    /// Typewriter text (`CMTT`), `OT1` with ASCII in place of some glyphs.
    Typewriter,
    /// AMS symbols A.
    Msam,
    /// AMS symbols B.
    Msbm,
}

/// Font families by name without the size digits, for [`tex_encoding`].
const TEX_FAMILIES: &[(&str, TexEncoding)] = &[
    ("CMB", TexEncoding::Ot1),
    ("CMBSY", TexEncoding::Oms),
    ("CMBX", TexEncoding::Ot1),
    ("CMBXSL", TexEncoding::Ot1),
    ("CMBXTI", TexEncoding::Ot1Italic),
    ("CMCSC", TexEncoding::Ot1),
    ("CMDUNH", TexEncoding::Ot1),
    ("CMEX", TexEncoding::Omx),
    ("CMFF", TexEncoding::Ot1),
    ("CMFIB", TexEncoding::Ot1),
    ("CMITT", TexEncoding::Typewriter),
    ("CMMI", TexEncoding::Oml),
    ("CMMIB", TexEncoding::Oml),
    ("CMR", TexEncoding::Ot1),
    ("CMSL", TexEncoding::Ot1),
    ("CMSLTT", TexEncoding::Typewriter),
    ("CMSS", TexEncoding::Ot1),
    ("CMSSBX", TexEncoding::Ot1),
    ("CMSSDC", TexEncoding::Ot1),
    ("CMSSI", TexEncoding::Ot1),
    ("CMSSQ", TexEncoding::Ot1),
    ("CMSSQI", TexEncoding::Ot1),
    ("CMSY", TexEncoding::Oms),
    ("CMTI", TexEncoding::Ot1Italic),
    ("CMTT", TexEncoding::Typewriter),
    ("MSAM", TexEncoding::Msam),
    ("MSBM", TexEncoding::Msbm),
];

/// `name` without a subset tag (`ABCDEF+`).
fn strip_subset(name: &[u8]) -> &[u8] {
    match name.get(..7) {
        Some([tag @ .., b'+']) if tag.iter().all(u8::is_ascii_uppercase) => &name[7..],
        _ => name,
    }
}

/// The TeX built-in encoding of the font named `base_font`: a known family
/// name (any case) followed by the design size in digits, as `CMSY10`.
fn tex_encoding(base_font: &[u8]) -> Option<TexEncoding> {
    let name = strip_subset(base_font);
    let digits = name.iter().rev().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let family = &name[..name.len() - digits];
    TEX_FAMILIES
        .iter()
        .find(|(known, _)| known.as_bytes().eq_ignore_ascii_case(family))
        .map(|&(_, encoding)| encoding)
}

/// The code 0 to 127 a code above 127 duplicates in the Type1 versions of
/// the Computer Modern fonts (the remapping of the control range in the
/// AMS Type1 fonts, as their embedded programs list it); 160 is the space.
fn tex_high_alias(code: u8) -> Option<u8> {
    match code {
        128 => Some(32),
        161..=170 => Some(code - 161),
        173..=195 => Some(code - 163),
        196 => Some(127),
        _ => None,
    }
}

/// Per-byte text of a simple font; every entry holds zero chars (the code
/// has no glyph, or one that no table resolves) or exactly one, which keeps
/// the font `one_to_one` so dropped codes are counted and warned about.
fn unmapped_entries() -> Vec<Option<String>> {
    vec![Some(String::new()); 256]
}

fn set_entry(entries: &mut [Option<String>], code: u8, ch: Option<char>) {
    if let Some(slot) = entries.get_mut(usize::from(code)) {
        *slot = Some(ch.map(String::from).unwrap_or_default());
    }
}

/// The glyph named `name` in the table built into `lopdf` (the Adobe Glyph
/// List plus TeX names), which `lopdf` does not export: it is queried
/// through a one-entry `/Differences` encoding. An unknown name makes `lopdf`
/// fall back to `StandardEncoding`, which has no glyph at code 0, so the
/// empty result reads as "unknown".
fn lopdf_glyph(doc: &Document, name: &[u8]) -> Option<char> {
    let mut encoding = Dictionary::new();
    encoding.set("Type", Object::Name(b"Encoding".to_vec()));
    encoding.set(
        "Differences",
        Object::Array(vec![Object::Integer(0), Object::Name(name.to_vec())]),
    );
    let mut font = Dictionary::new();
    font.set("Type", Object::Name(b"Font".to_vec()));
    font.set("Encoding", Object::Dictionary(encoding));
    let resolved = font.get_font_encoding(doc).ok()?;
    let text = Document::decode_text(&resolved, &[0]).ok()?;
    let mut chars = text.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(ch)
}

/// `uniXXXX` (exactly four hex digits) or `uXXXX` to `uXXXXXX`, the Adobe
/// Glyph List forms that name a code point directly. Control characters and
/// surrogates are refused.
fn uni_glyph(name: &[u8]) -> Option<char> {
    let hex = if let Some(rest) = name.strip_prefix(b"uni") {
        if rest.len() != 4 {
            return None;
        }
        rest
    } else {
        let rest = name.strip_prefix(b"u")?;
        if !(4..=6).contains(&rest.len()) {
            return None;
        }
        rest
    };
    if !hex.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let text = std::str::from_utf8(hex).ok()?;
    let value = u32::from_str_radix(text, 16).ok()?;
    char::from_u32(value).filter(|ch| !ch.is_control())
}

/// An opaque glyph name that identifies a glyph by number only (`g37`,
/// `cid1024`): it says nothing about the character.
fn opaque_glyph(name: &[u8]) -> bool {
    [b"g".as_slice(), b"cid".as_slice()].iter().any(|prefix| {
        name.strip_prefix(*prefix)
            .is_some_and(|digits| !digits.is_empty() && digits.iter().all(u8::is_ascii_digit))
    })
}

/// The character a glyph name stands for: [`GLYPH_NAMES`] first, then the
/// `uniXXXX`/`uXXXX` forms, then `lopdf`'s own glyph list. A suffix after a
/// period (`a.sc`, `one.oldstyle`) is dropped first, as the Adobe Glyph List
/// specification says; `.notdef` and opaque names give `None`.
fn glyph_char(doc: &Document, name: &[u8]) -> Option<char> {
    let base = match name.iter().position(|&byte| byte == b'.') {
        Some(0) => return None,
        Some(period) => &name[..period],
        None => name,
    };
    if let Ok(index) = GLYPH_NAMES.binary_search_by(|(known, _)| known.as_bytes().cmp(base)) {
        return GLYPH_NAMES.get(index).map(|&(_, ch)| ch);
    }
    if let Some(ch) = uni_glyph(base) {
        return Some(ch);
    }
    if opaque_glyph(base) {
        return None;
    }
    lopdf_glyph(doc, base)
}

/// The per-byte text of one of `lopdf`'s named single-byte encodings
/// (`StandardEncoding` when `name` is `None`).
fn named_entries(doc: &Document, name: Option<&[u8]>) -> Option<Vec<Option<String>>> {
    let mut font = Dictionary::new();
    font.set("Type", Object::Name(b"Font".to_vec()));
    if let Some(name) = name {
        font.set("Encoding", Object::Name(name.to_vec()));
    }
    let encoding = font.get_font_encoding(doc).ok()?;
    Some(ByteTable::build(&encoding).entries)
}

/// Split `PostScript` source into tokens: runs of non-whitespace, with a
/// name's `/` always starting a new token (`50/element` is two tokens).
fn ps_tokens(source: &[u8]) -> Vec<&[u8]> {
    let mut tokens = Vec::new();
    let mut start: Option<usize> = None;
    for (index, &byte) in source.iter().enumerate() {
        if is_pdf_space(byte) || byte == b'/' {
            if let Some(first) = start.take() {
                tokens.push(&source[first..index]);
            }
            if byte == b'/' {
                start = Some(index);
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(first) = start {
        tokens.push(&source[first..]);
    }
    tokens
}

/// The built-in encoding in the clear-text part of a Type1 font program:
/// `(code, glyph name)` for every `dup <code> /<name> put` of its
/// `/Encoding` array. `None` when the program uses `StandardEncoding` or has
/// no encoding array.
fn type1_encoding(program: &[u8]) -> Option<Vec<(u8, Vec<u8>)>> {
    let clear_end = program
        .windows(5)
        .position(|window| window == b"eexec")
        .unwrap_or(program.len());
    let clear = &program[..clear_end];
    let start = clear.windows(9).position(|window| window == b"/Encoding")?;
    let tokens = ps_tokens(&clear[start + 9..]);
    if tokens.first() == Some(&b"StandardEncoding".as_slice()) {
        return None;
    }
    let mut found = Vec::new();
    for (index, &token) in tokens.iter().enumerate() {
        if token == b"def" {
            break;
        }
        if token != b"dup" {
            continue;
        }
        let (Some(code), Some(name), Some(put)) = (
            tokens.get(index + 1),
            tokens.get(index + 2),
            tokens.get(index + 3),
        ) else {
            continue;
        };
        if *put != b"put" {
            continue;
        }
        let Some(glyph) = name.strip_prefix(b"/") else {
            continue;
        };
        let parsed = std::str::from_utf8(code)
            .ok()
            .and_then(|text| text.parse::<u8>().ok());
        if let Some(code) = parsed {
            found.push((code, glyph.to_vec()));
        }
    }
    if found.is_empty() { None } else { Some(found) }
}

/// The resolved built-in encoding of the embedded Type1 font program
/// (`/FontFile`), if the font has one with its own encoding array.
fn embedded_encoding(doc: &Document, dict: &Dictionary) -> Option<Vec<(u8, Option<char>)>> {
    let descriptor = dict
        .get_deref(b"FontDescriptor", doc)
        .and_then(Object::as_dict)
        .ok()?;
    let stream = descriptor
        .get_deref(b"FontFile", doc)
        .and_then(Object::as_stream)
        .ok()?;
    let program = stream.get_plain_content_with_limit(MAX_FONT_PROGRAM).ok()?;
    let listed = type1_encoding(&program)?;
    Some(
        listed
            .into_iter()
            .map(|(code, name)| (code, glyph_char(doc, &name)))
            .collect(),
    )
}

/// The per-byte text of a TeX font's built-in encoding. The `OT1`, `OML`
/// and `OMS` tables list every code from 0 to 127 and are used alone (with
/// the codes above 127 that duplicate them). For the partly known tables,
/// codes the table lacks come from the embedded font program, else
/// (typewriter text only) from `StandardEncoding`.
fn tex_entries(
    doc: &Document,
    dict: &Dictionary,
    encoding: TexEncoding,
) -> Option<Vec<Option<String>>> {
    let (partial, symbolic): (&[(u8, &str)], bool) = match encoding {
        TexEncoding::Ot1 | TexEncoding::Ot1Italic => {
            return Some(full_tex_entries(doc, &OT1_NAMES, encoding));
        }
        TexEncoding::Oml => return Some(full_tex_entries(doc, &OML_NAMES, encoding)),
        TexEncoding::Oms => return Some(full_tex_entries(doc, &OMS_NAMES, encoding)),
        TexEncoding::Omx => (OMX_NAMES, true),
        TexEncoding::Typewriter => (TYPEWRITER_NAMES, false),
        TexEncoding::Msam => (MSAM_NAMES, true),
        TexEncoding::Msbm => (MSBM_NAMES, true),
    };
    let mut entries = if symbolic {
        unmapped_entries()
    } else {
        named_entries(doc, None)?
    };
    if let Some(embedded) = embedded_encoding(doc, dict) {
        for (code, ch) in embedded {
            set_entry(&mut entries, code, ch);
        }
    }
    for &(code, name) in partial {
        set_entry(&mut entries, code, glyph_char(doc, name.as_bytes()));
    }
    Some(entries)
}

/// The per-byte text of a TeX font whose table `names` covers codes 0 to
/// 127 (an empty name has no glyph).
fn full_tex_entries(
    doc: &Document,
    names: &[&str; 128],
    encoding: TexEncoding,
) -> Vec<Option<String>> {
    let mut entries = unmapped_entries();
    for code in 0..=u8::MAX {
        let low = if code < 128 {
            Some(code)
        } else {
            tex_high_alias(code)
        };
        let Some(name) = low.and_then(|low| names.get(usize::from(low))) else {
            continue;
        };
        if !name.is_empty() {
            set_entry(&mut entries, code, glyph_char(doc, name.as_bytes()));
        }
    }
    set_entry(&mut entries, 160, Some(' '));
    if encoding == TexEncoding::Ot1Italic {
        set_entry(&mut entries, 36, glyph_char(doc, b"sterling"));
    }
    entries
}

/// The per-byte text of a simple font's built-in encoding, when it is not
/// `StandardEncoding`: a TeX font's known table, else the encoding array of
/// the embedded Type1 program. An embedded array in which fewer than half
/// the names resolve is not trusted (its names are likely opaque, and the
/// codes likely ASCII), so the caller keeps `StandardEncoding`.
fn builtin_entries(doc: &Document, dict: &Dictionary) -> Option<Vec<Option<String>>> {
    let subtype = dict.get(b"Subtype").and_then(Object::as_name);
    if subtype.is_ok_and(|name| name == b"Type3") {
        return None;
    }
    let base_font = dict.get(b"BaseFont").and_then(Object::as_name).ok();
    if let Some(encoding) = base_font.and_then(tex_encoding) {
        return tex_entries(doc, dict, encoding);
    }
    let embedded = embedded_encoding(doc, dict)?;
    let resolved = embedded.iter().filter(|(_, ch)| ch.is_some()).count();
    if resolved * 2 < embedded.len() {
        return None;
    }
    let mut entries = unmapped_entries();
    for (code, ch) in embedded {
        set_entry(&mut entries, code, ch);
    }
    Some(entries)
}

/// Overlay an `/Encoding` dictionary's `/Differences` on `entries`. Unlike
/// `lopdf`, which drops the whole encoding for `StandardEncoding` when one
/// name is unknown, only that code becomes unmapped; out-of-range codes and
/// items of the wrong type are skipped.
fn apply_differences(doc: &Document, encoding: &Dictionary, entries: &mut [Option<String>]) {
    let Ok(items) = encoding
        .get_deref(b"Differences", doc)
        .and_then(Object::as_array)
    else {
        return;
    };
    let mut code: Option<u8> = None;
    for item in items {
        match item {
            Object::Integer(value) => code = u8::try_from(*value).ok(),
            Object::Name(name) => {
                if let Some(current) = code {
                    set_entry(entries, current, glyph_char(doc, name));
                    code = current.checked_add(1);
                }
            }
            _ => {}
        }
    }
}

/// The byte table of a simple font whose encoding this backend resolves
/// itself rather than through `lopdf`: an `/Encoding` dictionary (base, then
/// `/Differences`), or no `/Encoding` and no `/ToUnicode` with a non-standard
/// built-in encoding. `None` leaves the font to `lopdf` (a named
/// `/Encoding`, a `/ToUnicode` `CMap`, or `StandardEncoding`).
///
/// The base under `/Differences` is the named `/BaseEncoding`, else the
/// font's built-in encoding (a Type3 font has none), else
/// `StandardEncoding`.
fn own_table(doc: &Document, dict: &Dictionary) -> Option<ByteTable> {
    match dict.get_deref(b"Encoding", doc) {
        Ok(Object::Dictionary(encoding)) => {
            let base_name = encoding
                .get_deref(b"BaseEncoding", doc)
                .and_then(Object::as_name)
                .ok()
                .filter(|name| BASE_ENCODINGS.contains(name));
            let mut entries = match base_name {
                Some(name) => named_entries(doc, Some(name))?,
                None => match builtin_entries(doc, dict) {
                    Some(entries) => entries,
                    None => named_entries(doc, None)?,
                },
            };
            apply_differences(doc, encoding, &mut entries);
            Some(ByteTable { entries })
        }
        Ok(_) => None,
        Err(_) => {
            let to_unicode = dict
                .get_deref(b"ToUnicode", doc)
                .and_then(Object::as_stream);
            if to_unicode.is_ok() {
                return None;
            }
            builtin_entries(doc, dict).map(|entries| ByteTable { entries })
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
    if let Some(table) = own_table(doc, dict) {
        return (Decode::Table(table), true);
    }
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
    CompositeWidths::new(ranges, default_width)
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
enum OpKind {
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
    fn from_operator(operator: &[u8]) -> Option<Self> {
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
    fn is_path(self) -> bool {
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
struct TextOp {
    kind: OpKind,
    first: usize,
    end: usize,
}

/// The operators of one content stream that the interpreter acts on, in
/// stream order, with their operands exactly as `Content::decode` yields them,
/// and the painted paths among them.
#[derive(Default)]
struct TextProgram {
    ops: Vec<TextOp>,
    operands: Vec<Object>,
    /// `[x0, y0, x1, y1]` of each painted path, in the stream's coordinates.
    paths: Vec<[f32; 4]>,
}

impl TextProgram {
    /// Rough size of the program in memory: the vectors' elements plus the
    /// heap bytes of string, name and array operands.
    fn estimated_bytes(&self) -> usize {
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
    fn operands(&self, op: TextOp) -> &[Object] {
        if op.kind.is_path() {
            return &[];
        }
        self.operands.get(op.first..op.end).unwrap_or_default()
    }

    /// The box of a painted path.
    fn path_box(&self, op: TextOp) -> Option<[f32; 4]> {
        if op.kind.is_path() {
            self.paths.get(op.first).copied()
        } else {
            None
        }
    }
}

/// Why no object or operation could be read at some position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Halt {
    /// Nothing valid here (a `nom` error): `lopdf` backtracks, and at the
    /// top level it stops and keeps the operations read so far.
    Stop,
    /// `lopdf` rejects the whole content stream (a `nom` failure).
    Fatal,
}

/// The end of a lexed object (before any white space after it) and, in
/// build mode, the object.
type Lexed = Result<(usize, Option<Object>), Halt>;

/// White space `lopdf` skips between content-stream tokens.
fn is_content_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

/// PDF white space, skipped inside arrays, dictionaries and hex strings.
fn is_pdf_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b'\0' | 0x0C)
}

fn is_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn is_regular(byte: u8) -> bool {
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
fn skip_while(bytes: &[u8], pos: usize, keep: fn(u8) -> bool) -> usize {
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
fn skip_space(bytes: &[u8], mut pos: usize) -> usize {
    loop {
        let next = skip_while(bytes, pos, is_pdf_space);
        match comment_end(bytes, next) {
            Some(end) => pos = end,
            None => return next,
        }
    }
}

fn hex_value(digit: u8) -> u8 {
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
fn lex_object(bytes: &[u8], pos: usize, depth: usize, direct: bool, build: bool) -> Lexed {
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
/// quietly at the first token it cannot read, keeping what came before,
/// and fails only where `lopdf` rejects the whole stream (an inline image
/// without `ID` or `EI`, arrays or dictionaries nested too deep). The one
/// place it is more lenient is an inline image whose `EI` ends the stream
/// (see [`skip_inline_image`]).
fn lex_content(bytes: &[u8]) -> Result<TextProgram, LopdfError> {
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
            return Ok(program);
        }
        let operator = bytes.get(at..end).unwrap_or_default();
        if let Some(kind) = OpKind::from_operator(operator) {
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

/// The smallest box holding every one of `corners`.
fn box_of(corners: [(f32, f32); 4]) -> BBox {
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
    bbox
}

/// The smallest box holding both `a` and `b`.
fn enclose(a: BBox, b: BBox) -> BBox {
    BBox {
        x0: a.x0.min(b.x0),
        y0: a.y0.min(b.y0),
        x1: a.x1.max(b.x1),
        y1: a.y1.max(b.y1),
    }
}

/// Whether `a` and `b` overlap or lie within `gap` of each other.
fn near(a: BBox, b: BBox, gap: f32) -> bool {
    a.x0 <= b.x1 + gap && b.x0 <= a.x1 + gap && a.y0 <= b.y1 + gap && b.y0 <= a.y1 + gap
}

/// Merge `boxes` into clusters: two boxes share a cluster when they (or
/// the clusters grown so far around them) overlap or lie within
/// [`CLUSTER_GAP`]. Each new box absorbs every cluster near it, rescanning
/// after each merge, so no two clusters left are near each other (a fixed
/// point). Clusters come top to bottom, then left to right.
fn cluster(boxes: &[BBox]) -> Vec<BBox> {
    let mut clusters: Vec<BBox> = Vec::new();
    for &bbox in boxes {
        let mut grown = bbox;
        let mut at = 0;
        while at < clusters.len() {
            if near(clusters[at], grown, CLUSTER_GAP) {
                grown = enclose(grown, clusters.swap_remove(at));
                at = 0;
            } else {
                at += 1;
            }
        }
        clusters.push(grown);
    }
    clusters.sort_by(|a, b| b.y1.total_cmp(&a.y1).then(a.x0.total_cmp(&b.x0)));
    clusters
}

/// An Image `XObject` as placed on the page.
struct Raster {
    bbox: BBox,
    width_px: Option<u32>,
    height_px: Option<u32>,
}

/// Painted paths and images of one page, in page space, gathered while its
/// content runs.
#[derive(Default)]
struct Graphics {
    /// Thin painted boxes (see [`RULE_THICKNESS`]).
    rules: Vec<BBox>,
    /// The other painted boxes, at most [`MAX_CLUSTER_BOXES`].
    shapes: Vec<BBox>,
    /// Union of every box that is not a rule.
    extent: Option<BBox>,
    /// More than [`MAX_CLUSTER_BOXES`] boxes that are not rules were painted.
    overflow: bool,
    /// Image placements, at most [`MAX_CLUSTER_BOXES`].
    rasters: Vec<Raster>,
}

impl Graphics {
    fn add_path(&mut self, bbox: BBox) {
        let width = bbox.x1 - bbox.x0;
        let height = bbox.y1 - bbox.y0;
        let horizontal = height < RULE_THICKNESS && width >= RULE_LENGTH;
        let vertical = width < RULE_THICKNESS && height >= RULE_LENGTH;
        if horizontal || vertical {
            self.rules.push(bbox);
            return;
        }
        self.extent = Some(match self.extent {
            Some(so_far) => enclose(so_far, bbox),
            None => bbox,
        });
        if self.shapes.len() < MAX_CLUSTER_BOXES {
            self.shapes.push(bbox);
        } else {
            self.overflow = true;
        }
    }

    fn add_raster(&mut self, raster: Raster) {
        if self.rasters.len() < MAX_CLUSTER_BOXES {
            self.rasters.push(raster);
        }
    }

    /// `rule` figures, then `vector` clusters at least [`MIN_VECTOR_SIDE`]
    /// wide or high, then `raster` figures; indexed in that order.
    fn into_figures(self) -> Vec<Figure> {
        let mut figures: Vec<Figure> = Vec::new();
        for bbox in self.rules {
            push_figure(&mut figures, "rule", bbox, None, None);
        }
        let clusters: Vec<BBox> = if self.overflow {
            self.extent.into_iter().collect()
        } else {
            cluster(&self.shapes)
        };
        for bbox in clusters {
            if bbox.x1 - bbox.x0 >= MIN_VECTOR_SIDE || bbox.y1 - bbox.y0 >= MIN_VECTOR_SIDE {
                push_figure(&mut figures, "vector", bbox, None, None);
            }
        }
        for raster in self.rasters {
            let (width_px, height_px) = (raster.width_px, raster.height_px);
            push_figure(&mut figures, "raster", raster.bbox, width_px, height_px);
        }
        figures
    }
}

/// Append a figure of `kind` with the next index; no bytes are captured.
fn push_figure(
    figures: &mut Vec<Figure>,
    kind: &str,
    bbox: BBox,
    width_px: Option<u32>,
    height_px: Option<u32>,
) {
    let index = u32::try_from(figures.len()).unwrap_or(u32::MAX);
    figures.push(Figure {
        index,
        bbox: Some(bbox),
        kind: kind.to_string(),
        mime: None,
        width_px,
        height_px,
        sha256: None,
        file: None,
        caption: None,
    });
}

/// A non-negative integer entry of an image dictionary, when it is direct.
fn pixel_count(dict: &Dictionary, key: &[u8]) -> Option<u32> {
    let value = dict.get(key).ok()?.as_i64().ok()?;
    u32::try_from(value).ok()
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
    graphics: Graphics,
    form_work: FormWork,
    resource_error: Option<&'static str>,
}

impl<'a> Interpreter<'a> {
    fn warn(&mut self, message: String) {
        if !self.page.warnings.contains(&message) {
            self.page.warnings.push(message);
        }
    }

    /// The finished page, with its figures and the ligature count recorded
    /// as one warning.
    fn finish(mut self) -> PageText {
        let graphics = std::mem::take(&mut self.graphics);
        self.page.figures = graphics.into_figures();
        if self.ligatures > 0 {
            let count = self.ligatures;
            self.page
                .warnings
                .push(format!("ligatures expanded: {count}"));
        }
        self.page
    }

    fn run(&mut self, program: &TextProgram, contexts: &mut Vec<Context<'a>>, depth: u32) {
        for &op in &program.ops {
            if self.resource_error.is_some() {
                break;
            }
            let operands = program.operands(op);
            match op.kind {
                OpKind::Save => self.stack.push(self.state.clone()),
                OpKind::Restore => {
                    if let Some(state) = self.stack.pop() {
                        self.state = state;
                    }
                }
                OpKind::Concat => {
                    if let Some(matrix) = matrix_from_operands(operands) {
                        self.state.ctm = matrix.then(self.state.ctm);
                    }
                }
                OpKind::BeginText => {
                    self.tm = Matrix::IDENTITY;
                    self.tlm = Matrix::IDENTITY;
                }
                OpKind::Font => self.set_font(operands),
                OpKind::Move => {
                    if let Some(tx) = float_at(operands, 0)
                        && let Some(ty) = float_at(operands, 1)
                    {
                        self.text_move(tx, ty);
                    }
                }
                OpKind::MoveSetLeading => {
                    if let Some(tx) = float_at(operands, 0)
                        && let Some(ty) = float_at(operands, 1)
                    {
                        self.state.leading = -ty;
                        self.text_move(tx, ty);
                    }
                }
                OpKind::TextMatrix => {
                    if let Some(matrix) = matrix_from_operands(operands) {
                        self.tm = matrix;
                        self.tlm = matrix;
                    }
                }
                OpKind::NextLine => self.next_line(),
                OpKind::Leading => {
                    if let Some(value) = float_at(operands, 0) {
                        self.state.leading = value;
                    }
                }
                OpKind::CharSpacing => {
                    if let Some(value) = float_at(operands, 0) {
                        self.state.char_spacing = value;
                    }
                }
                OpKind::WordSpacing => {
                    if let Some(value) = float_at(operands, 0) {
                        self.state.word_spacing = value;
                    }
                }
                OpKind::HorizontalScale => {
                    if let Some(value) = float_at(operands, 0) {
                        self.state.hscale = value / 100.0;
                    }
                }
                OpKind::Rise => {
                    if let Some(value) = float_at(operands, 0) {
                        self.state.rise = value;
                    }
                }
                OpKind::Show => {
                    if let Some(bytes) = string_at(operands, 0) {
                        self.show(bytes, contexts);
                    }
                }
                OpKind::NextLineShow => {
                    self.next_line();
                    if let Some(bytes) = string_at(operands, 0) {
                        self.show(bytes, contexts);
                    }
                }
                OpKind::SpacingShow => {
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
                OpKind::ShowArray => self.show_array(operands, contexts),
                OpKind::Invoke => self.do_xobject(operands, contexts, depth),
                OpKind::FillPath | OpKind::StrokePath => {
                    if let Some(bounds) = program.path_box(op) {
                        self.paint(bounds);
                    }
                }
            }
        }
    }

    /// Record a painted path whose box in the current user space is
    /// `[x0, y0, x1, y1]`, by the box around its corners in page space.
    fn paint(&mut self, bounds: [f32; 4]) {
        let [x0, y0, x1, y1] = bounds;
        let ctm = self.state.ctm;
        let bbox = box_of([
            ctm.apply(x0, y0),
            ctm.apply(x1, y0),
            ctm.apply(x0, y1),
            ctm.apply(x1, y1),
        ]);
        self.graphics.add_path(bbox);
    }

    /// Record an Image `XObject`: the unit square under the CTM, with the
    /// pixel size its dictionary states directly. No bytes are read.
    fn place_image(&mut self, stream: &Stream) {
        let ctm = self.state.ctm;
        let bbox = box_of([
            ctm.apply(0.0, 0.0),
            ctm.apply(1.0, 0.0),
            ctm.apply(0.0, 1.0),
            ctm.apply(1.0, 1.0),
        ]);
        self.graphics.add_raster(Raster {
            bbox,
            width_px: pixel_count(&stream.dict, b"Width"),
            height_px: pixel_count(&stream.dict, b"Height"),
        });
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
        let bbox = box_of([
            full.apply(0.0, rise + DESCENT * size),
            full.apply(0.0, rise + ASCENT * size),
            full.apply(advance, rise + DESCENT * size),
            full.apply(advance, rise + ASCENT * size),
        ]);
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
        let subtype = stream
            .dict
            .get(b"Subtype")
            .and_then(Object::as_name)
            .unwrap_or_default();
        if subtype == b"Image" {
            self.place_image(stream);
            return;
        }
        if subtype != b"Form" {
            return;
        }
        if depth >= self.max_depth {
            let limit = self.max_depth;
            self.warn(format!(
                "XObject {label}: nesting deeper than {limit}; skipped"
            ));
            return;
        }
        if !charge(&mut self.form_work.calls, 1) {
            self.resource_error = Some("Form invocation budget exceeded");
            return;
        }
        let cached = stream_id.and_then(|id| self.cache.forms.get(&id).cloned());
        let (program, work) = if let Some(cached) = cached {
            cached
        } else {
            let Some(policy) = form_decode_policy(stream) else {
                self.resource_error = Some("Form filter/predictor limit exceeded");
                return;
            };
            // Reserve before decoding. For a single filter, refund unused
            // bytes afterwards unless prediction needs scratch space. Chained
            // filters can have large intermediate outputs, so keep their full
            // worst-case charge. Non-predictor DecodeParms do not prevent refunds.
            let layers = policy.layers;
            let limit = MAX_FORM_DECODE_BYTES.min(self.form_work.decode / layers);
            if limit == 0 || stream.content.len() > limit {
                self.resource_error = Some("Form decode byte budget exceeded");
                return;
            }
            self.form_work.decode -= limit * layers;
            let content_bytes = match stream.get_plain_content_with_limit(limit) {
                Ok(bytes) => bytes,
                Err(LopdfError::Decompress(lopdf::DecompressError::MemoryLimitExceeded {
                    ..
                })) => {
                    self.resource_error = Some("Form decoded stream limit exceeded");
                    return;
                }
                Err(_) => {
                    self.warn(format!("XObject {label}: undecodable content stream"));
                    return;
                }
            };
            if layers == 1 && !policy.uses_predictor {
                self.form_work.decode += limit - content_bytes.len().max(stream.content.len());
            }
            let Ok(program) = lex_content(&content_bytes) else {
                self.warn(format!("XObject {label}: undecodable content stream"));
                return;
            };
            let work = program.estimated_bytes().max(MIN_FORM_CHARGE);
            let program = Rc::new(program);
            if let Some(id) = stream_id {
                self.cache.insert_form(id, &program);
            }
            (program, work)
        };
        if !charge(&mut self.form_work.execute, work) {
            self.resource_error = Some("Form execution byte budget exceeded");
            return;
        }
        // Nothing in it shows text, paints or moves the text position, and
        // it cannot reach the caller's state, so running it would change
        // nothing.
        if program.ops.is_empty() {
            return;
        }
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

        // The Form runs on a stack of its own: an unbalanced `Q` inside it
        // cannot pop graphics states the caller saved (PDF 32000-1, 8.10.1).
        let saved_state = self.state.clone();
        let saved_stack = std::mem::take(&mut self.stack);
        self.state.ctm = matrix.then(self.state.ctm);
        contexts.push(form_context);
        self.run(&program, contexts, depth + 1);
        contexts.pop();
        self.stack = saved_stack;
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
    let program = match lex_content(&content_bytes) {
        Ok(program) => program,
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

    page_text.links = link_annotations(doc, page_dict);

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
        graphics: Graphics::default(),
        form_work: FormWork::default(),
        resource_error: None,
    };
    let mut contexts = vec![page_context];
    interpreter.run(&program, &mut contexts, 0);
    #[cfg(test)]
    {
        interpreter.cache.last_form_work = Some(interpreter.form_work);
    }
    if let Some(reason) = interpreter.resource_error {
        return Err(page_error(page, format!("resource_limit: {reason}")));
    }
    Ok(interpreter.finish())
}

/// Glyph names and the characters they stand for, sorted by name (bytes)
/// for binary search in [`glyph_char`]. Generated from the glyph list built
/// into `lopdf` 0.45 (the Adobe Glyph List plus the TeX names), restricted
/// to Latin, Greek, combining marks, punctuation, super- and subscripts,
/// letterlike symbols, arrows, mathematical operators, technical and
/// geometric shapes, and the Latin ligatures, without private-use targets;
/// plus the TeX glyph names `lopdf` lacks (`phi1`, `mapsto`, `vextendsingle`,
/// the AMS symbol names, ...), and these deliberate changes: `bardbl` is
/// U+2016, `Delta` and `mu` are the Greek letters, `triangleleft` and
/// `triangleright` point the way their `CMMI` glyphs do, the old-style
/// digits are digits, `circlecopyrt` is U+25EF, and the `CMEX` delimiter
/// pieces (`parenlefttp`, `braceex`, ...) are the U+239B to U+23AD pieces.
static GLYPH_NAMES: &[(&str, char)] = &[
    ("A", '\u{0041}'),
    ("AE", '\u{00C6}'),
    ("Aacute", '\u{00C1}'),
    ("Abreve", '\u{0102}'),
    ("Acircumflex", '\u{00C2}'),
    ("Adieresis", '\u{00C4}'),
    ("Agrave", '\u{00C0}'),
    ("Alpha", '\u{0391}'),
    ("Amacron", '\u{0100}'),
    ("Aogonek", '\u{0104}'),
    ("Aring", '\u{00C5}'),
    ("Atilde", '\u{00C3}'),
    ("B", '\u{0042}'),
    ("Beta", '\u{0392}'),
    ("C", '\u{0043}'),
    ("Cacute", '\u{0106}'),
    ("Ccaron", '\u{010C}'),
    ("Ccedilla", '\u{00C7}'),
    ("Ccircumflex", '\u{0108}'),
    ("Cdot", '\u{010A}'),
    ("Cdotaccent", '\u{010A}'),
    ("Chi", '\u{03A7}'),
    ("D", '\u{0044}'),
    ("Dbar", '\u{0110}'),
    ("Dcaron", '\u{010E}'),
    ("Dcroat", '\u{0110}'),
    ("Deicoptic", '\u{03EE}'),
    ("Delta", '\u{0394}'),
    ("Deltagreek", '\u{0394}'),
    ("Digammagreek", '\u{03DC}'),
    ("Dslash", '\u{0110}'),
    ("E", '\u{0045}'),
    ("Eacute", '\u{00C9}'),
    ("Ebreve", '\u{0114}'),
    ("Ecaron", '\u{011A}'),
    ("Ecircumflex", '\u{00CA}'),
    ("Edieresis", '\u{00CB}'),
    ("Edot", '\u{0116}'),
    ("Edotaccent", '\u{0116}'),
    ("Egrave", '\u{00C8}'),
    ("Emacron", '\u{0112}'),
    ("Eng", '\u{014A}'),
    ("Eogonek", '\u{0118}'),
    ("Epsilon", '\u{0395}'),
    ("Eta", '\u{0397}'),
    ("Eth", '\u{00D0}'),
    ("Euro", '\u{20AC}'),
    ("F", '\u{0046}'),
    ("Feicoptic", '\u{03E4}'),
    ("G", '\u{0047}'),
    ("Gamma", '\u{0393}'),
    ("Gangiacoptic", '\u{03EA}'),
    ("Gbreve", '\u{011E}'),
    ("Gcedilla", '\u{0122}'),
    ("Gcircumflex", '\u{011C}'),
    ("Gcommaaccent", '\u{0122}'),
    ("Gdot", '\u{0120}'),
    ("Gdotaccent", '\u{0120}'),
    ("Germandbls", '\u{0053}'),
    ("H", '\u{0048}'),
    ("H18533", '\u{25CF}'),
    ("H18543", '\u{25AA}'),
    ("H18551", '\u{25AB}'),
    ("H22073", '\u{25A1}'),
    ("Hbar", '\u{0126}'),
    ("Hcircumflex", '\u{0124}'),
    ("Horicoptic", '\u{03E8}'),
    ("I", '\u{0049}'),
    ("IJ", '\u{0132}'),
    ("Iacute", '\u{00CD}'),
    ("Ibreve", '\u{012C}'),
    ("Icircumflex", '\u{00CE}'),
    ("Idieresis", '\u{00CF}'),
    ("Idot", '\u{0130}'),
    ("Idotaccent", '\u{0130}'),
    ("Ifractur", '\u{2111}'),
    ("Ifraktur", '\u{2111}'),
    ("Igrave", '\u{00CC}'),
    ("Imacron", '\u{012A}'),
    ("Iogonek", '\u{012E}'),
    ("Iota", '\u{0399}'),
    ("Iotadieresis", '\u{03AA}'),
    ("Itilde", '\u{0128}'),
    ("J", '\u{004A}'),
    ("Jcircumflex", '\u{0134}'),
    ("K", '\u{004B}'),
    ("Kappa", '\u{039A}'),
    ("Kcedilla", '\u{0136}'),
    ("Kcommaaccent", '\u{0136}'),
    ("Kheicoptic", '\u{03E6}'),
    ("Koppagreek", '\u{03DE}'),
    ("L", '\u{004C}'),
    ("Lacute", '\u{0139}'),
    ("Lambda", '\u{039B}'),
    ("Lcaron", '\u{013D}'),
    ("Lcedilla", '\u{013B}'),
    ("Lcommaaccent", '\u{013B}'),
    ("Ldot", '\u{013F}'),
    ("Ldotaccent", '\u{013F}'),
    ("Lslash", '\u{0141}'),
    ("M", '\u{004D}'),
    ("Mu", '\u{039C}'),
    ("N", '\u{004E}'),
    ("Nacute", '\u{0143}'),
    ("Ncaron", '\u{0147}'),
    ("Ncedilla", '\u{0145}'),
    ("Ncommaaccent", '\u{0145}'),
    ("Ng", '\u{014A}'),
    ("Ntilde", '\u{00D1}'),
    ("Nu", '\u{039D}'),
    ("O", '\u{004F}'),
    ("OE", '\u{0152}'),
    ("Oacute", '\u{00D3}'),
    ("Obreve", '\u{014E}'),
    ("Ocircumflex", '\u{00D4}'),
    ("Odblacute", '\u{0150}'),
    ("Odieresis", '\u{00D6}'),
    ("Ograve", '\u{00D2}'),
    ("Ohm", '\u{2126}'),
    ("Ohungarumlaut", '\u{0150}'),
    ("Omacron", '\u{014C}'),
    ("Omega", '\u{2126}'),
    ("Omegagreek", '\u{03A9}'),
    ("Omicron", '\u{039F}'),
    ("Oslash", '\u{00D8}'),
    ("Otilde", '\u{00D5}'),
    ("P", '\u{0050}'),
    ("Phi", '\u{03A6}'),
    ("Pi", '\u{03A0}'),
    ("Psi", '\u{03A8}'),
    ("Q", '\u{0051}'),
    ("R", '\u{0052}'),
    ("Racute", '\u{0154}'),
    ("Rcaron", '\u{0158}'),
    ("Rcedilla", '\u{0156}'),
    ("Rcommaaccent", '\u{0156}'),
    ("Rfractur", '\u{211C}'),
    ("Rfraktur", '\u{211C}'),
    ("Rho", '\u{03A1}'),
    ("S", '\u{0053}'),
    ("SS", '\u{0053}'),
    ("Sacute", '\u{015A}'),
    ("Sampigreek", '\u{03E0}'),
    ("Scaron", '\u{0160}'),
    ("Scedilla", '\u{015E}'),
    ("Scircumflex", '\u{015C}'),
    ("Scommaaccent", '\u{0218}'),
    ("Sheicoptic", '\u{03E2}'),
    ("Shimacoptic", '\u{03EC}'),
    ("Sigma", '\u{03A3}'),
    ("Stigmagreek", '\u{03DA}'),
    ("T", '\u{0054}'),
    ("Tau", '\u{03A4}'),
    ("Tbar", '\u{0166}'),
    ("Tcaron", '\u{0164}'),
    ("Tcedilla", '\u{0162}'),
    ("Tcommaaccent", '\u{0162}'),
    ("Theta", '\u{0398}'),
    ("Thorn", '\u{00DE}'),
    ("U", '\u{0055}'),
    ("Uacute", '\u{00DA}'),
    ("Ubreve", '\u{016C}'),
    ("Ucircumflex", '\u{00DB}'),
    ("Udblacute", '\u{0170}'),
    ("Udieresis", '\u{00DC}'),
    ("Ugrave", '\u{00D9}'),
    ("Uhungarumlaut", '\u{0170}'),
    ("Umacron", '\u{016A}'),
    ("Uogonek", '\u{0172}'),
    ("Upsilon", '\u{03A5}'),
    ("Upsilon1", '\u{03D2}'),
    ("Upsilonacutehooksymbolgreek", '\u{03D3}'),
    ("Upsilondieresis", '\u{03AB}'),
    ("Upsilondieresishooksymbolgreek", '\u{03D4}'),
    ("Upsilonhooksymbol", '\u{03D2}'),
    ("Uring", '\u{016E}'),
    ("Utilde", '\u{0168}'),
    ("V", '\u{0056}'),
    ("W", '\u{0057}'),
    ("Wcircumflex", '\u{0174}'),
    ("X", '\u{0058}'),
    ("Xi", '\u{039E}'),
    ("Y", '\u{0059}'),
    ("Yacute", '\u{00DD}'),
    ("Ycircumflex", '\u{0176}'),
    ("Ydieresis", '\u{0178}'),
    ("Z", '\u{005A}'),
    ("Zacute", '\u{0179}'),
    ("Zcaron", '\u{017D}'),
    ("Zdot", '\u{017B}'),
    ("Zdotaccent", '\u{017B}'),
    ("Zeta", '\u{0396}'),
    ("a", '\u{0061}'),
    ("aacute", '\u{00E1}'),
    ("abreve", '\u{0103}'),
    ("acircumflex", '\u{00E2}'),
    ("acute", '\u{00B4}'),
    ("acutebelowcmb", '\u{0317}'),
    ("acutecmb", '\u{0301}'),
    ("acutecomb", '\u{0301}'),
    ("acutelowmod", '\u{02CF}'),
    ("adieresis", '\u{00E4}'),
    ("ae", '\u{00E6}'),
    ("afii00208", '\u{2015}'),
    ("afii61248", '\u{2105}'),
    ("afii61289", '\u{2113}'),
    ("afii61352", '\u{2116}'),
    ("afii61573", '\u{202C}'),
    ("afii61574", '\u{202D}'),
    ("afii61575", '\u{202E}'),
    ("agrave", '\u{00E0}'),
    ("aleph", '\u{2135}'),
    ("allequal", '\u{224C}'),
    ("alpha", '\u{03B1}'),
    ("alphatonos", '\u{03AC}'),
    ("amacron", '\u{0101}'),
    ("ampersand", '\u{0026}'),
    ("angbracketleft", '\u{27E8}'),
    ("angbracketleftBig", '\u{27E8}'),
    ("angbracketleftBigg", '\u{27E8}'),
    ("angbracketleftbig", '\u{27E8}'),
    ("angbracketleftbigg", '\u{27E8}'),
    ("angbracketright", '\u{27E9}'),
    ("angbracketrightBig", '\u{27E9}'),
    ("angbracketrightBigg", '\u{27E9}'),
    ("angbracketrightbig", '\u{27E9}'),
    ("angbracketrightbigg", '\u{27E9}'),
    ("angle", '\u{2220}'),
    ("angleleft", '\u{2329}'),
    ("angleleftBig", '\u{2329}'),
    ("angleleftBigg", '\u{2329}'),
    ("angleleftbig", '\u{2329}'),
    ("angleleftbigg", '\u{2329}'),
    ("angleright", '\u{232A}'),
    ("anglerightBig", '\u{232A}'),
    ("anglerightBigg", '\u{232A}'),
    ("anglerightbig", '\u{232A}'),
    ("anglerightbigg", '\u{232A}'),
    ("angstrom", '\u{212B}'),
    ("aogonek", '\u{0105}'),
    ("approaches", '\u{2250}'),
    ("approxequal", '\u{2248}'),
    ("approxequalorimage", '\u{2252}'),
    ("approximatelyequal", '\u{2245}'),
    ("arc", '\u{2312}'),
    ("aring", '\u{00E5}'),
    ("arrowboth", '\u{2194}'),
    ("arrowbothv", '\u{2195}'),
    ("arrowbt", '\u{2193}'),
    ("arrowdashdown", '\u{21E3}'),
    ("arrowdashleft", '\u{21E0}'),
    ("arrowdashright", '\u{21E2}'),
    ("arrowdashup", '\u{21E1}'),
    ("arrowdblboth", '\u{21D4}'),
    ("arrowdblbothv", '\u{21D5}'),
    ("arrowdbldown", '\u{21D3}'),
    ("arrowdblleft", '\u{21D0}'),
    ("arrowdblright", '\u{21D2}'),
    ("arrowdbltp", '\u{21D1}'),
    ("arrowdblup", '\u{21D1}'),
    ("arrowdblvertex", '\u{21D5}'),
    ("arrowdown", '\u{2193}'),
    ("arrowdownleft", '\u{2199}'),
    ("arrowdownright", '\u{2198}'),
    ("arrowdownwhite", '\u{21E9}'),
    ("arrowhookleft", '\u{21AA}'),
    ("arrowhookright", '\u{21A9}'),
    ("arrowleft", '\u{2190}'),
    ("arrowleftbothalf", '\u{21BD}'),
    ("arrowleftdbl", '\u{21D0}'),
    ("arrowleftdblstroke", '\u{21CD}'),
    ("arrowleftoverright", '\u{21C6}'),
    ("arrowlefttophalf", '\u{21BC}'),
    ("arrowleftwhite", '\u{21E6}'),
    ("arrownortheast", '\u{2197}'),
    ("arrownorthwest", '\u{2196}'),
    ("arrowright", '\u{2192}'),
    ("arrowrightbothalf", '\u{21C1}'),
    ("arrowrightdblstroke", '\u{21CF}'),
    ("arrowrightoverleft", '\u{21C4}'),
    ("arrowrighttophalf", '\u{21C0}'),
    ("arrowrightwhite", '\u{21E8}'),
    ("arrowsoutheast", '\u{2198}'),
    ("arrowsouthwest", '\u{2199}'),
    ("arrowtableft", '\u{21E4}'),
    ("arrowtabright", '\u{21E5}'),
    ("arrowtp", '\u{2191}'),
    ("arrowup", '\u{2191}'),
    ("arrowupdn", '\u{2195}'),
    ("arrowupdnbse", '\u{21A8}'),
    ("arrowupdownbase", '\u{21A8}'),
    ("arrowupleft", '\u{2196}'),
    ("arrowupleftofdown", '\u{21C5}'),
    ("arrowupright", '\u{2197}'),
    ("arrowupwhite", '\u{21E7}'),
    ("arrowvertex", '\u{2195}'),
    ("asciicircum", '\u{005E}'),
    ("asciitilde", '\u{007E}'),
    ("asterisk", '\u{002A}'),
    ("asteriskcentered", '\u{2217}'),
    ("asteriskmath", '\u{2217}'),
    ("asterism", '\u{2042}'),
    ("asymptoticallyequal", '\u{2243}'),
    ("at", '\u{0040}'),
    ("atilde", '\u{00E3}'),
    ("b", '\u{0062}'),
    ("backslash", '\u{005C}'),
    ("backslashBig", '\u{005C}'),
    ("backslashBigg", '\u{005C}'),
    ("backslashbig", '\u{005C}'),
    ("backslashbigg", '\u{005C}'),
    ("bar", '\u{007C}'),
    ("bardbl", '\u{2016}'),
    ("bardblex", '\u{2016}'),
    ("barex", '\u{007C}'),
    ("beamedsixteenthnotes", '\u{266C}'),
    ("because", '\u{2235}'),
    ("beta", '\u{03B2}'),
    ("betasymbolgreek", '\u{03D0}'),
    ("blackcircle", '\u{25CF}'),
    ("blackdiamond", '\u{25C6}'),
    ("blackdownpointingtriangle", '\u{25BC}'),
    ("blackleftpointingpointer", '\u{25C4}'),
    ("blackleftpointingtriangle", '\u{25C0}'),
    ("blacklowerlefttriangle", '\u{25E3}'),
    ("blacklowerrighttriangle", '\u{25E2}'),
    ("blackrectangle", '\u{25AC}'),
    ("blackrightpointingpointer", '\u{25BA}'),
    ("blackrightpointingtriangle", '\u{25B6}'),
    ("blacksmallsquare", '\u{25AA}'),
    ("blacksmilingface", '\u{263B}'),
    ("blacksquare", '\u{25A0}'),
    ("blackstar", '\u{2605}'),
    ("blackupperlefttriangle", '\u{25E4}'),
    ("blackupperrighttriangle", '\u{25E5}'),
    ("blackuppointingsmalltriangle", '\u{25B4}'),
    ("blackuppointingtriangle", '\u{25B2}'),
    ("braceex", '\u{23AA}'),
    ("braceleft", '\u{007B}'),
    ("braceleftBig", '\u{007B}'),
    ("braceleftBigg", '\u{007B}'),
    ("braceleftbig", '\u{007B}'),
    ("braceleftbigg", '\u{007B}'),
    ("braceleftbt", '\u{23A9}'),
    ("braceleftmid", '\u{23A8}'),
    ("bracelefttp", '\u{23A7}'),
    ("braceright", '\u{007D}'),
    ("bracerightBig", '\u{007D}'),
    ("bracerightBigg", '\u{007D}'),
    ("bracerightbig", '\u{007D}'),
    ("bracerightbigg", '\u{007D}'),
    ("bracerightbt", '\u{23AD}'),
    ("bracerightmid", '\u{23AC}'),
    ("bracerighttp", '\u{23AB}'),
    ("bracketleft", '\u{005B}'),
    ("bracketleftBig", '\u{005B}'),
    ("bracketleftBigg", '\u{005B}'),
    ("bracketleftbig", '\u{005B}'),
    ("bracketleftbigg", '\u{005B}'),
    ("bracketleftbt", '\u{23A3}'),
    ("bracketleftex", '\u{23A2}'),
    ("bracketlefttp", '\u{23A1}'),
    ("bracketright", '\u{005D}'),
    ("bracketrightBig", '\u{005D}'),
    ("bracketrightBigg", '\u{005D}'),
    ("bracketrightbig", '\u{005D}'),
    ("bracketrightbigg", '\u{005D}'),
    ("bracketrightbt", '\u{23A6}'),
    ("bracketrightex", '\u{23A5}'),
    ("bracketrighttp", '\u{23A4}'),
    ("breve", '\u{02D8}'),
    ("brevebelowcmb", '\u{032E}'),
    ("brevecmb", '\u{0306}'),
    ("breveinvertedbelowcmb", '\u{032F}'),
    ("breveinvertedcmb", '\u{0311}'),
    ("bridgebelowcmb", '\u{032A}'),
    ("brokenbar", '\u{00A6}'),
    ("bullet", '\u{2022}'),
    ("bulletinverse", '\u{25D8}'),
    ("bulletoperator", '\u{2219}'),
    ("bullseye", '\u{25CE}'),
    ("c", '\u{0063}'),
    ("cacute", '\u{0107}'),
    ("candrabinducmb", '\u{0310}'),
    ("capslock", '\u{21EA}'),
    ("careof", '\u{2105}'),
    ("caron", '\u{02C7}'),
    ("caronbelowcmb", '\u{032C}'),
    ("caroncmb", '\u{030C}'),
    ("carriagereturn", '\u{21B5}'),
    ("ccaron", '\u{010D}'),
    ("ccedilla", '\u{00E7}'),
    ("ccircumflex", '\u{0109}'),
    ("cdot", '\u{010B}'),
    ("cdotaccent", '\u{010B}'),
    ("cedilla", '\u{00B8}'),
    ("cedillacmb", '\u{0327}'),
    ("ceilingleft", '\u{2308}'),
    ("ceilingleftBig", '\u{2308}'),
    ("ceilingleftBigg", '\u{2308}'),
    ("ceilingleftbig", '\u{2308}'),
    ("ceilingleftbigg", '\u{2308}'),
    ("ceilingright", '\u{2309}'),
    ("ceilingrightBig", '\u{2309}'),
    ("ceilingrightBigg", '\u{2309}'),
    ("ceilingrightbig", '\u{2309}'),
    ("ceilingrightbigg", '\u{2309}'),
    ("cent", '\u{00A2}'),
    ("centigrade", '\u{2103}'),
    ("check", '\u{2713}'),
    ("checkmark", '\u{2713}'),
    ("chi", '\u{03C7}'),
    ("circle", '\u{25CB}'),
    ("circlecopyrt", '\u{25EF}'),
    ("circledivide", '\u{2298}'),
    ("circledot", '\u{2299}'),
    ("circledotdisplay", '\u{2299}'),
    ("circledottext", '\u{2299}'),
    ("circleminus", '\u{2296}'),
    ("circlemultiply", '\u{2297}'),
    ("circlemultiplydisplay", '\u{2297}'),
    ("circlemultiplytext", '\u{2297}'),
    ("circleot", '\u{2299}'),
    ("circleplus", '\u{2295}'),
    ("circleplusdisplay", '\u{2295}'),
    ("circleplustext", '\u{2295}'),
    ("circlewithlefthalfblack", '\u{25D0}'),
    ("circlewithrighthalfblack", '\u{25D1}'),
    ("circumflex", '\u{02C6}'),
    ("circumflexbelowcmb", '\u{032D}'),
    ("circumflexcmb", '\u{0302}'),
    ("clear", '\u{2327}'),
    ("club", '\u{2663}'),
    ("clubsuitblack", '\u{2663}'),
    ("clubsuitwhite", '\u{2667}'),
    ("colon", '\u{003A}'),
    ("colontriangularhalfmod", '\u{02D1}'),
    ("colontriangularmod", '\u{02D0}'),
    ("comma", '\u{002C}'),
    ("commaabovecmb", '\u{0313}'),
    ("commaaboverightcmb", '\u{0315}'),
    ("commareversedabovecmb", '\u{0314}'),
    ("commaturnedabovecmb", '\u{0312}'),
    ("compass", '\u{263C}'),
    ("congruent", '\u{2245}'),
    ("contintegraldisplay", '\u{222E}'),
    ("contintegraltext", '\u{222E}'),
    ("contourintegral", '\u{222E}'),
    ("control", '\u{2303}'),
    ("coproduct", '\u{2A3F}'),
    ("coproductdisplay", '\u{2210}'),
    ("coproducttext", '\u{2210}'),
    ("copyright", '\u{00A9}'),
    ("curlyand", '\u{22CF}'),
    ("curlyor", '\u{22CE}'),
    ("currency", '\u{00A4}'),
    ("d", '\u{0064}'),
    ("dagger", '\u{2020}'),
    ("daggerdbl", '\u{2021}'),
    ("dbar", '\u{0111}'),
    ("dblarchinvertedbelowcmb", '\u{032B}'),
    ("dblarrowleft", '\u{21D4}'),
    ("dblarrowright", '\u{21D2}'),
    ("dblgravecmb", '\u{030F}'),
    ("dblintegral", '\u{222C}'),
    ("dbllowline", '\u{2017}'),
    ("dbllowlinecmb", '\u{0333}'),
    ("dblverticalbar", '\u{2016}'),
    ("dblverticallineabovecmb", '\u{030E}'),
    ("dcaron", '\u{010F}'),
    ("dcroat", '\u{0111}'),
    ("defines", '\u{225C}'),
    ("degree", '\u{00B0}'),
    ("deicoptic", '\u{03EF}'),
    ("deleteleft", '\u{232B}'),
    ("deleteright", '\u{2326}'),
    ("delta", '\u{03B4}'),
    ("diamond", '\u{2662}'),
    ("diamond2", '\u{2666}'),
    ("diamondmath", '\u{22C4}'),
    ("diamondsuitwhite", '\u{2662}'),
    ("dieresis", '\u{00A8}'),
    ("dieresisbelowcmb", '\u{0324}'),
    ("dieresiscmb", '\u{0308}'),
    ("divide", '\u{00F7}'),
    ("divides", '\u{2223}'),
    ("divisionslash", '\u{2215}'),
    ("dmacron", '\u{0111}'),
    ("dollar", '\u{0024}'),
    ("dotacc", '\u{02D9}'),
    ("dotaccent", '\u{02D9}'),
    ("dotaccentcmb", '\u{0307}'),
    ("dotbelowcmb", '\u{0323}'),
    ("dotbelowcomb", '\u{0323}'),
    ("dotlessi", '\u{0131}'),
    ("dotlessj", '\u{0237}'),
    ("dotmath", '\u{22C5}'),
    ("dottedcircle", '\u{25CC}'),
    ("downslope", '\u{2572}'),
    ("downtackbelowcmb", '\u{031E}'),
    ("downtackmod", '\u{02D5}'),
    ("e", '\u{0065}'),
    ("eacute", '\u{00E9}'),
    ("earth", '\u{2641}'),
    ("ebreve", '\u{0115}'),
    ("ecaron", '\u{011B}'),
    ("ecircumflex", '\u{00EA}'),
    ("edieresis", '\u{00EB}'),
    ("edot", '\u{0117}'),
    ("edotaccent", '\u{0117}'),
    ("egrave", '\u{00E8}'),
    ("eight", '\u{0038}'),
    ("eighthnotebeamed", '\u{266B}'),
    ("eightinferior", '\u{2088}'),
    ("eightoldstyle", '\u{0038}'),
    ("eightsuperior", '\u{2078}'),
    ("element", '\u{2208}'),
    ("ellipsis", '\u{2026}'),
    ("ellipsisvertical", '\u{22EE}'),
    ("emacron", '\u{0113}'),
    ("emdash", '\u{2014}'),
    ("emptyset", '\u{2205}'),
    ("endash", '\u{2013}'),
    ("eng", '\u{014B}'),
    ("eogonek", '\u{0119}'),
    ("epsilon", '\u{03B5}'),
    ("epsilon1", '\u{03F5}'),
    ("epsiloninv", '\u{03F6}'),
    ("epsilontonos", '\u{03AD}'),
    ("equal", '\u{003D}'),
    ("equalsuperior", '\u{207C}'),
    ("equivalence", '\u{2261}'),
    ("equivasymptotic", '\u{224D}'),
    ("estimated", '\u{212E}'),
    ("eta", '\u{03B7}'),
    ("etatonos", '\u{03AE}'),
    ("eth", '\u{00F0}'),
    ("euro", '\u{20AC}'),
    ("exclam", '\u{0021}'),
    ("exclamdbl", '\u{203C}'),
    ("exclamdown", '\u{00A1}'),
    ("existential", '\u{2203}'),
    ("f", '\u{0066}'),
    ("f_f", '\u{FB00}'),
    ("f_f_i", '\u{FB03}'),
    ("f_f_l", '\u{FB04}'),
    ("f_i", '\u{FB01}'),
    ("f_l", '\u{FB02}'),
    ("fahrenheit", '\u{2109}'),
    ("feicoptic", '\u{03E5}'),
    ("female", '\u{2640}'),
    ("ff", '\u{FB00}'),
    ("ffi", '\u{FB03}'),
    ("ffl", '\u{FB04}'),
    ("fi", '\u{FB01}'),
    ("figuredash", '\u{2012}'),
    ("filledbox", '\u{25A0}'),
    ("filledrect", '\u{25AC}'),
    ("firsttonechinese", '\u{02C9}'),
    ("fisheye", '\u{25C9}'),
    ("five", '\u{0035}'),
    ("fiveinferior", '\u{2085}'),
    ("fiveoldstyle", '\u{0035}'),
    ("fivesuperior", '\u{2075}'),
    ("fl", '\u{FB02}'),
    ("flat", '\u{266D}'),
    ("floorleft", '\u{230A}'),
    ("floorleftBig", '\u{230A}'),
    ("floorleftBigg", '\u{230A}'),
    ("floorleftbig", '\u{230A}'),
    ("floorleftbigg", '\u{230A}'),
    ("floorright", '\u{230B}'),
    ("floorrightBig", '\u{230B}'),
    ("floorrightBigg", '\u{230B}'),
    ("floorrightbig", '\u{230B}'),
    ("floorrightbigg", '\u{230B}'),
    ("florin", '\u{0192}'),
    ("follows", '\u{227B}'),
    ("followsequal", '\u{227D}'),
    ("forall", '\u{2200}'),
    ("four", '\u{0034}'),
    ("fourinferior", '\u{2084}'),
    ("fouroldstyle", '\u{0034}'),
    ("foursuperior", '\u{2074}'),
    ("fourthtonechinese", '\u{02CB}'),
    ("fraction", '\u{2044}'),
    ("g", '\u{0067}'),
    ("gamma", '\u{03B3}'),
    ("gangiacoptic", '\u{03EB}'),
    ("gbreve", '\u{011F}'),
    ("gcedilla", '\u{0123}'),
    ("gcircumflex", '\u{011D}'),
    ("gcommaaccent", '\u{0123}'),
    ("gdot", '\u{0121}'),
    ("gdotaccent", '\u{0121}'),
    ("geometricallyequal", '\u{2251}'),
    ("germandbls", '\u{00DF}'),
    ("gradient", '\u{2207}'),
    ("grave", '\u{0060}'),
    ("gravebelowcmb", '\u{0316}'),
    ("gravecmb", '\u{0300}'),
    ("gravecomb", '\u{0300}'),
    ("gravelowmod", '\u{02CE}'),
    ("greater", '\u{003E}'),
    ("greaterequal", '\u{2265}'),
    ("greaterequalorless", '\u{22DB}'),
    ("greatermuch", '\u{226B}'),
    ("greaterorequalslant", '\u{2A7E}'),
    ("greaterorequivalent", '\u{2273}'),
    ("greaterorless", '\u{2277}'),
    ("greaterorsimilar", '\u{2273}'),
    ("greateroverequal", '\u{2267}'),
    ("guillemotleft", '\u{00AB}'),
    ("guillemotright", '\u{00BB}'),
    ("guilsinglleft", '\u{2039}'),
    ("guilsinglright", '\u{203A}'),
    ("h", '\u{0068}'),
    ("harpoonleftbarbup", '\u{21BC}'),
    ("harpoonleftdown", '\u{21BD}'),
    ("harpoonleftright", '\u{21CC}'),
    ("harpoonleftup", '\u{21BC}'),
    ("harpoonrightbarbup", '\u{21C0}'),
    ("harpoonrightdown", '\u{21C1}'),
    ("harpoonrightup", '\u{21C0}'),
    ("hatwide", '\u{0302}'),
    ("hatwider", '\u{0302}'),
    ("hatwiderr", '\u{0302}'),
    ("hatwidest", '\u{0302}'),
    ("hbar", '\u{0127}'),
    ("hcircumflex", '\u{0125}'),
    ("heart", '\u{2661}'),
    ("heart2", '\u{2665}'),
    ("heartsuitblack", '\u{2665}'),
    ("heartsuitwhite", '\u{2661}'),
    ("hookabovecomb", '\u{0309}'),
    ("hookcmb", '\u{0309}'),
    ("hookleftchar", '\u{21A9}'),
    ("hookpalatalizedbelowcmb", '\u{0321}'),
    ("hookretroflexbelowcmb", '\u{0322}'),
    ("hookrightchar", '\u{21AA}'),
    ("horicoptic", '\u{03E9}'),
    ("horizontalbar", '\u{2015}'),
    ("horncmb", '\u{031B}'),
    ("hotsprings", '\u{2668}'),
    ("house", '\u{2302}'),
    ("hungarumlaut", '\u{02DD}'),
    ("hungarumlautcmb", '\u{030B}'),
    ("hyphen", '\u{002D}'),
    ("hyphen_alt", '\u{2010}'),
    ("hyphenchar", '\u{002D}'),
    ("hyphentwo", '\u{2010}'),
    ("i", '\u{0069}'),
    ("iacute", '\u{00ED}'),
    ("ibreve", '\u{012D}'),
    ("icircumflex", '\u{00EE}'),
    ("idieresis", '\u{00EF}'),
    ("igrave", '\u{00EC}'),
    ("ij", '\u{0133}'),
    ("ilde", '\u{02DC}'),
    ("imacron", '\u{012B}'),
    ("imageorapproximatelyequal", '\u{2253}'),
    ("increment", '\u{2206}'),
    ("infinity", '\u{221E}'),
    ("integerdivide", '\u{2216}'),
    ("integral", '\u{222B}'),
    ("integralbottom", '\u{2321}'),
    ("integralbt", '\u{2321}'),
    ("integraldisplay", '\u{222B}'),
    ("integraltext", '\u{222B}'),
    ("integraltop", '\u{2320}'),
    ("integraltp", '\u{2320}'),
    ("intercal", '\u{22BA}'),
    ("interrobang", '\u{203D}'),
    ("intersection", '\u{2229}'),
    ("intersectiondisplay", '\u{22C2}'),
    ("intersectionsq", '\u{2293}'),
    ("intersectiontext", '\u{22C2}'),
    ("invbullet", '\u{25D8}'),
    ("invcircle", '\u{25D9}'),
    ("invsmileface", '\u{263B}'),
    ("iogonek", '\u{012F}'),
    ("iota", '\u{03B9}'),
    ("iotadieresis", '\u{03CA}'),
    ("iotatonos", '\u{03AF}'),
    ("itilde", '\u{0129}'),
    ("j", '\u{006A}'),
    ("jcircumflex", '\u{0135}'),
    ("k", '\u{006B}'),
    ("kappa", '\u{03BA}'),
    ("kappasymbolgreek", '\u{03F0}'),
    ("kcedilla", '\u{0137}'),
    ("kcommaaccent", '\u{0137}'),
    ("kgreenlandic", '\u{0138}'),
    ("kheicoptic", '\u{03E7}'),
    ("l", '\u{006C}'),
    ("lacute", '\u{013A}'),
    ("lambda", '\u{03BB}'),
    ("largecircle", '\u{25EF}'),
    ("latticetop", '\u{22A4}'),
    ("lcaron", '\u{013E}'),
    ("lcedilla", '\u{013C}'),
    ("lcommaaccent", '\u{013C}'),
    ("ldot", '\u{0140}'),
    ("ldotaccent", '\u{0140}'),
    ("leftangleabovecmb", '\u{031A}'),
    ("lefttackbelowcmb", '\u{0318}'),
    ("less", '\u{003C}'),
    ("lessequal", '\u{2264}'),
    ("lessequalorgreater", '\u{22DA}'),
    ("lessmuch", '\u{226A}'),
    ("lessorequalslant", '\u{2A7D}'),
    ("lessorequivalent", '\u{2272}'),
    ("lessorgreater", '\u{2276}'),
    ("lessorsimilar", '\u{2272}'),
    ("lessoverequal", '\u{2266}'),
    ("logicaland", '\u{2227}'),
    ("logicalanddisplay", '\u{22C0}'),
    ("logicalandtext", '\u{22C0}'),
    ("logicalnot", '\u{00AC}'),
    ("logicalnotreversed", '\u{2310}'),
    ("logicalor", '\u{2228}'),
    ("logicalordisplay", '\u{22C1}'),
    ("logicalortext", '\u{22C1}'),
    ("longs", '\u{017F}'),
    ("lowlinecmb", '\u{0332}'),
    ("lozenge", '\u{25CA}'),
    ("lscript", '\u{2113}'),
    ("lslash", '\u{0142}'),
    ("lsquare", '\u{2113}'),
    ("m", '\u{006D}'),
    ("macron", '\u{00AF}'),
    ("macronbelowcmb", '\u{0331}'),
    ("macroncmb", '\u{0304}'),
    ("macronlowmod", '\u{02CD}'),
    ("male", '\u{2642}'),
    ("mapsto", '\u{21A6}'),
    ("mars", '\u{2642}'),
    ("middot", '\u{00B7}'),
    ("minus", '\u{2212}'),
    ("minusbelowcmb", '\u{0320}'),
    ("minuscircle", '\u{2296}'),
    ("minusmod", '\u{02D7}'),
    ("minusplus", '\u{2213}'),
    ("minute", '\u{2032}'),
    ("mu", '\u{03BC}'),
    ("mu1", '\u{00B5}'),
    ("muchgreater", '\u{226B}'),
    ("muchless", '\u{226A}'),
    ("mugreek", '\u{03BC}'),
    ("multicloseleft", '\u{22C9}'),
    ("multicloseright", '\u{22CA}'),
    ("multiply", '\u{00D7}'),
    ("musicalnote", '\u{266A}'),
    ("musicalnotedbl", '\u{266B}'),
    ("musicflatsign", '\u{266D}'),
    ("musicsharpsign", '\u{266F}'),
    ("n", '\u{006E}'),
    ("nabla", '\u{2207}'),
    ("nacute", '\u{0144}'),
    ("napostrophe", '\u{0149}'),
    ("natural", '\u{266E}'),
    ("nbspace", '\u{00A0}'),
    ("ncaron", '\u{0148}'),
    ("ncedilla", '\u{0146}'),
    ("ncommaaccent", '\u{0146}'),
    ("negationslash", '\u{0338}'),
    ("ng", '\u{014B}'),
    ("nine", '\u{0039}'),
    ("nineinferior", '\u{2089}'),
    ("nineoldstyle", '\u{0039}'),
    ("ninesuperior", '\u{2079}'),
    ("nonbreakingspace", '\u{00A0}'),
    ("notcontains", '\u{220C}'),
    ("notelement", '\u{2209}'),
    ("notelementof", '\u{2209}'),
    ("notequal", '\u{2260}'),
    ("notfollows", '\u{2281}'),
    ("notgreater", '\u{226F}'),
    ("notgreaterequal", '\u{2271}'),
    ("notgreaternorequal", '\u{2271}'),
    ("notgreaternorless", '\u{2279}'),
    ("notidentical", '\u{2262}'),
    ("notless", '\u{226E}'),
    ("notlessequal", '\u{2270}'),
    ("notlessnorequal", '\u{2270}'),
    ("notparallel", '\u{2226}'),
    ("notprecedes", '\u{2280}'),
    ("notsimilar", '\u{2241}'),
    ("notsubset", '\u{2284}'),
    ("notsubsetoreql", '\u{2288}'),
    ("notsucceeds", '\u{2281}'),
    ("notsuperset", '\u{2285}'),
    ("nsuperior", '\u{207F}'),
    ("ntilde", '\u{00F1}'),
    ("nu", '\u{03BD}'),
    ("numbersign", '\u{0023}'),
    ("numero", '\u{2116}'),
    ("o", '\u{006F}'),
    ("oacute", '\u{00F3}'),
    ("obreve", '\u{014F}'),
    ("ocircumflex", '\u{00F4}'),
    ("odblacute", '\u{0151}'),
    ("odieresis", '\u{00F6}'),
    ("oe", '\u{0153}'),
    ("ogonek", '\u{02DB}'),
    ("ogonekcmb", '\u{0328}'),
    ("ograve", '\u{00F2}'),
    ("ohungarumlaut", '\u{0151}'),
    ("omacron", '\u{014D}'),
    ("omega", '\u{03C9}'),
    ("omega1", '\u{03D6}'),
    ("omegatonos", '\u{03CE}'),
    ("omicron", '\u{03BF}'),
    ("omicrontonos", '\u{03CC}'),
    ("one", '\u{0031}'),
    ("onedotenleader", '\u{2024}'),
    ("onehalf", '\u{00BD}'),
    ("oneinferior", '\u{2081}'),
    ("oneoldstyle", '\u{0031}'),
    ("onequarter", '\u{00BC}'),
    ("onesuperior", '\u{00B9}'),
    ("openbullet", '\u{25E6}'),
    ("option", '\u{2325}'),
    ("ordfeminine", '\u{00AA}'),
    ("ordmasculine", '\u{00BA}'),
    ("orthogonal", '\u{221F}'),
    ("oslash", '\u{00F8}'),
    ("otilde", '\u{00F5}'),
    ("overline", '\u{203E}'),
    ("overlinecmb", '\u{0305}'),
    ("overscore", '\u{00AF}'),
    ("owner", '\u{220B}'),
    ("p", '\u{0070}'),
    ("pagedown", '\u{21DF}'),
    ("pageup", '\u{21DE}'),
    ("paragraph", '\u{00B6}'),
    ("parallel", '\u{2225}'),
    ("parenleft", '\u{0028}'),
    ("parenleftBig", '\u{0028}'),
    ("parenleftBigg", '\u{0028}'),
    ("parenleftbig", '\u{0028}'),
    ("parenleftbigg", '\u{0028}'),
    ("parenleftbt", '\u{239D}'),
    ("parenleftex", '\u{239C}'),
    ("parenleftinferior", '\u{208D}'),
    ("parenleftsuperior", '\u{207D}'),
    ("parenlefttp", '\u{239B}'),
    ("parenright", '\u{0029}'),
    ("parenrightBig", '\u{0029}'),
    ("parenrightBigg", '\u{0029}'),
    ("parenrightbig", '\u{0029}'),
    ("parenrightbigg", '\u{0029}'),
    ("parenrightbt", '\u{23A0}'),
    ("parenrightex", '\u{239F}'),
    ("parenrightinferior", '\u{208E}'),
    ("parenrightsuperior", '\u{207E}'),
    ("parenrighttp", '\u{239E}'),
    ("partialdiff", '\u{2202}'),
    ("percent", '\u{0025}'),
    ("period", '\u{002E}'),
    ("periodcentered", '\u{00B7}'),
    ("perpendicular", '\u{22A5}'),
    ("pertenthousand", '\u{2031}'),
    ("perthousand", '\u{2030}'),
    ("phi", '\u{03C6}'),
    ("phi1", '\u{03C6}'),
    ("phi2", '\u{03D5}'),
    ("phisymbolgreek", '\u{03D5}'),
    ("pi", '\u{03C0}'),
    ("pi1", '\u{03D6}'),
    ("pisymbolgreek", '\u{03D6}'),
    ("plus", '\u{002B}'),
    ("plusbelowcmb", '\u{031F}'),
    ("pluscircle", '\u{2295}'),
    ("plusminus", '\u{00B1}'),
    ("plusmod", '\u{02D6}'),
    ("plussuperior", '\u{207A}'),
    ("pointingindexdownwhite", '\u{261F}'),
    ("pointingindexleftwhite", '\u{261C}'),
    ("pointingindexrightwhite", '\u{261E}'),
    ("pointingindexupwhite", '\u{261D}'),
    ("precedes", '\u{227A}'),
    ("precedesequal", '\u{227C}'),
    ("prescription", '\u{211E}'),
    ("prime", '\u{2032}'),
    ("primereversed", '\u{2035}'),
    ("product", '\u{220F}'),
    ("productdisplay", '\u{220F}'),
    ("producttext", '\u{220F}'),
    ("projective", '\u{2305}'),
    ("propellor", '\u{2318}'),
    ("propersubset", '\u{2282}'),
    ("propersuperset", '\u{2283}'),
    ("proportion", '\u{2237}'),
    ("proportional", '\u{221D}'),
    ("psi", '\u{03C8}'),
    ("punctdash", '\u{2014}'),
    ("q", '\u{0071}'),
    ("quarternote", '\u{2669}'),
    ("question", '\u{003F}'),
    ("questiondown", '\u{00BF}'),
    ("quotedbl", '\u{0022}'),
    ("quotedblbase", '\u{201E}'),
    ("quotedblleft", '\u{201C}'),
    ("quotedblright", '\u{201D}'),
    ("quoteleft", '\u{2018}'),
    ("quoteleftreversed", '\u{201B}'),
    ("quotereversed", '\u{201B}'),
    ("quoteright", '\u{2019}'),
    ("quoterightn", '\u{0149}'),
    ("quotesinglbase", '\u{201A}'),
    ("quotesingle", '\u{0027}'),
    ("r", '\u{0072}'),
    ("racute", '\u{0155}'),
    ("radical", '\u{221A}'),
    ("radicalBig", '\u{221A}'),
    ("radicalBigg", '\u{221A}'),
    ("radicalbig", '\u{221A}'),
    ("radicalbigg", '\u{221A}'),
    ("radicalbt", '\u{221A}'),
    ("rangedash", '\u{2013}'),
    ("ratio", '\u{2236}'),
    ("rcaron", '\u{0159}'),
    ("rcedilla", '\u{0157}'),
    ("rcommaaccent", '\u{0157}'),
    ("referencemark", '\u{203B}'),
    ("reflexsubset", '\u{2286}'),
    ("reflexsuperset", '\u{2287}'),
    ("registered", '\u{00AE}'),
    ("reversedtilde", '\u{223D}'),
    ("revlogicalnot", '\u{2310}'),
    ("rho", '\u{03C1}'),
    ("rho1", '\u{03F1}'),
    ("rhosymbolgreek", '\u{03F1}'),
    ("rightangle", '\u{221F}'),
    ("righttackbelowcmb", '\u{0319}'),
    ("righttriangle", '\u{22BF}'),
    ("ring", '\u{02DA}'),
    ("ringbelowcmb", '\u{0325}'),
    ("ringcmb", '\u{030A}'),
    ("ringhalfleftbelowcmb", '\u{031C}'),
    ("ringhalfleftcentered", '\u{02D3}'),
    ("ringhalfrightcentered", '\u{02D2}'),
    ("s", '\u{0073}'),
    ("sacute", '\u{015B}'),
    ("scaron", '\u{0161}'),
    ("scedilla", '\u{015F}'),
    ("scircumflex", '\u{015D}'),
    ("scommaaccent", '\u{0219}'),
    ("second", '\u{2033}'),
    ("secondtonechinese", '\u{02CA}'),
    ("section", '\u{00A7}'),
    ("semicolon", '\u{003B}'),
    ("seven", '\u{0037}'),
    ("seveninferior", '\u{2087}'),
    ("sevenoldstyle", '\u{0037}'),
    ("sevensuperior", '\u{2077}'),
    ("sfthyphen", '\u{00AD}'),
    ("sharp", '\u{266F}'),
    ("sheicoptic", '\u{03E3}'),
    ("shimacoptic", '\u{03ED}'),
    ("sigma", '\u{03C3}'),
    ("sigma1", '\u{03C2}'),
    ("sigmafinal", '\u{03C2}'),
    ("sigmalunatesymbolgreek", '\u{03F2}'),
    ("similar", '\u{223C}'),
    ("similarequal", '\u{2243}'),
    ("six", '\u{0036}'),
    ("sixinferior", '\u{2086}'),
    ("sixoldstyle", '\u{0036}'),
    ("sixsuperior", '\u{2076}'),
    ("slash", '\u{002F}'),
    ("slashBig", '\u{2215}'),
    ("slashBigg", '\u{2215}'),
    ("slashbig", '\u{2215}'),
    ("slashbigg", '\u{2215}'),
    ("slong", '\u{017F}'),
    ("slurabove", '\u{2322}'),
    ("slurbelow", '\u{2323}'),
    ("smileface", '\u{263A}'),
    ("softhyphen", '\u{00AD}'),
    ("soliduslongoverlaycmb", '\u{0338}'),
    ("solidusshortoverlaycmb", '\u{0337}'),
    ("space", '\u{0020}'),
    ("spacehackarabic", '\u{0020}'),
    ("spade", '\u{2660}'),
    ("spadesuitblack", '\u{2660}'),
    ("spadesuitwhite", '\u{2664}'),
    ("square", '\u{25A1}'),
    ("squarediagonalcrosshatchfill", '\u{25A9}'),
    ("squarehorizontalfill", '\u{25A4}'),
    ("squareorthogonalcrosshatchfill", '\u{25A6}'),
    ("squareplus", '\u{229E}'),
    ("squaresolid", '\u{25A0}'),
    ("squareupperlefttolowerrightfill", '\u{25A7}'),
    ("squareupperrighttolowerleftfill", '\u{25A8}'),
    ("squareverticalfill", '\u{25A5}'),
    ("squarewhitewithsmallblack", '\u{25A3}'),
    ("squiggleright", '\u{21DD}'),
    ("star", '\u{22C6}'),
    ("sterling", '\u{00A3}'),
    ("strokelongoverlaycmb", '\u{0336}'),
    ("strokeshortoverlaycmb", '\u{0335}'),
    ("subset", '\u{2282}'),
    ("subsetnoteql", '\u{228A}'),
    ("subsetnotequal", '\u{228A}'),
    ("subsetorequal", '\u{2286}'),
    ("subsetsqequal", '\u{2291}'),
    ("succeeds", '\u{227B}'),
    ("suchthat", '\u{220B}'),
    ("summation", '\u{2211}'),
    ("summationdisplay", '\u{2211}'),
    ("summationtext", '\u{2211}'),
    ("sun", '\u{263C}'),
    ("superset", '\u{2283}'),
    ("supersetnotequal", '\u{228B}'),
    ("supersetorequal", '\u{2287}'),
    ("supersetsqequal", '\u{2292}'),
    ("t", '\u{0074}'),
    ("tackdown", '\u{22A4}'),
    ("tackleft", '\u{22A3}'),
    ("tau", '\u{03C4}'),
    ("tbar", '\u{0167}'),
    ("tcaron", '\u{0165}'),
    ("tcedilla", '\u{0163}'),
    ("tcommaaccent", '\u{0163}'),
    ("telephone", '\u{2121}'),
    ("telephoneblack", '\u{260E}'),
    ("thereexists", '\u{2203}'),
    ("therefore", '\u{2234}'),
    ("theta", '\u{03B8}'),
    ("theta1", '\u{03D1}'),
    ("thetasymbolgreek", '\u{03D1}'),
    ("thorn", '\u{00FE}'),
    ("three", '\u{0033}'),
    ("threeinferior", '\u{2083}'),
    ("threeoldstyle", '\u{0033}'),
    ("threequarters", '\u{00BE}'),
    ("threesuperior", '\u{00B3}'),
    ("tie", '\u{2040}'),
    ("tilde", '\u{02DC}'),
    ("tildebelowcmb", '\u{0330}'),
    ("tildecmb", '\u{0303}'),
    ("tildecomb", '\u{0303}'),
    ("tildeoperator", '\u{223C}'),
    ("tildeoverlaycmb", '\u{0334}'),
    ("tildewide", '\u{0303}'),
    ("tildewider", '\u{0303}'),
    ("tildewiderr", '\u{0303}'),
    ("tildewidest", '\u{0303}'),
    ("timescircle", '\u{2297}'),
    ("trademark", '\u{2122}'),
    ("triagdn", '\u{25BC}'),
    ("triaglf", '\u{25C4}'),
    ("triagrt", '\u{25BA}'),
    ("triagup", '\u{25B2}'),
    ("triangle", '\u{25B3}'),
    ("triangleinv", '\u{25BD}'),
    ("triangleleft", '\u{25C3}'),
    ("triangleleftsld", '\u{25C0}'),
    ("triangleright", '\u{25B9}'),
    ("turnstileleft", '\u{22A2}'),
    ("turnstileright", '\u{22A3}'),
    ("two", '\u{0032}'),
    ("twodotenleader", '\u{2025}'),
    ("twodotleader", '\u{2025}'),
    ("twoinferior", '\u{2082}'),
    ("twooldstyle", '\u{0032}'),
    ("twosuperior", '\u{00B2}'),
    ("u", '\u{0075}'),
    ("uacute", '\u{00FA}'),
    ("ubreve", '\u{016D}'),
    ("ucircumflex", '\u{00FB}'),
    ("udblacute", '\u{0171}'),
    ("udieresis", '\u{00FC}'),
    ("ugrave", '\u{00F9}'),
    ("uhungarumlaut", '\u{0171}'),
    ("umacron", '\u{016B}'),
    ("underscore", '\u{005F}'),
    ("underscoredbl", '\u{2017}'),
    ("union", '\u{222A}'),
    ("uniondisplay", '\u{22C3}'),
    ("unionmulti", '\u{228E}'),
    ("unionmultidisplay", '\u{228E}'),
    ("unionmultitext", '\u{228E}'),
    ("unionsq", '\u{2294}'),
    ("unionsqdisplay", '\u{2294}'),
    ("unionsqtext", '\u{2294}'),
    ("uniontext", '\u{22C3}'),
    ("universal", '\u{2200}'),
    ("uogonek", '\u{0173}'),
    ("upsilon", '\u{03C5}'),
    ("upsilondieresis", '\u{03CB}'),
    ("upsilondieresistonos", '\u{03B0}'),
    ("upsilontonos", '\u{03CD}'),
    ("upslope", '\u{2571}'),
    ("uptackbelowcmb", '\u{031D}'),
    ("uptackmod", '\u{02D4}'),
    ("uring", '\u{016F}'),
    ("utilde", '\u{0169}'),
    ("v", '\u{0076}'),
    ("vector", '\u{20D7}'),
    ("venus", '\u{2640}'),
    ("verticalbar", '\u{007C}'),
    ("verticallineabovecmb", '\u{030D}'),
    ("verticallinebelowcmb", '\u{0329}'),
    ("verticallinelowmod", '\u{02CC}'),
    ("verticallinemod", '\u{02C8}'),
    ("vextenddouble", '\u{2225}'),
    ("vextendsingle", '\u{2223}'),
    ("w", '\u{0077}'),
    ("wcircumflex", '\u{0175}'),
    ("weierstrass", '\u{2118}'),
    ("whitebullet", '\u{25E6}'),
    ("whitecircle", '\u{25CB}'),
    ("whitecircleinverse", '\u{25D9}'),
    ("whitediamond", '\u{25C7}'),
    ("whitediamondcontainingblacksmalldiamond", '\u{25C8}'),
    ("whitedownpointingsmalltriangle", '\u{25BF}'),
    ("whitedownpointingtriangle", '\u{25BD}'),
    ("whiteleftpointingsmalltriangle", '\u{25C3}'),
    ("whiteleftpointingtriangle", '\u{25C1}'),
    ("whiterightpointingsmalltriangle", '\u{25B9}'),
    ("whiterightpointingtriangle", '\u{25B7}'),
    ("whitesmallsquare", '\u{25AB}'),
    ("whitesmilingface", '\u{263A}'),
    ("whitesquare", '\u{25A1}'),
    ("whitestar", '\u{2606}'),
    ("whitetelephone", '\u{260F}'),
    ("whiteuppointingsmalltriangle", '\u{25B5}'),
    ("whiteuppointingtriangle", '\u{25B3}'),
    ("wreathproduct", '\u{2240}'),
    ("x", '\u{0078}'),
    ("xi", '\u{03BE}'),
    ("y", '\u{0079}'),
    ("yacute", '\u{00FD}'),
    ("ycircumflex", '\u{0177}'),
    ("ydieresis", '\u{00FF}'),
    ("yen", '\u{00A5}'),
    ("yinyang", '\u{262F}'),
    ("yotgreek", '\u{03F3}'),
    ("z", '\u{007A}'),
    ("zacute", '\u{017A}'),
    ("zcaron", '\u{017E}'),
    ("zdot", '\u{017C}'),
    ("zdotaccent", '\u{017C}'),
    ("zero", '\u{0030}'),
    ("zeroinferior", '\u{2080}'),
    ("zerooldstyle", '\u{0030}'),
    ("zerosuperior", '\u{2070}'),
    ("zeta", '\u{03B6}'),
];
/// `OT1` (`CMR`, `CMBX`, ...): glyph names by code, as the embedded `CMR`,
/// `CMSS` and `CMBX` font programs of the dev corpus list them.
static OT1_NAMES: [&str; 128] = [
    "Gamma",
    "Delta",
    "Theta",
    "Lambda",
    "Xi",
    "Pi",
    "Sigma",
    "Upsilon",
    "Phi",
    "Psi",
    "Omega",
    "ff",
    "fi",
    "fl",
    "ffi",
    "ffl",
    "dotlessi",
    "dotlessj",
    "grave",
    "acute",
    "caron",
    "breve",
    "macron",
    "ring",
    "cedilla",
    "germandbls",
    "ae",
    "oe",
    "oslash",
    "AE",
    "OE",
    "Oslash",
    "suppress",
    "exclam",
    "quotedblright",
    "numbersign",
    "dollar",
    "percent",
    "ampersand",
    "quoteright",
    "parenleft",
    "parenright",
    "asterisk",
    "plus",
    "comma",
    "hyphen",
    "period",
    "slash",
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "colon",
    "semicolon",
    "exclamdown",
    "equal",
    "questiondown",
    "question",
    "at",
    "A",
    "B",
    "C",
    "D",
    "E",
    "F",
    "G",
    "H",
    "I",
    "J",
    "K",
    "L",
    "M",
    "N",
    "O",
    "P",
    "Q",
    "R",
    "S",
    "T",
    "U",
    "V",
    "W",
    "X",
    "Y",
    "Z",
    "bracketleft",
    "quotedblleft",
    "bracketright",
    "circumflex",
    "dotaccent",
    "quoteleft",
    "a",
    "b",
    "c",
    "d",
    "e",
    "f",
    "g",
    "h",
    "i",
    "j",
    "k",
    "l",
    "m",
    "n",
    "o",
    "p",
    "q",
    "r",
    "s",
    "t",
    "u",
    "v",
    "w",
    "x",
    "y",
    "z",
    "endash",
    "emdash",
    "hungarumlaut",
    "tilde",
    "dieresis",
];
/// `OML` (`CMMI`, `CMMIB`): glyph names by code, from the embedded programs.
static OML_NAMES: [&str; 128] = [
    "Gamma",
    "Delta",
    "Theta",
    "Lambda",
    "Xi",
    "Pi",
    "Sigma",
    "Upsilon",
    "Phi",
    "Psi",
    "Omega",
    "alpha",
    "beta",
    "gamma",
    "delta",
    "epsilon1",
    "zeta",
    "eta",
    "theta",
    "iota",
    "kappa",
    "lambda",
    "mu",
    "nu",
    "xi",
    "pi",
    "rho",
    "sigma",
    "tau",
    "upsilon",
    "phi",
    "chi",
    "psi",
    "omega",
    "epsilon",
    "theta1",
    "pi1",
    "rho1",
    "sigma1",
    "phi1",
    "arrowlefttophalf",
    "arrowleftbothalf",
    "arrowrighttophalf",
    "arrowrightbothalf",
    "arrowhookleft",
    "arrowhookright",
    "triangleright",
    "triangleleft",
    "zerooldstyle",
    "oneoldstyle",
    "twooldstyle",
    "threeoldstyle",
    "fouroldstyle",
    "fiveoldstyle",
    "sixoldstyle",
    "sevenoldstyle",
    "eightoldstyle",
    "nineoldstyle",
    "period",
    "comma",
    "less",
    "slash",
    "greater",
    "star",
    "partialdiff",
    "A",
    "B",
    "C",
    "D",
    "E",
    "F",
    "G",
    "H",
    "I",
    "J",
    "K",
    "L",
    "M",
    "N",
    "O",
    "P",
    "Q",
    "R",
    "S",
    "T",
    "U",
    "V",
    "W",
    "X",
    "Y",
    "Z",
    "flat",
    "natural",
    "sharp",
    "slurbelow",
    "slurabove",
    "lscript",
    "a",
    "b",
    "c",
    "d",
    "e",
    "f",
    "g",
    "h",
    "i",
    "j",
    "k",
    "l",
    "m",
    "n",
    "o",
    "p",
    "q",
    "r",
    "s",
    "t",
    "u",
    "v",
    "w",
    "x",
    "y",
    "z",
    "dotlessi",
    "dotlessj",
    "weierstrass",
    "vector",
    "tie",
];
/// `OMS` (`CMSY`, `CMBSY`): glyph names by code, from the embedded programs.
static OMS_NAMES: [&str; 128] = [
    "minus",
    "periodcentered",
    "multiply",
    "asteriskmath",
    "divide",
    "diamondmath",
    "plusminus",
    "minusplus",
    "circleplus",
    "circleminus",
    "circlemultiply",
    "circledivide",
    "circledot",
    "circlecopyrt",
    "openbullet",
    "bullet",
    "equivasymptotic",
    "equivalence",
    "reflexsubset",
    "reflexsuperset",
    "lessequal",
    "greaterequal",
    "precedesequal",
    "followsequal",
    "similar",
    "approxequal",
    "propersubset",
    "propersuperset",
    "lessmuch",
    "greatermuch",
    "precedes",
    "follows",
    "arrowleft",
    "arrowright",
    "arrowup",
    "arrowdown",
    "arrowboth",
    "arrownortheast",
    "arrowsoutheast",
    "similarequal",
    "arrowdblleft",
    "arrowdblright",
    "arrowdblup",
    "arrowdbldown",
    "arrowdblboth",
    "arrownorthwest",
    "arrowsouthwest",
    "proportional",
    "prime",
    "infinity",
    "element",
    "owner",
    "triangle",
    "triangleinv",
    "negationslash",
    "mapsto",
    "universal",
    "existential",
    "logicalnot",
    "emptyset",
    "Rfractur",
    "Ifractur",
    "latticetop",
    "perpendicular",
    "aleph",
    "A",
    "B",
    "C",
    "D",
    "E",
    "F",
    "G",
    "H",
    "I",
    "J",
    "K",
    "L",
    "M",
    "N",
    "O",
    "P",
    "Q",
    "R",
    "S",
    "T",
    "U",
    "V",
    "W",
    "X",
    "Y",
    "Z",
    "union",
    "intersection",
    "unionmulti",
    "logicaland",
    "logicalor",
    "turnstileleft",
    "turnstileright",
    "floorleft",
    "floorright",
    "ceilingleft",
    "ceilingright",
    "braceleft",
    "braceright",
    "angbracketleft",
    "angbracketright",
    "bar",
    "bardbl",
    "arrowbothv",
    "arrowdblbothv",
    "backslash",
    "wreathproduct",
    "radical",
    "coproduct",
    "nabla",
    "integral",
    "unionsq",
    "intersectionsq",
    "subsetsqequal",
    "supersetsqequal",
    "section",
    "dagger",
    "daggerdbl",
    "paragraph",
    "club",
    "diamond",
    "heart",
    "spade",
];
/// `OMX` (`CMEX`): the codes the embedded programs of the dev corpus list.
static OMX_NAMES: &[(u8, &str)] = &[
    (0, "parenleftbig"),
    (1, "parenrightbig"),
    (2, "bracketleftbig"),
    (3, "bracketrightbig"),
    (4, "floorleftbig"),
    (5, "floorrightbig"),
    (6, "ceilingleftbig"),
    (7, "ceilingrightbig"),
    (8, "braceleftbig"),
    (9, "bracerightbig"),
    (10, "angbracketleftbig"),
    (11, "angbracketrightbig"),
    (12, "vextendsingle"),
    (13, "vextenddouble"),
    (14, "slashbig"),
    (16, "parenleftBig"),
    (17, "parenrightBig"),
    (18, "parenleftbigg"),
    (19, "parenrightbigg"),
    (20, "bracketleftbigg"),
    (21, "bracketrightbigg"),
    (26, "braceleftbigg"),
    (27, "bracerightbigg"),
    (32, "parenleftBigg"),
    (33, "parenrightBigg"),
    (34, "bracketleftBigg"),
    (35, "bracketrightBigg"),
    (40, "braceleftBigg"),
    (41, "bracerightBigg"),
    (48, "parenlefttp"),
    (49, "parenrighttp"),
    (50, "bracketlefttp"),
    (51, "bracketrighttp"),
    (52, "bracketleftbt"),
    (53, "bracketrightbt"),
    (54, "bracketleftex"),
    (55, "bracketrightex"),
    (56, "bracelefttp"),
    (57, "bracerighttp"),
    (58, "braceleftbt"),
    (59, "bracerightbt"),
    (60, "braceleftmid"),
    (61, "bracerightmid"),
    (62, "braceex"),
    (64, "parenleftbt"),
    (65, "parenrightbt"),
    (66, "parenleftex"),
    (67, "parenrightex"),
    (68, "angbracketleftBig"),
    (69, "angbracketrightBig"),
    (80, "summationtext"),
    (81, "producttext"),
    (82, "integraltext"),
    (83, "uniontext"),
    (87, "logicalortext"),
    (88, "summationdisplay"),
    (89, "productdisplay"),
    (90, "integraldisplay"),
    (91, "uniondisplay"),
    (92, "intersectiondisplay"),
    (95, "logicalordisplay"),
    (96, "coproducttext"),
    (97, "coproductdisplay"),
    (98, "hatwide"),
    (99, "hatwider"),
    (100, "hatwidest"),
    (101, "tildewide"),
    (102, "tildewider"),
    (104, "bracketleftBig"),
    (105, "bracketrightBig"),
    (106, "floorleftBig"),
    (107, "floorrightBig"),
    (108, "ceilingleftBig"),
    (109, "ceilingrightBig"),
    (110, "braceleftBig"),
    (111, "bracerightBig"),
    (112, "radicalbig"),
    (113, "radicalBig"),
    (114, "radicalbigg"),
    (115, "radicalBigg"),
    (116, "radicalbt"),
    (117, "radicalvertex"),
    (118, "radicaltp"),
    (122, "bracehtipdownleft"),
    (123, "bracehtipdownright"),
    (124, "bracehtipupleft"),
    (125, "bracehtipupright"),
];
/// `CMTT` codes whose glyph differs from `StandardEncoding` or was observed,
/// from the embedded programs of the dev corpus.
static TYPEWRITER_NAMES: &[(u8, &str)] = &[
    (13, "quotesingle"),
    (34, "quotedbl"),
    (35, "numbersign"),
    (37, "percent"),
    (39, "quoteright"),
    (40, "parenleft"),
    (41, "parenright"),
    (43, "plus"),
    (44, "comma"),
    (45, "hyphen"),
    (46, "period"),
    (47, "slash"),
    (48, "zero"),
    (49, "one"),
    (50, "two"),
    (51, "three"),
    (52, "four"),
    (53, "five"),
    (54, "six"),
    (55, "seven"),
    (56, "eight"),
    (57, "nine"),
    (58, "colon"),
    (60, "less"),
    (61, "equal"),
    (62, "greater"),
    (63, "question"),
    (64, "at"),
    (65, "A"),
    (66, "B"),
    (67, "C"),
    (68, "D"),
    (69, "E"),
    (70, "F"),
    (71, "G"),
    (72, "H"),
    (73, "I"),
    (74, "J"),
    (75, "K"),
    (76, "L"),
    (77, "M"),
    (78, "N"),
    (79, "O"),
    (80, "P"),
    (82, "R"),
    (83, "S"),
    (84, "T"),
    (85, "U"),
    (86, "V"),
    (87, "W"),
    (89, "Y"),
    (91, "bracketleft"),
    (93, "bracketright"),
    (95, "underscore"),
    (97, "a"),
    (98, "b"),
    (99, "c"),
    (100, "d"),
    (101, "e"),
    (102, "f"),
    (103, "g"),
    (104, "h"),
    (105, "i"),
    (106, "j"),
    (107, "k"),
    (108, "l"),
    (109, "m"),
    (110, "n"),
    (111, "o"),
    (112, "p"),
    (113, "q"),
    (114, "r"),
    (115, "s"),
    (116, "t"),
    (117, "u"),
    (118, "v"),
    (119, "w"),
    (120, "x"),
    (121, "y"),
    (122, "z"),
];
/// `MSAM`: the codes the embedded programs of the dev corpus list.
static MSAM_NAMES: &[(u8, &str)] = &[
    (1, "squareplus"),
    (3, "square"),
    (4, "squaresolid"),
    (10, "harpoonleftright"),
    (32, "squiggleright"),
    (38, "greaterorsimilar"),
    (44, "defines"),
    (46, "lessorsimilar"),
    (54, "lessorequalslant"),
    (62, "greaterorequalslant"),
    (70, "star"),
    (74, "triangleleftsld"),
    (88, "check"),
    (124, "intercal"),
];
/// `MSBM`: the codes the embedded programs of the dev corpus list.
static MSBM_NAMES: &[(u8, &str)] = &[
    (40, "subsetnoteql"),
    (63, "emptyset"),
    (65, "A"),
    (67, "C"),
    (68, "D"),
    (69, "E"),
    (73, "I"),
    (76, "L"),
    (78, "N"),
    (80, "P"),
    (82, "R"),
    (83, "S"),
    (84, "T"),
    (90, "Z"),
    (92, "hatwider"),
    (110, "multicloseleft"),
    (111, "multicloseright"),
    (114, "integerdivide"),
];

#[cfg(test)]
mod tests {
    use lopdf::content::{Content, Operation};
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
        config.insert("content".to_string(), "5".to_string());
        config.insert("encodings".to_string(), "1".to_string());
        assert_eq!(identity.config_digest, config_digest(&config));
        // Nor the digest from before figures.
        config.insert("content".to_string(), "2".to_string());
        assert_ne!(identity.config_digest, config_digest(&config));
        config.insert("content".to_string(), "3".to_string());
        assert_ne!(identity.config_digest, config_digest(&config));
        config.insert("content".to_string(), "5".to_string());
        // Nor the digest from before the TeX encodings.
        config.remove("encodings");
        assert_ne!(identity.config_digest, config_digest(&config));
        config.insert("encodings".to_string(), "1".to_string());
        // The digests before ligature expansion and before the streaming
        // lexer must not be reused.
        config.insert("content".to_string(), "1".to_string());
        assert_ne!(identity.config_digest, config_digest(&config));
        config.remove("content");
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

    /// The page produced by showing `shown` (one hex string) with the font
    /// `make_font` returns as `/F1`.
    fn show_with_font<F>(shown: &[u8], make_font: F) -> PageText
    where
        F: FnOnce(&mut Document) -> Dictionary,
    {
        let ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![10.into(), 10.into()]),
            Operation::new(
                "Tj",
                vec![Object::String(shown.to_vec(), StringFormat::Hexadecimal)],
            ),
            Operation::new("ET", vec![]),
        ];
        let bytes = build_pdf_with_font(vec![ops], None, make_font);
        let mut session = open_session(&bytes);
        session.page_text(1).unwrap()
    }

    /// The clear-text start of a Type1 font program whose built-in encoding
    /// puts `/element` at 50 and `/bardbl` at 107.
    const TYPE1_PROGRAM: &[u8] = b"%!PS-AdobeFont-1.0: Foo 1.0\n/FontName /Foo def\n\
        /Encoding 256 array\n0 1 255 {1 index exch /.notdef put} for\n\
        dup 50 /element put\ndup 107/bardbl put\nreadonly def\ncurrentdict end\n\
        currentfile eexec\ndup 51 /A put\n";

    #[test]
    fn tex_symbol_font_without_encoding_uses_its_builtin_encoding() {
        for base_font in ["CMSY10", "ABCDEF+CMSY10", "cmsy7"] {
            let page = show_with_font(&[0x32, 0x6B, 0x00, 0x66, 0x67, 0xA1], |_| {
                dictionary! {
                    "Type" => "Font",
                    "Subtype" => "Type1",
                    "BaseFont" => base_font,
                }
            });
            assert_eq!(page.spans.len(), 1);
            // Not "2k" and a dropped byte, as `StandardEncoding` gives.
            assert_eq!(page.spans[0].text, "\u{2208}\u{2016}\u{2212}{}\u{2212}");
            assert!(page.warnings.is_empty(), "{:?}", page.warnings);
        }
    }

    #[test]
    fn tex_math_italic_and_extension_fonts_use_their_builtin_encodings() {
        let page = show_with_font(&[0x0B, 0x15, 0x40, 0x61], |_| {
            dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "CMMI10" }
        });
        assert_eq!(page.spans[0].text, "\u{3B1}\u{3BB}\u{2202}a");
        let page = show_with_font(&[0x58, 0x5A, 0x08], |_| {
            dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "CMEX10" }
        });
        assert_eq!(page.spans[0].text, "\u{2211}\u{222B}{");
    }

    #[test]
    fn differences_resolve_tex_glyph_names_over_the_base() {
        let page = show_with_font(&[50, 51, 52], |_| {
            dictionary! {
                "Type" => "Font",
                "Subtype" => "Type1",
                "BaseFont" => "Times-Roman",
                "Encoding" => dictionary! {
                    "Type" => "Encoding",
                    "Differences" => vec![50.into(), "element".into(), "bardbl".into()],
                },
            }
        });
        assert_eq!(page.spans[0].text, "\u{2208}\u{2016}4");
        assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    }

    #[test]
    fn differences_over_a_tex_font_keep_its_builtin_base() {
        // No `/BaseEncoding`: codes outside `/Differences` use the font's
        // built-in (`OMS`) encoding, not `StandardEncoding`.
        let page = show_with_font(&[0x32, 0x41], |_| {
            dictionary! {
                "Type" => "Font",
                "Subtype" => "Type1",
                "BaseFont" => "CMSY10",
                "Encoding" => dictionary! {
                    "Differences" => vec![65.into(), "infinity".into()],
                },
            }
        });
        assert_eq!(page.spans[0].text, "\u{2208}\u{221E}");
    }

    #[test]
    fn unknown_glyph_name_is_unmapped_and_warned_without_losing_the_rest() {
        let page = show_with_font(&[66, 65], |_| {
            dictionary! {
                "Type" => "Font",
                "Subtype" => "Type1",
                "BaseFont" => "Times-Roman",
                "Encoding" => dictionary! {
                    "Type" => "Encoding",
                    "Differences" => vec![65.into(), "eacute".into(), "zzunknownglyph".into()],
                },
            }
        });
        // `lopdf` would have dropped the whole encoding for
        // `StandardEncoding` and read "BA".
        assert_eq!(page.spans[0].text, "\u{E9}\u{FFFD}");
        assert_eq!(
            page.warnings,
            vec!["font F1: 1 unmapped byte(s); U+FFFD used".to_string()]
        );
    }

    #[test]
    fn tex_text_font_ligatures_are_still_expanded() {
        let page = show_with_font(&[12, 0x6E, 0x64, 0x7B, 0x7C], |_| {
            dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "CMR10" }
        });
        assert_eq!(page.spans[0].text, "find\u{2013}\u{2014}");
        assert_eq!(page.warnings, vec!["ligatures expanded: 1".to_string()]);
    }

    #[test]
    fn embedded_type1_program_supplies_the_builtin_encoding() {
        let page = show_with_font(&[50, 107], |doc| {
            let program = Stream::new(
                dictionary! { "Length1" => i64::try_from(TYPE1_PROGRAM.len()).unwrap() },
                TYPE1_PROGRAM.to_vec(),
            );
            let program_id = doc.add_object(program);
            let descriptor_id = doc.add_object(dictionary! {
                "Type" => "FontDescriptor",
                "FontName" => "Foo",
                "FontFile" => program_id,
            });
            dictionary! {
                "Type" => "Font",
                "Subtype" => "Type1",
                "BaseFont" => "Foo",
                "FontDescriptor" => descriptor_id,
            }
        });
        assert_eq!(page.spans[0].text, "\u{2208}\u{2016}");
        assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    }

    #[test]
    fn type1_encoding_reads_the_clear_text_array_only() {
        let codes = type1_encoding(TYPE1_PROGRAM).unwrap();
        assert_eq!(
            codes,
            vec![(50, b"element".to_vec()), (107, b"bardbl".to_vec())]
        );
        let standard = b"/FontName /Foo def\n/Encoding StandardEncoding def\ncurrentfile eexec\n";
        assert_eq!(type1_encoding(standard), None);
        assert_eq!(type1_encoding(b"/FontName /Foo def\n"), None);
    }

    #[test]
    fn glyph_names_table_is_sorted_and_unique() {
        for pair in GLYPH_NAMES.windows(2) {
            assert!(
                pair[0].0.as_bytes() < pair[1].0.as_bytes(),
                "{} >= {}",
                pair[0].0,
                pair[1].0
            );
        }
        for names in [&OT1_NAMES, &OML_NAMES, &OMS_NAMES] {
            let doc = Document::with_version("1.5");
            let resolved = names
                .iter()
                .filter(|name| glyph_char(&doc, name.as_bytes()).is_some())
                .count();
            // Only `OT1` code 32 (`suppress`, the Polish L stroke) is left.
            assert!(resolved >= 127, "{resolved}");
        }
    }

    #[test]
    fn glyph_names_resolve_through_every_form() {
        let doc = Document::with_version("1.5");
        let cases: [(&[u8], Option<char>); 13] = [
            (b"element", Some('\u{2208}')),
            (b"bardbl", Some('\u{2016}')),
            (b"summationdisplay", Some('\u{2211}')),
            (b"fi", Some('\u{FB01}')),
            (b"uni2208", Some('\u{2208}')),
            (b"u1D400", Some('\u{1D400}')),
            (b"a.sc", Some('a')),
            // Outside the static table: `lopdf`'s own glyph list.
            (b"afii10017", Some('\u{410}')),
            (b"uni0003", None),
            (b"g37", None),
            (b"cid1024", None),
            (b".notdef", None),
            (b"zzunknownglyph", None),
        ];
        for (name, expected) in cases {
            assert_eq!(
                glyph_char(&doc, name),
                expected,
                "{}",
                String::from_utf8_lossy(name)
            );
        }
    }

    #[test]
    fn tex_fonts_are_recognised_by_family_and_size() {
        assert_eq!(tex_encoding(b"CMSY10"), Some(TexEncoding::Oms));
        assert_eq!(tex_encoding(b"XYZABC+cmmi7"), Some(TexEncoding::Oml));
        assert_eq!(tex_encoding(b"CMSSBX10"), Some(TexEncoding::Ot1));
        assert_eq!(tex_encoding(b"CMTI12"), Some(TexEncoding::Ot1Italic));
        assert_eq!(tex_encoding(b"CMTT10"), Some(TexEncoding::Typewriter));
        assert_eq!(tex_encoding(b"MSBM10"), Some(TexEncoding::Msbm));
        assert_eq!(tex_encoding(b"CMSY"), None);
        assert_eq!(tex_encoding(b"CMUSerif-Roman"), None);
        assert_eq!(tex_encoding(b"Helvetica"), None);
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
            graphics: Graphics::default(),
            form_work: FormWork::default(),
            resource_error: None,
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
    fn form_cache_stops_growing_at_its_byte_budget() {
        let mut cache = SessionCache::default();
        let big = Rc::new(TextProgram {
            ops: Vec::new(),
            operands: vec![Object::string_literal(vec![
                b'x';
                MAX_FORM_CACHE_BYTES / 2 + 1
            ])],
            paths: Vec::new(),
        });
        assert!(cache.insert_form((1, 0), &big));
        assert!(cache.insert_form((2, 0), &big), "evict the oldest entry");
        assert!(!cache.forms.contains_key(&(1, 0)));
        assert_eq!(cache.forms.len(), 1);
        let small = Rc::new(TextProgram {
            ops: Vec::new(),
            operands: Vec::new(),
            paths: Vec::new(),
        });
        assert!(cache.insert_form((3, 0), &small));
        assert_eq!(cache.forms.len(), 2);
    }

    #[test]
    fn tiny_forms_are_charged_and_cache_cardinality_is_bounded() {
        let mut cache = SessionCache::default();
        let empty = Rc::new(TextProgram::default());
        for id in 1..=10_000 {
            assert!(cache.insert_form((id, 0), &empty));
            assert!(cache.forms.len() <= MAX_FORM_CACHE_ENTRIES);
            assert_eq!(cache.form_order.len(), cache.forms.len());
            assert_eq!(cache.form_bytes, cache.forms.len() * MIN_FORM_CHARGE);
        }
        let before = cache.form_bytes;
        assert!(cache.insert_form((10_000, 0), &empty));
        assert_eq!(
            cache.form_bytes, before,
            "duplicate insert is not charged twice"
        );
        assert!(cache.forms.contains_key(&(10_000, 0)));
        assert!(!cache.forms.contains_key(&(1, 0)));
    }

    #[test]
    fn form_charge_includes_spare_capacity_and_nested_dictionary_data() {
        let program = TextProgram {
            operands: vec![Object::Dictionary(dictionary! {
                "Data" => Object::string_literal(Vec::<u8>::with_capacity(4096)),
            })],
            ops: Vec::with_capacity(100),
            paths: Vec::new(),
        };
        assert!(program.estimated_bytes() >= 4096 + 100 * size_of::<TextOp>());
    }

    fn replace_test_form(session: &mut LopdfSession, mut content: Stream, direct: bool) {
        let (id, original) = session
            .doc
            .objects
            .iter()
            .find_map(|(id, obj)| {
                let stream = obj.as_stream().ok()?;
                (stream.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Form"))
                    .then_some((*id, stream.clone()))
            })
            .unwrap();
        content.dict.set("Subtype", "Form");
        content.dict.set(
            "Resources",
            original.dict.get(b"Resources").unwrap().clone(),
        );
        session
            .doc
            .objects
            .insert(id, Object::Stream(content.clone()));
        if direct {
            for obj in session.doc.objects.values_mut() {
                if let Ok(dict) = obj.as_dict_mut()
                    && let Ok(xobjects) = dict.get_mut(b"XObject").and_then(Object::as_dict_mut)
                {
                    xobjects.set("X1", Object::Stream(content.clone()));
                }
            }
        }
    }

    #[test]
    fn oversized_compressed_form_is_rejected_before_lexing() {
        let bytes = build_pdf(
            vec![vec![Operation::new("Do", vec!["X1".into()])]],
            Some(vec![]),
        );
        let mut session = open_session(&bytes);
        let mut stream = Stream::new(dictionary! {}, vec![b' '; MAX_FORM_DECODE_BYTES + 1]);
        stream.compress().unwrap();
        assert!(stream.content.len() < MAX_FORM_DECODE_BYTES / 100);
        replace_test_form(&mut session, stream, false);
        let error = session.page_text(1).unwrap_err().to_string();
        assert!(
            error.contains("resource_limit: Form decoded stream"),
            "{error}"
        );
        assert!(
            session.cache.forms.is_empty(),
            "never lexed/cached the compressed bytes as fallback"
        );
    }

    #[test]
    fn repeated_uncached_forms_stop_at_the_page_decode_budget() {
        let calls = vec![Operation::new("Do", vec!["X1".into()]); 9];
        let bytes = build_pdf(vec![calls], Some(vec![]));
        let mut session = open_session(&bytes);
        let stream = Stream::new(dictionary! {}, vec![b' '; MAX_FORM_DECODE_BYTES]);
        replace_test_form(&mut session, stream, true);
        let error = session.page_text(1).unwrap_err().to_string();
        assert!(
            error.contains("resource_limit: Form decode byte budget"),
            "{error}"
        );
        assert!(
            session.cache.forms.is_empty(),
            "direct Forms cannot be cached by object id"
        );
    }

    #[test]
    fn cached_empty_forms_have_a_work_limit_and_page_budgets_reset() {
        let many = vec![Operation::new("Do", vec!["X1".into()]); MAX_PAGE_FORM_CALLS + 1];
        let once = vec![Operation::new("Do", vec!["X1".into()])];
        let bytes = build_pdf(vec![many, once], Some(vec![]));
        let mut session = open_session(&bytes);
        let error = session.page_text(1).unwrap_err().to_string();
        assert!(error.contains("resource_limit: Form invocation"), "{error}");
        assert_eq!(session.cache.forms.len(), 1);
        assert!(session.page_text(2).is_ok());
    }

    #[test]
    fn hostile_form_filter_metadata_is_rejected_before_decoding() {
        let mut stream = Stream::new(
            dictionary! {
                "Filter" => vec![Object::Name(b"FlateDecode".to_vec()); MAX_FORM_FILTERS + 1],
            },
            vec![],
        );
        assert!(form_decode_policy(&stream).is_none());
        stream.dict.set("Filter", "FlateDecode");
        stream.dict.set(
            "DecodeParms",
            dictionary! {
                "Predictor" => 12,
                "Columns" => i64::MAX,
                "Colors" => i64::MAX,
            },
        );
        assert!(form_decode_policy(&stream).is_none());
    }

    #[test]
    fn subbyte_tiff_accumulator_is_bounded_before_decoding() {
        let bytes = build_pdf(
            vec![vec![Operation::new("Do", vec!["X1".into()])]],
            Some(vec![]),
        );
        for component_bits in [1, 2, 4] {
            // Each packed row fits exactly, while Vec<u16> would allocate
            // 128 / 64 / 32 MiB respectively, even for one decoded byte.
            let colors = MAX_FORM_DECODE_BYTES * 8 / component_bits;
            let mut stream = flate_test_form(&[0]);
            stream.dict.set(
                "DecodeParms",
                dictionary! {
                    "Predictor" => 2,
                    "Columns" => 1,
                    "Colors" => i64::try_from(colors).unwrap(),
                    "BitsPerComponent" => i64::try_from(component_bits).unwrap(),
                },
            );
            assert!(form_decode_policy(&stream).is_none());
            let mut session = open_session(&bytes);
            replace_test_form(&mut session, stream.clone(), false);
            let error = session.page_text(1).unwrap_err().to_string();
            assert!(
                error.contains("resource_limit: Form filter/predictor"),
                "{error}"
            );
            assert!(session.cache.forms.is_empty());
            assert_eq!(
                session.cache.last_form_work.unwrap().decode,
                MAX_PAGE_FORM_DECODE_BYTES
            );

            // Boundary check without actually allocating the accumulator.
            let params = stream
                .dict
                .get_mut(b"DecodeParms")
                .unwrap()
                .as_dict_mut()
                .unwrap();
            params.set(
                "Colors",
                i64::try_from(MAX_FORM_DECODE_BYTES / size_of::<u16>()).unwrap(),
            );
            assert!(form_decode_policy(&stream).is_some());
            stream
                .dict
                .get_mut(b"DecodeParms")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .set(
                    "Colors",
                    i64::try_from(MAX_FORM_DECODE_BYTES / size_of::<u16>() + 1).unwrap(),
                );
            assert!(form_decode_policy(&stream).is_none());
        }
    }

    /// Nine distinct indirect Forms force nine cache misses on one page.
    fn distinct_parameterized_forms(stream: &Stream) -> LopdfSession {
        let bytes = build_pdf(vec![vec![]], None);
        let mut session = open_session(&bytes);
        let mut xobjects = Dictionary::new();
        let mut operations = Vec::new();
        for index in 0..9 {
            let name = format!("X{index}");
            let mut form = stream.clone();
            form.dict.set("Subtype", "Form");
            let id = session.doc.add_object(form);
            xobjects.set(name.as_bytes(), id);
            operations.push(Operation::new("Do", vec![Object::Name(name.into_bytes())]));
        }
        let content = session.doc.add_object(Stream::new(
            dictionary! {},
            Content { operations }.encode().unwrap(),
        ));
        let page = session
            .doc
            .get_object_mut(session.pages[&1])
            .unwrap()
            .as_dict_mut()
            .unwrap();
        page.set("Contents", content);
        page.set("Resources", dictionary! { "XObject" => xobjects });
        session
    }

    fn flate_test_form(plain: &[u8]) -> Stream {
        use std::io::Write;
        // Stream::compress skips compression when the encoding would grow;
        // these tiny fixtures must still exercise the actual Flate decoder.
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(plain).unwrap();
        Stream::new(
            dictionary! { "Filter" => "FlateDecode" },
            encoder.finish().unwrap(),
        )
    }

    #[test]
    fn non_predictor_decode_parameters_refund_nine_distinct_forms() {
        let plain = b"q Q ";
        let mut flate = flate_test_form(plain);
        flate
            .dict
            .set("DecodeParms", dictionary! { "Predictor" => 1 });
        // MSB-first 9-bit LZW codes: clear, four literals, EOD. This tiny
        // fixture stays below the code-width transition for either EarlyChange.
        let codes = [256u16, 113, 32, 81, 32, 257];
        let mut encoded = vec![0u8; (codes.len() * 9).div_ceil(8)];
        for (index, code) in codes.into_iter().enumerate() {
            for bit in 0..9 {
                let offset = index * 9 + bit;
                encoded[offset / 8] |= (((code >> (8 - bit)) & 1) as u8) << (7 - offset % 8);
            }
        }
        let lzw = Stream::new(
            dictionary! {
                "Filter" => "LZWDecode",
                "DecodeParms" => dictionary! { "EarlyChange" => 0 },
            },
            encoded,
        );
        for stream in [flate, lzw] {
            assert_eq!(stream.get_plain_content_with_limit(1024).unwrap(), plain);
            let mut session = distinct_parameterized_forms(&stream);
            let page = session.page_text(1).unwrap();
            assert!(page.warnings.is_empty(), "{:?}", page.warnings);
            assert_eq!(session.cache.forms.len(), 9);
            assert_eq!(
                MAX_PAGE_FORM_DECODE_BYTES - session.cache.last_form_work.unwrap().decode,
                9 * plain.len().max(stream.content.len()),
            );
        }
    }

    #[test]
    fn enabled_predictors_and_chains_keep_the_worst_case_reservation() {
        let mut tiff = flate_test_form(b"q Q ");
        tiff.dict.set(
            "DecodeParms",
            dictionary! {
                "Predictor" => 2, "Columns" => 1, "Colors" => 1, "BitsPerComponent" => 8,
            },
        );
        let mut png = flate_test_form(b"\0q Q ");
        png.dict.set(
            "DecodeParms",
            dictionary! { "Predictor" => 12, "Columns" => 4 },
        );
        let chain = Stream::new(
            dictionary! {
                "Filter" => vec![Object::Name(b"ASCIIHexDecode".to_vec()), Object::Name(b"ASCIIHexDecode".to_vec())],
            },
            b"3731323035313230>".to_vec(),
        );
        for stream in [tiff, png, chain] {
            assert_eq!(stream.get_plain_content_with_limit(1024).unwrap(), b"q Q ");
            let mut session = distinct_parameterized_forms(&stream);
            let error = session.page_text(1).unwrap_err().to_string();
            assert!(
                error.contains("resource_limit: Form decode byte budget"),
                "{error}"
            );
            assert_eq!(session.cache.last_form_work.unwrap().decode, 0);
        }
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

    /// The operator a kept [`OpKind`] stands for.
    fn operator_name(kind: OpKind) -> &'static str {
        match kind {
            OpKind::Save => "q",
            OpKind::Restore => "Q",
            OpKind::Concat => "cm",
            OpKind::BeginText => "BT",
            OpKind::Font => "Tf",
            OpKind::Move => "Td",
            OpKind::MoveSetLeading => "TD",
            OpKind::TextMatrix => "Tm",
            OpKind::NextLine => "T*",
            OpKind::Leading => "TL",
            OpKind::CharSpacing => "Tc",
            OpKind::WordSpacing => "Tw",
            OpKind::HorizontalScale => "Tz",
            OpKind::Rise => "Ts",
            OpKind::Show => "Tj",
            OpKind::NextLineShow => "'",
            OpKind::SpacingShow => "\"",
            OpKind::ShowArray => "TJ",
            OpKind::Invoke => "Do",
            OpKind::FillPath => "f",
            OpKind::StrokePath => "S",
        }
    }

    type OpList = Result<Vec<(String, Vec<Object>)>, String>;

    #[test]
    #[ignore = "requires TPE_CORPUS_CACHE containing the pinned public PDFs"]
    fn measure_corpus_form_work() {
        let root = std::env::var("TPE_CORPUS_CACHE").expect("set TPE_CORPUS_CACHE");
        let mut peaks = [0usize; 3];
        let mut peak_files = [String::new(), String::new(), String::new()];
        let mut documents = 0;
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path().join("paper.pdf");
            if !path.is_file() {
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            let mut session = open_session(&bytes);
            documents += 1;
            for page in 1..=session.pages.len() as u32 {
                session.page_text(page).unwrap();
                let used = session.cache.last_form_work.unwrap();
                for (index, value) in [
                    MAX_PAGE_FORM_CALLS - used.calls,
                    MAX_PAGE_FORM_DECODE_BYTES - used.decode,
                    MAX_PAGE_FORM_WORK_BYTES - used.execute,
                ]
                .into_iter()
                .enumerate()
                {
                    if value > peaks[index] {
                        peaks[index] = value;
                        peak_files[index] = format!("{} page {page}", path.display());
                    }
                }
            }
        }
        assert_eq!(documents, 70);
        eprintln!(
            "corpus Form peaks: calls={}, decode_bytes={}, execution_charge={}; locations={peak_files:?}",
            peaks[0], peaks[1], peaks[2]
        );
    }

    /// Three Form levels: A calls B N times, B calls C N times, C saves/restores N times.
    fn shallow_nested_forms_pdf(n: usize) -> Vec<u8> {
        let bytes = build_pdf(
            vec![vec![Operation::new("Do", vec!["X1".into()])]],
            Some(vec![]),
        );
        let mut session = open_session(&bytes);
        let form = |content, resources| {
            Stream::new(
                dictionary! {
                    "Type" => "XObject", "Subtype" => "Form",
                    "BBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                    "Resources" => resources,
                },
                content,
            )
        };
        let c = session
            .doc
            .add_object(form(b"q Q\n".repeat(n), dictionary! {}));
        let b = session.doc.add_object(form(
            b"/C Do\n".repeat(n),
            dictionary! {
                "XObject" => dictionary! { "C" => c },
            },
        ));
        let a = session
            .doc
            .objects
            .values_mut()
            .find_map(|obj| {
                let stream = obj.as_stream_mut().ok()?;
                (stream.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Form"))
                    .then_some(stream)
            })
            .unwrap();
        a.set_content(b"/B Do\n".repeat(n));
        a.dict.set(
            "Resources",
            dictionary! { "XObject" => dictionary! { "B" => b } },
        );
        let mut output = Vec::new();
        session.doc.save_to(&mut output).unwrap();
        output
    }

    #[test]
    fn shallow_nested_forms_charge_each_repeated_execution_and_fail_explicitly() {
        for n in [20, 40, 80] {
            let bytes = shallow_nested_forms_pdf(n);
            let mut session = open_session(&bytes);
            let page = session.page_text(1).unwrap();
            assert!(page.warnings.is_empty(), "{:?}", page.warnings);
            assert!(page.spans.is_empty());
            assert_eq!(session.cache.forms.len(), 3);
            let used = session.cache.last_form_work.unwrap();
            assert_eq!(MAX_PAGE_FORM_CALLS - used.calls, 1 + n + n * n);
            let mut expected_charge = 0;
            for (program, charge) in session.cache.forms.values() {
                let repetitions = match program.ops[0].kind {
                    OpKind::Save => n * n, // C: N^2 visits, N q/Q pairs each.
                    OpKind::Invoke if program.operands[0].as_name().unwrap() == b"C" => n,
                    OpKind::Invoke => 1,
                    _ => panic!("unexpected nested Form program"),
                };
                expected_charge += charge * repetitions;
            }
            assert_eq!(MAX_PAGE_FORM_WORK_BYTES - used.execute, expected_charge);
        }
        let mut session = open_session(&shallow_nested_forms_pdf(160));
        let error = session.page_text(1).unwrap_err().to_string();
        assert!(
            error.contains("resource_limit: Form execution byte budget"),
            "{error}"
        );
        assert_eq!(session.cache.forms.len(), 3);
        assert!(
            session.cache.last_form_work.unwrap().calls > 0,
            "work cap fires before call cap"
        );
    }

    /// Run each N in a separate process to measure RSS without prior test peaks.
    #[test]
    #[ignore = "manual nested-Forms timing/RSS diagnostic; set TPE_FORM_DIAGNOSTIC_N"]
    fn measure_shallow_nested_forms() {
        let n = std::env::var("TPE_FORM_DIAGNOSTIC_N")
            .unwrap_or_else(|_| "80".into())
            .parse::<usize>()
            .unwrap();
        let bytes = shallow_nested_forms_pdf(n);
        let mut session = open_session(&bytes);
        let start = std::time::Instant::now();
        let result = session.page_text(1);
        let elapsed = start.elapsed();
        let used = session.cache.last_form_work.unwrap();
        eprintln!(
            "N={n}, PDF bytes={}, uncapped q/Q pairs={}, elapsed={elapsed:?}, calls={}, execution_charge={}, result={}",
            bytes.len(),
            n.pow(3),
            MAX_PAGE_FORM_CALLS - used.calls,
            MAX_PAGE_FORM_WORK_BYTES - used.execute,
            result.map_or_else(|err| err.to_string(), |_| "ok".into())
        );
    }

    /// Manual diagnostic; no flaky wall-clock threshold in the test suite.
    #[test]
    #[ignore = "run in release mode with --nocapture for a cache reuse diagnostic"]
    fn measure_form_cache_reuse() {
        let mut form = Vec::new();
        for _ in 0..100 {
            form.extend(text_ops(10, 50, 50, "Reusable Form text"));
        }
        let bytes = build_pdf(
            vec![vec![Operation::new("Do", vec!["X1".into()])]],
            Some(form),
        );
        let mut cached = open_session(&bytes);
        let mut cold = open_session(&bytes);
        let expected = cached.page_text(1).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            assert_eq!(cached.page_text(1).unwrap(), expected);
        }
        let retained = start.elapsed();
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            cold.cache.forms.clear();
            cold.cache.form_order.clear();
            cold.cache.form_bytes = 0;
            assert_eq!(cold.page_text(1).unwrap(), expected);
        }
        eprintln!(
            "1000 repeated pages, retained Form cache: {retained:?}; no persistent Form cache: {:?}",
            start.elapsed()
        );
    }

    /// The kept operators and their operands, from the streaming lexer
    /// (painted paths left out).
    fn lexed(bytes: &[u8]) -> OpList {
        let program = lex_content(bytes).map_err(|err| format!("{err}"))?;
        let ops = program
            .ops
            .iter()
            .filter(|op| !op.kind.is_path())
            .map(|&op| {
                let name = operator_name(op.kind).to_string();
                (name, program.operands(op).to_vec())
            })
            .collect();
        Ok(ops)
    }

    /// The same, from `lopdf`'s full `Content::decode`.
    fn decoded(bytes: &[u8]) -> OpList {
        let content = Content::decode(bytes).map_err(|err| format!("{err}"))?;
        let ops = content
            .operations
            .into_iter()
            .filter(|op| OpKind::from_operator(op.operator.as_bytes()).is_some())
            .map(|op| (op.operator, op.operands))
            .collect();
        Ok(ops)
    }

    fn assert_same_as_lopdf(bytes: &[u8]) {
        let expected = decoded(bytes);
        let actual = lexed(bytes);
        assert_eq!(
            actual,
            expected,
            "stream: {}",
            String::from_utf8_lossy(bytes)
        );
    }

    /// `(operator, operands)` for a test expectation.
    fn op(name: &str, operands: Vec<Object>) -> (String, Vec<Object>) {
        (name.to_string(), operands)
    }

    /// Path construction, painting, clipping and colour operators only:
    /// `rounds` × 10 operators.
    fn vector_ops(rounds: i32) -> Vec<Operation> {
        let mut ops = Vec::new();
        for index in 0..rounds {
            let x = index % 500;
            ops.push(Operation::new("m", vec![x.into(), 10.into()]));
            ops.push(Operation::new("l", vec![(x + 5).into(), 20.into()]));
            let curve = vec![
                1.into(),
                2.into(),
                Object::Real(3.5),
                4.into(),
                5.into(),
                6.into(),
            ];
            ops.push(Operation::new("c", curve));
            ops.push(Operation::new("h", vec![]));
            ops.push(Operation::new("S", vec![]));
            let rect = vec![x.into(), 0.into(), 10.into(), 10.into()];
            ops.push(Operation::new("re", rect));
            ops.push(Operation::new("W", vec![]));
            ops.push(Operation::new("n", vec![]));
            ops.push(Operation::new(
                "rg",
                vec![Object::Real(0.5), 0.into(), 1.into()],
            ));
            ops.push(Operation::new("f*", vec![]));
        }
        ops
    }

    #[test]
    fn lexer_matches_content_decode() {
        let deep_ok = format!("BT {}{} TJ ET", "[".repeat(100), "]".repeat(100));
        let deep_bad = format!("BT {}{} TJ ET", "[".repeat(101), "]".repeat(101));
        let parens_ok = format!("BT ({}{}) Tj ET", "(".repeat(100), ")".repeat(100));
        let parens_bad = format!("BT ({}{}) Tj (x) Tj ET", "(".repeat(101), ")".repeat(101));
        let cases: Vec<&[u8]> = vec![
            &b""[..],
            b"BT /F1 12 Tf 72 700 Td (Hello) Tj ET",
            b"% leading\nq 1 0 0 1 5 5 cm % after cm\n BT /F1 9 Tf (a) Tj ET\n%end\n",
            b"BT 1 0 % mid-operands comment\n 0 1 0 0 Tm (x) Tj ET",
            b"BT (a) Tj ET % no end of line",
            br"BT (a\(b\)c) Tj (nest (inner (deep)) x) Tj (oct \053\5\1234\777) Tj ET",
            br"BT (esc \n\r\t\b\f\\\q\)) Tj ET",
            b"BT (a\\\r\nb) Tj (c\\\nd) Tj (e\r\nf) Tj (g\rh) Tj ET",
            b"BT (trailing backslash\\",
            b"BT <48 65 6c6C 6f> Tj <414> Tj <> Tj < 4\x001 > Tj <4G> Tj (after) Tj ET",
            b"BT -.5 6. Td +3 .5 TD 1.2.3 Tc 12Tz -0 Ts 0 0 d0 BT ET",
            b"BT null true false Tj nullTf falseTw ET",
            b"BT /A#20B 12 Tf /#46#31 1 Tf ET /C#2 3 Tf (lost) Tj",
            b"BT [(a) -120 (b) 1 0 R (c) 2.5 [(n)] << /K 1 >> % c\n (d)] TJ ET",
            b"BT [(a)1 0R(b)-7.25<41>] TJ [ ] TJ [(x) 99999999999 0 R] TJ ET",
            b"/Span << /ActualText (a\\)b) /MCID 3 /Sub <</X [1 2]>> >> BDC BT (x) Tj ET EMC",
            b"/P <</A /B /C>> BDC BT (lost) Tj ET",
            b"q 1 0 0 1 10 10 cm BT (x) Tj (corrupted Q",
            b"BT (a) Tj 99999999999999999999 Tc (b) Tj ET",
            b"BT (a) Tj 1 2",
            b"BT (a) Tj\x0C(b) Tj ET",
            b"\0BT (a) Tj ET",
            b"BT 14 TL (a) ' 1 2 (b) \" 3 T* ET",
            b"0.5 g 1 0 0 RG [3 2] 0 d /GS1 gs 2 w 1 J 0 j 4 M BT /F1 1 Tf (x) Tj ET",
            b"q 100 0 0 50 0 0 cm /Im1 Do Q BT 1 0 R Tf ET",
            b"BT(a)Tj[(b)]TJ/F1 9 Tf<<>>BDC(c)Tj ET",
            b"q BI /W 2 /H 1 /BPC 8 /CS /RGB ID a EI ) EI Q BT (after) Tj ET",
            b"q BI /Width 2 /Height 1 /BitsPerComponent 8 /ImageMask true ID ab EI Q",
            b"BI /W 2 /H 1 /BPC 8 /CS /G /F /AHx ID 0a0b EI BT (after) Tj ET",
            b"BI /W 1 /H 1 /BPC 8 /CS /Indexed ID x EI BT (after) Tj ET",
            b"BT (a) Tj ET BI /W 1 /H 1 EI",
            b"BI /W 1 /H 1 /BPC 8 /CS /G /F /DCT ID xyz",
            b"BI /W 1 /H 1 /BPC 8 /CS /G ID x Q",
            deep_ok.as_bytes(),
            deep_bad.as_bytes(),
            parens_ok.as_bytes(),
            parens_bad.as_bytes(),
        ];
        for bytes in cases {
            assert_same_as_lopdf(bytes);
        }
        // Operators `lopdf` writes itself round-trip too.
        let mut ops = vector_ops(1_000);
        ops.extend(text_ops(12, 100, 600, "Middle"));
        ops.extend(vector_ops(1_000));
        let encoded = Content { operations: ops }.encode().unwrap();
        assert_same_as_lopdf(&encoded);
    }

    #[test]
    fn lexer_reads_strings_numbers_and_arrays_exactly() {
        let bytes = br"BT -.5 6. Td (a (b) \(c\) \101\n) Tj [(x) -250 (y) 12.5] TJ ET";
        let expected = vec![
            op("BT", vec![]),
            op("Td", vec![Object::Real(-0.5), Object::Real(6.0)]),
            op(
                "Tj",
                vec![Object::string_literal(b"a (b) (c) A\n".to_vec())],
            ),
            op(
                "TJ",
                vec![Object::Array(vec![
                    Object::string_literal("x"),
                    Object::Integer(-250),
                    Object::string_literal("y"),
                    Object::Real(12.5),
                ])],
            ),
        ];
        assert_eq!(lexed(bytes), Ok(expected));
        let hex = lexed(b"<48 65 6>Tj").unwrap();
        let hex_string = Object::String(b"He`".to_vec(), StringFormat::Hexadecimal);
        assert_eq!(hex, vec![op("Tj", vec![hex_string])]);
    }

    #[test]
    fn inline_image_data_containing_ei_is_skipped() {
        // 2 × 1 RGB pixels: exactly 6 data bytes, `a EI )`. Scanning for the
        // first ` EI ` would resume inside the data and stop at `)`.
        let bytes = b"q BI /W 2 /H 1 /BPC 8 /CS /RGB ID a EI ) EI Q BT (after) Tj ET";
        let expected = vec![
            op("q", vec![]),
            op("Q", vec![]),
            op("BT", vec![]),
            op("Tj", vec![Object::string_literal("after")]),
        ];
        assert_eq!(lexed(bytes), Ok(expected));
        // Filtered data has no computable length: it ends at ` EI `.
        let filtered = b"BI /W 9 /H 9 /BPC 8 /CS /G /F /Fl ID \x01EI\x02 EI BT (b) Tj ET";
        let expected = vec![
            op("BT", vec![]),
            op("Tj", vec![Object::string_literal("b")]),
        ];
        assert_eq!(lexed(filtered), Ok(expected));
        // No `ID` at all: `lopdf` rejects the whole stream.
        assert!(lexed(b"BT (a) Tj ET BI /W 1 EI").is_err());
        // An `EI` that ends the stream: `lopdf` rejects it, the lexer keeps
        // the text before the image.
        let cut = b"BT (a) Tj ET BI /W 9 /H 9 /BPC 8 /CS /G /F /Fl ID \x01\x02 EI";
        assert!(decoded(cut).is_err());
        let expected = vec![
            op("BT", vec![]),
            op("Tj", vec![Object::string_literal("a")]),
        ];
        assert_eq!(lexed(cut), Ok(expected));
        // Without white space before that `EI` it is still data.
        assert!(lexed(b"BT (a) Tj ET BI /W 9 /H 9 /BPC 8 /CS /G /F /Fl ID \x01EI").is_err());
    }

    #[test]
    fn painted_paths_become_figures_without_changing_spans() {
        let text = text_ops(12, 100, 600, "Only text");
        let mut busy = vec![Operation::new("q", vec![])];
        busy.extend(vector_ops(500));
        busy.push(Operation::new("Q", vec![]));
        busy.extend(text.clone());
        busy.extend(vector_ops(500));
        let bytes = build_pdf(vec![text, busy], None);
        let mut session = open_session(&bytes);
        let plain = session.page_text(1).unwrap();
        let busy_page = session.page_text(2).unwrap();
        assert_eq!(plain.spans.len(), 1);
        assert_eq!(busy_page.spans, plain.spans);
        assert_eq!(busy_page.warnings, plain.warnings);

        let content = session.doc.get_page_content(session.pages[&2]);
        let program = lex_content(&content).unwrap();
        let kinds: Vec<OpKind> = program.ops.iter().map(|op| op.kind).collect();
        // Each round strokes one path (`m l c h S`); `re W n` is discarded
        // and the `f*` after it has no path to paint.
        let mut expected = vec![OpKind::Save];
        expected.extend(std::iter::repeat_n(OpKind::StrokePath, 500));
        expected.extend([
            OpKind::Restore,
            OpKind::BeginText,
            OpKind::Font,
            OpKind::Move,
            OpKind::Show,
        ]);
        expected.extend(std::iter::repeat_n(OpKind::StrokePath, 500));
        assert_eq!(kinds, expected);
        assert_eq!(program.paths.len(), 1_000);
        assert!(program.operands(program.ops[1]).is_empty());

        // Every stroked box spans x 1..5, so all 1000 form one figure.
        assert!(plain.figures.is_empty());
        assert_eq!(busy_page.figures.len(), 1, "{:?}", busy_page.figures);
        let figure = &busy_page.figures[0];
        assert_eq!(figure.kind, "vector");
        assert_box(figure, 0.0, 2.0, 504.0, 20.0);
    }

    #[test]
    fn pure_vector_form_is_cached_with_its_paths_and_mixed_form_still_recurses() {
        let page = vec![
            Operation::new("q", vec![]),
            cm_translate(200, 300),
            Operation::new("Do", vec!["X1".into()]),
            Operation::new("Q", vec![]),
        ];
        let figure = build_pdf(vec![page.clone()], Some(vector_ops(200)));
        let mut session = open_session(&figure);
        let result = session.page_text(1).unwrap();
        assert!(result.spans.is_empty());
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(session.cache.forms.len(), 1);
        let cached = session.cache.forms.values().next().unwrap();
        assert_eq!(cached.0.ops.len(), 200);
        assert!(cached.0.ops.iter().all(|op| op.kind == OpKind::StrokePath));
        // The form's boxes (0,2)-(204,20) moved by the page's `cm`.
        assert_eq!(result.figures.len(), 1, "{:?}", result.figures);
        assert_box(&result.figures[0], 200.0, 302.0, 404.0, 320.0);
        let again = session.page_text(1).unwrap();
        assert_eq!(again.figures, result.figures);

        let mut mixed = vector_ops(200);
        mixed.extend(text_ops(10, 50, 50, "Form"));
        mixed.extend(vector_ops(200));
        let bytes = build_pdf(vec![page], Some(mixed));
        let mut session = open_session(&bytes);
        let result = session.page_text(1).unwrap();
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(result.spans.len(), 1);
        assert_eq!(result.spans[0].text, "Form");
        let form_box = result.spans[0].bbox.unwrap();
        assert!(close(form_box.x0, 250.0), "x0 {}", form_box.x0);
        assert!(close(form_box.y0, 348.0), "y0 {}", form_box.y0);
    }

    #[test]
    fn form_cannot_pop_the_callers_graphics_state() {
        let page = vec![
            Operation::new("q", vec![]),
            cm_translate(200, 300),
            Operation::new("Do", vec!["X1".into()]),
            Operation::new("Q", vec![]),
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 10.into()]),
            Operation::new("Td", vec![50.into(), 50.into()]),
            Operation::new("Tj", vec![Object::string_literal("After")]),
            Operation::new("ET", vec![]),
        ];
        let form = vec![
            Operation::new("Q", vec![]),
            Operation::new("Q", vec![]),
            Operation::new("BT", vec![]),
            Operation::new("ET", vec![]),
        ];
        let bytes = build_pdf(vec![page], Some(form));
        let mut session = LopdfBackend::default().open(&bytes, None).unwrap();
        let result = session.page_text(1).unwrap();
        assert_eq!(result.spans.len(), 1);
        // The page's own `Q` still restores the identity CTM.
        let after_box = result.spans[0].bbox.unwrap();
        assert!(close(after_box.x0, 50.0), "x0 {}", after_box.x0);
    }

    /// `figure`'s box is `(x0, y0)`-`(x1, y1)` and it carries no bytes.
    fn assert_box(figure: &Figure, x0: f32, y0: f32, x1: f32, y1: f32) {
        let bbox = figure.bbox.unwrap();
        let ok =
            close(bbox.x0, x0) && close(bbox.y0, y0) && close(bbox.x1, x1) && close(bbox.y1, y1);
        assert!(ok, "{figure:?}");
        assert_eq!(figure.mime, None);
        assert_eq!(figure.sha256, None);
        assert_eq!(figure.file, None);
    }

    fn numbers(values: &[f32]) -> Vec<Object> {
        values.iter().map(|&value| Object::Real(value)).collect()
    }

    /// `x y width height re` then `paint`.
    fn rect_ops(x: f32, y: f32, width: f32, height: f32, paint: &str) -> Vec<Operation> {
        vec![
            Operation::new("re", numbers(&[x, y, width, height])),
            Operation::new(paint, vec![]),
        ]
    }

    #[test]
    fn lexer_folds_painted_paths_into_boxes() {
        let bytes = b"0 0 m 10 5 l S 1 1 2 2 re W n f 0 0 20 30 re 40 -2 m B* \
            10 10 -5 -5 re f (a) 1 m 3 3 m 4 4 l s 9 9 m 1 2 3 4 5 6 c 7 8 9 10 v 0 1 2 3 y b 7 7 m";
        let program = lex_content(bytes).unwrap();
        let kinds: Vec<OpKind> = program.ops.iter().map(|op| op.kind).collect();
        assert_eq!(
            kinds,
            vec![
                OpKind::StrokePath,
                OpKind::FillPath,
                OpKind::FillPath,
                OpKind::StrokePath,
                OpKind::FillPath,
            ]
        );
        let boxes: Vec<[f32; 4]> = program
            .ops
            .iter()
            .map(|&op| program.path_box(op).unwrap())
            .collect();
        assert_eq!(
            boxes,
            vec![
                [0.0, 0.0, 10.0, 5.0],
                [0.0, -2.0, 40.0, 30.0],
                [5.0, 5.0, 10.0, 10.0],
                [3.0, 3.0, 4.0, 4.0],
                [0.0, 1.0, 9.0, 10.0],
            ]
        );
        assert!(program.operands.is_empty());
    }

    #[test]
    fn nearby_filled_rectangles_are_one_vector_figure_and_text_is_unchanged() {
        let text = text_ops(12, 100, 600, "Caption");
        let mut drawn = rect_ops(100.0, 100.0, 50.0, 50.0, "f");
        drawn.extend(text.clone());
        drawn.extend(rect_ops(153.0, 100.0, 40.0, 50.0, "f"));
        // Far away and too small on its own: dropped.
        drawn.extend(rect_ops(400.0, 400.0, 5.0, 5.0, "f"));
        // Clipping only: never a figure.
        drawn.push(Operation::new("re", numbers(&[0.0, 0.0, 612.0, 792.0])));
        drawn.push(Operation::new("W", vec![]));
        drawn.push(Operation::new("n", vec![]));
        let bytes = build_pdf(vec![text, drawn], None);
        let mut session = LopdfBackend::default().open(&bytes, None).unwrap();
        let plain = session.page_text(1).unwrap();
        let page = session.page_text(2).unwrap();
        assert_eq!(page.spans, plain.spans);
        assert_eq!(page.warnings, plain.warnings);
        assert_eq!(page.figures.len(), 1, "{:?}", page.figures);
        let figure = &page.figures[0];
        assert_eq!(figure.index, 0);
        assert_eq!(figure.kind, "vector");
        assert_box(figure, 100.0, 100.0, 193.0, 150.0);
    }

    #[test]
    fn thin_rectangles_and_lines_are_rules() {
        let mut ops = rect_ops(72.0, 400.0, 200.0, 0.5, "f");
        // A vertical stroked line.
        ops.push(Operation::new("m", numbers(&[300.0, 100.0])));
        ops.push(Operation::new("l", numbers(&[300.0, 200.0])));
        ops.push(Operation::new("S", vec![]));
        // Too short for a rule and too small for a figure.
        ops.extend(rect_ops(72.0, 300.0, 5.0, 0.5, "f"));
        // A box far from both, emitted after the rules.
        ops.extend(rect_ops(400.0, 600.0, 100.0, 80.0, "B"));
        let bytes = build_pdf(vec![ops], None);
        let mut session = LopdfBackend::default().open(&bytes, None).unwrap();
        let page = session.page_text(1).unwrap();
        let kinds: Vec<&str> = page.figures.iter().map(|f| f.kind.as_str()).collect();
        assert_eq!(kinds, vec!["rule", "rule", "vector"], "{:?}", page.figures);
        let indexes: Vec<u32> = page.figures.iter().map(|f| f.index).collect();
        assert_eq!(indexes, vec![0, 1, 2]);
        assert_box(&page.figures[0], 72.0, 400.0, 272.0, 400.5);
        assert_box(&page.figures[1], 300.0, 100.0, 300.0, 200.0);
        assert_box(&page.figures[2], 400.0, 600.0, 500.0, 680.0);
    }

    #[test]
    fn form_paths_are_placed_through_cm() {
        let page = vec![
            Operation::new("q", vec![]),
            cm_translate(200, 300),
            Operation::new("Do", vec!["X1".into()]),
            Operation::new("Q", vec![]),
        ];
        let form = rect_ops(10.0, 20.0, 50.0, 40.0, "f");
        let bytes = build_pdf(vec![page], Some(form));
        let mut session = LopdfBackend::default().open(&bytes, None).unwrap();
        let result = session.page_text(1).unwrap();
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(result.figures.len(), 1, "{:?}", result.figures);
        assert_eq!(result.figures[0].kind, "vector");
        assert_box(&result.figures[0], 210.0, 320.0, 260.0, 360.0);
    }

    /// One page drawing `ops` with a 4 × 2 gray Image `XObject` `/Im1`.
    fn build_image_pdf(ops: Vec<Operation>) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let tree_id = doc.new_object_id();
        let image_dict = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => 4,
            "Height" => 2,
            "BitsPerComponent" => 8,
            "ColorSpace" => "DeviceGray",
        };
        let image_id = doc.add_object(Stream::new(image_dict, vec![0x80; 8]));
        let content = Content { operations: ops }.encode().unwrap();
        let content_id = doc.add_object(Stream::new(dictionary! {}, content));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => tree_id,
            "Contents" => content_id,
            "Resources" => dictionary! { "XObject" => dictionary! { "Im1" => image_id } },
        });
        let tree = dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        };
        doc.objects.insert(tree_id, Object::Dictionary(tree));
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => tree_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn image_xobject_is_a_raster_figure_under_the_ctm() {
        let placement = vec![
            100.into(),
            0.into(),
            0.into(),
            50.into(),
            72.into(),
            700.into(),
        ];
        let ops = vec![
            Operation::new("q", vec![]),
            Operation::new("cm", placement),
            Operation::new("Do", vec!["Im1".into()]),
            Operation::new("Q", vec![]),
        ];
        let bytes = build_image_pdf(ops);
        let mut session = LopdfBackend::default().open(&bytes, None).unwrap();
        let page = session.page_text(1).unwrap();
        assert!(page.warnings.is_empty(), "{:?}", page.warnings);
        assert!(page.spans.is_empty());
        assert_eq!(page.figures.len(), 1, "{:?}", page.figures);
        let figure = &page.figures[0];
        assert_eq!(figure.index, 0);
        assert_eq!(figure.kind, "raster");
        assert_eq!(figure.width_px, Some(4));
        assert_eq!(figure.height_px, Some(2));
        assert_box(figure, 72.0, 700.0, 172.0, 750.0);
    }

    #[test]
    fn clusters_merge_to_a_fixed_point_and_overflow_to_one_union() {
        let boxed = |x0: f32, y0: f32, x1: f32, y1: f32| BBox { x0, y0, x1, y1 };
        // The third box bridges the first two only once they are merged.
        let clusters = cluster(&[
            boxed(0.0, 0.0, 10.0, 10.0),
            boxed(30.0, 0.0, 40.0, 10.0),
            boxed(14.0, 0.0, 26.0, 10.0),
            boxed(0.0, 100.0, 10.0, 110.0),
        ]);
        assert_eq!(
            clusters,
            vec![boxed(0.0, 100.0, 10.0, 110.0), boxed(0.0, 0.0, 40.0, 10.0)]
        );

        let mut graphics = Graphics::default();
        for step in 0..=MAX_CLUSTER_BOXES {
            let x = (step * 20) as f32;
            graphics.add_path(boxed(x, 0.0, x + 10.0, 10.0));
        }
        let figures = graphics.into_figures();
        assert_eq!(figures.len(), 1);
        assert_eq!(figures[0].kind, "vector");
        assert_box(&figures[0], 0.0, 0.0, 40_010.0, 10.0);
    }

    #[test]
    fn raster_placements_are_bounded() {
        let mut graphics = Graphics::default();
        for step in 0..=MAX_CLUSTER_BOXES {
            graphics.add_raster(Raster {
                bbox: BBox {
                    x0: step as f32,
                    y0: 0.0,
                    x1: step as f32 + 1.0,
                    y1: 1.0,
                },
                width_px: Some(1),
                height_px: Some(1),
            });
        }
        let figures = graphics.into_figures();
        assert_eq!(figures.len(), MAX_CLUSTER_BOXES);
        assert!(figures.iter().all(|figure| figure.kind == "raster"));
    }

    #[test]
    fn composite_widths_lookup_matches_first_match_scan() {
        let ranges = vec![
            (10, 10, 300.0),
            (1, 3, 100.0),
            (20, 25, 700.0),
            (5, 4, 999.0),
        ];
        let widths = CompositeWidths::new(ranges, 1000.0);
        assert!(widths.disjoint);
        assert!(close(widths.width(2), 0.1));
        assert!(close(widths.width(10), 0.3));
        assert!(close(widths.width(25), 0.7));
        assert!(close(widths.width(0), 1.0));
        assert!(close(widths.width(4), 1.0));
        assert!(close(widths.width(11), 1.0));
        assert!(close(widths.width(26), 1.0));

        let overlapping = CompositeWidths::new(vec![(1, 10, 100.0), (5, 5, 900.0)], 500.0);
        assert!(!overlapping.disjoint);
        assert!(close(overlapping.width(5), 0.1));
        assert!(close(overlapping.width(11), 0.5));
    }
}

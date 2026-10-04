//! Optional, explicitly configured native providers. The ABI is in
//! `native/provider.h`; engine-specific exception handling stays in C/C++.
//! CLI calls execute inside the existing disposable worker. Providers never
//! spawn children. Library callers must supply their own process boundary.
//!
//! Provider/runtime files are trusted deployment code, not PDF inputs. Both
//! absolute files are bounded and fingerprinted before loading; their bytes
//! must remain unchanged during a job. Transitive system dependencies are not
//! snapshotted. No default loader search or current-directory lookup is used.

use std::collections::BTreeMap;
use std::ffi::{CString, c_char, c_int, c_void};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use libloading::Library;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use super::{BackendError, DocumentSession, EncryptionProblem, Extractor};
use crate::schema::{BBox, BackendIdentity, Figure, Link, PageText, Span, config_digest};

const ABI: u32 = 1;
const MAX_LIBRARY: u64 = 256 * 1024 * 1024;
const MAX_OUTPUT: usize = 16 * 1024 * 1024;
const MAX_CHARACTERS: usize = 1_000_000;

/// Only these built-in engine specifications can select a provider.
#[derive(Clone, Copy, Debug)]
pub enum Engine {
    #[cfg(feature = "mupdf")]
    MuPdf,
    #[cfg(feature = "poppler")]
    Poppler,
}
impl Engine {
    fn names(self) -> (&'static str, &'static str, &'static str) {
        match self {
            #[cfg(feature = "mupdf")]
            Self::MuPdf => ("mupdf", "TPE_MUPDF_PROVIDER_PATH", "MUPDF_DYNAMIC_LIB_PATH"),
            #[cfg(feature = "poppler")]
            Self::Poppler => (
                "poppler",
                "TPE_POPPLER_PROVIDER_PATH",
                "POPPLER_DYNAMIC_LIB_PATH",
            ),
        }
    }
}

#[derive(Clone, Debug)]
struct Configuration {
    provider: PathBuf,
    runtime: PathBuf,
    provider_sha: String,
    runtime_sha: String,
}
impl Configuration {
    fn read(engine: Engine) -> Result<Self, String> {
        let (_, provider, runtime) = engine.names();
        let path = |name| -> Result<PathBuf, String> {
            let value = std::env::var_os(name)
                .ok_or_else(|| format!("set {name} to an absolute library file"))?;
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(format!("{name} must name an absolute library file"));
            }
            path.canonicalize().map_err(|e| format!("{name}: {e}"))
        };
        let provider = path(provider)?;
        let runtime = path(runtime)?;
        Ok(Self {
            provider_sha: fingerprint(&provider)?,
            runtime_sha: fingerprint(&runtime)?,
            provider,
            runtime,
        })
    }
    fn verify(&self) -> Result<(), String> {
        if fingerprint(&self.provider)? != self.provider_sha
            || fingerprint(&self.runtime)? != self.runtime_sha
        {
            return Err(
                "configured native provider/runtime changed after fingerprinting".to_string(),
            );
        }
        Ok(())
    }
}

fn fingerprint(path: &Path) -> Result<String, String> {
    let metadata = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_LIBRARY {
        return Err("native library must be a regular file no larger than 256 MiB".to_string());
    }
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut digest = Sha256::new();
    let mut bytes = [0; 16 * 1024];
    let mut length = 0_u64;
    loop {
        let count = file.read(&mut bytes).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        length += count as u64;
        if length > MAX_LIBRARY {
            return Err("native library exceeded 256 MiB while hashing".to_string());
        }
        digest.update(&bytes[..count]);
    }
    Ok(hex::encode(digest.finalize()))
}

/// The same safe Rust adapter is used for every optional C ABI provider.
#[derive(Clone, Debug)]
pub struct NativeProviderBackend {
    engine: Engine,
    configuration: Result<Configuration, String>,
}
impl NativeProviderBackend {
    pub fn new(engine: Engine) -> Self {
        Self {
            engine,
            configuration: Configuration::read(engine),
        }
    }
}
impl Extractor for NativeProviderBackend {
    fn identity(&self) -> BackendIdentity {
        let mut config = BTreeMap::new();
        config.insert("abi".to_string(), ABI.to_string());
        config.insert("page_output_limit".to_string(), MAX_OUTPUT.to_string());
        match &self.configuration {
            Ok(c) => {
                config.insert(
                    "provider_path".to_string(),
                    c.provider.to_string_lossy().into_owned(),
                );
                config.insert(
                    "runtime_path".to_string(),
                    c.runtime.to_string_lossy().into_owned(),
                );
                config.insert("provider_sha256".to_string(), c.provider_sha.clone());
                config.insert("runtime_sha256".to_string(), c.runtime_sha.clone());
            }
            Err(e) => {
                config.insert("configuration_error".to_string(), e.clone());
            }
        }
        BackendIdentity {
            name: self.engine.names().0.to_string(),
            version: "native-provider-abi-1".to_string(),
            config_digest: config_digest(&config),
        }
    }
    fn open(
        &self,
        bytes: &[u8],
        password: Option<&str>,
    ) -> Result<Box<dyn DocumentSession>, BackendError> {
        let config = self
            .configuration
            .as_ref()
            .map_err(|e| BackendError::Unsupported(e.clone()))?;
        let api =
            Arc::new(Api::load(config, self.engine.names().0).map_err(BackendError::Unsupported)?);
        let bytes = bytes.to_vec().into_boxed_slice();
        let password = password
            .map(CString::new)
            .transpose()
            .map_err(|_| BackendError::Unsupported("password contains NUL".to_string()))?;
        let mut handle = std::ptr::null_mut();
        let mut pages = 0;
        let mut error = [0; 512];
        // SAFETY: ABI-checked trusted provider; owners retain input/API until close.
        let status = unsafe {
            (api.open)(
                bytes.as_ptr(),
                bytes.len(),
                password.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()),
                &raw mut handle,
                &raw mut pages,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if status != 0 || handle.is_null() || pages == 0 {
            if !handle.is_null() {
                unsafe {
                    (api.close)(handle);
                }
            }
            return Err(match status {
                2 => BackendError::Encrypted(EncryptionProblem::PasswordRequired),
                3 => BackendError::Encrypted(EncryptionProblem::WrongPassword),
                _ => BackendError::Malformed(format!(
                    "{} provider open: {}",
                    self.engine.names().0,
                    error_message(&error)
                )),
            });
        }
        Ok(Box::new(Session {
            api,
            handle,
            _bytes: bytes,
            pages,
        }))
    }
}

type Open = unsafe extern "C" fn(
    *const u8,
    usize,
    *const c_char,
    *mut *mut c_void,
    *mut u32,
    *mut c_char,
    usize,
) -> c_int;
type Page = unsafe extern "C" fn(
    *mut c_void,
    u32,
    usize,
    *mut *mut u8,
    *mut usize,
    *mut c_char,
    usize,
) -> c_int;
type Close = unsafe extern "C" fn(*mut c_void);
type Free = unsafe extern "C" fn(*mut u8);
struct Api {
    // Function pointers are used only while both libraries remain loaded.
    _provider: Library,
    _runtime: Library,
    open: Open,
    page: Page,
    close: Close,
    free: Free,
    engine: String,
    version: String,
}
impl Api {
    fn load(config: &Configuration, expected_engine: &str) -> Result<Self, String> {
        config.verify()?;
        // SAFETY: explicit, fingerprinted trusted deployment files. The provider
        // contract forbids exceptions crossing ABI and requires thread-safe
        // independent sessions. Native faults are contained by the CLI worker.
        unsafe {
            let runtime = Library::new(&config.runtime).map_err(|e| e.to_string())?;
            let provider = Library::new(&config.provider).map_err(|e| e.to_string())?;
            let abi = provider
                .get::<unsafe extern "C" fn() -> u32>(b"tpe_pdf_provider_abi_version\0")
                .map_err(|e| e.to_string())?;
            if abi() != ABI {
                return Err("unsupported native provider ABI".to_string());
            }
            let string = |name: &[u8]| -> Result<String, String> {
                let function = provider
                    .get::<unsafe extern "C" fn() -> *const c_char>(name)
                    .map_err(|e| e.to_string())?;
                bounded_string(function())
            };
            let engine = string(b"tpe_pdf_provider_engine\0")?;
            let version = string(b"tpe_pdf_provider_version\0")?;
            if engine != expected_engine
                || version.is_empty()
                || !version.bytes().all(|b| b.is_ascii_graphic())
            {
                return Err("native provider engine/version mismatch".to_string());
            }
            let anchor_symbol = CString::new(string(b"tpe_pdf_provider_runtime_symbol\0")?)
                .map_err(|e| e.to_string())?;
            let runtime_anchor = runtime
                .get::<*const c_void>(anchor_symbol.as_bytes_with_nul())
                .map_err(|e| e.to_string())?;
            let anchor = provider
                .get::<unsafe extern "C" fn() -> usize>(b"tpe_pdf_provider_runtime_anchor\0")
                .map_err(|e| e.to_string())?;
            if anchor() != *runtime_anchor as usize {
                return Err("provider does not use the fingerprinted native runtime".to_string());
            }
            let open = *provider
                .get::<Open>(b"tpe_pdf_provider_open\0")
                .map_err(|e| e.to_string())?;
            let page = *provider
                .get::<Page>(b"tpe_pdf_provider_page\0")
                .map_err(|e| e.to_string())?;
            let close = *provider
                .get::<Close>(b"tpe_pdf_provider_close\0")
                .map_err(|e| e.to_string())?;
            let free = *provider
                .get::<Free>(b"tpe_pdf_provider_free\0")
                .map_err(|e| e.to_string())?;
            config.verify()?;
            Ok(Self {
                _provider: provider,
                _runtime: runtime,
                open,
                page,
                close,
                free,
                engine,
                version,
            })
        }
    }
}

// SAFETY requirement: provider string is readable through its NUL, max 255 bytes.
unsafe fn bounded_string(pointer: *const c_char) -> Result<String, String> {
    if pointer.is_null() {
        return Err("provider returned a null identity string".to_string());
    }
    let mut bytes = Vec::new();
    for offset in 0..256 {
        let byte = unsafe { *pointer.add(offset) }.cast_unsigned();
        if byte == 0 {
            return String::from_utf8(bytes).map_err(|e| e.to_string());
        }
        bytes.push(byte);
    }
    Err("provider identity string exceeds 255 bytes".to_string())
}
fn error_message(error: &[c_char]) -> String {
    String::from_utf8_lossy(
        &error
            .iter()
            .take_while(|&&b| b != 0)
            .map(|b| b.cast_unsigned())
            .collect::<Vec<_>>(),
    )
    .into_owned()
}
struct Session {
    api: Arc<Api>,
    handle: *mut c_void,
    _bytes: Box<[u8]>,
    pages: u32,
}
impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            (self.api.close)(self.handle);
        }
    }
}
struct Output<'a> {
    api: &'a Api,
    pointer: *mut u8,
}
impl Drop for Output<'_> {
    fn drop(&mut self) {
        if !self.pointer.is_null() {
            unsafe {
                (self.api.free)(self.pointer);
            }
        }
    }
}
impl DocumentSession for Session {
    fn page_count(&self) -> u32 {
        self.pages
    }
    fn info(&self) -> BTreeMap<String, String> {
        BTreeMap::new()
    }
    fn page_text(&mut self, page: u32) -> Result<PageText, BackendError> {
        if page == 0 || page > self.pages {
            return Err(BackendError::PageRange {
                page,
                count: self.pages,
            });
        }
        let mut pointer = std::ptr::null_mut();
        let mut length = 0;
        let mut error = [0; 512];
        // SAFETY: live handle and bounded output ABI; provider memory is freed
        // on every path, before the shared libraries or document can be dropped.
        let status = unsafe {
            (self.api.page)(
                self.handle,
                page,
                MAX_OUTPUT,
                &raw mut pointer,
                &raw mut length,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        let output = Output {
            api: &self.api,
            pointer,
        };
        let fail = |message| BackendError::Page { page, message };
        if status != 0 {
            return Err(fail(error_message(&error)));
        }
        if length == 0 || length > MAX_OUTPUT || output.pointer.is_null() {
            return Err(fail(
                "provider returned invalid or oversized page output".to_string(),
            ));
        }
        let bytes = unsafe { std::slice::from_raw_parts(output.pointer, length) };
        let mut result = parse_page(bytes, page).map_err(fail)?;
        result.warnings.push(format!(
            "native_provider: {} {}",
            self.api.engine, self.api.version
        ));
        Ok(result)
    }
}

#[derive(Deserialize)]
struct NativePage {
    abi: u32,
    page: u32,
    bounds: [f32; 4],
    to_pdf: [f32; 6],
    page_size: [f32; 2],
    rotation: i32,
    characters: usize,
    unmapped: usize,
    warnings: u32,
    structured: Structured,
    links: Vec<NativeLink>,
}
#[derive(Deserialize)]
struct Structured {
    blocks: Vec<Block>,
}
#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    bbox: Rect,
    #[serde(default)]
    lines: Vec<NativeLine>,
}
#[derive(Deserialize)]
struct NativeLine {
    bbox: Rect,
    font: Font,
    text: String,
}
#[derive(Deserialize)]
struct Font {
    name: String,
    size: f32,
}
#[derive(Clone, Copy, Deserialize)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}
#[derive(Deserialize)]
struct NativeLink {
    uri: String,
    bounds: Option<[f32; 4]>,
}

fn checked_bounds(b: [f32; 4]) -> Result<[f32; 4], String> {
    if b.iter().all(|v| v.is_finite() && v.abs() <= 1.0e8) && b[0] <= b[2] && b[1] <= b[3] {
        Ok(b)
    } else {
        Err("provider returned invalid geometry".to_string())
    }
}
fn convert(b: [f32; 4], transform: [f32; 6]) -> Result<BBox, String> {
    let b = checked_bounds(b)?;
    let [xx, yx, xy, yy, tx, ty] = transform;
    let corners = [(b[0], b[1]), (b[0], b[3]), (b[2], b[1]), (b[2], b[3])];
    let mut result = BBox {
        x0: f32::INFINITY,
        y0: f32::INFINITY,
        x1: f32::NEG_INFINITY,
        y1: f32::NEG_INFINITY,
    };
    for (x, y) in corners {
        let (x, y) = (xx * x + xy * y + tx, yx * x + yy * y + ty);
        result.x0 = result.x0.min(x);
        result.y0 = result.y0.min(y);
        result.x1 = result.x1.max(x);
        result.y1 = result.y1.max(y);
    }
    checked_bounds([result.x0, result.y0, result.x1, result.y1])?;
    Ok(result)
}
impl Rect {
    fn convert(self, transform: [f32; 6]) -> Result<BBox, String> {
        convert(
            [self.x, self.y, self.x + self.w, self.y + self.h],
            transform,
        )
    }
}
fn parse_page(bytes: &[u8], requested: u32) -> Result<PageText, String> {
    let raw: NativePage =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid provider JSON: {e}"))?;
    let bounds = checked_bounds(raw.bounds)?;
    if raw.abi != ABI
        || raw.page != requested
        || bounds[2] <= bounds[0]
        || bounds[3] <= bounds[1]
        || raw.characters > MAX_CHARACTERS
        || raw.unmapped > raw.characters
        || raw.links.len() > 100_000
    {
        return Err("provider page identity, bounds, or coverage is invalid".to_string());
    }
    if !matches!(raw.rotation, 0 | 90 | 180 | 270) {
        return Err("provider page rotation is invalid".to_string());
    }
    let size = raw.page_size;
    if size
        .iter()
        .any(|v| !v.is_finite() || *v <= 0.0 || *v > 1.0e8)
    {
        return Err("provider unrotated page size is invalid".to_string());
    }
    let [xx, yx, xy, yy, tx, ty] = raw.to_pdf;
    if [xx, yx, xy, yy, tx, ty]
        .iter()
        .any(|v| !v.is_finite() || v.abs() > 1.0e8)
    {
        return Err("provider PDF coordinate transform is invalid".to_string());
    }
    let font_scale = (xx * yy - yx * xy).abs().sqrt();
    if !font_scale.is_finite() || font_scale == 0.0 {
        return Err("provider PDF coordinate transform is singular".to_string());
    }
    let mut page = PageText::new(requested, size[0], size[1], raw.rotation);
    let mut characters = 0;
    let mut replacements = 0;
    for block in raw.structured.blocks {
        let bbox = block.bbox.convert(raw.to_pdf)?;
        match block.kind.as_str() {
            "text" => {
                for line in block.lines {
                    let bbox = line.bbox.convert(raw.to_pdf)?;
                    if !line.font.size.is_finite()
                        || line.font.size < 0.0
                        || line.font.size * font_scale > 1.0e8
                    {
                        return Err("provider returned invalid font size".to_string());
                    }
                    characters += line.text.chars().count();
                    replacements += line
                        .text
                        .chars()
                        .filter(|c| *c == '\u{fffd}' || *c == '\0')
                        .count();
                    if characters > MAX_CHARACTERS || page.spans.len() >= MAX_CHARACTERS {
                        return Err("provider page exceeds character limit".to_string());
                    }
                    page.spans.push(Span {
                        text: line.text.nfc().collect(),
                        bbox: Some(bbox),
                        font: Some(line.font.name),
                        size: Some(line.font.size * font_scale),
                        seq: u32::try_from(page.spans.len()).map_err(|e| e.to_string())?,
                    });
                }
            }
            "image" => page.figures.push(Figure {
                index: u32::try_from(page.figures.len()).map_err(|e| e.to_string())?,
                bbox: Some(bbox),
                kind: "raster".to_string(),
                mime: None,
                width_px: None,
                height_px: None,
                sha256: None,
                file: None,
                caption: None,
            }),
            _ => return Err(format!("unsupported provider block type: {}", block.kind)),
        }
    }
    if characters != raw.characters {
        return Err("provider text coverage count differs from output".to_string());
    }
    if raw.unmapped > 0 || replacements > 0 {
        page.warnings.push(format!(
            "unicode_mapping: native provider reported {} unresolved characters",
            raw.unmapped.max(replacements)
        ));
    }
    if raw.warnings > 0 {
        page.warnings.push(format!(
            "extraction_incomplete: native provider reported {} parser warnings",
            raw.warnings
        ));
    }
    for link in raw.links {
        page.links.push(Link {
            bbox: link
                .bounds
                .map(|rect| convert(rect, raw.to_pdf))
                .transpose()?,
            uri: link.uri,
        });
    }
    crate::router::mark_incomplete(&mut page);
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> serde_json::Value {
        serde_json::json!({"abi":1,"page":2,"bounds":[10,20,210,320],"rotation":0,"page_size":[200,300],"to_pdf":[1,0,0,-1,-10,320],"characters":3,"unmapped":0,"warnings":0,"structured":{"blocks":[{"type":"text","bbox":{"x":20,"y":30,"w":40,"h":15},"lines":[{"bbox":{"x":20,"y":30,"w":40,"h":15},"font":{"name":"Times","size":12},"text":"abc"}]}]},"links":[{"uri":"https://doi.org/10.1234/a?x=\"b\"","bounds":[20,30,60,45]}]})
    }
    #[test]
    fn preserves_real_geometry_fonts_and_raw_uri() {
        let result = parse_page(&serde_json::to_vec(&fixture()).unwrap(), 2).unwrap();
        assert_eq!((result.width, result.height), (200.0, 300.0));
        assert_eq!(
            result.spans[0].bbox,
            Some(BBox {
                x0: 10.0,
                y0: 275.0,
                x1: 50.0,
                y1: 290.0
            })
        );
        assert_eq!(result.spans[0].font.as_deref(), Some("Times"));
        assert_eq!(result.links[0].uri, "https://doi.org/10.1234/a?x=\"b\"");
        assert_eq!(result.extraction_status(), crate::schema::Status::Complete);
    }
    #[test]
    fn rejects_mismatched_coverage_page_geometry_and_missing_links() {
        for (field, value) in [
            ("characters", serde_json::json!(2)),
            ("page", serde_json::json!(1)),
            ("bounds", serde_json::json!([10, 20, 10, 30])),
        ] {
            let mut data = fixture();
            data[field] = value;
            assert!(parse_page(&serde_json::to_vec(&data).unwrap(), 2).is_err());
        }
        let mut data = fixture();
        data.as_object_mut().unwrap().remove("links");
        assert!(parse_page(&serde_json::to_vec(&data).unwrap(), 2).is_err());
    }
    #[test]
    fn reports_unknown_characters_and_native_parser_warnings_as_partial() {
        for field in ["unmapped", "warnings"] {
            let mut data = fixture();
            data[field] = serde_json::json!(1);
            assert_eq!(
                parse_page(&serde_json::to_vec(&data).unwrap(), 2)
                    .unwrap()
                    .extraction_status(),
                crate::schema::Status::Partial
            );
        }
    }
    #[cfg(feature = "mupdf")]
    #[test]
    #[ignore = "requires separately licensed MuPDF provider and runtime paths"]
    fn native_output_limit_fails_closed_and_releases_the_document() {
        let config = Configuration::read(Engine::MuPdf).unwrap();
        let api = Api::load(&config, "mupdf").unwrap();
        let bytes = super::super::probe_pdf().unwrap();
        let mut handle = std::ptr::null_mut();
        let mut pages = 0;
        let mut error = [0; 512];
        let status = unsafe {
            (api.open)(
                bytes.as_ptr(),
                bytes.len(),
                std::ptr::null(),
                &raw mut handle,
                &raw mut pages,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        assert_eq!(status, 0);
        let mut pointer = std::ptr::null_mut();
        let mut length = 0;
        let status = unsafe {
            (api.page)(
                handle,
                1,
                32,
                &raw mut pointer,
                &raw mut length,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        assert_ne!(status, 0);
        assert!(pointer.is_null());
        assert_eq!(length, 0);
        assert!(error_message(&error).contains("output limit"));
        let status = unsafe {
            (api.page)(
                handle,
                1,
                MAX_OUTPUT,
                &raw mut pointer,
                &raw mut length,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        let output = Output { api: &api, pointer };
        assert_eq!(status, 0);
        assert!(length > 32 && length < MAX_OUTPUT);
        drop(output);
        unsafe {
            (api.close)(handle);
        }
    }

    #[test]
    fn library_fingerprint_is_bounded_and_content_based() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime");
        std::fs::write(&path, b"first").unwrap();
        let first = fingerprint(&path).unwrap();
        std::fs::write(&path, b"other").unwrap();
        assert_ne!(first, fingerprint(&path).unwrap());
        File::create(&path)
            .unwrap()
            .set_len(MAX_LIBRARY + 1)
            .unwrap();
        assert!(fingerprint(&path).is_err());
    }
}

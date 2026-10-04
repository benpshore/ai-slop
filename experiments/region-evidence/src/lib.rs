//! Offline, opt-in evidence retention. No extraction, alignment or promotion.
//!
//! Hashes identify bytes, not correctness. Runtime/frame metadata are producer
//! declarations. Use [`Sidecar::validate`] before consuming a deserialized sidecar.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

pub const CONTRACT_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Digest(String);

impl Digest {
    pub fn of(bytes: &[u8]) -> Self {
        use fmt::Write as _;
        let mut hex = String::with_capacity(64);
        for byte in Sha256::digest(bytes) {
            write!(&mut hex, "{byte:02x}").expect("writing to String");
        }
        Self(hex)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(&self) -> Result<()> {
        ensure(
            self.0.len() == 64
                && self
                    .0
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "digest must be lowercase hexadecimal SHA-256",
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceIdentity {
    pub sha256: Digest,
    pub size: u64,
}

impl SourceIdentity {
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            sha256: Digest::of(bytes),
            size: bytes.len() as u64,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    pub sha256: Digest,
    pub size: u64,
}

/// Caller-controlled validation limits, deliberately outside serialized sidecars.
/// These bound accepted input sizes/counts; they do not schedule or stop workers.
#[derive(Clone, Debug)]
pub struct Limits {
    pub max_source_bytes: u64,
    pub max_sidecar_bytes: u64,
    pub max_alternative_attempts: usize,
    pub max_artifacts: usize,
    pub max_artifact_bytes: u64,
    pub max_total_artifact_bytes: u64,
    pub max_regions: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_source_bytes: u64::MAX,
            max_sidecar_bytes: 1024 * 1024,
            max_alternative_attempts: 8,
            max_artifacts: 9,
            max_artifact_bytes: 16 * 1024 * 1024,
            max_total_artifact_bytes: 64 * 1024 * 1024,
            max_regions: 10_000,
        }
    }
}

/// Exact, owned artifact bytes. No JSON reserialization or text normalization.
#[derive(Default, Debug)]
pub struct ArtifactStore(BTreeMap<Digest, Vec<u8>>);

impl ArtifactStore {
    pub fn insert(&mut self, bytes: Vec<u8>, limits: &Limits) -> Result<ArtifactRef> {
        let reference = ArtifactRef {
            sha256: Digest::of(&bytes),
            size: bytes.len() as u64,
        };
        ensure(
            reference.size <= limits.max_artifact_bytes,
            "artifact byte budget exceeded",
        )?;
        let exists = self.0.contains_key(&reference.sha256);
        ensure(
            self.0.len() + usize::from(!exists) <= limits.max_artifacts,
            "artifact count budget exceeded",
        )?;
        let additional = if exists { 0 } else { reference.size };
        let total = self
            .total_bytes()?
            .checked_add(additional)
            .ok_or_else(|| invalid("artifact byte count overflow"))?;
        ensure(
            total <= limits.max_total_artifact_bytes,
            "total artifact byte budget exceeded",
        )?;
        self.0.entry(reference.sha256.clone()).or_insert(bytes);
        Ok(reference)
    }

    fn total_bytes(&self) -> Result<u64> {
        self.0.values().try_fold(0_u64, |total, bytes| {
            total
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| invalid("artifact byte count overflow"))
        })
    }

    fn bytes(&self, reference: &ArtifactRef) -> Result<&[u8]> {
        reference.sha256.validate()?;
        let bytes = self
            .0
            .get(&reference.sha256)
            .ok_or_else(|| invalid("artifact bytes missing"))?;
        ensure(
            bytes.len() as u64 == reference.size && Digest::of(bytes) == reference.sha256,
            "artifact hash/size mismatch",
        )?;
        Ok(bytes)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendIdentity {
    pub name: String,
    pub version: String,
    pub config_digest: Digest,
}

/// Producer-declared runtime identity; the contract does not inspect executables.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "identity", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeIdentity {
    ProducerDeclared {
        name: String,
        version: String,
        build_digest: Digest,
    },
    Unknown {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    Complete,
    Partial {
        reason: String,
    },
    Failed {
        reason: String,
    },
    Cancelled {
        reason: String,
        requested_by: String,
    },
    ResourceLimit {
        resource: String,
        limit: u64,
        observed: Option<u64>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeometryFrame {
    Unknown,
    /// Declaration of points, bottom-left origin, unrotated PDF user space.
    /// This is not proof of the native-to-PDF transform or correspondence.
    ProducerDeclaredPdfUserSpaceUnrotated,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub id: String,
    pub source: SourceIdentity,
    /// Exact, ordered requested pages, matching the baseline.
    pub pages: Vec<u32>,
    pub backend: BackendIdentity,
    pub runtime: RuntimeIdentity,
    pub outcome: Outcome,
    pub artifact: Option<ArtifactRef>,
    pub frame: GeometryFrame,
}

/// Each index addresses the named array, never `Span.seq` or text equality.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EvidenceKind {
    Text { span_index: u32 },
    Layout { line_index: u32 },
    Uri { link_index: u32 },
    Figure { figure_index: u32 },
}

impl EvidenceKind {
    fn array_index(&self) -> (&'static str, usize) {
        match *self {
            Self::Text { span_index } => ("spans", span_index as usize),
            Self::Layout { line_index } => ("lines", line_index as usize),
            Self::Uri { link_index } => ("links", link_index as usize),
            Self::Figure { figure_index } => ("figures", figure_index as usize),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegionEvidence {
    pub id: String,
    pub attempt_id: String,
    pub artifact: ArtifactRef,
    pub page_index: u32,
    pub page: u32,
    pub evidence: EvidenceKind,
}

/// The only supported decision. No candidate can become retained output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    RetainBaselineAndAbstain,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sidecar {
    pub contract_version: u32,
    pub source: SourceIdentity,
    pub document_pages: u32,
    pub pages: Vec<u32>,
    pub baseline: Attempt,
    pub alternatives: Vec<Attempt>,
    pub regions: Vec<RegionEvidence>,
    pub decision: Decision,
}

impl Sidecar {
    /// Byte-budget check precedes deserialization. Still call `validate` next.
    pub fn from_json(bytes: &[u8], limits: &Limits) -> Result<Self> {
        ensure(
            bytes.len() as u64 <= limits.max_sidecar_bytes,
            "sidecar byte budget exceeded",
        )?;
        serde_json::from_value(unique_json(bytes)?)
            .map_err(|error| invalid(format!("invalid sidecar JSON: {error}")))
    }

    /// Validate source bytes, exact artifact bytes and every declared reference.
    /// Selected page counts are compared with extraction records; PDF parsing is
    /// intentionally absent and page-count correctness is not independently proven.
    pub fn validate<'a>(
        &'a self,
        source_bytes: &[u8],
        store: &'a ArtifactStore,
        limits: &Limits,
    ) -> Result<Validated<'a>> {
        let mut writer = BudgetWriter {
            remaining: limits.max_sidecar_bytes,
        };
        serde_json::to_writer(&mut writer, self)
            .map_err(|_| invalid("sidecar byte budget exceeded"))?;
        ensure(
            source_bytes.len() as u64 <= limits.max_source_bytes,
            "source byte budget exceeded",
        )?;
        ensure(
            self.source == SourceIdentity::of(source_bytes),
            "source hash/size mismatch",
        )?;
        ensure(
            self.contract_version == CONTRACT_VERSION,
            "unsupported contract version",
        )?;
        ensure(
            !self.pages.is_empty()
                && self
                    .pages
                    .iter()
                    .all(|page| *page > 0 && *page <= self.document_pages)
                && self.pages.windows(2).all(|pair| pair[0] < pair[1]),
            "invalid ordered selected pages",
        )?;
        ensure(
            self.alternatives.len() <= limits.max_alternative_attempts,
            "alternative attempt budget exceeded",
        )?;
        ensure(
            self.regions.len() <= limits.max_regions,
            "region budget exceeded",
        )?;
        ensure(
            store.0.len() <= limits.max_artifacts
                && store.total_bytes()? <= limits.max_total_artifact_bytes,
            "artifact store budget exceeded",
        )?;
        ensure(
            store
                .0
                .values()
                .all(|bytes| bytes.len() as u64 <= limits.max_artifact_bytes),
            "artifact byte budget exceeded",
        )?;
        ensure(
            self.baseline.backend.name == "lopdf",
            "baseline must identify lopdf",
        )?;
        ensure(
            matches!(
                self.baseline.outcome,
                Outcome::Complete | Outcome::Partial { .. }
            ) && self.baseline.artifact.is_some(),
            "baseline requires complete or partial artifact",
        )?;
        let mut attempts = BTreeMap::new();
        let mut artifacts = BTreeMap::new();
        for attempt in std::iter::once(&self.baseline).chain(&self.alternatives) {
            nonempty(&attempt.id, "attempt id")?;
            ensure(
                attempts.insert(attempt.id.as_str(), attempt).is_none(),
                "duplicate attempt id",
            )?;
            ensure(attempt.source == self.source, "attempt source mismatch")?;
            ensure(
                attempt.pages == self.pages,
                "attempt ordered pages mismatch",
            )?;
            nonempty(&attempt.backend.name, "backend name")?;
            nonempty(&attempt.backend.version, "backend version")?;
            attempt.backend.config_digest.validate()?;
            match &attempt.runtime {
                RuntimeIdentity::ProducerDeclared {
                    name,
                    version,
                    build_digest,
                } => {
                    nonempty(name, "runtime name")?;
                    nonempty(version, "runtime version")?;
                    build_digest.validate()?;
                }
                RuntimeIdentity::Unknown { reason } => nonempty(reason, "unknown runtime reason")?,
            }
            validate_outcome(&attempt.outcome)?;
            if let Some(reference) = &attempt.artifact {
                let bytes = store.bytes(reference)?;
                let record = unique_json(bytes)?;
                validate_record(&record, self, attempt)?;
                artifacts.insert(reference.sha256.clone(), record);
            } else {
                ensure(
                    !matches!(attempt.outcome, Outcome::Complete | Outcome::Partial { .. }),
                    "complete/partial attempt requires artifact",
                )?;
                ensure(
                    attempt.frame == GeometryFrame::Unknown,
                    "artifact-free attempt cannot declare geometry",
                )?;
            }
        }
        let mut ids = BTreeSet::new();
        let mut locators = BTreeSet::new();
        for region in &self.regions {
            nonempty(&region.id, "region id")?;
            ensure(ids.insert(&region.id), "duplicate region id")?;
            let attempt = attempts
                .get(region.attempt_id.as_str())
                .ok_or_else(|| invalid("region attempt missing"))?;
            ensure(
                attempt.artifact.as_ref() == Some(&region.artifact),
                "region artifact/attempt mismatch",
            )?;
            ensure(
                locators.insert((&region.attempt_id, region.page_index, &region.evidence)),
                "duplicate region locator",
            )?;
            let record = artifacts
                .get(&region.artifact.sha256)
                .ok_or_else(|| invalid("region artifact missing"))?;
            let page = record["pages"]
                .get(region.page_index as usize)
                .ok_or_else(|| invalid("region page index out of bounds"))?;
            ensure(
                page["page"].as_u64() == Some(u64::from(region.page)),
                "region page identity mismatch",
            )?;
            let (array, index) = region.evidence.array_index();
            ensure(
                page[array].get(index).is_some(),
                "region array index out of bounds",
            )?;
        }
        Ok(Validated {
            sidecar: self,
            store,
            artifacts,
        })
    }
}

/// Borrowed immutable input plus parsed artifact views. Mutation requires dropping
/// this validated view and validating again. No candidate-selection API exists.
pub struct Validated<'a> {
    sidecar: &'a Sidecar,
    store: &'a ArtifactStore,
    artifacts: BTreeMap<Digest, Value>,
}

impl<'a> Validated<'a> {
    pub fn sidecar(&self) -> &'a Sidecar {
        self.sidecar
    }

    /// The complete original baseline JSON, byte for byte.
    pub fn retained_baseline_bytes(&self) -> &'a [u8] {
        self.store
            .bytes(
                self.sidecar
                    .baseline
                    .artifact
                    .as_ref()
                    .expect("validated baseline"),
            )
            .expect("validated store")
    }

    pub fn alternative_bytes(&self, attempt_id: &str) -> Option<&'a [u8]> {
        let reference = self
            .sidecar
            .alternatives
            .iter()
            .find(|attempt| attempt.id == attempt_id)?
            .artifact
            .as_ref()?;
        self.store.bytes(reference).ok()
    }

    pub fn evidence(&self, id: &str) -> Option<ResolvedEvidence<'_>> {
        let region = self.sidecar.regions.iter().find(|region| region.id == id)?;
        let attempt = std::iter::once(&self.sidecar.baseline)
            .chain(&self.sidecar.alternatives)
            .find(|attempt| attempt.id == region.attempt_id)?;
        let (array, index) = region.evidence.array_index();
        let value = &self.artifacts[&region.artifact.sha256]["pages"][region.page_index as usize]
            [array][index];
        let geometry = if attempt.frame == GeometryFrame::Unknown || value["bbox"].is_null() {
            None
        } else {
            Some(DeclaredBox {
                frame: attempt.frame,
                coordinates: ["x0", "y0", "x1", "y1"]
                    .map(|key| value["bbox"][key].as_f64().expect("validated bbox")),
            })
        };
        Some(ResolvedEvidence {
            locator: region,
            value,
            geometry,
        })
    }
}

pub struct ResolvedEvidence<'a> {
    pub locator: &'a RegionEvidence,
    /// Existing record, including normalized text, font, seq, links or figures.
    pub value: &'a Value,
    pub geometry: Option<DeclaredBox>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeclaredBox {
    pub frame: GeometryFrame,
    /// x0, y0, x1, y1 in the producer-declared frame. Never invented.
    pub coordinates: [f64; 4],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError(pub String);

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for ValidationError {}
pub type Result<T> = std::result::Result<T, ValidationError>;

fn invalid(message: impl Into<String>) -> ValidationError {
    ValidationError(message.into())
}
fn ensure(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(invalid(message))
    }
}
fn nonempty(value: &str, field: &str) -> Result<()> {
    ensure(
        !value.trim().is_empty(),
        &format!("{field} must not be empty"),
    )
}

fn validate_outcome(outcome: &Outcome) -> Result<()> {
    match outcome {
        Outcome::Complete => Ok(()),
        Outcome::Partial { reason } | Outcome::Failed { reason } => {
            nonempty(reason, "outcome reason")
        }
        Outcome::Cancelled {
            reason,
            requested_by,
        } => {
            nonempty(reason, "cancellation reason")?;
            nonempty(requested_by, "cancellation requester")
        }
        Outcome::ResourceLimit { resource, .. } => nonempty(resource, "limited resource"),
    }
}

fn array<'a>(value: &'a Value, field: &str) -> Result<&'a [Value]> {
    value[field]
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| invalid(format!("missing/invalid {field} array")))
}
fn strings(value: &Value, field: &str) -> Result<()> {
    ensure(
        array(value, field)?.iter().all(Value::is_string),
        "invalid string array",
    )
}
fn finite(value: &Value) -> bool {
    value.as_f64().is_some_and(f64::is_finite)
}
fn uint(value: &Value) -> bool {
    value
        .as_u64()
        .is_some_and(|number| u32::try_from(number).is_ok())
}

fn validate_box(value: &Value) -> Result<()> {
    if value.is_null() {
        return Ok(());
    }
    ensure(
        value.is_object()
            && ["x0", "y0", "x1", "y1"]
                .iter()
                .all(|key| finite(&value[*key])),
        "invalid/nonfinite bbox",
    )?;
    ensure(
        value["x0"].as_f64() <= value["x1"].as_f64()
            && value["y0"].as_f64() <= value["y1"].as_f64(),
        "reversed bbox",
    )
}

/// Minimal schema-5 envelope and addressable evidence checks, not a second
/// production schema or a semantic correctness validator. Unknown artifact fields
/// are preserved in exact bytes. Unknown sidecar fields are rejected by serde.
fn validate_record(record: &Value, sidecar: &Sidecar, attempt: &Attempt) -> Result<()> {
    ensure(
        record["schema_version"].as_u64() == Some(5),
        "unsupported extraction schema",
    )?;
    ensure(
        record["document"]["hash"].as_str() == Some(sidecar.source.sha256.as_str())
            && record["document"]["size"].as_u64() == Some(sidecar.source.size),
        "artifact source mismatch",
    )?;
    ensure(
        record["document"]["pages"].as_u64() == Some(u64::from(sidecar.document_pages)),
        "artifact document page count mismatch",
    )?;
    array(&record["document"], "sources")?;
    let backend: BackendIdentity = serde_json::from_value(record["backend"].clone())
        .map_err(|_| invalid("invalid artifact backend"))?;
    ensure(
        backend == attempt.backend,
        "artifact backend/config identity mismatch",
    )?;
    let accepted = match &attempt.outcome {
        Outcome::Complete => record["status"] == "complete",
        Outcome::Partial { .. } => record["status"] == "partial",
        Outcome::Failed { .. } => record["status"] == "failed",
        Outcome::Cancelled { .. } | Outcome::ResourceLimit { .. } => {
            record["status"] == "partial" || record["status"] == "failed"
        }
    };
    ensure(accepted, "artifact outcome mismatch")?;
    for field in ["chunks", "references", "citations"] {
        array(record, field)?;
    }
    for field in ["metadata", "timings"] {
        ensure(record[field].is_object(), "missing extraction object")?;
    }
    strings(record, "warnings")?;
    let pages = array(record, "pages")?;
    ensure(
        pages.len() == sidecar.pages.len(),
        "artifact selected page count mismatch",
    )?;
    for (page, expected) in pages.iter().zip(&sidecar.pages) {
        ensure(
            page["page"].as_u64() == Some(u64::from(*expected)),
            "artifact ordered page identity mismatch",
        )?;
        ensure(
            ["width", "height"]
                .iter()
                .all(|field| finite(&page[*field]) && page[*field].as_f64() >= Some(0.0))
                && page["rotation"]
                    .as_i64()
                    .is_some_and(|value| i32::try_from(value).is_ok()),
            "invalid page geometry",
        )?;
        ensure(page["text"].is_string(), "missing page text")?;
        strings(page, "warnings")?;
        let spans = array(page, "spans")?;
        for span in spans {
            ensure(
                span["text"].is_string()
                    && uint(&span["seq"])
                    && (span["font"].is_null() || span["font"].is_string())
                    && (span["size"].is_null() || finite(&span["size"])),
                "invalid span",
            )?;
        }
        for line in array(page, "lines")? {
            ensure(
                line["text"].is_string()
                    && uint(&line["column"])
                    && (line.get("role").is_none() || line["role"].is_string()),
                "invalid line",
            )?;
            for index in array(line, "spans")? {
                ensure(
                    index
                        .as_u64()
                        .is_some_and(|index| index < spans.len() as u64),
                    "line span index out of bounds",
                )?;
            }
        }
        for link in array(page, "links")? {
            ensure(link["uri"].is_string(), "invalid URI evidence")?;
        }
        for figure in array(page, "figures")? {
            ensure(
                uint(&figure["index"]) && figure["kind"].is_string(),
                "invalid figure evidence",
            )?;
        }
        for field in ["spans", "lines", "links", "figures"] {
            for item in array(page, field)? {
                ensure(
                    item.get("bbox").is_some(),
                    "missing bbox field (use null when unknown)",
                )?;
                validate_box(&item["bbox"])?;
            }
        }
    }
    Ok(())
}

struct BudgetWriter {
    remaining: u64,
}
impl std::io::Write for BudgetWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("sidecar byte budget exceeded"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `Value` normally silently accepts duplicate object members. Reject them at
/// every depth before using an artifact or converting a sidecar to typed fields.
fn unique_json(bytes: &[u8]) -> Result<Value> {
    struct Unique(Value);
    impl<'de> Deserialize<'de> for Unique {
        fn deserialize<D: serde::Deserializer<'de>>(
            deserializer: D,
        ) -> std::result::Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Unique;
                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("JSON without duplicate object members")
                }
                fn visit_bool<E: serde::de::Error>(
                    self,
                    value: bool,
                ) -> std::result::Result<Unique, E> {
                    Ok(Unique(value.into()))
                }
                fn visit_i64<E: serde::de::Error>(
                    self,
                    value: i64,
                ) -> std::result::Result<Unique, E> {
                    Ok(Unique(value.into()))
                }
                fn visit_u64<E: serde::de::Error>(
                    self,
                    value: u64,
                ) -> std::result::Result<Unique, E> {
                    Ok(Unique(value.into()))
                }
                fn visit_f64<E: serde::de::Error>(
                    self,
                    value: f64,
                ) -> std::result::Result<Unique, E> {
                    serde_json::Number::from_f64(value)
                        .map(|number| Unique(Value::Number(number)))
                        .ok_or_else(|| E::custom("nonfinite number"))
                }
                fn visit_str<E: serde::de::Error>(
                    self,
                    value: &str,
                ) -> std::result::Result<Unique, E> {
                    Ok(Unique(value.into()))
                }
                fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Unique, E> {
                    Ok(Unique(Value::Null))
                }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut seq: A,
                ) -> std::result::Result<Unique, A::Error> {
                    let mut values = Vec::new();
                    while let Some(Unique(value)) = seq.next_element()? {
                        values.push(value);
                    }
                    Ok(Unique(Value::Array(values)))
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> std::result::Result<Unique, A::Error> {
                    let mut values = serde_json::Map::new();
                    while let Some(key) = map.next_key::<String>()? {
                        if values.contains_key(&key) {
                            return Err(serde::de::Error::custom(format!(
                                "duplicate JSON member: {key}"
                            )));
                        }
                        let Unique(value) = map.next_value()?;
                        values.insert(key, value);
                    }
                    Ok(Unique(Value::Object(values)))
                }
            }
            deserializer.deserialize_any(Visitor)
        }
    }
    serde_json::from_slice::<Unique>(bytes)
        .map(|value| value.0)
        .map_err(|error| invalid(format!("invalid JSON: {error}")))
}

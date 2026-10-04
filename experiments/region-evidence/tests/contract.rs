use serde_json::{Value, json};
use tpe_region_evidence::*;

const SOURCE: &[u8] = b"synthetic source bytes; this fixture does not parse a PDF";

fn backend(name: &str) -> BackendIdentity {
    BackendIdentity {
        name: name.into(),
        version: "fixture".into(),
        config_digest: Digest::of(b"configuration fixture"),
    }
}

fn page(number: u32, text: &str) -> Value {
    let bbox = json!({"x0": 1.0, "y0": 2.0, "x1": 30.0, "y1": 12.0});
    json!({
        "page": number, "width": 612.0, "height": 792.0, "rotation": 0,
        "spans": [
            {"text": text, "bbox": bbox, "font": "fixture", "size": 10.0, "seq": 99},
            {"text": text, "bbox": null, "font": null, "size": null, "seq": 99}
        ],
        "lines": [{"text": text, "bbox": bbox, "column": 0, "spans": [1, 0], "role": "body"}],
        "links": [{"bbox": null, "uri": "https://example.invalid/doi/unchanged"}],
        "figures": [{"index": 42, "bbox": bbox, "kind": "vector", "mime": null,
            "width_px": null, "height_px": null, "sha256": null, "file": null,
            "caption": "keep this figure"}],
        "text": text, "warnings": ["fixture warning retained"]
    })
}

fn record(name: &str, status: &str, text: &str) -> Value {
    json!({
        "schema_version": 5,
        "document": {"hash": Digest::of(SOURCE), "size": SOURCE.len(), "pages": 3,
            "sources": [{"path": "original.pdf", "mtime_unix": null, "size": SOURCE.len()}]},
        "backend": backend(name), "status": status,
        "pages": [page(1, text), page(3, "café")],
        "chunks": [], "metadata": {"title": "keep this metadata"},
        "references": [{"raw": "keep this reference"}],
        "citations": [{"text": "keep this citation"}],
        "warnings": ["keep this document warning"], "timings": {"parse_ms": 1.0},
        "future_artifact_field": {"nested": ["preserve unknown artifact fields too"]}
    })
}

fn exact_bytes(record: &Value) -> Vec<u8> {
    let mut bytes = b" \n".to_vec();
    bytes.extend(serde_json::to_vec_pretty(record).unwrap());
    bytes.extend_from_slice(b"\n\t");
    bytes
}

fn attempt(id: &str, name: &str, outcome: Outcome, artifact: Option<ArtifactRef>) -> Attempt {
    Attempt {
        id: id.into(),
        source: SourceIdentity::of(SOURCE),
        pages: vec![1, 3],
        backend: backend(name),
        runtime: RuntimeIdentity::Unknown {
            reason: "fixture has no runtime binary".into(),
        },
        outcome,
        artifact,
        frame: GeometryFrame::Unknown,
    }
}

fn fixture() -> (Sidecar, ArtifactStore, Vec<Vec<u8>>) {
    let limits = Limits::default();
    let bytes = vec![
        exact_bytes(&record("lopdf", "partial", "short baseline")),
        exact_bytes(&record(
            "native-a",
            "complete",
            "A much much longer alternative including possibly wrong content",
        )),
        exact_bytes(&record("native-b", "partial", "competing alternative")),
    ];
    let mut store = ArtifactStore::default();
    let references: Vec<_> = bytes
        .iter()
        .map(|bytes| store.insert(bytes.clone(), &limits).unwrap())
        .collect();
    let baseline = attempt(
        "baseline",
        "lopdf",
        Outcome::Partial {
            reason: "baseline uncertainty".into(),
        },
        Some(references[0].clone()),
    );
    let alternatives = vec![
        attempt(
            "native-a",
            "native-a",
            Outcome::Complete,
            Some(references[1].clone()),
        ),
        attempt(
            "native-b",
            "native-b",
            Outcome::Partial {
                reason: "partial mapping".into(),
            },
            Some(references[2].clone()),
        ),
        attempt(
            "failure",
            "unavailable",
            Outcome::Failed {
                reason: "runtime unavailable".into(),
            },
            None,
        ),
        attempt(
            "cancellation",
            "cancelled",
            Outcome::Cancelled {
                reason: "caller stopped".into(),
                requested_by: "test caller".into(),
            },
            None,
        ),
        attempt(
            "limited",
            "bounded",
            Outcome::ResourceLimit {
                resource: "elapsed_ms".into(),
                limit: 50,
                observed: Some(51),
            },
            None,
        ),
    ];
    let mut regions = vec![];
    for owner in std::iter::once(&baseline).chain(&alternatives) {
        if let Some(artifact) = &owner.artifact {
            for (suffix, evidence) in [
                ("span-0", EvidenceKind::Text { span_index: 0 }),
                ("span-1", EvidenceKind::Text { span_index: 1 }),
                ("line", EvidenceKind::Layout { line_index: 0 }),
                ("uri", EvidenceKind::Uri { link_index: 0 }),
                ("figure", EvidenceKind::Figure { figure_index: 0 }),
            ] {
                regions.push(RegionEvidence {
                    id: format!("{}-{suffix}", owner.id),
                    attempt_id: owner.id.clone(),
                    artifact: artifact.clone(),
                    page_index: 0,
                    page: 1,
                    evidence,
                });
            }
        }
    }
    (
        Sidecar {
            contract_version: CONTRACT_VERSION,
            source: SourceIdentity::of(SOURCE),
            document_pages: 3,
            pages: vec![1, 3],
            baseline,
            alternatives,
            regions,
            decision: Decision::RetainBaselineAndAbstain,
        },
        store,
        bytes,
    )
}

fn rejected(sidecar: &Sidecar, store: &ArtifactStore, expected: &str) {
    let error = sidecar
        .validate(SOURCE, store, &Limits::default())
        .err()
        .expect("must reject");
    assert!(
        error.to_string().contains(expected),
        "expected {expected:?}, got {error}"
    );
}

fn replace_baseline(sidecar: &mut Sidecar, store: &mut ArtifactStore, bytes: Vec<u8>) {
    let reference = store.insert(bytes, &Limits::default()).unwrap();
    sidecar.baseline.artifact = Some(reference.clone());
    for region in &mut sidecar.regions {
        if region.attempt_id == sidecar.baseline.id {
            region.artifact = reference.clone();
        }
    }
}

#[test]
fn serialized_roundtrip_retains_complete_baseline_and_every_alternative_byte_for_byte() {
    let (sidecar, store, bytes) = fixture();
    let encoded = serde_json::to_vec_pretty(&sidecar).unwrap();
    let decoded = Sidecar::from_json(&encoded, &Limits::default()).unwrap();
    assert_eq!(decoded, sidecar);
    let checked = decoded
        .validate(SOURCE, &store, &Limits::default())
        .unwrap();
    assert_eq!(checked.retained_baseline_bytes(), bytes[0]);
    assert_eq!(checked.alternative_bytes("native-a").unwrap(), bytes[1]);
    assert_eq!(checked.alternative_bytes("native-b").unwrap(), bytes[2]);
    assert_eq!(checked.sidecar().alternatives.len(), 5);
    assert_eq!(
        checked.sidecar().decision,
        Decision::RetainBaselineAndAbstain
    );
    assert!(checked.alternative_bytes("failure").is_none());
    assert!(checked.alternative_bytes("cancellation").is_none());
    assert!(checked.alternative_bytes("limited").is_none());
}

#[test]
fn longer_complete_candidate_never_replaces_partial_baseline() {
    let (sidecar, store, bytes) = fixture();
    assert!(bytes[1].len() > bytes[0].len());
    let checked = sidecar
        .validate(SOURCE, &store, &Limits::default())
        .unwrap();
    assert_eq!(checked.retained_baseline_bytes(), bytes[0]);
    assert_eq!(
        checked.evidence("baseline-span-0").unwrap().value["text"],
        "short baseline"
    );
}

#[test]
fn repeated_text_and_seq_are_distinct_array_locators_and_geometry_is_never_inferred() {
    let (mut sidecar, store, _) = fixture();
    let checked = sidecar
        .validate(SOURCE, &store, &Limits::default())
        .unwrap();
    let first = checked.evidence("baseline-span-0").unwrap();
    let second = checked.evidence("baseline-span-1").unwrap();
    assert_eq!(first.value["text"], second.value["text"]);
    assert_eq!(first.value["seq"], second.value["seq"]);
    assert_ne!(first.locator.evidence, second.locator.evidence);
    assert!(first.geometry.is_none()); // Unknown frame, despite stored bbox.
    drop(checked);
    sidecar.baseline.frame = GeometryFrame::ProducerDeclaredPdfUserSpaceUnrotated;
    let checked = sidecar
        .validate(SOURCE, &store, &Limits::default())
        .unwrap();
    assert_eq!(
        checked
            .evidence("baseline-span-0")
            .unwrap()
            .geometry
            .unwrap()
            .coordinates,
        [1.0, 2.0, 30.0, 12.0]
    );
    assert!(
        checked
            .evidence("baseline-span-1")
            .unwrap()
            .geometry
            .is_none()
    ); // Missing bbox stays missing.
    assert_eq!(
        checked.evidence("baseline-uri").unwrap().value["uri"],
        "https://example.invalid/doi/unchanged"
    );
    assert_eq!(
        checked.evidence("baseline-figure").unwrap().value["index"],
        42
    ); // Locator was array index 0.
}

#[test]
fn source_artifact_digest_and_size_tampering_are_rejected() {
    let (mut sidecar, store, _) = fixture();
    assert!(
        sidecar
            .validate(b"different source", &store, &Limits::default())
            .is_err()
    );
    sidecar.baseline.artifact.as_mut().unwrap().size += 1;
    rejected(&sidecar, &store, "artifact hash/size mismatch");
    let (mut sidecar, store, _) = fixture();
    sidecar.baseline.artifact.as_mut().unwrap().sha256 = Digest::of(b"unregistered modified bytes");
    rejected(&sidecar, &store, "artifact bytes missing");
    let (sidecar, _, bytes) = fixture();
    let mut wrong_store = ArtifactStore::default();
    let mut tampered = bytes[0].clone();
    tampered.extend_from_slice(b" "); // Semantically same JSON, different evidence bytes.
    wrong_store.insert(tampered, &Limits::default()).unwrap();
    rejected(&sidecar, &wrong_store, "artifact bytes missing");
}

#[test]
fn claims_must_match_embedded_artifact_identity_and_ordered_pages() {
    for (mutation, expected) in [
        (0, "artifact source mismatch"),
        (1, "artifact source mismatch"),
        (2, "artifact document page count mismatch"),
        (3, "artifact ordered page identity mismatch"),
        (4, "artifact backend/config identity mismatch"),
        (5, "artifact outcome mismatch"),
    ] {
        let (mut sidecar, mut store, _) = fixture();
        let mut artifact = record("lopdf", "partial", "short baseline");
        match mutation {
            0 => artifact["document"]["hash"] = json!(Digest::of(b"other source")),
            1 => artifact["document"]["size"] = json!(SOURCE.len() + 1),
            2 => artifact["document"]["pages"] = json!(4),
            3 => artifact["pages"].as_array_mut().unwrap().swap(0, 1),
            4 => artifact["backend"]["config_digest"] = json!(Digest::of(b"different config")),
            5 => artifact["status"] = json!("complete"),
            _ => unreachable!(),
        }
        replace_baseline(&mut sidecar, &mut store, exact_bytes(&artifact));
        rejected(&sidecar, &store, expected);
    }
    let (mut sidecar, store, _) = fixture();
    sidecar.alternatives[0].source = SourceIdentity::of(b"other PDF");
    rejected(&sidecar, &store, "attempt source mismatch");
    sidecar.alternatives[0].source = sidecar.source.clone();
    sidecar.alternatives[0].pages = vec![3, 1];
    rejected(&sidecar, &store, "attempt ordered pages mismatch");
}

#[test]
fn missing_cross_attempt_and_out_of_bounds_region_locators_are_errors() {
    for (mutation, expected) in [
        (0, "region attempt missing"),
        (1, "region artifact/attempt mismatch"),
        (2, "region page index out of bounds"),
        (3, "region page identity mismatch"),
        (4, "region array index out of bounds"),
    ] {
        let (mut sidecar, store, _) = fixture();
        match mutation {
            0 => sidecar.regions[0].attempt_id = "missing".into(),
            1 => sidecar.regions[0].artifact = sidecar.alternatives[0].artifact.clone().unwrap(),
            2 => sidecar.regions[0].page_index = 3,
            3 => sidecar.regions[0].page = 2,
            4 => sidecar.regions[0].evidence = EvidenceKind::Text { span_index: 99 },
            _ => unreachable!(),
        }
        rejected(&sidecar, &store, expected);
    }
}

#[test]
fn duplicate_attempts_regions_and_consumed_locations_are_errors() {
    let (mut sidecar, store, _) = fixture();
    sidecar.alternatives[0].id = sidecar.baseline.id.clone();
    rejected(&sidecar, &store, "duplicate attempt id");
    let (mut sidecar, store, _) = fixture();
    sidecar.regions[1].id = sidecar.regions[0].id.clone();
    rejected(&sidecar, &store, "duplicate region id");
    let (mut sidecar, store, _) = fixture();
    sidecar.regions[1].evidence = sidecar.regions[0].evidence.clone();
    rejected(&sidecar, &store, "duplicate region locator");
}

#[test]
fn malformed_unreferenced_geometry_and_internal_line_references_are_rejected() {
    for (mutation, expected) in [
        (0, "reversed bbox"),
        (1, "invalid/nonfinite bbox"),
        (2, "line span index out of bounds"),
        (3, "missing/invalid figures array"),
    ] {
        let (mut sidecar, mut store, _) = fixture();
        let mut artifact = record("lopdf", "partial", "short baseline");
        match mutation {
            0 => artifact["pages"][1]["spans"][0]["bbox"]["x0"] = json!(999),
            1 => artifact["pages"][1]["spans"][0]["bbox"]["y0"] = json!("NaN"),
            2 => artifact["pages"][1]["lines"][0]["spans"] = json!([2]),
            3 => {
                artifact["pages"][1]
                    .as_object_mut()
                    .unwrap()
                    .remove("figures");
            }
            _ => unreachable!(),
        }
        replace_baseline(&mut sidecar, &mut store, exact_bytes(&artifact));
        rejected(&sidecar, &store, expected);
    }
}

#[test]
fn partial_and_terminal_attempts_have_honest_artifact_requirements() {
    let (mut sidecar, store, _) = fixture();
    sidecar.alternatives[1].artifact = None;
    rejected(
        &sidecar,
        &store,
        "complete/partial attempt requires artifact",
    );
    let (mut sidecar, store, _) = fixture();
    sidecar.alternatives[3].outcome = Outcome::Cancelled {
        reason: String::new(),
        requested_by: "caller".into(),
    };
    rejected(&sidecar, &store, "cancellation reason");
    let (mut sidecar, store, _) = fixture();
    sidecar.alternatives[2].frame = GeometryFrame::ProducerDeclaredPdfUserSpaceUnrotated;
    rejected(
        &sidecar,
        &store,
        "artifact-free attempt cannot declare geometry",
    );
}

#[test]
fn sidecar_rejects_missing_locators_unknown_frames_decisions_and_fields() {
    let (sidecar, _, _) = fixture();
    for mutation in 0..4 {
        let mut encoded = serde_json::to_value(&sidecar).unwrap();
        match mutation {
            0 => {
                encoded["regions"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("evidence");
            }
            1 => encoded["baseline"]["frame"] = json!("assumed_pdf_frame"),
            2 => encoded["decision"] = json!("promote_longest_candidate"),
            3 => encoded["confidence"] = json!(0.99),
            _ => unreachable!(),
        }
        assert!(
            Sidecar::from_json(&serde_json::to_vec(&encoded).unwrap(), &Limits::default()).is_err()
        );
    }
}

#[test]
fn duplicate_json_members_are_rejected_at_every_depth() {
    let (mut sidecar, mut store, _) = fixture();
    let encoded = serde_json::to_string(&sidecar).unwrap().replacen(
        "\"contract_version\":1",
        "\"contract_version\":1,\"contract_version\":1",
        1,
    );
    assert!(
        Sidecar::from_json(encoded.as_bytes(), &Limits::default())
            .err()
            .unwrap()
            .to_string()
            .contains("duplicate JSON member")
    );
    let artifact = serde_json::to_string(&record("lopdf", "partial", "short baseline")).unwrap();
    let duplicate = artifact.replacen(
        "\"text\":\"short baseline\"",
        "\"text\":\"discarded hidden content\",\"text\":\"short baseline\"",
        1,
    );
    assert_ne!(duplicate, artifact);
    replace_baseline(&mut sidecar, &mut store, duplicate.into_bytes());
    rejected(&sidecar, &store, "duplicate JSON member: text");
}

#[test]
fn every_contract_budget_is_enforced_without_replacing_baseline() {
    let (sidecar, store, bytes) = fixture();
    let encoded = serde_json::to_vec(&sidecar).unwrap();
    assert_eq!(Limits::default().max_source_bytes, u64::MAX);
    for mut limits in [
        Limits {
            max_source_bytes: SOURCE.len() as u64 - 1,
            ..Limits::default()
        },
        Limits {
            max_alternative_attempts: 4,
            ..Limits::default()
        },
        Limits {
            max_regions: 14,
            ..Limits::default()
        },
        Limits {
            max_artifacts: 2,
            ..Limits::default()
        },
        Limits {
            max_artifact_bytes: bytes[0].len() as u64 - 1,
            ..Limits::default()
        },
        Limits {
            max_total_artifact_bytes: bytes.iter().map(|bytes| bytes.len() as u64).sum::<u64>() - 1,
            ..Limits::default()
        },
        Limits {
            max_sidecar_bytes: encoded.len() as u64 - 1,
            ..Limits::default()
        },
    ] {
        assert!(sidecar.validate(SOURCE, &store, &limits).is_err());
        limits.max_sidecar_bytes = 1;
        assert!(Sidecar::from_json(&encoded, &limits).is_err());
    }
    let mut tiny_store = ArtifactStore::default();
    assert!(
        tiny_store
            .insert(
                bytes[0].clone(),
                &Limits {
                    max_artifact_bytes: 1,
                    ..Limits::default()
                }
            )
            .is_err()
    );
    assert!(
        tiny_store
            .insert(
                bytes[0].clone(),
                &Limits {
                    max_artifacts: 0,
                    ..Limits::default()
                }
            )
            .is_err()
    );
    assert!(
        tiny_store
            .insert(
                bytes[0].clone(),
                &Limits {
                    max_total_artifact_bytes: 1,
                    ..Limits::default()
                }
            )
            .is_err()
    );
    assert_eq!(
        sidecar
            .validate(SOURCE, &store, &Limits::default())
            .unwrap()
            .retained_baseline_bytes(),
        bytes[0]
    );
}

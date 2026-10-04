//! Validate two existing schema-5 extraction files; never runs a backend.
use std::{env, fs};

use serde_json::Value;
use tpe_region_evidence::{
    ArtifactStore, Attempt, CONTRACT_VERSION, Decision, EvidenceKind, GeometryFrame, Limits,
    Outcome, RegionEvidence, RuntimeIdentity, Sidecar, SourceIdentity,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths: Vec<_> = env::args_os().skip(1).collect();
    if paths.len() != 3 {
        return Err(
            "usage: verify_artifacts <source.pdf> <lopdf-result.json> <candidate-result.json>"
                .into(),
        );
    }
    // This example loads files before contract validation; Limits are accepted
    // evidence bounds, not streaming I/O, process RSS or scheduler limits.
    let source = fs::read(&paths[0])?;
    let source_identity = SourceIdentity::of(&source);
    let limits = Limits::default();
    let mut store = ArtifactStore::default();
    let mut attempts = Vec::new();
    let mut regions = Vec::new();
    let mut document_pages = 0;
    for (id, path) in ["baseline", "candidate"].into_iter().zip(&paths[1..]) {
        let bytes = fs::read(path)?;
        let value: Value = serde_json::from_slice(&bytes)?;
        let artifact = store.insert(bytes, &limits)?;
        let pages = value["pages"].as_array().ok_or("missing pages")?;
        if id == "baseline" {
            document_pages = serde_json::from_value(value["document"]["pages"].clone())?;
        }
        let outcome = match value["status"].as_str() {
            Some("complete") => Outcome::Complete,
            Some("partial") => Outcome::Partial {
                reason: "artifact declares partial extraction".into(),
            },
            Some("failed") => Outcome::Failed {
                reason: "artifact declares failed extraction".into(),
            },
            _ => return Err("example accepts complete, partial or failed artifacts".into()),
        };
        let mut selected_pages = Vec::new();
        for (page_index, page) in pages.iter().enumerate() {
            let page_number = serde_json::from_value(page["page"].clone())?;
            selected_pages.push(page_number);
            for array in ["spans", "lines", "links", "figures"] {
                for (index, _) in page[array]
                    .as_array()
                    .ok_or("missing evidence array")?
                    .iter()
                    .enumerate()
                {
                    let index = u32::try_from(index)?;
                    let evidence = match array {
                        "spans" => EvidenceKind::Text { span_index: index },
                        "lines" => EvidenceKind::Layout { line_index: index },
                        "links" => EvidenceKind::Uri { link_index: index },
                        "figures" => EvidenceKind::Figure {
                            figure_index: index,
                        },
                        _ => unreachable!(),
                    };
                    regions.push(RegionEvidence {
                        id: format!("{id}/{page_index}/{array}/{index}"),
                        attempt_id: id.into(),
                        artifact: artifact.clone(),
                        page_index: u32::try_from(page_index)?,
                        page: page_number,
                        evidence,
                    });
                }
            }
        }
        attempts.push(Attempt {
            id: id.into(),
            source: source_identity.clone(),
            pages: selected_pages,
            backend: serde_json::from_value(value["backend"].clone())?,
            runtime: RuntimeIdentity::Unknown {
                reason: "runtime binary was not supplied".into(),
            },
            outcome,
            artifact: Some(artifact),
            frame: GeometryFrame::Unknown,
        });
    }
    let baseline = attempts.remove(0);
    let sidecar = Sidecar {
        contract_version: CONTRACT_VERSION,
        source: source_identity,
        document_pages,
        pages: baseline.pages.clone(),
        baseline,
        alternatives: attempts,
        regions,
        decision: Decision::RetainBaselineAndAbstain,
    };
    // Round-trip the serialized sidecar as an external consumer would. Artifact
    // validation also rejects duplicate JSON keys, including keys read above.
    let sidecar = Sidecar::from_json(&serde_json::to_vec(&sidecar)?, &limits)?;
    let validated = sidecar.validate(&source, &store, &limits)?;
    println!(
        "Integrity checked: {} pages, {} locators; retained {} baseline bytes and {} alternative bytes. Decision: retain_baseline_and_abstain. No semantic correctness or correspondence established.",
        sidecar.pages.len(),
        sidecar.regions.len(),
        validated.retained_baseline_bytes().len(),
        validated
            .alternative_bytes("candidate")
            .map_or(0, <[u8]>::len),
    );
    Ok(())
}

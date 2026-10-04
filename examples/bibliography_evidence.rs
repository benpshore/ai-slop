//! Focused corpus diagnostics, not a supported extraction CLI or score baseline.
//! Original backend spans are retained by the pipeline. Export those alongside
//! final lines and a fresh order-only projection to locate the stage of a defect.

#[cfg(any(feature = "pdfium", feature = "docling-text"))]
mod enabled {
    use std::fs::{self, File};
    use std::path::Path;

    use anyhow::{Context, Result, ensure};
    use serde::Serialize;
    use serde_json::json;
    use tpe::{backend, corpus, eval, latex_refs, pipeline, reading_order, schema};

    #[cfg(feature = "docling")]
    fn omit_picture_payloads(node: &mut docling_core::Node, images: &mut Vec<serde_json::Value>) {
        use docling_core::Node;
        match node {
            Node::Picture {
                image: Some(image), ..
            } => {
                images.push(json!({
                    "picture_visit_index": images.len(), "bytes": image.data.len(),
                    "sha256": schema::sha256_hex(&image.data),
                    "mimetype": image.mimetype, "width": image.width, "height": image.height,
                }));
                image.data.clear();
            }
            Node::Group { children, .. } => {
                for child in children {
                    omit_picture_payloads(child, images);
                }
            }
            Node::Table(table) | Node::Chart { table, .. } => {
                if let Some(rows) = &mut table.cell_blocks {
                    for child in rows.iter_mut().flatten().flatten() {
                        omit_picture_payloads(child, images);
                    }
                }
            }
            Node::Commented { inner, .. }
            | Node::Located { inner, .. }
            | Node::Prov { inner, .. }
            | Node::Furniture { inner, .. }
            | Node::DoclangOnly(inner) => omit_picture_payloads(inner, images),
            _ => {}
        }
    }

    fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
        serde_json::to_writer_pretty(File::create(path)?, value)?;
        Ok(())
    }

    #[cfg(feature = "docling")]
    fn capture_docling_nodes(pdf: &Path, output: &Path) -> Result<()> {
        let bytes = fs::read(pdf)?;
        // A separate, explicitly identified upstream run with the same
        // production settings; this is not substituted into pipeline scores.
        let mut upstream = docling_pdf::Pipeline::new()?
            .ocr_engine(Some(docling_pdf::OcrEngine::PpOcr))
            .no_ocr(false)
            .no_table_former(true)
            .force_full_page_ocr(false);
        upstream.set_pages(None);
        let mut document = upstream.convert(&bytes, None, "doc")?;
        let mut images = Vec::new();
        for node in &mut document.nodes {
            omit_picture_payloads(node, &mut images);
        }
        write_json(
            &output.join("upstream-picture-payload-hashes.json"),
            &images,
        )?;
        fs::write(
            output.join("upstream-docling-nodes.txt"),
            format!("{:#?}\n", document.nodes),
        )?;
        write_json(
            &output.join("upstream-docling-config.json"),
            &json!({
                "version": "1.69.2", "ocr_engine": "ppocr", "ocr": true,
                "table_former": false, "force_full_page_ocr": false,
                "pages": null, "password": null, "document_name": "doc",
                "scope": "separate upstream-node diagnostic pass; not the scored pass",
                "picture_payloads": "data arrays omitted; sizes/hashes retained in upstream-picture-payload-hashes.json; text, node types and coordinates unchanged",
            }),
        )?;
        Ok(())
    }

    #[cfg(not(feature = "docling"))]
    fn capture_docling_nodes(_pdf: &Path, _output: &Path) -> Result<()> {
        anyhow::bail!("full Docling node capture requires --features docling")
    }

    pub fn run() -> Result<()> {
        let arguments: Vec<String> = std::env::args().skip(1).collect();
        ensure!(
            arguments.len() == 5,
            "usage: bibliography_evidence MANIFEST CACHE BACKEND PAPER_ID OUTPUT"
        );
        let manifest = corpus::load_manifest(Path::new(&arguments[0]))?;
        let cache = Path::new(&arguments[1]);
        let backend_name = &arguments[2];
        ensure!(
            backend_name != "docling" || cfg!(feature = "docling"),
            "full Docling diagnostics require --features docling"
        );
        let id = &arguments[3];
        let output = Path::new(&arguments[4]);
        ensure!(!output.exists(), "output directory must not exist");
        fs::create_dir_all(output)?;
        let item = manifest
            .items
            .iter()
            .find(|item| &item.id == id)
            .context("paper is absent from the selected manifest")?;
        // Network acquisition belongs to the existing corpus-fetch command.
        // Cached PDF/source hashes are still reverified by fetch_item offline.
        let fetched = corpus::fetch_item(item, cache, "bibliography-evidence", true)?;
        let source = fetched.source_dir.as_deref().context("no LaTeX source")?;
        let truth = latex_refs::ground_truth(&corpus::find_latex_files(source)?)?;
        let job = schema::Job {
            path: fetched.pdf_path.to_string_lossy().into_owned(),
            backend: backend_name.clone(),
            pages: None,
            password: None,
            max_bytes: None,
            figures_dir: None,
        };
        let result = pipeline::run_job(&job)?;
        let score = eval::evaluate(id, &result, &truth);
        write_json(&output.join("result.json"), &result)?;
        write_json(&output.join("score.json"), &score)?;
        write_json(
            &output.join("dump.json"),
            &eval::dump_paper(id, &result, &truth, &score),
        )?;
        let extractor = backend::by_name(backend_name).context("backend unavailable")?;
        let mut ordered = Vec::with_capacity(result.pages.len());
        for page in &result.pages {
            let mut raw = schema::PageText::new(page.page, page.width, page.height, page.rotation);
            raw.spans.clone_from(&page.spans);
            if extractor.provides_reading_order() {
                reading_order::lines_in_backend_order(&mut raw);
            } else {
                reading_order::order_page(&mut raw);
            }
            ordered.push(raw);
        }
        write_json(&output.join("order-only.json"), &ordered)?;
        write_json(
            &output.join("provenance.json"),
            &json!({
                "paper": id,
                "pdf_sha256": fetched.pdf_sha256,
                "source_sha256": fetched.source_sha256,
                "backend": result.backend,
                "status": result.status,
                "original_spans": "result.json pages[].spans: retained backend evidence",
                "ordered_projection": "order-only.json: recomputed from retained spans, before cleanup/tagging",
                "final_lines": "result.json pages[].lines: actual full-pipeline output",
                "truth_method": truth.method,
                "truth_reference_count": truth.references.len(),
            }),
        )?;
        if backend_name == "docling" {
            capture_docling_nodes(&fetched.pdf_path, output)?;
        }
        Ok(())
    }
}

#[cfg(any(feature = "pdfium", feature = "docling-text"))]
fn main() -> anyhow::Result<()> {
    enabled::run()
}

#[cfg(not(any(feature = "pdfium", feature = "docling-text")))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("bibliography_evidence requires --features pdfium or docling-text")
}

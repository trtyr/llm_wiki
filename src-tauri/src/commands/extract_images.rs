//! Image extraction from PDF / PPTX / DOCX (Phase 1 of the multimodal
//! pipeline; see plans/multimodal-images.md).
//!
//! NO LLM calls happen in this module. Output is the raw extracted
//! images as PNG-encoded base64 strings, ready for either:
//!   - the vision-caption helper (Phase 3) which sends them to a VLM
//!   - direct write-to-disk in `wiki/media/<source-slug>/`
//!
//! This module is intentionally separate from `fs.rs` (which already
//! has its own pdfium binding lifecycle for text extraction). PDF
//! image extraction reuses the same global `Pdfium` instance via the
//! `pdfium()` helper exposed by `fs.rs`.
//!
//! Outputs are deterministic for a given input file (same image
//! ordering, same `index` per image), so the dedup cache in Phase 3
//! can key purely on the SHA-256 of `data_base64`.

use std::path::Path;

const MIN_FULL_PAGE_SKIP_RATIO: f32 = 0.5;
const MAX_FULL_PAGE_SKIP_RATIO: f32 = 1.0;

/// Filter knobs. The defaults mirror what's documented in
/// plans/multimodal-images.md; callers (the TS layer wiring this up)
/// will eventually surface them in Settings.
#[derive(Debug, Clone)]
pub struct ExtractOptions {
    /// Skip images smaller than this on EITHER axis. 100×100 catches
    /// the vast majority of icons / logos / page-corner decorations
    /// without dropping legitimate small chart insets.
    pub min_width: u32,
    pub min_height: u32,
    /// Hard cap on the number of images returned per document. A
    /// pathological 5000-image PDF would otherwise blow up memory
    /// (each image base64'd is ~MB-scale) AND blow up downstream VLM
    /// cost during Phase 3.
    pub max_images: usize,
    /// Skip raster image objects that cover most of a PDF page when the page
    /// already has meaningful extractable text. Pure scanned pages, and scans
    /// with only a page number or watermark text layer, keep their full-page
    /// raster because that image is the only reliable content. Searchable OCR
    /// scans with a full text layer may be treated as text pages; this is an
    /// intentional default tradeoff to avoid importing every PDF page as a
    /// large duplicate PNG.
    pub skip_full_page_images: bool,
    /// Fraction of page width and height an image must cover to be considered
    /// page-sized. Values are clamped to 0.5..=1.0 before use.
    pub full_page_coverage_ratio: f32,
    /// Minimum non-whitespace text characters before a page is treated as a
    /// text page for page-sized image filtering.
    pub full_page_min_text_chars: usize,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            min_width: 100,
            min_height: 100,
            max_images: 500,
            skip_full_page_images: true,
            full_page_coverage_ratio: 0.90,
            full_page_min_text_chars: 80,
        }
    }
}

fn is_full_page_image_bounds(
    object_width: f32,
    object_height: f32,
    page_width: f32,
    page_height: f32,
    coverage_ratio: f32,
) -> bool {
    if object_width <= 0.0 || object_height <= 0.0 || page_width <= 0.0 || page_height <= 0.0 {
        return false;
    }
    let ratio = coverage_ratio.clamp(MIN_FULL_PAGE_SKIP_RATIO, MAX_FULL_PAGE_SKIP_RATIO);
    object_width / page_width >= ratio && object_height / page_height >= ratio
}

fn has_meaningful_page_text(text: &str, min_chars: usize) -> bool {
    text.chars().filter(|c| !c.is_whitespace()).count() >= min_chars
}

fn should_skip_full_page_pdf_image(
    object: &pdfium_render::prelude::PdfPageObject<'_>,
    page_width: f32,
    page_height: f32,
    page_has_meaningful_text: bool,
    options: &ExtractOptions,
) -> bool {
    should_skip_full_page_image_decision(
        options.skip_full_page_images,
        page_has_meaningful_text,
        is_full_page_pdf_image_object(
            object,
            page_width,
            page_height,
            options.full_page_coverage_ratio,
        ),
    )
}

// ── PDF (pdfium) ────────────────────────────────────────────────────────

fn is_full_page_pdf_image_object(
    object: &pdfium_render::prelude::PdfPageObject<'_>,
    page_width: f32,
    page_height: f32,
    coverage_ratio: f32,
) -> bool {
    use pdfium_render::prelude::PdfPageObjectCommon;

    object
        .bounds()
        .map(|bounds| {
            is_full_page_image_bounds(
                bounds.width().value,
                bounds.height().value,
                page_width,
                page_height,
                coverage_ratio,
            )
        })
        .unwrap_or(false)
}

fn should_skip_full_page_image_decision(
    skip_enabled: bool,
    page_has_meaningful_text: bool,
    image_is_full_page: bool,
) -> bool {
    skip_enabled && page_has_meaningful_text && image_is_full_page
}

/// Combined PDF text + image extraction in a single pdfium session.
///
/// Output is a markdown string with `## Page N` headers, the page's
/// extracted text, and `![](url)` references to images embedded on
/// that page — interleaved per-page so the document reads top-to-
/// bottom the way the source did.
///
/// When `media_dest_dir` is `Some`, every embedded raster image
/// passing the size filter is written to that directory as
/// `img-<N>.png` (1-based across the whole document) and referenced
/// in the markdown via `media_url_prefix + "/img-<N>.png"`. Pass an
/// absolute path as the prefix when you want the markdown to render
/// regardless of where the file is opened (this is what the raw-
/// source preview wants — see `extract_pdf_text` in fs.rs).
///
/// When `media_dest_dir` is `None`, image objects are skipped
/// entirely and the output is text + page headers only — useful for
/// PDFs outside the project's `raw/sources/` layout where there's no
/// stable place to land the image files.
///
/// Holds the global pdfium lock for its full duration. Callers MUST
/// NOT acquire the lock themselves before calling this (would
/// deadlock — `std::sync::Mutex` is non-reentrant).
pub fn extract_pdf_markdown(
    path: &str,
    media_dest_dir: Option<&Path>,
    media_url_prefix: &str,
    options: &ExtractOptions,
) -> Result<String, String> {
    use pdfium_render::prelude::*;

    let _guard = crate::commands::fs::lock_pdfium();
    let pdfium = crate::commands::fs::pdfium()?;
    let doc = pdfium.load_pdf_from_file(path, None).map_err(|e| match e {
        PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => {
            format!("PDF is password-protected and cannot be read: '{path}'")
        }
        _ => format!("Failed to open PDF '{path}': {e}"),
    })?;

    let mut out = String::new();
    let mut idx: u32 = 0;
    let mut total_saved: u32 = 0;
    // Strip a single trailing slash from the prefix so we can always
    // emit `prefix + "/" + name` without producing `path//name`.
    let prefix = media_url_prefix.trim_end_matches('/');

    let page_count = doc.pages().len();
    if media_dest_dir.is_some() {
        eprintln!(
            "[extract_pdf_markdown] '{path}': {page_count} page(s), images→{:?}",
            media_dest_dir.map(|d| d.display().to_string())
        );
    }

    for (page_idx, page) in doc.pages().iter().enumerate() {
        let page_num = page_idx + 1;
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("## Page {page_num}\n\n"));

        let page_text = page
            .text()
            .map_err(|e| format!("Page {page_num} text extraction failed in '{path}': {e}"))?;
        let page_text_content = page_text.all();
        let page_has_meaningful_text =
            has_meaningful_page_text(&page_text_content, options.full_page_min_text_chars);
        out.push_str(&page_text_content);
        // Single trailing newline so the next block starts on its own
        // line; the `\n\n` separator before the next `## Page` heading
        // gets prepended by the loop entry above.
        out.push('\n');

        // Skip image extraction when the caller didn't supply a
        // destination — no point burning pdfium cycles to throw the
        // pixels away.
        let dest_dir = match media_dest_dir {
            Some(d) => d,
            None => continue,
        };

        let mut page_image_md: Vec<String> = Vec::new();
        for object in page.objects().iter() {
            let image = match object.as_image_object() {
                Some(img) => img,
                None => continue,
            };
            if should_skip_full_page_pdf_image(
                &object,
                page.width().value,
                page.height().value,
                page_has_meaningful_text,
                options,
            ) {
                continue;
            }
            let dyn_img = match image.get_raw_image() {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[extract_pdf_markdown] page {page_num} image read failed: {e}");
                    continue;
                }
            };
            let width = dyn_img.width();
            let height = dyn_img.height();
            if width < options.min_width || height < options.min_height {
                continue;
            }
            let mut png_bytes: Vec<u8> = Vec::new();
            if let Err(e) = dyn_img.write_to(
                &mut std::io::Cursor::new(&mut png_bytes),
                image::ImageFormat::Png,
            ) {
                eprintln!("[extract_pdf_markdown] page {page_num} PNG encode failed: {e}");
                continue;
            }
            idx += 1;
            let file_name = format!("img-{idx}.png");
            // `dest_dir_relative_to` is unused here (we don't need a
            // rel_path return — the markdown uses media_url_prefix);
            // pass dest_dir for both args so save_one_image's
            // strip_prefix is a no-op.
            if let Err(e) = save_one_image(&png_bytes, dest_dir, dest_dir, &file_name) {
                eprintln!("[extract_pdf_markdown] page {page_num} save failed: {e}");
                continue;
            }
            total_saved += 1;
            // Empty alt-text on purpose: until the vision-caption
            // helper lands (Phase 3a) we have nothing meaningful to
            // put there, and a placeholder like "image" or the file
            // name only adds noise to the LLM and to screen readers.
            page_image_md.push(format!("![]({prefix}/{file_name})"));
            if total_saved as usize >= options.max_images {
                eprintln!(
                    "[extract_pdf_markdown] reached max_images={} cap; skipped rest",
                    options.max_images
                );
                break;
            }
        }
        if !page_image_md.is_empty() {
            out.push('\n');
            for img_md in &page_image_md {
                out.push_str(img_md);
                out.push('\n');
            }
        }
        if total_saved as usize >= options.max_images {
            break;
        }
    }

    if media_dest_dir.is_some() {
        eprintln!("[extract_pdf_markdown] '{path}' DONE — pages={page_count}, saved={total_saved}");
    }

    Ok(out)
}

// ── PPTX / DOCX (zip) ──────────────────────────────────────────────────

// ── Extract-and-save: write to disk, skip the base64 round-trip ────────
//
// The base64-returning commands above are useful for future
// captioning paths that need bytes in-memory. The ingest pipeline
// just wants to land images on disk and reference them from
// markdown — the round-trip via JS is wasteful (5 MB image → 6.7 MB
// base64 string → JS allocates → JS calls back to Rust → decode →
// write). The functions below short-circuit by writing directly.

fn save_one_image(
    bytes: &[u8],
    dest_dir: &Path,
    dest_dir_relative_to: &Path,
    file_name: &str,
) -> Result<(String, String), String> {
    if !dest_dir.exists() {
        std::fs::create_dir_all(dest_dir)
            .map_err(|e| format!("create_dir_all '{}': {e}", dest_dir.display()))?;
    }
    let abs = dest_dir.join(file_name);
    std::fs::write(&abs, bytes).map_err(|e| format!("write '{}': {e}", abs.display()))?;

    let rel = abs
        .strip_prefix(dest_dir_relative_to)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| file_name.to_string());
    Ok((rel, abs.to_string_lossy().to_string()))
}

fn ext_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        _ => "bin",
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_options_defaults_match_plan() {
        // Plan documents 100×100 / 500 as the v1 defaults. Pinning
        // these here so a casual change is visible.
        let o = ExtractOptions::default();
        assert_eq!(o.min_width, 100);
        assert_eq!(o.min_height, 100);
        assert_eq!(o.max_images, 500);
        assert!(o.skip_full_page_images);
        assert_eq!(o.full_page_coverage_ratio, 0.90);
        assert_eq!(o.full_page_min_text_chars, 80);
    }

    #[test]
    fn full_page_image_bounds_detects_page_scans() {
        assert!(is_full_page_image_bounds(
            900.0, 900.0, 1000.0, 1000.0, 0.90
        ));
        assert!(is_full_page_image_bounds(
            950.0, 910.0, 1000.0, 1000.0, 0.90
        ));
        assert!(!is_full_page_image_bounds(
            899.0, 1000.0, 1000.0, 1000.0, 0.90
        ));
        assert!(!is_full_page_image_bounds(
            1000.0, 899.0, 1000.0, 1000.0, 0.90
        ));
        assert!(!is_full_page_image_bounds(
            400.0, 300.0, 1000.0, 1000.0, 0.90
        ));
        assert!(!is_full_page_image_bounds(900.0, 900.0, 0.0, 1000.0, 0.90));
        assert!(is_full_page_image_bounds(500.0, 500.0, 1000.0, 1000.0, 0.0));
        assert!(is_full_page_image_bounds(
            500.0, 500.0, 1000.0, 1000.0, -1.0
        ));
        assert!(!is_full_page_image_bounds(
            999.0, 1000.0, 1000.0, 1000.0, 1.5
        ));
    }

    #[test]
    fn meaningful_page_text_ignores_short_stamp_layers() {
        assert!(!has_meaningful_page_text(" 12 \n", 80));
        assert!(!has_meaningful_page_text("confidential watermark", 80));
        assert!(has_meaningful_page_text(
            "This page has enough extracted words to be treated as a real text page rather than a scanned page with only a short OCR stamp.",
            80
        ));
    }

    #[test]
    fn full_page_skip_decision_requires_all_conditions() {
        assert!(should_skip_full_page_image_decision(true, true, true));
        assert!(!should_skip_full_page_image_decision(false, true, true));
        assert!(!should_skip_full_page_image_decision(true, false, true));
        assert!(!should_skip_full_page_image_decision(true, true, false));
        assert!(!should_skip_full_page_image_decision(false, false, false));
    }
}

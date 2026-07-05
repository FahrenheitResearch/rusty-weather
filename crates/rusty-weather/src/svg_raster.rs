//! Rasterize a server-generated SVG card to PNG for shareable / copyable
//! images.
//!
//! The on-screen path stays crisp vector SVG — this is only used when a client
//! asks for `?format=png` (e.g. a "share"/"open image" link). SVG is great in
//! the page but a raw SVG document can't be copied as a picture (a drag-select
//! grabs the `<text>` elements), so the shared artifact needs to be a raster.
//!
//! Font fidelity: the cards name `Inter` (daily) and `IBM Plex Mono`
//! (meteograms). Those must be present on the box —
//! `apt install fonts-inter fonts-ibm-plex fonts-dejavu-core` — so
//! `load_system_fonts()` picks them up and the PNG matches the intended
//! design. `fonts-dejavu-core` is a broad-coverage fallback for stray glyphs
//! (e.g. the `▸` extended-range marker) the primary faces may lack.

use std::sync::{Arc, OnceLock};

use resvg::tiny_skia;
use resvg::usvg::{self, fontdb};

/// Loaded once per process. Cloning the returned `Arc` is cheap; each request
/// reuses the same database rather than re-scanning the font directories.
static FONTDB: OnceLock<Arc<fontdb::Database>> = OnceLock::new();

fn shared_fontdb() -> Arc<fontdb::Database> {
    FONTDB
        .get_or_init(|| {
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            // Resolve the generic keywords our stacks fall back to.
            db.set_sans_serif_family("Inter");
            db.set_serif_family("Inter");
            db.set_monospace_family("IBM Plex Mono");
            Arc::new(db)
        })
        .clone()
}

/// Render `svg` to PNG bytes at `scale`× its intrinsic size. 2× gives a crisp,
/// retina-quality share image without blowing up the byte size (the cards are
/// mostly flat fills, which PNG compresses well).
pub fn svg_to_png(svg: &str, scale: f32) -> Result<Vec<u8>, String> {
    let mut opt = usvg::Options::default();
    opt.fontdb = shared_fontdb();
    // Only used if an element ever carries no font-family; our cards always set
    // one, but this keeps text visible rather than falling back to a face that
    // may not exist on the box.
    opt.font_family = "Inter".to_string();

    let tree = usvg::Tree::from_str(svg, &opt).map_err(|err| format!("parse svg: {err}"))?;
    let size = tree.size();
    let scale = scale.clamp(1.0, 4.0);
    let width = (size.width() * scale).round().max(1.0) as u32;
    let height = (size.height() * scale).round().max(1.0) as u32;
    let mut pixmap =
        tiny_skia::Pixmap::new(width, height).ok_or_else(|| "allocate pixmap".to_string())?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap.encode_png().map_err(|err| format!("encode png: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_minimal_svg_to_png() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20" viewBox="0 0 40 20"><rect width="40" height="20" fill="#123456"/><text x="4" y="14" font-size="10" fill="#ffffff">hi</text></svg>"##;
        let png = svg_to_png(svg, 2.0).expect("render minimal svg");
        // A 80x40 PNG with content is comfortably over 100 bytes.
        assert!(png.len() > 100, "png suspiciously small: {} bytes", png.len());
        // PNG signature.
        assert_eq!(
            &png[0..8],
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
            "output is not a PNG"
        );
    }
}

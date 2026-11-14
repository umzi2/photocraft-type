//! Type layers read from a file, made ready for the canvas and the panels.

use photocraft_doc::{Document, Layer, LayerContent, TextLayer};

/// Largest type raster the import draws on its own (pixels). The model comes from an untrusted
/// file: an absurd type size, scale or box must not make opening it allocate gigabytes (the
/// renderer's own guard is far larger). A layer over it keeps empty pixels, as before, until
/// it is edited.
pub(crate) const MAX_RENDER_PIXELS: u64 = 64 * 1024 * 1024;

/// Renders `t` from its model unless its ink would exceed [`MAX_RENDER_PIXELS`].
fn render_bounded(eng: &mut photocraft_text::TextEngine, t: &mut TextLayer, dpi: f32, format: photocraft_color::PixelFormat) {
    let layout = eng.layout(t, dpi);
    let warp = photocraft_text::render::layout_deform(&layout, t);
    let ink = photocraft_text::render::ink_rect_warped(&layout, &t.transform, warp.as_ref());
    if u64::from(ink.width()).saturating_mul(u64::from(ink.height())) > MAX_RENDER_PIXELS {
        t.cache = Some(photocraft_raster::Surface::new(photocraft_color::PixelFormat { alpha: true, ..format }));
        return;
    }
    let r = photocraft_text::render::rasterize_warped(&layout, &t.transform, format, t.antialias, warp.as_ref());
    t.cache = Some(r.surface);
}

/// Prepares the type layers of a document that was just imported:
/// - a run whose font came as a PostScript name (PSD files) gets the installed family that name
///   resolves to, as the layout does, so Missing Fonts and the Character panel see the real
///   family (`CCWildWords-Regular` is the family "CCWildWords", not the guessed "CC Wild Words");
/// - a type layer without pixels (ag-psd, GIMP and other writers leave the image data out and
///   let Photoshop re-render on open) is rendered from its model, instead of staying empty
///   until it is edited;
/// - a type layer whose pixels were composed with the old cap-height first baseline, while the
///   document's `Txt2` frames make Photoshop compose it with the default **Ascent** (a file
///   written by a PhotoCraft build from before the frame rule), is re-composed from the model
///   with the document's `Txt2` block. The recompose command applies the same render; without
///   this an open would keep the stale pixels and sit 1-3 px above Photoshop's composition.
pub(crate) fn prepare(doc: &mut Document) {
    fn visit(layers: &mut [Layer], eng: &mut photocraft_text::TextEngine, dpi: f32, format: photocraft_color::PixelFormat) {
        for l in layers {
            match &mut l.content {
                LayerContent::Text(t) => {
                    let mut changed = false;
                    for r in &mut t.runs {
                        if let Some(ps) = r.style.postscript_name.clone()
                            && !eng.fonts.faces(&r.style.font_family).iter().any(|f| f.postscript_name.as_deref() == Some(&ps))
                        {
                            let f = eng.fonts.resolve_postscript(&ps);
                            // Keep installed families, including legacy names whose faces are
                            // indexed under a typographic family. An exact face may recover a
                            // truncated hyphenated family; a heuristic only fills a missing one.
                            let installed = eng.fonts.has_family(&r.style.font_family);
                            let exact_recovery =
                                f.exact && (!installed || f.family.strip_prefix(&r.style.font_family).is_some_and(|suffix| suffix.starts_with('-')));
                            if exact_recovery || (!installed && eng.fonts.has_family(&f.family)) {
                                r.style.font_family = f.family;
                                r.style.weight = f.weight;
                                r.style.italic = f.italic;
                                changed = true;
                            }
                        }
                    }
                    if changed {
                        t.sync_summary();
                    }
                    if t.cache.is_none() {
                        render_bounded(eng, t, dpi, format);
                    }
                }
                LayerContent::Group(g) => visit(&mut g.children, eng, dpi, format),
                _ => {}
            }
        }
    }
    let dpi = doc.resolution_dpi;
    let format = doc.pixel_format();
    {
        let mut eng = photocraft_text::shared().lock().unwrap_or_else(|e| e.into_inner());
        visit(&mut doc.layers, &mut eng, dpi, format);
    }
    // After the shared engine's lock: the render takes the reimplementation's own font store,
    // and its layout falls back to the shared engine's when it does not model the layer.
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(txt2) = doc.metadata.psd_global_blocks.iter().find(|(_, key, _)| key == b"Txt2").map(|(_, _, data)| data.clone()) {
        recompose_default_ascent(&mut doc.layers, dpi, format, &txt2);
    }
}

/// Re-composes every type layer of an imported document that Photoshop would lay out with the
/// `Txt2` frame's default **Ascent** first baseline (see
/// [`photocraft_text::render_re::txt2_uses_default_ascent`]).
///
/// This is the render the recompose command (`type.updateAllTextLayers`) runs on such a layer:
/// the cache is rebuilt with the document's `Txt2` block and, because the Ascent path drops the
/// stored rectangle, the layer's rectangle comes from the ink — exactly what Photoshop's own
/// recompose does. A layer the reimplementation cannot render (a path, a colour mode it does not
/// model, a font that is not installed) keeps the file's pixels.
#[cfg(not(target_arch = "wasm32"))]
fn recompose_default_ascent(layers: &mut [Layer], dpi: f32, format: photocraft_color::PixelFormat, txt2: &[u8]) {
    for l in layers {
        match &mut l.content {
            LayerContent::Text(t) => {
                if !photocraft_text::render_re::txt2_uses_default_ascent(t, Some(txt2)) {
                    continue;
                }
                // `None`: the stored rectangle is the cache's old composition and must not clip
                // the lower line rows (the Ascent branch of `render_layer_into` drops it too).
                let Ok((surface, _rect)) = photocraft_text::render_re::render_layer_into(t, dpi, format, Some(txt2), None) else {
                    continue;
                };
                // The `TySh` descriptor keeps the layer's text-space ink bounds, taken from the
                // layout the layer is now drawn with, so the descriptor and the pixels cannot
                // disagree (the recompose command does the same).
                let layout = photocraft_text::layer_layout(t, dpi, format);
                t.cache = Some(surface);
                t.psd_raw = Some(std::sync::Arc::new(photocraft_text::psd::build_tysh(t, dpi, layout.bounds())));
            }
            LayerContent::Group(g) => recompose_default_ascent(&mut g.children, dpi, format, txt2),
            _ => {}
        }
    }
}

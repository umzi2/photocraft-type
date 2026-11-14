use super::{Affine, Arc, Document, LayerContent, LayerId, PhotocraftApp, Point, PoisonError, TextLayer, TextLayout, text_layer};

/// Layout of a type layer (cached per document revision) and its text → document transform.
pub fn layout(app: &mut PhotocraftApp, id: LayerId) -> Option<(Arc<TextLayout>, Affine, String)> {
    let dragged = crate::type_transform::current_transform(app, id);
    // Borrows the document in place (no clone): the canvas hover hit-tests type every frame.
    let st = app.session.active()?;
    let t = text_layer(&st.doc, id)?;
    let transform = dragged.unwrap_or(t.transform);
    let key = (st.doc.id.0, st.revision, id.0);
    if let Some((k, l)) = &app.type_layout
        && *k == key
    {
        return Some((l.clone(), transform, t.text.clone()));
    }
    // The canvas draws type through the reimplemented engine, so caret, selection and line
    // boxes have to use that engine's layout: parley breaks paragraphs differently, which puts
    // the caret on glyphs that were never drawn. Layers the engine declines (vertical, warped,
    // path, transformed, non-RGB) keep the built-in layout, because their pixels are its work.
    // The Type tool asks for the layout several times per frame (overlay, hit testing, IME,
    // selection) and the hover hit test probes every text layer under the pointer, which used
    // to evict this one-slot cache each time. A small per-thread memo keyed by the same
    // (document, revision, layer) plus the DPI, colour mode and text keeps one engine layout
    // per layer and revision.
    let dpi = st.doc.resolution_dpi;
    let format = st.doc.pixel_format();
    let memo = (key.0, key.1, key.2, dpi.to_bits(), format.mode == photocraft_color::ColorMode::Rgb, text_hash(&t.text));
    if let Some(l) = cached_layout(memo) {
        app.type_layout = Some((key, l.clone()));
        return Some((l, transform, t.text.clone()));
    }
    let l = Arc::new(match engine_layout(t, dpi, format) {
        Ok(layout) => layout,
        Err(why) => {
            trace_fallback(t, &why);
            photocraft_text::shared().lock().ok()?.layout(t, dpi)
        }
    });
    remember_layout(memo, &l);
    app.type_layout = Some((key, l.clone()));
    Some((l, transform, t.text.clone()))
}

/// Cache key of a sized layout: document, revision, layer, DPI, whether the document is RGB,
/// and a hash of the layer text (a cheap guard against a stale revision).
type LayoutKey = (u64, u64, u64, u32, bool, u64);

/// Enough for every text layer of a showcase document at once, so hovering over them does not
/// evict the layer being edited.
const LAYOUT_CACHE_ENTRIES: usize = 48;

thread_local! {
    static LAYOUTS: std::cell::RefCell<Vec<(LayoutKey, Arc<TextLayout>)>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn cached_layout(key: LayoutKey) -> Option<Arc<TextLayout>> {
    LAYOUTS.with(|cache| {
        let mut cache = cache.borrow_mut();
        let index = cache.iter().position(|(k, _)| *k == key)?;
        let entry = cache.remove(index);
        let layout = entry.1.clone();
        cache.insert(0, entry); // most recently used first
        Some(layout)
    })
}

fn remember_layout(key: LayoutKey, layout: &Arc<TextLayout>) {
    LAYOUTS.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.retain(|(k, _)| *k != key);
        cache.insert(0, (key, Arc::clone(layout)));
        cache.truncate(LAYOUT_CACHE_ENTRIES);
    });
}

fn text_hash(text: &str) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// The layer's layout from the reimplemented Photoshop engine (the one the canvas draws), or
/// why the built-in layout has to be used instead.
#[cfg(not(target_arch = "wasm32"))]
fn engine_layout(t: &TextLayer, dpi: f32, format: photocraft_color::PixelFormat) -> Result<TextLayout, String> {
    // An escape hatch for triage and for a machine whose fonts make the engine path unusable:
    // `PHOTOCRAFT_TEXT_LAYOUT_RE=0` keeps the built-in layout everywhere (the caret then follows
    // the built-in renderer, so it is only for diagnosis, not for the shipped app).
    if std::env::var_os("PHOTOCRAFT_TEXT_LAYOUT_RE").is_some_and(|v| v == "0" || v == "off") {
        return Err("layout_re disabled by PHOTOCRAFT_TEXT_LAYOUT_RE".into());
    }
    // The engine's font store is scanned once and takes a noticeable part of a second; launch
    // starts the scan in the background, and this is a no-op when it did.
    photocraft_text::layout_re::warm_async();
    let start = std::time::Instant::now();
    let result = photocraft_text::layout_re::layout_layer(t, dpi, format).map_err(|why| why.to_string());
    if photocraft_text::layout_re::debug_enabled() {
        let shape = match &t.shape {
            photocraft_doc::text::TextShape::Point => "point",
            photocraft_doc::text::TextShape::Box { .. } => "box",
            photocraft_doc::text::TextShape::Area { .. } => "area",
            _ => "other",
        };
        let outcome = match &result {
            Ok(layout) => format!("{} lines", layout.lines.len()),
            Err(why) => format!("refused: {why}"),
        };
        let preview: String = t.text.chars().take(20).collect();
        log::info!(
            target: "photocraft_ui_egui::type_layout",
            "layout_re: {:.2} ms {shape} {outcome} {preview:?}",
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
    result
}

/// The engine reads its fonts from the filesystem; the web build keeps the built-in layout.
#[cfg(target_arch = "wasm32")]
fn engine_layout(_t: &TextLayer, _dpi: f32, _format: photocraft_color::PixelFormat) -> Result<TextLayout, String> {
    Err("the Photoshop engine layout is native-only".into())
}

/// `PHOTOCRAFT_DEBUG_LAYOUT_RE=1` names the layers that still use the built-in layout.
fn trace_fallback(t: &TextLayer, why: &str) {
    #[cfg(not(target_arch = "wasm32"))]
    if photocraft_text::layout_re::debug_enabled() {
        let preview: String = t.text.chars().take(24).collect();
        log::info!(target: "photocraft_ui_egui::type_layout", "layout: built-in layout for {preview:?}: {why}");
    }
    #[cfg(target_arch = "wasm32")]
    let _ = (t, why);
}

pub(super) fn to_text(aff: &Affine, x: f64, y: f64) -> (f32, f32) {
    let p = aff.inverse().unwrap_or(Affine::IDENTITY).apply(Point::new(x, y));
    (p.x as f32, p.y as f32)
}

/// Topmost shown type layer (it and every group around it visible, #4320) whose laid-out text
/// contains the document point. The press and the canvas hover (`hover_path_target`) share it, so
/// the pointer never promises a path click that the press would turn into editing this layer.
pub(crate) fn hit_layer(app: &mut PhotocraftApp, x: f64, y: f64) -> Option<LayerId> {
    let slop = 6.0 / app.point_zoom().max(0.01);
    // A Type Mask session's text is a hidden layer; it is still the text being edited.
    let editing = app.ui.text_edit.as_ref().filter(|ed| ed.mask).map(|ed| LayerId(ed.layer));
    let mut ids: Vec<LayerId> = {
        let doc = &app.session.active()?.doc;
        doc.walk()
            .into_iter()
            .filter(|(path, _, l)| matches!(l.content, LayerContent::Text(_)) && (editing == Some(l.id) || doc.shown_at(path)))
            .map(|(_, _, l)| l.id)
            .collect()
    };
    ids.reverse(); // walk() is bottom-up; hit the topmost first
    ids.into_iter().find(|id| {
        // The pixels shown count too: a PSD's type keeps Photoshop's rendering until edited,
        // which can sit off our layout when fonts are substituted (#123).
        let shown = app.session.active().and_then(|st| text_layer(&st.doc, *id)).and_then(|t| t.cache.as_ref()).map(|c| c.content_bounds()).is_some_and(|r| {
            let s = f64::from(slop);
            !r.is_empty() && x >= f64::from(r.x0) - s && x <= f64::from(r.x1) + s && y >= f64::from(r.y0) - s && y <= f64::from(r.y1) + s
        });
        // The hover calls this every frame: skip the layout when the pixels already answer.
        if shown {
            return true;
        }
        let Some((l, aff, _)) = layout(app, *id) else { return false };
        let (tx, ty) = to_text(&aff, x, y);
        photocraft_text::text_point_inside(&l, tx, ty, slop)
    })
}

/// True when the layer's pixels are what our engine draws for it. A PSD's type layer keeps
/// Photoshop's pixels until it is edited, and with substituted fonts or a different line layout
/// they sit somewhere else than the glyphs the caret and selection are placed on.
///
/// The comparison uses the renderer the canvas and the edit use — the reimplemented Photoshop
/// engine when it models the layer, the built-in one otherwise — and compares the pixels, not
/// only their bounds: a PSD's stored raster for a paragraph layer can have the same ink bounds as
/// our render while its line breaks and advances differ (the layer would then be "edited" as a
/// no-op and the caret would sit on glyphs that are not the ones on screen). A cache this
/// renderer made is byte for byte its output, so an already-ours layer is not re-rendered (and
/// not moved: the two renderers' ink bounds differ by a pixel or more).
pub(crate) fn shows_own_layout(doc: &Document, t: &TextLayer) -> bool {
    let Some(cache) = &t.cache else { return false };
    let have = cache.content_bounds();
    if have.is_empty() {
        return false;
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        // The Txt2 block is what `refresh_at` hands the rasteriser, so the comparison uses it
        // too; without it a PSD's own paragraph values would render differently.
        let txt2 = photocraft_engine::type_cmds::txt2_block(doc);
        if let Ok((surface, _)) =
            photocraft_text::render_re::render_layer_with_txt2(t, doc.resolution_dpi, doc.pixel_format(), txt2.as_deref().map(Vec::as_slice))
        {
            return same_render(&surface, cache, have);
        }
    }
    let mut eng = photocraft_text::shared().lock().unwrap_or_else(PoisonError::into_inner);
    let ours = eng.render(t, doc.resolution_dpi, doc.pixel_format()).1.surface;
    same_render(&ours, cache, have)
}

/// Whether `cache` holds exactly what `render` drew over their common ink rectangle. Bounds alone
/// are not enough: a PSD's stored raster can share them with a different layout.
fn same_render(render: &photocraft_raster::Surface, cache: &photocraft_raster::Surface, ink: photocraft_geom::Rect) -> bool {
    render.content_bounds() == ink && cache.content_bounds() == ink && render.read_region(ink) == cache.read_region(ink)
}

pub(super) fn hit_offset(app: &mut PhotocraftApp, id: LayerId, x: f64, y: f64) -> usize {
    offset_at(app, id, x, y)
}

/// The character offset of the caret position nearest document point (`x`, `y`) in layer `id`.
pub(crate) fn offset_at(app: &mut PhotocraftApp, id: LayerId, x: f64, y: f64) -> usize {
    let Some((l, aff, text)) = layout(app, id) else { return 0 };
    let (tx, ty) = to_text(&aff, x, y);
    // Type on a path: the point's place along the path is the position in the (single) line.
    let shape = app.session.active().and_then(|s| text_layer(&s.doc, id).map(|t| t.shape.clone()));
    let text_start = l.line_bounds().map_or(0.0, |bounds| f64::from(bounds[0]));
    let text_end = l.line_bounds().map_or(0.0, |bounds| f64::from(bounds[2]));
    let (tx, ty) = shape
        .as_ref()
        .and_then(|shape| photocraft_text::warp::Warp::on_path_oriented(shape, text_start, l.vertical))
        .and_then(|warp| warp.path_text_x_near_text(f64::from(tx), f64::from(ty), text_end))
        .map_or((tx, ty), |offset| l.to_text(offset as f32, -1.0));
    photocraft_text::hit_char(&l, &text, tx, ty).0
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_geom::{Rect, Size};

    /// The pixels, not the bounds: the same ink rectangle with a different glyph inside is not
    /// our render (a PSD's stored paragraph raster can share the bounds of ours).
    #[test]
    fn an_identical_render_is_recognised_by_its_pixels() {
        use photocraft_raster::Surface;
        let ink = Rect::new(10, 10, 60, 40);
        let mut render = Surface::new(photocraft_color::PixelFormat::RGBA8);
        render.fill_rect(ink, &[0.0, 0.0, 0.0, 1.0]);
        let mut same = Surface::new(photocraft_color::PixelFormat::RGBA8);
        same.fill_rect(ink, &[0.0, 0.0, 0.0, 1.0]);
        assert!(same_render(&render, &same, ink));
        let mut other = Surface::new(photocraft_color::PixelFormat::RGBA8);
        other.fill_rect(ink, &[0.0, 0.0, 0.0, 0.5]);
        assert!(!same_render(&render, &other, ink), "a different glyph is not this render");
        let mut moved = Surface::new(photocraft_color::PixelFormat::RGBA8);
        moved.fill_rect(Rect::new(11, 10, 60, 40), &[0.0, 0.0, 0.0, 1.0]);
        assert!(!same_render(&render, &moved, ink), "another ink rectangle is another layout");
    }

    /// No pixels, no claim: a type layer the file left empty is re-rendered when it is edited.
    #[test]
    fn a_layer_without_pixels_does_not_show_our_layout() {
        let doc = Document::new("t", Size::new(10, 10), photocraft_color::ColorMode::Rgb, photocraft_color::SampleType::U8);
        assert!(!shows_own_layout(&doc, &TextLayer::default()));
    }
}

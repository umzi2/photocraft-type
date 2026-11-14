//! Layout of a text layer with the reimplemented Photoshop engine, in the editor's
//! [`TextLayout`] form.
//!
//! The canvas draws type through [`crate::render_re`] whenever that engine accepts the layer; the
//! Type tool's caret, hit testing, selection, line boxes and geometry commands must measure the
//! *pixels*, so they all take this one layout. There used to be a second adapter here that
//! re-derived the origin, box, first baseline and paragraph probing around the engine's area
//! layout call; this module is now a thin mapping onto the engine's own layout API
//! (`layer_render::layout_from_engine_data*` / `layout_vertical_from_engine_data*`), whose entry
//! points run the same `prepare` stage as the rasteriser and stop before painting. The caret
//! therefore cannot drift from the drawn glyphs: both come from one `prepare` result.
//!
//! # Coordinates
//!
//! Text-space pixels (the space [`TextLayout`] uses and [`crate::render_re`] draws in), y down,
//! before the layer transform:
//!
//! * point text: the text origin, first baseline at `y = 0`;
//! * box/area text: the box top-left at `(x, y)` from the layer shape, first baseline
//!   `first_baseline` below it (Photoshop's "first baseline = ascent of 'd'", the rule the
//!   engine's `first_baseline_ratio` models);
//! * vertical (`Ornt = Vrtc`) text is mapped into the editor's line space (line direction down,
//!   successive columns to the left), exactly like the built-in layout: one engine column becomes
//!   one line whose `baseline` is minus the column's cross position, and [`TextLayout::to_text`]
//!   turns that line space into text space.
//!
//! The engine's layout reports `x` as the pen from the text origin and `y` as the distance down
//! from the first baseline; [`Placement`] adds the box origin and the first baseline.
//!
//! # What it does not model
//!
//! Only layers the engine's rasteriser also refuses are refused here, so the caret falls back to
//! the built-in layout exactly when the pixels do: type on a path (the engine has no on-path
//! layout), non-RGB documents (the engine writes straight RGBA), a text box with no size, and
//! warped type when `PHOTOCRAFT_WARP_RE=0` disables the engine's warp. Warped, uniformly scaled
//! and rotated/sheared layers keep the engine's flat layout: the transform bends or moves the
//! finished pixels, not the pens.
//!
//! The engine's layout does not report a face per glyph (upstream sets face 0 and exposes no
//! per-glyph face), so [`TextLayout::faces`] is left empty. The two consumers that need outlines
//! — `type_extra_cmds::text_path` and `crates/io/src/pdf/text.rs` — keep the built-in parley
//! layout for that reason (they are documented there); everything the Type tool positions uses
//! this layout.

use std::ops::Range;
use std::sync::{Mutex, OnceLock};

use photocraft_color::{ColorMode, PixelFormat};
use photocraft_doc::TextLayer;
use photocraft_doc::text::{CharStyle, Orientation, TextShape};
use photocraft_text_re::engine_data;
use photocraft_text_re::layer_render::{self, FaceStore, TextLayout as EngineLayout};

use crate::layout::{ClusterInfo, GlyphOrient, LineInfo, PlacedGlyph, TextLayout};
use crate::render_re::{self, Unsupported};

/// The rasteriser's own font store: one instance, one folder scan, so a layer resolves its
/// fonts here exactly as it does when it is drawn. Poisoning only means a caught panic in a
/// previous layout, and the store stays usable.
fn store() -> &'static Mutex<FaceStore> {
    crate::render_re::store()
}

/// Scans the font folders and resolves the fallback face ahead of the first layout.
///
/// The store scan (system folders, the Wine Photoshop folders and the compiled-in faces) plus
/// the "first face that can outline 'A'" fallback cost a large part of a second on a cold
/// process. Doing it on the first `Type` edit stalls that command's frame; the Type tool calls
/// this from a background thread once, so the first caret/keystroke already has a warm store.
pub fn warm() {
    let guard = store().lock().unwrap_or_else(|e| e.into_inner());
    let _ = guard.names().next();
}

/// [`warm`] on a background thread, started at launch (like the built-in font database's scan).
/// Without this the first Type edit pays the store scan inside its own frame.
pub fn warm_async() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        std::thread::spawn(warm);
    });
}

/// Whether [`layout_layer`] should log why it left a layer to the built-in layout
/// (`PHOTOCRAFT_DEBUG_LAYOUT_RE=1`).
pub fn debug_enabled() -> bool {
    std::env::var_os("PHOTOCRAFT_DEBUG_LAYOUT_RE").is_some()
}

/// Where the engine's layout sits in text space, and whether the first baseline offsets it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Placement {
    /// Text-space position of the layout origin (the box's top-left; `(0, 0)` for point text).
    origin: (f32, f32),
    /// `first_baseline` for box/area type, `0` for point text (its origin *is* the baseline).
    add_baseline: f32,
    /// The engine placed no glyph line, so its reported `first_baseline` is `0` and its empty
    /// rows are measured from the box top: shift them onto the real first baseline.
    baseline_shift: f32,
}

/// Everything [`convert`] needs; a struct because the derivation has many inputs and the
/// workspace denies `too_many_arguments`.
struct Convert<'a> {
    engine: &'a EngineLayout,
    /// The layer text (paragraph ends as CR: byte offsets equal the model text's).
    text: &'a str,
    placement: Placement,
    /// Vertical type: layout space is turned 90° clockwise into text space.
    vertical: bool,
    /// Resolved character styles, one per run of the layer.
    styles: Vec<CharStyle>,
    /// Pixels per point used (dpi / 72); positions are already in text-space pixels.
    px_per_pt: f32,
}

/// Lays out `layer` with the reimplemented Photoshop engine, in text space.
///
/// This is the layout the rasteriser draws for the same layer: the same `EngineData` document,
/// the same paragraph bridge, and the engine's own layout entry point (which shares the
/// renderer's `prepare` stage). Refused layers return [`Unsupported`]; their pixels come from the
/// built-in renderer, so their caret must come from the built-in layout.
pub fn layout_layer(layer: &TextLayer, dpi: f32, format: PixelFormat) -> Result<TextLayout, Unsupported> {
    refuse(layer, dpi, format)?;
    let ((width, height), (x, y)) = render_re::text_box(layer)?;
    let point = matches!(layer.shape, TextShape::Point);
    // The same document the rasteriser consumes, and its own paragraph bridge, so the two
    // layouts cannot drift.
    let text = layer.text.replace('\n', "\r");
    let ed_bytes = crate::engine_data::write(&crate::psd::build_engine_data(layer, None, dpi));
    let ed = engine_data::parse(&ed_bytes).map_err(|e| Unsupported(format!("its EngineData does not parse: {e}")))?;
    let paragraphs = render_re::paragraph_runs(layer, dpi, &text);
    let vertical = layer.orientation != Orientation::Horizontal;
    // Same choice `render_re` makes: a vertical paragraph box wraps columns at the box height,
    // point text stays one column per hard break.
    // A vertical paragraph box is anchored at its top-left with the first column on the box's
    // right edge, so the layout needs both sides (see `render_re`).
    let vertical_box = (!point).then_some((width, height));
    let guard = store().lock().unwrap_or_else(|e| e.into_inner());
    let engine = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if vertical {
            match vertical_box {
                Some(box_size) => layer_render::layout_vertical_from_engine_data_in_box(&guard, &ed, &text, (0.0, 0.0), box_size),
                None => layer_render::layout_vertical_from_engine_data(&guard, &ed, &text, (0.0, 0.0)),
            }
        } else {
            // The paragraph sheets are the model's (a `Txt2` sheet can differ from the
            // `EngineData` one); the plain call is the same fallback chain `render_re` uses.
            layer_render::layout_from_engine_data_with_paragraphs(&guard, &ed, &text, &paragraphs, (width, height), (0.0, 0.0))
                .or_else(|_| layer_render::layout_from_engine_data(&guard, &ed, &text, (width, height), (0.0, 0.0)))
        }
    }));
    let engine = match engine {
        Ok(Ok(layout)) => layout,
        Ok(Err(e)) => return Err(Unsupported(format!("the reimplementation could not lay it out: {e}"))),
        Err(payload) => {
            let what = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            return Err(Unsupported(format!("the engine layout panicked: {what}")));
        }
    };
    // The layout reports the first baseline of the lines it placed; an empty layer has none, so
    // the first baseline still comes from the same helper the renderer anchors with.
    let first_baseline = if engine.first_baseline.is_finite() && engine.first_baseline > 0.0 {
        engine.first_baseline
    } else {
        layer_render::first_baseline_offset_of(&guard, &ed).unwrap_or(0.0)
    };
    drop(guard);
    if !first_baseline.is_finite() || !width.is_finite() || !height.is_finite() || !x.is_finite() || !y.is_finite() {
        return Err(Unsupported("its text geometry is not finite".into()));
    }
    let baseline_shift = first_baseline - engine.first_baseline;
    let placement = if point {
        Placement { origin: (0.0, 0.0), add_baseline: 0.0, baseline_shift }
    } else {
        Placement { origin: (x, y), add_baseline: first_baseline, baseline_shift }
    };
    let out = convert(&Convert {
        engine: &engine,
        text: &text,
        placement,
        vertical,
        styles: layer.char_runs().into_iter().map(|r| r.style).collect(),
        px_per_pt: if dpi.is_finite() && dpi > 0.0 { dpi / 72.0 } else { 1.0 },
    });
    if debug_enabled() {
        eprintln!(
            "layout_re: dpi={dpi} size_pt={} shape={:?} transform={:?} first={} lines={} bounds={:?}",
            layer.size_pt,
            layer.shape,
            layer.transform,
            first_baseline,
            out.lines.len(),
            out.bounds()
        );
    }
    Ok(out)
}

/// The layer's refusals, mirroring [`crate::render_re::render_layer`]'s: a layer whose pixels
/// come from the built-in renderer must take its caret from the built-in layout.
fn refuse(layer: &TextLayer, dpi: f32, format: PixelFormat) -> Result<(), Unsupported> {
    if format.mode != ColorMode::Rgb {
        return Err(Unsupported(format!("a {:?} document", format.mode)));
    }
    let [a, b, c, d, tx, ty] = layer.transform.m.map(|v| v as f32);
    if ![a, b, c, d, tx, ty].iter().all(|v| v.is_finite()) {
        return Err(Unsupported("a non-finite layer transform".into()));
    }
    if !(dpi.is_finite() && dpi > 0.0) {
        return Err(Unsupported("a non-finite document resolution".into()));
    }
    if matches!(layer.shape, TextShape::Path { .. }) {
        return Err(Unsupported("type on a path is laid out by the built-in renderer".into()));
    }
    let warped = layer.warp.as_ref().is_some_and(|w| w.style != "warpNone" && w.value.abs() > 1e-3);
    if warped && std::env::var("PHOTOCRAFT_WARP_RE").ok().as_deref() == Some("0") {
        return Err(Unsupported("warped text".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Engine layout → TextLayout
// ---------------------------------------------------------------------------

/// Maps the engine layout into the editor's [`TextLayout`].
fn convert(c: &Convert<'_>) -> TextLayout {
    let mut out = TextLayout { px_per_pt: c.px_per_pt, vertical: c.vertical, styles: c.styles.clone(), ..TextLayout::default() };
    if c.vertical {
        vertical_lines(c, &mut out);
        vertical_glyphs(c, &mut out);
    } else {
        horizontal_lines(c, &mut out);
        horizontal_glyphs(c, &mut out);
    }
    out
}

/// Horizontal lines: the engine's baselines and ink extents, moved from the layout origin to the
/// box top-left (or the point-text origin) and flipped into the editor's y-down space.
fn horizontal_lines(c: &Convert<'_>, out: &mut TextLayout) {
    let (ox, oy) = c.placement.origin;
    let base = oy + c.placement.add_baseline + c.placement.baseline_shift;
    for line in &c.engine.lines {
        out.lines.push(LineInfo {
            range: without_trailing_break(c.text, line.range.clone()),
            baseline: base + line.baseline,
            x0: ox + line.x0,
            x1: ox + line.x1,
            ascent: line.ascent,
            descent: line.descent,
            paragraph: line.paragraph,
        });
    }
    if out.lines.is_empty() && c.text.is_empty() {
        // An empty layer draws nothing, but the editor still needs one caret row.
        let (ascent, descent) = empty_metrics(c);
        out.lines.push(LineInfo { range: 0..0, baseline: base, x0: ox, x1: ox, ascent, descent, paragraph: 0 });
    }
}

/// Horizontal glyphs: engine pens, `y` moved from the first baseline to text space.
fn horizontal_glyphs(c: &Convert<'_>, out: &mut TextLayout) {
    let (ox, oy) = c.placement.origin;
    let base = oy + c.placement.add_baseline + c.placement.baseline_shift;
    for glyph in &c.engine.glyphs {
        out.glyphs.push(PlacedGlyph {
            face: 0,
            id: u32::from(glyph.gid),
            x: ox + glyph.x,
            y: base + glyph.y,
            style: style_index(glyph.run, c.styles.len()),
            orient: GlyphOrient::Horizontal,
            line_center: None,
        });
    }
    out.glyph_ranges.extend(c.engine.glyphs.iter().map(|g| g.range.clone()));
    let lines: Vec<MappedLine> = out.lines.iter().map(|l| MappedLine { end: l.x1 }).collect();
    out.clusters = clusters(c, &lines, false);
}

/// The along-column extent of one vertical line: one em cell per drawn glyph, and U+0020's own
/// advance for a space, matching the library's `column_extent` (measured 2026-11-14: Myriad 24 pt
/// `"abc def ghi"` recomposes to 215 px, the no-space control to 204 = our rect exactly, and the
/// space advance is 5.088 px). The engine's vertical advance does not agree with either.
fn column_along_extent(c: &Convert<'_>, line: &photocraft_text_re::layer_render::TextLine) -> f32 {
    let mut total = 0.0;
    for glyph in &c.engine.glyphs {
        if glyph.range.start < line.range.start || glyph.range.start >= line.range.end {
            continue;
        }
        let ch = c.text.get(glyph.range.clone()).and_then(|s| s.chars().next()).unwrap_or(' ');
        if ch == ' ' {
            // Positions and advances are both in text-space pixels here.
            total += glyph.advance;
            continue;
        }
        let size = c
            .styles
            .get(style_index(glyph.run, c.styles.len()) as usize)
            .map_or(12.0, |style| style.size_pt)
            * c.px_per_pt;
        total += size;
    }
    total
}

/// Vertical lines: the engine sets one em column per paragraph line; the editor's line space has
/// the line direction down (`x`) and the cross-line direction left (`baseline`), so every column
/// becomes one line whose `baseline` is minus the column's centre x and whose along-axis extent
/// is the column. The engine's own `TextLine` carries the column's across extents (`x0`/`x1`)
/// and its down-the-column end (`end`); the cross position is their centre.
fn vertical_lines(c: &Convert<'_>, out: &mut TextLayout) {
    let (ox, oy) = c.placement.origin;
    let (size_ascent, size_descent) = empty_metrics(c);
    for line in &c.engine.lines {
        let centre = (line.x0 + line.x1) * 0.5;
        let half = ((line.x1 - line.x0) * 0.5).max(size_descent * 0.5);
        // The along extent Photoshop stores is one em cell per drawn glyph and zero for U+0020
        // (the library renderer's `column_extent`). The engine's `line.end` counts the font's
        // vertical advance instead — 0.95 em for Noto Sans — so the cached rect came out short
        // (205 vs Photoshop's 218 on "abc def ghi" at 24 pt). Derive it from the fixed library
        // column layout; the cross extent still comes from the engine.
        let along = column_along_extent(c, line);
        if std::env::var_os("PHOTOCRAFT_VSPACE_DEBUG").is_some() {
            eprintln!("vertical_lines: engine end={:.3} along={:.3} glyphs={} range={:?}", line.end, along, c.engine.glyphs.iter().filter(|g| g.range.start >= line.range.start && g.range.start < line.range.end).count(), line.range);
        }
        out.lines.push(LineInfo {
            range: without_trailing_break(c.text, line.range.clone()),
            baseline: -(ox + centre),
            x0: oy,
            x1: oy + along,
            ascent: half,
            descent: half,
            paragraph: line.paragraph,
        });
    }
    if out.lines.is_empty() && c.text.is_empty() {
        // An empty layer still needs its caret column.
        out.lines.push(LineInfo { range: 0..0, baseline: -ox, x0: oy, x1: oy, ascent: size_ascent, descent: size_descent, paragraph: 0 });
    }
}

/// Vertical glyphs: the engine's column positions are already text space (x across the column,
/// y down it); both all-upright.
fn vertical_glyphs(c: &Convert<'_>, out: &mut TextLayout) {
    let (ox, oy) = c.placement.origin;
    for glyph in &c.engine.glyphs {
        out.glyphs.push(PlacedGlyph {
            face: 0,
            id: u32::from(glyph.gid),
            x: ox + glyph.x,
            y: oy + glyph.y,
            style: style_index(glyph.run, c.styles.len()),
            orient: GlyphOrient::Upright,
            line_center: None,
        });
    }
    out.glyph_ranges.extend(c.engine.glyphs.iter().map(|g| g.range.clone()));
    let lines: Vec<MappedLine> = out.lines.iter().map(|l| MappedLine { end: l.x1 }).collect();
    out.clusters = clusters(c, &lines, true);
}

/// One line's mapped extent along its inline axis: where the pen ends after its last glyph.
struct MappedLine {
    end: f32,
}

/// Caret stops: one per drawn grapheme cluster of the layer text, at the engine's pen.
///
/// A paragraph separator draws nothing and the editor's [`TextLayout::caret`] already places the
/// caret before/after a break from the line's range, so the engine's separator cluster (a
/// zero-advance stop carrying the rest of the line's width) is left out. Advances are pen
/// distances to the next stop on the same line, the last one closing at the line's pen end; that
/// repairs the engine's own zero advance for the glyph before a paragraph break.
fn clusters(c: &Convert<'_>, lines: &[MappedLine], vertical: bool) -> Vec<ClusterInfo> {
    let origin = if vertical { c.placement.origin.1 } else { c.placement.origin.0 };
    let engine = &c.engine.clusters;
    let xs: Vec<f32> = engine.iter().map(|cluster| origin + cluster.x).collect();
    let mut broken = vec![false; engine.len()];
    let mut by_line: Vec<Vec<usize>> = vec![Vec::new(); c.engine.lines.len().max(1)];
    for (i, cluster) in engine.iter().enumerate() {
        let slice = c.text.get(cluster.range.clone()).unwrap_or("");
        broken[i] = is_paragraph_separator(slice);
        if !broken[i] && let Some(list) = by_line.get_mut(cluster.line) {
            list.push(i);
        }
    }
    let mut advances = vec![0.0f32; engine.len()];
    for (line, indices) in by_line.iter().enumerate() {
        let end = lines.get(line).map_or(0.0, |mapped| mapped.end);
        for (k, &i) in indices.iter().enumerate() {
            let next = indices.get(k + 1).map_or(end, |&j| xs[j]);
            advances[i] = (next - xs[i]).max(0.0);
        }
    }
    engine
        .iter()
        .enumerate()
        .filter(|(i, _)| !broken[*i])
        .map(|(i, cluster)| ClusterInfo { range: cluster.range.clone(), x: xs[i], advance: advances[i], line: cluster.line, rtl: cluster.rtl })
        .collect()
}

/// The paragraph separators `EngineData`/the engine text uses, as [`without_trailing_break`] and
/// [`is_paragraph_separator`] test them.
fn is_paragraph_separator(slice: &str) -> bool {
    !slice.is_empty() && slice.chars().all(|ch| matches!(ch, '\r' | '\n' | '\u{2029}'))
}

/// The editor's line range excludes the paragraph break the line ends with (the built-in layout's
/// contract: `LineInfo::range` is "without the paragraph break"); the engine includes it so the
/// following line starts at `range.end`.
fn without_trailing_break(text: &str, range: Range<usize>) -> Range<usize> {
    let mut end = range.end;
    if let Some(ch) = text.get(range.start..end).and_then(|slice| slice.chars().next_back())
        && matches!(ch, '\r' | '\n' | '\u{2029}')
    {
        end -= ch.len_utf8();
    }
    range.start..end.max(range.start)
}

/// Glyph/run index clamped into the resolved styles (a run the adapter could not resolve keeps
/// the last style rather than panicking in the renderer's `styles[g.style]`).
fn style_index(run: usize, styles: usize) -> u32 {
    run.min(styles.saturating_sub(1)) as u32
}

/// Ascent/descent of an empty row, from the first resolved style: a caret needs a non-degenerate
/// box even when the layer has no glyphs.
fn empty_metrics(c: &Convert<'_>) -> (f32, f32) {
    let size = c.styles.first().map_or(12.0, |style| style.size_pt) * c.px_per_pt;
    let size = if size.is_finite() && size > 0.0 { size } else { 12.0 };
    (size * 0.8, size * 0.2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_doc::text::{ParagraphRun, ParagraphStyle as ModelParagraph, TextRun};

    fn layer(text: &str, shape: TextShape) -> TextLayer {
        // Noto Sans is installed in the Wine Photoshop font folder the fixtures use; naming the
        // PostScript face keeps the test independent of the app's bundled fonts.
        let style = CharStyle { postscript_name: Some("NotoSans-Regular".into()), size_pt: 24.0, ..CharStyle::default() };
        TextLayer {
            text: text.into(),
            font_family: String::new(),
            size_pt: 24.0,
            color: photocraft_doc::Color::BLACK,
            transform: photocraft_geom::Affine::IDENTITY,
            cache: None,
            psd_raw: None,
            runs: vec![TextRun { len: text.len(), style }],
            paragraphs: vec![ParagraphRun { len: text.len(), style: ModelParagraph::default() }],
            shape,
            orientation: Orientation::Horizontal,
            antialias: photocraft_doc::text::AntiAlias::Smooth,
            warp: None,
        }
    }

    fn fonts_ready() -> bool {
        let guard = store().lock().unwrap_or_else(|e| e.into_inner());
        guard.contains("NotoSans-Regular")
    }

    fn text_box(w: f32, h: f32) -> TextShape {
        TextShape::Box { x: 20.0, y: 10.0, width: w, height: h }
    }

    fn rgba() -> PixelFormat {
        PixelFormat::RGBA8
    }

    #[test]
    fn empty_point_text_has_one_caret_line_at_the_origin() {
        let out = layout_layer(&layer("", TextShape::Point), 72.0, rgba()).expect("empty point text still has a caret");
        assert_eq!(out.lines.len(), 1);
        assert_eq!(out.lines[0].range, 0..0);
        assert!(!out.vertical);
        assert_eq!(out.caret(0).0, 0.0);
        assert_eq!(out.hit_test(100.0, 0.0), 0);
    }

    #[test]
    fn empty_box_text_puts_the_caret_on_the_first_baseline() {
        let out = layout_layer(&layer("", text_box(300.0, 100.0)), 72.0, rgba()).unwrap();
        assert_eq!(out.lines.len(), 1);
        let (x, top, bottom) = out.caret(0);
        assert!(x >= 20.0);
        assert!(top < bottom);
        // The box top is 10; the first baseline is one ascent below it, inside the box.
        let offset = out.lines[0].baseline - 10.0;
        assert!(offset > 0.0 && offset < 24.0, "first baseline offset {offset}");
    }

    #[test]
    fn only_the_layers_the_drawn_engine_refuses_are_refused() {
        // Vertical type is drawn by the engine now, so its caret comes from the engine too.
        let mut vertical = layer("Hi", TextShape::Point);
        vertical.orientation = Orientation::Vertical;
        let out = layout_layer(&vertical, 72.0, rgba()).expect("the engine draws vertical type");
        assert!(out.vertical && !out.lines.is_empty());

        let mut warped = layer("Hi", TextShape::Point);
        warped.warp = Some(photocraft_doc::text::TextWarp {
            style: "warpArc".into(),
            value: 50.0,
            horizontal_distortion: 0.0,
            vertical_distortion: 0.0,
            horizontal: true,
        });
        // A warp bends the pixels, not the pens: the flat engine layout is its caret layout.
        assert!(layout_layer(&warped, 72.0, rgba()).is_ok());

        let mut scaled = layer("Hi", TextShape::Point);
        scaled.transform = photocraft_geom::Affine::scale(2.0);
        assert!(layout_layer(&scaled, 72.0, rgba()).is_ok());

        let path = layer("Hi", TextShape::Path { path: photocraft_doc::vector::Subpath::default(), start: 0.0, end: None, flip: false });
        assert!(layout_layer(&path, 72.0, rgba()).is_err());

        let cmyk = PixelFormat::new(ColorMode::Cmyk, photocraft_color::SampleType::U8, true);
        assert!(layout_layer(&layer("Hi", TextShape::Point), 72.0, cmyk).is_err());
    }

    #[test]
    fn point_text_clusters_and_carets_follow_the_engine_pens() {
        if !fonts_ready() {
            return;
        }
        let l = layer("Hamburg", TextShape::Point);
        let out = layout_layer(&l, 72.0, rgba()).unwrap();
        assert_eq!(out.lines.len(), 1);
        assert_eq!(out.clusters.len(), 7);
        let pens: Vec<f32> = out.clusters.iter().map(|c| c.x).collect();
        assert!(pens.windows(2).all(|w| w[1] > w[0]), "caret stops advance left to right: {pens:?}");
        assert!(out.clusters.iter().all(|c| c.advance > 0.0));
        assert_eq!(out.caret(0).0, out.clusters[0].x);
        // The caret after 'H' is the engine's pen for 'a'.
        assert_eq!(out.caret(1).0, out.clusters[1].x);
        // Hit testing on either side of a cluster's midpoint picks that cluster's boundary.
        let c = out.clusters[2].clone();
        assert_eq!(out.hit_test(c.x + c.advance / 2.0 - 0.1, out.lines[0].baseline), c.range.start);
        assert_eq!(out.hit_test(c.x + c.advance / 2.0 + 0.1, out.lines[0].baseline), c.range.end);
        // Past the end of the line the caret is the text end.
        assert_eq!(out.hit_test(1.0e6, out.lines[0].baseline), l.text.len());
        assert!(crate::text_point_inside(&out, out.lines[0].x0 + 1.0, out.lines[0].baseline, 0.0));
    }

    #[test]
    fn box_text_wraps_on_the_engine_breaks_and_carets_cross_lines() {
        if !fonts_ready() {
            return;
        }
        let l = layer("Hamburg font wraps here", text_box(120.0, 200.0));
        let out = layout_layer(&l, 72.0, rgba()).unwrap();
        assert!(out.lines.len() > 1, "the narrow box has to wrap: {:?}", out.lines.iter().map(|l| l.range.clone()).collect::<Vec<_>>());
        for pair in out.lines.windows(2) {
            assert!(pair[0].baseline < pair[1].baseline, "baselines step down the box");
            assert_eq!(pair[0].range.end, pair[1].range.start, "a soft wrap's line ranges are contiguous");
        }
        assert_eq!(out.lines[0].range.start, 0);
        assert_eq!(out.lines.last().unwrap().range.end, l.text.len());
        let first_end = out.lines[0].range.end;
        let (x, top, bottom) = out.caret(first_end);
        assert!(x <= out.lines[0].x1 + 1.0, "caret {x} past the first line's extent {}", out.lines[0].x1);
        assert!(top < bottom);
        // Clicking on the second line's baseline picks an offset on that line.
        let second = out.lines[1].range.start;
        let hit = out.hit_test(21.0, out.lines[1].baseline);
        assert!(
            hit == second || hit == out.clusters.iter().find(|c| c.range.start == second).map(|c| c.range.end).unwrap_or(hit),
            "click on the second line's first character: {hit}"
        );
    }

    #[test]
    fn a_box_too_short_for_the_text_reports_overflow() {
        if !fonts_ready() {
            return;
        }
        // The engine drops a line whose top passes the box height; the layout must not pretend a
        // paragraph was laid out (that is what `TypeLayout::overflows` decides the "+" handle by).
        let text = "ab\ncd\nef";
        let short = layer(text, TextShape::Box { x: 200.0, y: 150.0, width: 260.0, height: 40.0 });
        let out = layout_layer(&short, 72.0, rgba()).unwrap();
        let debug: Vec<(std::ops::Range<usize>, f32, f32)> = out.lines.iter().map(|l| (l.range.clone(), l.baseline, l.ascent)).collect();
        assert!(out.overflows(text), "a 40 px box at 24 pt holds less than the text: {debug:?}");
        assert!(out.lines.len() < 3);
        assert!(out.lines.iter().all(|l| l.range.end < text.len()));

        let tall = layer(text, TextShape::Box { x: 200.0, y: 150.0, width: 260.0, height: 600.0 });
        let out = layout_layer(&tall, 72.0, rgba()).unwrap();
        assert!(!out.overflows(text));
        assert_eq!(out.lines.len(), 3);
        assert_eq!(out.lines.last().unwrap().range.end, text.len());
    }

    #[test]
    fn empty_paragraphs_get_a_line_that_holds_a_caret() {
        if !fonts_ready() {
            return;
        }
        let l = layer("A\n\nB", text_box(300.0, 300.0));
        let out = layout_layer(&l, 72.0, rgba()).unwrap();
        assert_eq!(out.lines.len(), 3, "the empty paragraph takes a line");
        assert_eq!(out.lines[1].range, 2..2);
        assert!(out.lines[0].baseline < out.lines[1].baseline && out.lines[1].baseline < out.lines[2].baseline);
        let (x, top, bottom) = out.caret(2);
        assert!(x >= 0.0);
        assert!(top < bottom);
        assert_eq!(out.hit_test(x + 0.5, out.lines[1].baseline), 2);
    }

    #[test]
    fn multibyte_carets_stay_on_char_boundaries() {
        if !fonts_ready() {
            return;
        }
        // The bug this guards: run lengths are UTF-16 code units while the editor's caret and
        // ranges are UTF-8 byte offsets. Mixing them cut a line range inside a multibyte
        // character and panicked `&text[start..end]` on the Japanese layer (25 inside bytes
        // 24..27 of '\u{3059}').
        for text in [
            "\u{300c}\u{3053}\u{308c}\u{306f}\u{30c6}\u{30b9}\u{30c8}\u{3067}\u{3059}\u{3002}\u{300d}\u{5f7c}\u{306f}\u{8a00}\u{3063}\u{305f}\u{3002}",
            "\u{0422}\u{0438}\u{043f}\u{043e}\u{0433}\u{0440}\u{0430}\u{0444}\u{0438}\u{043a}\u{0430}",
            "emoji \u{1f642} and combining e\u{301} ok",
            "Stra\u{00df}e",
        ] {
            for shape in [TextShape::Point, text_box(400.0, 300.0)] {
                let out = layout_layer(&layer(text, shape), 72.0, rgba()).unwrap_or_else(|why| panic!("{text:?}: {why}"));
                // Every caret offset is a char boundary of the layer text, and none panics.
                for (byte, _) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
                    assert!(text.is_char_boundary(byte), "{text:?}: offset {byte} is not a boundary");
                    let (x, top, bottom) = out.caret(byte);
                    assert!(x.is_finite() && top < bottom, "{text:?} at {byte}: {x} {top}..{bottom}");
                    let hit = out.hit_test(x, out.lines[0].baseline);
                    assert!(hit <= text.len() && text.is_char_boundary(hit), "{text:?}: hit {hit} off a boundary");
                }
                for c in &out.clusters {
                    assert!(text.is_char_boundary(c.range.start) && text.is_char_boundary(c.range.end), "{text:?}: cluster {:?}", c.range);
                }
                // Within one line the caret only moves right.
                for (index, _) in out.lines.iter().enumerate() {
                    let mut previous = f32::MIN;
                    for c in out.clusters.iter().filter(|c| c.line == index) {
                        assert!(c.x >= previous - 0.01, "{text:?}: line {index} stop {} went backwards", c.range.start);
                        previous = c.x;
                    }
                }
            }
        }
    }

    #[test]
    fn utf16_run_lengths_do_not_move_byte_ranges() {
        if !fonts_ready() {
            return;
        }
        // Non-ASCII before a line break: the run lengths are UTF-16 units (each Cyrillic char
        // is one unit but two bytes), so the second paragraph's byte ranges must still be the
        // model text's byte offsets.
        let l = layer("\u{0422}\u{0438}\u{043f}\n\u{043e}\u{0433}\u{0440}", text_box(400.0, 300.0));
        let out = layout_layer(&l, 72.0, rgba()).unwrap();
        assert_eq!(out.lines.len(), 2);
        assert_eq!(out.lines[0].range.end, 6, "the break after three two-byte characters is byte 6");
        assert_eq!(out.lines[1].range.start, 7);
        assert_eq!(out.lines[1].range.end, l.text.len());
        for c in &out.clusters {
            assert!(c.range.start <= c.range.end && c.range.end <= l.text.len());
            assert!(l.text.is_char_boundary(c.range.start) && l.text.is_char_boundary(c.range.end));
        }
        // Every drawn character has one caret stop; the paragraph break draws nothing and the
        // caret places itself at the line's edge through the range.
        assert_eq!(out.clusters.len(), l.text.chars().count() - 1, "the paragraph break has no caret stop");
    }

    #[test]
    fn caps_expansion_shares_one_caret_stop() {
        if !fonts_ready() {
            return;
        }
        let mut l = layer("Stra\u{00df}e", TextShape::Point);
        l.runs[0].style.caps = photocraft_doc::text::Caps::AllCaps;
        let out = layout_layer(&l, 72.0, rgba()).unwrap();
        // 'ß' uppercases to two characters that the engine draws as two glyphs; both belong to
        // the one source character, so the editor still sees one stop per character (six for
        // "Straße") and the ß stop spans its two bytes at "SS" width.
        assert_eq!(out.clusters.len(), l.text.chars().count());
        assert_eq!(out.clusters[4].range, 4..6, "{:?}", out.clusters.iter().map(|c| c.range.clone()).collect::<Vec<_>>());
        assert_eq!(out.clusters.iter().map(|c| c.range.len()).sum::<usize>(), l.text.len());
        assert!(out.clusters[4].advance > out.clusters[3].advance, "the two glyphs widen the ß stop");
    }

    #[test]
    fn hanging_quote_starts_left_of_the_box_edge() {
        if !fonts_ready() {
            return;
        }
        let mut l = layer("\u{201C}Hung", text_box(300.0, 100.0));
        if let Some(p) = l.paragraphs.first_mut() {
            p.style.hanging_punctuation = true;
        }
        let out = layout_layer(&l, 72.0, rgba()).unwrap();
        assert_eq!(out.lines.len(), 1);
        assert!(out.clusters[0].x < 20.0, "the quote hangs left of the box edge: {}", out.clusters[0].x);
        assert_eq!(out.caret(0).0, out.clusters[0].x);
        // The quote hangs fully, so the next glyph starts on the box edge.
        assert!((out.clusters[1].x - 20.0).abs() < 0.5, "next glyph at {}", out.clusters[1].x);
    }

    #[test]
    fn vertical_carets_run_down_the_column() {
        if !fonts_ready() {
            return;
        }
        let mut l = layer("Vertical", TextShape::Point);
        l.orientation = Orientation::Vertical;
        let out = layout_layer(&l, 72.0, rgba()).unwrap();
        assert!(out.vertical);
        assert_eq!(out.lines.len(), 1, "the engine sets vertical type as one column");
        assert!(out.clusters.len() >= l.text.len());
        // Down the column is the editor's line-space x; each caret is a horizontal bar.
        let mut previous = f32::MIN;
        for cluster in &out.clusters {
            assert!(cluster.x >= previous - 0.01, "vertical stops are in reading order");
            previous = cluster.x;
            let (x, top, bottom) = out.caret(cluster.range.start);
            assert!((x - cluster.x).abs() < 0.01, "the caret is at the stop");
            let (t0, t1) = (out.to_text(x, top), out.to_text(x, bottom));
            assert!((t1.0 - t0.0).abs() > 0.01, "the vertical caret spans the column");
        }
    }

    #[test]
    fn vertical_paragraph_breaks_start_a_column_to_the_left() {
        if !fonts_ready() {
            return;
        }
        let mut l = layer("ABC\nDEF", TextShape::Point);
        l.orientation = Orientation::Vertical;
        let out = layout_layer(&l, 72.0, rgba()).unwrap();
        assert!(out.vertical);
        assert_eq!(out.lines.len(), 2, "one engine column per paragraph line");
        assert_eq!(out.lines[0].range, 0..3);
        assert_eq!(out.lines[1].range, 4..7);
        // Columns advance leftwards: the second column's baseline is a pitch to the left.
        assert!(out.lines[1].baseline > out.lines[0].baseline + 10.0, "second column is to the left");
        // Both columns start at the top of the text box; the first stops are at the same height.
        assert!((out.lines[0].x0 - out.lines[1].x0).abs() < 0.01);
        let (x0, ..) = out.caret(0);
        let (x1, ..) = out.caret(4);
        assert!((x0 - x1).abs() < 0.01, "a new column starts at the top again");
        // The caret of a column start is a bar across that column.
        let (_, top, bottom) = out.caret(4);
        let (t0, t1) = (out.to_text(x1, top), out.to_text(x1, bottom));
        assert!((t1.0 - t0.0).abs() > 10.0, "the vertical caret spans the column: {t0:?}..{t1:?}");
        // A click in the middle of it lands on the same column and byte.
        let mid = ((t0.0 + t1.0) * 0.5, (t0.1 + t1.1) * 0.5);
        assert_eq!(out.hit_test(mid.0, mid.1), 4, "hit-test the column start");
    }

    #[test]
    fn showcase_scale_layouts_stay_fast() {
        if !fonts_ready() {
            return;
        }
        // The layers the Type tool has to lay out per edit: a long justified paragraph (layer
        // "04 area justify full hanging") and a CJK line ("10 cjk heiti"). The caret path must
        // stay interactive; the first call also warms the store.
        let paragraph = "\u{201c}Hanging punctuation,\u{201d} she said, \u{201c}is optical margin alignment; commas, periods. Overhanging quotes and dashes\u{2014}like these\u{2014}are shifted left. The final line stays stretched under justify-all, and its period hangs past the right margin.\r";
        let cjk = "\u{65e5}\u{672c}\u{8a9e}\u{306e}\u{30c6}\u{30ad}\u{30b9}\u{30c8}\u{306f}\u{7e26}\u{66f8}\u{304d}\u{3067}\u{3059}\u{3002}\u{6a2a}\u{66f8}\u{304d}\u{306e}\u{5834}\u{5408}\u{306f}\u{5de6}\u{304b}\u{3089}\u{53f3}\u{3078}\u{6d41}\u{308c}\u{307e}\u{3059}\u{3002}";
        for (text, shape) in [
            (paragraph, TextShape::Box { x: 32.0, y: 240.0, width: 493.0, height: 155.0 }),
            (paragraph, TextShape::Point),
            (cjk, TextShape::Box { x: 623.0, y: 259.0, width: 546.0, height: 47.0 }),
        ] {
            let l = layer(text, shape);
            let _ = layout_layer(&l, 72.0, rgba()).expect("warm-up layout");
            let rounds = 4;
            let start = std::time::Instant::now();
            for _ in 0..rounds {
                let out = layout_layer(&l, 72.0, rgba()).expect("layout");
                assert!(!out.lines.is_empty());
            }
            let ms = start.elapsed().as_secs_f64() * 1000.0 / f64::from(rounds);
            assert!(ms < 50.0, "{:?} took {ms:.1} ms per layout", &text[..text.len().min(20)]);
        }
    }
}

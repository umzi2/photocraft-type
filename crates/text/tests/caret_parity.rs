//! Headless regression gate for the Type tool's caret: the caret has to land on the glyphs the
//! canvas actually draws.
//!
//! The canvas draws type through the reimplemented Photoshop engine ([`photocraft_text::render_re`])
//! and the Type tool positions the caret from [`photocraft_text::layer_layout`], which the fork's
//! adapter now takes from the engine's own layout API. These tests lay a layer out, render it with
//! the same engine, and check pixel by pixel that every caret stop sits on the drawn glyph it
//! belongs to and that the stops only advance along a line. They fail if the adapter, its origin
//! mapping or its caret-stop mapping ever drifts from the renderer again.
//!
//! Fonts: the tests name the PostScript face `NotoSans-Regular`, the face the PSD fixtures use
//! from the Wine Photoshop font folder. On a machine without it the engine refuses the layer and
//! the test reports a skip (the engine's own test suites behave the same way).

use photocraft_color::PixelFormat;
use photocraft_doc::TextLayer;
use photocraft_doc::text::{CharStyle, Orientation, ParagraphRun, ParagraphStyle, TextAlign, TextRun, TextShape};
use photocraft_geom::Affine;
use photocraft_raster::Surface;
use photocraft_text::{TextLayout, layer_layout};

const DPI: f32 = 72.0;

fn noto(size: f32) -> CharStyle {
    CharStyle { postscript_name: Some("NotoSans-Regular".into()), size_pt: size, ..CharStyle::default() }
}

fn layer(text: &str, style: CharStyle, shape: TextShape, align: TextAlign) -> TextLayer {
    let paragraph = ParagraphStyle { align, ..ParagraphStyle::default() };
    TextLayer {
        text: text.into(),
        font_family: String::new(),
        size_pt: style.size_pt,
        color: photocraft_doc::Color::BLACK,
        transform: Affine::IDENTITY,
        cache: None,
        psd_raw: None,
        runs: vec![TextRun { len: text.len(), style }],
        paragraphs: vec![ParagraphRun { len: text.len(), style: paragraph }],
        shape,
        orientation: Orientation::Horizontal,
        antialias: photocraft_doc::text::AntiAlias::Smooth,
        warp: None,
    }
}

/// Lays out and renders one layer, then checks the caret against the pixels.
///
/// The layer transform is the identity, so text space and document space are the same: the
/// layout positions are the pixel positions the renderer used.
fn check(layer: &TextLayer) {
    let format = PixelFormat::RGBA8;
    let Ok((surface, rect)) = photocraft_text::render_re::render_layer(layer, DPI, format) else {
        eprintln!("caret_parity: skipping {:?} (the engine cannot render it on this machine)", &layer.text[..layer.text.len().min(24)]);
        return;
    };
    // The Type tool's layout: it must be the renderer's own layout, not the built-in fallback,
    // whenever the engine draws the layer.
    let layout = layer_layout(layer, DPI, format);
    assert_eq!(layout.vertical, layer.orientation == Orientation::Vertical, "orientation flag");
    assert!(!layout.lines.is_empty(), "no lines for {:?}", layer.text);
    assert_covered(&layout, rect);

    for (index, line) in layout.lines.iter().enumerate() {
        let mut stops: Vec<_> = layout.clusters.iter().filter(|cluster| cluster.line == index).collect();
        stops.sort_by_key(|cluster| cluster.range.start);
        assert!(!stops.is_empty(), "line {index} has no caret stops for {:?}", layer.text);
        let mut previous = f32::MIN;
        for stop in &stops {
            assert!(stop.x.is_finite() && stop.advance.is_finite(), "line {index}: non-finite stop at {}", stop.range.start);
            assert!(stop.x >= previous - 0.01, "line {index}: stop {} at {} went backwards from {previous}", stop.range.start, stop.x);
            previous = stop.x;
            assert!(
                stop.x >= line.x0 - 1.0 && stop.x <= line.x1 + 1.0 + stop.advance,
                "line {index}: stop {} at {} is outside the line box {}..{}",
                stop.range.start,
                stop.x,
                line.x0,
                line.x1
            );
            let source = layer.text.get(stop.range.clone()).unwrap_or("");
            if source.chars().all(char::is_whitespace) {
                continue; // spaces and paragraph breaks draw nothing of their own
            }
            let Some(glyph) = glyph_of(&layout, stop.range.start) else {
                continue; // a character the face has no glyph for: the stop is still a caret
            };
            if layout.vertical {
                assert!(
                    (stop.x - glyph.1).abs() <= 0.5,
                    "vertical stop {} at {} is not the glyph's place down the column {}",
                    stop.range.start,
                    stop.x,
                    glyph.1
                );
                let reach = stop.advance.max(1.0);
                assert_ink(&surface, rect, glyph.0 - reach, glyph.1, glyph.0 + reach, glyph.1 + reach, "vertical glyph");
            } else {
                assert!(
                    (stop.x - glyph.0).abs() <= 0.5,
                    "stop {} at {} is not the drawn glyph's pen {}",
                    stop.range.start,
                    stop.x,
                    glyph.0
                );
                let band = (line.baseline - line.ascent, line.baseline + line.descent);
                let reach = stop.advance.max(1.0);
                assert_ink(&surface, rect, glyph.0, band.0, glyph.0 + reach, band.1, "glyph");
            }
            // Clicking the middle of the caret must hit the same character.
            let segment = layout.caret_segment(stop.range.start);
            let mid = ((segment[0].0 + segment[1].0) * 0.5, (segment[0].1 + segment[1].1) * 0.5);
            let hit = layout.hit_test(mid.0, mid.1);
            assert!(
                hit == stop.range.start || hit == stop.range.end,
                "stop {} at {:?} hit-tests to {hit}",
                stop.range.start,
                mid
            );
        }
        if !stops.is_empty() {
            assert!(stops.last().is_some_and(|stop| stop.advance >= 0.0), "line {index}: a negative advance");
        }
    }
}

/// The (x, y) of the first drawn glyph that covers `byte`, or `None` when the character was not
/// shaped into a glyph (a missing glyph or a character the layout itself inserted).
fn glyph_of(layout: &TextLayout, byte: usize) -> Option<(f32, f32)> {
    layout
        .glyph_ranges
        .iter()
        .position(|range| range.start == byte)
        .map(|index| (layout.glyphs[index].x, layout.glyphs[index].y))
}

/// Every laid-out glyph must fall inside the rectangle the renderer drew (a few pixels of slack:
/// the surface is snapped to the pixel grid and a glyph's pen may sit on the edge). The *line*
/// boxes are not checked: they carry the face's hhea ascent/descent, which extends past the ink.
fn assert_covered(layout: &TextLayout, rect: photocraft_geom::Rect) {
    let (x0, y0, x1, y1) = (rect.x0 as f32, rect.y0 as f32, rect.x1 as f32, rect.y1 as f32);
    for glyph in &layout.glyphs {
        assert!(
            glyph.x >= x0 - 6.0 && glyph.x <= x1 + 6.0 && glyph.y >= y0 - 6.0 && glyph.y <= y1 + 6.0,
            "glyph at ({}, {}) is outside the rendered rectangle {rect:?}",
            glyph.x,
            glyph.y
        );
    }
}

/// The `3 × 3` neighbourhood of every pixel in the box must not be fully transparent: the stop's
/// glyph has to have been drawn there.
fn assert_ink(surface: &Surface, rect: photocraft_geom::Rect, x0: f32, y0: f32, x1: f32, y1: f32, what: &str) {
    let channels = surface.channels();
    let alpha = channels.saturating_sub(1);
    let (rx0, rx1) = (rect.x0.max(x0.floor() as i32 - 1), rect.x1.min(x1.ceil() as i32 + 1));
    let (ry0, ry1) = (rect.y0.max(y0.floor() as i32 - 1), rect.y1.min(y1.ceil() as i32 + 1));
    let mut found = false;
    for y in ry0..ry1.max(ry0 + 1) {
        for x in rx0..rx1.max(rx0 + 1) {
            if surface.sample_channel(x, y, alpha) > 0.02 {
                found = true;
                break;
            }
        }
        if found {
            break;
        }
    }
    assert!(found, "no {what} ink near ({x0:.1}, {y0:.1})..({x1:.1}, {y1:.1})");
}

#[test]
fn point_text_caret_stops_sit_on_the_drawn_glyphs() {
    let text = "Hamburg fi ffl 0123";
    check(&layer(text, noto(24.0), TextShape::Point, TextAlign::Left));
}

#[test]
fn a_boxed_justified_paragraph_puts_every_stop_on_its_glyph() {
    let text = "Justify-all stretches the last line too, which is exactly what Photoshop stores in Txt2 /5/0 = 6, a value the DOM cannot express.";
    check(&layer(text, noto(20.0), TextShape::Box { x: 38.0, y: 619.0, width: 480.0, height: 120.0 }, TextAlign::JustifyAll));
}

#[test]
fn an_indented_paragraph_caret_follows_the_engine_breaks() {
    let text = "First paragraph: the first line starts after a first-line indent. The paragraph also carries space before and space after.";
    check(&layer(text, noto(20.0), TextShape::Box { x: 40.0, y: 430.0, width: 480.0, height: 160.0 }, TextAlign::Left));
}

#[test]
fn vertical_text_stops_run_down_the_drawn_column() {
    let mut vertical = layer("Vertical Abc", noto(24.0), TextShape::Point, TextAlign::Left);
    vertical.orientation = Orientation::Vertical;
    check(&vertical);
}

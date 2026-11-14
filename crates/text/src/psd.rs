//! PSD type layers: the `TySh` block (transform + text descriptor with `EngineData` + warp) ⇄ the
//! typed [`TextLayer`] model.
//!
//! Units: EngineData sizes are in text-space units, which the `TySh` transform maps to document
//! pixels; Photoshop shows them as points at the document resolution, so
//! `size_pt = FontSize × 72 / dpi` (and likewise leading, tracking-independent lengths, indents).
//! Box bounds stay in text-space pixels.
//!
//! Writing starts from the layer's original `TySh` (when present) and patches only the text, runs,
//! paragraphs, fonts, shape and transform, so Photoshop-only settings survive an edit.

use photocraft_color::{Color, ColorMode};
use photocraft_doc::TextLayer;
use photocraft_doc::text::{
    AntiAlias, Caps, CharStyle, FontBaseline, Kerning, Orientation, ParagraphRun, ParagraphStyle, TextAlign, TextRun, TextShape, TextWarp,
};
use photocraft_geom::{Affine, Point};
use photocraft_psd::descriptor::{Descriptor, Id, Value as D};

use crate::engine_data::{self as ed, Value as E};
use crate::fonts::guess_from_postscript;

mod build;
mod edit;
mod import;

#[cfg(test)]
pub(crate) use build::font_entry;
pub use build::{MAX_TEXT_INDEX, build_engine_data, build_txt2, build_txt2_at, build_tysh, needs_txt2};
pub(crate) use build::{font_set, paragraph_properties, style_sheet_data};
pub use edit::{set_text_index, text_index, txt2_object_count};
#[cfg(test)]
use import::MAX_ENGINE_RUNS;
pub use import::{
    OPENTYPE_KEYS, TXT2_FEATURES, TXT2_OLD_STYLE, TXT2_STYLISTIC_SETS, apply_txt2, apply_txt2_frame, apply_txt2_path, parse_txt2, refine_legacy_point_origin,
    text_layer_from_tysh, text_layer_from_tysh_with_txt2, txt2_is_valid,
};
pub(crate) use import::{arr_f, char_style, para_style, postscript_for, utf16_to_byte_lengths};
/// Makes Photoshop recompose the layout of every text object whose editing model changed since the
/// block a save replaces (the text logic lives in the reimplemented engine crate, see
/// [`crate::psd`]).
pub use photocraft_text_re::txt2::invalidate_changed_composed_layouts;

/// Parsed `TySh` contents.
#[derive(Clone, Debug, PartialEq)]
pub struct TySh {
    pub transform: Affine,
    pub text: Descriptor,
    pub warp: Option<Descriptor>,
    /// The four trailing big-endian f32 values (left, top, right, bottom) after the warp
    /// descriptor. Photoshop writes zeros for unwarped type and the text-space bounds (the
    /// `bounds` descriptor's rectangle) for warped type.
    pub bounds: [f32; 4],
}

/// Whether a type transform can reach rendering or egui. Pixel coordinates use i32 throughout
/// the document, so non-finite and unrepresentable affine terms are rejected (#4758).
pub fn transform_is_safe(t: &Affine) -> bool {
    t.m.iter().all(|v| v.is_finite() && v.abs() <= f64::from(i32::MAX))
}

/// Parses a `TySh` block.
///
/// The block layout (version, transform, text descriptor, warp descriptor, trailing
/// bounds/padding) is owned by the reimplemented engine's PSD crate
/// (`photocraft_psd::TySh`); this wrapper only maps its fields onto the document model's
/// `Affine`/`Descriptor` shapes.
pub fn parse_tysh(data: &[u8]) -> Option<TySh> {
    let parsed = photocraft_psd::TySh::from_bytes(data).ok()?;
    let transform = Affine { m: parsed.transform };
    if !transform_is_safe(&transform) {
        return None;
    }
    Some(TySh {
        transform,
        text: parsed.descriptor.descriptor.clone(),
        warp: Some(parsed.warp.descriptor.clone()),
        bounds: parsed.bounds_points(),
    })
}

/// `tysh` moved by (`dx`, `dy`): only the transform's translation changes. The text, its styles,
/// the warp and the trailing bounds (which are in text space) stay byte for byte, so a moved
/// Photoshop type layer keeps everything Photoshop wrote. `None` when the block is too short.
pub fn translate_tysh(tysh: &[u8], dx: f64, dy: f64) -> Option<Vec<u8>> {
    let mut parsed = photocraft_psd::TySh::from_bytes(tysh).ok()?;
    parsed.transform[4] += dx;
    parsed.transform[5] += dy;
    Some(parsed.to_bytes())
}

/// Serializes a `TySh` block.
pub fn write_tysh(t: &TySh) -> Vec<u8> {
    let warp = t
        .warp
        .clone()
        .unwrap_or_else(|| photocraft_psd::text_layer::warp_descriptor().descriptor);
    let mut block = photocraft_psd::TySh::new_text("", &[], t.transform.m, [0; 4]);
    block.descriptor.descriptor = t.text.clone();
    block.warp.descriptor = warp;
    block.set_bounds_points(t.bounds);
    block.to_bytes()
}

fn enumv(t: &str, v: &str) -> D {
    D::Enumerated { type_id: Id::new(t), value: Id::new(v) }
}

fn enum_value(d: &Descriptor, key: &str) -> Option<String> {
    match d.get(key) {
        Some(D::Enumerated { value, .. }) => Some(String::from_utf8_lossy(value.as_bytes()).into_owned()),
        _ => None,
    }
}

fn dnum(v: Option<&D>) -> Option<f64> {
    match v {
        Some(D::Double(x)) if x.is_finite() => Some(*x),
        Some(D::UnitFloat { value, .. }) if value.is_finite() => Some(*value),
        Some(D::Integer(i)) => Some(f64::from(*i)),
        _ => None,
    }
}

/// The EngineData of a text descriptor.
pub fn engine_data(text: &Descriptor) -> Option<E> {
    match text.get("EngineData") {
        Some(D::RawData(b)) => ed::parse(b).ok(),
        _ => None,
    }
}

/// Whether the text engine of a `TySh` block has nothing to draw: its EngineData text
/// (`EngineDict/Editor/Text`, what Photoshop lays out) is present and only whitespace. Some
/// Photoshop layers carry their characters only in the descriptor's `Txt ` and render nothing.
pub fn engine_text_is_blank(tysh: &[u8]) -> bool {
    parse_tysh(tysh)
        .and_then(|t| engine_data(&t.text))
        .and_then(|e| e.path(&["EngineDict", "Editor", "Text"]).and_then(E::as_str).map(|s| s.trim().is_empty()))
        .unwrap_or(false)
}

/// The descriptor's `Txt ` text of a `TySh` block: the layer's characters with `\r` line breaks and
/// no terminal `\r` (Photoshop writes the terminal one into EngineData's `Editor/Text` only).
pub fn tysh_text(tysh: &[u8]) -> Option<String> {
    photocraft_psd::TySh::from_bytes(tysh).ok()?.text()
}

/// Whether `tysh` describes `layer`'s current text: both the descriptor's `Txt ` and the
/// EngineData `Editor/Text` (which carries the terminal `\r`) must say what the model says.
/// Photoshop re-renders a type layer from the model when either copy disagrees, which moves text
/// whose stored pixels came from another layout, so a save must rebuild such a `TySh`.
pub fn tysh_text_matches(tysh: &[u8], layer: &TextLayer) -> bool {
    let want = layer.text.replace('\n', "\r");
    if tysh_text(tysh).as_deref() != Some(want.as_str()) {
        return false;
    }
    parse_tysh(tysh)
        .and_then(|t| engine_data(&t.text))
        .and_then(|e| e.path(&["EngineDict", "Editor", "Text"]).and_then(E::as_str).map(str::to_string))
        .is_none_or(|text| text == format!("{want}\r"))
}

/// The orientation a preserved `TySh` block's `Ornt` summary says, when it parses.
///
/// Photoshop composes from the engine data (`WritingDirection`) but refuses a document whose
/// summary disagrees with it ("program error"), so a save must compare this with the model.
pub fn tysh_orientation(tysh: &[u8]) -> Option<Orientation> {
    let t = photocraft_psd::TySh::from_bytes(tysh).ok()?;
    Some(match t.orientation() {
        photocraft_psd::text_layer::Orientation::Horizontal => Orientation::Horizontal,
        photocraft_psd::text_layer::Orientation::Vertical => Orientation::Vertical,
    })
}

/// Rewrites only a preserved `TySh` block's `Ornt` summary to `orientation`, keeping every other
/// byte (transform, bounds, `EngineData`, warp, trailing padding) as it is.
///
/// A block written by an older build (or imported from one) can say `Hrzn` while its engine data
/// composes `WritingDirection 2`; writing it back unchanged makes Photoshop refuse the document,
/// and rebuilding the whole block instead would drop Photoshop-only settings.
pub fn with_tysh_orientation(tysh: &[u8], orientation: Orientation) -> Option<Vec<u8>> {
    let mut t = photocraft_psd::TySh::from_bytes(tysh).ok()?;
    t.set_orientation(match orientation {
        Orientation::Horizontal => photocraft_psd::text_layer::Orientation::Horizontal,
        Orientation::Vertical => photocraft_psd::text_layer::Orientation::Vertical,
    });
    Some(t.to_bytes())
}

/// The text-space bounds a `TySh` block's descriptor stores (`bounds`), the value
/// [`build_tysh`] takes as its `ink`: keeping it when a stale block is rebuilt preserves the
/// layer's measured ink instead of falling back to its frame or the origin.
pub fn tysh_ink(tysh: &[u8]) -> Option<[f32; 4]> {
    let t = parse_tysh(tysh)?;
    let Some(D::Descriptor(rect)) = t.text.get("bounds") else { return None };
    let side = |key: &str| match rect.get(key) {
        Some(D::UnitFloat { value, .. }) | Some(D::Double(value)) if value.is_finite() => Some(*value as f32),
        Some(D::Integer(i)) => Some(*i as f32),
        _ => None,
    };
    Some([side("Left")?, side("Top ")?, side("Rght")?, side("Btom")?])
}


#[cfg(test)]
mod run_length_safety_tests {
    use super::*;

    #[test]
    fn utf16_lengths_stop_after_text_is_consumed() {
        assert_eq!(utf16_to_byte_lengths("é😀", &[1.0, 2.0, 1.0, 1.0e300]), [2, 4]);
        assert_eq!(utf16_to_byte_lengths("", &[1.0, 1.0e300]), [0]);
        assert_eq!(utf16_to_byte_lengths("abc", &[0.0, 1.0, 0.0, 1.0, 1.0, 1.0e300]), [0, 1, 0, 1, 1]);
        let zero_runs = vec![0.0; MAX_ENGINE_RUNS + 1];
        assert_eq!(utf16_to_byte_lengths("a", &zero_runs).len(), MAX_ENGINE_RUNS);
    }

    #[test]
    fn engine_run_length_array_conversion_is_bounded() {
        let values = E::Array(vec![E::Real(0.0); MAX_ENGINE_RUNS + 1]);
        assert_eq!(arr_f(Some(&values)).len(), MAX_ENGINE_RUNS);
    }
}

#[cfg(test)]
mod transform_safety_tests {
    use super::*;

    #[test]
    fn unsafe_affine_terms_are_not_editable_type() {
        let mut t = TySh { transform: Affine::IDENTITY, text: Descriptor::new("TxLr"), warp: None, bounds: [0.0; 4] };
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1.315776645269349e62, 1.4759871577491642e307] {
            for i in 0..6 {
                t.transform = Affine::IDENTITY;
                t.transform.m[i] = bad;
                assert!(parse_tysh(&write_tysh(&t)).is_none(), "term {i}: {bad}");
            }
        }
        t.transform = Affine { m: [0.0, -2.01242653341062, 2.286521576443544, 0.0, 586.63, 2452.60] };
        assert_eq!(parse_tysh(&write_tysh(&t)).unwrap().transform, t.transform);
    }
}

#[cfg(test)]
mod nonfinite_tests {
    use super::*;
    use crate::engine_data::parse;

    #[test]
    fn non_finite_style_numbers_fall_back_to_defaults() {
        let d = parse(b"<< /FontSize 1e999 /Tracking nan /HorizontalScale inf /Kerning -inf /FillColor << /Values [1.0 nan 0.0 0.0] >> >>").unwrap();
        let s = char_style(&[], &d, &[], 1.0);
        let dflt = CharStyle::default();
        assert_eq!(s.size_pt, dflt.size_pt);
        assert_eq!((s.tracking, s.horizontal_scale, s.kern), (0.0, 1.0, 0.0));
        assert_eq!(s.color, Color::BLACK);
        // A finite f64 too big for f32 is dropped as well.
        let d = parse(b"<< /FontSize 1e300 /StartIndent inf >> ").unwrap();
        assert_eq!(char_style(&[], &d, &[], 1.0).size_pt, dflt.size_pt);
        assert_eq!(para_style(&[], &d, 1.0).start_indent_pt, 0.0);
        let d = parse(b"<< /FontSize 24.0 >>").unwrap();
        assert_eq!(char_style(&[], &d, &[], 1.0).size_pt, 24.0);
    }

    /// #9470: `BaselineDirection` sets vertical type's roman text upright (1, what Photoshop's
    /// Vertical Type tool writes in the run) or rotated (2); a run without one takes the style
    /// sheet's (Photoshop's default sheet says 2). It is written back.
    #[test]
    fn baseline_direction_round_trips() {
        let sheet = parse(b"<< /BaselineDirection 2 >>").unwrap();
        let upright = parse(b"<< /BaselineDirection 1 >>").unwrap();
        assert!(!char_style(&[Some(&sheet)], &upright, &[], 1.0).rotate_roman, "the run's own value wins");
        let plain = parse(b"<< /FontSize 24.0 >>").unwrap();
        assert!(char_style(&[Some(&sheet)], &plain, &[], 1.0).rotate_roman, "else the sheet's");
        assert!(!char_style(&[], &plain, &[], 1.0).rotate_roman, "none at all: upright");
        for rotate in [false, true] {
            let s = CharStyle { rotate_roman: rotate, ..Default::default() };
            let data = style_sheet_data(&s, 0, 1.0);
            assert_eq!(data.get("BaselineDirection").and_then(E::as_i64), Some(if rotate { 2 } else { 1 }));
            assert_eq!(char_style(&[Some(&sheet)], &data, &[], 1.0).rotate_roman, rotate);
        }
    }

    /// #5645: `FontBaseline` (1 superscript, 2 subscript) is read and written back.
    #[test]
    fn font_baseline_round_trips() {
        use photocraft_doc::text::FontBaseline;
        let d = parse(b"<< /FontBaseline 2 >>").unwrap();
        assert_eq!(char_style(&[], &d, &[], 1.0).baseline, FontBaseline::Subscript);
        let d = parse(b"<< /FontBaseline 1 >>").unwrap();
        assert_eq!(char_style(&[], &d, &[], 1.0).baseline, FontBaseline::Superscript);
        let s = CharStyle { baseline: FontBaseline::Subscript, ..Default::default() };
        let back = char_style(&[], &style_sheet_data(&s, 0, 1.0), &[], 1.0);
        assert_eq!(back.baseline, FontBaseline::Subscript);
    }

    #[test]
    fn area_path_that_is_not_finite_saves_as_point_text() {
        let path = photocraft_doc::vector::Path::new(vec![photocraft_doc::vector::Subpath::polygon(&[(0.0, 0.0), (f64::NAN, 0.0), (10.0, f64::INFINITY)])]);
        let layer = TextLayer {
            text: "x".into(),
            runs: vec![TextRun { len: 1, style: CharStyle { size_pt: 12.0, ..Default::default() } }],
            shape: TextShape::Area { path },
            ..Default::default()
        };
        let out = String::from_utf8_lossy(&crate::engine_data::write(&build_engine_data(&layer, None, 72.0))).into_owned();
        assert!(out.contains("/PointBase") && !out.contains("/BoxBounds"), "{out}");
        // Check number tokens, not substrings: `GridInfo` contains "inf".
        for token in out.split(|c: char| c.is_whitespace() || "[]()<>/".contains(c)) {
            if let Ok(number) = token.parse::<f64>() {
                assert!(number.is_finite(), "non-finite token {token:?} in {out}");
            }
        }
        let t = parse_tysh(&build_tysh(&layer, 72.0, None)).unwrap();
        for key in ["bounds", "boundingBox"] {
            let Some(D::Descriptor(d)) = t.text.get(key) else { panic!("no {key}") };
            for side in ["Left", "Top ", "Rght", "Btom"] {
                assert!(matches!(d.get(side), Some(D::UnitFloat { value, .. }) if value.is_finite()), "{key} {side}: {:?}", d.get(side));
            }
        }
    }

    #[test]
    fn warp_values_beyond_f32_clamp_to_the_warp_range() {
        let warp = Descriptor::new("warp")
            .with("warpStyle", enumv("warpStyle", "warpArc"))
            .with("warpValue", D::Double(1e39))
            .with("warpPerspective", D::Double(-1e39))
            .with("warpPerspectiveOther", D::Double(250.0))
            .with("warpRotate", enumv("Ornt", "Hrzn"));
        let tysh = write_tysh(&TySh { transform: Affine::IDENTITY, text: Descriptor::new("TxLr"), warp: Some(warp), bounds: [0.0; 4] });
        let w = text_layer_from_tysh(&tysh, 72.0).and_then(|l| l.warp).unwrap();
        assert_eq!((w.value, w.horizontal_distortion, w.vertical_distortion), (100.0, -100.0, 100.0));
    }

    #[test]
    fn infinite_dpi_does_not_write_a_zero_size() {
        let layer =
            TextLayer { text: "x".into(), runs: vec![TextRun { len: 1, style: CharStyle { size_pt: 12.0, ..Default::default() } }], ..Default::default() };
        let out = String::from_utf8_lossy(&crate::engine_data::write(&build_engine_data(&layer, None, f32::INFINITY))).into_owned();
        assert!(out.contains("/FontSize 12.0"), "{out}");
    }
}

/// What an exported type layer must say for Photoshop to accept it and re-render it in place:
/// the run tables cover the engine text (terminal `\r` included), the `TySh` text is the model's
/// current text (not a stale template copy), the descriptor bounds are the layout's ink in text
/// space rather than the box frame, and a new box layer's `Txt2` object is drawn in a frame that
/// holds its rectangle (Photoshop opens a frame without a path as point text and a re-render then
/// collapses centred text towards the origin — the user's "boxes shift left" report).
#[cfg(test)]
mod export_validity_tests {
    use super::*;

    fn run(len: usize) -> TextRun {
        TextRun { len, style: CharStyle::default() }
    }

    fn layer(text: &str, shape: TextShape) -> TextLayer {
        TextLayer { text: text.into(), runs: vec![run(text.len())], paragraphs: Vec::new(), shape, ..Default::default() }
    }

    fn boxed(text: &str) -> TextLayer {
        layer(text, TextShape::Box { x: 0.0, y: 0.0, width: 544.0, height: 308.0 })
    }

    fn engine_runs(e: &E) -> (String, Vec<i64>, Vec<i64>) {
        let ed = e.path(&["EngineDict"]).expect("EngineDict");
        let text = ed.path(&["Editor", "Text"]).and_then(E::as_str).expect("Editor/Text").to_string();
        let lens = |key: &str| {
            ed.path(&[key, "RunLengthArray"]).and_then(E::as_array).expect(key).iter().map(|v| v.as_i64().expect("a run length")).collect()
        };
        (text, lens("StyleRun"), lens("ParagraphRun"))
    }

    /// Item `i` of the array `v`.
    fn arr(v: &E, i: usize) -> Option<&E> {
        v.as_array()?.get(i)
    }

    fn u16(s: &str) -> i64 {
        s.encode_utf16().count() as i64
    }

    fn rect(t: &Descriptor, key: &str) -> [f64; 4] {
        let Some(D::Descriptor(d)) = t.get(key) else { panic!("no {key}") };
        let side = |k: &str| match d.get(k) {
            Some(D::UnitFloat { value, .. }) | Some(D::Double(value)) => *value,
            Some(D::Integer(i)) => *i as f64,
            other => panic!("{key} {k}: {other:?}"),
        };
        [side("Left"), side("Top "), side("Rght"), side("Btom")]
    }

    #[test]
    fn engine_run_lengths_cover_the_text_including_the_terminal_cr() {
        // "фывфывыф" is eight characters; the engine text adds the terminal `\r`, so ten units is
        // impossible for it and nine is right (measured: python3 len -> 8, utf-16 -> 8).
        for text in ["фывфывыф", "фывфывыф\n\nфывафыа", "abc\n", "a😀b", "This wraps\nin a box"] {
            let l = boxed(text);
            let (ed_text, style, para) = engine_runs(&build_engine_data(&l, None, 72.0));
            assert_eq!(ed_text, format!("{}\r", text.replace('\n', "\r")), "engine text for {text:?}");
            assert_eq!(style.iter().sum::<i64>(), u16(&ed_text), "style runs for {text:?}: {style:?}");
            assert_eq!(para.iter().sum::<i64>(), u16(&ed_text), "paragraph runs for {text:?}: {para:?}");
        }
        // The user's multi-paragraph layer: "фывфывыф\r\rфывафыа\r" is 18 UTF-16 units and, with
        // the model's per-paragraph runs, the paragraph runs are one per paragraph, [9, 1, 8].
        let mut l = boxed("фывфывыф\n\nфывафыа");
        l.paragraphs = vec![
            ParagraphRun { len: 17, style: ParagraphStyle::default() },
            ParagraphRun { len: 1, style: ParagraphStyle::default() },
            ParagraphRun { len: 14, style: ParagraphStyle::default() },
        ];
        let (ed_text, _, para) = engine_runs(&build_engine_data(&l, None, 72.0));
        assert_eq!(u16(&ed_text), 18);
        assert_eq!(para, [9, 1, 8]);
    }

    #[test]
    fn a_stale_template_run_table_is_refitted_to_the_text() {
        // A template whose own run table disagrees with the model's text must not leak through:
        // Photoshop calls such a layer invalid.
        let stale = E::Dict(vec![
            (
                "EngineDict".into(),
                E::Dict(vec![
                    ("Editor".into(), E::Dict(vec![("Text".into(), E::String("old\r".into()))])),
                    (
                        "StyleRun".into(),
                        E::Dict(vec![("RunArray".into(), E::Array(vec![])), ("RunLengthArray".into(), E::Array(vec![E::Int(1)]))]),
                    ),
                    (
                        "ParagraphRun".into(),
                        E::Dict(vec![("RunArray".into(), E::Array(vec![])), ("RunLengthArray".into(), E::Array(vec![E::Int(7)]))]),
                    ),
                ]),
            ),
            ("ResourceDict".into(), E::dict()),
        ]);
        let l = boxed("фывфывыф");
        let (ed_text, style, para) = engine_runs(&build_engine_data(&l, Some(stale), 72.0));
        assert_eq!(u16(&ed_text), 9);
        assert_eq!(style.iter().sum::<i64>(), 9, "{style:?}");
        assert_eq!(para.iter().sum::<i64>(), 9, "{para:?}");
    }

    #[test]
    fn tysh_text_is_the_model_text_not_a_stale_copy() {
        let old = boxed("фывфывыф");
        let stale_raw = build_tysh(&old, 72.0, Some([0.0, -11.0, 60.0, 3.0]));
        let mut edited = boxed("фывфывыф\n\nфывафыа");
        edited.psd_raw = Some(std::sync::Arc::new(stale_raw));
        assert!(!tysh_text_matches(&edited.psd_raw.clone().unwrap(), &edited), "the old block must be seen as stale");

        let fresh = build_tysh(&edited, 72.0, Some([0.0, -11.0, 60.0, 40.0]));
        assert_eq!(tysh_text(&fresh).as_deref(), Some("фывфывыф\r\rфывафыа"));
        assert!(tysh_text_matches(&fresh, &edited));
        let (ed_text, style, para) = engine_runs(&engine_data(&parse_tysh(&fresh).unwrap().text).unwrap());
        assert_eq!(ed_text, "фывфывыф\r\rфывафыа\r");
        assert_eq!(style.iter().sum::<i64>(), 18);
        assert_eq!(para.iter().sum::<i64>(), 18);

        // And the Txt2 object this layer exports says the same text.
        let txt2 = parse_txt2(&build_txt2_at(&[(0, &edited)], None, 72.0)).unwrap();
        let object = txt2.path(&["1", "1"]).and_then(|a| arr(a, 0)).expect("object 0");
        assert_eq!(object.path(&["0", "0"]).and_then(E::as_str), Some("фывфывыф\r\rфывафыа\r"));
    }

    #[test]
    fn box_text_bounds_are_the_layout_ink_not_the_frame() {
        let ink = [-1.5, -2.0, 300.0, 120.0];
        let l = boxed("фывфывыф");
        let t = parse_tysh(&build_tysh(&l, 72.0, Some(ink))).unwrap();
        assert_eq!(rect(&t.text, "bounds"), ink.map(f64::from), "descriptor bounds must be the ink");
        assert_eq!(rect(&t.text, "boundingBox"), ink.map(f64::from), "descriptor boundingBox must be the ink");
        // The frame stays in EngineData's Photoshop cookie, where Photoshop reads it for box text.
        let ed = engine_data(&t.text).unwrap();
        let shape = ed
            .path(&["EngineDict", "Rendered", "Shapes", "Children"])
            .and_then(|a| arr(a, 0))
            .expect("a rendered shape");
        assert_eq!(shape.get("ShapeType").and_then(E::as_i64), Some(1));
        let photoshop = shape.path(&["Cookie", "Photoshop"]).expect("the Photoshop cookie");
        let bounds: Vec<f64> = photoshop.get("BoxBounds").and_then(E::as_array).expect("BoxBounds").iter().map(|v| v.as_f64().unwrap()).collect();
        assert_eq!(bounds, [0.0, 0.0, 544.0, 308.0]);

        // Point text keeps the ink too (it has no frame at all).
        let p = layer("фывфывыф", TextShape::Point);
        let t = parse_tysh(&build_tysh(&p, 72.0, Some(ink))).unwrap();
        assert_eq!(rect(&t.text, "bounds"), ink.map(f64::from));
    }

    #[test]
    fn a_new_box_layer_gets_a_box_frame_in_the_txt2_block() {
        let point = layer("abc", TextShape::Point);
        let native = build_txt2_at(&[(0, &point)], None, 72.0);
        let text_box = boxed("фывфывыф");
        let block = parse_txt2(&build_txt2_at(&[(0, &point), (1, &text_box)], Some(&native), 72.0)).unwrap();
        let frames = block.path(&["0", "8", "0"]).and_then(E::as_array).expect("the frame list");
        let frame = frames.get(1).expect("frame 1");
        // A text box's frame holds its rectangle path and the box kind; the placeholder point frame
        // (`/2` only) made Photoshop open the layer as point text and re-render it at the origin.
        let path = frame.path(&["0", "1", "0"]).and_then(E::as_array).expect("the frame path");
        let coords: Vec<f64> = path.iter().map(|v| v.as_f64().unwrap()).collect();
        assert_eq!(coords.len(), 32, "four straight segments: {coords:?}");
        assert_eq!(&coords[..8], [0.0, 0.0, 0.0, 0.0, 544.0, 0.0, 544.0, 0.0]);
        assert_eq!(frame.path(&["0", "2", "0"]).and_then(E::as_i64), Some(1), "the box frame kind");
        // The object is drawn in its own frame, in both its view and its rendered strike.
        let object = block.path(&["1", "1"]).and_then(|a| arr(a, 1)).expect("object 1");
        let view_frame = object.path(&["1", "0"]).and_then(|a| arr(a, 0)).and_then(|f| f.get("0")).and_then(E::as_i64);
        assert_eq!(view_frame, Some(1), "the view names the object's own frame");
        // When the template carries a rendered strike, it names the object's own frame too; a
        // strike left on the template's frame made Photoshop draw the box in a point frame.
        let strike_frame = object
            .path(&["1", "2"])
            .and_then(|a| arr(a, 0))
            .and_then(|s| s.get("6"))
            .and_then(|a| arr(a, 0))
            .and_then(|p| p.get("10"))
            .and_then(E::as_i64);
        assert!(strike_frame.is_none_or(|f| f == 1), "a rendered strike names the object's own frame, not {strike_frame:?}");
        // A point layer keeps the point frame Photoshop writes (`/2`, no path).
        assert!(frames[0].path(&["0", "1"]).is_none(), "point frames carry no path");
    }
}

use super::{
    Affine, AntiAlias, Caps, CharStyle, Color, ColorMode, D, Descriptor, E, FontBaseline, Kerning, Orientation, ParagraphRun, ParagraphStyle, Point, TextAlign,
    TextLayer, TextRun, TextShape, TextWarp, TySh, dnum, ed, engine_data, enum_value, guess_from_postscript, parse_tysh,
};

/// Parses the document's `Txt2` block (Photoshop's text engine data for all type layers).
pub fn parse_txt2(data: &[u8]) -> Option<E> {
    // Txt2 is a bare sequence of `/key value` pairs (no enclosing `<< >>`).
    let mut wrapped = Vec::with_capacity(data.len() + 4);
    wrapped.extend_from_slice(b"<<");
    wrapped.extend_from_slice(data);
    wrapped.extend_from_slice(b">>");
    ed::parse(&wrapped).ok().filter(|v| matches!(v, E::Dict(_)))
}

/// Applies the paragraph-sheet keys present in a `Txt2` paragraph run (`/5`) to `style`.
///
/// Photoshop's composer reads box text from `Txt2` (`docs/txt2-map.md`), so a paragraph value the
/// file stores there wins over the `EngineData` copy. Only keys the sheet carries are applied:
/// an absent key keeps the engine default the `EngineData` reader already resolved.
fn apply_txt2_para_sheet(style: &mut ParagraphStyle, sheet: &E) {
    let num = |key: &str| sheet.get(key).and_then(E::as_f64).filter(|v| v.is_finite()).map(|v| v as f32);
    let int = |key: &str| sheet.get(key).and_then(E::as_i64).and_then(|v| u8::try_from(v).ok());
    let flag = |key: &str| sheet.get(key).and_then(E::as_bool);
    match sheet.get("0").and_then(E::as_i64) {
        Some(1) => style.align = TextAlign::Right,
        Some(2) => style.align = TextAlign::Center,
        Some(3) => style.align = TextAlign::JustifyLeft,
        Some(4) => style.align = TextAlign::JustifyRight,
        Some(5) => style.align = TextAlign::JustifyCenter,
        Some(6) => style.align = TextAlign::JustifyAll,
        Some(_) => style.align = TextAlign::Left,
        None => {}
    }
    for (key, slot) in [
        ("1", &mut style.first_line_indent_pt),
        ("2", &mut style.start_indent_pt),
        ("3", &mut style.end_indent_pt),
        ("4", &mut style.space_before_pt),
        ("5", &mut style.space_after_pt),
        ("7", &mut style.auto_leading),
        ("14", &mut style.hyphenation_zone_pt),
    ] {
        if let Some(v) = num(key) {
            *slot = v;
        }
    }
    for (key, slot) in
        [("9", &mut style.hyphenate), ("15", &mut style.hyphenate_capitalized), ("21", &mut style.hanging_punctuation), ("29", &mut style.every_line_composer)]
    {
        if let Some(v) = flag(key) {
            *slot = v;
        }
    }
    for (key, slot) in [
        ("10", &mut style.hyphenate_word_size),
        ("11", &mut style.hyphenate_min_before),
        ("12", &mut style.hyphenate_min_after),
        ("13", &mut style.hyphen_limit),
    ] {
        if let Some(v) = int(key) {
            *slot = v;
        }
    }
    for (key, slot) in [("17", &mut style.justification_word), ("18", &mut style.justification_letter), ("19", &mut style.justification_glyph)] {
        let Some(values) = sheet.get(key).and_then(E::as_array) else { continue };
        for (out, value) in slot.iter_mut().zip(values.iter()) {
            if let Some(v) = value.as_f64().filter(|v| v.is_finite()) {
                *out = v as f32;
            }
        }
    }
}

/// Applies what Photoshop keeps only in `Txt2` to `layer`, read from `tysh`: the automatic
/// kerning modes (EngineData writes Optical and "0" alike as `AutoKerning true`) and the
/// OpenType settings besides (discretionary) ligatures: stylistic sets, contextual alternates,
/// swashes and the rest ([`TXT2_FEATURES`]), which Photoshop leaves out of EngineData. Only when
/// the layer's `TextIndex` entry still holds the layer's text (an edit elsewhere leaves `Txt2`
/// stale).
///
/// Observed layout: `Txt2 /1` is the list of text objects; an object's `/0` is its text model
/// with `/0` the text (`\r` breaks, trailing `\r`) and `/6 /0` its style runs, each
/// `<< /0 << /0 << … /6 <<style>> >> >> /1 utf16-length >>`; style key `/11` is the auto-kern
/// mode of the pair after each character: 0 manual, 1 metrics, 2 optical.
pub fn apply_txt2(layer: &mut TextLayer, tysh: &[u8], txt2: &E) {
    let Some(index) = parse_tysh(tysh).and_then(|t| match t.text.get("TextIndex") {
        Some(D::Integer(i)) => usize::try_from(*i).ok(),
        _ => None,
    }) else {
        return;
    };
    // The numbered object, else the older Photoshop's at `/DocumentObjects /TextObjects` (named
    // keys: run lengths under `Length`, the style under `RunData /StyleSheet /Features`, the
    // auto-kern mode under `AutoKern`).
    let numbered = txt2
        .path(&["1", "1"])
        .and_then(E::as_array)
        .and_then(|a| a.get(index))
        .and_then(|o| o.get("0"))
        .and_then(|model| Some((model, model.get("0").and_then(E::as_str)?, model.path(&["6", "0"]).and_then(E::as_array)?)));
    let legacy = || {
        let model = txt2.path(&["DocumentObjects", "TextObjects"]).and_then(E::as_array).and_then(|a| a.get(index))?.get("Model")?;
        Some((model.get("Text").and_then(E::as_str)?, model.path(&["StyleRun", "RunArray"]).and_then(E::as_array)?))
    };
    let (text, style_runs, is_numbered, model_paras) = match numbered {
        Some((model, text, runs)) => (text, runs, true, Some(model)),
        None => {
            let Some((text, runs)) = legacy() else { return };
            (text, runs, false, None)
        }
    };
    if text.strip_suffix('\r').unwrap_or(text).replace('\r', "\n") != layer.text {
        return;
    }
    let (len_key, style_path, kern_key): (&str, &[&str], &str) =
        if is_numbered { ("1", &["0", "0", "6"], "11") } else { ("Length", &["RunData", "StyleSheet", "Features"], "AutoKern") };
    let styles: Vec<(f64, Option<&E>)> = style_runs.iter().map(|r| (r.get(len_key).and_then(E::as_f64).unwrap_or(0.0), r.path(style_path))).collect();
    let lens = utf16_to_byte_lengths(text, &styles.iter().map(|m| m.0).collect::<Vec<_>>());
    // Txt2 style per byte segment, then split the model runs at segment boundaries.
    let mut segs: Vec<(usize, Option<&E>)> = lens.into_iter().zip(styles.iter().map(|m| m.1)).collect();
    segs.retain(|s| s.0 > 0);
    let runs = layer.char_runs();
    let mut out: Vec<TextRun> = Vec::new();
    let (mut si, mut seg_end) = (0usize, segs.first().map_or(usize::MAX, |s| s.0));
    let mut at = 0usize;
    for r in runs {
        let end = at + r.len;
        while at < end {
            while at >= seg_end && si + 1 < segs.len() {
                si += 1;
                seg_end = seg_end.saturating_add(segs.get(si).map_or(0, |s| s.0));
            }
            let piece_end = if at < seg_end { end.min(seg_end) } else { end };
            let mut style = r.style.clone();
            let txt2_style = segs.get(si).and_then(|s| s.1).filter(|_| at < seg_end);
            // Txt2 has the exact mode (EngineData's reading also marks neighbours of manual
            // kerns manual, like Photoshop's legacy reader).
            match txt2_style.and_then(|st| st.get(kern_key)).and_then(E::as_i64) {
                Some(0) => style.kerning = Kerning::Off,
                Some(1) => style.kerning = Kerning::Metrics,
                Some(2) => style.kerning = Kerning::Optical,
                _ => {}
            }
            if let Some(st) = txt2_style.filter(|_| is_numbered) {
                add_txt2_features(&mut style, st);
            }
            let len = piece_end - at;
            match out.last_mut() {
                Some(last) if last.style == style => last.len += len,
                _ => out.push(TextRun { len, style }),
            }
            at = piece_end;
        }
    }
    layer.runs = out;
    layer.sync_summary();
    // Paragraph sheets: histogram of what the file's `Txt2` paragraph runs say. They are the
    // composer's input, and `Txt2` keeps one key EngineData has no field for
    // (`hyphenate_capitalized`, `/15`; `docs/txt2-map.md` §3).
    if is_numbered && let Some(sheets) = model_paras.as_ref().and_then(|m| m.path(&["5", "0"]).and_then(E::as_array)) {
        let mut runs = layer.paragraph_runs();
        for (i, run) in runs.iter_mut().enumerate() {
            if let Some(sheet) = sheets.get(i).and_then(super::build::para_run_sheet) {
                apply_txt2_para_sheet(&mut run.style, sheet);
            }
        }
        layer.paragraphs = runs;
    }
}

/// The frame (`Txt2 /0 /8 /0` entry) of text object `index`: its rendered frame's `/10` names it
/// (with `named`, else the frame its view names; see [`object_frame`]).
fn txt2_frame(txt2: &E, index: usize, named: bool) -> Option<&E> {
    let object = txt2.path(&["1", "1"]).and_then(E::as_array)?.get(index)?;
    txt2.path(&["0", "8", "0"]).and_then(E::as_array)?.get(object_frame(object, named)?)?.get("0")
}

/// The number of the frame the text object `object` is drawn in: its rendered frame's `/10`, else
/// (with `named`) the frame its view names (`/1 /0 [ << /0 n >> ]`). Photoshop leaves the rendered
/// strikes out for type it did not lay out (`text/path-wave-open.psd`, whose EngineData text is
/// blank).
pub(super) fn object_frame(object: &E, named: bool) -> Option<usize> {
    let rendered = object
        .path(&["1", "2"])
        .and_then(E::as_array)
        .and_then(|s| s.first())
        .and_then(|s| s.get("6"))
        .and_then(E::as_array)
        .and_then(|parts| parts.iter().find_map(|f| f.get("10").and_then(E::as_i64)));
    let view = || named.then(|| object.path(&["1", "0"]).and_then(E::as_array).and_then(|f| f.first()).and_then(|f| f.get("0")).and_then(E::as_i64)).flatten();
    usize::try_from(rendered.or_else(view)?).ok()
}

/// The `TySh` of `tysh` and the `Txt2` frame of its text object (`named`: see [`object_frame`]),
/// when that object still holds `layer`'s text (an edit elsewhere leaves `Txt2` stale).
fn layer_frame<'a>(layer: &TextLayer, tysh: &[u8], txt2: &'a E, named: bool) -> Option<(TySh, &'a E)> {
    let t = parse_tysh(tysh)?;
    let index = match t.text.get("TextIndex") {
        Some(D::Integer(i)) => usize::try_from(*i).ok()?,
        _ => return None,
    };
    let text = txt2.path(&["1", "1"]).and_then(E::as_array).and_then(|a| a.get(index)).and_then(|o| o.path(&["0", "0"])).and_then(E::as_str)?;
    if text.strip_suffix('\r').unwrap_or(text).replace('\r', "\n") != layer.text {
        return None;
    }
    let frame = txt2_frame(txt2, index, named)?;
    Some((t, frame))
}

/// Puts `layer` on its path when its `Txt2` frame is type on a path (#7962): Photoshop keeps the
/// path only there, and EngineData describes the layer as a text box the size of the path's
/// bounds. The path is read into the `TySh` text space with the type's range on it (see
/// `psd_path`). `Ok(true)` when the layer is now path text, `Ok(false)` when its frame is
/// no path (or `Txt2` holds another text for it), `Err` with the reason when the path can't be
/// read: the layer then stays the box or point text EngineData describes.
pub fn apply_txt2_path(layer: &mut TextLayer, tysh: &[u8], txt2: &E) -> Result<bool, String> {
    let Some((_, frame)) = layer_frame(layer, tysh, txt2, true) else { return Ok(false) };
    if crate::psd_path::frame_kind(frame) != crate::psd_path::PATH_FRAME {
        return Ok(false);
    }
    layer.shape = crate::psd_path::read_shape(frame)?;
    Ok(true)
}

/// Places `layer` where Photoshop draws it when the text object's frame in `Txt2` is moved
/// (#1317). Recent Photoshop keeps a type layer's position partly in its `Txt2` frame: a matrix
/// (`/2 /2`) from the frame's space to the `TySh` text space and, for point text, the anchor
/// (`/0`), while EngineData's `PointBase` stays at the origin and `BoxBounds` stays in the
/// frame's space. Ignoring them put the text far from Photoshop's pixels (off the canvas, so
/// it "disappeared" as soon as an edit started), and point text a line too high and off its
/// alignment anchor. Only when the layer's `TextIndex` entry still holds the layer's text, as
/// for [`apply_txt2`], and the frame agrees with the `TySh` `bounds`.
pub fn apply_txt2_frame(layer: &mut TextLayer, tysh: &[u8], txt2: &E) {
    // Type on a path: [`apply_txt2_path`] already read the path through the frame's matrix.
    if matches!(layer.shape, TextShape::Path { .. }) {
        return;
    }
    let Some((t, frame)) = layer_frame(layer, tysh, txt2, false) else { return };
    let m = arr_f(frame.path(&["2", "2"]));
    let matrix = if m.len() == 6 && m.iter().all(|v| v.is_finite()) { Affine { m: [m[0], m[1], m[2], m[3], m[4], m[5]] } } else { Affine::IDENTITY };
    let at = match layer.shape {
        TextShape::Point | TextShape::Path { .. } => {
            let p = arr_f(frame.get("0"));
            if p.len() == 2 && p.iter().all(|v| v.is_finite()) { Affine::translate(p[0], p[1]) } else { Affine::IDENTITY }
        }
        TextShape::Box { .. } | TextShape::Area { .. } => Affine::IDENTITY,
    };
    let to_text = matrix.mul(&at);
    if to_text == Affine::IDENTITY || to_text.determinant().abs() <= 1e-9 {
        return;
    }
    // The frame's origin (point text: the anchor; box text: the box's top left) must land in
    // the text-space `bounds` Photoshop wrote beside it, and not already be there without it. A `TySh` PhotoCraft rewrote (an edited
    // or moved layer whose text is unchanged keeps its old `Txt2` object and frame) already has
    // the frame folded into its transform and its `bounds` near the origin: applying the frame
    // again would move it twice.
    let origin = match layer.shape {
        TextShape::Point | TextShape::Path { .. } => Point::new(0.0, 0.0),
        TextShape::Box { .. } | TextShape::Area { .. } => {
            let b = engine_data(&t.text)
                .map(|e| {
                    arr_f(
                        e.path(&["EngineDict", "Rendered", "Shapes", "Children"])
                            .and_then(E::as_array)
                            .and_then(|c| c.first())
                            .and_then(|s| s.path(&["Cookie", "Photoshop", "BoxBounds"])),
                    )
                })
                .unwrap_or_default();
            let [x, y, ..] = b[..] else { return };
            Point::new(x, y)
        }
    };
    let Some([l, top, r, bottom]) = descriptor_rect(&t.text, "bounds") else { return };
    let inside = |p: Point| p.x >= l - 1.0 && p.x <= r + 1.0 && p.y >= top - 1.0 && p.y <= bottom + 1.0;
    if inside(origin) || !inside(to_text.apply(origin)) {
        return;
    }
    {
        // From the file's own transform: the frame's anchor is exact, so it replaces the
        // ink-box estimate `legacy_point_origin` made for such point text (#1469), which put
        // the baseline at the ink's top and the anchor at the ink's left whatever the alignment.
        layer.transform = t.transform.mul(&to_text);
    }
}

/// OpenType settings of a `Txt2` style that are on/off flags, by style key, as Photoshop
/// writes them (each turned on alone in the Character panel's OpenType settings and read back):
/// `/20` Contextual Alternates, `/23` Fractions, `/24` Ordinals, `/25` Swash, `/26` Titling
/// Alternates, `/28` Stylistic Alternates, `/29` Ornaments. (`/18` and `/19` are the standard and
/// discretionary ligatures, which EngineData carries too; `/27` is Arabic connection forms.)
pub const TXT2_FEATURES: [(&str, &str); 7] = [("20", "calt"), ("23", "frac"), ("24", "ordn"), ("25", "swsh"), ("26", "titl"), ("28", "salt"), ("29", "ornm")];

/// The `Txt2` style key holding the Stylistic Sets as a bit mask: bit n is set n + 1
/// (`ss01`..`ss20`).
pub const TXT2_STYLISTIC_SETS: &str = "85";

/// Sets in `style` the OpenType features a `Txt2` style dictionary turns on ([`TXT2_FEATURES`]
/// and the Stylistic Sets mask). A feature the style already sets keeps its value. A flag that is
/// off changes nothing, except Contextual Alternates: shapers apply `calt` by default, Photoshop
/// only when it is on (its default is off), so off is written as `calt` 0.
fn add_txt2_features(style: &mut CharStyle, txt2_style: &E) {
    let mut set = |tag: String, value: u16| {
        if !style.features.iter().any(|f| f.tag == tag) {
            style.features.push(photocraft_doc::text::FontFeature { tag, value });
        }
    };
    for (key, tag) in TXT2_FEATURES {
        match txt2_style.get(key).and_then(E::as_bool) {
            Some(true) => set(tag.to_string(), 1),
            Some(false) if tag == "calt" => set(tag.to_string(), 0),
            _ => {}
        }
    }
    // A mask beyond the 20 sets is not Photoshop's: ignore it.
    let sets = txt2_style.get(TXT2_STYLISTIC_SETS).and_then(E::as_i64).filter(|m| (0..1 << 20).contains(m)).unwrap_or(0);
    for n in 0..20 {
        if sets & (1 << n) != 0 {
            set(format!("ss{:02}", n + 1), 1);
        }
    }
}

/// Builds a [`TextLayer`] (model, text and transform; no cache, no `psd_raw`) from `TySh` data.
///
/// The reimplemented engine owns the parsing: this crate hands it a one-layer
/// document carrying the raw block and translates the [`LayerSpec`] it returns
/// onto the document model (`photocraft_text_re::psd::read_layer`).
pub fn text_layer_from_tysh(data: &[u8], dpi: f32) -> Option<TextLayer> {
    let t = parse_tysh(data)?;
    let spec = engine_layer(data, None)?;
    Some(finish_layer(layer_from_spec(&spec, &t, dpi)?, &t))
}

/// Whether a document `Txt2` block is one a CoolType reader accepts: Photoshop's `/98` version,
/// the document resources (`/0 /5`, `/0 /6`) and the text-engine state (`/1 /0`). A block without
/// them is refused at open with "General Photoshop error", and its objects are the stale text of
/// the save that wrote them, so the importer reads the layer from the `TySh` engine data instead.
pub fn txt2_is_valid(txt2: &[u8]) -> bool {
    parse_txt2(txt2).is_some_and(|p| {
        p.path(&["0", "5"]).is_some() && p.path(&["0", "6"]).is_some() && p.path(&["1", "0"]).is_some()
    })
}

/// [`text_layer_from_tysh`] with the document's `Txt2` block, the import path:
/// the engine applies the `Txt2` style, paragraph, frame and antialias data
/// itself, so no `apply_txt2*` step follows.
pub fn text_layer_from_tysh_with_txt2(data: &[u8], txt2: &[u8], dpi: f32) -> Option<TextLayer> {
    let t = parse_tysh(data)?;
    let spec = engine_layer(data, Some(txt2))?;
    Some(finish_layer(layer_from_spec(&spec, &t, dpi)?, &t))
}

/// A one-layer `PsdFile` carrying `tysh` (and, when given, the document's
/// `Txt2` block): the input the engine's reader takes.
fn engine_file(tysh: &[u8], txt2: Option<&[u8]>) -> Option<photocraft_psd::PsdFile> {
    use photocraft_psd::builder::{PixelData, PsdBuilder, TextLayerSpec};
    let mut builder = PsdBuilder::new(1, 1);
    builder
        .push_text_layer(TextLayerSpec {
            name: "Text".into(),
            rect: photocraft_psd::layer::Rect { top: 0, left: 0, bottom: 1, right: 1 },
            pixels: PixelData::Rgba8(vec![0, 0, 0, 0]),
            ty_sh: tysh.to_vec(),
            txt2: txt2.map(<[u8]>::to_vec),
        })
        .ok()?;
    builder.build().ok()
}

/// The engine's [`LayerSpec`](photocraft_text_re::psd::LayerSpec) for a `TySh`
/// block (and the optional `Txt2` block).
fn engine_layer(tysh: &[u8], txt2: Option<&[u8]>) -> Option<photocraft_text_re::psd::LayerSpec> {
    let file = engine_file(tysh, txt2)?;
    photocraft_text_re::psd::read_layer(&file, 0).ok()
}

/// The legacy point-text origin correction and the model summary, shared by both
/// read entry points.
fn finish_layer(mut layer: TextLayer, tysh: &TySh) -> TextLayer {
    // Some legacy PSDs store point text at a large local origin even though PointBase is zero.
    // Their transform maps that origin onto the canvas, but maps our zero-based layout far away.
    // Move the imported layout to the descriptor's ink origin so editing, engine commands, and
    // writing the TySh all agree on the visible position (#1469).
    if matches!(layer.shape, TextShape::Point)
        && let Some([x, y]) = legacy_point_origin(tysh)
    {
        let [a, b, c, d, e, f] = layer.transform.m;
        let corrected = [a, b, c, d, a * x + c * y + e, b * x + d * y + f];
        if corrected.iter().all(|v| v.is_finite()) {
            layer.transform.m = corrected;
        }
    }
    layer.sync_summary();
    layer
}

/// Maps the engine's [`LayerSpec`](photocraft_text_re::psd::LayerSpec) onto the
/// fork's model. `tysh` supplies the fields the engine keeps only in the `TySh`
/// descriptor (warp, the platform antialias modes), `dpi` the point conversion
/// (`k = 72 / dpi` on the engine's text-space units).
pub(crate) fn layer_from_spec(spec: &photocraft_text_re::psd::LayerSpec, tysh: &TySh, dpi: f32) -> Option<TextLayer> {
    let k = 72.0 / if dpi.is_finite() && dpi > 0.0 { dpi } else { 72.0 };
    let text = spec.text.replace('\r', "\n");
    let mut layer = TextLayer {
        text: text.clone(),
        transform: spec.transform.map_or(tysh.transform, |m| Affine { m }),
        orientation: if spec.orientation == Some(1) { Orientation::Vertical } else { Orientation::Horizontal },
        antialias: antialias_of(spec, tysh),
        shape: shape_of(spec),
        runs: run_styles_of(spec, &text, k),
        paragraphs: paragraphs_of(spec, tysh, &text, k),
        ..Default::default()
    };
    if let Some(w) = &tysh.warp {
        let style = enum_value(w, "warpStyle").unwrap_or_else(|| "warpNone".into());
        if style != "warpNone" {
            // Bend and distortion are percentages in -100..100 (as `type.warpText` clamps them);
            // clamping before the cast keeps a value beyond f32 from becoming inf.
            let pct = |key: &str| dnum(w.get(key)).unwrap_or(0.0).clamp(-100.0, 100.0) as f32;
            layer.warp = Some(TextWarp {
                style,
                value: pct("warpValue"),
                horizontal_distortion: pct("warpPerspective"),
                vertical_distortion: pct("warpPerspectiveOther"),
                horizontal: enum_value(w, "warpRotate").as_deref() != Some("Vrtc"),
            });
        }
    }
    layer.sync_summary();
    Some(layer)
}

/// The layer's shape: the engine's decoded path for type on a path, else its
/// frame rectangle as box text, else point text.
fn shape_of(spec: &photocraft_text_re::psd::LayerSpec) -> TextShape {
    // A box/area frame (kind 1) also carries its rectangle as a curve, so only a kind-2 frame is
    // type on a path: reading a box frame as a path turned every text box into path text on
    // import (and the round trip lost the box).
    let on_path = spec.shape.as_ref().is_some_and(|shape| shape.path.as_ref().is_some_and(|path| path.kind.as_deref() == Some("path")));
    if let Some(path) = on_path.then(|| spec.shape.as_ref().and_then(|shape| shape.path.as_ref())).flatten() {
        let knots: Vec<photocraft_doc::vector::Knot> = path
            .knots
            .iter()
            .map(|knot| photocraft_doc::vector::Knot {
                anchor: Point::new(f64::from(knot.anchor[0]), f64::from(knot.anchor[1])),
                in_ctrl: Point::new(f64::from(knot.in_ctrl[0]), f64::from(knot.in_ctrl[1])),
                out_ctrl: Point::new(f64::from(knot.out_ctrl[0]), f64::from(knot.out_ctrl[1])),
                smooth: knot.smooth.unwrap_or(false),
            })
            .collect();
        return TextShape::Path {
            path: photocraft_doc::vector::Subpath { closed: path.closed, knots, op: Default::default() },
            start: path.start,
            end: path.end,
            flip: path.flip,
        };
    }
    match spec.frame {
        Some([x0, y0, x1, y1]) => TextShape::Box { x: x0, y: y0, width: x1 - x0, height: y1 - y0 },
        None => TextShape::Point,
    }
}

/// Anti-alias mode: the descriptor's `AntA` first (it has the platform modes the
/// engine's numeric codes cannot express), then the engine's code, then smooth
/// as Photoshop's own default.
fn antialias_of(spec: &photocraft_text_re::psd::LayerSpec, tysh: &TySh) -> AntiAlias {
    match enum_value(&tysh.text, "AntA").as_deref() {
        Some("Anno" | "antiAliasNone") => AntiAlias::None,
        Some("antiAliasSharp" | "AnSh") => AntiAlias::Sharp,
        Some("AnCr" | "antiAliasCrisp") => AntiAlias::Crisp,
        Some("AnSt" | "antiAliasStrong") => AntiAlias::Strong,
        Some("antiAliasPlatformGray") => AntiAlias::Windows,
        Some("antiAliasPlatformLCD") => AntiAlias::WindowsLcd,
        Some(_) => AntiAlias::Smooth,
        None => match spec.aa {
            Some(0) => AntiAlias::None,
            Some(1) => AntiAlias::Crisp,
            Some(2) => AntiAlias::Strong,
            Some(4) => AntiAlias::Sharp,
            _ => AntiAlias::Smooth,
        },
    }
}

/// The `Txt2` `/6/38` language id as a BCP-47 tag; only the measured ids map,
/// unknown ones stay unset rather than guessing.
fn language_tag(code: i64) -> Option<String> {
    Some(match code {
        14 => "en-GB".to_string(),
        42 => "de-DE".to_string(),
        _ => return None,
    })
}

/// The engine's run lengths are UTF-16 units; the model's are UTF-8 bytes.
fn run_styles_of(spec: &photocraft_text_re::psd::LayerSpec, text: &str, k: f32) -> Vec<TextRun> {
    let lens = utf16_to_byte_lengths(text, &spec.runs.iter().map(|run| run.len as f64).collect::<Vec<_>>());
    spec.runs
        .iter()
        .zip(lens)
        .map(|(run, len)| TextRun { len, style: char_style_of(run, k) })
        .collect()
}

fn char_style_of(run: &photocraft_text_re::psd::RunSpec, k: f32) -> CharStyle {
    let mut style = CharStyle::default();
    if let Some(ps) = &run.font {
        let resolved = guess_from_postscript(ps);
        style.font_family = resolved.family;
        style.weight = resolved.weight;
        style.italic = resolved.italic;
        style.postscript_name = Some(ps.clone());
    }
    style.size_pt = run.size.map_or(style.size_pt, |v| v * k);
    // `AutoLeading` (Photoshop's auto leading) leaves the explicit `Leading` value
    // meaningless; the model spells auto leading as `None`. Writing `Some(0.0)` here
    // collapsed vertical columns (a column pitch of 0) after a PSD round trip.
    // `AutoLeading` means the model's `None`; an `EngineData` default sheet also
    // stores `Leading 0.0`, which is not an explicit leading either, so a zero
    // never becomes `Some(0.0)` (it collapsed vertical columns before).
    style.leading_pt =
        if run.auto_leading == Some(true) { None } else { run.leading.map(|v| v * k).filter(|v| *v != 0.0) };
    style.tracking = run.tracking.map_or(0.0, f32::from);
    style.baseline_shift_pt = run.baseline_shift.unwrap_or(0.0) * k;
    style.horizontal_scale = run.horizontal_scale.unwrap_or(1.0);
    style.vertical_scale = run.vertical_scale.unwrap_or(1.0);
    style.faux_bold = run.bold;
    style.faux_italic = run.italic;
    style.underline = run.underline;
    style.strikethrough = run.strikethrough;
    style.ligatures = run.ligatures.unwrap_or(true);
    style.discretionary_ligatures = run.alternate_ligatures.unwrap_or(false);
    style.kerning = match run.kerning_mode.or_else(|| run.auto_kerning.map(|auto| if auto { 1 } else { 0 })) {
        Some(2) => Kerning::Optical,
        Some(0) => Kerning::Off,
        _ => Kerning::Metrics,
    };
    style.kern = run.kerning.map_or(0.0, f32::from);
    style.caps = if run.all_caps {
        Caps::AllCaps
    } else if run.small_caps {
        Caps::SmallCaps
    } else {
        Caps::Normal
    };
    style.baseline = match run.baseline {
        Some(1) => FontBaseline::Superscript,
        Some(2) => FontBaseline::Subscript,
        _ => FontBaseline::Normal,
    };
    style.rotate_roman = run.rotate_roman.unwrap_or(false);
    style.features = run
        .features
        .iter()
        .filter(|feature| feature.value > 0)
        .map(|feature| photocraft_doc::text::FontFeature { tag: feature.tag.clone(), value: feature.value })
        .collect();
    if run.old_style {
        style.features.push(photocraft_doc::text::FontFeature { tag: "onum".into(), value: 1 });
    }
    style.language = run.language.and_then(language_tag);
    if let Some([r, g, b]) = run.color {
        style.color = Color::rgba(f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0, 1.0);
    }
    style
}

/// One paragraph run per paragraph line, with the engine's per-paragraph style
/// (a short list repeats its last entry, an empty one falls back to
/// [`LayerSpec::paragraph`]).
///
/// The run *lengths* come from the engine's own `ParagraphRun/RunLengthArray` (UTF-16 units,
/// converted to the model's UTF-8 bytes), not from splitting the text here: a layer whose model
/// puts a break at the start of the following run (a created or legacy layer) must keep that
/// split, and the engine run table is the only record of it. A paragraph run owns its trailing
/// break in the engine table, so the conversion covers the engine text's terminal `\r`; the
/// lengths are then capped to the model text (which carries `\n` breaks and no terminator).
fn paragraphs_of(spec: &photocraft_text_re::psd::LayerSpec, tysh: &TySh, text: &str, k: f32) -> Vec<ParagraphRun> {
    let engine = engine_data(&tysh.text);
    let ed_text = engine.as_ref().and_then(|e| e.path(&["EngineDict", "Editor", "Text"]).and_then(E::as_str)).unwrap_or("");
    let lens16: Vec<f64> = engine
        .as_ref()
        .and_then(|e| e.path(&["EngineDict", "ParagraphRun", "RunLengthArray"]).and_then(E::as_array))
        .map(|a| a.iter().filter_map(|v| v.as_i64()).map(|n| n as f64).collect())
        .unwrap_or_default();
    let mut lens: Vec<usize> = if lens16.is_empty() || ed_text.is_empty() {
        text.split_inclusive('\n').map(str::len).collect()
    } else {
        utf16_to_byte_lengths(ed_text, &lens16)
    };
    // Cap to the model text: the converted lengths include the engine's terminal break, and a
    // run that ends past the text (the last one always can) keeps only what the text holds.
    let mut at = 0usize;
    lens.retain_mut(|len| {
        let room = text.len().saturating_sub(at);
        *len = (*len).min(room);
        at += *len;
        room > 0
    });
    if lens.is_empty() {
        lens.push(0);
    }
    let paragraphs = &spec.paragraphs;
    lens.into_iter()
        .enumerate()
        .map(|(index, len)| {
            let para = paragraphs.get(index).or_else(|| paragraphs.last()).unwrap_or(&spec.paragraph);
            ParagraphRun { len, style: paragraph_style_of(para, k) }
        })
        .collect()
}

fn paragraph_style_of(paragraph: &photocraft_text_re::psd::ParagraphSpec, k: f32) -> ParagraphStyle {
    ParagraphStyle {
        // Measured code space (`out/verify/sync/justify_probe.py`):
        // 0 left, 1 right, 2 centre, 3 justify-last-left, 4 justify-last-right,
        // 5 justify-last-centre, 6 justify-all.
        align: match paragraph.justification {
            1 => TextAlign::Right,
            2 => TextAlign::Center,
            3 => TextAlign::JustifyLeft,
            4 => TextAlign::JustifyRight,
            5 => TextAlign::JustifyCenter,
            6 => TextAlign::JustifyAll,
            _ => TextAlign::Left,
        },
        first_line_indent_pt: paragraph.first_line_indent * k,
        start_indent_pt: paragraph.start_indent * k,
        end_indent_pt: paragraph.end_indent * k,
        space_before_pt: paragraph.space_before * k,
        space_after_pt: paragraph.space_after * k,
        auto_leading: paragraph.auto_leading,
        hyphenate: paragraph.hyphenate,
        hanging_punctuation: paragraph.hanging_punctuation,
        every_line_composer: paragraph.every_line_composer,
        // Photoshop's paragraph defaults (6/2/2/36) are not stored when they
        // are the default; a stored 0 means "unset", not zero.
        hyphenate_word_size: u8::try_from(paragraph.hyphenate_words_longer_than).ok().filter(|v| *v != 0).unwrap_or(6),
        hyphenate_min_before: u8::try_from(paragraph.hyphenate_after_first).ok().filter(|v| *v != 0).unwrap_or(2),
        hyphenate_min_after: u8::try_from(paragraph.hyphenate_before_last).ok().filter(|v| *v != 0).unwrap_or(2),
        hyphen_limit: u8::try_from(paragraph.hyphenate_limit).unwrap_or(0),
        hyphenation_zone_pt: if paragraph.hyphenate_zone != 0.0 { paragraph.hyphenate_zone * k } else { 36.0 },
        hyphenate_capitalized: paragraph.hyphenate_capital_words,
        justification_word: paragraph.word_spacing,
        justification_letter: paragraph.letter_spacing,
        justification_glyph: paragraph.glyph_scaling,
        burasagari: paragraph.burasagari,
        kinsoku_order: u8::try_from(paragraph.kinsoku_order).unwrap_or(0),
        ..Default::default()
    }
}

/// Refines a legacy point-text estimate after imported fonts and styles have been resolved.
/// Uses the rendering engine's font context; an accepted `Txt2` frame stays authoritative.
pub fn refine_legacy_point_origin(layer: &mut TextLayer, dpi: f32, engine: &mut crate::TextEngine) {
    let Some(raw) = layer.psd_raw.clone() else { return };
    let Some(t) = parse_tysh(&raw) else { return };
    let Some([x, y]) = legacy_point_origin(&t) else { return };
    if !matches!(layer.shape, TextShape::Point) {
        return;
    }
    let guessed = t.transform.mul(&Affine::translate(x, y));
    if layer.transform != guessed {
        return;
    }
    // `boundingBox` is the tight ink box (Photoshop writes the layout box as `bounds`), so match
    // it with the layout's ink, not its logical box: text whose ink starts below the ascent
    // line (`...`, `_`, lowercase) would otherwise land too low.
    let Some(ink) = crate::render::ink_bounds(&engine.layout(layer, dpi)) else { return };
    if !ink.iter().all(|v| v.is_finite()) {
        return;
    }
    let corrected = t.transform.mul(&Affine::translate(x - ink[0], y - ink[1]));
    if corrected.m.iter().all(|v| v.is_finite()) {
        layer.transform = corrected;
        // The frame (`Txt2`), when the file had one, is already folded into the layer's shape and
        // transform by `read_layer`, so the refinement must not reapply it.
    }
}

/// The local origin to fold into a legacy point-text transform, if present.
///
/// Photoshop normally keeps point text close to its PointBase. The affected legacy files instead
/// use a zero PointBase plus a very large `boundingBox` origin, with an equally remote transform
/// translation that cancels it out. Do not apply this to ordinary point text: its transform origin
/// is already the layout origin, and shifting it by the ink bounds would move the layer.
fn legacy_point_origin(t: &TySh) -> Option<[f64; 2]> {
    let e = engine_data(&t.text)?;
    let point_base = e
        .path(&["EngineDict", "Rendered", "Shapes", "Children"])
        .and_then(E::as_array)
        .and_then(|children| children.first())
        .and_then(|shape| shape.path(&["Cookie", "Photoshop", "PointBase"]))
        .and_then(E::as_array)?;
    let [px, py] = point_base else { return None };
    if px.as_f64() != Some(0.0) || py.as_f64() != Some(0.0) {
        return None;
    }
    let bounds = descriptor_rect(&t.text, "boundingBox")?;
    let [a, b, c, d, e, f] = t.transform.m;
    let values = [a, b, c, d, e, f, bounds[0], bounds[1]];
    if !values.iter().all(|v| v.is_finite()) {
        return None;
    }
    let shifted = [a * bounds[0] + c * bounds[1] + e, b * bounds[0] + d * bounds[1] + f];
    let origin_distance = e.abs().max(f.abs());
    let shifted_distance = shifted[0].abs().max(shifted[1].abs());
    // A local bounding-box origin in the thousands and a transform origin beyond a normal canvas
    // are the signature of this old writer. Requiring that the bounding-box origin is closer to
    // the document prevents applying the correction again after export/import.
    if !(bounds[0].abs().max(bounds[1].abs()) > 1024.0 && origin_distance > 4096.0 && shifted_distance < origin_distance) {
        return None;
    }

    Some([bounds[0], bounds[1]])
}

fn descriptor_rect(text: &Descriptor, key: &str) -> Option<[f64; 4]> {
    let D::Descriptor(rect) = text.get(key)? else { return None };
    let bounds = [dnum(rect.get("Left"))?, dnum(rect.get("Top "))?, dnum(rect.get("Rght"))?, dnum(rect.get("Btom"))?];
    (bounds.iter().all(|v| v.is_finite()) && bounds[2] > bounds[0] && bounds[3] > bounds[1]).then_some(bounds)
}

pub(super) const MAX_ENGINE_RUNS: usize = 65_536;

pub(crate) fn arr_f(v: Option<&E>) -> Vec<f64> {
    v.and_then(E::as_array).map(|a| a.iter().take(MAX_ENGINE_RUNS).filter_map(E::as_f64).collect()).unwrap_or_default()
}

/// Converts UTF-16 run lengths over `text` into UTF-8 byte lengths, stopping after the first run
/// once `text` is exhausted so trailing runs past the text aren't processed.
pub(crate) fn utf16_to_byte_lengths(text: &str, lens: &[f64]) -> Vec<usize> {
    let mut out = Vec::with_capacity(lens.len().min(text.len().saturating_add(1)));
    let mut chars = text.chars().peekable();
    let mut exhausted = false;
    for &l in lens.iter().take(MAX_ENGINE_RUNS) {
        if exhausted {
            break;
        }
        let mut units = l.max(0.0) as usize;
        let mut bytes = 0;
        while units > 0 {
            match chars.next() {
                Some(c) => {
                    units = units.saturating_sub(c.len_utf16());
                    bytes += c.len_utf8();
                }
                None => {
                    exhausted = true;
                    break;
                }
            }
        }
        if chars.peek().is_none() {
            exhausted = true;
        }
        out.push(bytes);
    }
    out
}

fn lookup<'a>(run: &'a E, base: &[Option<&'a E>], key: &str) -> Option<&'a E> {
    run.get(key).or_else(|| base.iter().flatten().find_map(|b| b.get(key)))
}

pub(crate) fn char_style(base: &[Option<&E>], d: &E, fonts: &[String], k: f32) -> CharStyle {
    let g = |key: &str| lookup(d, base, key);
    let num = |key: &str| g(key).and_then(E::as_f64);
    // An f32 that a huge finite f64 overflowed to infinity is dropped too.
    let numk = |key: &str, k: f32| num(key).map(|v| v as f32 * k).filter(|v| v.is_finite());
    let flag = |key: &str| g(key).and_then(E::as_bool);
    let mut s = CharStyle::default();
    if let Some(ps) = g("Font").and_then(E::as_i64).and_then(|i| usize::try_from(i).ok()).and_then(|i| fonts.get(i)) {
        let r = guess_from_postscript(ps);
        s.font_family = r.family;
        s.weight = r.weight;
        s.italic = r.italic;
        s.postscript_name = Some(ps.clone());
    }
    if let Some(v) = numk("FontSize", k) {
        s.size_pt = v;
    }
    if flag("AutoLeading") == Some(false)
        && let Some(v) = numk("Leading", k)
    {
        s.leading_pt = Some(v);
    }
    s.tracking = numk("Tracking", 1.0).unwrap_or(0.0);
    s.baseline_shift_pt = numk("BaselineShift", k).unwrap_or(0.0);
    s.horizontal_scale = numk("HorizontalScale", 1.0).unwrap_or(1.0);
    s.vertical_scale = numk("VerticalScale", 1.0).unwrap_or(1.0);
    s.faux_bold = flag("FauxBold").unwrap_or(false);
    s.faux_italic = flag("FauxItalic").unwrap_or(false);
    s.underline = flag("Underline").unwrap_or(false);
    s.strikethrough = flag("Strikethrough").unwrap_or(false);
    s.ligatures = flag("Ligatures").unwrap_or(true);
    s.discretionary_ligatures = flag("DLigatures").unwrap_or(false);
    for (key, tag) in OPENTYPE_KEYS {
        if flag(key) == Some(true) {
            s.features.push(photocraft_doc::text::FontFeature { tag: tag.to_string(), value: 1 });
        }
    }
    s.kerning = if flag("AutoKerning") == Some(false) { Kerning::Off } else { Kerning::Metrics };
    s.kern = numk("Kerning", 1.0).unwrap_or(0.0);
    s.caps = match g("FontCaps").and_then(E::as_i64) {
        Some(1) => Caps::SmallCaps,
        Some(2) => Caps::AllCaps,
        _ => Caps::Normal,
    };
    s.baseline = match g("FontBaseline").and_then(E::as_i64) {
        Some(1) => FontBaseline::Superscript,
        Some(2) => FontBaseline::Subscript,
        _ => FontBaseline::Normal,
    };
    // Vertical type's roman text: 2 rotated along the column, 1 upright (#9470). A run without
    // its own value takes the style sheet's, which Photoshop writes as 2.
    s.rotate_roman = g("BaselineDirection").and_then(E::as_i64) == Some(2);
    if let Some(c) = g("FillColor") {
        // Positions matter (alpha, then the channels): a non-finite entry voids the colour.
        let v: Vec<f64> = c.get("Values").and_then(E::as_array).map(|a| a.iter().filter_map(E::as_f64).collect()).unwrap_or_default();
        let n_raw = c.get("Values").and_then(E::as_array).map_or(0, <[E]>::len);
        let v = if v.len() == n_raw && v.iter().all(|x| (*x as f32).is_finite()) { v } else { Vec::new() };
        let a = v.first().copied().unwrap_or(1.0) as f32;
        s.color = match v.len() {
            5 => Color { mode: ColorMode::Cmyk, c: [v[1] as f32, v[2] as f32, v[3] as f32, v[4] as f32], alpha: a },
            2 => Color { mode: ColorMode::Grayscale, c: [v[1] as f32, 0.0, 0.0, 0.0], alpha: a },
            4 => Color::rgba(v[1] as f32, v[2] as f32, v[3] as f32, a),
            _ => Color::BLACK,
        };
    }
    s
}

pub(crate) fn para_style(base: &[Option<&E>], d: &E, k: f32) -> ParagraphStyle {
    let num = |key: &str| lookup(d, base, key).and_then(E::as_f64);
    let numk = |key: &str, k: f32| num(key).map(|v| v as f32 * k).filter(|v| v.is_finite());
    let flag = |key: &str| lookup(d, base, key).and_then(E::as_bool);
    let int = |key: &str, default: u8| lookup(d, base, key).and_then(E::as_i64).and_then(|v| u8::try_from(v).ok()).unwrap_or(default);
    let triple = |key: &str, default: [f32; 3]| {
        let Some(values) = lookup(d, base, key).and_then(E::as_array) else { return default };
        let mut out = default;
        for (slot, value) in out.iter_mut().zip(values.iter()) {
            if let Some(v) = value.as_f64().map(|v| v as f32).filter(|v| v.is_finite()) {
                *slot = v;
            }
        }
        out
    };
    ParagraphStyle {
        align: match num("Justification").map(|v| v as i64) {
            Some(1) => TextAlign::Right,
            Some(2) => TextAlign::Center,
            Some(3) => TextAlign::JustifyLeft,
            Some(4) => TextAlign::JustifyRight,
            Some(5) => TextAlign::JustifyCenter,
            Some(6) => TextAlign::JustifyAll,
            _ => TextAlign::Left,
        },
        first_line_indent_pt: numk("FirstLineIndent", k).unwrap_or(0.0),
        start_indent_pt: numk("StartIndent", k).unwrap_or(0.0),
        end_indent_pt: numk("EndIndent", k).unwrap_or(0.0),
        space_before_pt: numk("SpaceBefore", k).unwrap_or(0.0),
        space_after_pt: numk("SpaceAfter", k).unwrap_or(0.0),
        auto_leading: numk("AutoLeading", 1.0).unwrap_or(1.2),
        hyphenate: flag("AutoHyphenate").unwrap_or(false),
        // Paragraph-panel depth (`docs/txt2-map.md`): an absent key keeps the default, which is
        // what Photoshop's own reader does (engine defaults, not the shared sheet's values).
        hanging_punctuation: flag("Hanging").unwrap_or(false),
        burasagari: flag("Burasagari").unwrap_or(false),
        kinsoku_order: int("KinsokuOrder", 0),
        every_line_composer: flag("EveryLineComposer").unwrap_or(false),
        // The DOM shows `HyphenatedWordSize` one letter smaller than the stored value in some
        // probes (`docs/txt2-map.md` §3); the model keeps the stored engine value, which is what
        // a PSD round trip must preserve.
        hyphenate_word_size: int("HyphenatedWordSize", 6),
        hyphenate_min_before: int("PreHyphen", 2),
        hyphenate_min_after: int("PostHyphen", 2),
        hyphen_limit: int("ConsecutiveHyphens", 0),
        hyphenation_zone_pt: numk("Zone", k).unwrap_or(36.0),
        justification_word: triple("WordSpacing", [0.8, 1.0, 1.33]),
        justification_letter: triple("LetterSpacing", [0.0, 0.0, 0.0]),
        justification_glyph: triple("GlyphSpacing", [1.0, 1.0, 1.0]),
        ..Default::default()
    }
}

// ---------- writing ----------

pub(super) fn real(v: f32) -> E {
    E::Real(f64::from(v))
}

/// Manual kerning as Photoshop stores it: a whole number of 1/1000 em (its UI range is ±1000;
/// anything non-finite writes 0).
pub(crate) fn kern_units(v: f32) -> i64 {
    if v.is_finite() { v.round().clamp(-10_000.0, 10_000.0) as i64 } else { 0 }
}

// Kerning in EngineData, as observed in Photoshop 2026 (writing, and reading files without
// `Txt2`): a manual kern k (1/1000 em) after character c is written on character c + 1 as
// `AutoKerning false` + `Kerning k`; everything else is `true` + 0. Photoshop's reader takes
// `Kerning k` on character j as the kern after j - 1 (making j - 1 manual), and
// `AutoKerning false` on j as "character j - 2 is manually kerned" (no automatic kerning). The
// automatic mode itself (Metrics or Optical) isn't in EngineData: Photoshop keeps it in the
// document's `Txt2` (see [`apply_txt2`]). We write and read the same way, so Photoshop reads
// our kerning as we do and Photoshop's own fields round-trip unchanged. (The mode of the last
// character has no slot and reads back as Metrics.)

pub(super) fn manual(s: &CharStyle) -> bool {
    s.kerning == Kerning::Off || kern_units(s.kern) != 0
}

/// PostScript name for a style (the PSD `FontSet` stores these).
pub(crate) fn postscript_for(s: &CharStyle) -> String {
    if let Some(ps) = &s.postscript_name {
        return ps.clone();
    }
    let fam = if s.font_family.is_empty() { crate::fonts::DEFAULT_FAMILY } else { &s.font_family };
    let w = match s.weight {
        0..=150 => "Thin",
        151..=250 => "ExtraLight",
        251..=350 => "Light",
        351..=450 => "Regular",
        451..=550 => "Medium",
        551..=650 => "SemiBold",
        651..=750 => "Bold",
        751..=850 => "ExtraBold",
        _ => "Black",
    };
    let style = match (w, s.italic) {
        ("Regular", true) => "Italic".to_string(),
        (w, true) => format!("{w}Italic"),
        (w, false) => w.to_string(),
    };
    format!("{}-{style}", fam.replace(' ', ""))
}

/// EngineData style keys for the Type › OpenType toggles and the OpenType feature each one turns
/// on (the standard and discretionary ligature keys map to dedicated [`CharStyle`] fields).
pub const OPENTYPE_KEYS: [(&str, &str); 8] = [
    ("ContextualLigatures", "calt"),
    ("Swash", "swsh"),
    ("OldStyle", "onum"),
    ("StylisticAlternates", "salt"),
    ("Titling", "titl"),
    ("Ornaments", "ornm"),
    ("Ordinals", "ordn"),
    ("Fractions", "frac"),
];

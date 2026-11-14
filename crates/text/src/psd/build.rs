use super::edit::{TEMPLATE_NODES, edited_object, fits, is_stub, names_one_frame, new_object, pad_frames, td_edited_object, with_modes};
use super::import::{OPENTYPE_KEYS, TXT2_FEATURES, TXT2_STYLISTIC_SETS, kern_units, manual, object_frame, parse_txt2, postscript_for, real};

use super::{
    AntiAlias, Caps, CharStyle, Color, ColorMode, D, Descriptor, E, FontBaseline, Kerning, Orientation, ParagraphStyle, TextAlign, TextLayer, TextShape, ed,
    enum_value, enumv,
};

/// A fill colour as EngineData and `Txt2` store it: the colour type (0 gray, 1 RGB, 2 CMYK) and
/// its values, alpha first.
fn fill_color(c: &Color) -> (i64, Vec<E>) {
    match c.mode {
        ColorMode::Cmyk => (2, vec![real(c.alpha), real(c.c[0]), real(c.c[1]), real(c.c[2]), real(c.c[3])]),
        ColorMode::Grayscale => (0, vec![real(c.alpha), real(c.c[0])]),
        _ => {
            let [r, g, b] = c.to_rgb();
            (1, vec![real(c.alpha), real(r), real(g), real(b)])
        }
    }
}

pub(crate) fn style_sheet_data(s: &CharStyle, font: usize, k: f32) -> E {
    let (color_type, values) = fill_color(&s.color);
    let mut opentype: Vec<(String, E)> = OPENTYPE_KEYS
        .iter()
        .filter(|(_, tag)| s.features.iter().any(|f| f.tag == *tag && f.value > 0))
        .map(|(key, _)| (key.to_string(), E::Bool(true)))
        .collect();
    let mut dict = vec![
        ("Font".into(), E::Int(font as i64)),
        ("FontSize".into(), real(s.size_pt / k)),
        ("FauxBold".into(), E::Bool(s.faux_bold)),
        ("FauxItalic".into(), E::Bool(s.faux_italic)),
        ("AutoLeading".into(), E::Bool(s.leading_pt.is_none())),
        ("Leading".into(), real(s.leading_pt.unwrap_or(0.0) / k)),
        ("HorizontalScale".into(), real(s.horizontal_scale)),
        ("VerticalScale".into(), real(s.vertical_scale)),
        ("Tracking".into(), E::Int(s.tracking.round() as i64)),
        ("AutoKerning".into(), E::Bool(s.kerning != Kerning::Off)),
        ("Kerning".into(), E::Int(kern_units(s.kern))),
        ("BaselineShift".into(), real(s.baseline_shift_pt / k)),
        (
            "FontCaps".into(),
            E::Int(match s.caps {
                Caps::Normal => 0,
                Caps::SmallCaps => 1,
                Caps::AllCaps => 2,
            }),
        ),
        (
            "FontBaseline".into(),
            E::Int(match s.baseline {
                FontBaseline::Normal => 0,
                FontBaseline::Superscript => 1,
                FontBaseline::Subscript => 2,
            }),
        ),
        ("BaselineDirection".into(), E::Int(if s.rotate_roman { 2 } else { 1 })),
        ("Underline".into(), E::Bool(s.underline)),
        ("Strikethrough".into(), E::Bool(s.strikethrough)),
        ("Ligatures".into(), E::Bool(s.ligatures)),
        ("DLigatures".into(), E::Bool(s.discretionary_ligatures)),
        // Photopea lays out edited text without painting it unless its fill is enabled.
        ("FillFlag".into(), E::Bool(true)),
        ("FillColor".into(), E::Dict(vec![("Type".into(), E::Int(color_type)), ("Values".into(), E::Array(values))])),
    ];
    dict.append(&mut opentype);
    E::Dict(dict)
}

pub(crate) fn paragraph_properties(p: &ParagraphStyle, k: f32) -> E {
    let j = match p.align {
        TextAlign::Left => 0,
        TextAlign::Right => 1,
        TextAlign::Center => 2,
        TextAlign::JustifyLeft => 3,
        TextAlign::JustifyRight => 4,
        TextAlign::JustifyCenter => 5,
        // The shared `EngineData`/`Txt2` code space measured with the 7-alignment
        // oracle (`out/verify/sync/justify_probe.py`): 3 justify-last-left,
        // 4 justify-last-right, 5 justify-last-centre, 6 justify-all.
        TextAlign::JustifyAll => 6,
    };
    let triple = |v: [f32; 3]| E::Array(v.iter().map(|x| real(*x)).collect());
    E::Dict(vec![
        ("Justification".into(), E::Int(j)),
        ("FirstLineIndent".into(), real(p.first_line_indent_pt / k)),
        ("StartIndent".into(), real(p.start_indent_pt / k)),
        ("EndIndent".into(), real(p.end_indent_pt / k)),
        ("SpaceBefore".into(), real(p.space_before_pt / k)),
        ("SpaceAfter".into(), real(p.space_after_pt / k)),
        ("AutoHyphenate".into(), E::Bool(p.hyphenate)),
        ("AutoLeading".into(), real(p.auto_leading)),
        ("LeadingType".into(), E::Int(0)),
        // Paragraph-panel depth: the model of Photoshop's `ParagraphSheet` (`docs/txt2-map.md`,
        // `docs/cooltype-re.md`). `Txt2` mirrors these in the paragraph sheet (`/5`).
        ("Hanging".into(), E::Bool(p.hanging_punctuation)),
        ("Burasagari".into(), E::Bool(p.burasagari)),
        ("KinsokuOrder".into(), E::Int(i64::from(p.kinsoku_order))),
        ("EveryLineComposer".into(), E::Bool(p.every_line_composer)),
        ("HyphenatedWordSize".into(), E::Int(i64::from(p.hyphenate_word_size))),
        ("PreHyphen".into(), E::Int(i64::from(p.hyphenate_min_before))),
        ("PostHyphen".into(), E::Int(i64::from(p.hyphenate_min_after))),
        ("ConsecutiveHyphens".into(), E::Int(i64::from(p.hyphen_limit))),
        ("Zone".into(), real(p.hyphenation_zone_pt / k)),
        ("WordSpacing".into(), triple(p.justification_word)),
        ("LetterSpacing".into(), triple(p.justification_letter)),
        ("GlyphSpacing".into(), triple(p.justification_glyph)),
    ])
}

pub(super) fn utf16_len(s: &str) -> i64 {
    s.encode_utf16().count() as i64
}

pub(crate) fn font_entry(name: &str) -> E {
    E::Dict(vec![("Name".into(), E::String(name.into())), ("Script".into(), E::Int(0)), ("FontType".into(), E::Int(1)), ("Synthetic".into(), E::Int(0))])
}

/// Keep imported font metadata while retaining the requested font indices. New fonts use
/// the normal defaults; duplicate records are consumed once before reusing the first match.
pub(crate) fn font_set(names: &[String], original: Option<&E>) -> E {
    let entries = original.and_then(E::as_array).unwrap_or_default();
    // Keep a queue per name so differently ordered ResourceDict/DocumentResources do not
    // collapse distinct duplicate metadata. Each index leaves its queue at most once.
    let mut by_name = std::collections::HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        if let Some(name) = entry.get("Name").and_then(E::as_str) {
            let (_, available) = by_name.entry(name).or_insert_with(|| (i, std::collections::VecDeque::new()));
            available.push_back(i);
        }
    }
    let mut used = vec![false; entries.len()];
    E::Array(
        names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let matches = |entry: &&E| entry.get("Name").and_then(E::as_str) == Some(name.as_str());
                let positional = entries.get(i).filter(matches).filter(|_| used.get(i) == Some(&false)).map(|_| i);
                let selected = positional.or_else(|| {
                    let (_, available) = by_name.get_mut(name.as_str())?;
                    while let Some(index) = available.pop_front() {
                        if used.get(index) == Some(&false) {
                            return Some(index);
                        }
                    }
                    None
                });
                if let Some(index) = selected
                    && let Some(used) = used.get_mut(index)
                {
                    *used = true;
                }
                selected
                    .or_else(|| by_name.get(name.as_str()).map(|(first, _)| *first))
                    .and_then(|index| entries.get(index))
                    .cloned()
                    .unwrap_or_else(|| font_entry(name))
            })
            .collect(),
    )
}

/// The `Txt2` auto-kern mode of a style: 0 manual, 1 metrics, 2 optical — the values
/// [`apply_txt2`] maps back to [`Kerning`]. EngineData's `AutoKerning` can't say Metrics from
/// Optical, so the mode only lives in `Txt2`.
fn txt2_mode(s: &CharStyle) -> i64 {
    if manual(s) {
        0
    } else if s.kerning == Kerning::Optical {
        2
    } else {
        1
    }
}

/// `Txt2` style runs for `layer`: `(mode, UTF-16 length)` over the engine text (`\r` breaks and
/// the trailing `\r`, which takes the last run's mode), split where the auto-kern mode changes.
fn txt2_mode_runs(layer: &TextLayer) -> Vec<(i64, usize)> {
    let runs = layer.char_runs();
    let ed_text = format!("{}\r", layer.text.replace('\n', "\r"));
    let mut out: Vec<(i64, usize)> = Vec::new();
    let (mut ri, mut run_end) = (0usize, runs.first().map_or(0, |r| r.len));
    for (b, ch) in ed_text.char_indices() {
        while b >= run_end && ri + 1 < runs.len() {
            ri += 1;
            run_end = run_end.saturating_add(runs.get(ri).map_or(0, |r| r.len));
        }
        let mode = runs.get(ri).map_or(1, |r| txt2_mode(&r.style));
        match out.last_mut() {
            Some(last) if last.0 == mode => last.1 += ch.len_utf16(),
            _ => out.push((mode, ch.len_utf16())),
        }
    }
    out
}

/// The engine text of `layer` (`\r` breaks and the trailing `\r`) as `(character run, UTF-16
/// length)` spans: one per run that has text.
fn char_spans(layer: &TextLayer) -> Vec<(usize, i64)> {
    let runs = layer.char_runs();
    let ed_text = format!("{}\r", layer.text.replace('\n', "\r"));
    let mut spans: Vec<(usize, i64)> = Vec::new();
    let (mut ri, mut run_end) = (0usize, runs.first().map_or(0, |r| r.len));
    for (b, ch) in ed_text.char_indices() {
        while b >= run_end && ri + 1 < runs.len() {
            ri += 1;
            run_end = run_end.saturating_add(runs.get(ri).map_or(0, |r| r.len));
        }
        let n = ch.len_utf16() as i64;
        match spans.last_mut() {
            Some(last) if last.0 == ri => last.1 += n,
            _ => spans.push((ri, n)),
        }
    }
    spans
}

/// `Txt2` style runs for `layer`, one per character run: `base` (one of the file's style runs,
/// `<< /0 << /0 << /6 style >> >> /1 length >>`, every key Photoshop wrote) with its style set
/// from the run's own: font (`/0`, an index into `fonts`, the block's `/0 /1 /0` font list, which
/// a missing font is appended to), size (`/1`), faux bold and italic, leading, scales and tracking
/// (`/2`–`/8`, EngineData's order), fill colour (`/53`), the OpenType keys and the auto-kern mode
/// (`/11`). Photoshop reads the character style from here rather than from EngineData, and quit
/// on a run holding only the auto-kern mode. `None` when `base` has no style dictionary or is
/// larger than a template may be (it is copied once per run).
fn txt2_style_runs(layer: &TextLayer, base: &E, fonts: &mut Vec<E>, dpi: f32, seed_empty: bool) -> Option<Vec<E>> {
    if !matches!(base.path(&["0", "0", "6"]), Some(E::Dict(_))) || !fits(base, &mut { TEMPLATE_NODES }) {
        return None;
    }
    let k = if dpi.is_finite() && dpi > 0.0 { dpi / 72.0 } else { 1.0 };
    let runs = layer.char_runs();
    let spans = char_spans(layer);
    let default = CharStyle::default();
    let mut out = Vec::with_capacity(spans.len());
    let mut looked_up = std::collections::HashMap::new();
    for (ri, n) in spans {
        let s = runs.get(ri).map_or(&default, |r| &r.style);
        let font = *looked_up.entry(postscript_for(s)).or_insert_with_key(|name| txt2_font(fonts, name, seed_empty));
        let mut run = base.clone();
        let inner = run.get_mut("0")?.get_mut("0")?;
        if inner.get("5").is_none() {
            inner.set("5", E::Int(1));
        }
        let style = run.get_mut("0")?.get_mut("0")?.get_mut("6")?;
        if let Some(font) = font {
            style.set("0", E::Int(font));
        }
        style.set("1", real(s.size_pt * k));
        style.set("2", E::Bool(s.faux_bold));
        style.set("3", E::Bool(s.faux_italic));
        style.set("4", E::Bool(s.leading_pt.is_none()));
        style.set("5", real(s.leading_pt.unwrap_or(0.0) * k));
        style.set("6", real(s.horizontal_scale));
        style.set("7", real(s.vertical_scale));
        style.set("8", E::Int(s.tracking.round() as i64));
        if let Some(paint @ E::Dict(_)) = style.get_mut("53").and_then(|p| p.get_mut("0")) {
            let (kind, values) = fill_color(&s.color);
            paint.set("0", E::Int(kind));
            paint.set("1", E::Array(values));
        }
        let feature = |tag: &str| s.features.iter().find(|f| f.tag == tag).map(|f| f.value > 0);
        for (key, tag) in TXT2_FEATURES {
            style.set(key, E::Bool(feature(tag).unwrap_or(tag == "calt")));
        }
        let sets = (0..20).filter(|n| feature(&format!("ss{:02}", n + 1)) == Some(true)).fold(0i64, |m, n| m | 1 << n);
        style.set(TXT2_STYLISTIC_SETS, E::Int(sets));
        style.set("11", E::Int(txt2_mode(s)));
        run.set("1", E::Int(n));
        out.push(run);
    }
    Some(out)
}

/// Whether the style runs of a `Txt2` text model (`/6 /0`) agree with `layer`'s character runs
/// on every key [`txt2_style_runs`] writes, the auto-kern mode aside ([`with_modes`] refits that
/// on its own): font (by name, through `fonts`, the block's font list), size, faux bold and
/// italic, leading, scales, tracking, fill colour and the OpenType keys. A key the file's run
/// lacks, or holds as another type, is no evidence of an edit and is skipped, so an unedited
/// save still gets the file's block back byte for byte (#2374); a key that disagrees means the
/// layer's style was edited since the file was written (#8415).
fn txt2_style_matches(layer: &TextLayer, model: &E, fonts: &[E], dpi: f32) -> bool {
    let Some(file_runs) = model.path(&["6", "0"]).and_then(E::as_array) else { return true };
    let k = if dpi.is_finite() && dpi > 0.0 { dpi / 72.0 } else { 1.0 };
    style_runs_match(layer, file_runs, "1", &["0", "0", "6"], |s, style| txt2_style_agrees(s, style, fonts, k))
}

/// [`txt2_style_matches`] for the older Photoshop's object at `/DocumentObjects /TextObjects`
/// (`entry`): its `Model /StyleRun /RunArray` runs hold the style under `RunData /StyleSheet
/// /Features`, named keys, with the run length under `Length`. `fonts` is the block's
/// `DocumentResources /FontSet /Resources`.
fn td_style_matches(layer: &TextLayer, entry: &E, fonts: &[E], dpi: f32) -> bool {
    let Some(file_runs) = entry.path(&["Model", "StyleRun", "RunArray"]).and_then(E::as_array) else { return true };
    let k = if dpi.is_finite() && dpi > 0.0 { dpi / 72.0 } else { 1.0 };
    style_runs_match(layer, file_runs, "Length", &["RunData", "StyleSheet", "Features"], |s, style| td_style_agrees(s, style, fonts, k))
}

/// Whether `file_runs` (a run array whose runs hold their length under `len_key` and their style
/// at `style_path`) agree, run for run over the layer's engine text, with the layer's character
/// runs by `agrees`. A run without a style dictionary says nothing.
fn style_runs_match(layer: &TextLayer, file_runs: &[E], len_key: &str, style_path: &[&str], agrees: impl Fn(&CharStyle, &E) -> bool) -> bool {
    let run_len = |i: usize| file_runs.get(i).and_then(|r| r.get(len_key)).and_then(E::as_i64).unwrap_or(0);
    let runs = layer.char_runs();
    let default = CharStyle::default();
    let ed_text = format!("{}\r", layer.text.replace('\n', "\r"));
    let (mut ri, mut run_end) = (0usize, runs.first().map_or(0, |r| r.len));
    let (mut fi, mut left) = (0usize, run_len(0));
    // Each (layer run, file run) pair is compared once, however long it is.
    let mut compared = None;
    for (b, ch) in ed_text.char_indices() {
        while b >= run_end && ri + 1 < runs.len() {
            ri += 1;
            run_end = run_end.saturating_add(runs.get(ri).map_or(0, |r| r.len));
        }
        while left <= 0 && fi + 1 < file_runs.len() {
            fi += 1;
            left = run_len(fi);
        }
        if compared != Some((ri, fi)) {
            let s = runs.get(ri).map_or(&default, |r| &r.style);
            if let Some(style) = file_runs.get(fi).and_then(|r| r.path(style_path))
                && !agrees(s, style)
            {
                return false;
            }
            compared = Some((ri, fi));
        }
        left = left.saturating_sub(ch.len_utf16() as i64);
    }
    true
}

/// Whether one `Txt2` style dictionary agrees with `s` (see [`txt2_style_matches`]); `k` is the
/// document's points per PostScript point, as in [`txt2_style_runs`].
fn txt2_style_agrees(s: &CharStyle, style: &E, fonts: &[E], k: f32) -> bool {
    let close = |a: f64, b: f64| (a - b).abs() <= 1e-3 * a.abs().max(b.abs()).max(1.0);
    let number = |key: &str, want: f32| style.get(key).and_then(E::as_f64).is_none_or(|v| close(v, f64::from(want)));
    let flag = |key: &str, want: bool| style.get(key).and_then(E::as_bool).is_none_or(|v| v == want);
    let font = style.get("0").and_then(E::as_i64).and_then(|i| usize::try_from(i).ok()).and_then(|i| fonts.get(i)).and_then(|f| f.path(&["0", "0", "0"]));
    if font.and_then(E::as_str).is_some_and(|name| name != postscript_for(s)) {
        return false;
    }
    let auto_leading = s.leading_pt.is_none();
    if !(number("1", s.size_pt * k) && flag("2", s.faux_bold) && flag("3", s.faux_italic) && flag("4", auto_leading)) {
        return false;
    }
    // An automatic leading's value is Photoshop's own; only a set one is ours to compare.
    if style.get("4").and_then(E::as_bool) == Some(false) && !auto_leading && !number("5", s.leading_pt.unwrap_or(0.0) * k) {
        return false;
    }
    if !(number("6", s.horizontal_scale) && number("7", s.vertical_scale) && number("8", s.tracking.round())) {
        return false;
    }
    if let Some(paint) = style.get("53").and_then(|p| p.get("0")) {
        let (kind, values) = fill_color(&s.color);
        if paint.get("0").and_then(E::as_i64).is_some_and(|v| v != kind) {
            return false;
        }
        if let Some(have) = paint.get("1").and_then(E::as_array) {
            let same =
                have.len() == values.len() && have.iter().zip(&values).all(|(a, b)| matches!((a.as_f64(), b.as_f64()), (Some(a), Some(b)) if close(a, b)));
            if !same {
                return false;
            }
        }
    }
    let feature = |tag: &str| s.features.iter().find(|f| f.tag == tag).map(|f| f.value > 0);
    if !TXT2_FEATURES.iter().all(|&(key, tag)| flag(key, feature(tag).unwrap_or(tag == "calt"))) {
        return false;
    }
    // A mask beyond the 20 sets is ignored on import (`add_txt2_features`), so here too.
    // Missing or invalid key 85 counts as 0 (the reader's default), not "any mask agrees".
    let sets = (0..20).filter(|n| feature(&format!("ss{:02}", n + 1)) == Some(true)).fold(0i64, |m, n| m | 1 << n);
    style.get(TXT2_STYLISTIC_SETS).and_then(E::as_i64).filter(|m| (0..1 << 20).contains(m)).unwrap_or(0) == sets
}

/// EngineData's `FontCaps` code of a style: 0 normal, 1 small caps, 2 all caps.
fn caps_code(s: &CharStyle) -> i64 {
    match s.caps {
        Caps::Normal => 0,
        Caps::SmallCaps => 1,
        Caps::AllCaps => 2,
    }
}

/// EngineData's `FontBaseline` code of a style: 0 normal, 1 superscript, 2 subscript.
fn baseline_code(s: &CharStyle) -> i64 {
    match s.baseline {
        FontBaseline::Normal => 0,
        FontBaseline::Superscript => 1,
        FontBaseline::Subscript => 2,
    }
}

/// The OpenType flags of a legacy style (`Features`), by key, and the feature each turns on: the
/// named keys of [`TXT2_FEATURES`].
const TD_FEATURES: [(&str, &str); 7] = [
    ("ContextualLigatures", "calt"),
    ("Fractions", "frac"),
    ("Ordinals", "ordn"),
    ("Swash", "swsh"),
    ("Titling", "titl"),
    ("StylisticAlternates", "salt"),
    ("Ornaments", "ornm"),
];

/// The legacy key of the Stylistic Sets bit mask, as [`TXT2_STYLISTIC_SETS`] is for `Txt2`'s own.
const TD_STYLISTIC_SETS: &str = "StylisticSets";

/// The PostScript name of font `index` in a legacy font list (`DocumentResources /FontSet
/// /Resources`: `<< /Resource << /StreamTag /CoolTypeFont /Identifier << /Name (name) /Type n >> >> >>`).
fn td_font_name(fonts: &[E], index: i64) -> Option<&str> {
    fonts.get(usize::try_from(index).ok()?)?.path(&["Resource", "Identifier", "Name"])?.as_str()
}

/// Whether one legacy `Features` dictionary agrees with `s`: [`txt2_style_agrees`] for the named
/// keys. A key the file's run lacks, or holds as another type, is no evidence of an edit.
fn td_style_agrees(s: &CharStyle, style: &E, fonts: &[E], k: f32) -> bool {
    let close = |a: f64, b: f64| (a - b).abs() <= 1e-3 * a.abs().max(b.abs()).max(1.0);
    let number = |key: &str, want: f32| style.get(key).and_then(E::as_f64).is_none_or(|v| close(v, f64::from(want)));
    let flag = |key: &str, want: bool| style.get(key).and_then(E::as_bool).is_none_or(|v| v == want);
    if style.get("Font").and_then(E::as_i64).and_then(|i| td_font_name(fonts, i)).is_some_and(|name| name != postscript_for(s)) {
        return false;
    }
    let auto_leading = s.leading_pt.is_none();
    if !(number("FontSize", s.size_pt * k) && flag("FauxBold", s.faux_bold) && flag("FauxItalic", s.faux_italic) && flag("AutoLeading", auto_leading)) {
        return false;
    }
    if style.get("AutoLeading").and_then(E::as_bool) == Some(false) && !auto_leading && !number("Leading", s.leading_pt.unwrap_or(0.0) * k) {
        return false;
    }
    if !(number("HorizontalScale", s.horizontal_scale) && number("VerticalScale", s.vertical_scale) && number("Tracking", s.tracking.round())) {
        return false;
    }
    // Per run, like the rest: a baseline shift, the case and baseline position, the underline and
    // strikethrough (a non-zero position is on, whichever position the file chose), the ligatures
    // and the auto-kern mode.
    let whole = |key: &str, want: i64| style.get(key).and_then(E::as_i64).is_none_or(|v| v == want);
    let on = |key: &str, want: bool| style.get(key).and_then(E::as_i64).is_none_or(|v| (v != 0) == want);
    if !(number("BaselineShift", s.baseline_shift_pt * k)
        && whole("FontCaps", caps_code(s))
        && whole("FontBaseline", baseline_code(s))
        && on("UnderlinePosition", s.underline)
        && on("StrikethroughPosition", s.strikethrough)
        && flag("Ligatures", s.ligatures)
        && flag("DiscretionaryLigatures", s.discretionary_ligatures)
        && whole("AutoKern", txt2_mode(s)))
    {
        return false;
    }
    if let Some(paint) = style.get("FillColor").and_then(|p| p.get("Color")) {
        let (kind, values) = fill_color(&s.color);
        if paint.get("Type").and_then(E::as_i64).is_some_and(|v| v != kind) {
            return false;
        }
        if let Some(have) = paint.get("Values").and_then(E::as_array) {
            let same =
                have.len() == values.len() && have.iter().zip(&values).all(|(a, b)| matches!((a.as_f64(), b.as_f64()), (Some(a), Some(b)) if close(a, b)));
            if !same {
                return false;
            }
        }
    }
    let feature = |tag: &str| s.features.iter().find(|f| f.tag == tag).map(|f| f.value > 0);
    if !TD_FEATURES.iter().all(|&(key, tag)| flag(key, feature(tag).unwrap_or(tag == "calt"))) {
        return false;
    }
    let sets = (0..20).filter(|n| feature(&format!("ss{:02}", n + 1)) == Some(true)).fold(0i64, |m, n| m | 1 << n);
    style.get(TD_STYLISTIC_SETS).and_then(E::as_i64).filter(|m| (0..1 << 20).contains(m)).is_none_or(|m| m == sets)
}

/// The index of the PostScript font `name` in a legacy font list, appending it (shaped like the
/// list's first entry, with its font type) when missing. `None` for an empty list.
fn td_font(fonts: &mut Vec<E>, name: &str) -> Option<i64> {
    if let Some(i) = fonts.iter().position(|f| f.path(&["Resource", "Identifier", "Name"]).and_then(E::as_str) == Some(name)) {
        return i64::try_from(i).ok();
    }
    let mut entry = fonts.first()?.clone();
    entry.get_mut("Resource")?.get_mut("Identifier")?.set("Name", E::String(name.to_string()));
    fonts.push(entry);
    i64::try_from(fonts.len() - 1).ok()
}

/// `entry` (an object at `/DocumentObjects /TextObjects`) with its `Model /StyleRun` runs remade
/// from the layer's character runs: the entry's first style run with its `Features` set from each
/// run's own, every other key Photoshop wrote kept, as [`txt2_style_runs`] does for the numbered
/// objects. Photoshop re-renders a type layer from this copy when the file has one, so a
/// style-only edit left here showed the old style on reopen. `None` when the first run has no
/// `Features` dictionary, or is larger than a template may be: the entry is then left as the file
/// has it.
fn td_restyled(entry: &E, layer: &TextLayer, fonts: &mut Vec<E>, dpi: f32) -> Option<E> {
    let base = entry.path(&["Model", "StyleRun", "RunArray"])?.as_array()?.first()?;
    if !matches!(base.path(&["RunData", "StyleSheet", "Features"]), Some(E::Dict(_))) || !fits(base, &mut { TEMPLATE_NODES }) {
        return None;
    }
    let k = if dpi.is_finite() && dpi > 0.0 { dpi / 72.0 } else { 1.0 };
    let runs = layer.char_runs();
    let default = CharStyle::default();
    let mut out = Vec::new();
    for (ri, n) in char_spans(layer) {
        let s = runs.get(ri).map_or(&default, |r| &r.style);
        let mut run = base.clone();
        let features = run.get_mut("RunData")?.get_mut("StyleSheet")?.get_mut("Features")?;
        if let Some(font) = td_font(fonts, &postscript_for(s)) {
            features.set("Font", E::Int(font));
        }
        features.set("FontSize", real(s.size_pt * k));
        features.set("FauxBold", E::Bool(s.faux_bold));
        features.set("FauxItalic", E::Bool(s.faux_italic));
        features.set("AutoLeading", E::Bool(s.leading_pt.is_none()));
        // An automatic leading's value is Photoshop's own (`.01` in the files seen): kept.
        if let Some(leading) = s.leading_pt {
            features.set("Leading", real(leading * k));
        }
        features.set("HorizontalScale", real(s.horizontal_scale));
        features.set("VerticalScale", real(s.vertical_scale));
        features.set("Tracking", E::Int(s.tracking.round() as i64));
        features.set("AutoKern", E::Int(txt2_mode(s)));
        features.set("BaselineShift", real(s.baseline_shift_pt * k));
        features.set("FontCaps", E::Int(caps_code(s)));
        features.set("FontBaseline", E::Int(baseline_code(s)));
        // On keeps the position the file chose (a non-zero value), else the first one.
        for (key, on) in [("UnderlinePosition", s.underline), ("StrikethroughPosition", s.strikethrough)] {
            let kept = features.get(key).and_then(E::as_i64).filter(|v| *v != 0).unwrap_or(1);
            features.set(key, E::Int(if on { kept } else { 0 }));
        }
        features.set("Ligatures", E::Bool(s.ligatures));
        features.set("DiscretionaryLigatures", E::Bool(s.discretionary_ligatures));
        if let Some(paint @ E::Dict(_)) = features.get_mut("FillColor").and_then(|p| p.get_mut("Color")) {
            let (kind, values) = fill_color(&s.color);
            paint.set("Type", E::Int(kind));
            paint.set("Values", E::Array(values));
        }
        let feature = |tag: &str| s.features.iter().find(|f| f.tag == tag).map(|f| f.value > 0);
        for (key, tag) in TD_FEATURES {
            features.set(key, E::Bool(feature(tag).unwrap_or(tag == "calt")));
        }
        let sets = (0..20).filter(|n| feature(&format!("ss{:02}", n + 1)) == Some(true)).fold(0i64, |m, n| m | 1 << n);
        features.set(TD_STYLISTIC_SETS, E::Int(sets));
        run.set("Length", E::Int(n));
        out.push(run);
    }
    let mut entry = entry.clone();
    entry.get_mut("Model")?.get_mut("StyleRun")?.set("RunArray", E::Array(out));
    Some(entry)
}

/// A `Txt2` font-list entry (`<< /0 << /99 /CoolTypeFont /0 << /0 name /2 type >> >> >>`).
fn txt2_font_entry(name: &str, kind: i64) -> E {
    E::Dict(vec![(
        "0".into(),
        E::Dict(vec![
            ("99".into(), E::Name("CoolTypeFont".into())),
            ("0".into(), E::Dict(vec![("0".into(), E::String(name.to_string())), ("2".into(), E::Int(kind))])),
        ]),
    )])
}

/// The index of the PostScript font `name` in a `Txt2` font list (`<< /0 << /99 /CoolTypeFont
/// /0 << /0 name /2 type >> >> >>` entries), appending it (shaped like the list's first entry)
/// when missing. An empty list gets a seeded CoolTypeFont entry only when `seed_empty` is set —
/// a PhotoCraft-native block (#10308); a file's own block keeps its `/0` as it was (#2374), so
/// `None` there, as before.
fn txt2_font(fonts: &mut Vec<E>, name: &str, seed_empty: bool) -> Option<i64> {
    let named = |f: &E| f.path(&["0", "0", "0"]).and_then(E::as_str) == Some(name);
    if let Some(i) = fonts.iter().position(named) {
        return i64::try_from(i).ok();
    }
    if fonts.is_empty() {
        if !seed_empty {
            return None;
        }
        fonts.push(txt2_font_entry(name, 0));
        return Some(0);
    }
    let mut entry = fonts.first()?.clone();
    let font = entry.get_mut("0")?.get_mut("0")?;
    let kind = font.get("2").cloned();
    *font = E::Dict(vec![("0".into(), E::String(name.to_string()))]);
    if let Some(kind) = kind {
        font.set("2", kind);
    }
    fonts.push(entry);
    i64::try_from(fonts.len() - 1).ok()
}

/// The highest text-object number honoured here (`TextIndex`, and the slot array [`build_txt2`]
/// sizes). PSD text objects are numbered densely from 0; a crafted file can name object
/// `i32::MAX`, and sizing the array by it would make every save allocate gigabytes.
pub const MAX_TEXT_INDEX: i32 = 0xFFFF;

/// Whether `style` carries an OpenType setting that lives only in `Txt2` (EngineData has no field
/// for stylistic sets or the [`TXT2_FEATURES`] flags). Defaults that match a plain style do not
/// count: Contextual Alternates on (or absent), and the other flags off.
fn txt2_only_opentype(style: &CharStyle) -> bool {
    let on = |tag: &str| style.features.iter().any(|f| f.tag == tag && f.value > 0);
    let off = |tag: &str| style.features.iter().any(|f| f.tag == tag && f.value == 0);
    if (1..=20).any(|n| on(&format!("ss{n:02}"))) {
        return true;
    }
    for (_, tag) in TXT2_FEATURES {
        if tag == "calt" {
            if off("calt") {
                return true;
            }
        } else if on(tag) {
            return true;
        }
    }
    false
}

/// Whether the document `Txt2` block is needed for `objects`: a non-Metrics auto-kern mode
/// (`Optical`) is exclusive to `Txt2` — EngineData's `AutoKerning` cannot distinguish Metrics from
/// Optical — and so are Stylistic Sets and the other [`TXT2_FEATURES`] flags (#10308). A fresh
/// document of ordinary Metrics text with no such OpenType settings has nothing to put in the
/// block. Writing Photoshop's empty `<< /0 << >> >>` stub anyway makes some readers (Photopea)
/// fail (#3514); the export omits the block for such a document. Type on a path needs it too: its
/// path and range live only in the block's frame (see `psd_path`).
pub fn needs_txt2(objects: &[(i32, &TextLayer)]) -> bool {
    objects.iter().any(|(_, l)| {
        matches!(l.shape, TextShape::Path { .. }) || l.char_runs().iter().any(|r| r.style.kerning == Kerning::Optical || txt2_only_opentype(&r.style))
    })
}

/// A `/53` SimplePaint dictionary (`<< /99 /SimplePaint /0 << >> >>`) for a fresh style run.
fn simple_paint_dict() -> E {
    E::Dict(vec![("99".into(), E::Name("SimplePaint".into())), ("0".into(), E::dict())])
}

/// A minimal `Txt2` style run Photoshop will read: the shape [`txt2_style_runs`] fills from a
/// layer when the file has no template object to copy (a PhotoCraft-native document). Includes an
/// empty `/53` SimplePaint dict so the fill colour can be written.
fn minimal_txt2_style_run() -> E {
    let style = E::Dict(vec![("0".into(), E::Int(0)), ("11".into(), E::Int(1)), ("53".into(), simple_paint_dict())]);
    // Photoshop's style-run wrapper: `/0 << /0 (name) /5 1 /6 <sheet> >>`. The `/5 1` marks the
    // run as a character run; a wrapper without it is not a style run Photoshop's composer
    // reads, and the object then composes with the shared default sheet (12 pt black).
    let data = E::Dict(vec![("0".into(), E::String(String::new())), ("5".into(), E::Int(1)), ("6".into(), style)]);
    E::Dict(vec![("0".into(), E::Dict(vec![("0".into(), data)])), ("1".into(), E::Int(1))])
}

/// A point-text object with a view naming frame `frame` and empty style runs, for a layer the
/// file's block has no object for and no template to copy (#10308, #7698). Style runs are filled
/// by [`txt2_style_runs`] next.
fn fresh_point_text_object(frame: usize, text: &str) -> E {
    let frame = i64::try_from(frame).unwrap_or(0);
    // `/2 [ ]` marks the composed-layout cache as deliberately absent ("not laid out"):
    // Photoshop recomposes such an object from its model. An object without the marker is
    // not recomposed from `/0` — it falls back to the shared default character sheet
    // (`/1/2`: 12 pt, black), which is the "wrong size/colour on edit" bug.
    let view = E::Dict(vec![
        ("0".into(), E::Array(vec![E::Dict(vec![("0".into(), E::Int(frame))])])),
        ("2".into(), E::Array(vec![])),
    ]);
    // One paragraph run per `\r`-terminated paragraph (lengths in UTF-16 code units), as
    // `edited_object` refits them.
    let paragraphs = text
        .split_inclusive('\r')
        .map(|p| {
            // Photoshop's nested paragraph run; the sheet is filled when the object is written.
            let data = E::Dict(vec![("0".into(), E::String(String::new()))]);
            E::Dict(vec![("0".into(), E::Dict(vec![("0".into(), data)])), ("1".into(), E::Int(i64::try_from(p.encode_utf16().count()).unwrap_or(i64::MAX)))])
        })
        .collect();
    let model = E::Dict(vec![
        ("0".into(), E::String(text.to_string())),
        ("5".into(), E::Dict(vec![("0".into(), E::Array(paragraphs))])),
        ("6".into(), E::Dict(vec![("0".into(), E::Array(vec![]))])),
    ]);
    E::Dict(vec![("0".into(), model), ("1".into(), view)])
}

/// Justification code a `Txt2` paragraph sheet stores in `/5/0`, the code space `EngineData`
/// shares (docs/txt2-map.md): 0 left, 1 right, 2 centre, 3 justify last centre, 4 justify last
/// right, 5 fully justified (which stretches the last line too). Photoshop has no code for
/// "justify last left"; the nearest is 3. The pre-migration writer shifted the justified modes by
/// one, which read back as another alignment.
fn para_align_code(a: TextAlign) -> i64 {
    match a {
        TextAlign::Left => 0,
        TextAlign::Right => 1,
        TextAlign::Center => 2,
        TextAlign::JustifyLeft => 3,
        TextAlign::JustifyRight => 4,
        TextAlign::JustifyCenter => 5,
        TextAlign::JustifyAll => 6,
    }
}

/// Inserts or replaces one key of a `Txt2` paragraph sheet, keeping Photoshop's own key order.
fn para_key(items: &mut Vec<(String, E)>, key: &str, value: E) {
    match items.iter_mut().find(|(k, _)| k == key) {
        Some(slot) => slot.1 = value,
        None => items.push((key.to_string(), value)),
    }
}

/// A `Txt2` paragraph sheet (`/1/1[i]/0/5/0[p]/0/0/5`) for `p`.
///
/// `Txt2` is what Photoshop's composer reads back (`docs/txt2-map.md`): paragraph indents,
/// spacing, justification and the hyphenation numbers must be written here as well as into
/// `EngineData`, or Photoshop opens the file with the engine defaults and warns that the stored
/// layout will change. `base` is the file's own sheet: its unknown keys (`/24`, `/25`, `/33`,
/// `/35`, `/40`, the CJK sets) are kept byte for byte, our keys are overwritten. Hyphenation
/// parameters (`/10`…`/15`) are only written when hyphenation is on, as Photoshop does.
fn txt2_para_sheet(p: &ParagraphStyle, base: Option<&E>) -> E {
    let mut items = match base {
        Some(E::Dict(items)) => items.clone(),
        _ => Vec::new(),
    };
    para_key(&mut items, "0", E::Int(para_align_code(p.align)));
    for (key, value) in [
        ("1", p.first_line_indent_pt),
        ("2", p.start_indent_pt),
        ("3", p.end_indent_pt),
        ("4", p.space_before_pt),
        ("5", p.space_after_pt),
        ("7", p.auto_leading),
    ] {
        para_key(&mut items, key, real(value));
    }
    if base.is_none() {
        para_key(&mut items, "8", E::Int(0));
    }
    para_key(&mut items, "9", E::Bool(p.hyphenate));
    if p.hyphenate {
        // `/10` stores the engine value; the DOM shows it as `hyphenateWordsLongerThan`
        // (`docs/txt2-map.md` §3: DOM 6 was written as `/10 7`).
        para_key(&mut items, "10", E::Int(i64::from(p.hyphenate_word_size)));
        para_key(&mut items, "11", E::Int(i64::from(p.hyphenate_min_before)));
        para_key(&mut items, "12", E::Int(i64::from(p.hyphenate_min_after)));
        para_key(&mut items, "13", E::Int(i64::from(p.hyphen_limit)));
        para_key(&mut items, "14", real(p.hyphenation_zone_pt));
        para_key(&mut items, "15", E::Bool(p.hyphenate_capitalized));
    }
    for (key, values) in [("17", p.justification_word), ("18", p.justification_letter), ("19", p.justification_glyph)] {
        para_key(&mut items, key, E::Array(values.iter().map(|v| real(*v)).collect()));
    }
    para_key(&mut items, "21", E::Bool(p.hanging_punctuation));
    para_key(&mut items, "29", E::Bool(p.every_line_composer));
    if base.is_none() {
        // Photoshop's own run sheet carries these constants (measured in `ps_defaults.psd`:
        // `/24 0 /25 0 /33 0 /35 0 /40 2`; `/27` and `/28` are the name `/nil`). Their meaning is
        // unmeasured, so a file's own values are kept above and only a fresh sheet gets the
        // numeric defaults; `/27` and `/28` are omitted and Photoshop defaults them.
        for (key, value) in [("24", 0), ("25", 0), ("33", 0), ("35", 0), ("40", 2)] {
            para_key(&mut items, key, E::Int(value));
        }
    }
    E::Dict(items)
}

/// The paragraph sheet of a `Txt2` paragraph run. Photoshop nests it twice
/// (`<< /0 << /0 << /0 () /5 <<sheet>> /6 1 >> >> /1 len >>`); this writer's own earlier form
/// put the data directly under `/0` (`<< /0 << /0 () /5 <<sheet>> /6 1 >> /1 len >>`), so both
/// are read.
pub(crate) fn para_run_sheet(run: &E) -> Option<&E> {
    run.path(&["0", "0", "5"]).or_else(|| run.path(&["0", "5"]))
}

/// `Txt2` paragraph runs of a layer: one per text paragraph, each with its UTF-16 length (the
/// object text is the layer's text with `\r` breaks and a terminal `\r`) and the paragraph style
/// at the paragraph's first byte. Photoshop stores one run per paragraph here whether or not the
/// model carries an explicit `ParagraphRun` for it.
fn txt2_paragraph_runs(layer: &TextLayer, base: Option<&[E]>) -> Vec<E> {
    let paragraphs = text_paragraphs(layer);
    let mut out = Vec::with_capacity(paragraphs.len());
    for (i, (style, len)) in paragraphs.iter().enumerate() {
        let base_run = base.and_then(|runs| runs.get(i));
        let sheet = txt2_para_sheet(style, base_run.and_then(para_run_sheet));
        // Photoshop nests the run's data twice (`<< /0 << /0 << /0 () /5 … /6 1 >> >> /1 len >>`).
        // A base run in this writer's older flat form (`<< /0 << /0 () /5 … /6 1 >> /1 len >>`) is
        // read and rewritten nested, so both forms round-trip and Photoshop reads the sheet.
        let mut data = match base_run.and_then(|run| run.path(&["0", "0"])) {
            Some(E::Dict(items)) => E::Dict(items.clone()),
            _ => match base_run.and_then(|run| run.path(&["0"])) {
                Some(E::Dict(items)) => E::Dict(items.clone()),
                _ => E::dict(),
            },
        };
        data.set("5", sheet);
        if data.get("0").is_none() {
            data.set("0", E::String(String::new()));
        }
        if data.get("6").is_none() {
            data.set("6", E::Int(1));
        }
        let mut entry = match base_run {
            Some(E::Dict(items)) => E::Dict(items.clone()),
            _ => E::dict(),
        };
        entry.set("0", E::Dict(vec![("0".into(), data)]));
        entry.set("1", E::Int(i64::try_from(*len).unwrap_or(0)));
        out.push(entry);
    }
    out
}

/// The paragraph style of each text paragraph in `layer`, with the paragraph's UTF-16 length
/// (terminal `\r` included): the styles a `Txt2` paragraph run table must carry.
fn text_paragraphs(layer: &TextLayer) -> Vec<(ParagraphStyle, usize)> {
    let runs = layer.paragraph_runs();
    let text = format!("{}\r", layer.text.replace('\n', "\r"));
    let mut out = Vec::new();
    let mut at = 0usize;
    for part in text.split_inclusive('\r') {
        let mut pos = 0usize;
        let mut style = runs.last().map(|r| r.style.clone()).unwrap_or_default();
        for run in &runs {
            if at < pos.saturating_add(run.len) {
                style = run.style.clone();
                break;
            }
            pos = pos.saturating_add(run.len);
        }
        out.push((style, part.encode_utf16().count()));
        at = at.saturating_add(part.len());
    }
    out
}

// ---------------------------------------------------------------------------
// The engine adapter: `TextLayer` → `photocraft_text_re::psd::LayerSpec`
// ---------------------------------------------------------------------------

/// The engine's layer model for a fork layer. This crate keeps no text
/// serialization logic of its own: `photocraft_text_re::psd` owns the
/// `EngineData`/`Txt2`/`TySh` writers, the translation below is the whole
/// adapter (units: `k = 72 / dpi`, the model stores points, the engine
/// text-space units).
pub(crate) fn spec_of_layer(layer: &TextLayer, dpi: f32) -> photocraft_text_re::psd::LayerSpec {
    let k = 72.0 / if dpi.is_finite() && dpi > 0.0 { dpi } else { 72.0 };
    let text = layer.text.replace('\n', "\r");
    let chars = layer.char_runs();
    // One entry per engine character: the model's characters plus the trailing
    // U+000D terminator (which takes the last style). `pair_runs`' form: a
    // character's EngineData `Kerning` is the space after the *previous*
    // character, so character j carries the model kern of j-1 and the manual
    // mode of j-2. The reader (`runs::from_engine_data`) maps it back the same
    // way, so a per-character round trip is exact.
    let mut entries: Vec<(CharStyle, usize, usize, i64, bool)> = Vec::new();
    let mut offset = 0usize;
    for (index, run) in chars.iter().enumerate() {
        let end = (offset + run.len).min(text.len()).max(offset);
        for ch in text[offset..end].chars() {
            entries.push((run.style.clone(), index, ch.len_utf16(), 0, true));
        }
        offset = end;
    }
    if let Some(last) = chars.last() {
        entries.push((last.style.clone(), chars.len() - 1, 1, 0, true));
    }
    let kern_units = |v: f32| if v.is_finite() { v.round().clamp(-10_000.0, 10_000.0) as i64 } else { 0 };
    let manual = |s: &CharStyle| s.kerning == Kerning::Off || kern_units(s.kern) != 0;
    for j in 0..entries.len() {
        let (_, _, _, kern, auto) = &entries[j];
        let _ = (kern, auto);
        let k = if j >= 1 { kern_units(entries[j - 1].0.kern) } else { 0 };
        let off = match j.checked_sub(2) {
            Some(p) => manual(&entries[p].0) && (kern_units(entries[p].0.kern) == 0 || k != 0),
            None => k != 0,
        };
        entries[j].3 = k;
        entries[j].4 = !off;
    }
    let mut runs: Vec<photocraft_text_re::psd::RunSpec> = Vec::new();
    let mut i = 0usize;
    while i < entries.len() {
        let key = (entries[i].1, entries[i].3, entries[i].4);
        let mut j = i;
        let mut len = 0usize;
        while j < entries.len() && (entries[j].1, entries[j].3, entries[j].4) == key {
            len += entries[j].2;
            j += 1;
        }
        let mut spec = run_spec(&entries[i].0, k);
        // The pair fields always win over the style's own kern: they are the
        // shifted, per-character values the reader maps back.
        spec.kerning = Some(entries[i].3.clamp(-32_768, 32_767) as i16);
        spec.auto_kerning = Some(entries[i].4);
        // The engine's run lengths include the terminal U+000D.
        spec.len = len.max(1);
        runs.push(spec);
        i = j;
    }
    if runs.is_empty() {
        runs.push(photocraft_text_re::psd::RunSpec { len: 1, ..run_spec(&CharStyle::default(), k) });
    }
    let paragraphs: Vec<photocraft_text_re::psd::ParagraphSpec> =
        layer.paragraph_runs().iter().map(|run| paragraph_spec(&run.style, k)).collect();
    let (frame, shape) = frame_of(&layer.shape);
    let preserve_trailing_break = text.ends_with('\r');
    photocraft_text_re::psd::LayerSpec {
        text,
        runs,
        paragraph: paragraphs.first().cloned().unwrap_or_default(),
        paragraphs,
        aa: antialias_code(layer.antialias),
        frame,
        shape,
        orientation: Some(if layer.orientation == Orientation::Vertical { 1 } else { 0 }),
        transform: Some(layer.transform.m),
        atlas: None,
        fractional: None,
        // The editor keeps the empty final paragraph a caret can sit on.
        preserve_trailing_break,
    }
}

/// One `RunSpec`: the character style, converted to engine units.
fn run_spec(style: &CharStyle, k: f32) -> photocraft_text_re::psd::RunSpec {
    let unit = |pt: f32| pt / k;
    let old_style = style.features.iter().any(|f| f.tag == "onum" && f.value > 0);
    let features: Vec<photocraft_text_re::runs::RunFeature> =
        style.features.iter().filter(|f| f.tag != "onum").map(|f| photocraft_text_re::runs::RunFeature { tag: f.tag.clone(), value: f.value }).collect();
    let (kerning_mode, kerning) = kerning_of(style);
    photocraft_text_re::psd::RunSpec {
        len: 0,
        font: Some(style.postscript_name.clone().unwrap_or_else(|| postscript_for(style))),
        size: Some(unit(style.size_pt)),
        leading: style.leading_pt.map(unit),
        auto_leading: Some(style.leading_pt.is_none()),
        tracking: Some(style.tracking.round() as i16),
        baseline_shift: Some(unit(style.baseline_shift_pt)),
        horizontal_scale: Some(style.horizontal_scale),
        vertical_scale: Some(style.vertical_scale),
        color: Some(color_rgb(&style.color)),
        auto_kerning: Some(matches!(style.kerning, Kerning::Metrics)),
        kerning,
        kerning_mode,
        bold: style.faux_bold,
        italic: style.faux_italic,
        underline: style.underline,
        strikethrough: style.strikethrough,
        // The `Txt2` `/6/15`-`/6/16` style codes are unknown to this adapter:
        // the underlying model reads them, but the app only carries the legacy
        // booleans, and deriving a code from a boolean would be a guess
        // (stroke parity is a documented residual).
        underline_type: None,
        strike_type: None,
        all_caps: matches!(style.caps, Caps::AllCaps),
        small_caps: matches!(style.caps, Caps::SmallCaps),
        old_style,
        ligatures: Some(style.ligatures),
        alternate_ligatures: Some(style.discretionary_ligatures),
        no_break: None,
        language: language_id(style.language.as_deref()),
        stroke: None,
        baseline: Some(match style.baseline {
            FontBaseline::Superscript => 1,
            FontBaseline::Subscript => 2,
            FontBaseline::Normal => 0,
        }),
        rotate_roman: Some(style.rotate_roman),
        features,
    }
}

/// The engine's kerning pair: a manual `kern` wins, else the mode
/// (`0` manual, `1` metrics, `2` optical).
fn kerning_of(style: &CharStyle) -> (Option<i64>, Option<i16>) {
    if style.kern != 0.0 && style.kern.is_finite() {
        // Photoshop's manual kern is a 1/1000 em value; the engine clamps it to
        // its widest representable pair kern (as `read_layer` validates).
        return (Some(0), Some(style.kern.round().clamp(-10_000.0, 10_000.0) as i16));
    }
    let mode = match style.kerning {
        Kerning::Optical => 2,
        Kerning::Off => 0,
        Kerning::Metrics => 1,
    };
    (Some(mode), None)
}

/// A style colour as the engine stores it: straight RGB, 8 bits per channel.
fn color_rgb(color: &Color) -> [u8; 3] {
    let [r, g, b] = color.to_rgb();
    let channel = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [channel(r), channel(g), channel(b)]
}

/// One `ParagraphSpec`: the paragraph style, converted to engine units.
fn paragraph_spec(style: &ParagraphStyle, k: f32) -> photocraft_text_re::psd::ParagraphSpec {
    let unit = |pt: f32| pt / k;
    photocraft_text_re::psd::ParagraphSpec {
        // The measured code space (`out/verify/sync/justify_probe.py`):
        // 0 left, 1 right, 2 centre, 3 justify-last-left, 4 justify-last-right,
        // 5 justify-last-centre, 6 justify-all.
        justification: match style.align {
            TextAlign::Left => 0,
            TextAlign::Right => 1,
            TextAlign::Center => 2,
            TextAlign::JustifyLeft => 3,
            TextAlign::JustifyRight => 4,
            TextAlign::JustifyCenter => 5,
            TextAlign::JustifyAll => 6,
        },
        every_line_composer: style.every_line_composer,
        first_line_indent: unit(style.first_line_indent_pt),
        start_indent: unit(style.start_indent_pt),
        end_indent: unit(style.end_indent_pt),
        space_before: unit(style.space_before_pt),
        space_after: unit(style.space_after_pt),
        auto_leading: style.auto_leading,
        word_spacing: style.justification_word,
        letter_spacing: style.justification_letter,
        glyph_scaling: style.justification_glyph,
        hanging_punctuation: style.hanging_punctuation,
        hyphenate: style.hyphenate,
        hyphenate_words_longer_than: i64::from(style.hyphenate_word_size),
        hyphenate_after_first: i64::from(style.hyphenate_min_before),
        hyphenate_before_last: i64::from(style.hyphenate_min_after),
        hyphenate_limit: i64::from(style.hyphen_limit),
        hyphenate_zone: unit(style.hyphenation_zone_pt),
        hyphenate_capital_words: style.hyphenate_capitalized,
        burasagari: style.burasagari,
        kinsoku_order: i64::from(style.kinsoku_order),
    }
}

/// The engine's anti-alias code (`0` none, `1` crisp, `2` strong, `3` smooth,
/// `4` sharp); the platform modes live only in the `TySh` descriptor's `AntA`
/// and have no code, so they stay unset here and are written by [`build_tysh`].
fn antialias_code(antialias: AntiAlias) -> Option<i64> {
    Some(match antialias {
        AntiAlias::None => 0,
        AntiAlias::Crisp => 1,
        AntiAlias::Strong => 2,
        AntiAlias::Smooth => 3,
        AntiAlias::Sharp => 4,
        AntiAlias::Windows | AntiAlias::WindowsLcd => return None,
    })
}

/// The descriptor `AntA` value for a model anti-alias mode.
fn antialias_name(antialias: AntiAlias) -> &'static str {
    match antialias {
        AntiAlias::None => "Anno",
        AntiAlias::Sharp => "antiAliasSharp",
        AntiAlias::Crisp => "AnCr",
        AntiAlias::Strong => "AnSt",
        AntiAlias::Smooth => "AnSm",
        AntiAlias::Windows => "antiAliasPlatformGray",
        AntiAlias::WindowsLcd => "antiAliasPlatformLCD",
    }
}

/// The `Txt2` `/6/38` language id for a BCP-47 tag; only the measured ids map.
fn language_id(tag: Option<&str>) -> Option<i64> {
    match tag? {
        "en-GB" => Some(14),
        "de-DE" => Some(42),
        _ => None,
    }
}

/// The layer's frame and shape for the engine. Box and area text get their
/// rectangle as both the engine frame and a kind-1 box path; type on a path
/// keeps its curve, with the frame being the path's text-space bounds.
fn frame_of(shape: &TextShape) -> (Option<[f32; 4]>, Option<photocraft_text_re::psd::ShapeSpec>) {
    let bounds = |points: &[[f32; 2]]| -> Option<[f32; 4]> {
        let mut x0 = f32::INFINITY;
        let mut y0 = f32::INFINITY;
        let mut x1 = f32::NEG_INFINITY;
        let mut y1 = f32::NEG_INFINITY;
        for [x, y] in points {
            if !x.is_finite() || !y.is_finite() {
                return None;
            }
            x0 = x0.min(*x);
            y0 = y0.min(*y);
            x1 = x1.max(*x);
            y1 = y1.max(*y);
        }
        (x1 >= x0 && y1 >= y0).then_some([x0, y0, x1, y1])
    };
    // A shape with a non-finite point (`NaN`/`inf` from a malformed file) cannot
    // be authored: Photoshop would get a NaN frame. Degrade it to point text —
    // the engine then writes `PointBase` and finite descriptor bounds.
    let finite = |r: [f32; 4]| r.iter().all(|v| v.is_finite());
    match shape {
        TextShape::Point => (None, None),
        TextShape::Box { x, y, width, height } => {
            let frame = [*x, *y, x + width, y + height];
            if finite(frame) {
                (Some(frame), crate::psd_path::shape_spec(shape))
            } else {
                (None, None)
            }
        }
        TextShape::Area { path } => match path.control_bounds() {
            Some((x0, y0, x1, y1)) => {
                let frame = [x0 as f32, y0 as f32, x1 as f32, y1 as f32];
                if finite(frame) {
                    (Some(frame), crate::psd_path::shape_spec(shape))
                } else {
                    (None, None)
                }
            }
            None => (None, None),
        },
        TextShape::Path { .. } => {
            let (_, points) = crate::psd_path::shape_for_frame(shape);
            match bounds(&points) {
                Some(frame) => (Some(frame), crate::psd_path::shape_spec(shape)),
                None => (None, None),
            }
        }
    }
}

/// Photoshop's own scaffolding for a `Txt2` block (PS 20.0.2): the `/98` version, the document
/// resources under `/0` and the text-engine state under `/1`, with the object array, frame list
/// and font list empty. Photoshop refuses a block that lacks it ("General Photoshop error"), and
/// its CoolType reader walks the numbered `/0` and `/1` keys in order, so a native block starts
/// from this text (keeping Photoshop's key order) instead of a hand-built outer dictionary.
const NATIVE_TXT2_SCAFFOLD: &[u8] = include_bytes!("native_txt2.ps");

/// [`NATIVE_TXT2_SCAFFOLD`] parsed, or a bare `/98` block if the constant ever fails to parse.
fn native_scaffold() -> E {
    parse_txt2(NATIVE_TXT2_SCAFFOLD).unwrap_or_else(|| E::Dict(vec![("98".into(), E::Dict(vec![("0".into(), E::Int(7))]))]))
}

/// Add the scaffolding keys a numbered block needs to open in Photoshop, keeping everything the
/// block already holds. `/0 /5`, `/0 /6` and `/1 /0` are each required ("General Photoshop
/// error" when one is missing); the numbered keys stay in ascending order, which CoolType's
/// positional walk needs.
fn repair_scaffolding(block: E) -> E {
    let E::Dict(mut top) = block else { return block };
    let E::Dict(scaffold) = native_scaffold() else { return E::Dict(top) };
    for (key, wanted) in [("0", ["5", "6"].as_slice()), ("1", ["0", "2", "3"].as_slice())] {
        let Some(E::Dict(native)) = scaffold.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()) else { continue };
        let Some(slot) = top.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v) else {
            // The block has no `/0` or `/1` at all: take the scaffold's whole tree.
            top.push((key.to_string(), E::Dict(native)));
            continue;
        };
        let E::Dict(items) = slot else { continue };
        for want in wanted {
            if items.iter().any(|(k, _)| k == want) {
                continue;
            }
            if let Some((k, v)) = native.iter().find(|(k, _)| k == want) {
                items.push((k.clone(), v.clone()));
            }
        }
        items.sort_by_key(|(k, _)| k.parse::<u32>().unwrap_or(u32::MAX));
    }
    E::Dict(top)
}

/// The document `Txt2` block for `objects` (`(TextIndex, layer)`), starting from
/// `previous` so Photoshop's own objects and every unmodelled key survive.
pub fn build_txt2(objects: &[(i32, &TextLayer)], previous: Option<&[u8]>) -> Vec<u8> {
    build_txt2_at(objects, previous, 72.0)
}

/// [`build_txt2`] at `dpi` (the layer's point units become engine units).
pub fn build_txt2_at(objects: &[(i32, &TextLayer)], previous: Option<&[u8]>, dpi: f32) -> Vec<u8> {
    let mut prev = previous.and_then(parse_txt2);
    // A numbered block without Photoshop's scaffolding is refused at open ("General Photoshop
    // error"): our older native saves wrote only `/98`, a frame list and an object array, and a
    // CoolType reader walks the numbered `/0` and `/1` keys, so the block needs Photoshop's own
    // document resources (`/0 /5`, `/0 /6`) and text-engine state (`/1 /0`) as well. Such a block
    // is rebuilt from the scaffolding below instead of repaired in place; a Photoshop block (which
    // always has them) keeps its bytes as before (#2374).
    let numbered = prev.as_ref().is_some_and(|p| p.path(&["1", "1"]).and_then(E::as_array).is_some_and(|a| !a.is_empty()));
    let scaffolding = prev.as_ref().is_some_and(|p| {
        p.path(&["0", "5"]).is_some() && p.path(&["0", "6"]).is_some() && p.path(&["1", "0"]).is_some()
    });
    if numbered && !scaffolding {
        // Repair, do not discard: keep the file's own numbered keys (its fonts, frames, objects,
        // extras) and add the scaffolding keys CoolType needs. Dropping the block would lose the
        // file's objects and their composed caches; only the three keys below are required (each
        // proven by removing it and watching Photoshop refuse the file).
        prev = prev.map(repair_scaffolding);
    }
    let prev_objects: &[E] = prev.as_ref().and_then(|p| p.path(&["1", "1"]).and_then(E::as_array)).unwrap_or(&[]);
    let mut slots: Vec<E> = prev_objects.to_vec();
    // The older Photoshop's objects (#6971): named keys at `/DocumentObjects /TextObjects`
    // instead of the numbered `/1 /1`, one entry per layer by `TextIndex` position (the same
    // numbering the slots take), its text under `Model /Text` and its run lengths under `/Length`.
    // Photoshop re-renders a type layer from this copy when the file has one, so an edit that
    // left the old text here showed the old text back on reopen. The entries are only ever edited
    // in place, never grown: a layer the block has no entry for has no stale copy to fix.
    let mut td: Option<Vec<E>> = prev.as_ref().and_then(|p| p.path(&["DocumentObjects", "TextObjects"])).and_then(E::as_array).map(<[E]>::to_vec);
    let mut td_changed = false;
    // The legacy block's font list (`DocumentResources /FontSet /Resources`), which its style runs index.
    let mut td_fonts: Vec<E> =
        prev.as_ref().and_then(|p| p.path(&["DocumentResources", "FontSet", "Resources"])).and_then(E::as_array).map(<[E]>::to_vec).unwrap_or_default();
    let td_fonts_before = td_fonts.len();
    let mut fonts: Vec<E> = prev.as_ref().and_then(|p| p.path(&["0", "1", "0"]).and_then(E::as_array)).map(<[E]>::to_vec).unwrap_or_default();
    let fonts_before = fonts.len();
    // A PhotoCraft-native block: the document had no `Txt2` of its own (#10308). Only such a
    // block gets the fresh point-text objects, minimal style runs, seeded font list and frame
    // list below; a file's own block (Photoshop's, any era) is written as before (#2374, #7698).
    // A PhotoCraft-native block: one this save starts from nothing, or the engine's empty
    // scaffolding (no text object of either era). Only such a block gets the fresh frame and font
    // lists below; a file's own block (Photoshop's, any era) is written as before (#2374, #7698).
    let native = prev.as_ref().is_none_or(|p| {
        p.path(&["1", "1"]).and_then(E::as_array).is_none_or(<[E]>::is_empty)
            && p.path(&["DocumentObjects", "TextObjects"]).and_then(E::as_array).is_none_or(<[E]>::is_empty)
    });
    let seed_empty = native;
    // Looked up once per save, not per layer: a crafted block with many objects must not cost
    // a pass over all of them for every type layer.
    let all_stubs = prev_objects.iter().all(is_stub);
    // A template is copied once per new layer, so a large one is skipped (the stub is written).
    let template = prev_objects.iter().rev().find(|o| names_one_frame(o) && fits(o, &mut { TEMPLATE_NODES }) && edited_object(o, "\r").is_some());
    // The frame each written object is drawn in, with its layer's shape (type on a path lives
    // only in the frame, #7962).
    let mut frame_shapes: Vec<(usize, &TextShape, bool)> = Vec::new();
    // A new or rebuilt object must not be drawn in a frame another object is drawn in: the two
    // layers would share one shape. The frames the file's objects name are taken; a free one is
    // found past the frame list and the slots (`pad_frames` adds it).
    let frame_count = prev.as_ref().and_then(|p| p.path(&["0", "8", "0"])).and_then(E::as_array).map(<[E]>::len);
    let mut taken: std::collections::HashSet<usize> = prev_objects.iter().filter_map(|o| object_frame(o, true)).collect();
    let slot_count =
        objects.iter().filter_map(|(i, _)| usize::try_from(*i).ok().filter(|_| (0..=MAX_TEXT_INDEX).contains(i))).map(|i| i + 1).max().unwrap_or(0);
    let mut next_free = frame_count.unwrap_or(0).max(prev_objects.len()).max(slot_count);
    // The frame list grows only for the frames handed out here, never for a frame number an
    // object the file kept names past its list.
    let mut frames_needed = 0usize;
    for (index, layer) in objects {
        if !(0..=MAX_TEXT_INDEX).contains(index) {
            continue;
        }
        let text = format!("{}\r", layer.text.replace('\n', "\r"));
        let modes = txt2_mode_runs(layer);
        let at = *index as usize;
        // The older Photoshop's copy of this object (#6971), refitted like the numbered one
        // below. The entry's own text is the check, so a file a previous save left with a stale
        // copy beside a current `/1 /1` object is repaired here too.
        if let Some(entries) = td.as_mut() {
            let fixed = match entries.get(at) {
                Some(entry) if entry.path(&["Model", "Text"]).and_then(E::as_str) != Some(text.as_str()) => td_edited_object(entry, &text),
                _ => None,
            };
            if let Some(fixed) = fixed
                && let Some(slot) = entries.get_mut(at)
            {
                *slot = fixed;
                td_changed = true;
            }
            // And its character style, when it no longer says what the layer says: a style-only
            // edit (font, size, faux bold, …) left the old style here, and Photoshop reads it back.
            if let Some(entry) = entries.get_mut(at)
                && !td_style_matches(layer, entry, &td_fonts, dpi)
                && let Some(restyled) = td_restyled(entry, layer, &mut td_fonts, dpi)
            {
                *entry = restyled;
                td_changed = true;
            }
        }
        // A stub an earlier save wrote beside Photoshop's objects counts as no object at all, so
        // it is replaced like a missing one (below) and re-saving such a file repairs it, whether
        // or not the layer's text changed.
        let slot = prev_objects.get(at).filter(|o| !is_stub(o) || all_stubs);
        // Kept only while its style runs still say what the layer says (#8415): Photoshop reads
        // the character style from them, so a style-only edit (font, faux bold, size, …) must
        // restyle the object the way an edited one is, or the file's old style shows on reopen.
        let kept = slot.filter(|o| {
            let model = o.get("0");
            model.and_then(|m| m.get("0")).and_then(E::as_str) == Some(text.as_str()) && model.is_some_and(|m| txt2_style_matches(layer, m, &fonts, dpi))
        });
        // An edited object (#6968): the file's object at this slot with its text changed. It keeps
        // the file's structure — paragraph runs, the view naming its frame, the rest — with every
        // run array refitted to the new text. A bare `<< /0 << /0 text /6 runs >> >>` stub in
        // Photoshop's block (no paragraph runs, no view) made Photoshop quit while opening the file.
        let edited = kept.is_none().then(|| slot.and_then(|o| edited_object(o, &text))).flatten();
        // A new object (#7698): a layer the file's block has no object for (a type layer added
        // here) takes the shape of one of the file's own objects, refitted the same way, with its
        // view naming its own frame. A bare stub beside Photoshop's objects, its slot without a
        // frame in `/0 /8 /0`, made Photoshop and Photopea refuse the file. A slot whose object
        // can't be refitted keeps the frame that object was drawn in.
        let fresh = (kept.is_none() && edited.is_none())
            .then(|| {
                // Without a template, only a PhotoCraft-native block gets a fresh object; a file's
                // block keeps the stub it always got.
                if template.is_none() && !native {
                    return None;
                }
                let own = slot.and_then(|o| object_frame(o, true)).filter(|f| frame_count.is_some_and(|n| *f < n));
                let frame = match own {
                    Some(f) => f,
                    None if (frame_count.is_some() || template.is_none()) && taken.contains(&at) => {
                        while taken.contains(&next_free) {
                            next_free += 1;
                        }
                        if next_free > MAX_TEXT_INDEX as usize { at } else { next_free }
                    }
                    None => at,
                };
                // Prefer a file object as template (#7698); otherwise a point-text object with a
                // view naming its own frame so a PhotoCraft-native Txt2 still opens (#10308).
                let object = match template {
                    Some(template) => new_object(template, frame, &text)?,
                    None => fresh_point_text_object(frame, &text),
                };
                taken.insert(frame);
                frames_needed = frames_needed.max(frame.saturating_add(1));
                Some(object)
            })
            .flatten();
        let reuse = kept.is_some() || edited.is_some();
        let unchanged = kept.is_some();
        let candidate = kept.cloned().or(edited).or(fresh);
        // A file's block with no object for this layer and no template to copy keeps the bare
        // stub it always got: no view, no paragraph runs (#7698, #10308).
        let has_object = candidate.is_some();
        let mut object = candidate.unwrap_or_else(E::dict);
        let mut model = object.get("0").cloned().filter(|m| matches!(m, E::Dict(_))).unwrap_or_else(E::dict);
        // A new or edited object takes the layer's own style: Photoshop reads the character style
        // from these runs, so the template's (or the old text's) would show, and come back on
        // reopen. They are built on one of the object's own runs, every key Photoshop wrote kept;
        // without a template run, a minimal base is filled the same way (#10308).
        let styled = (!unchanged)
            .then(|| {
                let file_base = (!is_stub(&object)).then(|| model.path(&["6", "0"]).and_then(E::as_array).and_then(<[E]>::first).cloned()).flatten();
                let base = file_base.or_else(|| native.then(minimal_txt2_style_run))?;
                txt2_style_runs(layer, &base, &mut fonts, dpi, seed_empty)
            })
            .flatten();
        let file_runs = reuse.then(|| model.path(&["6", "0"]).and_then(E::as_array)).flatten().and_then(|runs| with_modes(runs, &modes));
        let runs = styled.or(file_runs).unwrap_or_else(|| {
            modes
                .iter()
                .map(|&(mode, len)| {
                    // `<< /0 << /0 << /0 () /6 << /0 0 /11 mode >> >> >> /1 len >>`.
                    let style = E::Dict(vec![("0".into(), E::Int(0)), ("11".into(), E::Int(mode))]);
                    // `/5 1` marks the wrapper as a character run; without it Photoshop's
                    // composer falls back to the shared default sheet and ignores the style.
                    let data = E::Dict(vec![("0".into(), E::String(String::new())), ("5".into(), E::Int(1)), ("6".into(), style)]);
                    E::Dict(vec![("0".into(), E::Dict(vec![("0".into(), data)])), ("1".into(), E::Int(len as i64))])
                })
                .collect()
        });
        model.set("0", E::String(text));
        match model.get_mut("6") {
            Some(style_run @ E::Dict(_)) => style_run.set("0", E::Array(runs)),
            _ => model.set("6", E::Dict(vec![("0".into(), E::Array(runs))])),
        }
        // The paragraph sheets (`/5`) are what Photoshop's composer reads back: one run per
        // paragraph of the model's text, the file's own sheet kept as the base when there is one.
        // A kept object keeps its sheets as the file has them (byte for byte); a new or edited one
        // gets them from the layer.
        if !unchanged && has_object && model.get("0").is_some() {
            let base = model.path(&["5", "0"]).and_then(E::as_array).map(<[E]>::to_vec);
            model.set("5", E::Dict(vec![("0".into(), E::Array(txt2_paragraph_runs(layer, base.as_deref())))]));
        }
        object.set("0", model);
        if let Some(frame) = object_frame(&object, true) {
            // Vertical text needs Photoshop's `/2 /1 2` frame marker: the `TySh` `Ornt` and the
            // engine data alone leave Photoshop opening the layer horizontal and re-flowing it.
            frame_shapes.push((frame, &layer.shape, layer.orientation == Orientation::Vertical));
        }
        if slots.len() <= at {
            slots.resize(at + 1, E::dict());
        }
        if let Some(slot) = slots.get_mut(at) {
            *slot = object;
        }
    }
    // The outer dictionary keeps whatever the block held (`/98` version, block extras, …), and
    // `/1` whatever it holds beside the text objects. A PhotoCraft-native block gets a frame list
    // (`/0 /8 /0`) so each object's view can name a frame (#10308, #7698); a file's block is never
    // given a `/0` or a frame list it didn't have (#2374).
    let mut outer = prev.clone().unwrap_or_else(native_scaffold);
    // The engine's empty scaffolding carries no frame list; a native save that draws an object
    // needs one (a view naming a frame the list lacks made Photoshop open point text at the
    // origin). A file's block keeps whatever it had (#2374).
    if native {
        let has_doc = outer.get("0").is_some();
        if !has_doc {
            outer.set("0", E::dict());
        }
        if let Some(doc @ E::Dict(_)) = outer.get_mut("0")
            && doc.get("8").is_none()
        {
            doc.set("8", E::Dict(vec![("0".into(), E::Array(vec![]))]));
        }
    }
    pad_frames(&mut outer, slots.len().max(frames_needed));
    // A path layer's frame holds its path; a layer taken off its path loses its path frame. An
    // unchanged path leaves the file's frame as it was, and so does a frame the file already had
    // more than one object drawn in (which layer's shape it should hold is unknown).
    let mut claims: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (at, _, _) in &frame_shapes {
        *claims.entry(*at).or_default() += 1;
    }
    if let Some(E::Array(frames)) = outer.get_mut("0").and_then(|v| v.get_mut("8")).and_then(|v| v.get_mut("0")) {
        for (at, shape, vertical) in frame_shapes {
            if claims.get(&at) != Some(&1) {
                continue;
            }
            if let Some(entry) = frames.get_mut(at) {
                crate::psd_path::update_frame_oriented(entry, shape, vertical);
            }
        }
    }
    // Write the font list whenever it grew. Create `/0/1` only in a PhotoCraft-native block;
    // otherwise leave the file's `/0` alone (#2374).
    if fonts.len() != fonts_before
        && let Some(doc @ E::Dict(_)) = outer.get_mut("0")
    {
        match doc.get_mut("1") {
            Some(list @ E::Dict(_)) => list.set("0", E::Array(fonts)),
            _ if seed_empty => {
                doc.set("1", E::Dict(vec![("0".into(), E::Array(fonts))]));
            }
            _ => {}
        }
    }
    // A kept object's frame is the file's own, so its text-flow marker can disagree with the
    // layer: the file may predate the vertical frame marker (`/2 /1 2`), and Photoshop then
    // opens a vertical layer horizontal however its `TySh` and engine data read. Set the marker
    // on every frame a written object names, before the objects move into `outer`.
    let frame_flow: Vec<(usize, bool)> = objects
        .iter()
        .filter(|(_, layer)| !matches!(layer.shape, TextShape::Path { .. }))
        .filter_map(|(index, layer)| {
            let at = usize::try_from(*index).ok()?;
            let frame = object_frame(slots.get(at)?, true)?;
            Some((frame, layer.orientation == Orientation::Vertical))
        })
        .collect();
    match outer.get_mut("1") {
        Some(objs @ E::Dict(_)) => objs.set("1", E::Array(slots)),
        _ => outer.set("1", E::Dict(vec![("1".into(), E::Array(slots))])),
    }
    if let Some(E::Array(frames)) = outer.get_mut("0").and_then(|v| v.get_mut("8")).and_then(|v| v.get_mut("0")) {
        for (frame, vertical) in frame_flow {
            if let Some(entry) = frames.get_mut(frame) {
                crate::psd_path::set_frame_orientation(entry, vertical);
            }
        }
    }
    // The older Photoshop's objects, whatever the loop fixed in them (an untouched list is left
    // exactly as the file has it, so the byte-for-byte return below still holds).
    if td_changed
        && let Some(entries) = td
        && let Some(objects @ E::Dict(_)) = outer.get_mut("DocumentObjects")
    {
        objects.set("TextObjects", E::Array(entries));
        if td_fonts.len() != td_fonts_before
            && let Some(set @ E::Dict(_)) = outer.get_mut("DocumentResources").and_then(|r| r.get_mut("FontSet"))
        {
            set.set("Resources", E::Array(td_fonts));
        }
    }
    // Nothing changed: the file's own bytes (writing them back would re-encode strings and numbers).
    if let (Some(bytes), Some(p)) = (previous, &prev)
        && *p == outer
    {
        return bytes.to_vec();
    }
    match outer {
        E::Dict(items) => ed::write_bare(&items),
        // Unreachable: `parse_txt2` and the fallback above both yield a dictionary.
        _ => ed::write_bare(&[]),
    }
}
/// The `EngineData` tree a layer's `TySh` carries, through the engine's writer.
///
/// `template` is the layer's current tree: the engine edits it in place so
/// Photoshop's unmodelled keys survive.
pub fn build_engine_data(layer: &TextLayer, template: Option<E>, dpi: f32) -> E {
    let spec = spec_of_layer(layer, dpi);
    let mut out = match template {
        None => from_engine(&photocraft_text_re::psd::build_engine_data(&spec)),
        Some(template) => {
            let mut root = to_engine(&template);
            if photocraft_text_re::psd::apply_engine_data(&mut root, &spec).is_err() {
                from_engine(&photocraft_text_re::psd::build_engine_data(&spec))
            } else {
                from_engine(&root)
            }
        }
    };
    rebuild_font_sets(&mut out, &spec);
    out
}

/// Rebuilds both `FontSet`s from the template's own records, keeping Photoshop's
/// duplicate/dictionary metadata (#1348): `ResourceDict` is the template's record
/// names plus the spec's fonts, one record per name (`font_set` pulls each
/// original record once); `DocumentResources` mirrors that name order, taking its
/// records from its own set first and the `ResourceDict` ones after, so a
/// nameless or duplicate record is re-emitted with its dictionary (never
/// synthesised), and a reordered copy is re-sorted to the resource order.
fn rebuild_font_sets(root: &mut E, spec: &photocraft_text_re::psd::LayerSpec) {
    let name_of = |f: &E| f.get("Name").and_then(E::as_str).unwrap_or("").to_string();
    let original = root.get("ResourceDict").and_then(|d| d.get("FontSet")).cloned();
    let mut names: Vec<String> = original.as_ref().and_then(E::as_array).map(|a| a.iter().map(name_of).collect()).unwrap_or_default();
    for run in &spec.runs {
        if let Some(font) = &run.font
            && !names.contains(font)
        {
            names.push(font.clone());
        }
    }
    let resource = font_set(&names, original.as_ref());
    if let Some(rd) = root.get_mut("ResourceDict") {
        rd.set("FontSet", resource);
    }
    let document = root.get("DocumentResources").and_then(|d| d.get("FontSet")).cloned();
    let mut candidates: Vec<E> = document.as_ref().and_then(E::as_array).map(<[_]>::to_vec).unwrap_or_default();
    if let Some(records) = original.as_ref().and_then(E::as_array) {
        candidates.extend(records.iter().cloned());
    }
    let mirrored = font_set(&names, Some(&E::Array(candidates)));
    if let Some(doc) = root.get_mut("DocumentResources") {
        doc.set("FontSet", mirrored);
    }
}

/// The `TySh` block for `layer`, patching its current `psd_raw` when present.
/// `ink` is the layer's rendered bounds in text space (`bounds`/`boundingBox`).
pub fn build_tysh(layer: &TextLayer, dpi: f32, ink: Option<[f32; 4]>) -> Vec<u8> {
    let spec = spec_of_layer(layer, dpi);
    let template = layer.psd_raw.as_deref().map(|raw| raw.as_slice());
    let Ok(bytes) = photocraft_text_re::psd::build_tysh_from(template, &spec, ink) else {
        return layer.psd_raw.as_deref().cloned().unwrap_or_default();
    };
    // The descriptor fields the engine model does not carry (`AntA`'s platform
    // modes, warp, the warped trailing bounds) are still the fork's.
    let Ok(mut tysh) = photocraft_psd::TySh::from_bytes(&bytes) else { return bytes };
    if layer.psd_raw.is_none() {
        // A layer the Type tool made has no `Txt2` object of its own yet; leaving
        // the placeholder `TextIndex 0` would let it claim the file's object 0.
        // The export assigns a free number and writes it before saving.
        tysh.text_descriptor_mut().items.retain(|(key, _)| !key.is("TextIndex"));
    }
    // A cache-only refresh must reproduce the stored `TySh` byte-for-byte: these
    // descriptor fields keep the file's own values (the platform `AntA` modes,
    // warp keys this model omits, the warped trailing bounds), so only a real
    // change is written.
    if tysh.antialias().as_deref() != Some(antialias_name(layer.antialias)) {
        tysh.set_antialias(antialias_name(layer.antialias));
    }
    let warped = match (&layer.warp, enum_value(&tysh.warp.descriptor, "warpStyle")) {
        (Some(warp), _) => {
            let want = photocraft_psd::text_layer::Warp {
                style: warp.style.clone(),
                value: f64::from(warp.value.clamp(-100.0, 100.0)),
                perspective: f64::from(warp.horizontal_distortion.clamp(-100.0, 100.0)),
                perspective_other: f64::from(warp.vertical_distortion.clamp(-100.0, 100.0)),
                rotate: if warp.horizontal { photocraft_psd::text_layer::Orientation::Horizontal } else { photocraft_psd::text_layer::Orientation::Vertical },
            };
            if tysh.warp_settings() != want {
                tysh.warp.descriptor = Descriptor::new("warp")
                    .with("warpStyle", enumv("warpStyle", &warp.style))
                    .with("warpValue", D::Double(want.value))
                    .with("warpPerspective", D::Double(want.perspective))
                    .with("warpPerspectiveOther", D::Double(want.perspective_other))
                    .with("warpRotate", enumv("Ornt", if warp.horizontal { "Hrzn" } else { "Vrtc" }));
            }
            warp.style != "warpNone"
        }
        (None, Some(style)) => style != "warpNone",
        (None, None) => false,
    };
    // Photoshop writes the 16 trailing bytes as zeros for unwarped type and the
    // text-space bounds for warped type.
    let trailing = if warped { ink.unwrap_or([0.0; 4]) } else { [0.0; 4] };
    if tysh.bounds_points() != trailing {
        tysh.set_bounds_points(trailing);
    }
    // Keep the model's paragraph split. The engine writes one paragraph run per `\r`-terminated
    // paragraph, each owning its break; a model can instead put the break at the start of the
    // following run (a layer created by an older editor, or a hand-authored model). Writing the
    // model's own lengths keeps the round trip exact, and it is layout-neutral: the engine splits
    // the text at its breaks, not at these run ends.
    if let Some(want) = model_paragraph_lens(layer)
        && let Some(raw) = tysh.engine_data().map(<[u8]>::to_vec)
        && let Ok(mut tree) = ed::parse(&raw)
    {
        let current: Vec<i64> = tree
            .path(&["EngineDict", "ParagraphRun", "RunLengthArray"])
            .or_else(|| tree.path(&["ParagraphRun", "RunLengthArray"]))
            .and_then(E::as_array)
            .map(|a| a.iter().filter_map(E::as_i64).collect())
            .unwrap_or_default();
        // Only when the two splits have the same number of runs: a model run may span several
        // engine paragraphs (a style-only split inside a paragraph, or the synthetic empty final
        // paragraph), and replacing those lengths would merge the paragraph sheets.
        if !current.is_empty() && current.len() == want.len() && current != want {
            let value = E::Array(want.iter().map(|n| E::Int(*n)).collect());
            let mut patched = false;
            if let Some(engine) = tree.get_mut("EngineDict")
                && let Some(pr) = engine.get_mut("ParagraphRun")
            {
                pr.set("RunLengthArray", value.clone());
                patched = true;
            }
            if !patched && let Some(pr) = tree.get_mut("ParagraphRun") {
                pr.set("RunLengthArray", value);
            }
            tysh.set_engine_data(&ed::write(&tree));
        }
    }
    tysh.to_bytes()
}

/// The `EngineData` paragraph run lengths (UTF-16 units) the model's paragraph split implies.
///
/// `EngineData` stores one run per paragraph and a run owns its trailing break, so a model whose
/// runs put the break at the start of the next one is written with those exact lengths; deriving
/// them from the text instead would re-split the runs on the way back. `None` when the model has
/// no paragraph runs.
fn model_paragraph_lens(layer: &TextLayer) -> Option<Vec<i64>> {
    let runs = layer.paragraph_runs();
    if runs.is_empty() {
        return None;
    }
    let text = layer.text.replace('\n', "\r");
    let mut at = 0usize;
    let mut out = Vec::with_capacity(runs.len());
    for (index, run) in runs.iter().enumerate() {
        let mut end = (at + run.len).min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end += 1;
        }
        let piece = text.get(at..end)?;
        let mut units = i64::try_from(piece.encode_utf16().count()).ok()?;
        if index + 1 == runs.len() {
            units += 1;
        }
        out.push(units);
        at = end;
    }
    Some(out)
}

/// Converts the fork's `EngineData` tree to the engine's value type.
fn to_engine(value: &E) -> photocraft_text_re::engine_data::Value {
    use photocraft_text_re::engine_data::Value as Engine;
    match value {
        E::Dict(items) => Engine::Dict(items.iter().map(|(key, value)| (key.clone(), to_engine(value))).collect()),
        E::Array(items) => Engine::Array(items.iter().map(to_engine).collect()),
        E::String(text) | E::RawString(text) => Engine::Str(text.clone()),
        E::Int(v) => Engine::Int(*v),
        E::Real(v) => Engine::Float(*v),
        E::Bool(v) => Engine::Bool(*v),
        E::Name(name) => Engine::Name(name.clone()),
    }
}

/// Converts the engine's `EngineData` tree to the fork's value type.
fn from_engine(value: &photocraft_text_re::engine_data::Value) -> E {
    use photocraft_text_re::engine_data::Value as Engine;
    match value {
        Engine::Dict(items) => E::Dict(items.iter().map(|(key, value)| (key.clone(), from_engine(value))).collect()),
        Engine::Array(items) => E::Array(items.iter().map(from_engine).collect()),
        Engine::Str(text) => E::String(text.clone()),
        Engine::Int(v) => E::Int(*v),
        Engine::Float(v) => E::Real(*v),
        Engine::Bool(v) => E::Bool(*v),
        Engine::Name(name) => E::Name(name.clone()),
    }
}

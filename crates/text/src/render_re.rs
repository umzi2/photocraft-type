//! Renders a text layer's pixels with the reimplemented Photoshop type engine.
//!
//! The crate's own renderer ([`render`]) lays out with parley and rasterises with our
//! coverage rasteriser: the line starts and ends match Photoshop's, but the glyph shapes,
//! hinting and sub-pixel coverage do not, so a canvas view differs from the PSD's stored
//! pixels (IoU ~0.2 on antialiased text). The reimplementation in `photocraft-text-re`
//! reproduces Photoshop's own rasteriser (IoU 0.98–0.998 against Photoshop's stored pixels),
//! so the app renders type through it.
//!
//! The two models are bridged through `EngineData`, which both sides already speak: the
//! exporter's writer ([`crate::psd::build_engine_data`]) turns the layer's runs, paragraph
//! sheets and anti-aliasing setting into Photoshop's own document, and the reimplementation
//! lays that document out and rasters it. Everything the editor edits therefore reaches the
//! pixels exactly as it reaches a saved PSD.
//!
//! [`render_layer`] mirrors [`crate::TextEngine::render_layer`]'s contract: the layer's
//! document-space rectangle and a cache surface covering it. It covers every text layer the
//! document model can hold:
//!
//! * point, box and area type, horizontal and vertical (`render_vertical_from_engine_data*`);
//! * warped type: horizontal text bends analytically through the fitted 4x4 patch
//!   (`render_from_engine_data_warped`); vertical warped text remaps its finished raster through
//!   the same patch (`warp::apply`). A layer with no stored rectangle yet gets one derived from
//!   the flat ink and is cropped back to the bent ink;
//! * a scaled or rotated layer transform: a *uniform* scale is folded into the render
//!   resolution, so the engine rasterises the glyphs at the transformed size just like
//!   Photoshop's transformed type; rotation, shear and non-uniform scaling cannot be expressed
//!   that way and are applied to the finished document-space raster (bilinear, about the text
//!   origin) in [`transform_raster`];
//! * every `EngineData` feature the editor edits (runs, paragraph sheets, the `Txt2` character
//!   features, optical kerning, the atlas).
//!
//! The only refusals left are genuinely outside the reimplementation: type on a path (the
//! engine has no on-path layout) and documents that are not RGB (the engine writes straight
//! RGBA and the cache is in the document's pixel format).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use photocraft_color::{ColorMode, PixelFormat};
use photocraft_doc::text::{Kerning, Orientation, TextShape};
use photocraft_doc::TextLayer;
use photocraft_geom::Rect;
use photocraft_raster::Surface;
use photocraft_text_re::area::Justification;
use photocraft_text_re::engine_data;
use photocraft_text_re::layer_render::{self, FaceStore, RectTarget, RunFeatures};
use photocraft_text_re::warp::FlatSurface;
use photocraft_text_re::runs::ParagraphRun;

/// Why [`render_layer`] left a layer to the built-in renderer.
///
/// Carries a human-readable reason for the fallback log; callers treat every value the same.
#[derive(Clone, Debug)]
pub struct Unsupported(pub String);

/// `((width, height), (left, top))` of a layer's text box in points.
pub(crate) type TextGeometry = ((f32, f32), (f32, f32));

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unsupported {}

/// The reimplementation's font store, scanned once.
///
/// Its layers name fonts by PostScript name (`CCMeanwhile-Regular`), so the store is filled
/// from the same folders the canvas engine scans — [`crate::fonts::system_font_dirs`] adds the
/// Wine prefix's two Photoshop font folders on Linux, where the PSDs' Adobe fonts live.
/// `PHOTOCRAFT_ATLAS` optionally attaches a captured glyph atlas (byte-identical glyphs for
/// the faces it covers), as the PSD-rewriting examples do.
pub(crate) fn store() -> &'static Mutex<FaceStore> {
    static STORE: OnceLock<Mutex<FaceStore>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(load_faces()))
}

fn load_faces() -> FaceStore {
    let mut store = FaceStore::new();
    // Photoshop's own files, in Photoshop's order (the Wine prefix's `Fonts`, the Adobe
    // application's PDFL faces, shared Adobe folders, distribution copies): the same list the
    // name lookup in [`crate::fonts`] uses, so a PSD's `TimesNewRomanPSMT` is drawn from
    // `times.ttf` and not from a fontconfig substitute. `load_dir` only reads a folder's top
    // level, so each indexed file is loaded by its stem (`load` = `load_file`).
    let photoshop = crate::fonts::PsFontIndex::scan();
    let mut loaded = 0usize;
    for entry in photoshop.entries() {
        // The store keys faces by the file's own names; a collection's other faces would share
        // the file, and the store's parser reads the first face only.
        if entry.face_index != 0 {
            continue;
        }
        let (Some(dir), Some(stem)) = (entry.path.parent(), entry.path.file_stem().and_then(|s| s.to_str())) else { continue };
        if store.load(dir, stem).is_ok() {
            loaded += 1;
        }
    }
    if std::env::var_os("PHOTOCRAFT_FONT_TRACE").is_some_and(|v| !v.is_empty() && v != "0") {
        eprintln!(
            "render_re: loaded {loaded} Photoshop faces from {} sources ({} indexed)",
            photoshop.sources().len(),
            photoshop.len()
        );
    }
    for dir in crate::fonts::system_font_dirs() {
        store.load_dir(&dir);
    }
    // The application's own faces (Inter, JetBrains Mono) are compiled into the binary rather
    // than installed, and new text layers name them (`Inter-Regular`). The workspace keeps the
    // same files under `assets/fonts`; loading them keeps new text on the reimplementation.
    store.load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts"));
    // A capture of Photoshop's own glyph bitmaps (per font, size, antialias and quarter-pixel
    // pen phase) makes the rasteriser draw Photoshop's pixels instead of analytic coverage.
    // `PHOTOCRAFT_ATLAS` is the single phase-less capture older tools pass; `PHOTOCRAFT_ATLAS_DIR`
    // (default: the workspace's `assets/atlas`) holds one directory per capture.
    if let Some(path) = std::env::var_os("PHOTOCRAFT_ATLAS").map(PathBuf::from) {
        if let Ok(atlas) = photocraft_text_re::atlas::Atlas::load(&path) {
            store.set_atlas(atlas);
        }
    } else if let Some(dir) = atlas_dir() {
        let mut captures = Vec::new();
        let mut dirs = vec![dir.clone()];
        if let Ok(read) = std::fs::read_dir(&dir) {
            for entry in read.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                }
            }
        }
        for dir in dirs {
            if let Ok(atlas) = photocraft_text_re::atlas::Atlas::load(&dir) {
                captures.push(atlas);
            }
        }
        if !captures.is_empty() {
            if debug_enabled() {
                let mut faces: Vec<String> = captures.iter().map(|a| format!("{}@{}/{}", a.font, a.size, a.phase)).collect();
                faces.sort();
                eprintln!("render_re: atlases={} ({} …{})", captures.len(), faces.first().map_or("", String::as_str), faces.last().map_or("", String::as_str));
            }
            store.set_atlases(captures);
        }
    }
    store
}

/// Logs the layer's requested PostScript names with the Photoshop file each resolves to
/// (`PHOTOCRAFT_FONT_TRACE=1`): the per-layer font diagnostic of the parity work.
fn trace_layer_fonts(layer: &TextLayer) {
    if !crate::fonts::trace_enabled() {
        return;
    }
    static INDEX: OnceLock<crate::fonts::PsFontIndex> = OnceLock::new();
    let index = INDEX.get_or_init(crate::fonts::PsFontIndex::scan_all);
    let excerpt: String = layer.text.chars().take(24).collect();
    let mut seen = std::collections::HashSet::new();
    for run in &layer.runs {
        let Some(ps) = run.style.postscript_name.as_deref() else { continue };
        if !seen.insert(ps.to_string()) {
            continue;
        }
        match index.resolve(ps) {
            Some(file) => eprintln!(
                "photocraft-text: layer {excerpt:?} font {ps:?} -> {} face={} sha256={}",
                file.path.display(),
                file.face_index,
                file.sha256
            ),
            None => eprintln!("photocraft-text: layer {excerpt:?} font {ps:?} -> no Photoshop file (substitute drawn)"),
        }
    }
}

/// Where the captured atlases live: `PHOTOCRAFT_ATLAS_DIR`, else `assets/atlas` beside the
/// workspace or the running executable (a packaged build ships it next to the binary).
fn atlas_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("PHOTOCRAFT_ATLAS_DIR") {
        let dir = PathBuf::from(dir);
        return dir.is_dir().then_some(dir);
    }
    let mut candidates = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/atlas")];
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("assets/atlas"));
        candidates.push(dir.join("atlas"));
    }
    candidates.into_iter().find(|dir| dir.is_dir())
}

/// The layer's text box in text space (pixels, before its transform): the box's size and
/// top-left corner. Point text has no box.
pub(crate) fn text_box(layer: &TextLayer) -> Result<TextGeometry, Unsupported> {
    match &layer.shape {
        TextShape::Point => Ok(((0.0, 0.0), (0.0, 0.0))),
        TextShape::Box { x, y, width, height } if width.is_finite() && height.is_finite() && *width > 0.0 && *height > 0.0 => {
            Ok(((*width, *height), (*x, *y)))
        }
        // Area type wraps inside its path's bounds; for the rectangular frames area type is
        // usually made in, that is exactly the text box the reimplementation lays out.
        TextShape::Area { path } => {
            let Some((x0, y0, x1, y1)) = crate::area::path_bounds(path) else {
                return Err(Unsupported("its area path has no bounds".into()));
            };
            let (width, height) = ((x1 - x0) as f32, (y1 - y0) as f32);
            if !(width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0) {
                return Err(Unsupported("its area path encloses no area".into()));
            }
            Ok(((width, height), (x0 as f32, y0 as f32)))
        }
        TextShape::Path { .. } => Err(Unsupported("type on a path is not rendered by the reimplementation yet".into())),
        _ => Err(Unsupported("its text box has no size".into())),
    }
}

/// The layer's paragraph sheets as the reimplementation takes them, converted to its units.
///
/// `EngineData` stores paragraph geometry in document pixels (`pt * dpi / 72`) and justifies
/// with Photoshop's own codes; the model keeps points. Passing these explicitly keeps the
/// model's paragraph sheets authoritative (a `Txt2` sheet can differ from the `EngineData`
/// one). One entry is produced per **paragraph**, not per model paragraph run: a model run can
/// span several paragraphs (its style then applies to each of them), and the reimplementation
/// indexes its paragraph sheets by paragraph. Passing one entry per run shifted every sheet after
/// the first multi-paragraph run — a trailing paragraph's alignment leaked into the paragraph
/// before it (`type.info` moved a left-aligned `b` line right).
pub(crate) fn paragraph_runs(layer: &TextLayer, dpi: f32, text: &str) -> Vec<ParagraphRun> {
    let k = if dpi.is_finite() && dpi > 0.0 { dpi / 72.0 } else { 1.0 };
    let runs = layer.paragraph_runs();
    if runs.is_empty() {
        return Vec::new();
    }
    // Byte offset of each model paragraph run: the run whose range starts at or before a
    // paragraph's first byte supplies its sheet.
    let mut starts = Vec::with_capacity(runs.len());
    let mut at = 0usize;
    for run in &runs {
        starts.push(at);
        at += run.len;
    }
    let mut out = Vec::with_capacity(runs.len().max(1));
    let mut para_start = 0usize;
    loop {
        let mut end = para_start;
        let mut break_len = 0usize;
        while let Some(ch) = text[end..].chars().next() {
            if matches!(ch, '\r' | '\n' | '\u{2029}') {
                // A CR LF pair is one break, as `paragraph_spans` in the engine treats it.
                break_len = if ch == '\r' && text[end + 1..].starts_with('\n') { 2 } else { ch.len_utf8() };
                break;
            }
            end += ch.len_utf8();
        }
        let para_end = (end + break_len).min(text.len());
        let index = starts.partition_point(|&start| start <= para_start).saturating_sub(1);
        let s = &runs.get(index).unwrap_or(&runs[runs.len() - 1]).style;
        let mut len = text.get(para_start..para_end).map_or(0, |piece| piece.encode_utf16().count());
        if para_end >= text.len() {
            // The synthetic paragraph end of PSD text, as `build_engine_data` writes it.
            len += 1;
        }
        out.push(ParagraphRun {
            len,
            word_spacing: s.justification_word,
            letter_spacing: s.justification_letter,
            glyph_scaling: s.justification_glyph,
            justification: Justification::from_psd(align_code(s.align)),
            first_line_indent: s.first_line_indent_pt * k,
            start_indent: s.start_indent_pt * k,
            end_indent: s.end_indent_pt * k,
            space_before: s.space_before_pt * k,
            space_after: s.space_after_pt * k,
            auto_leading: s.auto_leading,
            every_line_composer: s.every_line_composer,
            hanging: s.hanging_punctuation,
            hyphenate: s.hyphenate,
            words_longer_than: i64::from(s.hyphenate_word_size),
            pre_hyphen: i64::from(s.hyphenate_min_before),
            post_hyphen: i64::from(s.hyphenate_min_after),
            consecutive_hyphens: i64::from(s.hyphen_limit),
            hyphen_zone: s.hyphenation_zone_pt * k,
        });
        if para_end >= text.len() {
            break;
        }
        para_start = para_end;
    }
    out
}

/// Photoshop's `Justification` code for a model alignment (`Txt2` `/5/0`, `EngineData`).
pub(crate) fn align_code(align: photocraft_doc::text::TextAlign) -> i32 {
    use photocraft_doc::text::TextAlign;
    match align {
        TextAlign::Left => 0,
        TextAlign::Right => 1,
        TextAlign::Center => 2,
        TextAlign::JustifyLeft => 3,
        TextAlign::JustifyRight => 4,
        TextAlign::JustifyCenter => 5,
        TextAlign::JustifyAll => 6,
    }
}

/// The `Txt2`-only character features of the layer's style runs, indexed like `EngineData`'s.
///
/// Photoshop keeps a few character properties only in the document's `Txt2` block — notably
/// old-style figures (`/6/30`, "on" is 2), which `EngineData` cannot express. The layer's
/// `TySh` names its object in `TextIndex`; the object's run sheets carry the values. Missing or
/// unreadable `Txt2` data means no feature is on, so the layer still renders.
pub(crate) fn run_features(layer: &TextLayer, runs: usize, txt2: Option<&[u8]>) -> Vec<RunFeatures> {
    let mut out = vec![RunFeatures::default(); runs];
    let Some(txt2) = txt2 else { return out };
    let Some(index) = layer.psd_raw.as_deref().and_then(|tysh| crate::psd::text_index(tysh)) else {
        return out;
    };
    let Some(parsed) = crate::psd::parse_txt2(txt2) else { return out };
    let Some(objects) = parsed.path(&["1", "1"]).and_then(|v| v.as_array()) else { return out };
    let Some(model) = usize::try_from(index).ok().and_then(|i| objects.get(i)).and_then(|object| object.get("0")) else {
        return out;
    };
    let Some(sheets) = model.path(&["6", "0"]).and_then(|v| v.as_array()) else { return out };
    for (feature, sheet) in out.iter_mut().zip(sheets) {
        // Run sheet `<< /0 << /0 <char sheet> >> /1 <utf16 length> >>`, char sheet `/6/30`.
        let old_style = sheet.path(&["0", "0", "6", "30"]).and_then(|v| v.as_i64());
        feature.old_style = old_style == Some(2);
    }
    out
}

/// The every-line composer's uniform per-glyph advance delta, in points, from the layer's
/// composed `Txt2` cache.
///
/// Photoshop records the composer's result in the object's cache (`/1`): each composed run that
/// carries a spacing adjustment has a real `3` (points) in its sheet — e.g. -0.0468 for layer
/// "05 area indents spacing". A layer whose runs disagree (optical kerning's per-glyph
/// positions) is left to the engine's own handling for now. The vector is one entry per shaped
/// glyph; a uniform value is valid for any length the layout may need.
/// Whether the layer's composed `Txt2` cache was written by Photoshop's optical kerning.
///
/// The PSD does not record the kerning mode: `EngineData` carries the same `AutoKerning: True,
/// Kerning: 0` for a metrics layer and for one authored with optical kerning. The cache's `/3`
/// corrections do reveal it — optical corrections are pair-specific and an order of magnitude
/// larger (layer "02 optical kerning", 26 pt: `T->y` -1.42 pt, `T->space` -2.10 pt) than the
/// composer's nudges and justification stretches (< 0.2 pt in this document), which are uniform
/// per run or tiny. Same rule as the engine's own cache fit: a large correction that varies.
/// Whether the layer model itself asks for optical kerning.
///
/// New and edited layers have no composed `Txt2` cache yet, and `EngineData` cannot tell optical
/// from metrics (`AutoKerning` is true for both), so the model's [`Kerning`] runs are the source
/// for the editor's character panel setting. Imported layers get the same value from `Txt2`, so
/// this is consistent with the cache rule; when both are present the cache corrections stay the
/// better source for Photoshop's exact positions (see [`cache_is_optical`]).
fn model_is_optical(layer: &TextLayer) -> bool {
    layer.char_runs().iter().any(|run| run.style.kerning == Kerning::Optical)
}

fn cache_is_optical(layer: &TextLayer, txt2: Option<&[u8]>) -> bool {
    let Some(txt2) = txt2 else { return false };
    let Some(index) = layer.psd_raw.as_deref().and_then(|tysh| crate::psd::text_index(tysh)) else {
        return false;
    };
    let Some(parsed) = crate::psd::parse_txt2(txt2) else { return false };
    let Some(objects) = parsed.path(&["1", "1"]).and_then(|v| v.as_array()) else { return false };
    let Some(cache) = usize::try_from(index).ok().and_then(|i| objects.get(i)).and_then(|o| o.get("1")) else {
        return false;
    };
    let mut spacing = Vec::new();
    collect_cache_spacing(cache, &mut spacing);
    let largest = spacing.iter().fold(0.0f32, |m, d| m.max(d.abs()));
    let first = spacing.iter().find(|d| **d != 0.0).copied();
    let varied = first.is_some_and(|first| spacing.iter().any(|d| *d != 0.0 && (*d - first).abs() > 1.0e-4));
    largest > 0.5 && varied
}

pub(crate) fn advance_deltas(layer: &TextLayer, glyphs: usize, txt2: Option<&[u8]>) -> Vec<f32> {
    let Some(txt2) = txt2 else { return Vec::new() };
    let Some(index) = layer.psd_raw.as_deref().and_then(|tysh| crate::psd::text_index(tysh)) else {
        return Vec::new();
    };
    let Some(parsed) = crate::psd::parse_txt2(txt2) else { return Vec::new() };
    let Some(objects) = parsed.path(&["1", "1"]).and_then(|v| v.as_array()) else { return Vec::new() };
    let Some(cache) = usize::try_from(index).ok().and_then(|i| objects.get(i)).and_then(|o| o.get("1")) else {
        return Vec::new();
    };
    // The composed cache holds one spacing value per cached glyph: `/3` is what CoolType added
    // on top of the metrics advance (tracking, optical kerning and composer nudges all land
    // there), so the raw values are what the engine wants. Two cached glyphs have no shaped
    // counterpart — the space standing for each paragraph mark, and ligature glyphs covering two
    // or three characters. The cache stream is aligned against the model text; when the walk
    // cannot line the two up exactly (ligatures, a cache from other text) nothing is applied.
    let mut cached = Vec::new();
    if collect_cache_spacing(cache, &mut cached) {
        let plain = align_cache_to_text(layer, &cached);
        let aligned = plain.or_else(|| {
            // A ligature glyph covers two or three characters; when the cache stream is shorter
            // than the text it is worth marking the ligature sequences the text spells.
            let newlines = layer.text.chars().filter(|c| matches!(c, '\n' | '\r')).count();
            let chars = layer.text.chars().count();
            let extra = (chars + newlines + 1).saturating_sub(cached.len());
            (extra > 0 && extra < 16).then(|| align_with_ligatures(layer, &cached, extra)).flatten()
        });
        if let Some(mut aligned) = aligned {
            aligned.truncate(glyphs.max(1));
            if aligned.iter().any(|d| d.abs() > 1e-6) {
                return aligned;
            }
            return Vec::new();
        }
    }
    let mut found = Vec::new();
    collect_composer_deltas(cache, &mut found, 0);
    let Some(&first) = found.first() else { return Vec::new() };
    if found.iter().any(|d| (d - first).abs() > 1e-4) || first.abs() < 1e-6 {
        return Vec::new();
    }
    // Photoshop's every-line composer advances every glyph by `level * 0.0026 em`, so a real
    // composer delta is an integer number of those steps at the layer's size. Tracking lands in
    // the same cache field and is already applied by the layout; the quantum test rejects it.
    let size = layer.size_pt;
    if !(size.is_finite() && size > 0.0) {
        return Vec::new();
    }
    let level = first / (0.0026 * size);
    if (level - level.round()).abs() > 0.02 || level.abs() > 16.0 {
        return Vec::new();
    }
    // The composer's nudge widens the gaps between letters, not the spaces: the cache records a
    // zero for every space run. The aligned walk above is exact where it can run (layer 14); when
    // the cache has ligature glyphs it cannot be aligned here, and the same spacing is rebuilt
    // from the model text, where a space stays at its metrics advance.
    let mut per_char = Vec::with_capacity(glyphs);
    for ch in layer.text.chars() {
        if matches!(ch, '\n' | '\r') {
            continue;
        }
        per_char.push(if ch.is_whitespace() { 0.0 } else { first });
    }
    if per_char.is_empty() {
        return Vec::new();
    }
    per_char.truncate(glyphs);
    per_char
}

/// Reads the composed cache's per-glyph spacing values in composed order.
///
/// ```text
/// /1/1[i]/1/2[pc] /6[0] F /6[0] R /6[0] /6[line] L /6[0] S /6[0] G
/// G/5 = glyph ids, G/6 = {0: [dicts], 1: [run lengths]}, dicts[i]/3 = points
/// ```
///
/// Every line is visited (one paragraph container can hold several) and lines without a spacing
/// record contribute zeros. `false` means the cache did not have the expected shape.
fn collect_cache_spacing(cache: &crate::engine_data::Value, out: &mut Vec<f32>) -> bool {
    use crate::engine_data::Value;
    let Some(pcs) = cache.get("2").and_then(|v| v.as_array()) else { return false };
    for pc in pcs {
        let Some(lines) = pc
            .get("6")
            .and_then(|v| v.as_array().and_then(|a| a.first()))
            .and_then(|f| f.get("6"))
            .and_then(|v| v.as_array().and_then(|a| a.first()))
            .and_then(|r| r.get("6"))
            .and_then(|v| v.as_array().and_then(|a| a.first()))
            .and_then(|r2| r2.get("6"))
            .and_then(|v| v.as_array())
        else {
            return false;
        };
        for line in lines {
            let Some(g) = line
                .get("6")
                .and_then(|v| v.as_array().and_then(|a| a.first()))
                .and_then(|s| s.get("6"))
                .and_then(|v| v.as_array().and_then(|a| a.first()))
            else {
                return false;
            };
            let Some(gids) = g.get("5").and_then(|v| v.as_array()) else { return false };
            let mut values = vec![0.0f32; gids.len()];
            if let Some(spacing) = g.get("6") {
                let sheets_lengths = match spacing {
                    Value::Dict(_) => Some((spacing.get("0"), spacing.get("1"))),
                    Value::Array(a) if a.len() == 2 => Some((a.first(), a.get(1))),
                    _ => None,
                };
                let sheets_lengths = sheets_lengths.and_then(|(sheets, lengths)| Some((sheets?.as_array()?, lengths?.as_array()?)));
                if let Some((sheets, lengths)) = sheets_lengths {
                    let mut pos = 0usize;
                    for (sheet, len) in sheets.iter().zip(lengths) {
                        let Some(len) = len.as_f64().filter(|l| *l >= 0.0 && *l < 10000.0) else { break };
                        let value = sheet.get("3").and_then(|d| d.as_f64()).unwrap_or(0.0) as f32;
                        for slot in values.iter_mut().skip(pos).take(len as usize) {
                            *slot = value;
                        }
                        pos += len as usize;
                    }
                }
            }
            out.extend(values);
        }
    }
    !out.is_empty()
}

/// Aligns the composed cache stream with the shaped text, character by character: each
/// paragraph mark consumes the cached space that stands for it, every other character consumes
/// one cached spacing value. The cache also carries a trailing mark for the text's final
/// paragraph end, so a couple of cached glyphs may remain unconsumed; ligature text consumes
/// fewer cached values than characters and is rejected rather than misaligned.
fn align_cache_to_text(layer: &TextLayer, cached: &[f32]) -> Option<Vec<f32>> {
    let mut next = 0usize;
    let mut out = Vec::new();
    for ch in layer.text.chars() {
        if matches!(ch, '\n' | '\r') {
            next += 1;
            continue;
        }
        let &value = cached.get(next)?;
        out.push(value);
        next += 1;
    }
    if cached.len() > next + 2 {
        return None;
    }
    Some(out)
}

/// The ligature sequences a text shaper may replace with one glyph, longest first.
const LIGATURES: [(&str, usize); 7] = [("ffi", 3), ("ffl", 3), ("ff", 2), ("fi", 2), ("fl", 2), ("ft", 2), ("st", 2)];

/// Marks the first `extra` ligature sequences of `chars` with the number of characters each
/// glyph covers (0 for an ordinary character).
fn ligature_spans(chars: &[char], extra: usize) -> Vec<usize> {
    let mut spans = vec![0usize; chars.len()];
    let (mut marked, mut i) = (0usize, 0usize);
    while i < chars.len() && marked < extra {
        let mut advance = 1usize;
        for (pattern, len) in LIGATURES {
            let p: Vec<char> = pattern.chars().collect();
            if i + p.len() <= chars.len() && chars[i..i + p.len()] == p[..] {
                spans[i] = len;
                marked += 1;
                advance = p.len();
                break;
            }
        }
        i += advance;
    }
    spans
}

/// [`align_cache_to_text`] for text with ligatures: a ligature glyph consumes the characters it
/// covers, so the remaining glyphs keep their positions.
fn align_with_ligatures(layer: &TextLayer, cached: &[f32], extra: usize) -> Option<Vec<f32>> {
    let chars: Vec<char> = layer.text.chars().collect();
    let spans = ligature_spans(&chars, extra);
    let mut next = 0usize;
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if matches!(chars[i], '\n' | '\r') {
            next += 1;
            i += 1;
            continue;
        }
        let &value = cached.get(next)?;
        out.push(value);
        next += 1;
        i += if spans[i] > 0 { spans[i] } else { 1 };
    }
    if cached.len() > next + 2 {
        return None;
    }
    Some(out)
}

/// Collects every real `3` (a composer advance delta in points) under `node`.
fn collect_composer_deltas(node: &crate::engine_data::Value, out: &mut Vec<f32>, depth: usize) {
    use crate::engine_data::Value;
    if depth > 64 {
        return;
    }
    match node {
        Value::Dict(items) => {
            for (key, value) in items {
                if key == "3" {
                    if let Some(delta) = value.as_f64().filter(|d| d.is_finite()) {
                        out.push(delta as f32);
                    }
                } else {
                    collect_composer_deltas(value, out, depth + 1);
                }
            }
        }
        Value::Array(items) => {
            for value in items {
                collect_composer_deltas(value, out, depth + 1);
            }
        }
        _ => {}
    }
}

/// Whether a layer left to the built-in renderer should say so (`PHOTOCRAFT_DEBUG_RENDER_RE=1`).
pub fn debug_enabled() -> bool {
    std::env::var_os("PHOTOCRAFT_DEBUG_RENDER_RE").is_some()
}

/// Where `PHOTOCRAFT_DEBUG_RENDER_RE` asks for the machine-readable layer dump.
///
/// The variable's value is a file path; the usual truthy values (`1`, `true`) keep the plain
/// stderr trail only. The dump is one JSON object per rendered text layer, appended, and is
/// meant for fitting work on the engine's glyph placement: it carries the layer's geometry, the
/// `Txt2` object index the bridge read and that object's composed glyph ids and spacing values.
fn debug_dump_path() -> Option<std::path::PathBuf> {
    let value = std::env::var("PHOTOCRAFT_DEBUG_RENDER_RE").ok()?;
    let truthy = matches!(value.as_str(), "1" | "true" | "yes" | "on");
    (!truthy && !value.is_empty()).then(|| std::path::PathBuf::from(value))
}

/// Minimal JSON string escaping for the layer dump.
fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Appends one layer record to the debug dump, if one was asked for.
fn dump_layer(layer: &TextLayer, dpi: f32, atlas_count: usize, rect: Rect, txt2: Option<&[u8]>) {
    let Some(path) = debug_dump_path() else { return };
    let index = layer.psd_raw.as_deref().and_then(|tysh| crate::psd::text_index(tysh));
    let mut gids = Vec::new();
    let mut spacing = Vec::new();
    let _ = txt2
        .and_then(crate::psd::parse_txt2)
        .and_then(|parsed| {
            let objects = parsed.path(&["1", "1"]).and_then(|v| v.as_array())?;
            let cache = usize::try_from(index.unwrap_or(-1)).ok().and_then(|i| objects.get(i)).and_then(|o| o.get("1"))?;
            collect_cache_glyphs(cache, &mut gids);
            collect_cache_spacing(cache, &mut spacing);
            Some(())
        });
    // The shaped glyph ids the bridge's own alignment assigns to the text: the cache's gid
    // stream walked against the model text (paragraph marks skipped, ligatures spanning two or
    // three characters). Empty when the two cannot be lined up.
    let aligned_gids: Vec<String> = {
        let as_floats: Vec<f32> = gids.iter().map(|g| *g as f32).collect();
        let newlines = layer.text.chars().filter(|c| matches!(c, '\n' | '\r')).count();
        let chars = layer.text.chars().count();
        let extra = (chars + newlines + 1).saturating_sub(as_floats.len());
        let aligned = align_cache_to_text(layer, &as_floats).or_else(|| {
            (extra > 0 && extra < 16).then(|| align_with_ligatures(layer, &as_floats, extra)).flatten()
        });
        aligned
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|g| format!("{}", *g as i64))
            .collect()
    };
    let gids: Vec<String> = gids.iter().map(|g| g.to_string()).collect();
    let spacing: Vec<String> = spacing.iter().map(|v| format!("{v:.4}")).collect();
    let line = format!(
        "{{\"layer\":null,\"txt2_index\":{},\"text\":\"{}\",\"dpi\":{},\"rect\":[{},{},{},{}],\"atlas_count\":{},\"cache_gids\":[{}],\"cache_spacing\":[{}],\"aligned_gids\":[{}]}}\n",
        index.unwrap_or(-1),
        json_escape(&layer.text),
        dpi,
        rect.x0,
        rect.y0,
        rect.x1,
        rect.y1,
        atlas_count,
        gids.join(","),
        spacing.join(","),
        aligned_gids.join(","),
    );
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write as _;
        let _ = file.write_all(line.as_bytes());
    }
}

/// Collects the composed cache's glyph ids in composed order (same walk as the spacing
/// collector, which carries the shape documentation).
fn collect_cache_glyphs(cache: &crate::engine_data::Value, out: &mut Vec<i64>) {
    let Some(pcs) = cache.get("2").and_then(|v| v.as_array()) else { return };
    for pc in pcs {
        let Some(lines) = pc
            .get("6")
            .and_then(|v| v.as_array().and_then(|a| a.first()))
            .and_then(|f| f.get("6"))
            .and_then(|v| v.as_array().and_then(|a| a.first()))
            .and_then(|r| r.get("6"))
            .and_then(|v| v.as_array().and_then(|a| a.first()))
            .and_then(|r2| r2.get("6"))
            .and_then(|v| v.as_array())
        else {
            return;
        };
        for line in lines {
            let Some(g) = line
                .get("6")
                .and_then(|v| v.as_array().and_then(|a| a.first()))
                .and_then(|s| s.get("6"))
                .and_then(|v| v.as_array().and_then(|a| a.first()))
            else {
                return;
            };
            if let Some(gids) = g.get("5").and_then(|v| v.as_array()) {
                out.extend(gids.iter().filter_map(|v| v.as_f64()).map(|v| v as i64));
            }
        }
    }
}

/// The warp parameters and flat layout geometry Photoshop stores for a warped layer.
///
/// `TySh` carries the text-space layout box in its four trailing floats (the `bounds`
/// descriptor's rectangle): the warp maps that flat box onto the layer's stored, warped
/// rectangle. A layer without a usable box keeps the built-in renderer's fallback.
fn warp_inputs(layer: &TextLayer, tx: f32, ty: f32) -> Option<(photocraft_text_re::warp::Warp, photocraft_text_re::warp::FlatGeometry)> {
    let w = layer.warp.as_ref().filter(|w| w.style != "warpNone" && w.value.abs() > 1e-3)?;
    let bounds = layer.psd_raw.as_deref().and_then(|tysh| crate::psd::parse_tysh(tysh)).map(|tysh| tysh.bounds)?;
    if !bounds.iter().all(|v| v.is_finite()) || bounds.iter().all(|v| v.abs() < 1e-6) {
        return None;
    }
    let warp = photocraft_text_re::warp::Warp {
        style: photocraft_text_re::warp::Style::from_name(&w.style),
        value: w.value,
        perspective: w.horizontal_distortion,
        other: w.vertical_distortion,
    };
    let flat = photocraft_text_re::warp::FlatGeometry {
        bounds: (tx + bounds[0], ty + bounds[1], tx + bounds[2], ty + bounds[3]),
        baseline: ty,
    };
    Some((warp, flat))
}

/// Rasterises `layer` into a document-space cache surface./// Rasterises `layer` into a document-space cache surface.
///
/// Returns the surface and the box of document pixels it covers, exactly like
/// [`crate::TextEngine::render_layer`] does for its own renderer. The surface has an alpha
/// channel whatever the document's format, as text layers always keep transparency.
pub fn render_layer(layer: &TextLayer, dpi: f32, format: PixelFormat) -> Result<(Surface, Rect), Unsupported> {
    trace_layer_fonts(layer);
    render_layer_with_txt2(layer, dpi, format, None)
}

/// [`render_layer`] with the document's `Txt2` block for the `Txt2`-only character features
/// (old-style figures); a layer rendered without it still draws, with the features off.
pub fn render_layer_with_txt2(layer: &TextLayer, dpi: f32, format: PixelFormat, txt2: Option<&[u8]>) -> Result<(Surface, Rect), Unsupported> {
    render_layer_into(layer, dpi, format, txt2, None)
}

/// Photoshop's First baseline choice for a horizontal box/area layer, when the
/// document's `Txt2` block is one Photoshop composes from.
///
/// Photoshop places area text's first line by the frame's First baseline
/// (`Txt2` `/0 /8 /0 [frame] /0 /2 /10 /0`): the record absent (or `1`) means
/// **Ascent** — the ink top of `d`, `0.69499 * size` for Times — while `2`
/// means **Cap Height**, what the engine used for every layer before this
/// choice was read. A file written by a PhotoCraft build that left the record
/// out is therefore composed 1-3 px lower by Photoshop than by the app (the
/// user's `Untitled-16.psd`; `out/verify/untitled16/`), and that is the offset
/// reported here.
///
/// The choice applies only while the block is current: `Txt2` validity is the
/// same scaffolding test the importer uses, and a layer whose object still
/// holds another text (a stale block) would be composed by Photoshop from
/// `TySh`, so `None` keeps the engine's cap-height rule there. A layer the
/// block has no current object for is left to the default too: the save writes
/// it a frame without the record, which Photoshop composes as Ascent, so the
/// default is returned once the block itself is valid.
fn txt2_first_baseline(
    layer: &TextLayer,
    txt2: Option<&[u8]>,
) -> Option<photocraft_text_re::area::FirstBaseline> {
    use photocraft_text_re::area::FirstBaseline;
    if matches!(layer.shape, TextShape::Point) {
        return None;
    }
    let block = txt2?;
    if !crate::psd::txt2_is_valid(block) {
        return None;
    }
    let object = layer
        .psd_raw
        .as_deref()
        .and_then(|tysh| crate::psd::text_index(tysh))
        .and_then(|index| usize::try_from(index).ok());
    let current = object.is_some_and(|object| {
        photocraft_text_re::txt2::object_text(block, object).is_some_and(|stored| {
            let normalize = |text: &str| {
                text.replace('\r', "\n").trim_end_matches('\n').to_string()
            };
            normalize(&stored) == normalize(&layer.text)
        })
    });
    if current {
        match object.and_then(|object| photocraft_text_re::txt2::object_first_baseline(block, object)) {
            Some(FirstBaseline::CapHeight) => Some(FirstBaseline::CapHeight),
            _ => Some(FirstBaseline::Ascent),
        }
    } else {
        // The save writes this layer a frame without `/2 /10`, Photoshop's
        // default Ascent.
        Some(FirstBaseline::Ascent)
    }
}

/// Whether Photoshop composes this layer with the `Txt2` frame's default **Ascent** first
/// baseline, i.e. [`render_layer_into`] would drop the rectangle its stored pixels carry.
///
/// A file importer uses it to tell that the pixels a file came with were composed by a build
/// that used the cap-height rule for every frame: those are stale for such a layer and an open
/// must re-compose it, as the recompose command (`type.updateAllTextLayers`) already does.
/// Vertical type takes no horizontal first baseline, so it is never re-composed for this rule
/// (the renderer picks the same branch).
pub fn txt2_uses_default_ascent(layer: &TextLayer, txt2: Option<&[u8]>) -> bool {
    layer.orientation == Orientation::Horizontal && matches!(txt2_first_baseline(layer, txt2), Some(photocraft_text_re::area::FirstBaseline::Ascent))
}

/// Whether an importer should re-compose this layer from the document's `Txt2` block on open.
///
/// Horizontal type follows [`txt2_uses_default_ascent`]. Vertical type has no horizontal first
/// baseline, but Photoshop still composes its columns from the block, so a file whose pixels were
/// rasterised by an older build keeps a stale cache that disagrees with Photoshop's open view for
/// that layer too; a current object in the block is what makes the block authoritative.
pub fn txt2_recomposable(layer: &TextLayer, txt2: Option<&[u8]>) -> bool {
    if layer.orientation == Orientation::Horizontal {
        return txt2_uses_default_ascent(layer, txt2);
    }
    let Some(block) = txt2 else { return false };
    if !crate::psd::txt2_is_valid(block) {
        return false;
    }
    let Some(index) = layer.psd_raw.as_deref().and_then(|tysh| crate::psd::text_index(tysh)).and_then(|index| usize::try_from(index).ok()) else {
        return false;
    };
    photocraft_text_re::txt2::object_text(block, index).is_some_and(|stored| {
        let normalize = |text: &str| text.replace('\r', "\n").trim_end_matches('\n').to_string();
        normalize(&stored) == normalize(&layer.text)
    })
}

/// [`render_layer`], painting into `target` (a document rectangle, `x, y, w, h`) when the layer
/// already has one — an imported layer's stored pixels keep the document geometry Photoshop gave
/// them, so re-rendering inside that same rectangle keeps the layer in place instead of moving it
/// by the difference between the two rasterisers' ink extents. `None` derives the rectangle from
/// the rendered ink, as a freshly created layer has no stored rectangle.
pub fn render_layer_into(
    layer: &TextLayer,
    dpi: f32,
    format: PixelFormat,
    txt2: Option<&[u8]>,
    target: Option<(i32, i32, u32, u32)>,
) -> Result<(Surface, Rect), Unsupported> {
    trace_layer_fonts(layer);
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
    // A uniform scale (the Type tool's free transform, or an imported scaled layer) is folded
    // into the resolution: the engine rasterises the glyphs at the transformed size, exactly
    // like Photoshop's own transformed type, instead of resampling a smaller raster.
    let uniform_scale = b.abs() < 1e-6 && c.abs() < 1e-6 && (a - d).abs() < 1e-6 && a > 0.0;
    let render_dpi = if uniform_scale { dpi * a } else { dpi };
    // Rotation, shear and non-uniform scaling cannot be expressed as a resolution change, so
    // they are applied to the finished raster in document space (see the end of this function).
    let affine: Option<[f32; 4]> = (!uniform_scale).then_some([a, b, c, d]);
    // Warped layers are placed by the engine's fitted bends; `PHOTOCRAFT_WARP_RE=0` restores
    // the built-in renderer for bisecting a warp report.
    let warped = layer.warp.as_ref().is_some_and(|w| w.style != "warpNone" && w.value.abs() > 1e-3);
    let engine_warp = std::env::var("PHOTOCRAFT_WARP_RE").ok().as_deref() != Some("0");
    if warped && !engine_warp {
        return Err(Unsupported("warped text".into()));
    }
    let ((width, height), (x, y)) = text_box(layer)?;
    // A uniform scale is folded into the render resolution, so the engine works in *document*
    // pixels: the text-space box and its origin have to be scaled by the same factor, or the
    // glyphs (rendered at `dpi * scale`) would be laid out in an unscaled box. A 72 -> 144 ppi
    // resolution change (which keeps the document's pixels and gives the layer a 0.5 scale)
    // otherwise collapsed area text from 47 px high to 31 because the box doubled while the
    // glyphs did not.
    let shape_scale = if uniform_scale { a } else { 1.0 };
    let format = PixelFormat { alpha: true, ..format };
    if layer.text.is_empty() {
        return Ok((Surface::new(format), Rect::EMPTY));
    }

    // The exporter's own writer: the reimplementation lays out exactly the document a saved
    // PSD would carry, and the model's paragraph sheets are passed beside it so its own
    // `Txt2` values stay authoritative.
    let engine_data_bytes = crate::engine_data::write(&crate::psd::build_engine_data(layer, None, render_dpi));
    let ed = engine_data::parse(&engine_data_bytes).map_err(|e| Unsupported(format!("its EngineData does not parse: {e}")))?;
    // `EngineData` stores paragraph ends as CR, the model uses LF.
    let text = layer.text.replace('\n', "\r");
    let paragraphs = paragraph_runs(layer, render_dpi, &text);

    let guard = store().lock().unwrap_or_else(|e| e.into_inner());
    let atlas_count = guard.atlases().len();
    let box_size = (width * shape_scale, height * shape_scale);
    // The reimplementation is a port in progress: it may refuse a layer with an error, and a
    // missing font currently reaches an `assert!` there. Neither may take the canvas down, so
    // a panic during layout is caught and the layer falls back like any other refusal.
    let style_runs = photocraft_text_re::runs::from_engine_data(&ed).len();
    let mut features = run_features(layer, style_runs, txt2);
    let composer = paragraphs.iter().any(|p| p.every_line_composer);
    // The composed cache carries Photoshop's own optical corrections when the layer came from a
    // PSD; a fresh or edited layer has no such cache and its model runs say the mode directly.
    let optical = cache_is_optical(layer, txt2) || model_is_optical(layer);
    // Optical kerning's pair corrections live in the cache exactly like the composer's nudges,
    // and the engine tells them apart by their size and variation (a large, varying correction is
    // optical and replaces the metrics advance; a uniform tiny one is tracking/composer). Passing
    // them needs the same alignment as the composer deltas.
    let deltas = if (composer || optical) && std::env::var_os("PHOTOCRAFT_ADVANCES").is_none_or(|v| v != "0") {
        advance_deltas(layer, text.encode_utf16().count() + 2, txt2)
    } else {
        Vec::new()
    };
    if let Some(first) = features.first_mut() {
        first.advance_deltas = deltas;
    }
    if optical {
        for feature in &mut features {
            feature.optical_kerning = true;
        }
    }
    let vertical = layer.orientation != Orientation::Horizontal;
    let baseline_kind = if vertical { None } else { txt2_first_baseline(layer, txt2) };
    let (ascender_ratio, first_baseline) = match baseline_kind {
        Some(kind) => (
            layer_render::first_baseline_ratio_of_kind(&guard, &ed, kind),
            layer_render::first_baseline_offset_of_kind(&guard, &ed, kind).unwrap_or(0.0),
        ),
        None => (None, layer_render::first_baseline_offset_of(&guard, &ed).unwrap_or(0.0)),
    };
    // A layer composed with Photoshop's default Ascent was not composed by the rectangle
    // the cache carries (the app's old cap-height composition made it), so re-rendering
    // inside that rectangle would clip the lower line rows. Photoshop's own recompose moves
    // the layer, and so does this one: the ink defines the rectangle.
    let target = match baseline_kind {
        Some(photocraft_text_re::area::FirstBaseline::Ascent) => None,
        _ => target,
    };
    // `first_baseline` comes from the `EngineData` built at `render_dpi`, so it is already in
    // document pixels; only the text-space box origin is scaled. Point text is anchored at its
    // text origin (its first baseline) and a horizontal box at its top-left plus the first
    // baseline; a **vertical** box is anchored at the frame's top without it, because the
    // vertical renderer accounts for the first cell/pen itself — Photoshop's own composition
    // starts the column's em cell at the frame top (the composed cache's along origin `/8/1` is
    // 0), so adding the horizontal first baseline put every vertical box 32 px (48 pt Myriad)
    // too low. The same `anchor` feeds the stored-rectangle rerender below, so both paths agree.
    let anchor = if matches!(layer.shape, TextShape::Point) {
        (tx, ty)
    } else if vertical {
        (tx + x * shape_scale, ty + y * shape_scale)
    } else {
        (tx + x * shape_scale, ty + y * shape_scale + first_baseline)
    };
    // A vertical paragraph box wraps a column when it runs past the box height; point text has
    // no box and stays one column per hard break. Both entries share `vertical_prepare`, so the
    // layout in `layout_re` makes the same choice (the caret and the pixels cannot disagree).
    // A vertical paragraph box is anchored at its top-left with the first column on the box's
    // right edge, so the layout needs both sides: the width places the columns, the height wraps
    // them.
    let vertical_box = (!matches!(layer.shape, TextShape::Point)).then_some(box_size);
    // Subpixel phase: Photoshop keeps the fractional pen x and quantizes its glyph
    // bitmaps to a quarter-pixel grid (`round(frac*4) mod 4`; measured on eight
    // `H` layers at 48 pt, fractions k/8 — 0.125 and 0.250 render identically,
    // 0.875 and 0.000 do too, everything else differs; see
    // out/verify/sync/parity-log.md). Rendering at origin `(0, 0)` and blitting at
    // `round(anchor)` drops that fraction, so every phase rendered as phase 0.
    // Hand the fractional part of the anchor to the library and place the ink on
    // the anchor's integer floor instead; the phase is then baked into the
    // coverage. Only the x fraction: y snapping is a separate rule (PS keeps
    // integer baselines here, and the ink rows already match).
    let (fx, _fy) = (anchor.0 - anchor.0.floor(), anchor.1 - anchor.1.floor());
    // Scaled, rotated and warped layers take the `RectTarget` paths below, which
    // place the ink themselves; keep their origin integral so those stay exact.
    let transformed = affine.is_some();
    let phase = if transformed || warped { (0.0, 0.0) } else { (fx, 0.0) };
    if std::env::var("PHOTOCRAFT_PHASE_DBG").is_ok() {
        eprintln!("render_re: anchor={anchor:?} frac={fx:.4} phase={phase:?} target={:?} transformed={transformed}", target.is_some());
    }
    let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if vertical {
            match vertical_box {
                Some(box_size) => layer_render::render_vertical_from_engine_data_in_box(&guard, &ed, &text, phase, box_size),
                None => layer_render::render_vertical_from_engine_data(&guard, &ed, &text, phase),
            }
        } else {
            layer_render::render_from_engine_data_with_paragraphs_and_features_baseline(&guard, &ed, &text, &paragraphs, &features, box_size, phase, ascender_ratio)
                .or_else(|_| layer_render::render_from_engine_data_with_paragraphs(&guard, &ed, &text, &paragraphs, box_size, phase))
                .or_else(|_| layer_render::render_from_engine_data(&guard, &ed, &text, box_size, phase))
        }
    }));
    let pixels = match rendered {
        Ok(Ok(pixels)) => pixels,
        Ok(Err(e)) => return Err(Unsupported(format!("the reimplementation could not render it: {e}"))),
        Err(payload) => {
            let why = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "no message".into());
            return Err(Unsupported(format!("the reimplementation panicked: {why}")));
        }
    };
    if pixels.width == 0 || pixels.height == 0 {
        return Ok((Surface::new(format), Rect::EMPTY));
    }
    // Box and area type are anchored at their box's top-left plus the first baseline; point
    // text at the transform translation, which is its text origin and first baseline.
    // `LayerPixels::offset` is measured from that baseline (vertically) and the text origin
    // (horizontally), the same contract the PSD-rewriting examples use.
    let (anchor_x, anchor_y) = anchor;
    // The phase was consumed by the render origin, so the ink's own offset is
    // measured from the anchor's floor.
    let (left, top) = (anchor_x.floor() + pixels.offset.0, anchor_y.floor() + pixels.offset.1);
    if !left.is_finite() || !top.is_finite() {
        return Err(Unsupported("its ink lands at a non-finite position".into()));
    }
    // The raster is drawn on the text-space pixel grid; the integer translation keeps it on
    // the document's grid (the layer rectangles Photoshop stores are whole pixels).
    let (left_f, top_f) = (left, top);
    let (canvas_x, canvas_y) = (left.round() as i32, top.round() as i32);
    // The canvas can carry blank rows or columns the layout's ink estimate put there (the
    // capture-atlas bitmaps start on their own pixel row); crop to the pixels actually covered
    // so the surface's rectangle is the ink's, as a saved PSD layer's is.
    let (pw, ph) = (pixels.width as usize, pixels.height as usize);
    let (mut x0, mut y0, mut x1, mut y1) = (pw, ph, 0usize, 0usize);
    for row in 0..ph {
        for col in 0..pw {
            if pixels.rgba[(row * pw + col) * 4 + 3] != 0 {
                x0 = x0.min(col);
                x1 = x1.max(col + 1);
                y0 = y0.min(row);
                y1 = y1.max(row + 1);
            }
        }
    }
    if x1 <= x0 || y1 <= y0 {
        return Ok((Surface::new(format), Rect::EMPTY));
    }
    let rect = Rect::from_xywh(canvas_x + x0 as i32, canvas_y + y0 as i32, (x1 - x0) as u32, (y1 - y0) as u32);

    // Two-pass export: Photoshop's own composition — and the library example — render the ink
    // *into* the stored rectangle (`RectTarget`), so re-render through that contract instead of
    // translating the origin-0 raster. The translation path is where the app's saved layer put
    // every horizontal continuation line 1 px above the library render: same renderer, but the
    // frame's effect on the line stepping was lost (out/verify/sync/app-export-path.md).
    if !vertical && !warped && affine.is_none() && target.is_none() && !rect.is_empty() {
        let (ox, oy, ow, oh) = (rect.x0, rect.y0, rect.width().max(1), rect.height().max(1));
        let rect_target = RectTarget { origin: (ox, oy), size: (ow, oh), anchor };
        let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            layer_render::render_from_engine_data_in_rect_with_features_baseline(
                &guard, &ed, &text, &paragraphs, &features, box_size, (0.0, 0.0), rect_target,
                ascender_ratio,
            )
        }));
        if let Ok(Ok(out)) = rendered {
            if out.width == ow && out.height == oh {
                let out_rect = Rect::from_xywh(ox, oy, ow, oh);
                let mut surface = Surface::new(format);
                surface.write_region_par(out_rect, &rgba_to_samples(&out.rgba, format.channels()));
                surface.prune();
                drop(guard);
                return Ok((surface, out_rect));
            }
        }
    }

    // Warped type. Horizontal text bends analytically: the engine maps the glyph outlines
    // through the fitted 4x4 patch and rasterises them in the stored rectangle. Vertical text has
    // no analytical outline path in the engine yet, so its finished flat raster goes through the
    // same fitted patch with `warp::apply` (the bitmap remap the engine still ships). A freshly
    // bent layer has no stored rectangle, so one is derived from the flat ink with room for the
    // bend and the result is cropped back to its own ink.
    if warped {
        let Some((warp, flat)) = warp_inputs(layer, tx, ty) else {
            return Err(Unsupported("a warped layer without `TySh` flat bounds yet".into()));
        };
        let stored = target.map(|(sx, sy, sw, sh)| Rect::from_xywh(sx, sy, sw, sh));
        let pad = (rect.width().max(rect.height()) as i32).max(8);
        let out = stored.unwrap_or_else(|| {
            Rect::from_xywh(rect.x0 - pad, rect.y0 - pad, rect.width() + 2 * pad as u32, rect.height() + 2 * pad as u32)
        });
        let (ox, oy, ow, oh) = (out.x0, out.y0, out.width().max(1), out.height().max(1));
        let (rgba, out_rect) = if vertical {
            let surface = FlatSurface { origin: (canvas_x, canvas_y), width: pw.max(1) as u32, height: ph.max(1) as u32 };
            (photocraft_text_re::warp::apply(&pixels.rgba, surface, flat, &warp, (ox, oy), (ow, oh)), Rect::from_xywh(ox, oy, ow, oh))
        } else {
            let rect_target = RectTarget { origin: (ox, oy), size: (ow, oh), anchor };
            let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                layer_render::render_from_engine_data_warped(&guard, &ed, &text, &paragraphs, &features, box_size, &warp, flat, rect_target)
            }));
            match rendered {
                Ok(Ok(out)) => (out.pixels.rgba, Rect::from_xywh(out.rect.0, out.rect.1, out.rect.2.max(1), out.rect.3.max(1))),
                Ok(Err(e)) => return Err(Unsupported(format!("the reimplementation could not warp it: {e}"))),
                Err(_) => return Err(Unsupported("the reimplementation panicked while warping it".into())),
            }
        };
        let (rgba, rect) = match affine {
            Some(linear) => transform_raster(&rgba, out_rect, linear, (tx, ty)),
            None => (rgba, out_rect),
        };
        // A synthesized rectangle is a work canvas; the layer is its ink, like a fresh layer.
        let (rgba, rect) = if stored.is_none() { crop_to_ink(&rgba, rect) } else { (rgba, rect) };
        if rect.is_empty() {
            return Ok((Surface::new(format), Rect::EMPTY));
        }
        let mut surface = Surface::new(format);
        surface.write_region_par(rect, &rgba_to_samples(&rgba, format.channels()));
        surface.prune();
        drop(guard);
        if debug_enabled() {
            eprintln!(
                "render_re: warp into rect=({},{},{},{}) style={} value={} ed={}B runs={} adv={}",
                rect.x0, rect.y0, rect.x1, rect.y1,
                layer.warp.as_ref().map_or("", |w| w.style.as_str()),
                layer.warp.as_ref().map_or(0.0, |w| w.value),
                engine_data_bytes.len(), features.len(),
                features.first().map_or(0, |f| f.advance_deltas.len())
            );
        }
        let visible = stored.map_or(rect, |stored| stored.intersect(&rect));
        dump_layer(layer, dpi, atlas_count, visible, txt2);
        return Ok((surface, visible));
    }

    // Rotation, shear and non-uniform scaling: the finished document-space raster is rewritten
    // through the linear part about the text origin (see `transform_raster`). The stored
    // rectangle is not reused for these, because it belongs to the transformed raster.
    if let Some(linear) = affine {
        let (rgba, rect) = transform_raster(&pixels.rgba, rect, linear, (tx, ty));
        let (rgba, rect) = crop_to_ink(&rgba, rect);
        if rect.is_empty() {
            return Ok((Surface::new(format), Rect::EMPTY));
        }
        let mut surface = Surface::new(format);
        surface.write_region_par(rect, &rgba_to_samples(&rgba, format.channels()));
        surface.prune();
        drop(guard);
        if debug_enabled() {
            eprintln!(
                "render_re: re transform linear=({:.3},{:.3},{:.3},{:.3}) rect=({},{},{},{})",
                linear[0], linear[1], linear[2], linear[3], rect.x0, rect.y0, rect.x1, rect.y1
            );
        }
        dump_layer(layer, dpi, atlas_count, rect, txt2);
        return Ok((surface, rect));
    }

    // A layer that already has pixels keeps their document rectangle: the two rasterisers'
    // ink extents differ by a pixel on unhinted edges, and re-painting inside the stored
    // rectangle holds the layer still. A bigger difference means the text or its geometry
    // changed since those pixels were made, and the layer grows into its own new bounds.
    if let Some((sx, sy, sw, sh)) = target {
        let stored = Rect::from_xywh(sx, sy, sw, sh);
        let close = (rect.x0 - stored.x0).abs() <= 2
            && (rect.y0 - stored.y0).abs() <= 2
            && (rect.x1 - stored.x1).abs() <= 2
            && (rect.y1 - stored.y1).abs() <= 2;
        if close {
            let rect_target = RectTarget { origin: (sx, sy), size: (sw, sh), anchor };
            let again = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if vertical {
                    // A vertical box anchors its columns at the box's right edge, so the
                    // stored-rectangle rerender needs the box too: without it every column landed
                    // left of the origin, the rerender came back empty, and the editor cached a
                    // blank layer (the user's u12 layers lost their text after an edit).
                    layer_render::render_vertical_from_engine_data_in_rect(&guard, &ed, &text, (0.0, 0.0), vertical_box, rect_target)
                } else {
                    layer_render::render_from_engine_data_in_rect_with_features_baseline(&guard, &ed, &text, &paragraphs, &features, box_size, (0.0, 0.0), rect_target, ascender_ratio)
                }
            }));
            if let Ok(Ok(pixels)) = again {
                // A rerender that comes back blank is a refusal, not a result: caching it would
                // silently drop the layer's pixels (the text "disappears" after an edit). Fall
                // back to the canvas renderer instead, which still draws the glyphs.
                if !pixels.rgba.as_chunks::<4>().0.iter().any(|px| px[3] != 0) {
                    return Err(Unsupported("the reimplementation rerendered the layer blank".into()));
                }
                let channels = format.channels();
                let scale = 1.0 / 255.0;
                let mut data = Vec::with_capacity(pixels.rgba.len() / 4 * channels);
                for px in pixels.rgba.as_chunks::<4>().0 {
                    data.extend_from_slice(&[
                        f32::from(px[0]) * scale,
                        f32::from(px[1]) * scale,
                        f32::from(px[2]) * scale,
                        f32::from(px[3]) * scale,
                    ]);
                }
                let mut surface = Surface::new(format);
                surface.write_region_par(stored, &data);
                surface.prune();
                drop(guard);
                if debug_enabled() {
                    eprintln!(
                        "render_re: re into rect=({},{},{},{}) ed={}B paras={} runs={} old_style={} adv={} px={}x{}",
                        stored.x0, stored.y0, stored.x1, stored.y1, engine_data_bytes.len(), paragraphs.len(), features.len(),
                        features.iter().filter(|f| f.old_style).count(), features.first().map_or(0, |f| f.advance_deltas.len()), pixels.width, pixels.height
                    );
                }
                dump_layer(layer, dpi, atlas_count, stored, txt2);
                return Ok((surface, stored));
            }
        }
    }
    drop(guard);

    // Straight RGBA8 → the surface's normalised, straight-alpha samples (the colour model is
    // RGB, checked above; alpha is last, as `Surface::channels` expects).
    let channels = format.channels();
    let mut data = Vec::with_capacity((x1 - x0) * (y1 - y0) * channels);
    let scale = 1.0 / 255.0;
    for row in y0..y1 {
        for col in x0..x1 {
            let at = (row * pw + col) * 4;
            let px = &pixels.rgba[at..at + 4];
            data.extend_from_slice(&[
                f32::from(px[0]) * scale,
                f32::from(px[1]) * scale,
                f32::from(px[2]) * scale,
                f32::from(px[3]) * scale,
            ]);
        }
    }
    dump_layer(layer, dpi, atlas_count, rect, txt2);
    let mut surface = Surface::new(format);
    surface.write_region_par(rect, &data);
    surface.prune();
    if debug_enabled() {
        eprintln!(
            "render_re: re box={:?} origin=({:.3},{:.3}) ed={}B paras={} runs={} old_style={} adv={} px={}x{} offset=({:.3},{:.3}) ink=({:.4},{:.4}) rect=({},{},{},{})",
            box_size, x, y, engine_data_bytes.len(), paragraphs.len(), features.len(), features.iter().filter(|f| f.old_style).count(), features.first().map_or(0, |f| f.advance_deltas.len()), pixels.width, pixels.height, pixels.offset.0, pixels.offset.1, left_f, top_f, rect.x0, rect.y0, rect.x1, rect.y1
        );
    }
    Ok((surface, rect))
}

/// Straight RGBA8 -> the surface's normalised, straight-alpha samples.
fn rgba_to_samples(rgba: &[u8], channels: usize) -> Vec<f32> {
    let scale = 1.0 / 255.0;
    let mut data = Vec::with_capacity(rgba.len() / 4 * channels);
    for px in rgba.as_chunks::<4>().0 {
        data.extend_from_slice(&[f32::from(px[0]) * scale, f32::from(px[1]) * scale, f32::from(px[2]) * scale, f32::from(px[3]) * scale]);
    }
    data
}

/// The pixels actually inked in `rgba` (which covers `rect`) and their box.
fn crop_to_ink(rgba: &[u8], rect: Rect) -> (Vec<u8>, Rect) {
    let (w, h) = (rect.width() as usize, rect.height() as usize);
    if w == 0 || h == 0 || rgba.len() < w * h * 4 {
        return (Vec::new(), Rect::EMPTY);
    }
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
    for row in 0..h {
        for col in 0..w {
            if rgba[(row * w + col) * 4 + 3] != 0 {
                x0 = x0.min(col);
                x1 = x1.max(col + 1);
                y0 = y0.min(row);
                y1 = y1.max(row + 1);
            }
        }
    }
    if x1 <= x0 || y1 <= y0 {
        return (Vec::new(), Rect::EMPTY);
    }
    let (cw, ch) = (x1 - x0, y1 - y0);
    let mut out = Vec::with_capacity(cw * ch * 4);
    for row in y0..y1 {
        let at = (row * w + x0) * 4;
        out.extend_from_slice(&rgba[at..at + cw * 4]);
    }
    (out, Rect::from_xywh(rect.x0 + x0 as i32, rect.y0 + y0 as i32, cw as u32, ch as u32))
}

/// Rewrites a finished document-space raster through the layer transform's linear part.
///
/// The engine can fold a *uniform* scale into the resolution, but rotation, shear and
/// non-uniform scaling cannot be expressed that way. The layer transform maps text space to
/// document space as `p_doc = linear * p_text + t`; the raster already sits at `p_doc = p_text
/// + t`, so the rewrite is `p_doc = linear * (p_flat - t) + t`, sampled bilinearly.
fn transform_raster(rgba: &[u8], rect: Rect, linear: [f32; 4], t: (f32, f32)) -> (Vec<u8>, Rect) {
    let [a, b, c, d] = linear;
    let det = a * d - b * c;
    let (sw, sh) = (rect.width() as usize, rect.height() as usize);
    if !det.is_finite() || det.abs() < 1e-9 || sw == 0 || sh == 0 || rgba.len() < sw * sh * 4 || !t.0.is_finite() || !t.1.is_finite() {
        return (Vec::new(), Rect::EMPTY);
    }
    let (mut min, mut max) = ((f32::MAX, f32::MAX), (f32::MIN, f32::MIN));
    for (x, y) in [(rect.x0 as f32, rect.y0 as f32), (rect.x1 as f32, rect.y0 as f32), (rect.x0 as f32, rect.y1 as f32), (rect.x1 as f32, rect.y1 as f32)] {
        let (dx, dy) = (x - t.0, y - t.1);
        let (px, py) = (a * dx + c * dy + t.0, b * dx + d * dy + t.1);
        if !px.is_finite() || !py.is_finite() {
            return (Vec::new(), Rect::EMPTY);
        }
        min = (min.0.min(px), min.1.min(py));
        max = (max.0.max(px), max.1.max(py));
    }
    let (x0, y0) = (min.0.floor() as i32, min.1.floor() as i32);
    let out = Rect::from_xywh(x0, y0, (max.0.ceil() - min.0.floor()).max(1.0) as u32, (max.1.ceil() - min.1.floor()).max(1.0) as u32);
    let (w, h) = (out.width() as usize, out.height() as usize);
    let mut dst = vec![0u8; w * h * 4];
    let inv = 1.0 / det;
    for oy in 0..h {
        for ox in 0..w {
            let qx = out.x0 as f32 + ox as f32 + 0.5;
            let qy = out.y0 as f32 + oy as f32 + 0.5;
            let (dx, dy) = (qx - t.0, qy - t.1);
            let px = (d * dx - c * dy) * inv + t.0;
            let py = (-b * dx + a * dy) * inv + t.1;
            let fx = px - rect.x0 as f32 - 0.5;
            let fy = py - rect.y0 as f32 - 0.5;
            if fx < -0.5 || fy < -0.5 || fx > sw as f32 - 0.5 || fy > sh as f32 - 0.5 {
                continue;
            }
            let (fx0, fy0) = (fx.floor(), fy.floor());
            let (tx, ty) = (fx - fx0, fy - fy0);
            let mut acc = [0f32; 4];
            for (sx, sy, wgt) in [(0, 0, (1.0 - tx) * (1.0 - ty)), (1, 0, tx * (1.0 - ty)), (0, 1, (1.0 - tx) * ty), (1, 1, tx * ty)] {
                let ix = (fx0 as i32 + sx).clamp(0, sw as i32 - 1) as usize;
                let iy = (fy0 as i32 + sy).clamp(0, sh as i32 - 1) as usize;
                let at = (iy * sw + ix) * 4;
                for k in 0..4 {
                    acc[k] += f32::from(rgba[at + k]) * wgt;
                }
            }
            let at = (oy * w + ox) * 4;
            for k in 0..4 {
                dst[at + k] = acc[k].round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    (dst, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_doc::text::{CharStyle, ParagraphRun, ParagraphStyle, TextRun};

    fn label(text: &str) -> TextLayer {
        TextLayer {
            text: text.into(),
            runs: vec![TextRun { len: text.len(), style: CharStyle::default() }],
            paragraphs: vec![ParagraphRun { len: text.len(), style: ParagraphStyle::default() }],
            ..Default::default()
        }
    }

    /// Nothing to draw: an empty layer is an empty cache, not an error.
    #[test]
    fn empty_text_renders_nothing() {
        let (surface, rect) = render_layer(&label(""), 72.0, PixelFormat::RGBA8).expect("empty layer");
        assert_eq!(rect, Rect::EMPTY);
        assert_eq!(surface.format(), PixelFormat { alpha: true, ..PixelFormat::RGBA8 });
        assert_eq!(surface.tile_count(), 0);
    }

    /// The pipeline has no parley renderer left for these to fall back to, so the engine must
    /// cover them; only a colour mode it cannot write (and type on a path, which has no
    /// engine layout at all) may still refuse.
    #[test]
    fn only_unsupported_colour_and_path_layers_are_refused() {
        let mut vertical = label("Hi");
        vertical.orientation = Orientation::Vertical;
        let (_, rect) = render_layer(&vertical, 72.0, PixelFormat::RGBA8).expect("vertical type is rendered");
        assert!(!rect.is_empty(), "vertical type has no ink");

        // A uniform scale is folded into the resolution, so the ink grows with it.
        let plain = render_layer(&label("Hi"), 72.0, PixelFormat::RGBA8).expect("point text");
        let mut scaled = label("Hi");
        scaled.transform = photocraft_geom::Affine::scale(2.0);
        let (_, scaled_rect) = render_layer(&scaled, 72.0, PixelFormat::RGBA8).expect("scaled type is rendered");
        assert!(scaled_rect.width() >= plain.1.width() * 3 / 2, "a 2x scale doubles the ink width");

        // A rotation is applied to the finished raster: the ink box stays a document box.
        let mut rotated = label("Hi");
        rotated.transform = photocraft_geom::Affine::rotate(std::f64::consts::FRAC_PI_4);
        let (_, rotated_rect) = render_layer(&rotated, 72.0, PixelFormat::RGBA8).expect("rotated type is rendered");
        assert!(!rotated_rect.is_empty());

        let mut path = label("Hi");
        path.shape = TextShape::Path { path: photocraft_doc::vector::Subpath::default(), start: 0.0, end: None, flip: false };
        assert!(render_layer(&path, 72.0, PixelFormat::RGBA8).is_err());

        let cmyk = PixelFormat::new(ColorMode::Cmyk, photocraft_color::SampleType::U8, true);
        assert!(render_layer(&label("Hi"), 72.0, cmyk).is_err());
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod font_parity_tests {
    use super::*;

    /// The pixel renderer must draw the faces Photoshop composes with, not a substitute: every
    /// name the PSD corpus and the sync matrix store has to be in its `FaceStore`.
    #[test]
    fn the_pixel_renderer_loads_every_photoshop_face() {
        if crate::fonts::PsFontIndex::scan().is_empty() {
            eprintln!("skipping: no Photoshop font sources on this machine");
            return;
        }
        let store = store().lock().unwrap_or_else(|e| e.into_inner());
        for ps in [
            "TimesNewRomanPSMT",
            "ArialMT",
            "NotoSans-Regular",
            "NotoSans-Bold",
            "NotoSerif-Regular",
            "MyriadPro-Regular",
            "MinionPro-Regular",
            "AdobeHeitiStd-Regular",
            "CCMeanwhile",
            "CCGrimNGritty-Regular",
        ] {
            assert!(store.face(ps).is_some(), "the renderer has no face for {ps}");
        }
        let first = store.first_face().and_then(|f| f.postscript_name().as_deref().map(str::to_string));
        assert!(first.is_some(), "the store is empty");
    }
}

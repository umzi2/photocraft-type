# Making `photocraft-text` the only text implementation

Goal: the fork keeps **no** text logic. Layout, caret/hit-testing, shaping, rasterisation,
warping, PSD text authoring and PSD text import all live in
`/media/H/zed2/photocraft/photocraft-text` (and its PSD crate), and the fork only converts between
its document model and that API.

## Why (observed symptoms)

- **Caret/hit-testing disagrees with the pixels.** The canvas/layer pixels of a type layer come
  from `photocraft_text::layer_render` (`crates/text/src/render_re.rs`), but the caret, selection
  and hit-testing use the fork's own parley layout (`crates/text/src/layout.rs` →
  `type_caret_cmds`, `ui-egui/tools/type_tool`). Two engines, two answers: paragraph indents,
  leading, optical kerning and font fallback diverge, so the caret sits where the text would be
  in the fork's layout, not where it is drawn.
- **Warped (arc/arch) text renders wrong.** The warp path is split: `render_re` refuses warped
  layers without a target and falls back to the fork's parley renderer + `warp.rs` deformation,
  while the engine's warp renderer (`render_from_engine_data_warped`) is the one matched against
  Photoshop.
- **PSD text blocks drift** because the `Txt2`/`TySh` writer and reader are fork copies (see
  `docs/text-engine-migration.md`).

## Target architecture

```
        document model (crates/doc: TextLayer, CharStyle, ParagraphStyle, shape, warp)
                              │  adapter only (no text logic)
                              ▼
   photocraft_text::layer_render  ← fonts, shaping, layout, caret, raster, warp
   photocraft_text::psd           ← PSD Txt2/TySh authoring + import (LayerSpec-based)
   photocraft_psd                ← PSD container, TySh/Txt2 blocks, layer rasters, XMP
```

Fork side after the migration:

| Fork module | Becomes |
|---|---|
| `crates/text/src/layout*.rs` | adapter: model → EngineData-ish input → `layer_render::TextLayout` |
| `crates/text/src/render*.rs` | adapter: model → `layer_render` raster (no parley, no fallback) |
| `crates/text/src/warp.rs` | adapter: model warp → `photocraft_text::warp` |
| `crates/text/src/psd/*` | adapter: model ⇄ `photocraft_text::psd::{LayerSpec,…}` |
| `crates/text/src/{fonts,cjk,spell}.rs` | font database feeding `FaceStore` (names/weights only) |

## Workstreams

1. **U1 — upstream layout + caret API** (`photocraft-text`): expose the layout the renderer already
   uses (`layer_render`): glyphs with byte ranges, lines, caret clusters, caret rects, hit-testing,
   word/line navigation, bounds, overflow — for horizontal, area, vertical text; the same code path
   as `render_from_engine_data` so geometry cannot drift from pixels.
2. **U2 — upstream PSD authoring** (`photocraft-text::psd`): multi-layer `Txt2` writing (object
   index, fresh `EngineData`, `/5 1` character-run marker, preserve agreeing Photoshop objects),
   ported from the fork's `psd/build.rs`.
3. **U3 — upstream PSD import** (`photocraft-text::psd`): `Txt2`+`TySh` → `LayerSpec` reader.
4. **F1 — fork layout adapter**: replace parley layout with U1, keeping the fork's
   `TextLayout`/`ClusterInfo`/`LineInfo`/`PlacedGlyph` surface so engine/UI/PDF compile unchanged.
5. **F2 — fork render adapter**: all text pixels/outlines through the library; delete the parley
   renderer and the `render_re` fallbacks; fix warped (arc/arch) text.
6. **F3 — fork PSD adapter**: authoring/import through U2/U3.
7. **F4 — UI**: type tool overlay/caret/selection and warp handles on the new layout.

Order: U1 ∥ U2/U3 first; F1 after U1; F2 in parallel (no layout dependency); F3 after U2/U3;
F4 last.

## Verification

- Unit/integration: `cargo test -p photocraft-text@0.23.0`, `-p photocraft-io@0.23.0`,
  `-p photocraft-engine@0.23.0`, `-p photocraft-ui-egui@0.23.0`; clippy `--all-targets` for the
  fork crates the workstream touches.
- Caret parity: a headless check that, for each text layer of `out/psdcheck/showcase_psresave.psd`,
  every caret stop lies on the bounding box of the rendered glyphs of its cluster (the renderer and
  the caret API come from one layout, so this is an invariant, not a tolerance).
- Warp: re-render `Warp Arc 50%` / `Warp Arch 40%` layers and compare the layer pixels' ink box and
  coverage with Photoshop's flat raster (`psx_flat.jsx` / layer raster from
  `photocraft_psd::PsdFile::text_layer_raster`).
- PSD: Photoshop recompose oracle (`psx_recompose_any.jsx`): stored bounds vs recomposed bounds,
  and a Photoshop-authored optical layer with its cache intact must reproduce exactly.

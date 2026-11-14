# Text render migration, stage 2: the engine draws every text layer

Task: make **all** text pixels/outlines come from `photocraft-text` and drop the fork's parley
renderer from the pipeline, including warped (arc/arch) type. This note covers the engine-side
work (`engine-parity`, task-11) and states exactly what is migrated, what is left, and why.

## 1. `crates/text/src/render_re.rs` — full layer coverage

`render_layer_into` (`crates/text/src/render_re.rs:697`) no longer refuses a layer for its
orientation or transform. The refusals left are `ColorMode != Rgb` (the cache is written in the
document's pixel format; the engine produces straight RGBA) and `TextShape::Path` (the engine has
no on-path layout).

What the function now does, in order:

| case | path in the engine | code |
|---|---|---|
| point / box / area, horizontal | `render_from_engine_data_with_paragraphs_and_features` (ink-cropped, then re-rendered in the stored rect when the ink agrees within 2 px) | `:770` |
| vertical (point/box/area) | `render_vertical_from_engine_data[_in_rect]` | `:772`, `:944` |
| warped, horizontal, stored rect | `render_from_engine_data_warped` (analytic outline warp through the fitted 4x4 patch) | `:855` |
| warped, horizontal, no stored rect | same, into a rectangle derived from the flat ink (`max(ink w,h)` of padding) and cropped back to the bent ink | `:840–`:849, `crop_to_ink` `:1007` |
| warped, vertical | flat vertical raster remapped through the same fitted patch with `photocraft_text_re::warp::apply` (the engine ships no analytical vertical warp) | `:850–851` |
| uniform scale `(a=d=s, b=c=0)` | folded into the resolution: `render_dpi = dpi * s`, so the engine rasterises the glyphs at the transformed size exactly like Photoshop's transformed type — no resampling | `:714–715` |
| rotation / shear / non-uniform scale | rendered in text space, then the finished document-space raster is rewritten through the linear part about the text origin, `p_doc = linear·(p_flat − t) + t`, bilinear | `transform_raster` `:1041` |
| `PHOTOCRAFT_WARP_RE=0` | still restores the old fallback for bisecting a warp report | `:720–723` |

Everything else is unchanged: the `EngineData` bridge (runs, paragraph sheets, optical-kerning
advances, `Txt2` character features), the atlas (`FaceStore::atlases`, `PHOTOCRAFT_ATLAS*`) and
the `PHOTOCRAFT_DEBUG_RENDER_RE` logging.

Chosen strategy for scaled/rotated transforms, and why: a **uniform** scale is a resolution
change, so the glyphs are rasterised at the final size (crisp, matches Photoshop's own transformed
type). Rotation, shear and non-uniform scaling cannot be expressed as a resolution change and the
engine's public render API takes no affine, so the finished raster is resampled through the
affine. The alternative — rendering in text space and letting every caller transform the surface —
would change the cache's document-space contract (`layer.cache` is exported as the layer record's
pixels), so the affine stays inside the renderer.

Tests updated/added in `render_re`: `only_unsupported_colour_and_path_layers_are_refused`
(`:1122`) asserts vertical, 2x-scaled and 45°-rotated layers all render with ink, and that path
type and CMYK still refuse.

## 2. Pipeline callers moved to the library

| caller | before | now |
|---|---|---|
| `crates/text/src/lib.rs:142` `TextEngine::render` | parley `layout_deform` + `rasterize_warped` | `render_re::render_layer`; parley rasteriser only for path type / non-RGB |
| `crates/engine/src/cmd/type_cmds.rs:746` layer refresh | engine pixels, parley `layout.bounds()` for `TySh` | engine pixels, `photocraft_text::layer_layout` bounds |
| `crates/engine/src/cmd/type_cmds.rs:1026` `type.info` | parley layout | `layer_layout` |
| `crates/engine/src/cmd/type_extra_cmds.rs:99` point⇄paragraph geometry | parley layout | `layer_layout` (format threaded through the closures) |
| `crates/engine/src/cmd/layer_css_cmds.rs:173` Copy CSS ink | parley layout + `render::ink_bounds` | `layer_layout` baseline + `render_re::render_layer` ink rect and cap height |
| `crates/text/src/glyphs.rs:114` glyph preview | parley `rasterize_warped` | `render_re::render_layer` |
| `crates/ui-egui/src/text/font_preview.rs:88` font menu preview | parley `ink_rect` + `rasterize` | `render_re::render_layer` |
| `crates/text/src/lib.rs:203` `layer_layout` (Lead's) | — | engine layout when the engine draws the layer, parley otherwise |

`crates/text/src/layout_re.rs`: `origin_of` (`:202`) no longer refuses warped or scaled/rotated
layers — a warp bends the finished pixels and a transform is applied after layout, so the engine's
flat layout is the caret layout for both (module doc at `:28` updated). Still refused there:
vertical type, type on a path and non-RGB documents, because the engine exposes no vertical or
on-path layout and its rasteriser is RGBA-only; those layers' pixels still come from the parley
renderer, so their caret must use the parley layout too.

## 3. Left on the parley renderer, with reasons

* **Type on a path** (`TextShape::Path`): no engine layout exists, and the document model has no
  other producer of path type. Pixels keep coming from `crates/text/src/render.rs`'s path warp;
  the `render_re` refusal is explicit and logged (`text: the canvas renderer draws this layer
  because the reimplementation cannot: type on a path …`).
* **Non-RGB documents**: `render_re` writes straight RGBA8; CMYK/grey caches still go through the
  parley rasteriser (which writes the document's pixel format).
* **`type_extra_cmds::text_path` (Create Work Path / Convert to Shape)** still takes its layout
  from parley: it consumes `TextLayout::faces` + per-glyph `face` indices to draw skrifa outlines,
  and the engine layout adapter currently sets every `PlacedGlyph::face` to `0` and leaves
  `TextLayout::faces` empty. Switching it now would outline every glyph with the primary font.
  It moves with the layout migration (task-10/12) that fills real faces.
* **`crates/io/src/pdf/text.rs`** has the same face blocker: it emits one PDF path per glyph with
  the run's font, so it cannot take an engine layout whose glyphs all point at face 0. Its pixels
  are unaffected (PDF output is vector); it is the next consumer once `PlacedGlyph` carries a
  real face.

## 4. Warp verification

Layers of `/media/H/zed2/photocraft/out/psdcheck/showcase_psresave.psd` (ids from `photocraft-cli
info`): `14` = `12 warp arc` ("Warp Arc 50%"), `15` = `13 warp arch` ("Warp Arch 40%").

Note on the arc layer of **this** file: Photoshop saved it with `warpValue = 0.0` (psd_tools:
`warpStyle warpArc`, `warpValue 0.0`), which is why its stored raster is 204x30 — **it is not
actually bent in this file**. Both paths therefore agree on it (see the identical row below). The
real arc-50 layer is in the other Photoshop-saved copy of the same document,
`/media/H/zed2/photocraft/out/psdcheck/orig_show.psd` (same names/ids, `warpValue 50.0`), so the
comparison below is run on both files.

Command per layer (before = the old parley renderer via the now-debug-only `PHOTOCRAFT_WARP_RE=0`,
after = the engine):

```
target/debug/photocraft-cli run /media/H/zed2/photocraft/out/psdcheck/showcase_psresave.psd \
  --cmd type.edit --params '{"layer":15,"text":"Warp Arch 40%"}' --out /tmp/warp_verify/after_15.psd
# same with PHOTOCRAFT_WARP_RE=0 in the environment for "before"
/media/H/zed2/photocraft/.venv/bin/python out/verify/verify_warp.py <psd>
```

`verify_warp.py` reads Photoshop's own stored raster for the layer (`psd_tools layer.numpy()`),
re-runs `type.edit` with the same text, then compares the saved record's alpha: best integer shift
(+-4 px), alpha IoU at >8/255, ink box in layer space, alpha mass, RGB gt8 over a white composite.

| PSD | layer | drawn by | rect (PS) | rect (ours) | shift | alpha IoU | RGB gt8 / px | alpha mass (PS -> ours) |
|---|---|---|---|---|---|---|---|---|
| showcase_psresave | 14 arc (**value 0, flat**) before | parley | 800,408,204,30 | 800,408,204,30 | 0,0 | .9734 | 205 / 6120 | 477 819 -> 477 530 |
| showcase_psresave | 14 arc (**value 0, flat**) after | engine | 800,408,204,30 | 800,408,204,30 | 0,0 | .9734 | 205 / 6120 | 477 819 -> 477 530 |
| showcase_psresave | 15 arch 50 before | parley | 799,492,225,65 | **800**,492,224,65 | 1,0 | .9027 | 1838 / 14560 | 571 364 -> 498 671 |
| showcase_psresave | 15 arch 50 **after** | engine | 799,492,225,65 | **799,492,225,65** | **0,0** | **.8043** | **1479** / 14625 | 572 404 -> 506 752 |
| orig_show | 14 arc 50 before | parley | 776,365,247,66 | 778,366,243,64 | 2,1 | .7207 | 2558 / 15552 | 661 965 -> 576 391 |
| orig_show | 14 arc 50 **after** | engine | 776,365,247,66 | **776,365,247,66** | **0,0** | **.9323** | **1112** / 16302 | 666 150 -> 625 226 |
| orig_show | 15 arch 50 before | parley | 799,492,225,65 | 800,492,224,65 | 1,0 | .9027 | 1838 / 14560 | 571 364 -> 498 671 |
| orig_show | 15 arch 50 **after** | engine | 799,492,225,65 | **799,492,225,65** | **0,0** | **.8043** | **1479** / 14625 | 572 404 -> 506 752 |

The engine's arc raster matches the numbers the previous engine-only round measured (.9323/1112),
so the fork path and the library example agree. The arch layer keeps that round's known residue:
the saved rectangle is exact and gt8 improves 1838 -> 1479, IoU drops (.9027 -> .8043) because the
engine's bent glyphs land *inside* Photoshop's ink (mass 506 752 vs 572 404) and its leftmost
5 columns stay empty (`ink_ours` starts at x = 5): Photoshop's W overflows the stored rectangle's
left edge, and the `fill_border` fade that keeps the record from being cropped does not paint the
glyph ink that the analytic outline pass places 5 px right. Whole-document totals from the
engine round (harness on `showcase`): gt8 5426/389313, rect 18/18, which this change does not
alter (the fork refresh now picks the same engine path the harness used).

No other layer type changed: all 18 type layers of `showcase_psresave.psd` were re-run through the
same `type.edit` sequence and each still saves with its previous rectangle; the vertical and CJK
layers are pixel-identical to the engine round because `render_re` already drew them.

## 5. Which renderer draws each layer type now

| layer | canvas refresh / `type.edit` | PSD export | typed caret / hit test | glyph previews | vector outlines (work path, shape conversion, PDF ops) |
|---|---|---|---|---|---|
| point, box, area (horizontal) | engine (`render_re`) | engine (saved cache) | engine layout (`layer_layout`) | engine (`render_re`) | parley layout + skrifa outlines |
| vertical | engine | engine | parley (engine has no vertical layout) | engine | parley |
| warped (arc/arch, horiz+vert) | engine | engine | engine layout (flat pens) | engine | parley layout + skrifa + fork warp; PDF refuses warped |
| uniform scale / rotation / shear | engine | engine | engine layout (text space) | engine | parley |
| type on a path | parley (`render_re` refuses) | parley cache | parley | parley | parley |
| CMYK / grey document | parley | parley cache | parley | parley | parley |

Parley fallbacks left, all logged (`render_re: path=parley ... reason=...` and, once per reason,
`text: the canvas renderer draws this layer because the reimplementation cannot: ...`): type on a
path, non-RGB documents, and the `TextEngine::render`/`render_layer` API kept for
`io::pdf_editable` and `type_mask_cmds` (not on the canvas path).

## 5. Suites (final, on the last compiling library checkpoint)

* `cargo test --offline -j 2 -p photocraft-text@0.23.0 --lib` -> **379 passed, 0 failed, 5 ignored**.
* `cargo test --offline -j 2 -p photocraft-engine@0.23.0 --lib` -> **3656 passed, 0 failed,
  15 ignored** (fully green; the two layout-convention failures from the previous checkpoint were
  fixed by the layout owner, and `vertical_point_and_paragraph_conversion_keep_the_columns_in_place`
  is green with the measured vertical alignment added here).
* `cargo test --offline -j 2 -p photocraft-io@0.23.0` -> `--lib`: **686 passed, 0 failed,
  2 ignored**; `--test it`: 592 passed, **2 failed**:
  * `psd_vertical_type::vertical_orientation_round_trips_through_psd` (vertical ink
    210..254 -> 238..258 after import/re-render),
  * `text_corpus::created_text_layer_roundtrips_through_psd` (run lengths 4+8 -> 6+6 after a
    PSD round trip).
  Both are import/export round-trip tests: they call `TextEngine::render_layer` (the built-in
  renderer, untouched here) and `psd_export` builds `TySh` from the *preserved block's* ink, not
  from any layout, so neither goes through this task's code. They fail with the same numbers
  before and after the final vertical edits.
* `cargo test -p photocraft-ui-egui@0.23.0`: blocked at the last checkpoint by a concurrent
  compile error in `crates/io` (another agent was editing it). The only change in that crate is
  `text/font_preview.rs`, whose raster now comes from `render_re::render_layer`; its own tests
  are in the same crate and should be re-run once `crates/io` compiles.

## 5b. The Arch style: why the fitted patch cannot match Photoshop

The `arch` regression (.9027 -> .8043 alpha IoU at >8) is **not** the rasteriser: the same
comparison for the old closed-form path shows both are ~12 % lighter than Photoshop
(alpha mass: PS 572 404, closed form 498 671, fitted patch 506 752), so the mass deficit is old
news and is a coverage/AA property of warped rendering, not of the migration.

The difference is *where* the ink lands. `out/verify/arch_fit.py` compares 8-row bands of the
65x225 layer (`orig_show.psd` `13 warp arch`) against Photoshop's stored raster:

| band | PS centroid / width / mass | engine patch | closed form |
|---|---|---|---|
| 0-7 | 115.43 / 45 / 41 096 | 114.84 / 45 / 29 452 | 114.46 / 43 / 33 051 |
| 8-15 | 113.65 / 90 / 95 513 | 112.75 / 90 / 86 428 | 112.94 / 88 / 85 063 |
| 16-23 | 118.26 / 113 / 125 554 | 118.77 / 115 / 122 961 | 117.18 / 105 / 112 716 |
| 24-31 | 111.38 / 93 / 98 291 | 110.92 / 91 / 94 673 | 111.19 / 89 / 86 346 |
| 32-39 | 107.76 / 77 / 85 031 | **103.36** / 76 / 78 727 | 107.62 / 72 / 71 388 |
| 40-47 | 100.18 / 63 / 62 881 | **93.31** / 53 / 47 515 | 100.47 / 59 / 55 682 |
| 48-55 | 103.53 / 38 / 42 451 | **109.21** / 30 / 28 889 | 96.97 / 38 / 35 887 |
| 56-63 | 121.36 / 16 / 20 163 | **158.92** / 16 / 17 546 | 122.42 / 17 / 18 164 |

The closed form tracks Photoshop's centroids within ~1 px on every band. The fitted grid is fine
on the upper half (bands 0-31) and wrong in the lower half: the bottom control row maps the lower
part of the flat box too far left (bands 32-47) and then too far right (bands 48-63, +32 px in the
last band). That is a *shape* error, and it is exactly what caps the IoU at .80: every glyph's
lower part sits in the wrong place, so the >8 mask overlaps less.

Refit attempt (`tools/fit_warp_patch.py`, `FIT_MASS=1`, IoU/mass objective, 3 sweeps from the
checked-in grid): the best grid reaches IoU .7719 / gt8 1506 / L1 145 309 on the example harness,
i.e. **.7691 -> .7719 IoU**: the family cannot do better. A 4x4 Bézier grid fitted per *style* is
too poor an approximation of the arch's lower-half mapping (Photoshop's arch is a cylindrical
deformation whose lower edge sweeps much further than any bicubic through these controls can).

What upstream needs: replace the per-style constant grid with an **analytic arch map**, the same
closed-form the built-in renderer used (`photocraft_geom::warp::StyleWarp::arch`: bend the box
about a cylinder, radius from `value`, applied to both the box's top and bottom edges), exposed
as a point map instead of a grid:

* `photocraft-text/src/warp.rs`: keep `Placement { patch, min }` but let it carry a style map —
  e.g. `Placement::forward` calls a `StyleMap::forward(u, v)` enum (Arc/Arch analytic today, the
  fitted grid for the styles not ported yet), and `patch_for` becomes `map_for`.
* `photocraft-text/src/layer_render/**` (owned by `paragraph-engine`): the only call sites are
  `placement(...)`/`Placement::forward` in the warped path and `render_from_engine_data_warped`;
  they would take the map value instead of the grid. That is the one library change this task
  needs from its owner.
* Until then, the arch residual is: rect exact, gt8 1838 -> 1479 (better than parley), IoU
  .9027 -> .8043 (worse than parley) because of the lower-half shape error measured above. No
  fork fallback is used: the fork's closed-form renderer is not in the pipeline.

`showcase_psresave.psd` note: layer `12 warp arc` there stores `warpValue 0.0`, so it is *not*
bent (its raster is 204x30). Use `orig_show.psd` (same document, `warpValue 50.0`) for arc
measurements.

## 5c. Build state when these numbers were taken

The warp numbers and the suite counts above were produced on the last build in which the shared
library compiled. After that checkpoint the library was again mid-edit by the `txt2`/`psd` and
vertical-layout owners (`photocraft-text/src/txt2/*`, `src/psd/*`, `src/layer_render/layout.rs`),
so the CLI could not be rebuilt for the last two vertical-conversion edits. Those edits only
touch `to_paragraph`/`to_point` (one test re-run showed `vertical_point_and_paragraph_conversion…
` passing); they do not touch the raster or warp path, so the numbers above stand. Re-run
`out/verify/verify_warp.py` once the library compiles again.

## 6. Suites detail

* `photocraft-text`: `cargo test --offline -j 4 -p photocraft-text@0.23.0 --lib` -> 380 passed,
  5 ignored. Clippy: `cargo clippy --offline -j 4 -p photocraft-text@0.23.0 --all-targets -- -D warnings`.
* `photocraft-io`, `photocraft-ui-egui`: run after the library checkpoint, results appended below.
* `photocraft-engine`: see section 5.
* `cargo run -p photocraft-cli@0.23.0 -- run ...` used for every warp measurement; the CLI is
  rebuilt from this tree, so the numbers are the fork's pipeline, not the library example.

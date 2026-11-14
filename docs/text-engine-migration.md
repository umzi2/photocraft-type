# Text engine: the reverse-engineered crates are the only implementation

`crates/text` used to carry its own copy of the Photoshop text stack: a PSD reader/writer
(`crates/psd`, 16k lines), a Txt2/EngineData codec, a TySh serialiser and a whole PSD-type-layer
authoring/import implementation. Those copies drifted from the reverse-engineered engine they were
forked from, and the drift is what produced the user-visible PSD bugs (a `Txt2` style run without
the `/5 1` character-run marker, optical kerning rendered as metrics because Photoshop's own
spacing deltas were discarded, …). There is only one source of truth for text behaviour:

| Purpose | Upstream (authoritative) | Used by the fork as |
|---|---|---|
| PSD reader/writer, `TySh`, `Txt2` accessors, text-layer rasters, XMP | `/media/H/zed2/photocraft/photocraft-psd` | `photocraft-psd` workspace dependency |
| Fonts, shaping, layout, area text, warp, optical kerning, `Txt2`/EngineData codecs, PSD text authoring, PSD text rasterisation | `/media/H/zed2/photocraft/photocraft-text` | `photocraft-text-re` dependency |

Do not add text logic to the fork that also exists upstream; extend the upstream crate and call it.
The upstream crates are not workspace members — they are path dependencies, so they are built from
their own sources and keep their own tests.

## What already moved

- **`crates/psd` deleted** (94 files). `Cargo.toml` points `photocraft-psd` at
  `/media/H/zed2/photocraft/photocraft-psd`. The two shared test fixtures the engine tests used
  (`csh_fixture.rs`, `fixtures/atn/v12-commands.atn`) moved to `crates/testkit/fixtures/`, so the
  fork keeps no PSD implementation of its own.
- **`TySh` serialisation** (`crates/text/src/psd.rs`): `parse_tysh`, `translate_tysh`, `write_tysh`
  and `tysh_text` now wrap `photocraft_psd::TySh` (the block layout lives there: version, transform,
  text/warp descriptors, trailing point bounds and padding). The fork keeps its `TySh` struct only as
  a view (an `Affine` transform plus `photocraft_psd::descriptor::Descriptor`s) for the document
  model.
- `Txt2` composed-layout-cache invalidation already came from the engine (`txt2::
  invalidate_changed_composed_layouts`).

Verified: `cargo test -p photocraft-text@0.23.0` (380 tests), `photocraft-io` and
`photocraft-engine` suites, and the Photoshop recompose oracle on a re-exported document.

## What still lives in the fork

Ordered by how much divergence risk each carries.

1. **PSD text authoring** — `crates/text/src/psd/build.rs` (1.5k lines): builds `Txt2` objects
   (text, style/paragraph run tables, font table, frames/paths), the `TySh` `EngineData` mirror and
   the layer `TySh`. The engine's `photocraft_text::psd` module already has a structured authoring
   API (`LayerSpec`/`RunSpec`/`ParagraphSpec`, `author`, `with_engine_data`,
   `refresh_text_layer`) but only for the *first* text layer. Moving the fork's multi-layer writer
   there means generalising the `Txt2` writers from object index 0 to an arbitrary object and
   porting the "keep an already-agreeing Photoshop-authored object byte for byte" logic.
2. **PSD text import** — `crates/text/src/psd/import.rs` (900 lines), `psd/edit.rs`,
   `psd_path.rs`: reads `Txt2`/`TySh` into the document model. The engine has the `Txt2` token/path
   primitives and per-property getters; a reader that produces a `LayerSpec` belongs upstream, with
   the fork keeping only the `TextLayer` ⇄ `LayerSpec` mapping.
3. **`EngineData`/`Txt2` codec** — `crates/text/src/engine_data.rs` (456 lines). The engine has its
   own codec (`photocraft_text::engine_data`) and a `Txt2` tokenizer; the fork's copy exists because
   it is also used to parse `Txt2` blocks as a tree (raw, non-BOM strings included). Unifying means
   adding a `Txt2`-tree reader upstream and renaming the fork's value variants (`String`→`Str`,
   `Real`→`Float`) at ~60 call sites.
4. **Live text engine** — `crates/text/src/{layout,render,area,warp,cjk,fonts}.rs` (parley). The PSD
   export path already renders through the engine (`render_re.rs`); the interactive canvas still
   uses the app's own layout so the UI can shape text before a PSD render exists. Replacing it is
   the largest step and should come last.

## How to verify a migration step

```sh
cd /media/H/zed2/photocraft-fork/photocraft-type
cargo test --offline -j 2 -p photocraft-text@0.23.0     # 380
cargo test --offline -j 2 -p photocraft-io@0.23.0      # 686 + 594
cargo test --offline -j 2 -p photocraft-engine@0.23.0  # 3656
cargo clippy --offline -j 2 -p photocraft-text@0.23.0 --all-targets
```

For PSD output changes, re-export a document through `target/debug/photocraft-cli run … --cmd
type.edit …` and recompose it in Photoshop (`/media/H/zed2/photocraft/tools/psx_daemon.py`,
`out/psdcheck/psx_recompose_any.jsx`); stored-pixel bounds must agree with the recomposed bounds to
within a pixel, and a Photoshop-authored optical layer with its `Txt2` cache intact must reproduce
Photoshop's bounds exactly.

## Stage 1 verification (dependency swap + `TySh` delegation)

- `cargo test -p photocraft-text@0.23.0`: 380 passed.
- `cargo test -p photocraft-io@0.23.0`: 686 + 594 passed.
- `cargo test -p photocraft-engine@0.23.0`: green (see the run in the migration report).
- Photoshop recompose oracle on `/media/H/1/Untitled-6.psd` re-exported through the migrated build
  (`out/psdcheck/user6_after_migration.psd`): stored bounds `[751 136 999 360]`, `[406 393 654 617]`,
  `[401 136 659 360]`; Photoshop recomposes to `[752 136 998 360]`, `[407 393 653 617]`,
  `[401 136 659 360]` — identical to the pre-migration build (the 1 px difference on the two optical
  lines is the known optical-model residual, not a regression).
- Photoshop-authored optical layer with its `Txt2` cache intact
  (`out/psdcheck/ps_opt_cache_after_migration.psd`): our render `[51 167 436 211]`, Photoshop's
  recompose `[51 167 436 211]` — exact, as before.

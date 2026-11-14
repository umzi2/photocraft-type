# Photoshop glyph atlases (CoolType Crisp bitmaps)

These are Photoshop's own hinted glyph bitmaps for the showcase document
`out/psd_showcase/10_all_features.psd`, captured with Photoshop 20.0.2
(CC 2019, Windows build under Wine/Bottles) at 72 dpi, RGB, 8 bit, transparent
document, one point-text layer, antialias method **Crisp**.

## Why

CoolType's Crisp antialiasing is neither FreeType's hinting nor plain grid
fitting: it hints stems and rasters at a **quarter-pixel pen grid**. A glyph's
bitmap therefore depends on `fractional pen x` (phase), and the only way to
reproduce it is to reuse Photoshop's captured pixels. Measured on the showcase:
layer 04 goes from alpha-IoU 0.8793 (analytic outline) to 0.9962 with these
atlases; layers 03/06/09/10/14/16/17 become pixel-identical (alpha>8 IoU 1.0,
0 differing pixels > 8/255).

## Layout

One directory per capture, named `<face>_<size>[_faux][_h<scale>]_p<phase>`
(plus the gid-keyed sets `nsr26_onum_p*` for `onum` figures and
`nsr26_smallcaps_p*` for synthetic small caps, whose cells are selected by
`gid=` rather than by codepoint):

```text
assets/atlas/<capture>/atlas.txt   header + glyph table (UTF-8)
assets/atlas/<capture>/atlas.bin   concatenated 8-bit alpha cells
```

`atlas.txt` header (one key per line, order as written):

```text
font <FontSet name, e.g. NotoSans-Regular>
size <points>
aa <0 none | 1 crisp | 2 strong | 3 smooth | 4 sharp>
bold <0|1>          # Photoshop faux bold
italic <0|1>        # Photoshop faux italic
hscale <1.0|1.5>    # horizontal scale factor
pen_x <40>          # glyph pen x used during capture
baseline_y <120>    # baseline row used during capture
phase <0|0.25|0.5|0.75>
small_caps <0|1>    # captured from Photoshop's TextCase.SMALLCAPS (optional;
                    # matches synthetic small caps at the run's nominal size,
                    # because Photoshop hints the scaled capital for that size)
bin atlas.bin
```

Glyph rows: `glyph <hex codepoint> <dx> <dy> <w> <h> <offset> [<char>]`, where
`dx = ink_left - floor(pen_x)` and `dy = ink_top - baseline_y` inside the
capture document. A cell with `w = h = 0` means "no ink" and is skipped.

Load with `photocraft_text::atlas::Atlas::load(dir)` and attach every phase with
`FaceStore::set_atlases(Vec<Atlas>)` (one entry per phase). A single phase-less
atlas keeps the legacy integer placement.

## Faces covered

| capture | face | size (pt) | phases | showcase layers |
|---|---|---|---|---|
| nsr18 | NotoSans-Regular | 18 | 0/.25/.5/.75 | 05 area indents, 18 center (hyphenation too) |
| nsr20 | NotoSans-Regular | 20 | 0/.25/.5/.75 | 06 justify all, 16 ligatures |
| nsr22 | NotoSans-Regular | 22 | 0/.25/.5/.75 | 04 area justify, 14 hyphenation |
| nsr24 | NotoSans-Regular | 24 | 0/.25/.5/.75 | 07 cyrillic (all-caps Cyrillic), 11 vertical |
| nsr26 | NotoSans-Regular | 26 | 0/.25/.5/.75 | 02 strike box, 15 small caps (26 pt run; small caps themselves stay analytic) |
| h150  | NotoSans-Regular | 22 @ hscale 1.5 | 0/.25/.5/.75 | 03 tracking 300 + 150 % horizontal scale |
| bold30 | NotoSans-Bold | 30 | 0/.25/.5/.75 | 12 warp arc, 13 warp arch |
| bold30_faux | NotoSans-Bold | 30 faux bold | 0/.25/.5/.75 | 01 all caps + underline + faux bold |
| grim32 | CCGrimNGritty-Regular | 32 | 0/.25/.5/.75 | 08 CFF face |
| meanwhile32 | CCMeanwhile | 32 | 0/.25/.5/.75 | 09 CFF face, point text |
| heiti22 | AdobeHeitiStd-Regular | 22 | 0/.25/.5/.75 | 10 CJK |

Missing glyph, missing size/face/phase, or a glyph the atlas never saw (e.g.
`onum` old-style figures, `fi`/`ffl` ligatures) falls back to the analytic
outline raster; without any atlas file nothing changes.

TrueType Crisp atlases are applied per quarter phase at `floor(pen_x) + dx`.
Fonts whose bitmaps are identical across phases (CFF faces here: CCGrimNGritty,
CCMeanwhile) are detected per glyph at render time and placed at
`round(pen_x) + dx`, which is what Photoshop does for them.

## Reproducing

From the repo root (Photoshop 20.0.2 under Wine must be reachable through
`tools/psx.py`):

```bash
.venv/bin/python tools/capture_showcase_phases.py nsr18 nsr20 nsr22 nsr24 \
    nsr26 bold30 bold30_faux h150 grim32 meanwhile32 heiti22
```

`tools/capture_showcase_phases.py` captures all four phases per size: one
Photoshop launch per phase, glyphs laid out one per line with an explicit
`leading` (see `tools/js/psx_render.jsx`), then lifts each glyph cell from the
saved PSD. `tools/capture_atlas_batch.py` is the single-capture driver.
After capture, copy `atlas.txt` + `atlas.bin` here (the capture work directories
with the intermediate PSDs are not needed at runtime).

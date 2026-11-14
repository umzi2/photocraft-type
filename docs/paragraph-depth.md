# Paragraph depth: the Photoshop Paragraph panel beyond alignment

PhotoCraft's `ParagraphStyle` carried alignment, indents, paragraph spacing, auto leading,
direction and the hyphenate flag. Photoshop's Paragraph panel and its flyout menu hold more, and
its composer reads most of it from the document's `Txt2` paragraph sheet, not from `EngineData`.
This change adds the missing settings to the model, the engine commands, the panels and both
PSD representations, and fixes the writer that left the sheet out.

## What is stored where

Keys and slots were measured in the reverse-engineering workspace (`docs/txt2-map.md` and
`docs/cooltype-re.md` there); the table lists only what this port writes and reads.

| setting | `ParagraphStyle` field | `EngineData` paragraph key | `Txt2` `/5` slot |
|---|---|---|---|
| Justification | `align` | `Justification` 0…6 | `0` |
| First-line / left / right indent | `first_line_indent_pt` / `start_indent_pt` / `end_indent_pt` | `FirstLineIndent` / `StartIndent` / `EndIndent` | `1` / `2` / `3` |
| Space before / after | `space_before_pt` / `space_after_pt` | `SpaceBefore` / `SpaceAfter` | `4` / `5` |
| Auto leading | `auto_leading` | `AutoLeading` | `7` |
| Hyphenate | `hyphenate` | `AutoHyphenate` | `9` |
| Hyphenation: words longer than | `hyphenate_word_size` | `HyphenatedWordSize` | `10` |
| Hyphenation: after first / before last | `hyphenate_min_before` / `hyphenate_min_after` | `PreHyphen` / `PostHyphen` | `11` / `12` |
| Hyphen limit | `hyphen_limit` | `ConsecutiveHyphens` | `13` |
| Hyphenation zone (pt) | `hyphenation_zone_pt` | `Zone` | `14` |
| Hyphenate capitalized words | `hyphenate_capitalized` | — (no key) | `15` |
| Word / letter / glyph spacing limits | `justification_word` / `justification_letter` / `justification_glyph` | `WordSpacing` / `LetterSpacing` / `GlyphSpacing` | `17` / `18` / `19` |
| Hanging punctuation | `hanging_punctuation` | `Hanging` | `21` |
| Every-line composer | `every_line_composer` | `EveryLineComposer` | `29` |
| Burasagari | `burasagari` | `Burasagari` | — (unmeasured) |
| Kinsoku Shori order | `kinsoku_order` | `KinsokuOrder` | — (unmeasured) |

Numbers follow Photoshop's engine values. Two quirks are worth knowing: scripting's
`textItem.hyphenateWordsLongerThan` reports the stored `/10` value minus one (stored 9 reads 8),
and the shared default sheet stores `/13 0` while `EngineData` defaults `ConsecutiveHyphens` to 8.
`hyphenate_capitalized` exists only in `Txt2`, so a PhotoCraft-native PSD that has no `Txt2` block
keeps Photoshop's default (on) for it.

The `Txt2` paragraph run is nested like Photoshop's own:
`<< /0 << /0 << /0 () /5 <<sheet>> /6 1 >> >> /1 utf16-length >>`, one run per text paragraph
(terminal `\r` included). The writer reads this writer's older flat form too and rewrites it
nested, so old PhotoCraft files are repaired on the next save. A fresh sheet carries Photoshop's
constant keys `/24 /25 /33 /35 /40`; a file's own values for them (and for `/8`) are kept.

## Photoshop oracle (2026-11-12, Photoshop 20.0.2 under Wine, `Z:` paths)

Tooling: `photocraft-cli run` generates and edits PSDs headlessly;
`tools/js/psx_read_paragraph.jsx` opens a file in Photoshop and reports the layer's Paragraph
values; `tools/js/psx_resave.jsx` saves it back through Photoshop.

1. **Native PSD** (new document, two paragraphs with every field set): Photoshop opens it as
   `PARAGRAPHTEXT`, and reports `LEFTJUSTIFIED`, first-line 12 pt, left/right 6 pt, space
   before/after 6/8 pt, `ADOBEEVERYLINE`, hanging punctuation on, hyphenation 8/3/4 with zone
   12 pt, glyph scaling 95/100/105. Every value came from `EngineData`: a native document has
   no `Txt2` and Photoshop still honours the paragraph sheet properties.
2. **Edited file with `Txt2`** (`10_all_features.psd` → `existing_depth.psd`): Photoshop reports
   `FULLYJUSTIFIED`, the four indents/spaces, `ADOBEEVERYLINE`, hanging on, hyphenation
   9/3/4/zone 15 (scripting shows 8 for the stored 9), glyph scaling 95/100/105.
3. **`Txt2` is authoritative**: setting only the `Txt2` slots of that layer to left alignment,
   no hanging and single-line composer (leaving `EngineData` at the old values) made Photoshop
   report `LEFT`, no hanging and `ADOBESINGLELINE` — the sheet, not `EngineData`, decides.
4. **Round trip**: Photoshop's own save of the edited file keeps every `/5` value identical
   (including `/10 9`, `/13 2`, `/15 false`, and the three spacing arrays), and importing that
   file with `type.info` gives the same `ParagraphStyle`.
5. **A file Photoshop itself wrote** (`ps_defaults.psd`, a fresh layer made through Photoshop's
   DOM): editing its paragraph style with `type.setStyle` and saving kept the block's `/nil`
   name values and every other key; Photoshop opened the result and reported the new
   `FULLYJUSTIFIED`, every-line composer, hanging punctuation, hyphenation 8/3/4 and 20 pt zone.
6. **Native `Txt2` block caveat**: a block built from nothing is refused by this Photoshop at
   open; a block that came from a real file, with a new object added, opens. The exporter
   therefore writes `Txt2` only for documents that already had one (and repairs its paragraph
   sheets), and a native document's paragraph settings reach Photoshop through `EngineData`, as
   the probe above shows. The cost is that Optical kerning and the OpenType settings only `Txt2`
   can carry are lost on reopening a new document until a valid native block can be built.

## Layout

`layout/lines.rs` now applies the measured Roman hanging-punctuation table and the CJK set when
`Burasagari` is on: the glyphs that start a line move left by the part of their advance that
hangs, the rest of the line keeps the fitted margin, and a line made only of hanging glyphs keeps
its normal start. The table and its provenance (`docs/hanging-punctuation.md` in the
reverse-engineering workspace) are in the module.

The every-line composer, hyphenation with dictionaries and the justification spacing limits are
stored, written and read, and Photoshop uses them for the saved file; the on-canvas composer is
still parley's single-line breaker and does not hyphenate (#6710). The Paragraph panel says so in
its tooltips rather than pretending otherwise.

## Tests

- `crates/text/src/tests.rs`: EngineData round trip of every field; `Txt2` paragraph sheet
  structure, UTF-16 lengths, values, and the `Txt2`-only `/15`; byte-for-byte stability of an
  unchanged block; hanging-punctuation layout (prefix moves, content and line end do not).
- `crates/engine/src/cmd/type_cmds/tests.rs`: the `type.setStyle` keys, clamping/order of the
  spacing triples, and a range edit landing in the right paragraph run.
- `crates/ui-egui/src/panels/type_panels_ui.rs`: the Paragraph Style Options editor lists the new
  attributes (replacing the #6710 test that asserted Hyphenate was hidden).

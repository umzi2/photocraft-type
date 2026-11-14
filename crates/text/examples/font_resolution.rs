//! The font-resolution diagnostic (task-18): for each requested Photoshop PostScript name print
//! the file it resolves to, its face index and SHA-256, the parsed metrics Photoshop's EngineData
//! numbers depend on, and whether the style has to be synthesised.
//!
//! ```sh
//! cargo run -p photocraft-text --example font_resolution -- TimesNewRomanPSMT MyriadPro-Regular
//! cargo run -p photocraft-text --example font_resolution -- --dirs
//! ```
//!
//! The same data is logged while a document is composed with `PHOTOCRAFT_FONT_TRACE=1`.

use std::collections::HashMap;

use photocraft_text::fonts::{PsFontIndex, count_font_files, photoshop_font_dirs};

/// The 24 families the sync matrix and the PSD corpus use, plus the Photoshop faces whose
/// first-baseline ratio was measured on the oracle.
const DEFAULT_NAMES: &[&str] = &[
    "TimesNewRomanPSMT",
    "TimesNewRomanPS-BoldMT",
    "ArialMT",
    "Arial-BoldMT",
    "Arial-Black",
    "Georgia",
    "Georgia-Bold",
    "Verdana",
    "TrebuchetMS",
    "Impact",
    "ComicSansMS",
    "CourierNewPSMT",
    "AndaleMono",
    "MyriadPro-Regular",
    "MinionPro-Regular",
    "AdobeHeitiStd-Regular",
    "CCMeanwhile",
    "CCMeanwhile-Bold",
    "CCGrimNGritty-Regular",
    "CCGrimNGritty-Italic",
    "CCSpellcaster",
    "CCSpellcaster-Bold",
    "NotoSans-Regular",
    "NotoSans-Bold",
    "NotoSerif-Regular",
    "NotoSansCJKjp-Regular",
    "Inter-Regular",
];

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--dirs") {
        print_dirs();
        return;
    }
    if args.is_empty() {
        args = DEFAULT_NAMES.iter().map(|s| s.to_string()).collect();
    }
    let index = PsFontIndex::scan_all();
    println!(
        "indexed {} faces from {} sources ({} unreadable)",
        index.len(),
        index.sources().len(),
        index.unreadable()
    );
    println!();
    println!(
        "requested\tfile\tface\tsha256\tfamily\tstyle\tsource\tsynthetic\tupm\thhea_asc\thhea_desc\thhea_gap\ttypo_asc\ttypo_desc\ttypo_gap\twin_asc\twin_desc\tcap\th_ink\td_ink\tratio_ms"
    );
    for name in &args {
        let row = index.row(name);
        let Some(file) = &row.file else {
            println!("{name}\tNOT FOUND\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t");
            continue;
        };
        let metrics = Metrics::read(&file.path, file.face_index);
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            name,
            file.path.display(),
            file.face_index,
            &file.sha256[..16],
            file.family,
            file.style,
            file.source.label(),
            row.synthetic,
            metrics.map_or("-".into(), |m| m.upm.to_string()),
            metrics.map_or("-".into(), |m| m.hhea_asc.to_string()),
            metrics.map_or("-".into(), |m| m.hhea_desc.to_string()),
            metrics.map_or("-".into(), |m| m.hhea_gap.to_string()),
            metrics.map_or("-".into(), |m| m.typo_asc.to_string()),
            metrics.map_or("-".into(), |m| m.typo_desc.to_string()),
            metrics.map_or("-".into(), |m| m.typo_gap.to_string()),
            metrics.map_or("-".into(), |m| m.win_asc.to_string()),
            metrics.map_or("-".into(), |m| m.win_desc.to_string()),
            metrics.map_or("-".into(), |m| m.cap.map_or("-".into(), |v| v.to_string())),
            metrics.map_or("-".into(), |m| m.h_ink.map_or("-".into(), |v| v.to_string())),
            metrics.map_or("-".into(), |m| m.d_ink.map_or("-".into(), |v| v.to_string())),
            metrics.map_or("-".into(), |m| m.ratio.map_or("-".into(), |v| format!("{v:.4}"))),
        );
    }
}

fn print_dirs() {
    println!("Photoshop font sources, in resolution order:\n");
    let mut seen = std::collections::HashSet::new();
    for (dir, source) in photoshop_font_dirs() {
        if !seen.insert(dir.clone()) {
            continue;
        }
        println!("{:<18} {}  ({} font files)", source.label(), dir.display(), count_font_files(&dir));
    }
    let index = PsFontIndex::scan_all();
    let mut per_source: HashMap<&'static str, usize> = HashMap::new();
    for entry in index.entries() {
        *per_source.entry(entry.source.label()).or_default() += 1;
    }
    let mut rows: Vec<_> = per_source.into_iter().collect();
    rows.sort();
    println!("\nfaces per source:");
    for (source, count) in rows {
        println!("  {source:<18} {count}");
    }
    println!("\nsources scanned: {}", index.sources().len());
}

/// The metric set the task lists, plus the ink extents that reproduce Photoshop's first-baseline
/// ratio (min of `H` cap and `d` ascender ink over `upm`).
#[derive(Clone, Copy, Debug)]
struct Metrics {
    upm: u16,
    hhea_asc: i16,
    hhea_desc: i16,
    hhea_gap: i16,
    typo_asc: i16,
    typo_desc: i16,
    typo_gap: i16,
    win_asc: u16,
    win_desc: u16,
    cap: Option<i16>,
    h_ink: Option<f32>,
    d_ink: Option<f32>,
    ratio: Option<f32>,
}

impl Metrics {
    fn read(path: &std::path::Path, face_index: u32) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        let tables = table_directory(&bytes, face_index)?;
        let head = *tables.get(b"head")?;
        let hhea = *tables.get(b"hhea")?;
        let os2 = tables.get(b"OS/2").copied();
        let upm = u16::from_be_bytes(slice::<2>(&bytes, head.0, 18)?);
        let hhea_asc = i16::from_be_bytes(slice::<2>(&bytes, hhea.0, 4)?);
        let hhea_desc = i16::from_be_bytes(slice::<2>(&bytes, hhea.0, 6)?);
        let hhea_gap = i16::from_be_bytes(slice::<2>(&bytes, hhea.0, 8)?);
        let (typo_asc, typo_desc, typo_gap, win_asc, win_desc, cap) = match os2 {
            Some((at, _)) => {
                let version = u16::from_be_bytes(slice::<2>(&bytes, at, 0)?);
                let cap = (version >= 2).then(|| i16::from_be_bytes(slice::<2>(&bytes, at, 88).unwrap_or([0, 0])));
                (
                    i16::from_be_bytes(slice::<2>(&bytes, at, 68).unwrap_or([0, 0])),
                    i16::from_be_bytes(slice::<2>(&bytes, at, 70).unwrap_or([0, 0])),
                    i16::from_be_bytes(slice::<2>(&bytes, at, 72).unwrap_or([0, 0])),
                    u16::from_be_bytes(slice::<2>(&bytes, at, 74).unwrap_or([0, 0])),
                    u16::from_be_bytes(slice::<2>(&bytes, at, 76).unwrap_or([0, 0])),
                    cap,
                )
            }
            None => (0, 0, 0, 0, 0, None),
        };
        let (h_ink, d_ink) = ink_extents(&bytes, face_index)?;
        let ratio = match (h_ink, d_ink) {
            (Some(h), Some(d)) => Some(h.min(d) / f32::from(upm)),
            _ => None,
        };
        Some(Self { upm, hhea_asc, hhea_desc, hhea_gap, typo_asc, typo_desc, typo_gap, win_asc, win_desc, cap, h_ink, d_ink, ratio })
    }
}

/// `H`/`d` ink top in font units, read through skrifa (glyf and CFF outlines both).
fn ink_extents(bytes: &[u8], face_index: u32) -> Option<(Option<f32>, Option<f32>)> {
    use skrifa::MetadataProvider;
    use skrifa::raw::FileRef;
    let file = FileRef::new(bytes).ok()?;
    let font = match file {
        FileRef::Font(font) => font,
        FileRef::Collection(collection) => collection.get(face_index).ok()?,
    };
    let metrics = font.glyph_metrics(skrifa::instance::Size::unscaled(), skrifa::instance::LocationRef::default());
    let ink = |ch: char| font.charmap().map(ch).and_then(|gid| metrics.bounds(gid)).map(|b| b.y_max);
    Some((ink('H'), ink('d')))
}

/// The per-file table directory of one face: tag → (offset, length).
fn table_directory(bytes: &[u8], face_index: u32) -> Option<HashMap<[u8; 4], (usize, usize)>> {
    if bytes.get(0..4)? == b"ttcf" {
        let count = u32::from_be_bytes(slice::<4>(bytes, 8, 0)?);
        let offset = usize::try_from(face_index).ok()?;
        if offset >= count as usize {
            return None;
        }
        let face = u32::from_be_bytes(slice::<4>(bytes, 12, offset * 4)?);
        return table_directory_at(bytes, usize::try_from(face).ok()?);
    }
    table_directory_at(bytes, 0)
}

fn table_directory_at(bytes: &[u8], base: usize) -> Option<HashMap<[u8; 4], (usize, usize)>> {
    let count = u16::from_be_bytes(slice::<2>(bytes, base, 4)?);
    let mut tables = HashMap::new();
    for i in 0..count {
        let record = base + 12 + usize::from(i) * 16;
        let tag: [u8; 4] = bytes.get(record..record + 4)?.try_into().ok()?;
        let offset = u32::from_be_bytes(slice::<4>(bytes, record, 8)?);
        let length = u32::from_be_bytes(slice::<4>(bytes, record, 12)?);
        tables.insert(tag, (offset as usize, length as usize));
    }
    Some(tables)
}

fn slice<const N: usize>(bytes: &[u8], base: usize, offset: usize) -> Option<[u8; N]> {
    let start = base.checked_add(offset)?;
    bytes.get(start..start + N)?.try_into().ok()
}

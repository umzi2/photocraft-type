//! Photoshop's font sources, and the name → file resolution Photoshop performs.
//!
//! Photoshop composes a PSD's text with faces from its own font list: the fonts installed in the
//! Wine prefix it runs in (`C:\windows\Fonts`), the Adobe application's faces
//! (`Required/PDFL/Resource/Fonts`), the shared Adobe fonts folder and the host fonts Wine exports
//! to it through Fontconfig. fontique only scans the host folders, so a document naming
//! `TimesNewRomanPSMT` resolved through the family *guess* ("Times New Roman") and landed on
//! whatever the host aliased for that family — on this machine Liberation Serif — instead of the
//! `times.ttf` Photoshop composes with.
//!
//! [`PsFontIndex`] lists those files with the names their own `name` tables give, in Photoshop's
//! order, so a lookup by PostScript name stays exact and a name whose style has no file stays in
//! its family (Photoshop synthesises the style: `MyriadPro-BoldIt` is drawn from the single
//! `MyriadPro-Regular.otf`) instead of jumping to another family.
//!
//! Set `PHOTOCRAFT_FONT_TRACE=1` to log every lookup: requested name, resolved file, face index
//! and SHA-256 of the file that is drawn (see [`trace_enabled`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use skrifa::MetadataProvider;
use skrifa::attribute::Style as SkStyle;
use skrifa::raw::FileRef;
use skrifa::string::StringId;

use super::{ResolvedFont, collect_font_files, guess_from_postscript, system_font_dirs};

/// Extra Photoshop font folders (a path list, `:` on Unix): prepended to the default order. The
/// app host uses it for a prefix outside `HOME`, tests for fixtures.
pub const FONT_DIRS_ENV: &str = "PHOTOCRAFT_PS_FONT_DIRS";
/// The installed Photoshop application directory (the one holding `Required/`). Defaults to every
/// `Program Files/Adobe/Adobe Photoshop*` folder of the prefix, newest name first with CC 2019
/// preferred: that is the release the PSD oracle runs (`PSX_INSTALL` in `tools/psx_daemon.py`).
pub const PS_INSTALL_ENV: &str = "PHOTOCRAFT_PS_INSTALL";
/// The Wine prefix Photoshop runs in. Defaults to `$WINEPREFIX`, then
/// `$HOME/.local/share/photocraft/wine` (the prefix the daemon and the app share).
pub const WINE_PREFIX_ENV: &str = "PHOTOCRAFT_WINE_PREFIX";
/// PhotoCraft's checkouts that carry a Photoshop distribution copy (`<root>/app/*/Application`).
pub const PS_DIST_ENV: &str = "PHOTOCRAFT_PS_DIST";
/// Set to `1` to log each font lookup (requested name → file, face index, hash).
pub const FONT_TRACE_ENV: &str = "PHOTOCRAFT_FONT_TRACE";

/// Where a font file comes from, in Photoshop's own order of precedence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PsFontSource {
    /// `PHOTOCRAFT_PS_FONT_DIRS`.
    EnvDirs,
    /// `Common Files/Adobe/Fonts` (fonts shared by the installed Adobe applications).
    AdobeShared,
    /// The Photoshop application's own faces (`Required/PDFL/Resource/Fonts`, `Required/Fonts`,
    /// `Resources/Fonts`, `Fonts`). Photoshop ships Myriad Pro, Minion Pro and the CJK faces here.
    AdobeApplication,
    /// `C:\windows\Fonts` of the Wine prefix (the fonts setup installed, plus Photoshop's own).
    WindowsFonts,
    /// A Photoshop distribution checkout (`dist*/app/*/Application/...`), byte-identical copies
    /// of the same Adobe faces; used when the prefix is not present (tests, packaging).
    Distribution,
    /// PhotoCraft's own bundled fonts (`assets/fonts`).
    AppFonts,
    /// The host's font folders (`Fontconfig`, `/usr/share/fonts`, `~/.local/share/fonts`); Wine
    /// exposes these to Photoshop, so a face living only there is a Photoshop face too.
    HostFonts,
}

impl PsFontSource {
    /// A short stable name, for the trace log and the report table.
    pub fn label(self) -> &'static str {
        match self {
            Self::EnvDirs => "env-dirs",
            Self::AdobeShared => "adobe-shared",
            Self::AdobeApplication => "adobe-app",
            Self::WindowsFonts => "windows-fonts",
            Self::Distribution => "photoshop-dist",
            Self::AppFonts => "app-bundled",
            Self::HostFonts => "host-fonts",
        }
    }

    /// Whether the font database has to register this source's files itself: Fontconfig already
    /// hands the host faces to fontique, the bundled ones are registered by the database's
    /// constructor, and every other source is invisible to it.
    fn needs_registration(self) -> bool {
        !matches!(self, Self::HostFonts | Self::AppFonts)
    }
}

/// One face of one font file: what a Photoshop font name resolves to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PsFontFile {
    pub path: PathBuf,
    /// Face index inside the file (non-zero for TTC/OTC collections).
    pub face_index: u32,
    /// The `name` table's PostScript name (name ID 6): what PSDs store.
    pub postscript_name: String,
    /// The menu family: typographic family name (ID 16) when present, else family name (ID 1).
    pub family: String,
    /// Typographic subfamily (ID 17) when present, else subfamily (ID 2).
    pub style: String,
    /// `OS/2.usWeightClass`, or the `head`/style fallback skrifa computes.
    pub weight: u16,
    pub italic: bool,
    /// SHA-256 of the file, lowercase hex (identity of the exact physical face).
    pub sha256: String,
    pub source: PsFontSource,
}

impl PsFontFile {
    /// One line for the trace log and the report: `path#face`.
    pub fn location(&self) -> String {
        if self.face_index == 0 {
            format!("{}", self.path.display())
        } else {
            format!("{}#{}", self.path.display(), self.face_index)
        }
    }
}

/// How a requested name resolves: a file, plus whether the file carries the requested style.
#[derive(Clone, Debug)]
pub struct PsResolution {
    pub file: PsFontFile,
    /// The weight the name asks for (Photoshop synthesises it when the file is not bold).
    pub weight: u16,
    pub italic: bool,
    /// True when the file itself is not the requested style: the renderer has to synthesise it,
    /// as Photoshop does for `MyriadPro-BoldIt` (only `MyriadPro-Regular.otf` ships).
    pub synthetic: bool,
}

/// One line of the diagnostic table: what a requested PostScript name resolves to, and whether it
/// is the requested style, a synthesised style of the same family, or a substitution.
#[derive(Clone, Debug)]
pub struct ResolutionRow {
    pub requested: String,
    pub file: Option<PsFontFile>,
    pub weight: u16,
    pub italic: bool,
    /// The file is the family's face but not the requested style (Photoshop synthesises it).
    pub synthetic: bool,
}

impl ResolutionRow {
    /// True when the name resolved through a family Photoshop does not have (a substitution the
    /// parity work has to eliminate).
    pub fn substituted(&self) -> bool {
        self.file.is_none()
    }
}

/// Index of the faces Photoshop can compose with, keyed by the PostScript name PSDs store.
pub struct PsFontIndex {
    entries: Vec<PsFontFile>,
    by_name: HashMap<String, usize>,
    by_lower: HashMap<String, usize>,
    by_family: HashMap<String, Vec<usize>>,
    sources: Vec<PathBuf>,
    /// Files that could not be read or parsed; diagnostics only.
    unreadable: usize,
}

impl PsFontIndex {
    /// The faces of Photoshop's own sources (prefix, Adobe application, distributions, app
    /// bundle). Host faces are left to fontique, which already loads them.
    pub fn scan() -> Self {
        Self::from_dirs(&font_dirs())
    }

    /// Every face Photoshop can see, host folders included: what the diagnostic reports.
    pub fn scan_all() -> Self {
        let mut dirs = font_dirs();
        dirs.extend(host_font_dirs());
        Self::from_dirs(&dirs)
    }

    /// Builds an index from explicit `(directory, source)` pairs, in the given order. A file
    /// listed twice keeps its first source; a PostScript name present twice keeps its first
    /// entry (the earlier source wins, as in Photoshop's precedence).
    pub fn from_dirs(dirs: &[(PathBuf, PsFontSource)]) -> Self {
        let mut index = Self { entries: Vec::new(), by_name: HashMap::new(), by_lower: HashMap::new(), by_family: HashMap::new(), sources: Vec::new(), unreadable: 0 };
        let mut seen_dirs = std::collections::HashSet::new();
        let mut seen_files = std::collections::HashSet::new();
        for (dir, source) in dirs {
            let key = dir.canonicalize().unwrap_or_else(|_| dir.clone());
            if !seen_dirs.insert(key) || !dir.is_dir() {
                continue;
            }
            index.sources.push(dir.clone());
            let mut files = Vec::new();
            collect_font_files(dir, 0, &mut files);
            files.sort();
            for path in files {
                let key = path.canonicalize().unwrap_or_else(|_| path.clone());
                if !seen_files.insert(key) {
                    continue;
                }
                match faces_of(&path, *source) {
                    Some(faces) => index.entries.extend(faces),
                    None => index.unreadable += 1,
                }
            }
        }
        for (i, entry) in index.entries.iter().enumerate() {
            index.by_name.entry(entry.postscript_name.clone()).or_insert(i);
            index.by_lower.entry(entry.postscript_name.to_ascii_lowercase()).or_insert(i);
            index.by_family.entry(family_key(&entry.family)).or_default().push(i);
        }
        index
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[PsFontFile] {
        &self.entries
    }

    /// The directories that were scanned, in order.
    pub fn sources(&self) -> &[PathBuf] {
        &self.sources
    }

    /// Files that could not be parsed (counted, never fatal).
    pub fn unreadable(&self) -> usize {
        self.unreadable
    }

    /// The exact face PSDs name: PostScript name, case-insensitively as a second chance.
    pub fn resolve(&self, postscript_name: &str) -> Option<&PsFontFile> {
        self.by_name
            .get(postscript_name)
            .or_else(|| self.by_lower.get(&postscript_name.to_ascii_lowercase()))
            .and_then(|&i| self.entries.get(i))
    }

    /// The resolution used when composing: the exact face, else the face of the family the name
    /// implies (Photoshop's synthetic style), else nothing.
    pub fn resolve_style(&self, postscript_name: &str) -> Option<PsResolution> {
        if let Some(file) = self.resolve(postscript_name) {
            return Some(PsResolution { file: file.clone(), weight: file.weight, italic: file.italic, synthetic: false });
        }
        let guess: ResolvedFont = guess_from_postscript(postscript_name);
        let faces: Vec<&PsFontFile> = self.by_family.get(&family_key(&guess.family))?.iter().filter_map(|&i| self.entries.get(i)).collect();
        // The requested style itself when the family has it; else the family's upright regular,
        // the face Photoshop synthesises a missing bold or italic from (a SemiBold plus a
        // synthesised bold would be twice as heavy as Photoshop's `Inter-Bold`).
        let exact_style = faces.iter().copied().find(|f| f.weight == guess.weight && f.italic == guess.italic);
        let file = exact_style
            .or_else(|| faces.iter().copied().filter(|f| !f.italic).min_by_key(|f| f.weight.abs_diff(400)))
            .or_else(|| faces.iter().copied().min_by_key(|f| f.weight.abs_diff(guess.weight)))?;
        let synthetic = file.weight != guess.weight || file.italic != guess.italic;
        Some(PsResolution { file: file.clone(), weight: guess.weight, italic: guess.italic, synthetic })
    }

    /// The diagnostic row for one requested name: the exact face, the synthesised face of the
    /// family, or a substitution (`file: None`, the family it falls back to in `family`).
    pub fn row(&self, postscript_name: &str) -> ResolutionRow {
        if let Some(file) = self.resolve(postscript_name) {
            return ResolutionRow { requested: postscript_name.to_string(), file: Some(file.clone()), weight: file.weight, italic: file.italic, synthetic: false };
        }
        match self.resolve_style(postscript_name) {
            Some(r) => ResolutionRow { requested: postscript_name.to_string(), file: Some(r.file), weight: r.weight, italic: r.italic, synthetic: r.synthetic },
            None => {
                let guess: ResolvedFont = guess_from_postscript(postscript_name);
                ResolutionRow { requested: postscript_name.to_string(), file: None, weight: guess.weight, italic: guess.italic, synthetic: false }
            }
        }
    }

    /// Every face of a family, for the report.
    pub fn faces_of_family(&self, family: &str) -> Vec<&PsFontFile> {
        self.by_family.get(&family_key(family)).map(|v| v.iter().filter_map(|&i| self.entries.get(i)).collect()).unwrap_or_default()
    }

    /// The files the font database has to register (every source fontique cannot see).
    pub fn files_to_register(&self) -> Vec<PathBuf> {
        let mut seen = std::collections::HashSet::new();
        self.entries.iter().filter(|e| e.source.needs_registration()).map(|e| e.path.clone()).filter(|p| seen.insert(p.clone())).collect()
    }
}

/// `family` normalized to compare name spellings Photoshop and the `name` table use
/// ("Times New Roman", "TimesNewRoman", "Times New Roman PSMT" all agree).
pub(crate) fn family_key(family: &str) -> String {
    family.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
}

/// True when the trace log is enabled (`PHOTOCRAFT_FONT_TRACE` set to anything but `0`).
pub fn trace_enabled() -> bool {
    std::env::var_os(FONT_TRACE_ENV).is_some_and(|v| !v.is_empty() && v != "0")
}

/// The one-line trace record for a lookup. `resolved` is the index's answer, `fallback` the
/// family the caller will use when the index knows no such name (a substitution).
pub fn trace_line(requested: &str, resolved: Option<&PsResolution>, fallback: &ResolvedFont) -> String {
    match resolved {
        Some(r) => format!(
            "photocraft-text: font {requested:?} -> {} face={} sha256={} ({} / {}; source {}, synthetic={})",
            r.file.path.display(),
            r.file.face_index,
            r.file.sha256,
            r.file.family,
            r.file.style,
            r.file.source.label(),
            r.synthetic
        ),
        None => format!(
            "photocraft-text: font {requested:?} -> no Photoshop file; substituting family {:?} weight {} italic {} (exact={})",
            fallback.family, fallback.weight, fallback.italic, fallback.exact
        ),
    }
}

/// Number of font files a directory holds (recursively), for the diagnostic's source list.
pub fn count_font_files(dir: &Path) -> usize {
    let mut files = Vec::new();
    if dir.is_dir() {
        collect_font_files(dir, 0, &mut files);
    }
    files.len()
}

/// The Wine prefix Photoshop runs in, when it exists.
pub fn wine_prefix() -> Option<PathBuf> {
    let from_env = |name: &str| std::env::var_os(name).map(PathBuf::from).filter(|p| !p.as_os_str().is_empty());
    let prefix = from_env(WINE_PREFIX_ENV)
        .or_else(|| from_env("WINEPREFIX"))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/photocraft/wine")))?;
    prefix.is_dir().then_some(prefix)
}

/// The installed Photoshop application directories of `prefix`, newest first with CC 2019 first
/// (the release the PSD oracle runs).
pub fn photoshop_installs(prefix: &Path) -> Vec<PathBuf> {
    if let Some(dir) = std::env::var_os(PS_INSTALL_ENV).map(PathBuf::from).filter(|p| p.is_dir()) {
        return vec![dir];
    }
    let adobe = prefix.join("drive_c/Program Files/Adobe");
    let Ok(rd) = std::fs::read_dir(&adobe) else { return Vec::new() };
    let mut installs: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.contains("Photoshop")))
        .collect();
    installs.sort_by(|a, b| {
        let name = |p: &PathBuf| p.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        (!name(a).contains("CC 2019"), std::cmp::Reverse(name(a))).cmp(&(!name(b).contains("CC 2019"), std::cmp::Reverse(name(b))))
    });
    installs
}

/// The subfolders of an Adobe application that hold fonts it composes text with. `Resources/ui-fonts`
/// holds the *interface* faces (Adobe Clean, Source Sans Pro), not text faces, so it is skipped:
/// a document naming them is missing the font, as in Photoshop.
fn app_font_dirs(app: &Path) -> Vec<PathBuf> {
    ["Required/PDFL/Resource/Fonts", "Required/Fonts", "Resources/Fonts", "Fonts"].into_iter().map(|s| app.join(s)).collect()
}

/// Photoshop's own font folders, in Photoshop's precedence order.
pub fn font_dirs() -> Vec<(PathBuf, PsFontSource)> {
    let mut out = Vec::new();
    if let Some(list) = std::env::var_os(FONT_DIRS_ENV) {
        for dir in std::env::split_paths(&list).filter(|d| !d.as_os_str().is_empty()) {
            out.push((dir, PsFontSource::EnvDirs));
        }
    }
    if let Some(prefix) = wine_prefix() {
        out.push((prefix.join("drive_c/Program Files (x86)/Common Files/Adobe/Fonts"), PsFontSource::AdobeShared));
        for install in photoshop_installs(&prefix) {
            out.extend(app_font_dirs(&install).into_iter().map(|d| (d, PsFontSource::AdobeApplication)));
        }
        out.push((prefix.join("drive_c/windows/Fonts"), PsFontSource::WindowsFonts));
    }
    for root in photo_dist_roots() {
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        let mut apps = Vec::new();
        for entry in rd.flatten() {
            let app_root = entry.path();
            // `dist/app/1/Application` and `dist2019/app/1/Application`.
            let mut dirs: Vec<PathBuf> = vec![app_root.join("Application")];
            if let Ok(inner) = std::fs::read_dir(&app_root) {
                dirs.extend(inner.flatten().map(|e| e.path().join("Application")));
            }
            apps.extend(dirs.into_iter().filter(|d| d.is_dir()));
        }
        apps.sort();
        for app in apps {
            out.extend(app_font_dirs(&app).into_iter().map(|d| (d, PsFontSource::Distribution)));
        }
    }
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts");
    if assets.is_dir() {
        out.push((assets, PsFontSource::AppFonts));
    }
    out
}

/// Checkouts that carry a Photoshop distribution (`dist*/app/*/Application`), from
/// `PHOTOCRAFT_PS_DIST` or the repository's own checkout.
fn photo_dist_roots() -> Vec<PathBuf> {
    if let Some(list) = std::env::var_os(PS_DIST_ENV) {
        return std::env::split_paths(&list).filter(|d| d.is_dir()).collect();
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    ["dist", "dist2019"].into_iter().map(|d| root.join(d)).filter(|d| d.is_dir()).collect()
}

/// The host font folders Wine exports to Photoshop (and the fallback of every lookup the index
/// cannot answer). `PHOTOCRAFT_FONT_DIRS` is part of [`system_font_dirs`] and is kept there.
pub fn host_font_dirs() -> Vec<(PathBuf, PsFontSource)> {
    let ps_dirs: Vec<PathBuf> = font_dirs().into_iter().map(|(d, _)| d.canonicalize().unwrap_or(d)).collect();
    let mut seen = std::collections::HashSet::new();
    system_font_dirs()
        .into_iter()
        .filter(|d| d.is_dir())
        .filter(|d| !ps_dirs.contains(&d.canonicalize().unwrap_or_else(|_| d.clone())))
        .filter(|d| seen.insert(d.canonicalize().unwrap_or_else(|_| d.clone())))
        .map(|d| (d, PsFontSource::HostFonts))
        .collect()
}

/// Every face a font file holds (`None` when the file cannot be read or parsed).
fn faces_of(path: &Path, source: PsFontSource) -> Option<Vec<PsFontFile>> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > 256 * 1024 * 1024 {
        return None;
    }
    let sha256 = sha256_hex(&bytes);
    let file = FileRef::new(&bytes).ok()?;
    let mut faces = Vec::new();
    match file {
        FileRef::Font(font) => faces.push(face_of(path, 0, &font, &sha256, source)),
        FileRef::Collection(collection) => {
            for index in 0..collection.len() {
                let Ok(font) = collection.get(index) else { continue };
                faces.push(face_of(path, index, &font, &sha256, source));
            }
        }
    }
    faces.retain(|f| !f.postscript_name.is_empty());
    (!faces.is_empty()).then_some(faces)
}

fn face_of(path: &Path, face_index: u32, font: &skrifa::FontRef<'_>, sha256: &str, source: PsFontSource) -> PsFontFile {
    let name = |id: StringId| font.localized_strings(id).english_or_first().map(|s| s.to_string()).filter(|s| !s.is_empty());
    let attributes = font.attributes();
    let weight = attributes.weight.value().round().clamp(1.0, 1000.0) as u16;
    let italic = matches!(attributes.style, SkStyle::Italic | SkStyle::Oblique(_));
    PsFontFile {
        path: path.to_path_buf(),
        face_index,
        postscript_name: name(StringId::POSTSCRIPT_NAME).unwrap_or_default(),
        family: name(StringId::TYPOGRAPHIC_FAMILY_NAME).or_else(|| name(StringId::FAMILY_NAME)).unwrap_or_default(),
        style: name(StringId::TYPOGRAPHIC_SUBFAMILY_NAME).or_else(|| name(StringId::SUBFAMILY_NAME)).unwrap_or_default(),
        weight,
        italic,
        sha256: sha256.to_string(),
        source,
    }
}

/// SHA-256 of `data`, lowercase hex. A local implementation keeps the text crate's dependency set
/// unchanged (the task's file hash is only an identity marker, but a collision-free one matters
/// when two installs of the same font name are compared).
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
        0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
        0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut message = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());
    let mut w = [0u32; 64];
    for block in message.as_chunks::<64>().0 {
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }
    h.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_standard_vectors() {
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    fn bundled() -> Vec<(PathBuf, PsFontSource)> {
        vec![(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts"), PsFontSource::AppFonts)]
    }

    #[test]
    fn indexes_every_face_of_the_given_directories() {
        let index = PsFontIndex::from_dirs(&bundled());
        assert!(index.len() >= 4, "bundled faces: {}", index.len());
        let inter = index.resolve("Inter-SemiBold").expect("Inter-SemiBold");
        assert_eq!((inter.family.as_str(), inter.weight, inter.italic), ("Inter", 600, false));
        assert!(inter.path.ends_with("Inter-SemiBold.ttf"));
        assert_eq!(inter.sha256.len(), 64);
        assert!(index.resolve("inter-regular").is_some(), "PostScript names compare case-insensitively");
        assert!(index.resolve("Missing-Face").is_none());
    }

    #[test]
    fn a_style_without_a_file_stays_in_its_family() {
        let index = PsFontIndex::from_dirs(&bundled());
        // Inter's bundled files are Regular/Medium/SemiBold: only the regular face is upright 400.
        let r = index.resolve_style("Inter-Bold").expect("the family is known");
        assert!(r.file.path.ends_with("Inter-Regular.ttf"), "resolved: {:?}", r.file.path);
        assert_eq!((r.file.weight, r.file.italic), (400, false));
        assert_eq!((r.weight, r.italic, r.synthetic), (700, false, true));
        // A name no family implies resolves to nothing: the caller substitutes deliberately.
        assert!(index.resolve_style("Unrelated-PS30").is_none());
    }

    #[test]
    fn earlier_sources_win_and_directories_are_listed_once() {
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts");
        let index = PsFontIndex::from_dirs(&[(assets.clone(), PsFontSource::EnvDirs), (assets, PsFontSource::HostFonts)]);
        let inter = index.resolve("Inter-Regular").expect("Inter-Regular");
        assert_eq!(inter.source, PsFontSource::EnvDirs);
        assert_eq!(index.sources().len(), 1, "a directory listed twice is scanned once");
    }

    /// The file Photoshop loads for a name cannot be ambiguous when two of its installs ship the
    /// face: every copy must be the same bytes (checked for the faces the corpus composes with).
    #[test]
    fn every_copy_of_a_photoshop_face_is_the_same_file() {
        let index = PsFontIndex::scan_all();
        if index.is_empty() {
            return;
        }
        let mut by_name: HashMap<&str, std::collections::HashSet<&str>> = HashMap::new();
        for entry in index.entries() {
            by_name.entry(entry.postscript_name.as_str()).or_default().insert(entry.sha256.as_str());
        }
        let ambiguous: Vec<(&str, usize)> = by_name.iter().filter(|(_, hashes)| hashes.len() > 1).map(|(n, h)| (*n, h.len())).collect();
        for ps in [
            "TimesNewRomanPSMT",
            "ArialMT",
            "Georgia",
            "CCGrimNGritty-Regular",
            "CCMeanwhile",
            "NotoSans-Regular",
            "NotoSans-Bold",
            "NotoSerif-Regular",
            "MyriadPro-Regular",
            "MinionPro-Regular",
        ] {
            assert!(
                !ambiguous.iter().any(|(n, _)| *n == ps),
                "{ps} has {ambiguous:?} different files across Photoshop's sources"
            );
        }
    }

    #[test]
    fn all_packaged_sources_exist_on_this_machine_or_are_skipped() {
        // The dirs list is never fatal: absent folders (CI, other hosts) drop out silently.
        let dirs = font_dirs();
        assert!(dirs.iter().all(|(d, _)| !d.as_os_str().is_empty()));
        let index = PsFontIndex::from_dirs(&dirs);
        assert_eq!(index.unreadable(), 0, "every readable font parses");
    }
}

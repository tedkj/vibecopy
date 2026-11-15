//! Paths as the engine handles them: never through a lossy string (a name with a lone
//! surrogate is a real Windows name, and rounding it to U+FFFD would point at another file),
//! always in the `\\?\` form on the disk (no 260-character limit), compared the way the file
//! system does (case-blind, by Windows' own upper-case table).

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

pub fn units(p: &OsStr) -> Vec<u16> {
    p.encode_wide().collect()
}

fn ascii(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// The `\\?\` form of an absolute path, losslessly: `C:\x` -> `\\?\C:\x`,
/// `\\server\share\x` -> `\\?\UNC\server\share\x`; one already in that form stays.
pub fn long(p: &Path) -> PathBuf {
    let w = units(p.as_os_str());
    if w.starts_with(&ascii(r"\\?\")) || w.starts_with(&ascii(r"\\.\")) {
        return p.to_path_buf();
    }
    let mut out = if w.starts_with(&ascii(r"\\")) {
        let mut o = ascii(r"\\?\UNC\");
        o.extend_from_slice(&w[2..]);
        o
    } else {
        let mut o = ascii(r"\\?\");
        o.extend_from_slice(&w);
        o
    };
    // `\\?\` takes the path literally: a trailing separator stays off (except a drive root).
    let drive_root = |o: &[u16]| o.len() == 7 && o[5] == b':' as u16;
    while out.len() > 4 && out.last() == Some(&(b'\\' as u16)) && !drive_root(&out) {
        out.pop();
    }
    PathBuf::from(OsString::from_wide(&out))
}

/// The path as people know it (the `\\?\` prefix taken off again).
pub fn short(p: &Path) -> PathBuf {
    let w = units(p.as_os_str());
    if w.starts_with(&ascii(r"\\?\UNC\")) {
        let mut o = ascii(r"\\");
        o.extend_from_slice(&w[8..]);
        return PathBuf::from(OsString::from_wide(&o));
    }
    if w.starts_with(&ascii(r"\\?\")) {
        return PathBuf::from(OsString::from_wide(&w[4..]));
    }
    p.to_path_buf()
}

/// For the window and reports: the short form, lossy (a lone surrogate shows as U+FFFD), with
/// control characters made visible. Never used to find a file again.
pub fn shown(p: &Path) -> String {
    clean(&short(p).to_string_lossy())
}

/// Text from outside (file names) as the window may show it: control characters become
/// U+FFFD, so a name cannot break a line or a report; the format characters that change how
/// text reads unseen go (bidi overrides and isolates: `invoice\u{202E}fdp.exe` reads
/// "invoiceexe.pdf"; zero-width spaces, the word joiner, the BOM; family sweep 2, C49). The
/// joiners U+200C (ZWNJ) and U+200D (ZWJ) stay: emoji sequences and Persian, Arabic and Indic
/// names need them (family rule, 2026-10-20).
pub fn clean(s: &str) -> String {
    s.chars().filter_map(|c| if c.is_control() { Some('\u{fffd}') } else if invisible(c) { None } else { Some(c) }).collect()
}

/// Outside text as it may be shown: invisible characters gone, runs of whitespace (a newline
/// or tab included) one space, trimmed, at most `max` characters (an ellipsis marks a cut). The
/// template's `text::clean_max`, for About's install check (a signer's name, a reason).
pub fn clean_max(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max * 4));
    let mut space = false;
    let mut n = 0usize;
    let mut cut = false;
    for c in s.chars() {
        if c.is_whitespace() || c == '\u{2028}' || c == '\u{2029}' {
            space = !out.is_empty();
            continue;
        }
        if invisible(c) {
            continue;
        }
        if space {
            if n + 1 >= max {
                cut = true;
                break;
            }
            out.push(' ');
            n += 1;
            space = false;
        }
        if n >= max {
            cut = true;
            break;
        }
        out.push(c);
        n += 1;
    }
    if cut {
        // Room for the ellipsis: drop the last character (never a byte: chars only).
        out.pop();
        out.push('…');
    }
    out
}

/// UTF-16 from Windows as shown text: a lone surrogate becomes U+FFFD, then cleaned.
pub fn clean_wide(w: &[u16], max: usize) -> String {
    clean_max(&String::from_utf16_lossy(w), max)
}

/// Characters that change how the text around them reads without showing themselves: controls,
/// bidirectional embeddings, overrides, isolates and marks, zero-width spaces and line
/// separators, the soft hyphen and the Unicode tag characters (U+E0000..U+E007F). The template's
/// set (the VibeWare template's, IN-VibeCopy-4); `clean` shows a control as U+FFFD.
fn invisible(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}' | '\u{200E}' | '\u{200F}' | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}' | '\u{E0000}'..='\u{E007F}')
}

/// One plain name, as Windows takes it in a folder: not empty, not `.` or `..`, and no
/// separator, colon (a drive, a stream) or NUL in it (IN-VibeCopy-1, IN-VibeCopy-3).
pub fn plain_name(n: &OsStr) -> bool {
    let w = units(n);
    !w.is_empty() && w != ascii(".") && w != ascii("..") && !w.iter().any(|&u| u == 0 || u == b'\\' as u16 || u == b'/' as u16 || u == b':' as u16)
}

/// A path under a folder: relative, and every part of it a `plain_name` (no root, drive, `.`
/// or `..`), so it can only ever lead down into the folder it is joined to.
pub fn plain_rel(p: &Path) -> bool {
    let mut any = false;
    for c in p.components() {
        match c {
            std::path::Component::Normal(n) if plain_name(n) => any = true,
            _ => return false,
        }
    }
    any
}

/// `child`'s path below `parent` (`within`, and not `parent` itself), or None.
pub fn below(child: &Path, parent: &Path) -> Option<PathBuf> {
    if !within(child, parent) {
        return None;
    }
    let (c, p) = (trimmed(&short(child)), trimmed(&short(parent)));
    let mut rest = &c[p.len()..];
    while rest.first() == Some(&(b'\\' as u16)) {
        rest = &rest[1..];
    }
    (!rest.is_empty()).then(|| PathBuf::from(OsString::from_wide(rest)))
}

/// A name, for the window (lossy, cleaned).
pub fn shown_name(n: &OsStr) -> String {
    clean(&n.to_string_lossy())
}

/// Equal as the file system sees names: case-blind by Windows' upper-case table.
pub fn same(a: &OsStr, b: &OsStr) -> bool {
    let (a, b) = (units(a), units(b));
    a.len() == b.len() && eq_units(&a, &b)
}

fn eq_units(a: &[u16], b: &[u16]) -> bool {
    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
    if a.is_empty() && b.is_empty() {
        return true;
    }
    unsafe { CompareStringOrdinal(a.as_ptr(), a.len() as i32, b.as_ptr(), b.len() as i32, 1) == CSTR_EQUAL }
}

/// Same path (both in the same form; trailing separators ignored).
pub fn same_path(a: &Path, b: &Path) -> bool {
    let (a, b) = (path_key(a), path_key(b));
    a.len() == b.len() && eq_units(&a, &b)
}

/// What paths are compared by (the short form, trailing separators off): made once per path,
/// then sorted with `cmp_keys`, so thousands of pasted files are de-duplicated in O(n log n).
pub fn path_key(p: &Path) -> Vec<u16> {
    trimmed(&short(p))
}

/// The order of `path_key`s: by units, case-blind with the same table as `same_path`
/// (`Equal` exactly when `same_path` says so). A total order, safe to sort with.
pub fn cmp_keys(a: &[u16], b: &[u16]) -> Ordering {
    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CSTR_LESS_THAN, CompareStringOrdinal};
    if a.is_empty() || b.is_empty() {
        return a.len().cmp(&b.len());
    }
    match unsafe { CompareStringOrdinal(a.as_ptr(), a.len() as i32, b.as_ptr(), b.len() as i32, 1) } {
        CSTR_LESS_THAN => Ordering::Less,
        CSTR_EQUAL => Ordering::Equal,
        _ => Ordering::Greater,
    }
}

/// A name's units or a `path_key`, ordered by `cmp_keys`, for a map: two keys are equal exactly
/// when the file system takes them as one name.
pub struct Key(pub Vec<u16>);

impl PartialEq for Key {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}

impl Eq for Key {}

impl PartialOrd for Key {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl Ord for Key {
    fn cmp(&self, o: &Self) -> Ordering {
        cmp_keys(&self.0, &o.0)
    }
}

/// The paths with repeats dropped (`same_path`), the first of each kept, in their order.
pub fn unique(list: Vec<PathBuf>) -> Vec<PathBuf> {
    let keys: Vec<Vec<u16>> = list.iter().map(|p| path_key(p)).collect();
    let mut idx: Vec<usize> = (0..keys.len()).collect();
    idx.sort_by(|&a, &b| cmp_keys(&keys[a], &keys[b]).then(a.cmp(&b)));
    let mut keep = vec![false; keys.len()];
    let mut prev: Option<usize> = None;
    for i in idx {
        if prev.is_none_or(|p| cmp_keys(&keys[p], &keys[i]) != Ordering::Equal) {
            keep[i] = true;
            prev = Some(i);
        }
    }
    list.into_iter().zip(keep).filter_map(|(p, k)| k.then_some(p)).collect()
}

fn trimmed(p: &Path) -> Vec<u16> {
    let mut w = units(p.as_os_str());
    while w.len() > 3 && w.last() == Some(&(b'\\' as u16)) {
        w.pop();
    }
    w
}

/// `child` is `parent` itself or somewhere under it.
pub fn within(child: &Path, parent: &Path) -> bool {
    let (c, p) = (trimmed(&short(child)), trimmed(&short(parent)));
    if c.len() < p.len() || !eq_units(&c[..p.len()], &p) {
        return false;
    }
    c.len() == p.len() || p.last() == Some(&(b'\\' as u16)) || c[p.len()] == b'\\' as u16
}

/// The drive or share a path lives on: `D:\`, `\\server\share\`.
pub fn root_of(path: &Path) -> PathBuf {
    let p = short(path);
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\") {
        let mut parts = rest.split('\\').filter(|p| !p.is_empty());
        if let (Some(server), Some(share)) = (parts.next(), parts.next()) {
            return PathBuf::from(format!(r"\\{server}\{share}\"));
        }
        return p.clone();
    }
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' {
        return PathBuf::from(format!("{}:\\", (b[0] as char).to_ascii_uppercase()));
    }
    p.ancestors().last().map(Path::to_path_buf).unwrap_or(p.clone())
}

/// The key jobs share a drive by (one job at a time per destination drive).
pub fn drive_key(path: &Path) -> String {
    root_of(path).to_string_lossy().to_uppercase()
}

/// `.`, `..` and empty parts resolved as text, every name kept exactly. Windows' own resolving
/// (GetFullPathNameW, behind `std::path::absolute`) drops a trailing dot or space from a name:
/// `E:\Backup\end.` would become `E:\Backup\end`, another folder (C34). A `\\?\` path is literal
/// and stays as it is.
pub fn resolve(p: &Path) -> PathBuf {
    use std::path::{Component, Prefix};
    let mut head = OsString::new();
    let mut names: Vec<&OsStr> = Vec::new();
    for c in p.components() {
        match c {
            Component::Prefix(x) if matches!(x.kind(), Prefix::Verbatim(_) | Prefix::VerbatimDisk(_) | Prefix::VerbatimUNC(..)) => return p.to_path_buf(),
            Component::Prefix(_) | Component::RootDir => head.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                names.pop();
            }
            Component::Normal(n) => names.push(n),
        }
    }
    for (i, n) in names.iter().enumerate() {
        if i > 0 {
            head.push("\\");
        }
        head.push(n);
    }
    PathBuf::from(head)
}

/// What a typed folder path is taken to mean (decision R2-COPY-1): the text exactly, and, when
/// it ends in blanks, the path without them. Which one is meant is the disk's answer, asked on a
/// worker: the blanks stay only when that folder is there as typed.
pub fn typed(input: &str) -> Result<(PathBuf, Option<PathBuf>), String> {
    let cut = input.trim_end();
    if cut.len() == input.len() {
        return Ok((normalize(input)?, None));
    }
    let trimmed = normalize(cut)?;
    match normalize(input) {
        Ok(exact) if exact != trimmed => Ok((exact, Some(trimmed))),
        _ => Ok((trimmed, None)),
    }
}

/// Why VibeCopy will not take a folder whose path its text cannot hold (a lone surrogate in a
/// name): as text it would point at a lookalike (R2-COPY-2, C17).
pub fn unreadable(p: &Path) -> String {
    format!("{} has a character in its name VibeCopy cannot read. Rename it in Explorer, then pick it again.", shown(p))
}

/// Why Explorer cannot open this folder, if it cannot (C35): it drops a trailing dot or space
/// from a name, cannot reach a device name (`aux`, `nul.txt`), and stops at 260 characters.
pub fn explorer_refuses(p: &Path) -> Option<String> {
    let plain = short(p);
    let mut names = plain.components().filter_map(|c| match c {
        std::path::Component::Normal(n) => Some(n),
        _ => None,
    });
    let why = if names.any(|n| {
        let w = units(n);
        let last = w.last().copied();
        let stem = String::from_utf16_lossy(&w).split('.').next().unwrap_or("").trim_end().to_ascii_lowercase();
        let device = matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
            || ((stem.starts_with("com") || stem.starts_with("lpt")) && stem.chars().count() == 4 && stem.chars().nth(3).is_some_and(|c| matches!(c, '1'..='9' | '¹' | '²' | '³')));
        last == Some(b'.' as u16) || last == Some(b' ' as u16) || device
    }) {
        "a name in it ends in a dot or a space, or is a device name"
    } else if units(plain.as_os_str()).len() >= 260 {
        "its path is 260 characters or longer"
    } else {
        return None;
    };
    Some(format!("Explorer cannot open {}: {why}.", shown(p)))
}

/// A folder path as the app keeps it: absolute, backslashes, no trailing separator (except a
/// drive root, `D:\`), `..` and `.` resolved, every name exact (a trailing dot or space is part
/// of a name, C34; only blanks before it, quotes around it and a pasted line end go). Plain text
/// in, so no file is looked at. Picked, recent and `--to` paths are taken this way; typed ones
/// through `typed`.
pub fn normalize(input: &str) -> Result<PathBuf, String> {
    let t = input.trim_start().trim_end_matches(char::is_control);
    let t = if t.trim_end().ends_with('"') { t.trim_end().trim_end_matches('"') } else { t };
    let t = t.trim_start_matches('"').replace('/', "\\");
    if t.is_empty() {
        return Err("Choose where the files go.".into());
    }
    let b = t.as_bytes();
    let drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    let unc = t.starts_with(r"\\") && t[2..].split('\\').filter(|p| !p.is_empty()).count() >= 2;
    if t.starts_with(r"\\?\") || t.starts_with(r"\\.\") {
        return Err("Type the folder without the \\\\?\\ prefix.".into());
    }
    if !(drive && (b.len() == 2 || b[2] == b'\\')) && !unc {
        // The text as typed or sent, cut in the middle: a `--to` refusal never echoes a whole
        // command line (RRF6-1).
        return Err(format!("\"{}\" is not a full folder path (like E:\\Backup or \\\\server\\share\\folder).", crate::format::elide_middle(&clean(&t), 60)));
    }
    let full = if drive && b.len() == 2 { format!("{t}\\") } else { t.clone() };
    // From text, so it is text again.
    let mut s = resolve(Path::new(&full)).to_string_lossy().to_string();
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    if drive {
        s = s[..1].to_ascii_uppercase() + &s[1..];
    }
    Ok(PathBuf::from(s))
}

/// The name a source gets in the destination: its own name; a drive root (`D:\`) is named by
/// its letter.
pub fn top_name(source: &Path) -> OsString {
    match short(source).file_name() {
        Some(n) => n.to_os_string(),
        None => {
            let r = root_of(source).to_string_lossy().trim_end_matches('\\').trim_start_matches('\\').replace(['\\', ':'], " ");
            OsString::from(r.trim().to_string())
        }
    }
}

/// Lossless JSON for paths: a plain string when the path is valid Unicode (the usual case,
/// readable in queue.json), else its UTF-16 units.
pub mod serde_path {
    use super::*;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Text(String),
        Units(Vec<u16>),
    }

    pub fn to_repr(p: &OsStr) -> impl Serialize {
        match p.to_str() {
            Some(s) => Repr::Text(s.to_string()),
            None => Repr::Units(units(p)),
        }
    }

    fn from_repr(r: Repr) -> OsString {
        match r {
            Repr::Text(s) => OsString::from(s),
            Repr::Units(u) => OsString::from_wide(&u),
        }
    }

    pub fn serialize<S: Serializer>(p: &Path, s: S) -> Result<S::Ok, S::Error> {
        to_repr(p.as_os_str()).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
        Ok(PathBuf::from(from_repr(Repr::deserialize(d)?)))
    }

    pub mod opt {
        use super::*;
        pub fn serialize<S: Serializer>(p: &Option<PathBuf>, s: S) -> Result<S::Ok, S::Error> {
            p.as_ref().map(|p| to_repr(p.as_os_str())).serialize(s)
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<PathBuf>, D::Error> {
            Ok(Option::<Repr>::deserialize(d)?.map(|r| PathBuf::from(from_repr(r))))
        }
    }

    pub mod vec {
        use super::*;
        pub fn serialize<S: Serializer>(v: &[PathBuf], s: S) -> Result<S::Ok, S::Error> {
            s.collect_seq(v.iter().map(|p| to_repr(p.as_os_str())))
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<PathBuf>, D::Error> {
            Ok(Vec::<Repr>::deserialize(d)?.into_iter().map(|r| PathBuf::from(from_repr(r))).collect())
        }
    }
}

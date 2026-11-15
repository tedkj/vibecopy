//! The command line: `VibeCopy.exe <sources...>` (Explorer's Send To: pick the destination in
//! the window) and `VibeCopy.exe --to <destination> [--move] [--verify] <sources...>` (queued at
//! once). Relative paths are taken from the starting folder, before they travel to another
//! instance.

use crate::job::Mode;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub dest: Option<PathBuf>,
    pub mode: Mode,
    pub verify: bool,
    pub sources: Vec<PathBuf>,
}

/// Absolute, `.` and `..` resolved (as text: nothing is looked at), every name exact: a folder
/// `end ` sent from Explorer is that folder, not `end` (review 2, R2-COPY-1; `std::path::absolute`
/// drops a trailing dot or space). A drive-relative `D:x` is left to Windows.
pub fn absolute(p: &Path, cwd: &Path) -> PathBuf {
    let joined = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    if joined.has_root() {
        crate::paths::resolve(&joined)
    } else {
        std::path::absolute(&joined).unwrap_or(joined)
    }
}

/// The arguments with every path made absolute (what a second start hands on).
pub fn resolve(args: &[OsString], cwd: &Path) -> Vec<OsString> {
    let mut out = Vec::new();
    for a in args {
        if a.to_str().is_some_and(|s| s.starts_with("--")) {
            out.push(a.clone());
        } else {
            out.push(absolute(Path::new(a), cwd).into_os_string());
        }
    }
    out
}

/// What `parse` refuses: an option VibeCopy does not take, or a use of its own options that
/// cannot be done (`--to` without a folder or files). Shown as its message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Unknown(String),
    Usage(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The option as typed, cut in the middle: a toast never echoes a whole command line
            // (RRF6-1).
            Error::Unknown(s) => write!(f, "Unknown option {}. Use: VibeCopy.exe --to <folder> [--move] [--verify] <files...>", crate::format::elide_middle(&crate::paths::clean(s), 40)),
            Error::Usage(s) => f.write_str(s),
        }
    }
}

/// False when `parse` finds an option VibeCopy does not take: `main` then exits 2 before any
/// window, file or second-start message. The same grammar as `parse`, since it is `parse`
/// (RH-L2); any other mistake (`--to` without files) is said in the window.
pub fn known_options(args: &[OsString]) -> bool {
    !matches!(parse(args), Err(Error::Unknown(_)))
}

pub fn parse(args: &[OsString]) -> Result<Args, Error> {
    let mut out = Args::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.to_str() {
            Some("--to") => {
                let d = it.next().ok_or(Error::Usage("--to needs a destination folder."))?;
                out.dest = Some(PathBuf::from(d));
            }
            Some("--move") => out.mode = Mode::Move,
            Some("--verify") => out.verify = true,
            Some(s) if s.starts_with("--") => return Err(Error::Unknown(s.to_string())),
            _ => out.sources.push(PathBuf::from(a)),
        }
    }
    if out.dest.is_some() && out.sources.is_empty() {
        return Err(Error::Usage("--to needs files or folders to copy."));
    }
    Ok(out)
}

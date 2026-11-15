//! What happens to a file that is already there: the job's policy (or the user's answer) and the
//! two files' sizes and times in, one action out. Pure; the copy engine does what it says.
//! Also the "Keep both" name: `report (2).pdf`, counting past the ones already there.

use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};

/// A job's standing rule for conflicts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Policy {
    /// The compact dialog, per file (or for the rest of the job).
    #[default]
    Ask,
    Skip,
    Overwrite,
    /// Only when the one there is older; else skipped.
    OverwriteOlder,
    KeepBoth,
}

impl Policy {
    pub const ALL: [Policy; 5] = [Policy::Ask, Policy::Skip, Policy::Overwrite, Policy::OverwriteOlder, Policy::KeepBoth];

    pub fn label(self) -> &'static str {
        match self {
            Policy::Ask => "Ask",
            Policy::Skip => "Skip",
            Policy::Overwrite => "Overwrite",
            Policy::OverwriteOlder => "Overwrite if older",
            Policy::KeepBoth => "Keep both",
        }
    }

    pub fn index(self) -> i32 {
        Policy::ALL.iter().position(|p| *p == self).unwrap_or(0) as i32
    }

    pub fn from_index(i: i32) -> Policy {
        Policy::ALL.get(i.max(0) as usize).copied().unwrap_or_default()
    }
}

/// The dialog's answer for one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Choice {
    Skip,
    Overwrite,
    KeepBoth,
}

impl Choice {
    /// "Apply to all": the answer becomes the job's policy.
    pub fn policy(self) -> Policy {
        match self {
            Choice::Skip => Policy::Skip,
            Choice::Overwrite => Policy::Overwrite,
            Choice::KeepBoth => Policy::KeepBoth,
        }
    }
}

/// Size and modified time (FILETIME ticks) of one side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub size: u64,
    pub mtime: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Skip(&'static str),
    Overwrite,
    KeepBoth,
    Ask,
}

/// Times closer than this are the same time: FAT and exFAT keep times to 2 s and 10 ms, so a
/// file copied there earlier reads up to 2 s off.
pub const SAME_TIME: u64 = 2 * 10_000_000;

pub const SKIPPED: &str = "Already there; skipped";
pub const NEWER_THERE: &str = "The one there is newer";
pub const SAME_AGE: &str = "The one there is as new";

/// The action for `src` meeting `there`. An answer for this file wins over the policy.
pub fn decide(policy: Policy, answer: Option<Choice>, src: Meta, there: Meta) -> Action {
    let p = match answer {
        Some(c) => c.policy(),
        None => policy,
    };
    match p {
        Policy::Ask => Action::Ask,
        Policy::Skip => Action::Skip(SKIPPED),
        Policy::Overwrite => Action::Overwrite,
        Policy::KeepBoth => Action::KeepBoth,
        Policy::OverwriteOlder => {
            if src.mtime > there.mtime && src.mtime - there.mtime >= SAME_TIME {
                Action::Overwrite
            } else if there.mtime > src.mtime && there.mtime - src.mtime >= SAME_TIME {
                Action::Skip(NEWER_THERE)
            } else {
                Action::Skip(SAME_AGE)
            }
        }
    }
}

/// The longest name NTFS takes (UTF-16 units per name).
pub const MAX_NAME: usize = 255;

/// A free name for "Keep both": `report.pdf` -> `report (2).pdf`, or `(3)` when `(2)` is taken.
/// A name that already ends in ` (n)` counts on from n: `report (2).pdf` -> `report (3).pdf`,
/// never `report (2) (2).pdf`. The extension is what follows the last dot (`a.tar (2).gz`); a
/// leading dot is not one (`.gitignore (2)`). Works on UTF-16 units, so any name Windows holds
/// keeps its exact spelling. `taken` says whether a name is in use (the caller asks the disk).
pub fn keep_both_name(name: &OsStr, taken: &dyn Fn(&OsStr) -> bool) -> OsString {
    let w: Vec<u16> = name.encode_wide().collect();
    let (stem, ext) = stem_ext(&w);
    let (base, from) = match counted(stem) {
        Some((base, n)) => (base, n.saturating_add(1).max(2)),
        None => (stem, 2),
    };
    let mut n = from;
    loop {
        let suffix: Vec<u16> = format!(" ({n})").encode_utf16().collect();
        let room = MAX_NAME.saturating_sub(suffix.len() + ext.len());
        let mut b = base.to_vec();
        if b.len() > room {
            b.truncate(room);
            // Never split a surrogate pair.
            if b.last().is_some_and(|&u| (0xD800..0xDC00).contains(&u)) {
                b.pop();
            }
        }
        let mut cand = b;
        cand.extend_from_slice(&suffix);
        cand.extend_from_slice(ext);
        let cand = OsString::from_wide(&cand);
        if !taken(&cand) {
            return cand;
        }
        n += 1;
    }
}

/// A name split as `keep_both_name` does: the stem, and the extension (from the last dot, not
/// a leading one).
fn stem_ext(w: &[u16]) -> (&[u16], &[u16]) {
    match w.iter().rposition(|&u| u == b'.' as u16).filter(|&i| i > 0) {
        Some(i) => (&w[..i], &w[i..]),
        None => (w, &[][..]),
    }
}

/// `name` could be a "Keep both" name (its stem ends in ` (n)`): every name `keep_both_name`
/// gives is one, but for an extension so long it leaves no room for a stem.
pub fn counted_name(name: &OsStr) -> bool {
    let w: Vec<u16> = name.encode_wide().collect();
    counted(stem_ext(&w).0).is_some()
}

/// `stem` ending in ` (n)` (n a plain number): the part before it and n.
fn counted(stem: &[u16]) -> Option<(&[u16], u64)> {
    if stem.last() != Some(&(b')' as u16)) {
        return None;
    }
    let open = stem.iter().rposition(|&u| u == b'(' as u16)?;
    if open < 2 || stem[open - 1] != b' ' as u16 {
        return None;
    }
    let digits = &stem[open + 1..stem.len() - 1];
    if digits.is_empty() || digits.len() > 9 || !digits.iter().all(|&u| (b'0' as u16..=b'9' as u16).contains(&u)) {
        return None;
    }
    let n = digits.iter().fold(0u64, |a, &d| a * 10 + (d - b'0' as u16) as u64);
    Some((&stem[..open - 1], n))
}

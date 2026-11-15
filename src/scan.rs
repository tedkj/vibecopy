//! A job's sources walked into its item list: every folder before what is in it, names in a
//! fixed order (by UTF-16 units: a total order, whatever the names). Links and junctions are
//! listed and skipped, never followed. A source picked twice, or inside another picked one, is
//! walked once.

use crate::copy::{Control, describe};
use crate::disk::{Disk, Kind};
use crate::job::{Item, ItemState, Stage};
use crate::paths::{self, long};
use std::path::{Path, PathBuf};

pub const LINK: &str = "A link or junction: not followed";

/// A name a listing gave that is not one plain name (IN-VibeCopy-3).
pub const BAD_NAME: &str = "Its name is not one Windows allows (a separator, a colon, `.` or `..`): not copied";

/// A bad name as it is kept (and shown): a separator, colon or NUL becomes U+FFFD, and `.`,
/// `..` or nothing get one in front, so it is one plain name that leads nowhere.
fn shown_bad_name(n: &std::ffi::OsStr) -> std::ffi::OsString {
    use std::os::windows::ffi::OsStringExt;
    let mut w: Vec<u16> = paths::units(n).into_iter().map(|u| if u == 0 || u == b'\\' as u16 || u == b'/' as u16 || u == b':' as u16 { 0xFFFD } else { u }).collect();
    if !paths::plain_name(&std::ffi::OsString::from_wide(&w)) {
        w.insert(0, 0xFFFD);
    }
    std::ffi::OsString::from_wide(&w)
}

/// The sources with duplicates and nested ones dropped (the outer one carries the inner).
/// Sorted keys and binary searches, not a scan per source: 5 000 pasted files took 85 s.
/// The first spelling of each stays, in the caller's order.
pub fn distinct(sources: &[PathBuf]) -> Vec<PathBuf> {
    use paths::cmp_keys;
    const SEP: u16 = b'\\' as u16;
    let keys: Vec<Vec<u16>> = sources.iter().map(|s| paths::path_key(s)).collect();
    let mut set: Vec<usize> = (0..keys.len()).collect();
    set.sort_by(|&a, &b| cmp_keys(&keys[a], &keys[b]).then(a.cmp(&b)));
    set.dedup_by(|b, a| cmp_keys(&keys[*a], &keys[*b]) == std::cmp::Ordering::Equal);
    let pos = |k: &[u16]| set.binary_search_by(|&i| cmp_keys(&keys[i], k)).ok();
    // `paths::within`'s rule: an ancestor ends just before a separator, or (a drive root,
    // `D:\`, the only key that keeps one) just after it.
    let inside = |key: &[u16]| key.iter().enumerate().any(|(k, &u)| u == SEP && (pos(&key[..k]).is_some() || (k < 3 && k + 1 < key.len() && pos(&key[..=k]).is_some())));
    let keep: Vec<bool> = set.iter().map(|&i| !inside(&keys[i])).collect();
    let mut taken = vec![false; set.len()];
    let mut out = Vec::new();
    for (s, key) in sources.iter().zip(&keys) {
        if let Some(p) = pos(key)
            && keep[p]
            && !taken[p]
        {
            taken[p] = true;
            out.push(paths::short(s));
        }
    }
    out
}

/// Walk the sources. `found` hears files and bytes so far (every folder); None when the
/// brakes stopped it.
pub fn scan(disk: &dyn Disk, sources: &[PathBuf], ctl: &Control, found: &dyn Fn(u64, u64)) -> Option<Vec<Item>> {
    let mut items: Vec<Item> = Vec::new();
    let (mut files, mut bytes) = (0u64, 0u64);
    for src in distinct(sources) {
        let rel = PathBuf::from(paths::top_name(&src));
        let top = Item::new(src.clone(), rel, false, 0, 0, 0);
        walk(disk, top, &src, ctl, &mut items, &mut files, &mut bytes, found)?;
    }
    found(files, bytes);
    items.shrink_to_fit();
    Some(items)
}

/// Retry's walk (FS1): an item the count could not read (`Item::unwalked`: a folder that could
/// not be listed, or a picked item that could not be looked at), at `src`, read again. Its items
/// in walk order: it first (pending again, or failed again and still unwalked), then everything
/// in it. None when the brakes stopped it.
pub fn rewalk(disk: &dyn Disk, it: &Item, src: &Path, ctl: &Control) -> Option<Vec<Item>> {
    let mut top = it.clone();
    top.state = ItemState::Pending;
    top.unwalked = false;
    let mut items = Vec::new();
    let (mut files, mut bytes) = (0u64, 0u64);
    walk(disk, top, src, ctl, &mut items, &mut files, &mut bytes, &|_, _| {})?;
    Some(items)
}

/// One item, looked at, and (a folder) everything in it, pre-order, into `items`. `top` is the
/// item as it stands so far (its kind and size are taken from the disk). None: the brakes.
#[allow(clippy::too_many_arguments)]
fn walk(disk: &dyn Disk, mut top: Item, src: &Path, ctl: &Control, items: &mut Vec<Item>, files: &mut u64, bytes: &mut u64, found: &dyn Fn(u64, u64)) -> Option<()> {
    let st = match disk.stat(&long(src)) {
        Ok(st) => st,
        Err(e) => {
            // Never looked at: a retry looks again before anything is made for it.
            top.state = ItemState::Failed { stage: Stage::Read, reason: describe(&e) };
            top.unwalked = true;
            items.push(top);
            return Some(());
        }
    };
    top.dir = st.dir;
    top.mtime = st.mtime;
    top.size = if st.dir { 0 } else { st.size };
    if st.link {
        top.size = 0;
        top.state = ItemState::Skipped(LINK.into());
        items.push(top);
        return Some(());
    }
    if !st.dir {
        *files += 1;
        *bytes += top.size;
        items.push(top);
        return Some(());
    }
    // Pre-order: a stack of what is still to be listed, the next one on top.
    // With each its full source path (the items keep it only at the top).
    let mut stack: Vec<(Item, PathBuf)> = vec![(top, src.to_path_buf())];
    while let Some((mut it, full)) = stack.pop() {
        if !it.dir {
            *files += 1;
            *bytes += it.size;
            items.push(it);
            continue;
        }
        if ctl.stopping() {
            return None;
        }
        let listed = disk.list(&long(&full));
        let (src, rel, depth) = (full, it.rel.clone(), it.depth);
        match listed {
            Err(e) => {
                // FS1: nothing under it is known, so it stays failed until a retry lists it;
                // it is never made empty and called done.
                it.state = ItemState::Failed { stage: Stage::Read, reason: format!("Could not read the folder: {}", describe(&e)) };
                it.unwalked = true;
                items.push(it);
            }
            Ok(mut entries) => {
                items.push(it);
                entries.sort_by_key(|a| paths::units(&a.name));
                for e in entries.into_iter().rev() {
                    // A name from the listing (a server's) is joined into paths only when it
                    // is one plain name; else it fails, never read, walked or made.
                    if !paths::plain_name(&e.name) {
                        let name = shown_bad_name(&e.name);
                        let mut child = Item::new(src.join(&name), rel.join(&name), false, e.size, e.mtime, depth.saturating_add(1));
                        child.state = ItemState::Failed { stage: Stage::Read, reason: BAD_NAME.into() };
                        items.push(child);
                        continue;
                    }
                    let mut child = Item::new(src.join(&e.name), rel.join(&e.name), e.kind == Kind::Dir, e.size, e.mtime, depth.saturating_add(1));
                    if e.kind == Kind::Link {
                        child.state = ItemState::Skipped(LINK.into());
                        items.push(child);
                        // Skipped links go right after their folder; order among siblings
                        // does not matter for them.
                        continue;
                    }
                    stack.push((child, src.join(&e.name)));
                }
            }
        }
        found(*files, *bytes);
    }
    Some(())
}

/// Each item's folder (by index), from the walk order: `None` at the top.
pub fn parents(items: &[Item]) -> Vec<Option<usize>> {
    let mut out = Vec::with_capacity(items.len());
    let mut chain: Vec<usize> = Vec::new();
    for (i, it) in items.iter().enumerate() {
        chain.truncate(it.depth as usize);
        out.push(chain.last().copied());
        if it.dir {
            chain.push(i);
        }
    }
    out
}

/// Files of one job that land on one target (X15WFS-1): case twins from a case-sensitive
/// folder (`Makefile` and `makefile`, made from WSL), or same-named sources from different
/// folders. Per item, the group it shares its target with (None: no other file has it).
/// Folders with one target merge into one, so what is in them is compared as one folder; a file
/// and a folder of one name are left to the run (one of them fails as in the way, nothing is
/// lost). Names compare as the file system does (`paths::cmp_keys`); `parents` from `parents`.
pub fn twins(items: &[Item], parents: &[Option<usize>]) -> Vec<Option<u32>> {
    use paths::Key as Name;
    use std::collections::BTreeMap;
    // Per target folder (0: the destination) and name: the folder's own number, and the first
    // file there with its group once it has a twin.
    #[derive(Default)]
    struct Slot {
        dir: Option<u32>,
        file: Option<usize>,
        group: Option<u32>,
    }
    let mut map: BTreeMap<(u32, Name), Slot> = BTreeMap::new();
    let mut folder = vec![0u32; items.len()];
    let mut out = vec![None; items.len()];
    let (mut folders, mut groups) = (0u32, 0u32);
    for (i, it) in items.iter().enumerate() {
        let at = parents.get(i).copied().flatten().map_or(0, |p| folder[p]);
        let name = Name(paths::units(it.rel.file_name().unwrap_or(it.rel.as_os_str())));
        let slot = map.entry((at, name)).or_default();
        if it.dir {
            folder[i] = *slot.dir.get_or_insert_with(|| {
                folders += 1;
                folders
            });
        } else if let Some(first) = slot.file {
            let g = *slot.group.get_or_insert_with(|| {
                groups += 1;
                groups
            });
            out[first] = Some(g);
            out[i] = Some(g);
        } else {
            slot.file = Some(i);
        }
    }
    out
}

/// The items inside folder `i` (everything after it that is deeper), as a range.
pub fn subtree(items: &[Item], i: usize) -> std::ops::Range<usize> {
    let d = items[i].depth;
    let end = items[i + 1..].iter().position(|x| x.depth <= d).map(|p| i + 1 + p).unwrap_or(items.len());
    i + 1..end
}

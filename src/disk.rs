//! Everything the engine does to the disk goes through `Disk`: the real one in the app, and in
//! the tests the same with failures injected where they cannot be made for real in a temp
//! folder (a disk that fills up mid-file, a copy that lands corrupted, a delete that fails, a
//! copy held at a byte count so a pause lands inside the file).

use crate::copy::{self, Control, Copied, Opts, Stat, Step};
use crate::verify::{self, Digest};
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// A symlink, junction or mount point: never followed.
    Link,
}

/// One entry of a folder listing.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: OsString,
    pub kind: Kind,
    pub size: u64,
    pub mtime: u64,
}

pub trait Disk: Send + Sync {
    fn copy(&self, src: &Path, dst: &Path, opts: Opts, step: &dyn Fn(u64) -> Step) -> io::Result<Copied>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn replace(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn remove_dir(&self, path: &Path) -> io::Result<()>;
    fn create_dir(&self, path: &Path) -> io::Result<()>;
    fn stat(&self, path: &Path) -> io::Result<Stat>;
    /// SHA-256 of a file. `from_disk`: the copy's side, read past Windows' cache (FS7).
    fn hash(&self, path: &Path, from_disk: bool, ctl: &Control, read: &dyn Fn(u64)) -> io::Result<Option<Digest>>;
    fn list(&self, dir: &Path) -> io::Result<Vec<Entry>>;
    fn copy_dir_attributes(&self, src: &Path, dst: &Path);
    /// Two paths name one file (by volume and file id): a share under two names, a hard link.
    fn same_file(&self, a: &Path, b: &Path) -> bool;
}

/// The disk itself.
#[derive(Clone, Copy, Debug, Default)]
pub struct Real;

impl Disk for Real {
    fn copy(&self, src: &Path, dst: &Path, opts: Opts, step: &dyn Fn(u64) -> Step) -> io::Result<Copied> {
        copy::copy_file(src, dst, opts, step)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        copy::rename(from, to)
    }
    fn replace(&self, from: &Path, to: &Path) -> io::Result<()> {
        copy::replace(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        copy::remove_file(path)
    }
    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        copy::remove_empty_dir(path)
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir(path)
    }
    fn stat(&self, path: &Path) -> io::Result<Stat> {
        copy::stat(path)
    }
    fn hash(&self, path: &Path, from_disk: bool, ctl: &Control, read: &dyn Fn(u64)) -> io::Result<Option<Digest>> {
        verify::hash_file(path, from_disk, ctl, read)
    }
    fn list(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        use std::os::windows::fs::MetadataExt;
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let (ft, md) = match (e.file_type(), e.metadata()) {
                (Ok(ft), Ok(md)) => (ft, md),
                // Gone between the listing and now: as if never listed.
                _ => continue,
            };
            // `is_symlink` on Windows: a name-surrogate reparse point (symlink, junction,
            // mount point). Other reparse points (OneDrive, dedup) are data.
            let kind = if ft.is_symlink() {
                Kind::Link
            } else if ft.is_dir() {
                Kind::Dir
            } else {
                Kind::File
            };
            out.push(Entry { name: e.file_name(), kind, size: if kind == Kind::File { md.file_size() } else { 0 }, mtime: md.last_write_time() });
        }
        Ok(out)
    }
    fn copy_dir_attributes(&self, src: &Path, dst: &Path) {
        copy::copy_dir_attributes(src, dst)
    }
    fn same_file(&self, a: &Path, b: &Path) -> bool {
        match (copy::file_id(a), copy::file_id(b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        }
    }
}

/// Holds a copy at a byte count until it is opened (or the job's brakes are pulled).
#[derive(Default)]
pub struct Gate {
    state: Mutex<(bool, bool)>,
    cv: Condvar,
}

impl Gate {
    pub fn new() -> Arc<Gate> {
        Arc::new(Gate::default())
    }
    pub fn open(&self) {
        self.state.lock().unwrap().0 = true;
        self.cv.notify_all();
    }
    /// A copy is waiting at it.
    pub fn reached(&self) -> bool {
        self.state.lock().unwrap().1
    }
    /// Wait (really, up to 20 s) until a copy waits at it.
    pub fn wait_reached(&self) {
        let t0 = std::time::Instant::now();
        while !self.reached() {
            assert!(t0.elapsed().as_secs() < 20, "no copy reached the gate");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

#[derive(Clone)]
pub enum Fault {
    /// The copy fails at once with this OS error.
    FailCopy(i32),
    /// The copy fails after so many bytes (a disk that fills up: the fake writer).
    FailAfter(u64, i32),
    /// The copy lands with one byte flipped (what Verify is for).
    Corrupt,
    /// The copy lands with another modified time (an hour later).
    Retime,
    /// The copy lands one second later (what a FAT drive does to times).
    Nudge,
    /// The copy waits at so many bytes until the gate opens.
    Hold(Arc<Gate>, u64),
    /// Renaming or replacing something into this name fails.
    FailPlace(i32),
    /// Renaming something into this name waits until the gate opens (a share that is slow to
    /// answer; at most 30 s).
    HoldPlace(Arc<Gate>),
    /// Deleting this file fails (a move's original).
    FailRemove(i32),
    /// Making this folder fails.
    FailMkdir(i32),
    /// Listing this folder fails.
    FailList(i32),
    /// Listing this folder also gives these names (files), as a server might (IN-VibeCopy-3).
    Names(Vec<OsString>),
    /// Right after the copy is put in place under this name, this file (its original) is
    /// changed: someone saved it just then.
    TouchAfterPlace(std::path::PathBuf),
    /// When this file's copy starts, the folder goes away (renamed aside, as a drive pulled
    /// out) and the copy fails as Windows says then.
    Unplug(std::path::PathBuf),
    /// Looking at this file inside this folder fails as with its drive pulled out
    /// (ERROR_NOT_READY), while the rest of the disk answers.
    Away(std::path::PathBuf),
}

/// The real disk with failures injected by name (a file or folder name, case-blind).
#[derive(Default)]
pub struct Faulty {
    rules: Mutex<Vec<(OsString, Fault)>>,
    /// Folders that act as another drive: a rename out of one fails as across volumes.
    drives: Mutex<Vec<std::path::PathBuf>>,
    /// Every file hashed, and whether it was read from the drive (`Disk::hash`'s `from_disk`).
    hashed: Mutex<Vec<(std::path::PathBuf, bool)>>,
}

impl Faulty {
    pub fn new() -> Arc<Faulty> {
        Arc::new(Faulty::default())
    }
    pub fn add(&self, name: &str, f: Fault) {
        self.rules.lock().unwrap().push((OsString::from(name), f));
    }
    pub fn clear(&self) {
        self.rules.lock().unwrap().clear();
    }
    /// The files hashed so far, each with whether it was read from the drive.
    pub fn hashed(&self) -> Vec<(std::path::PathBuf, bool)> {
        self.hashed.lock().unwrap().clone()
    }
    /// Make `root` another drive: renames from inside it to outside fail with
    /// ERROR_NOT_SAME_DEVICE, as between two disks.
    pub fn other_drive(&self, root: &Path) {
        self.drives.lock().unwrap().push(root.to_path_buf());
    }
    fn touch_after(&self, placed: &Path) {
        for f in self.faults(placed) {
            if let Fault::TouchAfterPlace(orig) = f
                && let Ok(st) = crate::copy::stat(&orig)
            {
                let _ = set_mtime(&orig, st.mtime + 10_000_000);
            }
        }
    }
    fn across(&self, from: &Path, to: &Path) -> bool {
        self.drives.lock().unwrap().iter().any(|d| crate::paths::within(from, d) != crate::paths::within(to, d))
    }
    fn faults(&self, path: &Path) -> Vec<Fault> {
        let name = path.file_name().unwrap_or(OsStr::new(""));
        self.rules.lock().unwrap().iter().filter(|(n, _)| crate::paths::same(n, name)).map(|(_, f)| f.clone()).collect()
    }
}

fn os(code: i32) -> io::Error {
    io::Error::from_raw_os_error(code)
}

impl Disk for Faulty {
    fn copy(&self, src: &Path, dst: &Path, opts: Opts, step: &dyn Fn(u64) -> Step) -> io::Result<Copied> {
        let faults = self.faults(src);
        for f in &faults {
            if let Fault::FailCopy(c) = f {
                return Err(os(*c));
            }
            if let Fault::Unplug(dir) = f {
                let mut aside = dir.as_os_str().to_os_string();
                aside.push(".unplugged");
                std::fs::rename(dir, aside)?;
                return Err(os(crate::copy::ERROR_PATH_NOT_FOUND));
            }
        }
        let wrapped = |n: u64| {
            for f in &faults {
                match f {
                    Fault::FailAfter(at, c) if n != u64::MAX && n >= *at => return Step::Fail(*c),
                    Fault::Hold(gate, at) if n != u64::MAX && n >= *at => {
                        gate.state.lock().unwrap().1 = true;
                        loop {
                            let s = step(u64::MAX);
                            if s != Step::Go {
                                return s;
                            }
                            let g = gate.state.lock().unwrap();
                            if g.0 {
                                break;
                            }
                            let _ = gate.cv.wait_timeout(g, std::time::Duration::from_millis(5)).unwrap();
                        }
                    }
                    _ => {}
                }
            }
            step(n)
        };
        let r = Real.copy(src, dst, opts, &wrapped)?;
        if r == Copied::Done {
            for f in &faults {
                match f {
                    Fault::Corrupt => {
                        let mut b = std::fs::read(dst)?;
                        let mtime = copy::stat(dst)?.mtime;
                        if let Some(x) = b.first_mut() {
                            *x ^= 0x55;
                        } else {
                            b.push(0);
                        }
                        let ro = std::fs::metadata(dst)?.permissions();
                        let mut w = ro.clone();
                        #[allow(clippy::permissions_set_readonly_false)]
                        w.set_readonly(false);
                        std::fs::set_permissions(dst, w)?;
                        std::fs::write(dst, &b)?;
                        // Same time: only reading it back can tell (when the size stays, too).
                        if b.len() as u64 == copy::stat(src)?.size {
                            set_mtime(dst, mtime)?;
                        }
                        std::fs::set_permissions(dst, ro)?;
                    }
                    Fault::Retime => {
                        let m = copy::stat(dst)?.mtime;
                        set_mtime(dst, m + 3600 * 10_000_000)?;
                    }
                    Fault::Nudge => {
                        let m = copy::stat(dst)?.mtime;
                        set_mtime(dst, m + 10_000_000)?;
                    }
                    _ => {}
                }
            }
        }
        Ok(r)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        if self.across(from, to) {
            return Err(os(crate::copy::ERROR_NOT_SAME_DEVICE));
        }
        for f in self.faults(to) {
            if let Fault::FailPlace(c) = f {
                return Err(os(c));
            }
            if let Fault::HoldPlace(gate) = f {
                let t0 = std::time::Instant::now();
                let mut g = gate.state.lock().unwrap();
                g.1 = true;
                while !g.0 && t0.elapsed().as_secs() < 30 {
                    g = gate.cv.wait_timeout(g, std::time::Duration::from_millis(20)).unwrap().0;
                }
            }
        }
        Real.rename(from, to)?;
        self.touch_after(to);
        Ok(())
    }
    fn replace(&self, from: &Path, to: &Path) -> io::Result<()> {
        if self.across(from, to) {
            return Err(os(crate::copy::ERROR_NOT_SAME_DEVICE));
        }
        for f in self.faults(to) {
            if let Fault::FailPlace(c) = f {
                return Err(os(c));
            }
        }
        Real.replace(from, to)?;
        self.touch_after(to);
        Ok(())
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        for f in self.faults(path) {
            if let Fault::FailRemove(c) = f {
                return Err(os(c));
            }
        }
        Real.remove_file(path)
    }
    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        for f in self.faults(path) {
            if let Fault::FailRemove(c) = f {
                return Err(os(c));
            }
        }
        Real.remove_dir(path)
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        for f in self.faults(path) {
            if let Fault::FailMkdir(c) = f {
                return Err(os(c));
            }
        }
        Real.create_dir(path)
    }
    fn stat(&self, path: &Path) -> io::Result<Stat> {
        for f in self.faults(path) {
            if let Fault::Away(dir) = f
                && crate::paths::within(path, &dir)
            {
                return Err(os(21));
            }
        }
        Real.stat(path)
    }
    fn hash(&self, path: &Path, from_disk: bool, ctl: &Control, read: &dyn Fn(u64)) -> io::Result<Option<Digest>> {
        self.hashed.lock().unwrap().push((path.to_path_buf(), from_disk));
        Real.hash(path, from_disk, ctl, read)
    }
    fn list(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        for f in self.faults(dir) {
            if let Fault::FailList(c) = f {
                return Err(os(c));
            }
        }
        let mut out = Real.list(dir)?;
        for f in self.faults(dir) {
            if let Fault::Names(names) = f {
                out.extend(names.into_iter().map(|name| Entry { name, kind: Kind::File, size: 1, mtime: 0 }));
            }
        }
        Ok(out)
    }
    fn copy_dir_attributes(&self, src: &Path, dst: &Path) {
        Real.copy_dir_attributes(src, dst)
    }
    fn same_file(&self, a: &Path, b: &Path) -> bool {
        Real.same_file(a, b)
    }
}

/// Stamp a file's modified time (FILETIME ticks).
pub fn set_mtime(path: &Path, ticks: u64) -> io::Result<()> {
    let f = std::fs::OpenOptions::new().write(true).open(path)?;
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_nanos((ticks - 116_444_736_000_000_000) * 100);
    f.set_times(std::fs::FileTimes::new().set_modified(t))
}

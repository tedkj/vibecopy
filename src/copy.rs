//! Moving bytes and names on the disk. One file is copied with `CopyFile2` (attributes and the
//! modified time carried, alternate streams too, progress per chunk), and can be **paused inside
//! the file**: the progress routine answers `COPYFILE2_PROGRESS_PAUSE`, the part stays on disk,
//! and the next call with `COPY_FILE_RESUME_FROM_PAUSE` carries on where it stopped, in this
//! process or after a restart (CopyFile2 starts over by itself when the source changed
//! meanwhile). Measured on this Windows 11: `CopyFileExW`'s `COPY_FILE_RESTARTABLE` +
//! `PROGRESS_STOP` deletes the part and starts again from zero, and costs ~45 % of the speed;
//! so not that. Renames never replace unless asked to; deletes never follow links.
//! Every path here is already in the `\\?\` form (`paths::long`).

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};

fn wide(p: &Path) -> Vec<u16> {
    crate::host::wide_path(p)
}

pub const ERROR_FILE_NOT_FOUND: i32 = 2;
pub const ERROR_PATH_NOT_FOUND: i32 = 3;
pub const ERROR_ACCESS_DENIED: i32 = 5;
pub const ERROR_NOT_SAME_DEVICE: i32 = 17;
pub const ERROR_SHARING_VIOLATION: i32 = 32;
pub const ERROR_LOCK_VIOLATION: i32 = 33;
pub const ERROR_HANDLE_DISK_FULL: i32 = 39;
pub const ERROR_FILE_EXISTS: i32 = 80;
pub const ERROR_DISK_FULL: i32 = 112;
pub const ERROR_DIR_NOT_EMPTY: i32 = 145;
pub const ERROR_ALREADY_EXISTS: i32 = 183;
pub const ERROR_FILENAME_EXCED_RANGE: i32 = 206;
pub const ERROR_REQUEST_ABORTED: i32 = 1235;
pub const ERROR_REQUEST_PAUSED: i32 = 3050;
pub const ERROR_DISK_QUOTA_EXCEEDED: i32 = 1295;

/// A reason people can read, without the "(os error 32)".
pub fn describe(e: &io::Error) -> String {
    let code = e.raw_os_error().unwrap_or(-1);
    let s = match code {
        ERROR_FILE_NOT_FOUND => "Not found",
        ERROR_PATH_NOT_FOUND => "The folder is not there",
        ERROR_ACCESS_DENIED => "Access denied",
        ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION => "In use by another program",
        ERROR_HANDLE_DISK_FULL | ERROR_DISK_FULL => "The disk is full",
        ERROR_DISK_QUOTA_EXCEEDED => "Over the disk quota",
        15 | 21 | 53 | 55 | 64 | 67 | 1167 => "The drive or network share went away",
        ERROR_FILE_EXISTS | ERROR_ALREADY_EXISTS => "Something of that name is already there",
        ERROR_FILENAME_EXCED_RANGE => "The name is too long for the destination",
        123 => "The destination does not accept this name",
        1920 => "The file cannot be accessed by the system",
        23 => "The disk could not read it (a bad sector?)",
        ERROR_REQUEST_ABORTED => "Cancelled",
        ERROR_REQUEST_PAUSED => "Paused",
        ERROR_DIR_NOT_EMPTY => "The folder is not empty",
        _ => "",
    };
    if !s.is_empty() {
        return s.to_string();
    }
    let t = e.to_string();
    match t.find(" (os error") {
        Some(i) => t[..i].trim_end_matches('.').to_string(),
        None => t,
    }
}

pub fn is_disk_full(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(ERROR_DISK_FULL | ERROR_HANDLE_DISK_FULL | ERROR_DISK_QUOTA_EXCEEDED))
}

pub fn is_gone(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND))
}

/// The drive or share itself went (not just one file).
pub fn is_drive_gone(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(15 | 21 | 53 | 55 | 64 | 67 | 1167))
}

pub fn exists_error(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(ERROR_FILE_EXISTS | ERROR_ALREADY_EXISTS))
}

/// A job's brakes, set from the UI thread, read by its worker between chunks.
#[derive(Debug, Default)]
pub struct Control {
    state: AtomicU8,
}

const RUN: u8 = 0;
const PAUSE: u8 = 1;
const CANCEL: u8 = 2;

impl Control {
    pub fn pause(&self) {
        let _ = self.state.compare_exchange(RUN, PAUSE, Ordering::SeqCst, Ordering::SeqCst);
    }
    /// Cancel wins over pause.
    pub fn cancel(&self) {
        self.state.store(CANCEL, Ordering::SeqCst);
    }
    pub fn paused(&self) -> bool {
        self.state.load(Ordering::SeqCst) == PAUSE
    }
    pub fn cancelled(&self) -> bool {
        self.state.load(Ordering::SeqCst) == CANCEL
    }
    /// Paused or cancelled: stop at the next chunk.
    pub fn stopping(&self) -> bool {
        self.state.load(Ordering::SeqCst) != RUN
    }
    /// What a chunk boundary should do now.
    pub fn step(&self) -> Step {
        match self.state.load(Ordering::SeqCst) {
            PAUSE => Step::Pause,
            CANCEL => Step::Cancel,
            _ => Step::Go,
        }
    }
}

/// The progress routine's answer at a chunk boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Go,
    /// Stop and keep the part (resumable).
    Pause,
    /// Stop and delete the part.
    Cancel,
    /// Stop as if the write failed with this OS error (the tests' fake writer: a disk that
    /// fills up after so many bytes). The part is deleted.
    Fail(i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Copied {
    Done,
    /// Stopped by a pause: the part is on disk; copy again with `resume` to go on.
    Paused,
    Cancelled,
}

/// How to copy one file.
#[derive(Clone, Copy, Debug, Default)]
pub struct Opts {
    /// Unbuffered I/O (big files: the copy does not push everything else out of the cache).
    pub unbuffered: bool,
    /// Carry on with a part a pause left at `dst`.
    pub resume: bool,
    /// Make the drive write the copy out of its cache once its bytes are copied
    /// (`FlushFileBuffers`, Verify jobs: what Verify then reads back is on the drive, FS7).
    pub flush: bool,
}

/// Files from this size on are copied unbuffered.
pub const BIG: u64 = 256 << 20;

struct Ctx<'a> {
    step: &'a dyn Fn(u64) -> Step,
    failed: std::cell::Cell<Option<i32>>,
    flush: bool,
}

unsafe extern "system" fn on_progress(msg: *const windows_sys::Win32::Storage::FileSystem::COPYFILE2_MESSAGE, data: *const core::ffi::c_void) -> i32 {
    use windows_sys::Win32::Storage::FileSystem::*;
    let ctx = unsafe { &*(data as *const Ctx) };
    let m = unsafe { &*msg };
    if m.Type == COPYFILE2_CALLBACK_STREAM_FINISHED && ctx.flush {
        // The copy's own handle (opened for writing, whatever its attributes). A file system
        // that cannot flush at all (ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED) is let be:
        // Verify's read past the cache still writes the cached pages out first.
        let h = unsafe { m.Info.StreamFinished.hDestinationFile };
        if unsafe { FlushFileBuffers(h) } == 0 {
            let code = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if code != 1 && code != 50 {
                ctx.failed.set(Some(code));
                return COPYFILE2_PROGRESS_CANCEL;
            }
        }
    }
    let done = match m.Type {
        COPYFILE2_CALLBACK_CHUNK_FINISHED => unsafe { m.Info.ChunkFinished.uliTotalBytesTransferred },
        COPYFILE2_CALLBACK_STREAM_STARTED | COPYFILE2_CALLBACK_POLL_CONTINUE => u64::MAX,
        _ => return COPYFILE2_PROGRESS_CONTINUE,
    };
    // Stream starts and polls only check the brakes; chunks report bytes too.
    match (ctx.step)(done) {
        Step::Go => COPYFILE2_PROGRESS_CONTINUE,
        Step::Pause => COPYFILE2_PROGRESS_PAUSE,
        Step::Cancel => COPYFILE2_PROGRESS_CANCEL,
        Step::Fail(code) => {
            ctx.failed.set(Some(code));
            COPYFILE2_PROGRESS_CANCEL
        }
    }
}

/// Copy `src` to `dst`, never over something that is there (except the paused part of this very
/// copy, with `opts.resume`). `step` hears the bytes copied of this file so far at each chunk
/// (`u64::MAX`: no new count, just the brakes) and says whether to go on.
pub fn copy_file(src: &Path, dst: &Path, opts: Opts, step: &dyn Fn(u64) -> Step) -> io::Result<Copied> {
    use windows_sys::Win32::Storage::FileSystem::*;
    let (s, d) = (wide(src), wide(dst));
    let ctx = Ctx { step, failed: std::cell::Cell::new(None), flush: opts.flush };
    let mut flags = COPY_FILE_FAIL_IF_EXISTS;
    if opts.unbuffered {
        flags |= COPY_FILE_NO_BUFFERING;
    }
    if opts.resume {
        flags |= COPY_FILE_RESUME_FROM_PAUSE;
    }
    let params = COPYFILE2_EXTENDED_PARAMETERS {
        dwSize: std::mem::size_of::<COPYFILE2_EXTENDED_PARAMETERS>() as u32,
        dwCopyFlags: flags,
        pfCancel: std::ptr::null_mut(),
        pProgressRoutine: Some(on_progress),
        pvCallbackContext: &ctx as *const Ctx as *mut core::ffi::c_void,
    };
    let hr = unsafe { CopyFile2(s.as_ptr(), d.as_ptr(), &params) };
    if let Some(code) = ctx.failed.get() {
        return Err(io::Error::from_raw_os_error(code));
    }
    if hr >= 0 {
        return Ok(Copied::Done);
    }
    let code = if (hr as u32 & 0xFFFF_0000) == 0x8007_0000 { (hr as u32 & 0xFFFF) as i32 } else { hr };
    match code {
        ERROR_REQUEST_PAUSED => Ok(Copied::Paused),
        ERROR_REQUEST_ABORTED => Ok(Copied::Cancelled),
        c => Err(io::Error::from_raw_os_error(c)),
    }
}

/// Rename or move within one volume, never replacing what is at `to`. A folder moves whole.
/// Across volumes it fails with `ERROR_NOT_SAME_DEVICE` (nothing is copied).
pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let (f, t) = (wide(from), wide(to));
    if unsafe { MoveFileExW(f.as_ptr(), t.as_ptr(), 0) } == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Put `from` in the place of the file at `to` (one step on the volume: the old file is there
/// until the new one is). A read-only old file is replaced too; if the replace fails it keeps its
/// read-only flag.
pub fn replace(from: &Path, to: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MoveFileExW};
    let (f, t) = (wide(from), wide(to));
    let go = || if unsafe { MoveFileExW(f.as_ptr(), t.as_ptr(), MOVEFILE_REPLACE_EXISTING) } == 0 { Err(io::Error::last_os_error()) } else { Ok(()) };
    match go() {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) && readonly(to) => {
            set_readonly(to, false);
            let r = go();
            if r.is_err() {
                set_readonly(to, true);
            }
            r
        }
        r => r,
    }
}

fn readonly(p: &Path) -> bool {
    std::fs::symlink_metadata(p).map(|m| m.permissions().readonly()).unwrap_or(false)
}

fn set_readonly(p: &Path, on: bool) {
    if let Ok(m) = std::fs::symlink_metadata(p) {
        let mut perm = m.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(on);
        let _ = std::fs::set_permissions(p, perm);
    }
}

/// Delete one file (read-only too). A link is deleted itself, never what it points at.
pub fn remove_file(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied && readonly(path) => {
            set_readonly(path, false);
            let r = std::fs::remove_file(path);
            if r.is_err() {
                set_readonly(path, true);
            }
            r
        }
        r => r,
    }
}

/// Delete a folder only if it is empty (never anything in it).
pub fn remove_empty_dir(path: &Path) -> io::Result<()> {
    std::fs::remove_dir(path)
}

/// Give a new folder its source folder's hidden, system and not-indexed attributes (a hidden
/// `.git` stays hidden). Best effort.
pub fn copy_dir_attributes(src: &Path, dst: &Path) {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NOT_CONTENT_INDEXED, FILE_ATTRIBUTE_SYSTEM, GetFileAttributesW, INVALID_FILE_ATTRIBUTES, SetFileAttributesW,
    };
    let keep = FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;
    let (s, d) = (wide(src), wide(dst));
    unsafe {
        let a = GetFileAttributesW(s.as_ptr());
        if a != INVALID_FILE_ATTRIBUTES && a & keep != 0 {
            let cur = GetFileAttributesW(d.as_ptr());
            if cur != INVALID_FILE_ATTRIBUTES {
                SetFileAttributesW(d.as_ptr(), cur | (a & keep));
            }
        }
    }
}

/// What is at a path, without following a link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub size: u64,
    pub mtime: u64,
    pub dir: bool,
    pub link: bool,
}

pub fn stat(path: &Path) -> io::Result<Stat> {
    use std::os::windows::fs::MetadataExt;
    let m = std::fs::symlink_metadata(path)?;
    let link = m.file_type().is_symlink();
    Ok(Stat { size: if m.is_dir() { 0 } else { m.file_size() }, mtime: m.last_write_time(), dir: m.is_dir() || (link && m.file_attributes() & 0x10 != 0), link })
}

/// Which file a path is (volume serial and file id), without following a link. Two paths that
/// name the same file (a share under two names, an 8.3 name, a hard link) have the same one.
pub fn file_id(path: &Path) -> Option<(u64, [u8; 16])> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx, OPEN_EXISTING,
    };
    let w = wide(path);
    unsafe {
        let h = CreateFileW(
            w.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut info: FILE_ID_INFO = std::mem::zeroed();
        let out = if GetFileInformationByHandleEx(h, FileIdInfo, (&mut info as *mut FILE_ID_INFO).cast(), std::mem::size_of::<FILE_ID_INFO>() as u32) != 0
            && info.FileId.Identifier != [0u8; 16]
        {
            Some((info.VolumeSerialNumber, info.FileId.Identifier))
        } else {
            // FAT and some shares: the older 64-bit index.
            let mut b: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
            if GetFileInformationByHandle(h, &mut b) != 0 {
                let mut id = [0u8; 16];
                id[..4].copy_from_slice(&b.nFileIndexLow.to_le_bytes());
                id[4..8].copy_from_slice(&b.nFileIndexHigh.to_le_bytes());
                Some((b.dwVolumeSerialNumber as u64, id))
            } else {
                None
            }
        };
        CloseHandle(h);
        // Some servers answer with zeros: unknown, never "the same".
        out.filter(|(serial, id)| *serial != 0 && *id != [0u8; 16])
    }
}

/// Free bytes for this user on the volume of `dir` (an existing folder).
pub fn free_space(dir: &Path) -> io::Result<u64> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut w = wide(dir);
    // The API wants a folder with a trailing separator.
    if w.len() >= 2 && w[w.len() - 2] != b'\\' as u16 {
        w.insert(w.len() - 1, b'\\' as u16);
    }
    let mut free = 0u64;
    if unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(free)
}

/// Delete a folder tree we made ourselves (the tests' temp folders). Links inside are removed,
/// never followed.
pub fn remove_tree(path: &Path) -> io::Result<()> {
    let md = std::fs::symlink_metadata(path)?;
    if md.file_type().is_symlink() {
        return std::fs::remove_dir(path).or_else(|_| remove_file(path));
    }
    if !md.is_dir() {
        return remove_file(path);
    }
    for e in std::fs::read_dir(path)? {
        let e = e?;
        let p = e.path();
        let ft = e.file_type()?;
        if ft.is_symlink() {
            std::fs::remove_dir(&p).or_else(|_| remove_file(&p))?;
        } else if ft.is_dir() {
            remove_tree(&p)?;
        } else {
            remove_file(&p)?;
        }
    }
    std::fs::remove_dir(path)
}

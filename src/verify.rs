//! Checking a copy against its original. Always by size and modified time (the copy carries the
//! original's time, so a difference means the original changed meanwhile, or the copy is not
//! whole); with Verify on, both files are also read back and hashed with SHA-256 (Windows' own
//! CNG implementation, streamed in 4 MB chunks: no crate, hardware-accelerated where the CPU
//! has it). The copy is read past Windows' file cache at every size, so Verify reads what is on
//! the drive (FS7); the original only when big, as it was copied.

use crate::copy::{BIG, Control, Stat};
use std::io::{self, Read};
use std::path::Path;

pub type Digest = [u8; 32];

/// Size and time agree.
pub fn quick(a: &Stat, b: &Stat) -> bool {
    a.size == b.size && a.mtime == b.mtime
}

struct Hasher(windows_sys::Win32::Security::Cryptography::BCRYPT_HASH_HANDLE);

impl Hasher {
    fn new() -> io::Result<Hasher> {
        use windows_sys::Win32::Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptCreateHash};
        let mut h = std::ptr::null_mut();
        let st = unsafe { BCryptCreateHash(BCRYPT_SHA256_ALG_HANDLE, &mut h, std::ptr::null_mut(), 0, std::ptr::null(), 0, 0) };
        if st < 0 {
            return Err(io::Error::other(format!("SHA-256 is not available (NTSTATUS {st:#x})")));
        }
        Ok(Hasher(h))
    }

    fn update(&mut self, data: &[u8]) -> io::Result<()> {
        use windows_sys::Win32::Security::Cryptography::BCryptHashData;
        for chunk in data.chunks(u32::MAX as usize) {
            let st = unsafe { BCryptHashData(self.0, chunk.as_ptr(), chunk.len() as u32, 0) };
            if st < 0 {
                return Err(io::Error::other(format!("hashing failed (NTSTATUS {st:#x})")));
            }
        }
        Ok(())
    }

    fn finish(self) -> io::Result<Digest> {
        use windows_sys::Win32::Security::Cryptography::BCryptFinishHash;
        let mut out = [0u8; 32];
        let st = unsafe { BCryptFinishHash(self.0, out.as_mut_ptr(), 32, 0) };
        if st < 0 {
            return Err(io::Error::other(format!("hashing failed (NTSTATUS {st:#x})")));
        }
        Ok(out)
    }
}

impl Drop for Hasher {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Security::Cryptography::BCryptDestroyHash(self.0) };
    }
}

/// SHA-256 of bytes in memory (tests, and the known answers).
pub fn sha256(data: &[u8]) -> io::Result<Digest> {
    let mut h = Hasher::new()?;
    h.update(data)?;
    h.finish()
}

/// A buffer aligned for unbuffered reads (sector and page multiples).
struct Aligned {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}

impl Aligned {
    fn new(len: usize) -> Aligned {
        let layout = std::alloc::Layout::from_size_align(len, 4096).unwrap();
        let ptr = unsafe { std::alloc::alloc(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Aligned { ptr, layout }
    }
    fn buf(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}

impl Drop for Aligned {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr, self.layout) };
    }
}

/// Read size: big enough that an unbuffered read is not one disk round trip per megabyte.
const CHUNK: usize = 4 << 20;

/// Open a file to be hashed. `from_disk` (the copy's side, FS7): always without Windows' file
/// cache (`FILE_FLAG_NO_BUFFERING`), so the bytes come from the drive, not from the memory the
/// copy just wrote them through; the file system writes any of the file's pages still waiting in
/// the cache out to the drive first, then reads the drive. Otherwise (the original's side) only
/// big files go past the cache, as they were copied.
fn open_for_hash(path: &Path, from_disk: bool) -> io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_NO_BUFFERING, FILE_FLAG_SEQUENTIAL_SCAN, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};
    let direct = from_disk || std::fs::metadata(path)?.len() >= BIG;
    let flags = if direct { FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN } else { FILE_FLAG_SEQUENTIAL_SCAN };
    std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).custom_flags(flags).open(path)
}

/// Stream a file through SHA-256. `from_disk`: read past Windows' cache at every size (the
/// copy's side, `open_for_hash`). `Ok(None)`: stopped by the brakes (pause or cancel) before
/// the end. `read` hears the bytes read so far.
pub fn hash_file(path: &Path, from_disk: bool, ctl: &Control, read: &dyn Fn(u64)) -> io::Result<Option<Digest>> {
    // Reads past the cache go in sector multiples into a page-aligned buffer (`Aligned`, CHUNK);
    // only the last one comes back short.
    let mut f = open_for_hash(path, from_disk)?;
    let mut h = Hasher::new()?;
    let mut buf = Aligned::new(CHUNK);
    let mut total = 0u64;
    loop {
        if ctl.stopping() {
            return Ok(None);
        }
        let n = loop {
            match f.read(buf.buf()) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                r => break r?,
            }
        };
        if n == 0 {
            break;
        }
        h.update(&buf.buf()[..n])?;
        total += n as u64;
        read(total);
    }
    h.finish().map(Some)
}

pub fn hex(d: &Digest) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

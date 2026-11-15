//! Files copied or cut in Explorer, from the clipboard: `CF_HDROP` (the list) and Explorer's
//! "Preferred DropEffect" (cut or copy). Read only when the user presses Paste, on a worker
//! thread (a program that renders the clipboard late can keep `GetClipboardData` waiting).
//! The list itself is parsed here from the clipboard's bytes (`DROPFILES`), so the parsing is
//! tested without touching the real clipboard.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

/// What Paste found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Clip {
    pub files: Vec<PathBuf>,
    /// Cut in Explorer (Ctrl+X): the paste is a move.
    pub cut: bool,
}

const CF_HDROP: u32 = 15;
const DROPEFFECT_MOVE: u32 = 2;

/// The file list in a `DROPFILES` block: a 20-byte header (offset of the list, a point, two
/// flags, the last one "wide"), then NUL-separated names ending in a double NUL.
pub fn parse_dropfiles(b: &[u8]) -> Vec<PathBuf> {
    if b.len() < 20 {
        return Vec::new();
    }
    let off = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
    let wide = u32::from_le_bytes([b[16], b[17], b[18], b[19]]) != 0;
    if off >= b.len() {
        return Vec::new();
    }
    let rest = &b[off..];
    let mut out = Vec::new();
    if wide {
        let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        for name in units.split(|&u| u == 0) {
            if name.is_empty() {
                break;
            }
            out.push(PathBuf::from(OsString::from_wide(name)));
        }
    } else {
        for name in rest.split(|&c| c == 0) {
            if name.is_empty() {
                break;
            }
            out.push(PathBuf::from(OsString::from_wide(&ansi_to_wide(name))));
        }
    }
    out
}

/// An old program's ANSI names, in this system's code page.
fn ansi_to_wide(s: &[u8]) -> Vec<u16> {
    use windows_sys::Win32::Globalization::{CP_ACP, MultiByteToWideChar};
    let n = unsafe { MultiByteToWideChar(CP_ACP, 0, s.as_ptr(), s.len() as i32, std::ptr::null_mut(), 0) };
    if n <= 0 {
        return s.iter().map(|&c| c as u16).collect();
    }
    let mut w = vec![0u16; n as usize];
    unsafe { MultiByteToWideChar(CP_ACP, 0, s.as_ptr(), s.len() as i32, w.as_mut_ptr(), n) };
    w
}

/// Explorer's drop effect: cut when it says move.
pub fn parse_effect(b: &[u8]) -> bool {
    b.len() >= 4 && u32::from_le_bytes([b[0], b[1], b[2], b[3]]) & DROPEFFECT_MOVE != 0
}

/// The clipboard's files, if it holds any (None: something else is on it).
pub fn read() -> Result<Option<Clip>, String> {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW};
    unsafe {
        // Another program (a clipboard manager) may hold it for a moment: a few short tries.
        let mut open = OpenClipboard(std::ptr::null_mut()) != 0;
        for _ in 0..10 {
            if open {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            open = OpenClipboard(std::ptr::null_mut()) != 0;
        }
        if !open {
            return Err("The clipboard is busy (another program holds it). Try Paste again.".into());
        }
        let out = (|| {
            if IsClipboardFormatAvailable(CF_HDROP) == 0 {
                return None;
            }
            let files = parse_dropfiles(&global_bytes(GetClipboardData(CF_HDROP))?);
            let fmt = RegisterClipboardFormatW(crate::host::wide("Preferred DropEffect").as_ptr());
            let cut = fmt != 0 && IsClipboardFormatAvailable(fmt) != 0 && global_bytes(GetClipboardData(fmt)).is_some_and(|b| parse_effect(&b));
            Some(Clip { files, cut })
        })();
        CloseClipboard();
        Ok(out.filter(|c| !c.files.is_empty()))
    }
}

/// A clipboard handle's bytes.
unsafe fn global_bytes(h: windows_sys::Win32::Foundation::HANDLE) -> Option<Vec<u8>> {
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    if h.is_null() {
        return None;
    }
    unsafe {
        let size = GlobalSize(h as _);
        let p = GlobalLock(h as _) as *const u8;
        if p.is_null() {
            return None;
        }
        let v = std::slice::from_raw_parts(p, size).to_vec();
        GlobalUnlock(h as _);
        Some(v)
    }
}

//! The queue's progress on the taskbar button (`ITaskbarList3`, through its COM vtable:
//! windows-sys carries no COM interfaces). On the UI thread only.

use windows_sys::core::{GUID, HRESULT};

type Raw = *mut core::ffi::c_void;

#[repr(C)]
struct Vtbl {
    query_interface: usize,
    add_ref: usize,
    release: unsafe extern "system" fn(Raw) -> u32,
    hr_init: unsafe extern "system" fn(Raw) -> HRESULT,
    add_tab: usize,
    delete_tab: usize,
    activate_tab: usize,
    set_active_alt: usize,
    mark_fullscreen_window: usize,
    set_progress_value: unsafe extern "system" fn(Raw, isize, u64, u64) -> HRESULT,
    set_progress_state: unsafe extern "system" fn(Raw, isize, u32) -> HRESULT,
}

const CLSID_TASKBARLIST: GUID = GUID::from_u128(0x56fdf344_fd6d_11d0_958a_006097c9a090);
const IID_ITASKBARLIST3: GUID = GUID::from_u128(0xea1afb91_9e28_4b86_90e9_9e9f8a5eefaf);

/// What the taskbar button shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bar {
    /// Nothing to show.
    None,
    /// Working, amount not known yet (counting).
    Busy,
    /// Done of total (blue).
    Normal,
    /// Everything is paused (yellow).
    Paused,
    /// Something needs the user (red).
    Error,
}

impl Bar {
    fn flags(self) -> u32 {
        match self {
            Bar::None => 0,
            Bar::Busy => 1,
            Bar::Normal => 2,
            Bar::Error => 4,
            Bar::Paused => 8,
        }
    }
}

pub struct Taskbar {
    list: Raw,
}

impl Taskbar {
    pub fn new() -> Option<Taskbar> {
        use windows_sys::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
        unsafe {
            CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
            let mut p: Raw = std::ptr::null_mut();
            if CoCreateInstance(&CLSID_TASKBARLIST, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID_ITASKBARLIST3, &mut p) < 0 || p.is_null() {
                return None;
            }
            let v = &**(p as *mut *const Vtbl);
            if (v.hr_init)(p) < 0 {
                (v.release)(p);
                return None;
            }
            Some(Taskbar { list: p })
        }
    }

    /// Show `bar` on the window's button (`done` of `total` for Normal, Paused and Error).
    pub fn set(&self, hwnd: isize, bar: Bar, done: u64, total: u64) {
        unsafe {
            let v = &**(self.list as *mut *const Vtbl);
            (v.set_progress_state)(self.list, hwnd, bar.flags());
            if matches!(bar, Bar::Normal | Bar::Paused | Bar::Error) && total > 0 {
                (v.set_progress_value)(self.list, hwnd, done.min(total), total);
            }
        }
    }
}

impl Drop for Taskbar {
    fn drop(&mut self) {
        unsafe {
            let v = &**(self.list as *mut *const Vtbl);
            (v.release)(self.list);
        }
    }
}

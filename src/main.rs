// VibeCopy: one process, no browser engine, the software renderer.
#![windows_subsystem = "windows"]

use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
use slint::{ComponentHandle, Timer};
use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use vibecopy::clipboard::Clip;
use vibecopy::host;
use vibecopy::controller::{App, DropMode, Env, Shell};
use vibecopy::instance::{self, Start};
use vibecopy::taskbar::{Bar, Taskbar};

/// The real release-list fetcher (WinHTTP), the exe's alone: it is in no test build, not the
/// lib's, not the integration tests', not this exe's own unit tests', so no test can go online
/// (review RAB-2; tests/pe.rs checks this line).
#[cfg(not(test))]
mod net;


thread_local! {
    /// The controller, for the wake-ups posted from other threads.
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

fn with_app(f: impl FnOnce(&App)) {
    let app = APP.with(|a| a.borrow().clone());
    if let Some(a) = app {
        f(&a);
    }
}

#[derive(Default)]
struct WindowsShell {
    taskbar: RefCell<Option<Taskbar>>,
    tried: std::cell::Cell<bool>,
}

impl Shell for WindowsShell {
    fn pick_folder(&self, owner: Option<isize>, start: Option<PathBuf>, done: Box<dyn FnOnce(Option<PathBuf>) + Send>) {
        vibecopy::dialogs::pick_folder(owner, "Where do they go?", start, done);
    }

    fn reveal(&self, path: &Path) -> Result<(), String> {
        vibecopy::host::reveal(path, false)
    }

    fn read_clipboard(&self, done: Box<dyn FnOnce(Result<Option<Clip>, String>) + Send>) {
        std::thread::spawn(move || done(vibecopy::clipboard::read()));
    }

    fn taskbar(&self, owner: Option<isize>, bar: Bar, done: u64, total: u64) {
        let Some(hwnd) = owner else { return };
        if self.taskbar.borrow().is_none() && !self.tried.get() {
            self.tried.set(true);
            *self.taskbar.borrow_mut() = Taskbar::new();
        }
        if let Some(t) = self.taskbar.borrow().as_ref() {
            t.set(hwnd, bar, done, total);
        }
    }

    fn quit(&self) {
        let _ = slint::quit_event_loop();
    }
}

fn main() -> Result<(), slint::PlatformError> {
    // DLL hardening, first of all (VibeLink LINK-1): every DLL loaded from here on comes from
    // System32, never from the exe's own folder (the per-user install folder, which an elevated
    // VibeCopy runs from too). The static imports are covered by the link flag
    // /DEPENDENTLOADFLAG:0x800 (build.rs).
    // SAFETY: a process-wide setting with a valid flag; nothing has been loaded by name yet.
    unsafe {
        use windows_sys::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories};
        SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32);
    }
    // No Windows "No Disk" box: a file call on an empty card reader or drive gets its error back
    // instead of a modal box (review fix-popup, RX1). tests/pe.rs checks this block comes next.
    // SAFETY: a process-wide setting with a valid flag, the bits already set kept.
    unsafe {
        use windows_sys::Win32::System::Diagnostics::Debug::{GetErrorMode, SEM_FAILCRITICALERRORS, SetErrorMode};
        SetErrorMode(GetErrorMode() | SEM_FAILCRITICALERRORS);
    }
    // The command line (`cli`): an option VibeCopy does not take ends the process before any
    // window, file or second-start message (exit code 2); any other mistake (`--to` without
    // files) is said in the window.
    let raw: Vec<OsString> = std::env::args_os().skip(1).collect();
    if !vibecopy::cli::known_options(&raw) {
        std::process::exit(2);
    }
    #[allow(unused_mut)]
    let mut paths = vibecopy::host::paths();
    #[allow(unused_mut)]
    let mut name = instance::NAME.to_string();
    // Live checks and measurements only: the app's folders (and its single-instance name) in a
    // place of the developer's choosing, so nothing touches the real profile or a real
    // VibeCopy. Never in a shipped build.
    #[cfg(feature = "dev-hooks")]
    if let Some(home) = std::env::var_os("VIBECOPY_DEV_HOME").map(PathBuf::from) {
        paths = vibecopy::host::Paths { config: home.join("config"), local_data: home.join("local") };
        name = format!("{name}.dev");
    }
    // The probe hands back the value it was asked about, and `main` goes on with that one only: the
    // old binding is gone (`dev_probe`, review RRF-2).
    #[cfg(feature = "dev-hooks")]
    let paths = dev_probe(paths);
    let inbox = paths.local_data.join("inbox");
    let cwd = std::env::current_dir().unwrap_or_default();
    let args: Vec<OsString> = vibecopy::cli::resolve(&raw, &cwd);
    let mut inst = match instance::claim(&name, &inbox, &args) {
        // At once, with the code the window reads if it looks after this exit (RF2-2).
        Start::Second(code) => std::process::exit(code),
        Start::First(i) => i,
    };

    let prefs = vibecopy::prefs::Store::new(paths.config.join("prefs.json"));
    let ui = vibecopy::window(prefs.clone())?;
    // The one place that names the real release-list fetcher (tests/pe.rs checks it): every test
    // hands the controller a fake, so no test goes online. This exe's unit-test build has no
    // `net` and never runs `main`: it gets a stand-in that fetches nothing.
    #[cfg(not(test))]
    let fetch = std::sync::Arc::new(net::fetch_release_list);
    #[cfg(test)]
    let fetch = std::sync::Arc::new(|_: &str| vibecopy::check::Fetched::Failed("a test build has no fetcher".into()));
    let env = Env {
        local: paths.local_data.clone(),
        disk: Arc::new(vibecopy::disk::Real),
        shell: Rc::new(WindowsShell::default()),
        wake: Arc::new(|| {
            let _ = slint::invoke_from_event_loop(|| with_app(|a| a.poll()));
        }),
        clock: {
            let t0 = std::time::Instant::now();
            Rc::new(move || t0.elapsed().as_secs_f64())
        },
        about: vibecopy::controller::About::system(fetch),
    };
    let app = App::new(ui.clone_strong(), env, prefs.clone());
    APP.with(|a| *a.borrow_mut() = Some(app.clone()));
    app.boot();
    app.open_args(&args);
    // A second start that raced this one's start-up left its files already.
    app.inbox(&instance::pending(&inbox));

    // A second start (Send To, --to, a double click): its files, and this window to the front.
    // Taken only by a window that is not quitting (RI5-4): otherwise, or once the event loop has
    // ended, the message stays and its start runs on its own. A start whose message the window
    // did not take listens as the first one only once it owns the queue (RI6-2, decided inside
    // `on_signal`, RI7-2); beside a window that still has it, it runs deaf, as a guest.
    {
        let inbox = inbox.clone();
        inst.on_signal(!app.is_guest(), move || {
            // Read here, on the signal's thread; only the result goes to the window.
            let msgs = instance::pending(&inbox);
            let _ = slint::invoke_from_event_loop(move || {
                with_app(|a| {
                    if a.inbox(&msgs) {
                        a.ui.window().with_winit_window(|w| {
                            w.set_minimized(false);
                            w.focus_window();
                        });
                    }
                })
            });
        });
    }

    // Files dragged in from Explorer: highlight while they hover, take them on drop, in the mode
    // of the dialog they were dragged into (RRF2-1).
    let dropped: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
    // The window opens at its preferred size; on a small or highly scaled screen that runs past
    // the taskbar, so the first window event once it is on screen fits it into the monitor's work
    // area (X15A11Y-3), once (a start minimized waits until the window is restored).
    let fitted = std::cell::Cell::new(false);
    {
        let (weak, dropped) = (ui.as_weak(), dropped.clone());
        ui.window().on_winit_window_event(move |window, event| {
            if !fitted.get() && window.with_winit_window(hwnd_of).flatten().and_then(host::fit_to_work_area).is_some() {
                fitted.set(true);
            }
            match event {
                WindowEvent::HoveredFile(_) => {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_drop_hover(true);
                    }
                    with_app(|a| a.drag_entered());
                }
                WindowEvent::HoveredFileCancelled => {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_drop_hover(false);
                    }
                    with_app(|a| a.drag_left());
                }
                WindowEvent::DroppedFile(path) => {
                    // One event per file: collect them, act once the batch is in, in the mode the
                    // drop came in with (RRF3-1).
                    let first = dropped.borrow().is_empty();
                    dropped.borrow_mut().push(path.clone());
                    if first {
                        let mut mode = DropMode::default();
                        with_app(|a| mode = a.drop_arrived());
                        let (weak, dropped) = (weak.clone(), dropped.clone());
                        Timer::single_shot(Duration::from_millis(0), move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.set_drop_hover(false);
                            }
                            let files = std::mem::take(&mut *dropped.borrow_mut());
                            with_app(|a| a.dropped(files, mode));
                        });
                    }
                }
                _ => {}
            }
            EventResult::Propagate
        });
    }

    ui.show()?;
    let ran = slint::run_event_loop_until_quit();
    let _ = ui.hide();
    APP.with(|a| *a.borrow_mut() = None);
    // Let go in the reverse order of taking (RI4-1): the queue written and its lock let go
    // first, then the instance event.
    app.finish();
    drop(inst);
    ran
}

/// The window's Win32 handle, for `host::fit_to_work_area`.
fn hwnd_of(w: &slint::winit_030::winit::window::Window) -> Option<isize> {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match w.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// Live checks only (review RRE-5, RRF-2): with VIBECOPY_DEV_PROBE set, `main` writes the
/// app's data folders, one a line on stdout, and ends (exit 0) right after the dev home, before
/// any of them is used and before any window. Else it hands back the value it was given,
/// unchanged, and `main` goes on with it (`let paths = dev_probe(paths);`), so the folders the
/// probe names are the ones `main` uses. tests/pe.rs asks this before it starts the exe for real
/// (`run_exe`): every folder must be inside the scratch dev home it gave, so an override that is
/// gone, compiled out or too late fails there, by what the exe does.
#[cfg(feature = "dev-hooks")]
fn dev_probe(paths: vibecopy::host::Paths) -> vibecopy::host::Paths {
    use std::io::Write;
    if std::env::var_os("VIBECOPY_DEV_PROBE").is_some() {
        let mut out = std::io::stdout().lock();
        for f in [&paths.config, &paths.local_data] {
            let _ = writeln!(out, "{}", f.display());
        }
        let _ = out.flush();
        std::process::exit(0);
    }
    paths
}

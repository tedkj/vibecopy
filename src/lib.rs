//! VibeCopy: copies that finish and can be paused, each read back and compared if Verify is on. A VibeWare app: Rust and Slint on the software renderer,
//! one small exe, no runtime beside it.

pub mod check;
pub mod cli;
pub mod clipboard;
pub mod conflict;
pub mod controller;
pub mod copy;
pub mod dialogs;
pub mod disk;
pub mod format;
pub mod host;
pub mod instance;
pub mod job;
pub mod move_;
pub mod paths;
pub mod prefs;
pub mod queue;
pub mod run;
pub mod scan;
pub mod taskbar;
pub mod trust;
pub mod verify;

use serde::{Deserialize, Serialize};
use slint::ComponentHandle;

slint::include_modules!();

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Recent destinations kept.
pub const RECENT: usize = 5;

/// What the window remembers between runs (prefs.json in %APPDATA%\com.tedkj.vibecopy).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// "dark" or "light" once the user has picked one; until then the system's choice.
    pub theme: Option<String>,
    /// Destinations, newest first (shown as typed: these are only ever text in a field).
    pub recent: Vec<String>,
    /// The new job dialog starts with the last job's Verify and conflict rule.
    pub verify: bool,
    pub policy: i32,
}

impl Prefs {
    pub fn dark(&self) -> bool {
        match self.theme.as_deref() {
            Some("dark") => true,
            Some("light") => false,
            _ => host::system_dark(),
        }
    }
}

/// The window with its basics wired: theme, About, copying. The app's own controller adds the rest.
pub fn window(prefs: prefs::Store<Prefs>) -> Result<AppWindow, slint::PlatformError> {
    let ui = AppWindow::new()?;
    let p = prefs.load();
    ui.global::<Theme>().set_dark(p.dark());
    ui.set_version(VERSION.into());
    // The theme toggle is the controller's (`wire`): its change goes through `change_prefs`, which
    // tells a prefs.json kept aside (C44). No second handler here to fall back on (R2-COPY-4).
    ui.on_copy_text(|t| host::copy_text(&t).is_ok());
    ui.global::<Fmt>().on_elide(|s, n| format::elide_middle(&s, n.max(1) as usize).into());
    Ok(ui)
}

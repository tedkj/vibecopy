# VibeCopy

Copies and moves that finish: a queue you can pause and resume, with every file checked if you
like.

Drop files on the window, paste files you copied in Explorer, or send them from Explorer's
Send to menu, then pick where they go. Each job waits in a queue and you can pause it, even in the
middle of a big file, and pick it up later, after a restart too. A file that's already there gets
a sensible answer: ask, skip, overwrite, overwrite if older, or keep both. A file that fails gets a
plain reason, and the rest of the job carries on; Retry failed tries those again at the end.

Every copy is written under a temporary name first and renamed into place once it is complete,
so a crash never leaves half a file under the real name and a failed overwrite keeps the old file.
Each copy's size and modified time are compared with its original. Turn on Verify and every copy
is also read back from the drive and compared with its original (SHA-256). A move deletes an
original only once its copy is in place and checked.

It is written in Rust with a native Slint interface, with no browser engine inside, and uses
about 7 MB of memory when idle.

## What it does and doesn't do

- Copies and moves files and folders between local drives, USB sticks and network shares you
  can already open in Explorer. A move on the same drive is a rename.
- Runs one job at a time per destination drive (two copies onto one disk only fight each other),
  and jobs to different drives side by side.
- Shows the overall progress on its taskbar button.
- Can be started from the command line: `VibeCopy.exe --to <destination> <files or folders...>`.
- Doesn't replace Explorer's own copy, doesn't sync folders and doesn't schedule anything.

## What it touches on your PC

- Its settings in `%APPDATA%\com.tedkj.vibecopy` and the queue (while anything in it is
  unfinished) in `%LOCALAPPDATA%\com.tedkj.vibecopy`.
- The installer adds a Start menu shortcut and a VibeCopy entry in Explorer's Send to menu.
  Uninstalling removes the app and both shortcuts; your settings stay, so a reinstall finds them.

## Privacy

VibeCopy sends nothing. The one exception is About's "Check this install", and only when you
press it: it fetches the signed list of published releases from getvibeware.com to check that
your files match the release.

## Download

Get the installer from [Releases](https://github.com/tedkj/vibecopy/releases), or from
[getvibeware.com](https://getvibeware.com). It installs for your user only, without admin rights.
The installer, VibeCopy.exe and its uninstaller are code signed by "Open Source Developer Ted
Kenneth Johansen" (right-click › Properties › Digital Signatures). You can also compare the
installer's SHA-256 with the one on the release page:

```powershell
Get-FileHash .\VibeCopy_0.1.0-preview_x64-setup.exe
```

## Building from source

You need a current stable Rust toolchain on Windows (the MSVC target).

```sh
cargo build --release --locked
```

The app is then at `target\release\vibecopy.exe`.

### Building the installer

`installer\build.ps1` builds the app and its installer,
`target\release\bundle\VibeCopy_<version>_x64-setup.exe`. It needs the official NSIS 3.13:
download `nsis-3.13.zip` from SourceForge (SHA-256
`ba63dffc4410ee89193e1cb5a41989991bd77c61068da17e3156d136b7b0b3d8`) and unpack it to
`_tools\nsis-3.13` in the folder that holds this repository (`..\_tools\nsis-3.13`). The script
checks that whole folder against a pinned hash before it runs anything from it.
`installer\notices.ps1` writes `THIRD-PARTY-NOTICES.txt` for each build (every crate the app
links, with its licence text), and the installer puts it and `LICENSE` next to the app.

`vendor/` holds Slint's `i-slint-core` 1.18.1 with one small repaint fix; `vendor/README.md` says
what changed and why.

## License

[AGPL-3.0](LICENSE) (AGPL-3.0-only). Use it, change it, share it. If you hand out a changed
version, or run one as a service for other people, you share your changes under the same license.
Slint is used under its GPLv3 option.

Made in Norway by Ted K. Johansen.

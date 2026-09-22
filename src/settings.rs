//! Saved options and the Windows sign-in startup entry.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const RUN_VALUE: &str = "strix-lc-screen";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub close_to_tray: bool,
    pub start_with_windows: bool,
    /// Sign-in launches the app with `--tray`, so the window stays hidden.
    pub start_in_tray: bool,
    pub resume_last: bool,
    pub last_path: Option<String>,
    pub zoom: f32,
    pub pan_x: f32,
    pub pan_y: f32,
    pub speed: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            close_to_tray: true,
            start_with_windows: false,
            start_in_tray: true,
            resume_last: true,
            last_path: None,
            zoom: 1.0,
            pan_x: 0.0,
            pan_y: 0.0,
            speed: 1.0,
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let path = settings_path();
        let Ok(bytes) = std::fs::read(&path) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = settings_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(&path, bytes).with_context(|| format!("could not write {}", path.display()))
    }
}

pub fn settings_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("strix-lc-screen").join("settings.json")
}

pub fn set_run_at_logon(enable: bool, hidden: bool) -> Result<()> {
    #[cfg(windows)]
    {
        use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
        use winreg::RegKey;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (run, _) = hkcu
            .create_subkey_with_flags(
                "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                KEY_SET_VALUE,
            )
            .context("could not open the Windows startup list")?;
        if enable {
            let exe = std::env::current_exe().context("could not find this program's path")?;
            run.set_value(RUN_VALUE, &startup_command(&exe, hidden))
                .context("could not add this program to Windows startup")?;
        } else if let Err(error) = run.delete_value(RUN_VALUE) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error).context("could not remove this program from Windows startup");
            }
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (enable, hidden);
        anyhow::bail!("starting at sign-in is only available on Windows");
    }
}

fn startup_command(exe: &Path, hidden: bool) -> String {
    let path = exe.display().to_string();
    let quoted = format!("\"{path}\"");
    if hidden {
        format!("{quoted} --tray")
    } else {
        quoted
    }
}

/// Keeps a single window. A second launch shows the one already running.
pub fn claim_single_instance() -> bool {
    #[cfg(windows)]
    {
        claim_single_instance_windows()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

#[cfg(windows)]
fn claim_single_instance_windows() -> bool {
    use std::ffi::c_void;
    unsafe extern "system" {
        fn CreateMutexW(attrs: *mut c_void, initial_owner: i32, name: *const u16) -> *mut c_void;
        fn GetLastError() -> u32;
        fn FindWindowW(class: *const u16, window: *const u16) -> *mut c_void;
        fn ShowWindow(hwnd: *mut c_void, cmd: i32) -> i32;
        fn SetForegroundWindow(hwnd: *mut c_void) -> i32;
        fn IsIconic(hwnd: *mut c_void) -> i32;
    }
    const ERROR_ALREADY_EXISTS: u32 = 183;
    const SW_SHOW: i32 = 5;
    const SW_RESTORE: i32 = 9;
    let name: Vec<u16> = "Local\\strix-lc-screen"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let handle = CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr());
        if handle.is_null() {
            return true;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let title: Vec<u16> = "Pump screen"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
            if !hwnd.is_null() {
                let cmd = if IsIconic(hwnd) != 0 { SW_RESTORE } else { SW_SHOW };
                ShowWindow(hwnd, cmd);
                SetForegroundWindow(hwnd);
            }
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_startup_adds_the_tray_flag() {
        let command = startup_command(Path::new(r"C:\Pump\strix-lc-screen.exe"), true);
        assert_eq!(command, r#""C:\Pump\strix-lc-screen.exe" --tray"#);
    }
}

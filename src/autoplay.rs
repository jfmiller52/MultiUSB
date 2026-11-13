//! Suppress Explorer AutoPlay while disks are wiped, formatted and filled.
//!
//! Creating a RAW partition (or remove/reassign of a letter) looks like a new
//! drive to Explorer. We temporarily disable AutoPlay and Autorun for the
//! current user, then restore the previous values. A marker file repairs an
//! abrupt exit (`process::exit` skips Drop).

use std::path::PathBuf;

const MARKER_NAME: &str = "multiusb-autoplay-restore.txt";
const NO_VALUE: &str = "none";

/// Disable AutoPlay for removable + fixed (many USB sticks report as fixed).
const NODRIVE_TYPE_DISABLE_ALL: u32 = 0xff;

pub struct AutoplayGuard {
    active: bool,
}

impl AutoplayGuard {
    /// Disable AutoPlay for the current user until the guard drops.
    pub fn disable() -> Self {
        let prev_disable = read_disable_autoplay();
        let prev_nodrive = read_no_drive_type_autorun();
        let marker = format!(
            "disable={}\nnodrive={}",
            match prev_disable {
                Some(v) => v.to_string(),
                None => NO_VALUE.to_string(),
            },
            match prev_nodrive {
                Some(v) => v.to_string(),
                None => NO_VALUE.to_string(),
            }
        );
        if std::fs::write(marker_path(), marker).is_err() {
            return Self { active: false };
        }
        let ok_disable = write_disable_autoplay(Some(1));
        let ok_nodrive = write_no_drive_type_autorun(Some(NODRIVE_TYPE_DISABLE_ALL));
        let active = ok_disable || ok_nodrive;
        if !active {
            let _ = std::fs::remove_file(marker_path());
        }
        Self { active }
    }
}

impl Drop for AutoplayGuard {
    fn drop(&mut self) {
        if self.active {
            restore_pending();
        }
    }
}

/// Restore the user's AutoPlay setting if a previous run left it disabled.
pub fn restore_pending() {
    let path = marker_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    // New multi-line marker, or legacy single-token marker.
    let (prev_disable, prev_nodrive) = parse_marker(&text);
    let ok_disable = write_disable_autoplay(prev_disable);
    let ok_nodrive = write_no_drive_type_autorun(prev_nodrive);
    if ok_disable || ok_nodrive {
        let _ = std::fs::remove_file(path);
    }
}

fn parse_marker(text: &str) -> (Option<u32>, Option<u32>) {
    let trimmed = text.trim();
    if !trimmed.contains('=') {
        // Legacy: just the DisableAutoplay previous value.
        let disable = match trimmed {
            NO_VALUE => None,
            other => other.parse().ok(),
        };
        return (disable, None);
    }
    let mut disable = None;
    let mut nodrive = None;
    for line in trimmed.lines() {
        if let Some(rest) = line.strip_prefix("disable=") {
            disable = match rest.trim() {
                NO_VALUE => None,
                other => other.parse().ok(),
            };
        } else if let Some(rest) = line.strip_prefix("nodrive=") {
            nodrive = match rest.trim() {
                NO_VALUE => None,
                other => other.parse().ok(),
            };
        }
    }
    (disable, nodrive)
}

/// `process::exit` skips destructors; use this so AutoPlay is restored first.
pub fn exit(code: i32) -> ! {
    restore_pending();
    std::process::exit(code)
}

fn marker_path() -> PathBuf {
    std::env::temp_dir().join(MARKER_NAME)
}

#[cfg(windows)]
const AUTOPLAY_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\AutoplayHandlers";
#[cfg(windows)]
const AUTOPLAY_VALUE: &str = "DisableAutoplay";

#[cfg(windows)]
const POLICY_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
#[cfg(windows)]
const NODRIVE_VALUE: &str = "NoDriveTypeAutoRun";

#[cfg(windows)]
fn read_disable_autoplay() -> Option<u32> {
    read_dword(AUTOPLAY_KEY, AUTOPLAY_VALUE)
}

#[cfg(windows)]
fn write_disable_autoplay(value: Option<u32>) -> bool {
    write_or_delete_dword(AUTOPLAY_KEY, AUTOPLAY_VALUE, value)
}

#[cfg(windows)]
fn read_no_drive_type_autorun() -> Option<u32> {
    read_dword(POLICY_KEY, NODRIVE_VALUE)
}

#[cfg(windows)]
fn write_no_drive_type_autorun(value: Option<u32>) -> bool {
    write_or_delete_dword(POLICY_KEY, NODRIVE_VALUE, value)
}

#[cfg(windows)]
fn read_dword(key: &str, value: &str) -> Option<u32> {
    use windows::core::HSTRING;
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};

    let mut data = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(key),
            &HSTRING::from(value),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    status.is_ok().then_some(data)
}

/// `Some(v)` writes the DWORD; `None` deletes the value (it was absent before).
#[cfg(windows)]
fn write_or_delete_dword(key: &str, value: &str, data: Option<u32>) -> bool {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_DWORD, RegDeleteKeyValueW, RegSetKeyValueW,
    };

    let key_h = HSTRING::from(key);
    let name = HSTRING::from(value);
    match data {
        Some(v) => {
            let bytes = v.to_le_bytes();
            unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    &key_h,
                    &name,
                    REG_DWORD.0,
                    Some(bytes.as_ptr() as *const _),
                    bytes.len() as u32,
                )
            }
            .is_ok()
        }
        None => {
            let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &key_h, &name) };
            status.is_ok() || status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND
        }
    }
}

#[cfg(not(windows))]
fn read_disable_autoplay() -> Option<u32> {
    None
}

#[cfg(not(windows))]
fn write_disable_autoplay(_value: Option<u32>) -> bool {
    false
}

#[cfg(not(windows))]
fn read_no_drive_type_autorun() -> Option<u32> {
    None
}

#[cfg(not(windows))]
fn write_no_drive_type_autorun(_value: Option<u32>) -> bool {
    false
}

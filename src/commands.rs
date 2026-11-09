use crate::disk_id::{DiskId, EnumeratedDisk};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct ExternalCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub stdin: String,
}

pub const DISKPART_ATTEMPTS: u32 = 3;
pub const DISKPART_WAIT: Duration = Duration::from_secs(3);

pub fn run_with_retries(
    attempts: u32,
    wait: Duration,
    sleep: &dyn Fn(Duration),
    op: &mut dyn FnMut() -> Result<(), ()>,
) -> Result<(), ()> {
    for i in 0..attempts {
        if op().is_ok() {
            return Ok(());
        }
        if i + 1 < attempts {
            sleep(wait);
        }
    }
    Err(())
}

pub fn fat32format_beside_exe(exe: &Path) -> PathBuf {
    let parent = exe.parent().expect("executable path has a parent directory");
    parent.join("fat32format.exe")
}

pub fn fat32format_command(exe: &Path, letter: char) -> ExternalCommand {
    ExternalCommand {
        program: fat32format_beside_exe(exe),
        args: vec![format!("{letter}:")],
        stdin: "y\r\n".to_string(),
    }
}

pub fn volume_label_command(letter: char, label: &str) -> ExternalCommand {
    if label.is_empty() {
        ExternalCommand {
            program: PathBuf::from("cmd.exe"),
            args: vec!["/C".to_string(), format!("label {letter}:")],
            stdin: "\r\n".to_string(),
        }
    } else {
        ExternalCommand {
            program: PathBuf::from("cmd.exe"),
            args: vec!["/C".to_string(), format!("label {letter}:{label}")],
            stdin: String::new(),
        }
    }
}

#[derive(Deserialize)]
struct RawDisk {
    #[serde(rename = "MediaType")]
    media_type: String,
    #[serde(rename = "Index")]
    index: u32,
    #[serde(rename = "Model")]
    model: String,
    #[serde(rename = "Size")]
    size: u64,
    #[serde(rename = "PNPDeviceID")]
    pnp_device_id: Option<String>,
    #[serde(rename = "SerialNumber")]
    serial_number: Option<String>,
    #[serde(rename = "Letters")]
    letters: Vec<String>,
    #[serde(rename = "ContainsSystem")]
    contains_system: bool,
}

fn parse_letters(letters: &[String]) -> Result<Vec<char>, String> {
    letters
        .iter()
        .map(|s| {
            let mut chars = s.chars();
            let c = chars
                .next()
                .ok_or_else(|| format!("empty drive letter entry: {s:?}"))?;
            if chars.next().is_some() {
                return Err(format!("drive letter entry must be one character: {s:?}"));
            }
            Ok(c)
        })
        .collect()
}

fn raw_to_enumerated(raw: RawDisk) -> Result<EnumeratedDisk, String> {
    let letters = parse_letters(&raw.letters)?;
    let size = raw.size;
    Ok(EnumeratedDisk {
        number: raw.index,
        model: raw.model,
        size_bytes: size,
        letters,
        id: DiskId {
            pnp_device_id: raw.pnp_device_id,
            serial: raw.serial_number,
            size_bytes: size,
        },
        media_type: raw.media_type,
        contains_system_volume: raw.contains_system,
    })
}

fn parse_disk_entries(json: &str, removable_only: bool) -> Result<Vec<EnumeratedDisk>, String> {
    let raw: Vec<RawDisk> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    raw.into_iter()
        .filter(|d| !removable_only || d.media_type.contains("Removable"))
        .map(raw_to_enumerated)
        .collect()
}

pub fn parse_disk_json(json: &str) -> Result<Vec<EnumeratedDisk>, String> {
    parse_disk_entries(json, true)
}

pub fn parse_all_disk_json(json: &str) -> Result<Vec<EnumeratedDisk>, String> {
    parse_disk_entries(json, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::{Arc, Mutex};

    #[test]
    fn run_with_retries_succeeds_after_two_failures() {
        let sleep_calls: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_clone = Arc::clone(&sleep_calls);
        let sleep = move |d: Duration| {
            sleep_calls_clone.lock().unwrap().push(d);
        };

        let attempts = Cell::new(0u32);
        let mut op = || {
            attempts.set(attempts.get() + 1);
            if attempts.get() < 3 {
                Err(())
            } else {
                Ok(())
            }
        };

        let result = run_with_retries(3, DISKPART_WAIT, &sleep, &mut op);
        assert!(result.is_ok());
        let sleeps = sleep_calls.lock().unwrap();
        assert_eq!(sleeps.len(), 2);
        assert_eq!(sleeps[0], Duration::from_secs(3));
        assert_eq!(sleeps[1], Duration::from_secs(3));
    }

    #[test]
    fn run_with_retries_err_after_all_failures() {
        let sleep = |_d: Duration| {};
        let mut op = || Err(());
        let result = run_with_retries(3, DISKPART_WAIT, &sleep, &mut op);
        assert!(result.is_err());
    }

    #[test]
    fn fat32format_beside_exe_path() {
        assert_eq!(
            fat32format_beside_exe(Path::new(r"C:\App\MultiUSB.exe")),
            PathBuf::from(r"C:\App\fat32format.exe")
        );
    }

    #[test]
    fn fat32format_command_spec() {
        let exe = Path::new(r"C:\App\MultiUSB.exe");
        let cmd = fat32format_command(exe, 'E');
        assert_eq!(cmd.program, PathBuf::from(r"C:\App\fat32format.exe"));
        assert_eq!(cmd.args, vec!["E:".to_string()]);
        assert_eq!(cmd.stdin, "y\r\n");
    }

    #[test]
    fn volume_label_command_with_label() {
        let cmd = volume_label_command('E', "STORE");
        assert_eq!(cmd.program, PathBuf::from("cmd.exe"));
        assert_eq!(cmd.args, vec!["/C".to_string(), "label E:STORE".to_string()]);
        assert_eq!(cmd.stdin, "");
    }

    #[test]
    fn volume_label_command_empty_label() {
        let cmd = volume_label_command('E', "");
        assert_eq!(cmd.program, PathBuf::from("cmd.exe"));
        assert_eq!(cmd.args, vec!["/C".to_string(), "label E:".to_string()]);
        assert_eq!(cmd.stdin, "\r\n");
    }

    fn sample_disks_json() -> &'static str {
        r#"[
            {
                "MediaType": "Removable Media",
                "Index": 2,
                "Model": "Kingston",
                "Size": 100,
                "PNPDeviceID": "USB\\X",
                "SerialNumber": "ABC",
                "Letters": ["E"],
                "ContainsSystem": false
            },
            {
                "MediaType": "Fixed hard disk media",
                "Index": 0,
                "Model": "Fixed",
                "Size": 500,
                "PNPDeviceID": null,
                "SerialNumber": null,
                "Letters": ["C"],
                "ContainsSystem": true
            }
        ]"#
    }

    fn expected_removable() -> EnumeratedDisk {
        EnumeratedDisk {
            number: 2,
            model: "Kingston".to_string(),
            size_bytes: 100,
            letters: vec!['E'],
            id: DiskId {
                pnp_device_id: Some("USB\\X".to_string()),
                serial: Some("ABC".to_string()),
                size_bytes: 100,
            },
            media_type: "Removable Media".to_string(),
            contains_system_volume: false,
        }
    }

    #[test]
    fn parse_disk_json_keeps_removable_only() {
        let json = sample_disks_json();
        let disks = parse_disk_json(json).expect("parse");
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0], expected_removable());
    }

    #[test]
    fn parse_all_disk_json_keeps_all() {
        let json = sample_disks_json();
        let disks = parse_all_disk_json(json).expect("parse");
        assert_eq!(disks.len(), 2);
        assert_eq!(disks[0], expected_removable());
        assert!(disks[1].media_type.contains("Fixed"));
        assert_eq!(disks[1].id.pnp_device_id, None);
        assert_eq!(disks[1].id.serial, None);
    }
}

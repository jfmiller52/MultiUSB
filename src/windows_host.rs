use crate::commands::{
    DISKPART_ATTEMPTS, DISKPART_WAIT, ExternalCommand, parse_all_disk_json, run_with_retries,
};
use crate::disk_id::{DiskId, EnumeratedDisk};
use crate::job::VolumeHost;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Windows console apps (powershell, diskpart, cmd) flash a window unless this is set.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn hidden_command(program: impl AsRef<OsStr>) -> Command {
    let mut cmd = Command::new(program);
    hide_console_window(&mut cmd);
    cmd
}

fn hide_console_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// Shared with the window so closing it waits out an in-flight `diskpart`
/// and never starts another one.
#[derive(Clone)]
pub struct CloseGate {
    lock: Arc<Mutex<()>>,
    requested: Arc<AtomicBool>,
}

impl CloseGate {
    pub fn new() -> Self {
        Self {
            lock: Arc::new(Mutex::new(())),
            requested: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    /// Mark close requested, then block until `diskpart` is not running.
    pub fn request_and_wait_for_diskpart(&self) {
        self.requested.store(true, Ordering::SeqCst);
        let _guard = self.lock.lock().unwrap_or_else(|err| err.into_inner());
    }
}

pub struct WindowsHost {
    gate: CloseGate,
}

impl WindowsHost {
    pub fn new(gate: CloseGate) -> Self {
        Self { gate }
    }

    fn stop_if_closed(&self) {
        if self.gate.is_requested() {
            std::process::exit(0);
        }
    }
}

impl VolumeHost for WindowsHost {
    fn list_all(&mut self) -> Result<Vec<EnumeratedDisk>, String> {
        self.stop_if_closed();
        let json = inventory_json()?;
        let disks = parse_all_disk_json(slice_json(&json))?;
        Ok(disks.into_iter().map(tidy_disk).collect())
    }

    fn diskpart(&mut self, script: &str) -> Result<(), String> {
        let _guard = self.gate.lock.lock().unwrap_or_else(|err| err.into_inner());
        if self.gate.is_requested() {
            std::process::exit(0);
        }
        let path = temp_file("diskpart", "txt");
        std::fs::write(&path, script).map_err(|err| err.to_string())?;
        let outcome = run_with_retries(
            DISKPART_ATTEMPTS,
            DISKPART_WAIT,
            &|wait| std::thread::sleep(wait),
            &mut || run_diskpart_once(&path),
        );
        // The child has been waited on. Remove the script, then stop if the
        // window closed while diskpart was running.
        let _ = std::fs::remove_file(&path);
        if self.gate.is_requested() {
            std::process::exit(0);
        }
        outcome.map_err(|()| "diskpart failed".to_string())
    }

    fn letter_for(&mut self, id: &DiskId) -> Result<Option<char>, String> {
        self.stop_if_closed();
        let disks = self.list_all()?;
        let matched: Vec<&EnumeratedDisk> = disks
            .iter()
            .filter(|disk| same_identity(disk, id))
            .collect();
        match matched.as_slice() {
            [disk] => Ok(disk.letters.first().copied()),
            _ => Ok(None),
        }
    }

    fn run_command(&mut self, cmd: &ExternalCommand) -> Result<(), String> {
        self.stop_if_closed();
        let mut child = hidden_command(&cmd.program)
            .args(&cmd.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("{}: {err}", cmd.program.display()))?;
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            stdin
                .write_all(cmd.stdin.as_bytes())
                .map_err(|err| err.to_string())?;
        }
        let output = child.wait_with_output().map_err(|err| err.to_string())?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if !stderr.trim().is_empty() {
            stderr.trim()
        } else {
            stdout.trim()
        };
        if detail.is_empty() {
            Err(format!(
                "{} exited with {}",
                cmd.program.display(),
                output.status
            ))
        } else {
            Err(format!(
                "{} exited with {}: {detail}",
                cmd.program.display(),
                output.status
            ))
        }
    }

    fn exe_path(&self) -> PathBuf {
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("MultiUSB.exe"))
    }
}

pub fn is_elevated() -> bool {
    unsafe { windows::Win32::UI::Shell::IsUserAnAdmin().as_bool() }
}

pub fn relaunch_elevated(exe: &Path) -> std::io::Result<()> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR, w};

    let file = HSTRING::from(exe.as_os_str());
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            &file,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = result.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(code.max(1) as i32))
    }
}

fn same_identity(disk: &EnumeratedDisk, id: &DiskId) -> bool {
    match (&id.pnp_device_id, &id.serial) {
        (Some(pnp), _) => disk.id.pnp_device_id.as_ref() == Some(pnp),
        (None, Some(serial)) => {
            disk.id.serial.as_ref() == Some(serial) && disk.size_bytes == id.size_bytes
        }
        (None, None) => false,
    }
}

fn tidy_disk(mut disk: EnumeratedDisk) -> EnumeratedDisk {
    disk.model = disk.model.trim().to_string();
    if let Some(serial) = disk.id.serial.take() {
        let serial = serial.trim().to_string();
        if !serial.is_empty() {
            disk.id.serial = Some(serial);
        }
    }
    if let Some(pnp) = disk.id.pnp_device_id.take() {
        let pnp = pnp.trim().to_string();
        if !pnp.is_empty() {
            disk.id.pnp_device_id = Some(pnp);
        }
    }
    disk
}

fn slice_json(text: &str) -> &str {
    let text = text.trim().trim_start_matches('\u{feff}');
    let Some(start) = text.find('[') else {
        return text;
    };
    let Some(end) = text.rfind(']') else {
        return text;
    };
    &text[start..=end]
}

fn temp_file(kind: &str, ext: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "multiusb-{kind}-{}-{seq}.{ext}",
        std::process::id()
    ))
}

/// Run one `diskpart /s` and wait until it exits. Never kills the child.
fn run_diskpart_once(script: &Path) -> Result<(), ()> {
    let mut child = hidden_command("diskpart.exe")
        .arg("/s")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ())?;
    match child.wait() {
        Ok(status) if status.success() => Ok(()),
        _ => Err(()),
    }
}

fn inventory_json() -> Result<String, String> {
    let script_path = temp_file("inventory", "ps1");
    let out_path = temp_file("inventory", "json");
    std::fs::write(&script_path, INVENTORY_SCRIPT).map_err(|err| err.to_string())?;
    let output = hidden_command("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script_path)
        .arg(&out_path)
        .stdin(Stdio::null())
        .output();
    let result = match output {
        Ok(output) if output.status.success() => {
            std::fs::read_to_string(&out_path).map_err(|err| err.to_string())
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("PowerShell inventory failed: {}", stderr.trim()))
        }
        Err(err) => Err(err.to_string()),
    };
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&out_path);
    result
}

const INVENTORY_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

function ConvertTo-JsonString([string]$value) {
    if ([string]::IsNullOrEmpty($value)) { return '""' }
    $escaped = $value.Replace('\', '\\').Replace('"', '\"').Replace("`r", '\r').Replace("`n", '\n').Replace("`t", '\t')
    return '"' + $escaped + '"'
}

function ConvertTo-JsonOrNull([string]$value) {
    if ([string]::IsNullOrWhiteSpace($value)) { return 'null' }
    return (ConvertTo-JsonString $value.Trim())
}

try {
    $systemDrive = [string]$env:SystemDrive
    $disks = @(Get-CimInstance -ClassName Win32_DiskDrive)
    $objects = New-Object System.Collections.Generic.List[string]
    foreach ($disk in $disks) {
        $letters = New-Object System.Collections.Generic.List[string]
        $contains = $false
        $partitions = @(Get-CimAssociatedInstance -InputObject $disk -ResultClassName Win32_DiskPartition)
        foreach ($part in $partitions) {
            $logicals = @(Get-CimAssociatedInstance -InputObject $part -ResultClassName Win32_LogicalDisk)
            foreach ($logical in $logicals) {
                $device = [string]$logical.DeviceID
                if ($device -eq $systemDrive) { $contains = $true }
                if ($device.Length -ge 1) {
                    $letter = $device.Substring(0, 1)
                    if ($letter -match '^[A-Za-z]$' -and -not $letters.Contains($letter)) {
                        [void]$letters.Add($letter)
                    }
                }
            }
        }
        $media = [string]$disk.MediaType
        $model = ([string]$disk.Model).Trim()
        $index = [uint32]$disk.Index
        if ($null -eq $disk.Size) {
            $sizeText = '0'
        } else {
            $sizeText = [string]([uint64]$disk.Size)
        }
        $letterJson = ''
        if ($letters.Count -gt 0) {
            $quoted = @()
            foreach ($letter in $letters) { $quoted += ('"' + $letter + '"') }
            $letterJson = $quoted -join ','
        }
        $containsText = if ($contains) { 'true' } else { 'false' }
        $obj = '{' +
            '"MediaType":' + (ConvertTo-JsonString $media) +
            ',"Index":' + $index +
            ',"Model":' + (ConvertTo-JsonString $model) +
            ',"Size":' + $sizeText +
            ',"PNPDeviceID":' + (ConvertTo-JsonOrNull ([string]$disk.PNPDeviceID)) +
            ',"SerialNumber":' + (ConvertTo-JsonOrNull ([string]$disk.SerialNumber)) +
            ',"Letters":[' + $letterJson + ']' +
            ',"ContainsSystem":' + $containsText +
        '}'
        [void]$objects.Add($obj)
    }
    $json = '[' + ($objects -join ',') + ']'
    $utf8 = New-Object System.Text.UTF8Encoding $false
    [System.IO.File]::WriteAllText($args[0], $json, $utf8)
    exit 0
} catch {
    [Console]::Error.WriteLine($_.ToString())
    exit 1
}
"#;

#[cfg(test)]
mod tests {
    use super::INVENTORY_SCRIPT;
    use std::fs;

    #[cfg(windows)]
    #[test]
    fn hide_console_window_sets_create_no_window() {
        let mut cmd = std::process::Command::new("powershell.exe");
        super::hide_console_window(&mut cmd);
        assert_eq!(super::CREATE_NO_WINDOW, 0x0800_0000);
        assert_eq!(cmd.get_program(), "powershell.exe");
    }

    #[test]
    fn failed_association_query_fails_inventory() {
        let stub = r#"
function Get-CimInstance {
    [CmdletBinding()]
    param([string]$ClassName)
    [pscustomobject]@{
        MediaType = 'Removable Media'
        Model = 'TestStick'
        Index = [uint32]7
        Size = [uint64]100
        PNPDeviceID = 'USB\TEST'
        SerialNumber = 'ABC'
    }
}
function Get-CimAssociatedInstance {
    [CmdletBinding()]
    param($InputObject, [string]$ResultClassName)
    Write-Error 'association query failed'
}
"#;
        let dir = std::env::temp_dir().join(format!("multiusb_assoc_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let script_path = dir.join("inventory.ps1");
        let out_path = dir.join("out.json");
        fs::write(&script_path, format!("{stub}\r\n{INVENTORY_SCRIPT}")).unwrap();

        let output = super::hidden_command("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script_path)
            .arg(&out_path)
            .output()
            .expect("run inventory");
        let stderr = String::from_utf8_lossy(&output.stderr);
        let json = fs::read_to_string(&out_path).unwrap_or_default();
        let _ = fs::remove_dir_all(&dir);

        assert!(
            !output.status.success(),
            "a failed association query must not look like a disk without the system volume\nstderr={stderr}\njson={json}"
        );
        assert!(
            stderr.contains("association query failed"),
            "stderr={stderr}"
        );
        assert!(
            !json.contains("\"ContainsSystem\":false"),
            "failed query wrote a not-system inventory: {json}"
        );
    }
}

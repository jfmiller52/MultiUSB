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

    fn free_drive_letter(&mut self) -> Result<char, String> {
        self.stop_if_closed();
        free_drive_letter()
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

    fn format_large_fat32(
        &mut self,
        disk_number: u32,
        letter: char,
        label: &str,
    ) -> Result<(), String> {
        self.stop_if_closed();
        format_large_fat32_volume(disk_number, letter, label)
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

fn free_drive_letter() -> Result<char, String> {
    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::GetLogicalDrives;
        let mask = unsafe { GetLogicalDrives() };
        if mask == 0 {
            return Err("could not read logical drives".to_string());
        }
        // Prefer D–Z; A/B are floppy legacy and C is almost always the system volume.
        for index in 3u32..26 {
            if mask & (1u32 << index) == 0 {
                return Ok(char::from(b'A' + index as u8));
            }
        }
        Err("no free drive letter".to_string())
    }
    #[cfg(not(windows))]
    {
        Err("drive letters require Windows".to_string())
    }
}

fn pad_fat_volume_label(label: &str) -> [u8; 11] {
    let mut out = [b' '; 11];
    for (dst, src) in out.iter_mut().zip(label.bytes()) {
        *dst = src;
    }
    out
}

fn fat32_total_sectors(length_bytes: i64) -> Result<u32, &'static str> {
    const BYTES_PER_SECTOR: u64 = 512;
    if length_bytes <= 0 {
        return Err("non-positive size");
    }
    let total_sectors = (length_bytes as u64) / BYTES_PER_SECTOR;
    if total_sectors == 0 || total_sectors > u64::from(u32::MAX) {
        return Err("unsupported sector count");
    }
    Ok(total_sectors as u32)
}

fn format_large_fat32_volume(
    disk_number: u32,
    letter: char,
    label: &str,
) -> Result<(), String> {
    #[cfg(windows)]
    {
        format_large_fat32_volume_windows(disk_number, letter, label)
    }
    #[cfg(not(windows))]
    {
        let _ = (disk_number, letter, label);
        Err("FAT32 built-in format requires Windows".to_string())
    }
}

/// Rufus-style access window (`DRIVE_ACCESS_TIMEOUT` / `DRIVE_ACCESS_RETRIES`).
#[cfg(windows)]
const DRIVE_ACCESS_TIMEOUT_MS: u64 = 15_000;
#[cfg(windows)]
const DRIVE_ACCESS_RETRIES: u32 = 150;

/// Open a volume or physical drive for write.
///
/// Mirrors Rufus `GetHandle`: prefer exclusive write (`FILE_SHARE_READ` only) so
/// Explorer and other writers stay out; after ~1/3 of the retry window, escalate
/// to `FILE_SHARE_WRITE` on sharing/access failures. Always keep `FILE_SHARE_READ`.
#[cfg(windows)]
fn open_device_rw(path: &str) -> Result<std::fs::File, String> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

    let sleep_ms = DRIVE_ACCESS_TIMEOUT_MS / u64::from(DRIVE_ACCESS_RETRIES);
    let mut share_write = false;
    let mut last_err = None;
    for attempt in 0..DRIVE_ACCESS_RETRIES {
        let share = FILE_SHARE_READ.0 | if share_write { FILE_SHARE_WRITE.0 } else { 0 };
        match OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(share)
            .open(path)
        {
            Ok(file) => return Ok(file),
            Err(err) => {
                let code = err.raw_os_error();
                last_err = Some(err);
                // ERROR_SHARING_VIOLATION (32), ERROR_ACCESS_DENIED (5),
                // ERROR_FILE_NOT_FOUND (2) / ERROR_PATH_NOT_FOUND (3) while
                // Windows finishes mounting after `assign letter=`.
                let retryable = matches!(code, Some(2 | 3 | 5 | 32));
                if !retryable {
                    break;
                }
                if !share_write && attempt > DRIVE_ACCESS_RETRIES / 3 {
                    share_write = true;
                }
                if attempt + 1 < DRIVE_ACCESS_RETRIES {
                    std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
                }
            }
        }
    }
    Err(format!(
        "open {path}: {}",
        last_err
            .map(|err| err.to_string())
            .unwrap_or_else(|| "device did not appear".to_string())
    ))
}

#[cfg(windows)]
fn allow_extended_dasd_io(handle: windows::Win32::Foundation::HANDLE) {
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::FSCTL_ALLOW_EXTENDED_DASD_IO;
    // Best-effort: Rufus ignores failure. Disables end-of-volume boundary checks
    // that can reject format writes near the last sectors.
    unsafe {
        let _ = DeviceIoControl(
            handle,
            FSCTL_ALLOW_EXTENDED_DASD_IO,
            None,
            0,
            None,
            0,
            None,
            None,
        );
    }
}

#[cfg(windows)]
fn lock_volume_with_retries(
    handle: windows::Win32::Foundation::HANDLE,
    path: &str,
) -> Result<(), String> {
    use std::time::{Duration, Instant};
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::FSCTL_LOCK_VOLUME;

    let sleep_ms = DRIVE_ACCESS_TIMEOUT_MS / u64::from(DRIVE_ACCESS_RETRIES);
    let deadline = Instant::now() + Duration::from_millis(DRIVE_ACCESS_TIMEOUT_MS);
    loop {
        match unsafe {
            DeviceIoControl(handle, FSCTL_LOCK_VOLUME, None, 0, None, 0, None, None)
        } {
            Ok(()) => return Ok(()),
            Err(err) => {
                if Instant::now() >= deadline {
                    return Err(format!("lock {path}: {err}"));
                }
                std::thread::sleep(Duration::from_millis(sleep_ms));
            }
        }
    }
}

#[cfg(windows)]
fn first_partition_extent(physical: &std::fs::File) -> Result<(u64, u64), String> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{
        DRIVE_LAYOUT_INFORMATION_EX, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, PARTITION_INFORMATION_EX,
    };

    let entry_size = size_of::<PARTITION_INFORMATION_EX>();
    let header_size = size_of::<DRIVE_LAYOUT_INFORMATION_EX>();
    let mut buffer = vec![0u8; header_size + entry_size * 16];
    let handle = HANDLE(physical.as_raw_handle());
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_DRIVE_LAYOUT_EX,
            None,
            0,
            Some(buffer.as_mut_ptr() as *mut _),
            buffer.len() as u32,
            Some(&mut returned),
            None,
        )
        .map_err(|err| format!("drive layout: {err}"))?;
    }
    if (returned as usize) < header_size {
        return Err("drive layout: short response".to_string());
    }
    let layout = unsafe { &*(buffer.as_ptr() as *const DRIVE_LAYOUT_INFORMATION_EX) };
    let count = layout.PartitionCount as usize;
    if count == 0 {
        return Err("drive layout: no partitions".to_string());
    }
    let needed = header_size.saturating_sub(entry_size) + entry_size * count;
    if (returned as usize) < needed || buffer.len() < needed {
        return Err("drive layout: truncated partition list".to_string());
    }
    let entries = unsafe {
        std::slice::from_raw_parts(layout.PartitionEntry.as_ptr(), count)
    };
    for entry in entries {
        if entry.PartitionLength > 0 && entry.StartingOffset >= 0 {
            return Ok((entry.StartingOffset as u64, entry.PartitionLength as u64));
        }
    }
    Err("drive layout: no usable partition".to_string())
}

#[cfg(windows)]
fn volume_length_bytes(volume: &std::fs::File, path: &str) -> Result<i64, String> {
    use std::mem::size_of_val;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{GET_LENGTH_INFORMATION, IOCTL_DISK_GET_LENGTH_INFO};

    let handle = HANDLE(volume.as_raw_handle());
    let mut length_info = GET_LENGTH_INFORMATION::default();
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_LENGTH_INFO,
            None,
            0,
            Some(&mut length_info as *mut _ as *mut _),
            size_of_val(&length_info) as u32,
            Some(&mut returned),
            None,
        )
        .map_err(|err| format!("volume length {path}: {err}"))?;
    }
    Ok(length_info.Length)
}

#[cfg(windows)]
fn format_fat32_on_file(
    file: &mut std::fs::File,
    path: &str,
    total_sectors: u32,
    label: &str,
) -> Result<(), String> {
    use std::io::{Seek, SeekFrom};

    let mut io = crate::sector_io::SectorIo::new(file, 512);
    io.seek(SeekFrom::Start(0))
        .map_err(|err| format!("seek {path}: {err}"))?;
    let options = fatfs::FormatVolumeOptions::new()
        .fat_type(fatfs::FatType::Fat32)
        .bytes_per_sector(512)
        .total_sectors(total_sectors)
        .volume_label(pad_fat_volume_label(label));
    fatfs::format_volume(&mut io, options).map_err(|err| format!("fatfs format {path}: {err}"))?;
    let file = io.into_inner();
    file.sync_all()
        .map_err(|err| format!("flush {path}: {err}"))?;
    Ok(())
}

#[cfg(windows)]
fn format_fat32_on_partition_view(
    physical: std::fs::File,
    path: &str,
    offset: u64,
    length: u64,
    label: &str,
) -> Result<(), String> {
    use std::io::{Seek, SeekFrom};

    let total_sectors = fat32_total_sectors(length as i64)
        .map_err(|reason| format!("partition length on {path}: {reason}"))?;
    let view = crate::sector_io::PartitionView::new(physical, offset, length);
    let mut io = crate::sector_io::SectorIo::new(view, 512);
    io.seek(SeekFrom::Start(0))
        .map_err(|err| format!("seek {path}+{offset}: {err}"))?;
    let options = fatfs::FormatVolumeOptions::new()
        .fat_type(fatfs::FatType::Fat32)
        .bytes_per_sector(512)
        .total_sectors(total_sectors)
        .volume_label(pad_fat_volume_label(label));
    fatfs::format_volume(&mut io, options)
        .map_err(|err| format!("fatfs format {path} offset {offset}: {err}"))?;
    let physical = io.into_inner().into_inner();
    physical
        .sync_all()
        .map_err(|err| format!("flush {path}: {err}"))?;
    Ok(())
}

/// Rufus path: write FAT32 through the same volume handle that is locked+dismounted.
#[cfg(windows)]
fn format_via_locked_volume(letter: char, label: &str) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{FSCTL_DISMOUNT_VOLUME, FSCTL_UNLOCK_VOLUME};

    let volume_path = format!(r"\\.\{letter}:");
    let volume = open_device_rw(&volume_path)?;
    let volume_handle = HANDLE(volume.as_raw_handle());
    let unlock = || unsafe {
        let _ = DeviceIoControl(
            volume_handle,
            FSCTL_UNLOCK_VOLUME,
            None,
            0,
            None,
            0,
            None,
            None,
        );
    };

    allow_extended_dasd_io(volume_handle);
    if let Err(err) = lock_volume_with_retries(volume_handle, &volume_path) {
        unlock();
        return Err(err);
    }
    // Rufus always dismounts, then writes on this same handle (not a second open).
    unsafe {
        let _ = DeviceIoControl(
            volume_handle,
            FSCTL_DISMOUNT_VOLUME,
            None,
            0,
            None,
            0,
            None,
            None,
        );
    }

    let length = match volume_length_bytes(&volume, &volume_path) {
        Ok(length) => length,
        Err(err) => {
            unlock();
            return Err(err);
        }
    };
    let total_sectors = match fat32_total_sectors(length) {
        Ok(sectors) => sectors,
        Err(reason) => {
            unlock();
            return Err(format!("volume length {volume_path}: {reason}"));
        }
    };

    let mut volume = volume;
    let result = format_fat32_on_file(&mut volume, &volume_path, total_sectors, label);
    unlock();
    drop(volume);
    result
}

/// Fallback: open PhysicalDrive *before* touching the volume, lock the letter
/// without dismounting, then write at the partition offset.
///
/// Opening PhysicalDrive after `FSCTL_DISMOUNT_VOLUME` has failed with os error
/// 433 on removable USB (device evaporates for the second handle).
#[cfg(windows)]
fn format_via_physical_while_volume_locked(
    disk_number: u32,
    letter: char,
    label: &str,
) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::FSCTL_UNLOCK_VOLUME;

    let physical_path = format!(r"\\.\PhysicalDrive{disk_number}");
    let volume_path = format!(r"\\.\{letter}:");

    let physical = open_device_rw(&physical_path)?;
    let (offset, length) = first_partition_extent(&physical)?;

    let volume = open_device_rw(&volume_path)?;
    let volume_handle = HANDLE(volume.as_raw_handle());
    let unlock = || unsafe {
        let _ = DeviceIoControl(
            volume_handle,
            FSCTL_UNLOCK_VOLUME,
            None,
            0,
            None,
            0,
            None,
            None,
        );
    };

    allow_extended_dasd_io(volume_handle);
    if let Err(err) = lock_volume_with_retries(volume_handle, &volume_path) {
        unlock();
        return Err(err);
    }
    // Intentionally no dismount: that was poisoning PhysicalDrive writes (433).

    let result = format_fat32_on_partition_view(physical, &physical_path, offset, length, label);
    unlock();
    drop(volume);
    result
}

#[cfg(windows)]
fn format_large_fat32_volume_windows(
    disk_number: u32,
    letter: char,
    label: &str,
) -> Result<(), String> {
    // Primary: Rufus-style writes on the locked+dismounted volume handle.
    // Fallback: PhysicalDrive opened first, volume locked without dismount.
    match format_via_locked_volume(letter, label) {
        Ok(()) => Ok(()),
        Err(volume_err) => match format_via_physical_while_volume_locked(disk_number, letter, label)
        {
            Ok(()) => Ok(()),
            Err(physical_err) => Err(format!(
                "{volume_err}; physical fallback: {physical_err}"
            )),
        },
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
    use super::{fat32_total_sectors, pad_fat_volume_label, INVENTORY_SCRIPT};
    use std::fs;

    #[test]
    fn pad_fat_volume_label_truncates_and_pads() {
        assert_eq!(pad_fat_volume_label(""), *b"           ");
        assert_eq!(pad_fat_volume_label("STORE"), *b"STORE      ");
        assert_eq!(pad_fat_volume_label("EXACTLYELEV"), *b"EXACTLYELEV");
        assert_eq!(pad_fat_volume_label("TOOLONGVOLUME"), *b"TOOLONGVOLU");
    }

    #[test]
    fn fat32_total_sectors_from_volume_length() {
        assert_eq!(fat32_total_sectors(64 * 1024 * 1024), Ok(131_072));
        assert!(fat32_total_sectors(0).is_err());
        assert!(fat32_total_sectors(-1).is_err());
        assert!(fat32_total_sectors(400).is_err());
    }

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

use crate::commands::{
    DISKPART_ATTEMPTS, DISKPART_WAIT, ExternalCommand, parse_all_disk_json,
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
            crate::autoplay::exit(0);
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
            crate::autoplay::exit(0);
        }
        let path = temp_file("diskpart", "txt");
        std::fs::write(&path, script).map_err(|err| err.to_string())?;
        let mut last_err = "diskpart failed".to_string();
        let mut ok = false;
        let mut notes = Vec::new();
        for attempt in 0..DISKPART_ATTEMPTS {
            if let Err(err) = ensure_virtual_disk_service() {
                notes.push(format!("vds ensure attempt {}: {err}", attempt + 1));
            }
            match run_diskpart_once(&path) {
                Ok(()) => {
                    ok = true;
                    break;
                }
                Err(err) => {
                    let vds_down = diskpart_vds_startup_failure(&err);
                    last_err = err;
                    if vds_down {
                        notes.push(format!(
                            "vds startup failure on attempt {}; restarting Virtual Disk service",
                            attempt + 1
                        ));
                        if let Err(restart_err) = restart_virtual_disk_service() {
                            notes.push(format!("vds restart: {restart_err}"));
                        }
                    }
                    if attempt + 1 < DISKPART_ATTEMPTS {
                        let wait = if vds_down {
                            DISKPART_WAIT.saturating_mul(2)
                        } else {
                            DISKPART_WAIT
                        };
                        std::thread::sleep(wait);
                    }
                }
            }
        }
        // The child has been waited on. Remove the script, then stop if the
        // window closed while diskpart was running.
        let _ = std::fs::remove_file(&path);
        if self.gate.is_requested() {
            crate::autoplay::exit(0);
        }
        if ok {
            Ok(())
        } else {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let mut lines = vec![
                "diskpart failed after retries".to_string(),
                format!("script:\n{script}"),
                last_err.clone(),
            ];
            lines.extend(notes);
            if let Ok(path) = crate::log_file::write_run_log(
                &cwd,
                &chrono::Local::now()
                    .format("diskpart-%Y-%m-%d_%H-%M-%S")
                    .to_string(),
                &lines,
            ) {
                last_err = format!("{last_err}\ntrace file: {}", path.display());
            }
            Err(last_err)
        }
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

/// Step trace for built-in FAT32 format failures (live log + Logs/fat32-trace-*.log).
#[derive(Debug, Default)]
struct Fat32Trace {
    start: Option<std::time::Instant>,
    lines: Vec<String>,
}

impl Fat32Trace {
    fn new() -> Self {
        Self {
            start: Some(std::time::Instant::now()),
            lines: Vec::new(),
        }
    }

    fn elapsed_ms(&self) -> u128 {
        self.start
            .map(|start| start.elapsed().as_millis())
            .unwrap_or(0)
    }

    fn section(&mut self, title: &str) {
        self.lines.push(format!("--- {title} ---"));
    }

    fn note(&mut self, message: impl AsRef<str>) {
        self.lines
            .push(format!("[+{}ms] {}", self.elapsed_ms(), message.as_ref()));
    }

    fn ok(&mut self, step: &str, detail: impl AsRef<str>) {
        self.note(format!("{step}: {}", detail.as_ref()));
    }

    fn fail(&mut self, step: &str, err: impl AsRef<str>) {
        self.note(format!("{step}: FAIL {}", err.as_ref()));
    }

    fn render_failure(&self, disk_number: u32, letter: char, label: &str) -> String {
        let mut out = format!(
            "FAT32 format failed (disk {disk_number} letter {letter} label={label:?})\n"
        );
        out.push_str(&self.lines.join("\n"));
        out
    }
}

fn describe_os_error(err: &std::io::Error) -> String {
    match err.raw_os_error() {
        Some(433) => format!("ERROR_DEVICE_DOES_NOT_EXIST (433): {err}"),
        Some(87) => format!("ERROR_INVALID_PARAMETER (87): {err}"),
        Some(5) => format!("ERROR_ACCESS_DENIED (5): {err}"),
        Some(32) => format!("ERROR_SHARING_VIOLATION (32): {err}"),
        Some(2) => format!("ERROR_FILE_NOT_FOUND (2): {err}"),
        Some(3) => format!("ERROR_PATH_NOT_FOUND (3): {err}"),
        Some(code) => format!("os error {code}: {err}"),
        None => err.to_string(),
    }
}

/// Open a volume or physical drive for write.
///
/// Mirrors Rufus `GetHandle`: by default prefer exclusive write (`FILE_SHARE_READ`
/// only); after ~1/3 of the retry window, escalate to `FILE_SHARE_WRITE`.
/// PhysicalDrive paths should start shared (`share_write_first`) because Windows
/// already holds the mounted volume.
#[cfg(windows)]
fn open_device_rw(path: &str, trace: &mut Fat32Trace) -> Result<std::fs::File, String> {
    open_device_rw_ex(path, false, false, trace)
}

/// Ridgecrop fat32format open: exclusive (`CreateFile` share mode 0) on `\\.\X:`.
#[cfg(windows)]
fn open_volume_ridgecrop(path: &str, trace: &mut Fat32Trace) -> Result<std::fs::File, String> {
    open_device_rw_ex(path, false, true, trace)
}

#[cfg(windows)]
fn open_device_rw_ex(
    path: &str,
    share_write_first: bool,
    exclusive: bool,
    trace: &mut Fat32Trace,
) -> Result<std::fs::File, String> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

    let sleep_ms = DRIVE_ACCESS_TIMEOUT_MS / u64::from(DRIVE_ACCESS_RETRIES);
    let mut share_write = share_write_first;
    let mut last_err = None;
    let mut attempts = 0u32;
    for attempt in 0..DRIVE_ACCESS_RETRIES {
        attempts = attempt + 1;
        // fat32format.c uses share mode 0 (fully exclusive).
        let share = if exclusive {
            0
        } else {
            FILE_SHARE_READ.0 | if share_write { FILE_SHARE_WRITE.0 } else { 0 }
        };
        match OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(share)
            .open(path)
        {
            Ok(file) => {
                let share_label = if exclusive {
                    "exclusive"
                } else if share_write {
                    "read+write"
                } else {
                    "read"
                };
                trace.ok(
                    &format!("open {path}"),
                    format!("share={share_label} attempts={attempts}"),
                );
                return Ok(file);
            }
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
                if !exclusive && !share_write && attempt > DRIVE_ACCESS_RETRIES / 3 {
                    share_write = true;
                    trace.note(format!(
                        "open {path}: escalating to share-write after {attempts} attempts"
                    ));
                }
                if attempt + 1 < DRIVE_ACCESS_RETRIES {
                    std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
                }
            }
        }
    }
    let detail = last_err
        .as_ref()
        .map(describe_os_error)
        .unwrap_or_else(|| "device did not appear".to_string());
    let message = format!("open {path}: {detail} (attempts={attempts})");
    trace.fail(&format!("open {path}"), &detail);
    Err(message)
}

/// Drop Windows's volume handle. Do not call this before PhysicalDrive writes —
/// on the PNY it caused os error 55 and removed the drive letter.
#[cfg(windows)]
#[allow(dead_code)]
fn quiet_dismount_letter(letter: char, trace: &mut Fat32Trace) {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{
        FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, FSCTL_UNLOCK_VOLUME,
    };

    let volume_path = format!(r"\\.\{letter}:");
    let Ok(volume) = open_device_rw_ex(&volume_path, true, false, trace) else {
        trace.note(format!("quiet dismount {volume_path}: open failed, continuing"));
        return;
    };
    let handle = HANDLE(volume.as_raw_handle());
    let _ = unsafe { DeviceIoControl(handle, FSCTL_LOCK_VOLUME, None, 0, None, 0, None, None) };
    match unsafe { DeviceIoControl(handle, FSCTL_DISMOUNT_VOLUME, None, 0, None, 0, None, None) } {
        Ok(()) => trace.ok("quiet dismount", "ok"),
        Err(err) => trace.note(format!("quiet dismount: ignored ({err})")),
    }
    let _ = unsafe { DeviceIoControl(handle, FSCTL_UNLOCK_VOLUME, None, 0, None, 0, None, None) };
    drop(volume);
    std::thread::sleep(std::time::Duration::from_millis(300));
}

#[cfg(windows)]
fn allow_extended_dasd_io(
    handle: windows::Win32::Foundation::HANDLE,
    trace: &mut Fat32Trace,
) {
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::FSCTL_ALLOW_EXTENDED_DASD_IO;
    // Best-effort: Rufus ignores failure. Disables end-of-volume boundary checks
    // that can reject format writes near the last sectors.
    match unsafe {
        DeviceIoControl(
            handle,
            FSCTL_ALLOW_EXTENDED_DASD_IO,
            None,
            0,
            None,
            0,
            None,
            None,
        )
    } {
        Ok(()) => trace.ok("ALLOW_EXTENDED_DASD_IO", "ok"),
        Err(err) => trace.note(format!("ALLOW_EXTENDED_DASD_IO: ignored fail ({err})")),
    }
}

#[cfg(windows)]
fn lock_volume_with_retries(
    handle: windows::Win32::Foundation::HANDLE,
    path: &str,
    trace: &mut Fat32Trace,
) -> Result<(), String> {
    use std::time::{Duration, Instant};
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::FSCTL_LOCK_VOLUME;

    let sleep_ms = DRIVE_ACCESS_TIMEOUT_MS / u64::from(DRIVE_ACCESS_RETRIES);
    let deadline = Instant::now() + Duration::from_millis(DRIVE_ACCESS_TIMEOUT_MS);
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        match unsafe {
            DeviceIoControl(handle, FSCTL_LOCK_VOLUME, None, 0, None, 0, None, None)
        } {
            Ok(()) => {
                trace.ok(
                    &format!("lock {path}"),
                    format!("ok attempts={attempts}"),
                );
                return Ok(());
            }
            Err(err) => {
                if Instant::now() >= deadline {
                    let message = format!("lock {path}: {err} (attempts={attempts})");
                    trace.fail(&format!("lock {path}"), format!("{err} attempts={attempts}"));
                    return Err(message);
                }
                std::thread::sleep(Duration::from_millis(sleep_ms));
            }
        }
    }
}

#[cfg(windows)]
fn first_partition_extent(
    physical: &std::fs::File,
    trace: &mut Fat32Trace,
) -> Result<(u64, u64), String> {
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
    if let Err(err) = unsafe {
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
    } {
        let message = format!("drive layout: {err}");
        trace.fail("IOCTL_DISK_GET_DRIVE_LAYOUT_EX", &message);
        return Err(message);
    }
    if (returned as usize) < header_size {
        let message = "drive layout: short response".to_string();
        trace.fail("IOCTL_DISK_GET_DRIVE_LAYOUT_EX", &message);
        return Err(message);
    }
    let layout = unsafe { &*(buffer.as_ptr() as *const DRIVE_LAYOUT_INFORMATION_EX) };
    let count = layout.PartitionCount as usize;
    if count == 0 {
        let message = "drive layout: no partitions".to_string();
        trace.fail("IOCTL_DISK_GET_DRIVE_LAYOUT_EX", &message);
        return Err(message);
    }
    let needed = header_size.saturating_sub(entry_size) + entry_size * count;
    if (returned as usize) < needed || buffer.len() < needed {
        let message = "drive layout: truncated partition list".to_string();
        trace.fail("IOCTL_DISK_GET_DRIVE_LAYOUT_EX", &message);
        return Err(message);
    }
    let entries = unsafe {
        std::slice::from_raw_parts(layout.PartitionEntry.as_ptr(), count)
    };
    for (index, entry) in entries.iter().enumerate() {
        trace.note(format!(
            "partition[{index}]: style={} offset={} length={} number={}",
            entry.PartitionStyle.0,
            entry.StartingOffset,
            entry.PartitionLength,
            entry.PartitionNumber
        ));
        if entry.PartitionLength > 0 && entry.StartingOffset >= 0 {
            let offset = entry.StartingOffset as u64;
            let length = entry.PartitionLength as u64;
            trace.ok(
                "partition extent",
                format!("offset={offset} length={length} count={count}"),
            );
            return Ok((offset, length));
        }
    }
    let message = "drive layout: no usable partition".to_string();
    trace.fail("IOCTL_DISK_GET_DRIVE_LAYOUT_EX", &message);
    Err(message)
}

/// Partition offset/length/hidden sectors via the volume handle (Ridgecrop path).
#[cfg(windows)]
fn volume_partition_extent(
    volume: &std::fs::File,
    path: &str,
    trace: &mut Fat32Trace,
) -> Result<(u64, u64, u32, Option<u8>), String> {
    use std::mem::size_of_val;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{
        IOCTL_DISK_GET_PARTITION_INFO, IOCTL_DISK_GET_PARTITION_INFO_EX, PARTITION_INFORMATION,
        PARTITION_INFORMATION_EX, PARTITION_STYLE_MBR,
    };

    let handle = HANDLE(volume.as_raw_handle());
    let mut returned = 0u32;
    let mut pi = PARTITION_INFORMATION::default();
    if unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_PARTITION_INFO,
            None,
            0,
            Some(&mut pi as *mut _ as *mut _),
            size_of_val(&pi) as u32,
            Some(&mut returned),
            None,
        )
    }
    .is_ok()
    {
        let offset = pi.StartingOffset as u64;
        let length = pi.PartitionLength as u64;
        let hidden = pi.HiddenSectors;
        let mbr_type = pi.PartitionType;
        trace.ok(
            "IOCTL_DISK_GET_PARTITION_INFO",
            format!("offset={offset} length={length} hidden={hidden} type=0x{mbr_type:02x}"),
        );
        return Ok((offset, length, hidden, Some(mbr_type)));
    }

    let mut xpi = PARTITION_INFORMATION_EX::default();
    if let Err(err) = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_PARTITION_INFO_EX,
            None,
            0,
            Some(&mut xpi as *mut _ as *mut _),
            size_of_val(&xpi) as u32,
            Some(&mut returned),
            None,
        )
    } {
        let message = format!("partition info {path}: {err}");
        trace.fail("IOCTL_DISK_GET_PARTITION_INFO_EX", &message);
        return Err(message);
    }
    let offset = xpi.StartingOffset as u64;
    let length = xpi.PartitionLength as u64;
    let hidden = (offset / 512) as u32;
    let mbr_type = if xpi.PartitionStyle == PARTITION_STYLE_MBR {
        Some(unsafe { xpi.Anonymous.Mbr.PartitionType })
    } else {
        None
    };
    trace.ok(
        "IOCTL_DISK_GET_PARTITION_INFO_EX",
        format!("offset={offset} length={length} hidden={hidden} mbr_type={mbr_type:?}"),
    );
    Ok((offset, length, hidden, mbr_type))
}

/// Set MBR partition type to FAT32 LBA (0x0c), as fat32format.c does after writing.
#[cfg(windows)]
fn set_partition_type_fat32_lba(
    volume: &std::fs::File,
    path: &str,
    hidden_sectors: u32,
    trace: &mut Fat32Trace,
) {
    use std::mem::size_of_val;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{IOCTL_DISK_SET_PARTITION_INFO, SET_PARTITION_INFORMATION};

    // Super-floppy (no partition table) has HiddenSectors==0; fat32format skips failing then.
    if hidden_sectors == 0 {
        trace.note("SET_PARTITION_INFO: skipped (hidden_sectors=0, likely super-floppy)");
        return;
    }
    let handle = HANDLE(volume.as_raw_handle());
    let mut info = SET_PARTITION_INFORMATION::default();
    info.PartitionType = 0x0c; // FAT32 LBA
    let mut returned = 0u32;
    match unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_SET_PARTITION_INFO,
            Some(&info as *const _ as *const _),
            size_of_val(&info) as u32,
            None,
            0,
            Some(&mut returned),
            None,
        )
    } {
        Ok(()) => trace.ok("SET_PARTITION_INFO", "type=0x0c FAT32 LBA"),
        Err(err) => trace.note(format!("SET_PARTITION_INFO {path}: ignored ({err})")),
    }
}

/// Ridgecrop fat32format sequence: exclusive open → DASD → lock → write →
/// set partition type → dismount → unlock. No dismount before writes.
#[cfg(windows)]
fn format_via_ridgecrop(
    letter: char,
    label: &str,
    trace: &mut Fat32Trace,
) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{FSCTL_DISMOUNT_VOLUME, FSCTL_UNLOCK_VOLUME};

    let volume_path = format!(r"\\.\{letter}:");
    let volume = open_volume_ridgecrop(&volume_path, trace)?;
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

    allow_extended_dasd_io(volume_handle, trace);
    if let Err(err) = lock_volume_with_retries(volume_handle, &volume_path, trace) {
        unlock();
        return Err(err);
    }
    trace.note("dismount before write: skipped (ridgecrop / fat32format order)");

    let (_offset, length, hidden_sectors, mbr_type) =
        match volume_partition_extent(&volume, &volume_path, trace) {
            Ok(extent) => extent,
            Err(err) => {
                unlock();
                return Err(err);
            }
        };
    if length == 0 {
        unlock();
        let message = format!("partition length {volume_path}: zero");
        trace.fail("partition length", &message);
        return Err(message);
    }

    let mut volume = volume;
    if let Err(err) = format_fat32_on_file(
        &mut volume,
        &volume_path,
        length,
        hidden_sectors,
        label,
        trace,
    ) {
        unlock();
        return Err(err);
    }

    // Rewriting the type re-enumerates the volume (extra AutoPlay / "drive
    // connected"). diskpart already creates MBR partitions as 0x0C; GPT is skipped
    // like fat32format.c does.
    match mbr_type {
        Some(0x0c) => trace.note("SET_PARTITION_INFO: skipped (already 0x0c)"),
        Some(_) => set_partition_type_fat32_lba(&volume, &volume_path, hidden_sectors, trace),
        None => trace.note("SET_PARTITION_INFO: skipped (GPT)"),
    }

    match unsafe {
        DeviceIoControl(
            volume_handle,
            FSCTL_DISMOUNT_VOLUME,
            None,
            0,
            None,
            0,
            None,
            None,
        )
    } {
        Ok(()) => trace.ok(&format!("dismount after write {volume_path}"), "ok"),
        Err(err) => {
            unlock();
            let message = format!("dismount after write {volume_path}: {err}");
            trace.fail("dismount after write", &message);
            return Err(message);
        }
    }
    unlock();
    drop(volume);
    std::thread::sleep(std::time::Duration::from_millis(500));
    verify_fat32_mount(letter, trace)
}

#[cfg(windows)]
fn volume_length_bytes(
    volume: &std::fs::File,
    path: &str,
    trace: &mut Fat32Trace,
) -> Result<i64, String> {
    use std::mem::size_of_val;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{GET_LENGTH_INFORMATION, IOCTL_DISK_GET_LENGTH_INFO};

    let handle = HANDLE(volume.as_raw_handle());
    let mut length_info = GET_LENGTH_INFORMATION::default();
    let mut returned = 0u32;
    if let Err(err) = unsafe {
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
    } {
        let message = format!("volume length {path}: {err}");
        trace.fail("IOCTL_DISK_GET_LENGTH_INFO", &message);
        return Err(message);
    }
    trace.ok(
        "volume length",
        format!("{path} length={}", length_info.Length),
    );
    Ok(length_info.Length)
}

#[cfg(windows)]
fn format_fat32_on_file(
    file: &mut std::fs::File,
    path: &str,
    total_bytes: u64,
    hidden_sectors: u32,
    label: &str,
    trace: &mut Fat32Trace,
) -> Result<(), String> {
    use std::io::SeekFrom;

    if let Err(err) = std::io::Seek::seek(file, SeekFrom::Start(0)) {
        let message = format!("seek {path}: {}", describe_os_error(&err));
        trace.fail(&format!("seek {path}"), describe_os_error(&err));
        return Err(message);
    }
    trace.ok(&format!("seek {path}"), "0");
    let padded = pad_fat_volume_label(label);
    if let Err(err) =
        crate::format_fat32::format_fat32_volume(file, total_bytes, hidden_sectors, &padded)
    {
        let detail = describe_os_error(&err);
        let message = format!("direct fat32 format {path}: {detail}");
        trace.fail(&format!("direct fat32 format {path}"), detail);
        return Err(message);
    }
    trace.ok(
        &format!("direct fat32 format {path}"),
        format!("total_bytes={total_bytes} hidden_sectors={hidden_sectors}"),
    );
    if let Err(err) = file.sync_all() {
        let message = format!("flush {path}: {}", describe_os_error(&err));
        trace.fail(&format!("flush {path}"), describe_os_error(&err));
        return Err(message);
    }
    trace.ok(&format!("flush {path}"), "ok");
    Ok(())
}

#[cfg(windows)]
fn format_fat32_on_partition_view(
    physical: std::fs::File,
    path: &str,
    offset: u64,
    length: u64,
    label: &str,
    trace: &mut Fat32Trace,
) -> Result<(), String> {
    let hidden_sectors = (offset / 512) as u32;
    let mut view = crate::sector_io::PartitionView::new(physical, offset, length);
    let padded = pad_fat_volume_label(label);
    if let Err(err) =
        crate::format_fat32::format_fat32_volume(&mut view, length, hidden_sectors, &padded)
    {
        let detail = describe_os_error(&err);
        let message = format!("direct fat32 format {path} offset {offset}: {detail}");
        trace.fail(
            &format!("direct fat32 format {path} offset {offset}"),
            detail,
        );
        return Err(message);
    }
    trace.ok(
        &format!("direct fat32 format {path} offset {offset}"),
        format!("length={length} hidden_sectors={hidden_sectors}"),
    );
    let physical = view.into_inner();
    if let Err(err) = physical.sync_all() {
        let message = format!("flush {path}: {}", describe_os_error(&err));
        trace.fail(&format!("flush {path}"), describe_os_error(&err));
        return Err(message);
    }
    trace.ok(&format!("flush {path}"), "ok");
    Ok(())
}

/// Assign a drive letter once after the FAT32 image is on disk. Do not
/// remove+reassign — that makes Explorer treat the stick as unplugged/replugged.
#[cfg(windows)]
fn assign_letter_after_format(
    disk_number: u32,
    letter: char,
    label: &str,
    trace: &mut Fat32Trace,
) -> Result<(), String> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::SetVolumeLabelW;

    let root = format!(r"{letter}:\");
    let script = format!(
        "select disk {disk_number}\r\nselect partition 1\r\nassign letter={letter}\r\nexit\r\n"
    );
    run_diskpart_reassign(&script, trace)?;
    trace.ok("diskpart assign", format!("letter={letter}"));
    std::thread::sleep(std::time::Duration::from_millis(500));

    let mut last_err = String::new();
    for attempt in 1..=10u32 {
        match verify_fat32_mount(letter, trace) {
            Ok(()) => {
                let root_h = HSTRING::from(root.as_str());
                let label_h = HSTRING::from(label);
                match unsafe { SetVolumeLabelW(&root_h, &label_h) } {
                    Ok(()) => trace.ok("SetVolumeLabel", format!("{root} -> {label:?}")),
                    Err(err) => trace.note(format!("SetVolumeLabel ignored: {err}")),
                }
                return Ok(());
            }
            Err(err) => {
                last_err = err;
                trace.note(format!("verify mount attempt {attempt}: {last_err}"));
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
    }
    Err(last_err)
}

/// Kept for volume-path fallbacks that already have a letter (lock/dismount only).
#[cfg(windows)]
fn remount_fat32_letter(
    disk_number: u32,
    letter: char,
    label: &str,
    trace: &mut Fat32Trace,
) -> Result<(), String> {
    assign_letter_after_format(disk_number, letter, label, trace)
}

#[cfg(windows)]
fn run_diskpart_reassign(script: &str, trace: &mut Fat32Trace) -> Result<(), String> {
    let path = temp_file("diskpart-reassign", "txt");
    std::fs::write(&path, script).map_err(|err| err.to_string())?;
    let _ = ensure_virtual_disk_service();
    let result = run_diskpart_once(&path);
    let _ = std::fs::remove_file(&path);
    match &result {
        Ok(()) => {}
        Err(err) => trace.fail("diskpart reassign", err),
    }
    result
}

#[cfg(windows)]
fn verify_fat32_mount(letter: char, trace: &mut Fat32Trace) -> Result<(), String> {
    use windows::Win32::Storage::FileSystem::GetVolumeInformationW;

    let root = format!(r"{letter}:\");
    let mut fs_name = [0u16; 32];
    let ok = unsafe {
        GetVolumeInformationW(
            &windows::core::HSTRING::from(root.as_str()),
            None,
            None,
            None,
            None,
            Some(&mut fs_name),
        )
    };
    match ok {
        Ok(()) => {
            let name = String::from_utf16_lossy(
                &fs_name
                    .iter()
                    .copied()
                    .take_while(|c| *c != 0)
                    .collect::<Vec<_>>(),
            );
            trace.ok("GetVolumeInformation", format!("{root} fs={name}"));
            if name.eq_ignore_ascii_case("FAT32") || name.eq_ignore_ascii_case("FAT") {
                Ok(())
            } else {
                let message = format!(
                    "after format, {root} reports filesystem {name:?} (expected FAT32)"
                );
                trace.fail("verify mount", &message);
                Err(message)
            }
        }
        Err(err) => {
            let message = format!("GetVolumeInformation {root}: {err}");
            trace.fail("verify mount", &message);
            Err(message)
        }
    }
}

/// Write FAT32 through a locked volume handle. `dismount` matches Rufus when true;
/// PNY traces showed os error 433 mid-fatfs after a successful dismount, so we also
/// try lock-only first.
#[cfg(windows)]
fn format_via_locked_volume(
    letter: char,
    label: &str,
    hidden_sectors: u32,
    dismount: bool,
    trace: &mut Fat32Trace,
) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{FSCTL_DISMOUNT_VOLUME, FSCTL_UNLOCK_VOLUME};

    let volume_path = format!(r"\\.\{letter}:");
    let volume = open_device_rw(&volume_path, trace)?;
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

    allow_extended_dasd_io(volume_handle, trace);
    if let Err(err) = lock_volume_with_retries(volume_handle, &volume_path, trace) {
        unlock();
        return Err(err);
    }
    if dismount {
        match unsafe {
            DeviceIoControl(
                volume_handle,
                FSCTL_DISMOUNT_VOLUME,
                None,
                0,
                None,
                0,
                None,
                None,
            )
        } {
            Ok(()) => trace.ok(&format!("dismount {volume_path}"), "ok"),
            Err(err) => trace.note(format!("dismount {volume_path}: ignored fail ({err})")),
        }
    } else {
        trace.note(format!("dismount {volume_path}: skipped"));
    }

    let length = match volume_length_bytes(&volume, &volume_path, trace) {
        Ok(length) => length,
        Err(err) => {
            unlock();
            return Err(err);
        }
    };
    if length <= 0 {
        unlock();
        let message = format!("volume length {volume_path}: non-positive");
        trace.fail("volume length", &message);
        return Err(message);
    }
    trace.ok("volume bytes", format!("{length}"));

    let mut volume = volume;
    let result = format_fat32_on_file(
        &mut volume,
        &volume_path,
        length as u64,
        hidden_sectors,
        label,
        trace,
    );
    unlock();
    drop(volume);
    result
}

/// Write via PhysicalDrive only — no volume lock/dismount first.
/// Quiet-dismount-before-write caused os error 55 on the PNY and dropped the letter.
#[cfg(windows)]
fn format_via_physical_only(
    disk_number: u32,
    letter: char,
    offset: u64,
    length: u64,
    label: &str,
    trace: &mut Fat32Trace,
) -> Result<(), String> {
    let physical_path = format!(r"\\.\PhysicalDrive{disk_number}");
    trace.ok(
        "using saved partition extent",
        format!("offset={offset} length={length}"),
    );
    let physical = open_device_rw_ex(&physical_path, true, false, trace)?;
    format_fat32_on_partition_view(physical, &physical_path, offset, length, label, trace)?;
    remount_fat32_letter(disk_number, letter, label, trace)
}

/// Write via PhysicalDrive at a known partition extent while the letter is locked
/// without dismounting.
#[cfg(windows)]
fn format_via_physical_while_volume_locked(
    disk_number: u32,
    letter: char,
    label: &str,
    offset: u64,
    length: u64,
    trace: &mut Fat32Trace,
) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::FSCTL_UNLOCK_VOLUME;

    let physical_path = format!(r"\\.\PhysicalDrive{disk_number}");
    let volume_path = format!(r"\\.\{letter}:");

    trace.ok(
        "using saved partition extent",
        format!("offset={offset} length={length}"),
    );
    let physical = open_device_rw_ex(&physical_path, true, false, trace)?;

    let volume = open_device_rw_ex(&volume_path, true, false, trace)?;
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

    allow_extended_dasd_io(volume_handle, trace);
    if let Err(err) = lock_volume_with_retries(volume_handle, &volume_path, trace) {
        unlock();
        return Err(err);
    }
    trace.note("dismount: skipped (physical path keeps volume mounted)");

    let result =
        format_fat32_on_partition_view(physical, &physical_path, offset, length, label, trace);
    unlock();
    drop(volume);
    result?;
    remount_fat32_letter(disk_number, letter, label, trace)
}

#[cfg(windows)]
fn persist_fat32_trace_failure(
    disk_number: u32,
    letter: char,
    label: &str,
    trace: &Fat32Trace,
) -> String {
    let mut message = trace.render_failure(disk_number, letter, label);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut lines = vec![format!(
        "FAT32 format failed (disk {disk_number} letter {letter} label={label:?})"
    )];
    lines.extend(trace.lines.iter().cloned());
    match crate::log_file::write_fat32_trace(&cwd, &lines) {
        Ok(path) => {
            message.push('\n');
            message.push_str(&format!("trace file: {}", path.display()));
        }
        Err(err) => {
            message.push('\n');
            message.push_str(&format!("trace file write failed: {err}"));
        }
    }
    message
}

#[cfg(windows)]
#[allow(dead_code)]
fn capture_command_output(
    name: &str,
    output: &std::process::Output,
    trace: &mut Fat32Trace,
) -> Result<(), String> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.trim().is_empty() {
        trace.note(format!("{name} stdout: {}", stdout.trim()));
    }
    if !stderr.trim().is_empty() {
        trace.note(format!("{name} stderr: {}", stderr.trim()));
    }
    if output.status.success() {
        trace.ok(name, format!("exit {}", output.status));
        Ok(())
    } else {
        let detail = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else if !stdout.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            format!("exit {}", output.status)
        };
        let message = format!("{name}: {detail}");
        trace.fail(name, &detail);
        Err(message)
    }
}

/// Classic `format.com` — some Windows builds still allow >32 GiB FAT32 here
/// even when `Format-Volume` returns StorageWMI "Size Not Supported".
#[cfg(windows)]
#[allow(dead_code)]
fn format_via_format_com(
    letter: char,
    label: &str,
    trace: &mut Fat32Trace,
) -> Result<(), String> {
    let mut args = vec![
        format!("{letter}:"),
        "/FS:FAT32".to_string(),
        "/Q".to_string(),
        "/Y".to_string(),
    ];
    if !label.is_empty() {
        // format.com accepts /V:label; our labels are already restricted ASCII.
        args.push(format!("/V:{label}"));
    }
    trace.note(format!("format.com args: {}", args.join(" ")));
    let output = hidden_command("format.com")
        .args(&args)
        .output()
        .map_err(|err| {
            let message = format!("format.com spawn: {err}");
            trace.fail("format.com", &message);
            message
        })?;
    capture_command_output("format.com", &output, trace)
}

/// Windows Storage cmdlet — on this host it returned StorageWMI 4097
/// "Size Not Supported" for ~58 GiB; kept as a secondary OS attempt.
#[cfg(windows)]
#[allow(dead_code)]
fn format_via_format_volume(
    letter: char,
    label: &str,
    trace: &mut Fat32Trace,
) -> Result<(), String> {
    let label_ps = label.replace('\'', "''");
    let command = if label.is_empty() {
        format!(
            "Format-Volume -DriveLetter {letter} -FileSystem FAT32 -Confirm:$false -Force | Out-Null"
        )
    } else {
        format!(
            "Format-Volume -DriveLetter {letter} -FileSystem FAT32 -NewFileSystemLabel '{label_ps}' -Confirm:$false -Force | Out-Null"
        )
    };
    trace.note(format!("Format-Volume command: {command}"));
    let output = hidden_command("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &command,
        ])
        .output()
        .map_err(|err| {
            let message = format!("powershell Format-Volume spawn: {err}");
            trace.fail("Format-Volume", &message);
            message
        })?;
    capture_command_output("Format-Volume", &output, trace)
}

#[cfg(windows)]
fn format_large_fat32_volume_windows(
    disk_number: u32,
    letter: char,
    label: &str,
) -> Result<(), String> {
    let mut trace = Fat32Trace::new();
    trace.note(format!(
        "begin disk={disk_number} letter={letter} label={label:?}"
    ));

    // Letter is not assigned yet (caller wiped/created only). Write via
    // PhysicalDrive, then assign once — never mount RAW.
    let physical_path = format!(r"\\.\PhysicalDrive{disk_number}");
    trace.section("physical format (no letter yet)");
    let physical = match open_device_rw_ex(&physical_path, true, false, &mut trace) {
        Ok(file) => file,
        Err(err) => {
            trace.fail("probe physical", &err);
            return Err(persist_fat32_trace_failure(disk_number, letter, label, &trace));
        }
    };
    let (offset, length) = match first_partition_extent(&physical, &mut trace) {
        Ok(extent) => extent,
        Err(err) => {
            trace.fail("probe physical", &err);
            return Err(persist_fat32_trace_failure(disk_number, letter, label, &trace));
        }
    };
    trace.ok(
        "using partition extent",
        format!("offset={offset} length={length}"),
    );

    if let Err(err) = format_fat32_on_partition_view(
        physical,
        &physical_path,
        offset,
        length,
        label,
        &mut trace,
    ) {
        trace.fail("physical format", &err);
        // One reopen retry before giving up — no volume-letter paths (no letter).
        trace.section("physical format reopen");
        match open_device_rw_ex(&physical_path, true, false, &mut trace)
            .and_then(|file| {
                format_fat32_on_partition_view(
                    file,
                    &physical_path,
                    offset,
                    length,
                    label,
                    &mut trace,
                )
            }) {
            Ok(()) => trace.ok("physical format reopen", "ok"),
            Err(err) => {
                trace.fail("physical format reopen", &err);
                return Err(persist_fat32_trace_failure(disk_number, letter, label, &trace));
            }
        }
    }

    trace.section("assign letter after format");
    match assign_letter_after_format(disk_number, letter, label, &mut trace) {
        Ok(()) => {
            trace.ok("assign after format", "success");
            Ok(())
        }
        Err(err) => {
            trace.fail("assign after format", &err);
            Err(persist_fat32_trace_failure(disk_number, letter, label, &trace))
        }
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

fn diskpart_vds_startup_failure(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("starting the disk management services")
        || lower.contains("starting the com service")
        || lower.contains("could not start the virtual disk service")
        || lower.contains("unable to connect to virtual disk service")
}

fn ensure_virtual_disk_service() -> Result<(), String> {
    let query = hidden_command("sc.exe")
        .args(["query", "vds"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| format!("sc query vds: {err}"))?;
    let stdout = String::from_utf8_lossy(&query.stdout);
    if stdout.to_ascii_uppercase().contains("RUNNING") {
        return Ok(());
    }
    let start = hidden_command("net.exe")
        .args(["start", "vds"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| format!("net start vds: {err}"))?;
    if start.status.success()
        || String::from_utf8_lossy(&start.stdout)
            .to_ascii_lowercase()
            .contains("already been started")
        || String::from_utf8_lossy(&start.stderr)
            .to_ascii_lowercase()
            .contains("already been started")
    {
        std::thread::sleep(std::time::Duration::from_millis(500));
        return Ok(());
    }
    Err(format!(
        "net start vds failed: {}",
        String::from_utf8_lossy(&start.stderr).trim()
    ))
}

fn restart_virtual_disk_service() -> Result<(), String> {
    let _ = hidden_command("net.exe")
        .args(["stop", "vds"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output();
    std::thread::sleep(std::time::Duration::from_secs(1));
    ensure_virtual_disk_service()?;
    std::thread::sleep(std::time::Duration::from_secs(2));
    Ok(())
}

/// Run one `diskpart /s` and wait until it exits. Never kills the child.
/// Captures stdout/stderr — diskpart often exits 0 even when a command failed.
fn run_diskpart_once(script: &Path) -> Result<(), String> {
    let output = hidden_command("diskpart.exe")
        .arg("/s")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| format!("spawn diskpart: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");
    // `online disk noerr` can leave a VDS "already online" noise line and a
    // non-zero exit even when the wipe steps succeeded.
    let script_text = std::fs::read_to_string(script).unwrap_or_default();
    let expects_assign = script_text.lines().any(|line| {
        let t = line.trim().to_ascii_lowercase();
        t == "assign" || t.starts_with("assign ")
    });
    let wipe_ok = if expects_assign {
        diskpart_wipe_assign_succeeded(&combined)
    } else {
        diskpart_wipe_partition_succeeded(&combined)
    };
    if wipe_ok || diskpart_reassign_succeeded(&combined) {
        return Ok(());
    }
    let script_error = diskpart_script_error(&combined);
    if output.status.success() && script_error.is_none() {
        return Ok(());
    }
    let status = output.status;
    let detail = script_error.unwrap_or_else(|| {
        if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else if !stdout.trim().is_empty() {
            // Keep the tail — diskpart is chatty on success path prefixes.
            stdout
                .lines()
                .rev()
                .take(12)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            format!("exit {status}")
        }
    });
    Err(detail)
}

/// True when wipe + assign finished (legacy OS-format scripts).
fn diskpart_wipe_assign_succeeded(output: &str) -> bool {
    diskpart_wipe_partition_succeeded(output)
        && output
            .to_ascii_lowercase()
            .contains("successfully assigned the drive letter")
}

/// True when clean/convert/create finished (large FAT32: letter assigned later).
fn diskpart_wipe_partition_succeeded(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("succeeded in cleaning the disk")
        && lower.contains("successfully converted the selected disk")
        && lower.contains("succeeded in creating the specified partition")
}

fn diskpart_reassign_succeeded(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("successfully assigned the drive letter")
}

fn diskpart_script_error(output: &str) -> Option<String> {
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            let l = line.to_ascii_lowercase();
            // `online disk noerr` still prints the VDS header + "already online".
            !l.contains("already online") && l != "virtual disk service error:"
        })
        .collect();
    let joined = lines.join("\n");
    let lower = joined.to_ascii_lowercase();
    if !(lower.contains("diskpart has encountered an error")
        || lower.contains("starting the disk management services")
        || lower.contains("virtual disk service error")
        || lower.contains("there is no disk selected")
        || lower.contains("the disk is not writable")
        || lower.contains("access is denied")
        || lower.contains("the device is not ready")
        || lower.contains("no media"))
    {
        return None;
    }
    let start = lines
        .iter()
        .rposition(|line| {
            let l = line.to_ascii_lowercase();
            l.contains("diskpart has encountered an error")
                || l.contains("starting the disk management services")
                || l.contains("virtual disk service error")
                || l.contains("there is no disk selected")
                || l.contains("the disk is not writable")
                || l.contains("access is denied")
                || l.contains("the device is not ready")
                || l.contains("no media")
        })
        .unwrap_or(0);
    Some(lines[start..].join("\n"))
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
    use super::{
        describe_os_error, diskpart_script_error, diskpart_wipe_assign_succeeded,
        diskpart_wipe_partition_succeeded, fat32_total_sectors, pad_fat_volume_label,
        Fat32Trace, INVENTORY_SCRIPT,
    };
    use std::fs;
    use std::io;

    #[test]
    fn diskpart_success_ignores_already_online_vds_noise() {
        let output = r#"
Virtual Disk Service error:
This disk is already online.

Disk attributes cleared successfully.

DiskPart succeeded in cleaning the disk.

Disk 1 is now the selected disk.

DiskPart successfully converted the selected disk to MBR format.

DiskPart succeeded in creating the specified partition.

DiskPart successfully assigned the drive letter or mount point.

Leaving DiskPart...
"#;
        assert!(diskpart_wipe_assign_succeeded(output));
        assert!(diskpart_script_error(output).is_none());
    }

    #[test]
    fn diskpart_wipe_without_assign_is_success_for_builtin_fat32() {
        let output = r#"
DiskPart succeeded in cleaning the disk.
DiskPart successfully converted the selected disk to MBR format.
DiskPart succeeded in creating the specified partition.
Leaving DiskPart...
"#;
        assert!(diskpart_wipe_partition_succeeded(output));
        assert!(!diskpart_wipe_assign_succeeded(output));
    }

    #[test]
    fn pad_fat_volume_label_truncates_and_pads() {
        assert_eq!(pad_fat_volume_label(""), *b"           ");
        assert_eq!(pad_fat_volume_label("STORE"), *b"STORE      ");
        assert_eq!(pad_fat_volume_label("EXACTLYELEV"), *b"EXACTLYELEV");
        assert_eq!(pad_fat_volume_label("TOOLONGVOLUME"), *b"TOOLONGVOLU");
    }

    #[test]
    fn fat32_trace_failure_message_includes_sections_and_os_code() {
        let mut trace = Fat32Trace::new();
        trace.section("volume path");
        trace.fail(
            r"fatfs format \\.\D:",
            "ERROR_DEVICE_DOES_NOT_EXIST (433)",
        );
        trace.section("physical fallback");
        trace.fail("open", "os error 433");
        let message = trace.render_failure(1, 'D', "LAB");
        assert!(message.contains("FAT32 format failed (disk 1 letter D"));
        assert!(message.contains("--- volume path ---"));
        assert!(message.contains("--- physical fallback ---"));
        assert!(message.contains("433"));
        assert!(message.contains("ERROR_DEVICE_DOES_NOT_EXIST"));
    }

    #[test]
    fn describe_os_error_names_common_win32_codes() {
        let err = io::Error::from_raw_os_error(433);
        assert!(describe_os_error(&err).contains("ERROR_DEVICE_DOES_NOT_EXIST (433)"));
        let err = io::Error::from_raw_os_error(87);
        assert!(describe_os_error(&err).contains("ERROR_INVALID_PARAMETER (87)"));
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

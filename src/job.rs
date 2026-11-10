use crate::commands::ExternalCommand;
use crate::copy::{COPY_RETRY, CopyError, copy_tree};
use crate::disk_id::{DiskId, EnumeratedDisk, Gate, Rematch, gate, rematch};
use crate::format_plan::{PreparePlan, prepare_plan};
use crate::label::{Filesystem, PartitionStyle};
use crate::source::{EntryKind, SourceEntry};
use crate::verify::{mismatch_summary, verify_tree};
use std::path::PathBuf;

pub trait VolumeHost {
    fn list_all(&mut self) -> Result<Vec<EnumeratedDisk>, String>;
    fn diskpart(&mut self, script: &str) -> Result<(), String>;
    fn letter_for(&mut self, id: &DiskId) -> Result<Option<char>, String>;
    fn run_command(&mut self, cmd: &ExternalCommand) -> Result<(), String>;
    fn format_large_fat32(&mut self, letter: char, label: &str) -> Result<(), String>;
    fn exe_path(&self) -> PathBuf;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareFailure {
    Fat32TooLarge,
    Rematch(Rematch),
    Diskpart,
    NoLetter,
    Command(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub id: DiskId,
    pub letter: char,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanedDisk {
    pub number: u32,
    pub model: String,
    pub serial: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareAll {
    AbortSystemDisk {
        number: u32,
        model: String,
        serial: Option<String>,
        cleaned: Vec<CleanedDisk>,
    },
    Done(Vec<Result<Prepared, PrepareFailure>>),
}

pub fn system_disk_stop_message(
    number: u32,
    model: &str,
    serial: Option<&str>,
    cleaned: &[CleanedDisk],
) -> String {
    use crate::session::disk_log_name;

    let system = format!(
        "{} contains the Windows system volume.",
        disk_log_name(number, model, serial)
    );
    if cleaned.is_empty() {
        return format!("Stopped before any wipe. {system}");
    }
    let mut lines = Vec::with_capacity(cleaned.len() + 1);
    for disk in cleaned {
        lines.push(format!(
            "{} was already cleaned.",
            disk_log_name(disk.number, &disk.model, disk.serial.as_deref())
        ));
    }
    lines.push(system);
    lines.join(" ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareNotice {
    Formatting { index: usize, total: usize },
    Finished { index: usize },
}

enum DiskStep {
    Abort {
        number: u32,
        model: String,
        serial: Option<String>,
    },
    Finished {
        result: Result<Prepared, PrepareFailure>,
        cleaned: Option<CleanedDisk>,
    },
}

pub fn prepare_all(
    selected: &[DiskId],
    filesystem: Filesystem,
    style: PartitionStyle,
    label: &str,
    host: &mut dyn VolumeHost,
    on_prepare: &mut dyn FnMut(PrepareNotice),
) -> PrepareAll {
    let current = match host.list_all() {
        Ok(disks) => disks,
        Err(message) => {
            return PrepareAll::Done(
                selected
                    .iter()
                    .map(|_| Err(PrepareFailure::Command(message.clone())))
                    .collect(),
            );
        }
    };
    if let Gate::AbortSystemDisk {
        number,
        model,
        serial,
    } = gate(selected, &current)
    {
        return PrepareAll::AbortSystemDisk {
            number,
            model,
            serial,
            cleaned: Vec::new(),
        };
    }

    let total = selected.len();
    let mut prepared = Vec::with_capacity(total);
    let mut cleaned = Vec::new();
    for (index, id) in selected.iter().enumerate() {
        on_prepare(PrepareNotice::Formatting { index, total });
        match prepare_disk(id, filesystem, style, label, host) {
            DiskStep::Abort {
                number,
                model,
                serial,
            } => {
                return PrepareAll::AbortSystemDisk {
                    number,
                    model,
                    serial,
                    cleaned,
                };
            }
            DiskStep::Finished {
                result,
                cleaned: just_cleaned,
            } => {
                if let Some(disk) = just_cleaned {
                    cleaned.push(disk);
                }
                on_prepare(PrepareNotice::Finished { index });
                prepared.push(result);
            }
        }
    }
    PrepareAll::Done(prepared)
}

fn prepare_disk(
    id: &DiskId,
    filesystem: Filesystem,
    style: PartitionStyle,
    label: &str,
    host: &mut dyn VolumeHost,
) -> DiskStep {
    let current = match host.list_all() {
        Ok(disks) => disks,
        Err(message) => {
            return DiskStep::Finished {
                result: Err(PrepareFailure::Command(message)),
                cleaned: None,
            };
        }
    };
    let number = match rematch(id, &current) {
        Rematch::Unique(number) => number,
        other => {
            return DiskStep::Finished {
                result: Err(PrepareFailure::Rematch(other)),
                cleaned: None,
            };
        }
    };
    let Some(disk) = current.into_iter().find(|disk| disk.number == number) else {
        return DiskStep::Finished {
            result: Err(PrepareFailure::Rematch(Rematch::Missing)),
            cleaned: None,
        };
    };
    if disk.contains_system_volume {
        return DiskStep::Abort {
            number: disk.number,
            model: disk.model,
            serial: disk.id.serial,
        };
    }
    let (result, cleaned) =
        match prepare_plan(disk.number, style, filesystem, label, disk.size_bytes) {
            PreparePlan::Fat32TooLarge => (Err(PrepareFailure::Fat32TooLarge), None),
            PreparePlan::Script(script) => finish_script(host, &disk, &script),
            PreparePlan::LargeFat32 { script } => finish_large_fat32(host, &disk, &script, label),
        };
    DiskStep::Finished { result, cleaned }
}

fn finish_script(
    host: &mut dyn VolumeHost,
    disk: &EnumeratedDisk,
    script: &str,
) -> (Result<Prepared, PrepareFailure>, Option<CleanedDisk>) {
    if host.diskpart(script).is_err() {
        return (Err(PrepareFailure::Diskpart), None);
    }
    let cleaned = Some(cleaned_disk(disk));
    match take_letter(host, &disk.id) {
        Ok(letter) => (Ok(prepared_disk(disk, letter)), cleaned),
        Err(failure) => (Err(failure), cleaned),
    }
}

fn finish_large_fat32(
    host: &mut dyn VolumeHost,
    disk: &EnumeratedDisk,
    script: &str,
    label: &str,
) -> (Result<Prepared, PrepareFailure>, Option<CleanedDisk>) {
    if host.diskpart(script).is_err() {
        return (Err(PrepareFailure::Diskpart), None);
    }
    let cleaned = Some(cleaned_disk(disk));
    let letter = match take_letter(host, &disk.id) {
        Ok(letter) => letter,
        Err(failure) => return (Err(failure), cleaned),
    };
    if let Err(message) = host.format_large_fat32(letter, label) {
        return (Err(PrepareFailure::Command(message)), cleaned);
    }
    (Ok(prepared_disk(disk, letter)), cleaned)
}

fn cleaned_disk(disk: &EnumeratedDisk) -> CleanedDisk {
    CleanedDisk {
        number: disk.number,
        model: disk.model.clone(),
        serial: disk.id.serial.clone(),
    }
}

fn take_letter(host: &mut dyn VolumeHost, id: &DiskId) -> Result<char, PrepareFailure> {
    match host.letter_for(id) {
        Ok(Some(letter)) => Ok(letter),
        Ok(None) => Err(PrepareFailure::NoLetter),
        Err(message) => Err(PrepareFailure::Command(message)),
    }
}

fn prepared_disk(disk: &EnumeratedDisk, letter: char) -> Prepared {
    Prepared {
        id: disk.id.clone(),
        letter,
        model: disk.model.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyTarget {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub entries: Vec<SourceEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveOutcome {
    Passed { checked: usize },
    Failed { reason: String },
}

pub fn copy_and_verify_all(
    targets: Vec<CopyTarget>,
    on_enter: &(dyn Fn() + Sync),
    on_bytes: &(dyn Fn(usize, u64, u64) + Sync),
) -> Vec<DriveOutcome> {
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(targets.len());
        for (index, target) in targets.into_iter().enumerate() {
            handles.push(scope.spawn(move || {
                on_enter();
                drive_outcome(index, &target, on_bytes)
            }));
        }
        handles
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(outcome) => outcome,
                Err(payload) => std::panic::resume_unwind(payload),
            })
            .collect()
    })
}

fn drive_outcome(
    index: usize,
    target: &CopyTarget,
    on_bytes: &(dyn Fn(usize, u64, u64) + Sync),
) -> DriveOutcome {
    let source_total = target.entries.iter().fold(0u64, |sum, entry| {
        sum + match entry.kind {
            EntryKind::File => entry.size,
            EntryKind::Directory => 0,
        }
    });
    let copied = std::cell::Cell::new(0u64);
    match copy_tree(
        &target.source,
        &target.dest,
        &target.entries,
        COPY_RETRY,
        &|wait| std::thread::sleep(wait),
        &|bytes| {
            let total = copied.get().saturating_add(bytes);
            copied.set(total);
            on_bytes(index, total, source_total);
        },
    ) {
        Err(CopyError::File { path, attempts }) => DriveOutcome::Failed {
            reason: format!("Copy failed after {attempts} tries: {}", path.display()),
        },
        Ok(_) => verify_copied(target),
    }
}

fn verify_copied(target: &CopyTarget) -> DriveOutcome {
    let report = verify_tree(&target.dest, &target.entries);
    if report.passed() {
        return DriveOutcome::Passed {
            checked: report.checked,
        };
    }
    let summary = mismatch_summary(&report);
    let mut reason = format!(
        "Verification failed. checked {}, missing {}, size mismatches {}.",
        report.checked,
        report.missing.len(),
        report.size_mismatch.len()
    );
    if !summary.is_empty() {
        reason.push('\n');
        reason.push_str(&summary);
    }
    DriveOutcome::Failed { reason }
}

pub fn failure_text(failure: &PrepareFailure) -> &'static str {
    match failure {
        PrepareFailure::Fat32TooLarge => "Choose Large FAT32. Disk is larger than 32 GB.",
        PrepareFailure::Rematch(Rematch::NotUnique | Rematch::Unique(_)) => {
            "Disk could not be matched uniquely."
        }
        PrepareFailure::Rematch(Rematch::NotRemovable) => "Disk is no longer removable.",
        PrepareFailure::Rematch(Rematch::Missing) => "Disk is no longer present.",
        PrepareFailure::Diskpart => "diskpart failed after 3 tries.",
        PrepareFailure::NoLetter => "No drive letter after format.",
        // Callers show Command's String. This arm only keeps the match exhaustive.
        PrepareFailure::Command(_) => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ExternalCommand;
    use crate::disk_id::{DiskId, EnumeratedDisk, Rematch};
    use crate::format_plan::FAT32_MAX_INCLUSIVE_BYTES;
    use crate::label::{Filesystem, PartitionStyle};
    use crate::source::{EntryKind, SourceEntry, walk_source};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const REMOVABLE: &str = "Removable Media";

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Mark {
        Formatting { index: usize, total: usize },
        Script,
        Finished(usize),
    }

    struct FakeHost {
        rounds: Vec<Result<Vec<EnumeratedDisk>, String>>,
        cursor: usize,
        scripts: Vec<String>,
        fail_diskpart_for: Vec<u32>,
        letters: Vec<(String, Result<Option<char>, String>)>,
        commands: Vec<ExternalCommand>,
        command_error: Option<String>,
        format_calls: Vec<(char, String)>,
        format_error: Option<String>,
        format_errors_queue: Vec<String>,
        exe: PathBuf,
        log: Arc<Mutex<Vec<Mark>>>,
    }

    impl FakeHost {
        fn new(rounds: Vec<Result<Vec<EnumeratedDisk>, String>>) -> Self {
            Self {
                rounds,
                cursor: 0,
                scripts: Vec::new(),
                fail_diskpart_for: Vec::new(),
                letters: Vec::new(),
                commands: Vec::new(),
                command_error: None,
                format_calls: Vec::new(),
                format_error: None,
                format_errors_queue: Vec::new(),
                exe: PathBuf::from(r"C:\MultiUSBTest\MultiUSB.exe"),
                log: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn letter(&mut self, pnp: &str, letter: char) {
            self.letters.push((pnp.to_string(), Ok(Some(letter))));
        }
    }

    impl VolumeHost for FakeHost {
        fn list_all(&mut self) -> Result<Vec<EnumeratedDisk>, String> {
            if self.rounds.is_empty() {
                self.cursor += 1;
                return Ok(Vec::new());
            }
            let index = self.cursor.min(self.rounds.len() - 1);
            self.cursor += 1;
            self.rounds[index].clone()
        }

        fn diskpart(&mut self, script: &str) -> Result<(), String> {
            self.log.lock().expect("log").push(Mark::Script);
            self.scripts.push(script.to_string());
            if self
                .fail_diskpart_for
                .iter()
                .any(|number| script.contains(&format!("select disk {number}\r\n")))
            {
                Err("diskpart failed".into())
            } else {
                Ok(())
            }
        }

        fn letter_for(&mut self, id: &DiskId) -> Result<Option<char>, String> {
            let key = id.pnp_device_id.clone().unwrap_or_default();
            self.letters
                .iter()
                .find(|(pnp, _)| pnp == &key)
                .map(|(_, result)| result.clone())
                .unwrap_or(Ok(None))
        }

        fn run_command(&mut self, cmd: &ExternalCommand) -> Result<(), String> {
            self.commands.push(ExternalCommand {
                program: cmd.program.clone(),
                args: cmd.args.clone(),
                stdin: cmd.stdin.clone(),
            });
            match &self.command_error {
                Some(message) => Err(message.clone()),
                None => Ok(()),
            }
        }

        fn format_large_fat32(&mut self, letter: char, label: &str) -> Result<(), String> {
            self.format_calls.push((letter, label.to_string()));
            if !self.format_errors_queue.is_empty() {
                return Err(self.format_errors_queue.remove(0));
            }
            match &self.format_error {
                Some(message) => Err(message.clone()),
                None => Ok(()),
            }
        }

        fn exe_path(&self) -> PathBuf {
            self.exe.clone()
        }
    }

    fn disk(
        number: u32,
        model: &str,
        pnp: &str,
        size: u64,
        media: &str,
        system: bool,
    ) -> EnumeratedDisk {
        EnumeratedDisk {
            number,
            model: model.to_string(),
            size_bytes: size,
            letters: Vec::new(),
            id: DiskId {
                pnp_device_id: Some(pnp.to_string()),
                serial: None,
                size_bytes: size,
            },
            media_type: media.to_string(),
            contains_system_volume: system,
        }
    }

    fn removable(number: u32, model: &str, pnp: &str, size: u64) -> EnumeratedDisk {
        disk(number, model, pnp, size, REMOVABLE, false)
    }

    fn ids(disks: &[EnumeratedDisk]) -> Vec<DiskId> {
        disks.iter().map(|d| d.id.clone()).collect()
    }

    fn diskpart_script(number: u32, convert: &str, format_line: &str) -> String {
        format!(
            "rescan\r\nselect disk {number}\r\nclean\r\nconvert {convert}\r\ncreate partition primary\r\n{format_line}\r\nassign\r\nexit\r\n"
        )
    }

    fn assign_only_script(number: u32, convert: &str) -> String {
        format!(
            "rescan\r\nselect disk {number}\r\nclean\r\nconvert {convert}\r\ncreate partition primary\r\nassign\r\nexit\r\n"
        )
    }

    fn notices(host: &FakeHost) -> impl FnMut(PrepareNotice) + use<> {
        let log = Arc::clone(&host.log);
        move |notice| match notice {
            PrepareNotice::Formatting { index, total } => {
                log.lock()
                    .expect("log")
                    .push(Mark::Formatting { index, total });
            }
            PrepareNotice::Finished { index } => {
                log.lock().expect("log").push(Mark::Finished(index));
            }
        }
    }

    fn marks(host: &FakeHost) -> Vec<Mark> {
        host.log.lock().expect("log").clone()
    }

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("multiusb_job_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch dir");
        path
    }

    fn one_file(dir: &Path, name: &str, bytes: &[u8]) -> CopyTarget {
        let source = dir.join(format!("{name}_src"));
        let dest = dir.join(format!("{name}_dest"));
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(source.join("a.txt"), bytes).unwrap();
        let walk = walk_source(&source).unwrap();
        CopyTarget {
            source,
            dest,
            entries: walk.entries,
        }
    }

    #[test]
    fn prepare_all_does_not_diskpart_when_gate_aborts_system_disk() {
        let safe = removable(1, "Safe", "USB\\A", 10);
        let system = disk(2, "System USB", "USB\\B", 20, REMOVABLE, true);
        let selected = ids(&[safe.clone(), system.clone()]);
        let mut host = FakeHost::new(vec![Ok(vec![safe, system])]);
        let mut on_prepare = notices(&host);

        let result = prepare_all(
            &selected,
            Filesystem::Ntfs,
            PartitionStyle::Gpt,
            "STORE",
            &mut host,
            &mut on_prepare,
        );

        match result {
            PrepareAll::AbortSystemDisk {
                number: 2,
                model,
                serial,
                cleaned,
            } if model == "System USB" => {
                assert!(cleaned.is_empty());
                assert_eq!(serial, None);
                assert_eq!(
                    system_disk_stop_message(2, &model, serial.as_deref(), &cleaned),
                    "Stopped before any wipe. Disk 2 (System USB) · S/N unavailable contains the Windows system volume."
                );
            }
            other => panic!("expected the first inventory to abort, got {other:?}"),
        }
        assert!(host.scripts.is_empty());
        assert!(marks(&host).is_empty());
    }

    #[test]
    fn fat32_over_limit_records_no_script_and_the_next_disk_still_gets_one() {
        let big = removable(2, "Big", "USB\\BIG", FAT32_MAX_INCLUSIVE_BYTES + 1);
        let small = removable(3, "Small", "USB\\SMALL", 1024);
        let selected = ids(&[big.clone(), small.clone()]);
        let mut host = FakeHost::new(vec![Ok(vec![big, small])]);
        host.letter("USB\\SMALL", 'F');
        let mut on_prepare = notices(&host);

        let result = prepare_all(
            &selected,
            Filesystem::Fat32,
            PartitionStyle::Gpt,
            "STORE",
            &mut host,
            &mut on_prepare,
        );

        match result {
            PrepareAll::Done(results) => {
                assert!(matches!(results[0], Err(PrepareFailure::Fat32TooLarge)));
                match &results[1] {
                    Ok(prepared) => {
                        assert_eq!(prepared.letter, 'F');
                        assert_eq!(prepared.model, "Small");
                        assert_eq!(prepared.id.pnp_device_id.as_deref(), Some("USB\\SMALL"));
                    }
                    Err(failure) => panic!("second disk should prepare, got {failure:?}"),
                }
            }
            PrepareAll::AbortSystemDisk { .. } => panic!("expected both disks to finish"),
        }
        assert_eq!(
            host.scripts,
            vec![diskpart_script(
                3,
                "gpt",
                "format fs=fat32 quick label=\"STORE\""
            )]
        );
        assert_eq!(
            marks(&host),
            vec![
                Mark::Formatting { index: 0, total: 2 },
                Mark::Finished(0),
                Mark::Formatting { index: 1, total: 2 },
                Mark::Script,
                Mark::Finished(1),
            ]
        );
    }

    #[test]
    fn diskpart_error_on_the_first_disk_still_scripts_the_second() {
        let first = removable(3, "One", "USB\\ONE", 100);
        let second = removable(4, "Two", "USB\\TWO", 100);
        let selected = ids(&[first.clone(), second.clone()]);
        let mut host = FakeHost::new(vec![Ok(vec![first, second])]);
        host.fail_diskpart_for = vec![3];
        host.letter("USB\\TWO", 'G');
        let mut on_prepare = notices(&host);

        let result = prepare_all(
            &selected,
            Filesystem::Ntfs,
            PartitionStyle::Gpt,
            "STORE",
            &mut host,
            &mut on_prepare,
        );

        match result {
            PrepareAll::Done(results) => {
                assert!(matches!(results[0], Err(PrepareFailure::Diskpart)));
                assert!(matches!(
                    &results[1],
                    Ok(prepared) if prepared.letter == 'G' && prepared.model == "Two"
                ));
            }
            PrepareAll::AbortSystemDisk { .. } => panic!("expected both disks to finish"),
        }
        assert_eq!(
            host.scripts,
            vec![
                diskpart_script(3, "gpt", "format fs=ntfs quick label=\"STORE\""),
                diskpart_script(4, "gpt", "format fs=ntfs quick label=\"STORE\""),
            ]
        );
        assert_eq!(
            marks(&host),
            vec![
                Mark::Formatting { index: 0, total: 2 },
                Mark::Script,
                Mark::Finished(0),
                Mark::Formatting { index: 1, total: 2 },
                Mark::Script,
                Mark::Finished(1),
            ]
        );
    }

    #[test]
    fn large_fat32_runs_assign_script_then_format_large_fat32() {
        let stick = removable(4, "Wide", "USB\\WIDE", 1024);
        let selected = ids(&[stick.clone()]);
        let mut host = FakeHost::new(vec![Ok(vec![stick])]);
        host.letter("USB\\WIDE", 'E');
        let mut on_prepare = notices(&host);

        let result = prepare_all(
            &selected,
            Filesystem::LargeFat32,
            PartitionStyle::Mbr,
            "STORE",
            &mut host,
            &mut on_prepare,
        );

        match result {
            PrepareAll::Done(results) => match &results[0] {
                Ok(prepared) => {
                    assert_eq!(prepared.letter, 'E');
                    assert_eq!(prepared.model, "Wide");
                }
                Err(failure) => panic!("large fat32 should prepare, got {failure:?}"),
            },
            PrepareAll::AbortSystemDisk { .. } => panic!("expected the disk to finish"),
        }
        assert_eq!(host.scripts, vec![assign_only_script(4, "mbr")]);
        assert_eq!(host.format_calls, vec![('E', "STORE".to_string())]);
        assert!(host.commands.is_empty());
    }

    #[test]
    fn large_fat32_format_error_is_command_failure() {
        let stick = removable(4, "Wide", "USB\\WIDE", FAT32_MAX_INCLUSIVE_BYTES + 1);
        let selected = ids(&[stick.clone()]);
        let mut host = FakeHost::new(vec![Ok(vec![stick])]);
        host.letter("USB\\WIDE", 'E');
        host.format_error = Some("lock failed".to_string());
        let mut on_prepare = notices(&host);

        let result = prepare_all(
            &selected,
            Filesystem::LargeFat32,
            PartitionStyle::Gpt,
            "STORE",
            &mut host,
            &mut on_prepare,
        );

        match result {
            PrepareAll::Done(results) => match &results[0] {
                Err(PrepareFailure::Command(message)) => assert_eq!(message, "lock failed"),
                other => panic!("expected command failure, got {other:?}"),
            },
            PrepareAll::AbortSystemDisk { .. } => panic!("expected the disk to finish"),
        }
        assert_eq!(host.scripts, vec![assign_only_script(4, "gpt")]);
        assert_eq!(host.format_calls, vec![('E', "STORE".to_string())]);
        assert!(host.commands.is_empty());
    }

    #[test]
    fn large_fat32_format_error_on_first_disk_still_prepares_second() {
        let first = removable(4, "Wide", "USB\\WIDE", 1024);
        let second = removable(5, "Narrow", "USB\\NARROW", 1024);
        let selected = ids(&[first.clone(), second.clone()]);
        let mut host = FakeHost::new(vec![Ok(vec![first, second])]);
        host.letter("USB\\WIDE", 'E');
        host.letter("USB\\NARROW", 'F');
        host.format_errors_queue
            .push("lock failed".to_string());
        let mut on_prepare = notices(&host);

        let result = prepare_all(
            &selected,
            Filesystem::LargeFat32,
            PartitionStyle::Gpt,
            "STORE",
            &mut host,
            &mut on_prepare,
        );

        match result {
            PrepareAll::Done(results) => {
                assert!(matches!(
                    &results[0],
                    Err(PrepareFailure::Command(message)) if message == "lock failed"
                ));
                assert!(matches!(
                    &results[1],
                    Ok(prepared) if prepared.letter == 'F'
                ));
            }
            PrepareAll::AbortSystemDisk { .. } => panic!("expected both disks to finish"),
        }
        assert_eq!(
            host.scripts,
            vec![assign_only_script(4, "gpt"), assign_only_script(5, "gpt")]
        );
        assert_eq!(
            host.format_calls,
            vec![('E', "STORE".to_string()), ('F', "STORE".to_string())]
        );
    }

    #[test]
    fn other_filesystems_do_not_call_format_large_fat32() {
        let stick = removable(2, "Stick", "USB\\A", 100);
        let selected = ids(&[stick.clone()]);
        let mut host = FakeHost::new(vec![Ok(vec![stick])]);
        host.letter("USB\\A", 'F');
        let mut on_prepare = notices(&host);

        let result = prepare_all(
            &selected,
            Filesystem::Ntfs,
            PartitionStyle::Gpt,
            "STORE",
            &mut host,
            &mut on_prepare,
        );

        match result {
            PrepareAll::Done(results) => {
                assert!(matches!(results[0], Ok(ref prepared) if prepared.letter == 'F'));
            }
            PrepareAll::AbortSystemDisk { .. } => panic!("expected the disk to finish"),
        }
        assert!(host.format_calls.is_empty());
    }

    #[test]
    fn relist_system_disk_is_not_cleaned() {
        let first = removable(1, "System USB", "USB\\A", 100);
        let second = removable(2, "Other", "USB\\B", 100);
        let selected = ids(&[first.clone(), second.clone()]);
        let mut first_system = first.clone();
        first_system.contains_system_volume = true;
        let mut host = FakeHost::new(vec![
            Ok(vec![first, second.clone()]),
            Ok(vec![first_system, second]),
        ]);
        let mut on_prepare = notices(&host);

        let result = prepare_all(
            &selected,
            Filesystem::Ntfs,
            PartitionStyle::Gpt,
            "STORE",
            &mut host,
            &mut on_prepare,
        );

        assert!(matches!(
            result,
            PrepareAll::AbortSystemDisk {
                number: 1,
                model,
                cleaned,
                ..
            } if model == "System USB" && cleaned.is_empty()
        ));
        assert!(host.scripts.is_empty());
        assert_eq!(marks(&host), vec![Mark::Formatting { index: 0, total: 2 }]);
    }

    #[test]
    fn relist_system_disk_discards_earlier_prepared_disks() {
        let first = removable(1, "Safe", "USB\\A", 100);
        let second = removable(2, "System USB", "USB\\B", 100);
        let selected = ids(&[first.clone(), second.clone()]);
        let mut second_system = second.clone();
        second_system.contains_system_volume = true;
        let mut host = FakeHost::new(vec![
            Ok(vec![first.clone(), second.clone()]),
            Ok(vec![first, second]),
            Ok(vec![removable(1, "Safe", "USB\\A", 100), second_system]),
        ]);
        host.letter("USB\\A", 'E');
        let mut on_prepare = notices(&host);

        let result = prepare_all(
            &selected,
            Filesystem::Ntfs,
            PartitionStyle::Gpt,
            "STORE",
            &mut host,
            &mut on_prepare,
        );

        assert!(matches!(
            result,
            PrepareAll::AbortSystemDisk { number: 2, model, .. } if model == "System USB"
        ));
        assert_eq!(
            host.scripts,
            vec![diskpart_script(
                1,
                "gpt",
                "format fs=ntfs quick label=\"STORE\""
            )]
        );
        assert_eq!(
            marks(&host),
            vec![
                Mark::Formatting { index: 0, total: 2 },
                Mark::Script,
                Mark::Finished(0),
                Mark::Formatting { index: 1, total: 2 },
            ]
        );
    }

    #[test]
    fn relist_system_disk_names_disks_whose_script_already_ran() {
        let first = removable(1, "Safe", "USB\\A", 100);
        let second = removable(2, "System USB", "USB\\B", 100);
        let third = removable(3, "Later", "USB\\C", 100);
        let selected = ids(&[first.clone(), second.clone(), third.clone()]);
        let mut second_system = second.clone();
        second_system.contains_system_volume = true;
        let mut host = FakeHost::new(vec![
            Ok(vec![first.clone(), second.clone(), third.clone()]),
            Ok(vec![first, second, third.clone()]),
            Ok(vec![
                removable(1, "Safe", "USB\\A", 100),
                second_system,
                third,
            ]),
        ]);
        host.letter("USB\\A", 'E');
        let mut on_prepare = notices(&host);

        let result = prepare_all(
            &selected,
            Filesystem::Ntfs,
            PartitionStyle::Gpt,
            "STORE",
            &mut host,
            &mut on_prepare,
        );

        match result {
            PrepareAll::AbortSystemDisk {
                number,
                model,
                serial,
                cleaned,
            } => {
                assert_eq!(number, 2);
                assert_eq!(model, "System USB");
                assert_eq!(
                    system_disk_stop_message(number, &model, serial.as_deref(), &cleaned),
                    "Disk 1 (Safe) · S/N unavailable was already cleaned. Disk 2 (System USB) · S/N unavailable contains the Windows system volume."
                );
            }
            PrepareAll::Done(results) => panic!("expected abort, got {results:?}"),
        }
        assert_eq!(
            host.scripts,
            vec![diskpart_script(
                1,
                "gpt",
                "format fs=ntfs quick label=\"STORE\""
            )]
        );
    }

    #[test]
    fn relist_failure_skips_diskpart() {
        let missing = removable(1, "Gone", "USB\\A", 100);
        let fixed = removable(2, "Fixed", "USB\\B", 100);
        let ambiguous = removable(3, "Twin", "USB\\C", 100);
        let selected = ids(&[missing.clone(), fixed.clone(), ambiguous.clone()]);
        let mut fixed_now = fixed.clone();
        fixed_now.media_type = "Fixed hard disk media".to_string();
        let twin = removable(4, "Twin", "USB\\C", 100);
        let bad = vec![fixed_now, ambiguous.clone(), twin];
        let mut host = FakeHost::new(vec![Ok(vec![missing, fixed, ambiguous]), Ok(bad)]);
        let mut on_prepare = notices(&host);

        let result = prepare_all(
            &selected,
            Filesystem::Ntfs,
            PartitionStyle::Gpt,
            "STORE",
            &mut host,
            &mut on_prepare,
        );

        match result {
            PrepareAll::Done(results) => {
                assert!(matches!(
                    results[0],
                    Err(PrepareFailure::Rematch(Rematch::Missing))
                ));
                assert!(matches!(
                    results[1],
                    Err(PrepareFailure::Rematch(Rematch::NotRemovable))
                ));
                assert!(matches!(
                    results[2],
                    Err(PrepareFailure::Rematch(Rematch::NotUnique))
                ));
            }
            PrepareAll::AbortSystemDisk { .. } => panic!("expected per-disk failures"),
        }
        assert!(host.scripts.is_empty());
        assert!(
            marks(&host)
                .iter()
                .all(|mark| !matches!(mark, Mark::Script))
        );
    }

    #[test]
    fn no_letter_after_format_fails_that_disk() {
        let stick = removable(5, "Bare", "USB\\BARE", 100);
        let selected = ids(&[stick.clone()]);
        let mut host = FakeHost::new(vec![Ok(vec![stick])]);
        let mut on_prepare = notices(&host);

        let result = prepare_all(
            &selected,
            Filesystem::ExFat,
            PartitionStyle::Gpt,
            "STORE",
            &mut host,
            &mut on_prepare,
        );

        match result {
            PrepareAll::Done(results) => {
                assert!(matches!(results[0], Err(PrepareFailure::NoLetter)));
            }
            PrepareAll::AbortSystemDisk { .. } => panic!("expected the disk to finish"),
        }
        assert_eq!(
            host.scripts,
            vec![diskpart_script(
                5,
                "gpt",
                "format fs=exfat quick label=\"STORE\""
            )]
        );
        assert!(host.commands.is_empty());
    }

    #[test]
    fn missing_letter_skips_format_large_fat32() {
        let stick = removable(4, "Wide", "USB\\WIDE", 1024);
        let selected = ids(&[stick.clone()]);
        let mut host = FakeHost::new(vec![Ok(vec![stick])]);
        let mut on_prepare = notices(&host);

        let result = prepare_all(
            &selected,
            Filesystem::LargeFat32,
            PartitionStyle::Gpt,
            "STORE",
            &mut host,
            &mut on_prepare,
        );

        match result {
            PrepareAll::Done(results) => {
                assert!(matches!(results[0], Err(PrepareFailure::NoLetter)));
            }
            PrepareAll::AbortSystemDisk { .. } => panic!("expected the disk to finish"),
        }
        assert_eq!(host.scripts, vec![assign_only_script(4, "gpt")]);
        assert!(host.format_calls.is_empty());
        assert!(host.commands.is_empty());
    }

    #[test]
    fn list_failure_fails_every_disk_without_diskpart() {
        let stick = removable(1, "Stick", "USB\\A", 100);
        let selected = ids(&[stick]);
        let mut host = FakeHost::new(vec![Err("wmi down".to_string())]);
        let mut on_prepare = notices(&host);

        let result = prepare_all(
            &selected,
            Filesystem::Ntfs,
            PartitionStyle::Gpt,
            "STORE",
            &mut host,
            &mut on_prepare,
        );

        match result {
            PrepareAll::Done(results) => match &results[0] {
                Err(PrepareFailure::Command(message)) => assert_eq!(message, "wmi down"),
                other => panic!("expected command failure, got {other:?}"),
            },
            PrepareAll::AbortSystemDisk { .. } => panic!("expected a per-disk failure"),
        }
        assert!(host.scripts.is_empty());
        assert!(marks(&host).is_empty());
    }

    #[test]
    fn relist_error_fails_that_disk_and_the_next_still_runs() {
        let first = removable(1, "One", "USB\\ONE", 100);
        let second = removable(2, "Two", "USB\\TWO", 100);
        let selected = ids(&[first.clone(), second.clone()]);
        let mut host = FakeHost::new(vec![
            Ok(vec![first.clone(), second.clone()]),
            Err("relist failed".to_string()),
            Ok(vec![first, second]),
        ]);
        host.letter("USB\\TWO", 'H');
        let mut on_prepare = notices(&host);

        let result = prepare_all(
            &selected,
            Filesystem::Ntfs,
            PartitionStyle::Mbr,
            "STORE",
            &mut host,
            &mut on_prepare,
        );

        match result {
            PrepareAll::Done(results) => {
                assert!(matches!(
                    &results[0],
                    Err(PrepareFailure::Command(message)) if message == "relist failed"
                ));
                assert!(matches!(
                    &results[1],
                    Ok(prepared) if prepared.letter == 'H'
                ));
            }
            PrepareAll::AbortSystemDisk { .. } => panic!("expected both disks to finish"),
        }
        assert_eq!(
            host.scripts,
            vec![diskpart_script(
                2,
                "mbr",
                "format fs=ntfs quick label=\"STORE\""
            )]
        );
    }

    #[test]
    fn letter_query_error_is_the_disk_reason() {
        let stick = removable(5, "Bare", "USB\\BARE", 100);
        let selected = ids(&[stick.clone()]);
        let mut host = FakeHost::new(vec![Ok(vec![stick])]);
        host.letters.push((
            "USB\\BARE".to_string(),
            Err("letter query failed".to_string()),
        ));
        let mut on_prepare = notices(&host);

        let result = prepare_all(
            &selected,
            Filesystem::Ntfs,
            PartitionStyle::Gpt,
            "STORE",
            &mut host,
            &mut on_prepare,
        );

        match result {
            PrepareAll::Done(results) => match &results[0] {
                Err(PrepareFailure::Command(message)) => {
                    assert_eq!(message, "letter query failed");
                }
                other => panic!("expected the query error, got {other:?}"),
            },
            PrepareAll::AbortSystemDisk { .. } => panic!("expected the disk to finish"),
        }
        assert_eq!(host.scripts.len(), 1);
    }

    #[test]
    fn failure_text_returns_the_prepare_messages() {
        assert_eq!(
            failure_text(&PrepareFailure::Fat32TooLarge),
            "Choose Large FAT32. Disk is larger than 32 GB."
        );
        assert_eq!(
            failure_text(&PrepareFailure::Rematch(Rematch::NotUnique)),
            "Disk could not be matched uniquely."
        );
        assert_eq!(
            failure_text(&PrepareFailure::Rematch(Rematch::NotRemovable)),
            "Disk is no longer removable."
        );
        assert_eq!(
            failure_text(&PrepareFailure::Rematch(Rematch::Missing)),
            "Disk is no longer present."
        );
        assert_eq!(
            failure_text(&PrepareFailure::Diskpart),
            "diskpart failed after 3 tries."
        );
        assert_eq!(
            failure_text(&PrepareFailure::NoLetter),
            "No drive letter after format."
        );
    }

    #[test]
    fn two_targets_copy_concurrently_and_pass() {
        let root = scratch("concurrent");
        let first = one_file(&root, "one", b"ab");
        let second = one_file(&root, "two", b"world!");
        let entered = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(AtomicUsize::new(0));
        let timed_out = Arc::new(AtomicBool::new(false));

        let entered_w = Arc::clone(&entered);
        let release_w = Arc::clone(&release);
        let seen_w = Arc::clone(&seen);
        let timed_out_w = Arc::clone(&timed_out);
        let watcher = std::thread::spawn(move || {
            let start = Instant::now();
            loop {
                let count = entered_w.load(Ordering::SeqCst);
                if count >= 2 {
                    seen_w.store(count, Ordering::SeqCst);
                    release_w.store(true, Ordering::SeqCst);
                    break;
                }
                if start.elapsed() >= Duration::from_secs(2) {
                    timed_out_w.store(true, Ordering::SeqCst);
                    seen_w.store(count, Ordering::SeqCst);
                    release_w.store(true, Ordering::SeqCst);
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });

        let entered_c = Arc::clone(&entered);
        let release_c = Arc::clone(&release);
        let on_enter = move || {
            entered_c.fetch_add(1, Ordering::SeqCst);
            while !release_c.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let bytes = Arc::new(Mutex::new(Vec::<(usize, u64, u64)>::new()));
        let bytes_c = Arc::clone(&bytes);
        let on_bytes = move |index, copied, total| {
            bytes_c.lock().expect("bytes").push((index, copied, total));
        };

        let outcomes = copy_and_verify_all(vec![first, second], &on_enter, &on_bytes);
        watcher.join().unwrap();

        assert!(
            !timed_out.load(Ordering::SeqCst),
            "watcher timed out with count {}",
            seen.load(Ordering::SeqCst)
        );
        assert_eq!(seen.load(Ordering::SeqCst), 2);
        assert_eq!(
            outcomes,
            vec![
                DriveOutcome::Passed { checked: 1 },
                DriveOutcome::Passed { checked: 1 },
            ]
        );
        let recorded = bytes.lock().expect("bytes").clone();
        assert!(recorded.contains(&(0, 2, 2)), "{recorded:?}");
        assert!(recorded.contains(&(1, 6, 6)), "{recorded:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn on_bytes_reports_index_running_total_and_source_total() {
        let root = scratch("bytes");
        let source = root.join("src");
        let dest = root.join("dest");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(source.join("a.txt"), b"ab").unwrap();
        fs::write(source.join("b.txt"), b"hello").unwrap();
        let walk = walk_source(&source).unwrap();
        let bytes = Mutex::new(Vec::<(usize, u64, u64)>::new());
        let outcomes = copy_and_verify_all(
            vec![CopyTarget {
                source,
                dest,
                entries: walk.entries,
            }],
            &|| {},
            &|index, copied, total| bytes.lock().expect("bytes").push((index, copied, total)),
        );
        assert_eq!(outcomes, vec![DriveOutcome::Passed { checked: 2 }]);
        assert_eq!(*bytes.lock().expect("bytes"), vec![(0, 2, 7), (0, 7, 7)]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn copy_failure_does_not_verify_and_the_other_drive_passes() {
        let root = scratch("copy_fail");
        let bad_src = root.join("bad_src");
        fs::create_dir_all(&bad_src).unwrap();
        fs::write(bad_src.join("a.txt"), b"hello").unwrap();
        let parent_file = root.join("not_a_directory");
        fs::write(&parent_file, b"x").unwrap();
        let bad = CopyTarget {
            source: bad_src,
            dest: parent_file.join("stick"),
            entries: vec![SourceEntry {
                relative: PathBuf::from("a.txt"),
                kind: EntryKind::File,
                size: 5,
            }],
        };
        let good = one_file(&root, "good", b"ok");

        let outcomes = copy_and_verify_all(vec![bad, good], &|| {}, &|_, _, _| {});

        match &outcomes[0] {
            DriveOutcome::Failed { reason } => {
                assert!(reason.starts_with("Copy failed after 3 tries:"), "{reason}");
                assert!(!reason.starts_with("Verification failed"), "{reason}");
                assert!(reason.contains("a.txt"), "{reason}");
            }
            DriveOutcome::Passed { .. } => panic!("bad dest should fail"),
        }
        assert_eq!(outcomes[1], DriveOutcome::Passed { checked: 1 });
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn verification_failure_reports_counts_and_summary() {
        let root = scratch("verify_fail");
        let source = root.join("src");
        let dest = root.join("dest");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(source.join("a.txt"), b"hello").unwrap();
        let outcomes = copy_and_verify_all(
            vec![CopyTarget {
                source,
                dest,
                entries: vec![SourceEntry {
                    relative: PathBuf::from("a.txt"),
                    kind: EntryKind::File,
                    size: 99,
                }],
            }],
            &|| {},
            &|_, _, _| {},
        );
        assert_eq!(
            outcomes,
            vec![DriveOutcome::Failed {
                reason: "Verification failed. checked 1, missing 0, size mismatches 1.\nsize mismatch: a.txt"
                    .to_string(),
            }]
        );
        let _ = fs::remove_dir_all(&root);
    }
}

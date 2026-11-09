use crate::disk_id::{DiskId, EnumeratedDisk};
use crate::job::{
    copy_and_verify_all, failure_text, prepare_all, CopyTarget, DriveOutcome, PrepareAll,
    PrepareFailure, PrepareNotice, VolumeHost,
};
use crate::label::{Filesystem, PartitionStyle};
use crate::log_file::write_run_log;
use crate::session::{
    confirmation_body, initial_form, overall_fraction, run_another_batch, start_enabled, BatchUi,
    DriveRow,
};
use crate::source::{walk_source, EntryKind, SourceEntry};
use crate::windows_host::{is_elevated, relaunch_elevated, CloseGate, WindowsHost};
use iced::widget::{
    button, checkbox, column, pick_list, progress_bar, row, scrollable, text, text_input,
};
use iced::{event, window, Element, Event, Fill, Subscription, Task};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Once};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveStatus {
    Waiting,
    Formatting,
    Ready,
    Failed,
    Copying,
    Verifying,
    Passed,
}

impl DriveStatus {
    pub fn label(self) -> &'static str {
        match self {
            DriveStatus::Waiting => "Waiting",
            DriveStatus::Formatting => "Formatting",
            DriveStatus::Ready => "Ready",
            DriveStatus::Failed => "Failed",
            DriveStatus::Copying => "Copying",
            DriveStatus::Verifying => "Verifying",
            DriveStatus::Passed => "Passed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FsLabel(Filesystem);

impl std::fmt::Display for FsLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PartLabel(PartitionStyle);

impl std::fmt::Display for PartLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Confirm,
    Preparing,
    Copying,
    Done,
}

struct DriveRun {
    row_index: usize,
    status: DriveStatus,
    fraction: f32,
    copied: u64,
    reached_copy: bool,
    copy_slot: Option<usize>,
    prepare_failure: Option<String>,
}

#[derive(Debug, Clone)]
enum JobEvent {
    Prepare(PrepareNotice),
    Aborted { number: u32, model: String },
    Prepared(Vec<Result<crate::job::Prepared, PrepareFailure>>),
    Bytes { index: usize, copied: u64, total: u64 },
    Finished(Vec<DriveOutcome>),
    Crashed,
}

#[derive(Debug, Clone)]
enum Message {
    SourceEdited(String),
    Browse,
    Filesystem(Filesystem),
    Partition(PartitionStyle),
    LabelEdited(String),
    Toggle { index: usize, checked: bool },
    Refresh,
    Start,
    Confirm,
    Dismiss,
    Relaunch,
    RunAnother,
    CloseRequested,
    Worker(JobEvent),
}

pub struct App {
    ui: BatchUi,
    source_text: String,
    disks: Vec<EnumeratedDisk>,
    list_error: Option<String>,
    elevated: bool,
    gate: CloseGate,
    phase: Phase,
    runs: Vec<DriveRun>,
    prepared_done: usize,
    source_bytes: u64,
    skipped_links: Vec<PathBuf>,
}

pub fn run() -> iced::Result {
    iced::application(App::boot, App::update, App::view)
        .title("MultiUSB")
        .subscription(App::subscription)
        .exit_on_close_request(false)
        .window_size(iced::Size::new(760.0, 900.0))
        .run()
}

impl App {
    fn boot() -> Self {
        let mut app = Self {
            ui: BatchUi {
                form: initial_form(),
                rows: Vec::new(),
                log: Vec::new(),
                show_another: false,
            },
            source_text: String::new(),
            disks: Vec::new(),
            list_error: None,
            elevated: is_elevated(),
            gate: CloseGate::new(),
            phase: Phase::Idle,
            runs: Vec::new(),
            prepared_done: 0,
            source_bytes: 0,
            skipped_links: Vec::new(),
        };
        app.reload_disks();
        app
    }

    fn locked(&self) -> bool {
        matches!(self.phase, Phase::Preparing | Phase::Copying)
    }

    fn subscription(_app: &Self) -> Subscription<Message> {
        event::listen_with(on_window_event)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SourceEdited(value) if !self.locked() => {
                self.source_text = value;
                self.ui.form.source = if self.source_text.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(&self.source_text))
                };
            }
            Message::Browse if !self.locked() => {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("Source folder")
                    .pick_folder()
                {
                    self.source_text = path.display().to_string();
                    self.ui.form.source = Some(path);
                }
            }
            Message::Filesystem(filesystem) if !self.locked() => {
                self.ui.form.filesystem = Some(filesystem);
            }
            Message::Partition(style) if !self.locked() => {
                self.ui.form.partition = Some(style);
            }
            Message::LabelEdited(value) if !self.locked() => {
                self.ui.form.label = value;
            }
            Message::Toggle { index, checked } if !self.locked() => {
                if let Some(row) = self.ui.rows.get_mut(index) {
                    row.checked = checked;
                }
            }
            Message::Refresh if !self.locked() => {
                self.runs.clear();
                self.prepared_done = 0;
                self.reload_disks();
            }
            Message::Start if !self.locked() => {
                if self.elevated {
                    let checked = self.ui.rows.iter().filter(|row| row.checked).count();
                    if start_enabled(&self.ui.form, checked) {
                        self.phase = Phase::Confirm;
                    }
                }
            }
            Message::Dismiss => {
                if self.phase == Phase::Confirm {
                    self.phase = Phase::Idle;
                }
            }
            Message::Confirm if self.phase == Phase::Confirm && self.elevated => {
                return self.begin_job();
            }
            Message::Relaunch => match std::env::current_exe() {
                Ok(exe) => match relaunch_elevated(&exe) {
                    Ok(()) => std::process::exit(0),
                    Err(err) => {
                        self.ui
                            .log
                            .push(format!("Could not relaunch elevated: {err}"));
                    }
                },
                Err(err) => {
                    self.ui
                        .log
                        .push(format!("Could not relaunch elevated: {err}"));
                }
            },
            Message::RunAnother => {
                run_another_batch(&mut self.ui);
                self.phase = Phase::Idle;
                self.runs.clear();
                self.prepared_done = 0;
                self.source_bytes = 0;
                self.skipped_links.clear();
                self.reload_disks();
            }
            Message::CloseRequested => {
                self.gate.request_and_wait_for_diskpart();
                // Copy threads notice the flag at the next file boundary.
                if self.phase != Phase::Copying {
                    std::process::exit(0);
                }
            }
            Message::Worker(event) => self.on_worker(event),
            _ => {}
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let mut children: Vec<Element<'_, Message>> = Vec::new();
        if !self.elevated {
            children.push(text("Administrator rights are required to wipe disks.").into());
            children.push(
                button("Relaunch as administrator")
                    .on_press(Message::Relaunch)
                    .into(),
            );
        }

        let mut source_input = text_input("Source folder", &self.source_text).width(Fill);
        if !self.locked() {
            source_input = source_input.on_input(Message::SourceEdited);
        }
        let browse = if self.locked() {
            button("Browse")
        } else {
            button("Browse").on_press(Message::Browse)
        };
        children.push(row![source_input, browse].spacing(8).into());

        children.push(
            pick_list(
                Filesystem::ALL.map(FsLabel),
                self.ui.form.filesystem.map(FsLabel),
                |FsLabel(filesystem)| Message::Filesystem(filesystem),
            )
            .placeholder("Select filesystem")
            .width(Fill)
            .into(),
        );
        children.push(
            pick_list(
                PartitionStyle::ALL.map(PartLabel),
                self.ui.form.partition.map(PartLabel),
                |PartLabel(style)| Message::Partition(style),
            )
            .placeholder("Select partition type")
            .width(Fill)
            .into(),
        );

        let mut label_input = text_input("Volume label", &self.ui.form.label).width(Fill);
        if !self.locked() {
            label_input = label_input.on_input(Message::LabelEdited);
        }
        let mut label_row: Vec<Element<'_, Message>> = vec![label_input.into()];
        if let Some(filesystem) = self.ui.form.filesystem {
            label_row.push(text(format!("{} characters", filesystem.label_limit())).into());
        }
        children.push(row(label_row).spacing(8).into());

        let refresh = if self.locked() {
            button("Refresh")
        } else {
            button("Refresh").on_press(Message::Refresh)
        };
        children.push(row![text("Removable disks"), refresh].spacing(8).into());
        if let Some(error) = &self.list_error {
            children.push(text(error).into());
        } else if self.disks.is_empty() {
            children.push(text("No removable disks.").into());
        }
        for (index, row_state) in self.ui.rows.iter().enumerate() {
            let mut toggle = checkbox(row_state.checked).label(&row_state.detail);
            if !self.locked() {
                toggle = toggle.on_toggle(move |checked| Message::Toggle { index, checked });
            }
            children.push(toggle.into());
        }

        let show_start = !self.locked() && self.phase != Phase::Confirm;
        if show_start {
            let checked = self.ui.rows.iter().filter(|row| row.checked).count();
            let start = button("Start");
            let start = if start_enabled(&self.ui.form, checked) {
                start.on_press(Message::Start)
            } else {
                start
            };
            children.push(start.into());
        }

        if self.phase == Phase::Confirm {
            children.push(text(confirmation_body(&self.ui.form, &self.ui.rows)).into());
            children.push(
                row![
                    button("Confirm").on_press(Message::Confirm),
                    button("Dismiss").on_press(Message::Dismiss),
                ]
                .spacing(8)
                .into(),
            );
        }

        for run in &self.runs {
            let Some(disk) = self.disks.get(run.row_index) else {
                continue;
            };
            children.push(
                text(format!(
                    "Disk {} ({}) — {}",
                    disk.number,
                    disk.model,
                    run.status.label()
                ))
                .into(),
            );
            children.push(progress_bar(0.0..=1.0, run.fraction).into());
        }

        if self.phase == Phase::Preparing {
            children.push(
                text(format!(
                    "{} of {} disks prepared",
                    self.prepared_done,
                    self.runs.len()
                ))
                .into(),
            );
        }
        if matches!(self.phase, Phase::Copying | Phase::Done)
            && self.runs.iter().any(|run| run.reached_copy)
        {
            let copied: Vec<u64> = self
                .runs
                .iter()
                .filter(|run| run.reached_copy)
                .map(|run| run.copied)
                .collect();
            let fraction = overall_fraction(&copied, self.source_bytes);
            children.push(text("Overall").into());
            children.push(progress_bar(0.0..=1.0, fraction).into());
        }

        if !self.ui.log.is_empty() {
            let lines: Vec<Element<'_, Message>> = self
                .ui
                .log
                .iter()
                .map(|line| text(line).into())
                .collect();
            children.push(
                scrollable(column(lines).spacing(4))
                    .height(200)
                    .width(Fill)
                    .into(),
            );
        }

        if self.ui.show_another {
            children.push(
                button("Run another batch")
                    .on_press(Message::RunAnother)
                    .into(),
            );
        }

        scrollable(
            column(children)
                .spacing(10)
                .padding(16)
                .width(Fill),
        )
        .width(Fill)
        .height(Fill)
        .into()
    }

    fn reload_disks(&mut self) {
        let mut host = WindowsHost::new(self.gate.clone());
        match host.list_all() {
            Ok(disks) => {
                let removable: Vec<EnumeratedDisk> = disks
                    .into_iter()
                    .filter(|disk| disk.media_type.contains("Removable"))
                    .collect();
                self.ui.rows = removable.iter().map(drive_row).collect();
                self.disks = removable;
                self.list_error = None;
            }
            Err(err) => {
                self.disks.clear();
                self.ui.rows.clear();
                self.list_error = Some(format!("Could not list disks: {err}"));
            }
        }
    }

    fn begin_job(&mut self) -> Task<Message> {
        let checked = self.ui.rows.iter().filter(|row| row.checked).count();
        if !start_enabled(&self.ui.form, checked) {
            self.phase = Phase::Idle;
            return Task::none();
        }
        let Some(source) = self.ui.form.source.clone() else {
            return Task::none();
        };
        let Some(filesystem) = self.ui.form.filesystem else {
            return Task::none();
        };
        let Some(partition) = self.ui.form.partition else {
            return Task::none();
        };
        let walk = match walk_source(&source) {
            Ok(walk) => walk,
            Err(err) => {
                self.phase = Phase::Idle;
                self.ui
                    .log
                    .push(format!("Source folder could not be read: {err}"));
                return Task::none();
            }
        };
        let mut selected = Vec::new();
        let mut runs = Vec::new();
        for (index, row) in self.ui.rows.iter().enumerate() {
            if !row.checked {
                continue;
            }
            let Some(disk) = self.disks.get(index) else {
                continue;
            };
            selected.push(disk.id.clone());
            runs.push(DriveRun {
                row_index: index,
                status: DriveStatus::Waiting,
                fraction: 0.0,
                copied: 0,
                reached_copy: false,
                copy_slot: None,
                prepare_failure: None,
            });
        }
        if selected.is_empty() {
            self.phase = Phase::Idle;
            return Task::none();
        }
        self.source_bytes = walk.entries.iter().fold(0u64, |sum, entry| {
            sum + match entry.kind {
                EntryKind::File => entry.size,
                EntryKind::Directory => 0,
            }
        });
        self.skipped_links = walk.skipped_links;
        self.runs = runs;
        self.prepared_done = 0;
        self.ui.log.clear();
        self.ui.show_another = false;
        self.phase = Phase::Preparing;
        spawn_job(
            selected,
            filesystem,
            partition,
            self.ui.form.label.clone(),
            source,
            walk.entries,
            self.gate.clone(),
        )
    }

    fn on_worker(&mut self, event: JobEvent) {
        match event {
            JobEvent::Prepare(PrepareNotice::Formatting { index, .. }) => {
                if let Some(run) = self.runs.get_mut(index) {
                    run.status = DriveStatus::Formatting;
                    run.fraction = 0.0;
                }
            }
            JobEvent::Prepare(PrepareNotice::Finished { index }) => {
                self.prepared_done = self.prepared_done.saturating_add(1);
                if let Some(run) = self.runs.get_mut(index) {
                    run.fraction = 1.0;
                }
            }
            JobEvent::Aborted { number, model } => {
                self.runs.clear();
                self.publish_results(vec![format!(
                    "Stopped before any wipe. Disk {number} ({model}) contains the Windows system volume."
                )]);
            }
            JobEvent::Prepared(results) => {
                let mut slot = 0usize;
                let source_bytes = self.source_bytes;
                for (index, result) in results.into_iter().enumerate() {
                    let Some(run) = self.runs.get_mut(index) else {
                        continue;
                    };
                    match result {
                        Ok(_) => {
                            run.reached_copy = true;
                            run.copy_slot = Some(slot);
                            slot += 1;
                            run.fraction = 1.0;
                            run.status = if source_bytes == 0 {
                                DriveStatus::Verifying
                            } else {
                                DriveStatus::Ready
                            };
                        }
                        Err(failure) => {
                            run.status = DriveStatus::Failed;
                            run.fraction = 1.0;
                            run.prepare_failure = Some(prepare_reason(&failure));
                        }
                    }
                }
                if self.runs.iter().any(|run| run.reached_copy) {
                    self.phase = Phase::Copying;
                }
            }
            JobEvent::Bytes {
                index,
                copied,
                total,
            } => {
                if let Some(run) = self
                    .runs
                    .iter_mut()
                    .find(|run| run.copy_slot == Some(index))
                {
                    run.copied = copied;
                    if total > 0 && copied >= total {
                        run.status = DriveStatus::Verifying;
                        run.fraction = 1.0;
                    } else {
                        run.status = DriveStatus::Copying;
                        run.fraction = if total == 0 {
                            1.0
                        } else {
                            copied as f32 / total as f32
                        };
                    }
                }
            }
            JobEvent::Finished(outcomes) => {
                for run in &mut self.runs {
                    let Some(slot) = run.copy_slot else {
                        continue;
                    };
                    match outcomes.get(slot) {
                        Some(DriveOutcome::Passed { .. }) => {
                            run.status = DriveStatus::Passed;
                            run.fraction = 1.0;
                        }
                        Some(DriveOutcome::Failed { .. }) => {
                            run.status = DriveStatus::Failed;
                            run.fraction = 1.0;
                        }
                        None => {}
                    }
                }
                let lines = result_lines(
                    &self.runs,
                    &self.disks,
                    &outcomes,
                    &self.skipped_links,
                );
                self.publish_results(lines);
            }
            JobEvent::Crashed => {
                self.publish_results(vec![
                    "The batch stopped unexpectedly.".to_string(),
                ]);
            }
        }
    }

    fn publish_results(&mut self, lines: Vec<String>) {
        let stamp = chrono::Local::now()
            .format("%Y-%m-%d_%H-%M-%S")
            .to_string();
        let mut shown = lines.clone();
        match std::env::current_dir() {
            Ok(dir) => {
                if write_run_log(&dir, &stamp, &lines).is_err() {
                    shown.push(
                        "Logs could not be created in the current working directory.".to_string(),
                    );
                }
            }
            Err(_) => {
                shown.push(
                    "Logs could not be created in the current working directory.".to_string(),
                );
            }
        }
        self.ui.log = shown;
        self.ui.show_another = true;
        self.phase = Phase::Done;
        if self.gate.is_requested() {
            std::process::exit(0);
        }
    }
}

fn on_window_event(
    event: Event,
    _status: event::Status,
    _window: window::Id,
) -> Option<Message> {
    match event {
        Event::Window(window::Event::CloseRequested) => Some(Message::CloseRequested),
        _ => None,
    }
}

fn spawn_job(
    selected: Vec<DiskId>,
    filesystem: Filesystem,
    partition: PartitionStyle,
    label: String,
    source: PathBuf,
    entries: Vec<SourceEntry>,
    gate: CloseGate,
) -> Task<Message> {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    let tx = Arc::new(Mutex::new(tx));
    let tx_job = Arc::clone(&tx);
    let gate_job = gate.clone();
    std::thread::spawn(move || {
        install_close_panic_hook();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_batch(
                selected,
                filesystem,
                partition,
                label,
                source,
                entries,
                gate_job.clone(),
                &tx_job,
            );
        }));
        if gate_job.is_requested() {
            std::process::exit(0);
        }
        if result.is_err() {
            send_event(&tx_job, JobEvent::Crashed);
        }
    });
    Task::run(rx, Message::Worker)
}

fn run_batch(
    selected: Vec<DiskId>,
    filesystem: Filesystem,
    partition: PartitionStyle,
    label: String,
    source: PathBuf,
    entries: Vec<SourceEntry>,
    gate: CloseGate,
    tx: &Mutex<futures::channel::mpsc::UnboundedSender<JobEvent>>,
) {
    let mut host = WindowsHost::new(gate.clone());
    let prepared = {
        let mut on_prepare = |notice| {
            send_event(tx, JobEvent::Prepare(notice));
        };
        prepare_all(
            &selected,
            filesystem,
            partition,
            &label,
            &mut host,
            &mut on_prepare,
        )
    };
    match prepared {
        PrepareAll::AbortSystemDisk { number, model } => {
            send_event(tx, JobEvent::Aborted { number, model });
        }
        PrepareAll::Done(results) => {
            let targets: Vec<CopyTarget> = results
                .iter()
                .filter_map(|result| result.as_ref().ok())
                .map(|prepared| CopyTarget {
                    source: source.clone(),
                    dest: PathBuf::from(format!("{}:\\", prepared.letter)),
                    entries: entries.clone(),
                })
                .collect();
            send_event(tx, JobEvent::Prepared(results));
            let outcomes = {
                let gate_enter = gate.clone();
                let on_enter = move || stop_copy_if_closed(&gate_enter);
                let gate_bytes = gate.clone();
                let on_bytes = move |index, copied, total| {
                    // The current file has finished. Unwind this drive before
                    // the next file, and let the other drives do the same.
                    stop_copy_if_closed(&gate_bytes);
                    send_event(
                        tx,
                        JobEvent::Bytes {
                            index,
                            copied,
                            total,
                        },
                    );
                };
                copy_and_verify_all(targets, &on_enter, &on_bytes)
            };
            send_event(tx, JobEvent::Finished(outcomes));
        }
    }
}

const CLOSE_PANIC: &str = "multiusb-window-closed";

fn install_close_panic_hook() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let closed = info.payload().downcast_ref::<&str>() == Some(&CLOSE_PANIC)
                || info
                    .payload()
                    .downcast_ref::<String>()
                    .is_some_and(|message| message == CLOSE_PANIC);
            if !closed {
                previous(info);
            }
        }));
    });
}

fn stop_copy_if_closed(gate: &CloseGate) {
    if gate.is_requested() {
        std::panic::panic_any(CLOSE_PANIC);
    }
}

fn send_event(
    tx: &Mutex<futures::channel::mpsc::UnboundedSender<JobEvent>>,
    event: JobEvent,
) {
    let sender = tx.lock().unwrap_or_else(|err| err.into_inner());
    let _ = sender.unbounded_send(event);
}

fn prepare_reason(failure: &PrepareFailure) -> String {
    match failure {
        PrepareFailure::Command(message) => message.clone(),
        other => failure_text(other).to_string(),
    }
}

fn result_lines(
    runs: &[DriveRun],
    disks: &[EnumeratedDisk],
    outcomes: &[DriveOutcome],
    skipped: &[PathBuf],
) -> Vec<String> {
    let mut lines = Vec::new();
    for link in skipped {
        lines.push(format!("Skipped link: {}", link.display()));
    }
    for run in runs {
        let Some(disk) = disks.get(run.row_index) else {
            continue;
        };
        let name = format!("Disk {} ({})", disk.number, disk.model);
        if let Some(reason) = &run.prepare_failure {
            lines.push(format!("{name}: {reason}"));
            continue;
        }
        if let Some(slot) = run.copy_slot {
            match outcomes.get(slot) {
                Some(DriveOutcome::Passed { .. }) => lines.push(format!("{name}: Passed")),
                Some(DriveOutcome::Failed { reason }) => lines.push(format!("{name}: {reason}")),
                None => lines.push(format!("{name}: Failed")),
            }
        }
    }
    lines
}

fn drive_row(disk: &EnumeratedDisk) -> DriveRow {
    let letters = if disk.letters.is_empty() {
        "no letter".to_string()
    } else {
        disk.letters
            .iter()
            .map(|letter| letter.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let size = format_bytes(disk.size_bytes);
    DriveRow {
        key: disk_key(disk),
        checked: false,
        model: format!("Disk {} {} {}", disk.number, disk.model, size),
        detail: format!("Disk {}  {}  {}  {}", disk.number, disk.model, size, letters),
    }
}

fn disk_key(disk: &EnumeratedDisk) -> String {
    if let Some(pnp) = &disk.id.pnp_device_id {
        return pnp.clone();
    }
    if let Some(serial) = &disk.id.serial {
        return format!("{serial}:{}", disk.size_bytes);
    }
    format!("disk-{}", disk.number)
}

fn format_bytes(bytes: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else {
        format!("{} bytes", bytes as u64)
    }
}

#[cfg(test)]
#[test]
fn status_labels() {
    assert_eq!(DriveStatus::Waiting.label(), "Waiting");
    assert_eq!(DriveStatus::Formatting.label(), "Formatting");
    assert_eq!(DriveStatus::Ready.label(), "Ready");
    assert_eq!(DriveStatus::Failed.label(), "Failed");
    assert_eq!(DriveStatus::Copying.label(), "Copying");
    assert_eq!(DriveStatus::Verifying.label(), "Verifying");
    assert_eq!(DriveStatus::Passed.label(), "Passed");
}

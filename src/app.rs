use crate::disk_id::{DiskId, EnumeratedDisk};
use crate::job::{
    CleanedDisk, CopyTarget, DriveOutcome, PrepareAll, PrepareFailure, PrepareNotice, VolumeHost,
    copy_and_verify_all, failure_text, prepare_all, system_disk_stop_message,
};
use crate::label::{Filesystem, LabelError, PartitionStyle, validate_label};
use crate::log_file::write_run_log;
use crate::session::{
    BatchUi, DriveRow, confirmation_body, disk_log_name, format_drive_detail, format_drive_result,
    initial_form, overall_fraction, run_another_batch, start_enabled,
};
use crate::source::{EntryKind, SourceEntry, walk_source};
use crate::windows_host::{CloseGate, WindowsHost, is_elevated, relaunch_elevated};
use futures::StreamExt;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::form::{Field, Form};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Theme, ThemeMode, WindowExt};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, Task, Window, div, px,
};
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
enum Phase {
    Idle,
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
    Aborted {
        number: u32,
        model: String,
        cleaned: Vec<CleanedDisk>,
    },
    Prepared(Vec<Result<crate::job::Prepared, PrepareFailure>>),
    Bytes {
        index: usize,
        copied: u64,
        total: u64,
    },
    Finished(Vec<DriveOutcome>),
    Crashed,
}

pub struct App {
    ui: BatchUi,
    disks: Vec<EnumeratedDisk>,
    list_error: Option<String>,
    elevated: bool,
    gate: CloseGate,
    phase: Phase,
    runs: Vec<DriveRun>,
    prepared_done: usize,
    source_bytes: u64,
    skipped_links: Vec<PathBuf>,
    source: Entity<InputState>,
    label: Entity<InputState>,
    filesystem: Entity<SelectState<Vec<&'static str>>>,
    partition: Entity<SelectState<Vec<&'static str>>>,
    events: Arc<Mutex<futures::channel::mpsc::UnboundedSender<JobEvent>>>,
    _listener: Task<()>,
    _subscriptions: Vec<Subscription>,
}

pub(crate) fn app_theme_mode() -> ThemeMode {
    ThemeMode::Dark
}

pub(crate) fn apply_app_theme(cx: &mut gpui_kit::App) {
    Theme::change(app_theme_mode(), None, cx);
}

pub fn run() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            apply_app_theme(cx);
            gpui_kit::open_window(
                gpui_kit::WindowOptions {
                    window_bounds: Some(gpui_kit::WindowBounds::centered(
                        gpui_kit::size(px(760.), px(900.)),
                        cx,
                    )),
                    window_min_size: Some(gpui_kit::size(px(560.), px(640.))),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                window.set_window_title("MultiUSB");
                cx.new(|cx| App::new(window, cx))
            })
            .expect("failed to open window");
        });
}

impl App {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let source = cx.new(|cx| InputState::new(window, cx).placeholder("Source folder"));
        let label = cx.new(|cx| InputState::new(window, cx).placeholder("Volume label"));
        let filesystem = cx.new(|cx| {
            SelectState::new(
                Filesystem::ALL.map(|item| item.as_str()).to_vec(),
                None,
                window,
                cx,
            )
        });
        let partition = cx.new(|cx| {
            SelectState::new(
                PartitionStyle::ALL.map(|item| item.as_str()).to_vec(),
                None,
                window,
                cx,
            )
        });
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let events = Arc::new(Mutex::new(tx));
        let listener = cx.spawn(async move |this, cx| {
            while let Some(event) = rx.next().await {
                if this
                    .update(cx, |this, cx| {
                        this.on_worker(event);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let subscriptions = vec![
            cx.subscribe_in(&source, window, |this, state, event, _, cx| {
                if this.locked() || !matches!(event, InputEvent::Change) {
                    return;
                }
                let value = state.read(cx).value().to_string();
                this.ui.form.source = if value.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(value))
                };
                cx.notify();
            }),
            cx.subscribe_in(&label, window, |this, state, event, _, cx| {
                if this.locked() || !matches!(event, InputEvent::Change) {
                    return;
                }
                this.ui.form.label = state.read(cx).value().to_string();
                cx.notify();
            }),
            cx.subscribe_in(
                &filesystem,
                window,
                |this, _state, event, _, cx| {
                    if this.locked() {
                        return;
                    }
                    let SelectEvent::Confirm(value) = event;
                    this.ui.form.filesystem = value.as_deref().and_then(named_filesystem);
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &partition,
                window,
                |this, _state, event, _, cx| {
                    if this.locked() {
                        return;
                    }
                    let SelectEvent::Confirm(value) = event;
                    this.ui.form.partition = value.as_deref().and_then(named_partition);
                    cx.notify();
                },
            ),
        ];

        let view = cx.entity().downgrade();
        window.on_window_should_close(cx, move |_window, cx| {
            let mut close = true;
            if let Some(view) = view.upgrade() {
                view.update(cx, |this, _| {
                    this.gate.request_and_wait_for_diskpart();
                    close = this.phase != Phase::Copying;
                });
            }
            if close {
                std::process::exit(0);
            }
            false
        });

        let mut app = Self {
            ui: BatchUi {
                form: initial_form(),
                rows: Vec::new(),
                log: Vec::new(),
                show_another: false,
            },
            disks: Vec::new(),
            list_error: None,
            elevated: is_elevated(),
            gate: CloseGate::new(),
            phase: Phase::Idle,
            runs: Vec::new(),
            prepared_done: 0,
            source_bytes: 0,
            skipped_links: Vec::new(),
            source,
            label,
            filesystem,
            partition,
            events,
            _listener: listener,
            _subscriptions: subscriptions,
        };
        app.reload_disks();
        app
    }

    fn locked(&self) -> bool {
        matches!(self.phase, Phase::Preparing | Phase::Copying)
    }

    fn can_start(&self) -> bool {
        if self.locked() || !self.elevated {
            return false;
        }
        let checked = self.ui.rows.iter().filter(|row| row.checked).count();
        start_enabled(&self.ui.form, checked)
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

    fn browse_source(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.locked() {
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .set_title("Source folder")
            .pick_folder()
        else {
            return;
        };
        let shown = path.display().to_string();
        self.ui.form.source = Some(path);
        self.source.update(cx, |state, cx| {
            state.set_value(shown, window, cx);
        });
        cx.notify();
    }

    fn ask_to_wipe(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_start() {
            return;
        }
        let body = confirmation_body(&self.ui.form, &self.ui.rows);
        let view = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let view = view.clone();
            alert
                .title("Wipe these disks?")
                .description(body.clone())
                .confirm()
                .ok_text("Wipe disks")
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("Cancel")
                .on_ok(move |_, _, cx| {
                    view.update(cx, |this, cx| this.begin_job(cx));
                    true
                })
        });
    }

    fn append_disk_log(&mut self, row_index: usize, message: &str) {
        let Some(disk) = self.disks.get(row_index) else {
            return;
        };
        self.ui.log.push(format!(
            "{}: {message}",
            disk_log_name(disk.number, &disk.model, disk.id.serial.as_deref())
        ));
    }

    fn relaunch(&mut self, cx: &mut Context<Self>) {
        let message = match std::env::current_exe() {
            Ok(exe) => match relaunch_elevated(&exe) {
                Ok(()) => std::process::exit(0),
                Err(err) => format!("Could not relaunch elevated: {err}"),
            },
            Err(err) => format!("Could not relaunch elevated: {err}"),
        };
        self.ui.log.push(message);
        cx.notify();
    }

    fn run_another(&mut self, cx: &mut Context<Self>) {
        run_another_batch(&mut self.ui);
        self.phase = Phase::Idle;
        self.runs.clear();
        self.prepared_done = 0;
        self.source_bytes = 0;
        self.skipped_links.clear();
        self.reload_disks();
        cx.notify();
    }

    fn begin_job(&mut self, cx: &mut Context<Self>) {
        let checked = self.ui.rows.iter().filter(|row| row.checked).count();
        if !start_enabled(&self.ui.form, checked) || !self.elevated {
            self.phase = Phase::Idle;
            cx.notify();
            return;
        }
        let Some(source) = self.ui.form.source.clone() else {
            return;
        };
        let Some(filesystem) = self.ui.form.filesystem else {
            return;
        };
        let Some(partition) = self.ui.form.partition else {
            return;
        };
        let walk = match walk_source(&source) {
            Ok(walk) => walk,
            Err(err) => {
                self.phase = Phase::Idle;
                self.ui
                    .log
                    .push(format!("Source folder could not be read: {err}"));
                cx.notify();
                return;
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
            cx.notify();
            return;
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
            Arc::clone(&self.events),
        );
        cx.notify();
    }

    fn on_worker(&mut self, event: JobEvent) {
        match event {
            JobEvent::Prepare(PrepareNotice::Formatting { index, .. }) => {
                let row_index = self.runs.get_mut(index).map(|run| {
                    run.status = DriveStatus::Formatting;
                    run.fraction = 0.0;
                    run.row_index
                });
                if let Some(row_index) = row_index {
                    self.append_disk_log(row_index, "Formatting");
                }
            }
            JobEvent::Prepare(PrepareNotice::Finished { index }) => {
                self.prepared_done = self.prepared_done.saturating_add(1);
                let row_index = self.runs.get_mut(index).map(|run| {
                    run.fraction = 1.0;
                    run.row_index
                });
                if let Some(row_index) = row_index {
                    self.append_disk_log(row_index, "Prepared");
                }
            }
            JobEvent::Aborted {
                number,
                model,
                cleaned,
            } => {
                self.runs.clear();
                self.publish_results(vec![system_disk_stop_message(number, &model, &cleaned)]);
            }
            JobEvent::Prepared(results) => {
                let mut slot = 0usize;
                let source_bytes = self.source_bytes;
                let mut failure_logs = Vec::new();
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
                            let reason = prepare_reason(&failure);
                            failure_logs.push((run.row_index, reason.clone()));
                            run.prepare_failure = Some(reason);
                        }
                    }
                }
                for (row_index, reason) in failure_logs {
                    self.append_disk_log(row_index, &reason);
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
                let lines = result_lines(&self.runs, &self.disks, &outcomes, &self.skipped_links);
                self.publish_results(lines);
            }
            JobEvent::Crashed => {
                self.publish_results(vec!["The batch stopped unexpectedly.".to_string()]);
            }
        }
    }

    fn publish_results(&mut self, lines: Vec<String>) {
        self.ui.log.extend(lines);
        let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
        match std::env::current_dir() {
            Ok(dir) => {
                if write_run_log(&dir, &stamp, &self.ui.log).is_err() {
                    self.ui.log.push(
                        "Logs could not be created in the current working directory.".to_string(),
                    );
                }
            }
            Err(_) => {
                self.ui.log.push(
                    "Logs could not be created in the current working directory.".to_string(),
                );
            }
        }
        self.ui.show_another = true;
        self.phase = Phase::Done;
        if self.gate.is_requested() {
            std::process::exit(0);
        }
    }
}

impl Render for App {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let locked = self.locked();
        let mut column = v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .p_4()
            .gap_4();

        if !self.elevated {
            column = column
                .child(Alert::warning(
                    "elevation",
                    "Administrator rights are required to wipe disks.",
                ))
                .child(
                    Button::new("relaunch")
                        .label("Relaunch as administrator")
                        .on_click(cx.listener(|this, _, _, cx| this.relaunch(cx))),
                );
        }

        let label_note = label_note(self.ui.form.filesystem, &self.ui.form.label);
        let label_invalid = self.ui.form.filesystem.is_some_and(|filesystem| {
            validate_label(filesystem, &self.ui.form.label).is_err()
        });

        let mut form = Form::new()
            .child(
                Field::new().label("Source folder").child(
                    h_flex()
                        .gap_2()
                        .w_full()
                        .child(
                            div().flex_1().min_w_0().child(
                                Input::new(&self.source).disabled(locked),
                            ),
                        )
                        .child(
                            Button::new("browse")
                                .label("Browse")
                                .disabled(locked)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.browse_source(window, cx);
                                })),
                        ),
                ),
            )
            .child(
                Field::new().label("Filesystem").child(
                    Select::new(&self.filesystem)
                        .placeholder("Select filesystem")
                        .accessibility_label("Filesystem")
                        .disabled(locked),
                ),
            )
            .child(
                Field::new().label("Partition type").child(
                    Select::new(&self.partition)
                        .placeholder("Select partition type")
                        .accessibility_label("Partition type")
                        .disabled(locked),
                ),
            )
            .child(
                Field::new().label("Volume label").child(
                    v_flex().gap_1().w_full().child(
                        Input::new(&self.label).disabled(locked),
                    ).child(
                        div()
                            .text_sm()
                            .text_color(if label_invalid {
                                cx.theme().danger
                            } else {
                                cx.theme().muted_foreground
                            })
                            .child(label_note),
                    ),
                ),
            )
            .child(
                Field::new().label("Removable disks").child(self.disk_list(locked, cx)),
            );

        if !locked {
            form = form.footer(
                Button::new("start")
                    .primary()
                    .label("Start")
                    .disabled(!self.can_start())
                    .on_click(cx.listener(|this, _, window, cx| this.ask_to_wipe(window, cx))),
            );
        }
        column = column.child(form);

        if !self.runs.is_empty() {
            column = column.child(self.progress_block(cx));
        }
        if !self.ui.log.is_empty() {
            column = column.child(self.log_block());
        }
        if self.ui.show_another {
            column = column.child(
                Button::new("another-batch")
                    .label("Run another batch")
                    .on_click(cx.listener(|this, _, _, cx| this.run_another(cx))),
            );
        }
        column
    }
}

impl App {
    fn disk_list(&self, locked: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = v_flex()
            .id("disk-list")
            .w_full()
            .max_h(px(240.))
            .gap_2()
            .overflow_y_scrollbar();
        if let Some(error) = &self.list_error {
            list = list.child(Alert::error("disk-list-error", error.clone()));
        } else if self.ui.rows.is_empty() {
            list = list.child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child("No removable disks."),
            );
        }
        for row in &self.ui.rows {
            let key = row.key.clone();
            list = list.child(
                Checkbox::new(SharedString::from(format!("disk-{key}")))
                    .label(row.detail.clone())
                    .checked(row.checked)
                    .disabled(locked)
                    .on_change(cx.listener(move |this, checked, _, cx| {
                        if this.locked() {
                            return;
                        }
                        if let Some(row) = this.ui.rows.iter_mut().find(|row| row.key == key) {
                            row.checked = *checked;
                        }
                        cx.notify();
                    })),
            );
        }
        v_flex().gap_2().w_full().child(
            Button::new("refresh")
                .ghost()
                .label("Refresh")
                .disabled(locked)
                .on_click(cx.listener(|this, _, _, cx| {
                    if this.locked() {
                        return;
                    }
                    this.runs.clear();
                    this.prepared_done = 0;
                    this.reload_disks();
                    cx.notify();
                })),
        ).child(list)
    }

    fn progress_block(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut block = v_flex().gap_3().w_full();
        for run in &self.runs {
            let Some(disk) = self.disks.get(run.row_index) else {
                continue;
            };
            let key = disk_key(disk);
            let color = match run.status {
                DriveStatus::Failed => cx.theme().danger,
                DriveStatus::Passed => cx.theme().success,
                _ => cx.theme().foreground,
            };
            block = block
                .child(
                    h_flex()
                        .justify_between()
                        .w_full()
                        .child(format!("Disk {} ({})", disk.number, disk.model))
                        .child(div().text_color(color).child(run.status.label())),
                )
                .child(
                    Progress::new(SharedString::from(format!("progress-{key}")))
                        .value(run.fraction * 100.0),
                );
        }
        if self.phase == Phase::Preparing {
            block = block.child(format!(
                "{} of {} disks prepared",
                self.prepared_done,
                self.runs.len()
            ));
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
            block = block
                .child("Overall")
                .child(Progress::new("overall").value(fraction * 100.0));
        }
        block
    }

    fn log_block(&self) -> impl IntoElement {
        v_flex()
            .id("run-log")
            .w_full()
            .flex_1()
            .min_h(px(120.))
            .max_h(px(240.))
            .gap_1()
            .overflow_y_scrollbar()
            .text_sm()
            .children(self.ui.log.iter().cloned())
    }
}

fn label_note(filesystem: Option<Filesystem>, label: &str) -> String {
    let Some(filesystem) = filesystem else {
        return String::new();
    };
    match validate_label(filesystem, label) {
        Ok(()) => format!("{} characters", filesystem.label_limit()),
        Err(LabelError::TooLong { limit, .. }) => format!("Use at most {limit} characters."),
        Err(LabelError::Character { .. }) => {
            "Use letters, digits, spaces, hyphens, and underscores.".to_string()
        }
    }
}

fn named_filesystem(name: &str) -> Option<Filesystem> {
    Filesystem::ALL.into_iter().find(|item| item.as_str() == name)
}

fn named_partition(name: &str) -> Option<PartitionStyle> {
    PartitionStyle::ALL
        .into_iter()
        .find(|item| item.as_str() == name)
}

fn spawn_job(
    selected: Vec<DiskId>,
    filesystem: Filesystem,
    partition: PartitionStyle,
    label: String,
    source: PathBuf,
    entries: Vec<SourceEntry>,
    gate: CloseGate,
    tx: Arc<Mutex<futures::channel::mpsc::UnboundedSender<JobEvent>>>,
) {
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
                gate.clone(),
                &tx,
            );
        }));
        if gate.is_requested() {
            std::process::exit(0);
        }
        if result.is_err() {
            send_event(&tx, JobEvent::Crashed);
        }
    });
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
        PrepareAll::AbortSystemDisk {
            number,
            model,
            cleaned,
        } => {
            send_event(
                tx,
                JobEvent::Aborted {
                    number,
                    model,
                    cleaned,
                },
            );
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

fn send_event(tx: &Mutex<futures::channel::mpsc::UnboundedSender<JobEvent>>, event: JobEvent) {
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
        let serial = disk.id.serial.as_deref();
        if let Some(reason) = &run.prepare_failure {
            lines.push(format_drive_result(
                disk.number,
                &disk.model,
                serial,
                reason,
            ));
            continue;
        }
        if let Some(slot) = run.copy_slot {
            match outcomes.get(slot) {
                Some(DriveOutcome::Passed { .. }) => lines.push(format_drive_result(
                    disk.number,
                    &disk.model,
                    serial,
                    "Passed",
                )),
                Some(DriveOutcome::Failed { reason }) => lines.push(format_drive_result(
                    disk.number,
                    &disk.model,
                    serial,
                    reason,
                )),
                None => lines.push(format_drive_result(
                    disk.number,
                    &disk.model,
                    serial,
                    "Failed",
                )),
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
        detail: format_drive_detail(
            disk.number,
            &disk.model,
            &size,
            &letters,
            disk.id.serial.as_deref(),
        ),
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

#[cfg(test)]
#[test]
fn theme_starts_dark() {
    use gpui_kit::component::ThemeMode;

    assert_eq!(app_theme_mode(), ThemeMode::Dark);
}

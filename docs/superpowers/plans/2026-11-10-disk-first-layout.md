# Disk-First Layout + Serial Logs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reshape MultiUSB into the approved disk-first window with per-disk and batch-total progress, a live log pane, and USB serials on rows and in every disk-related log line.

**Architecture:** Pure naming/serial helpers live beside session/UI models so they are unit-tested without GPUI. `App` streams those formatted lines into `BatchUi.log` as job events arrive, and `render` rebuilds into settings strip → total progress → disks|log split → footer. Inventory still uses existing `EnumeratedDisk.id.serial`.

**Tech Stack:** Rust, gpui-kit 0.9.1, existing job/session/disk_id pipeline

**Spec:** `docs/superpowers/specs/2026-11-10-disk-first-layout-design.md`

## Global Constraints

- Stay on `main`; no feature branch or PR.
- GPUI Kit components and `cx.theme()` tokens only — stock dark stays.
- Reuse `EnumeratedDisk.id.serial` / WMI; never invent serials.
- Keep wipe → prepare → concurrent copy → path+size verify behavior unchanged.
- Windows subsystem remains `windows`.
- Missing serial text is exactly `S/N unavailable`.

## Review Focus

- Cold start still forces stock dark before first paint.
- Wipe confirmation still lists the selected disks (model text) and remains usable after the layout change.
- Live log updates during prepare/copy, not only after `Done`.
- Total bar uses prep count while formatting, then byte `overall_fraction` during copy/verify.
- A disk with `serial: None` never shows a fabricated number in UI or `Logs\*.log`.

## File map

| File | Responsibility |
|------|----------------|
| `src/session.rs` | `serial_text`, `disk_log_name`; optional `DriveRow` fields if needed for confirmation |
| `src/app.rs` | `drive_row`, `result_lines`, job-event log appends, disk-first `render` |
| Tests in those modules | Pin serial formatting, log line content, prep vs copy total fraction helper if extracted |

---

### Task 1: Serial naming helpers

**Files:**
- Modify: `src/session.rs`
- Test: `src/session.rs` (`mod tests`)

**Interfaces:**
- Consumes: `Option<&str>` serial from `DiskId`
- Produces:
  - `pub fn serial_text(serial: Option<&str>) -> String`
  - `pub fn disk_log_name(number: u32, model: &str, serial: Option<&str>) -> String`
  - Format: `Disk {number} ({model}) · {serial_text}` for log/result lines

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn serial_text_uses_value_or_unavailable() {
    assert_eq!(serial_text(Some("ABC123")), "S/N ABC123");
    assert_eq!(serial_text(Some("  ")), "S/N unavailable");
    assert_eq!(serial_text(None), "S/N unavailable");
}

#[test]
fn disk_log_name_includes_serial_text() {
    assert_eq!(
        disk_log_name(1, "SanDisk", Some("ABC123")),
        "Disk 1 (SanDisk) · S/N ABC123"
    );
    assert_eq!(
        disk_log_name(2, "Kingston", None),
        "Disk 2 (Kingston) · S/N unavailable"
    );
}
```

- [ ] **Step 2: Run tests — expect FAIL**

Run: `cargo test --lib serial_text disk_log_name -- --nocapture`  
Expected: FAIL (functions missing or wrong)

- [ ] **Step 3: Implement `serial_text` and `disk_log_name` in `src/session.rs`**

Trim serials; empty/whitespace → unavailable.

- [ ] **Step 4: Run tests — expect PASS**

Run: `cargo test --lib serial_text disk_log_name`  
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/session.rs
git commit -m "feat: format disk log names with serial numbers"
```

---

### Task 2: Wire serials into rows, results, and live event log

**Files:**
- Modify: `src/app.rs` (`drive_row`, `result_lines`, job event handlers that push log lines)
- Test: `src/app.rs` and/or `src/session.rs` for any extracted append helper

**Interfaces:**
- Consumes: Task 1 `serial_text` / `disk_log_name`; `EnumeratedDisk.id.serial`
- Produces:
  - `drive_row` detail includes serial text
  - `result_lines` uses `disk_log_name` instead of `Disk N (model)` alone
  - On prepare formatting/finished, copy bytes milestones (optional sparse), and final outcomes, append lines to `ui.log` during the run using `disk_log_name`

- [ ] **Step 1: Write failing tests for row/result formatting**

Add tests that build a minimal `EnumeratedDisk` (or call pure helpers wrapping `drive_row`/`result_lines` if you extract them) proving:
- detail contains `S/N ABC` when serial present
- detail contains `S/N unavailable` when absent
- a Passed result line contains the serial text

If `drive_row`/`result_lines` stay private in `app`, extract thin `pub(crate)` formatters into `session.rs` or `app` test-visible helpers rather than exporting GPUI types.

- [ ] **Step 2: Run tests — expect FAIL**

Run: `cargo test --lib -- --nocapture` (filter to the new test names)  
Expected: FAIL

- [ ] **Step 3: Implement formatting + live `ui.log` appends in event handling**

At minimum append on:
- prepare Formatting / Finished (and prepare failure)
- copy/verify terminal outcomes (and keep end-of-batch `result_lines` / `write_run_log` consistent with the same names)

Do not clear the live log on every byte tick; byte progress stays on the progress bars.

- [ ] **Step 4: Run full suite**

Run: `cargo test`  
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/app.rs src/session.rs
git commit -m "feat: show serials on disk rows and stream them into the run log"
```

---

### Task 3: Disk-first render with dual progress and log pane

**Files:**
- Modify: `src/app.rs` (`Render for App`, `disk_list`, `progress_block`, `log_block`)
- Test: extract `pub(crate) fn batch_total_fraction(phase, prepared_done, run_count, copied, source_bytes) -> f32` (or equivalent) in `session.rs` / `app` with unit tests for prep vs copy modes

**Interfaces:**
- Consumes: Task 2 live `ui.log`; existing `overall_fraction`; per-run `DriveRun.fraction` / status
- Produces: layout order settings strip → total progress (when `!runs.is_empty()`) → `h_flex` disks | live log → footer Start / another batch

- [ ] **Step 1: Write failing tests for total fraction modes**

```rust
#[test]
fn batch_total_fraction_uses_prep_ratio_then_bytes() {
    assert!((batch_total_fraction_prep(1, 4) - 0.25).abs() < f32::EPSILON);
    let bytes = batch_total_fraction_copy(&[600, 400], 1000);
    assert!((bytes - 0.5).abs() < 0.001);
}
```

Names may match the helper you introduce; assertions must encode prep count vs `overall_fraction` byte math.

- [ ] **Step 2: Run tests — expect FAIL**

Run: `cargo test --lib batch_total_fraction -- --nocapture`  
Expected: FAIL

- [ ] **Step 3: Implement helpers + rebuild `render`**

- Top: source/browse, filesystem, partition, label (+ note) in a compact strip (not the old full stacked `Form` as the only structure — Form fields may still be used inside the strip).
- When runs active: total `Progress` bound to prep ratio while `Phase::Preparing`, else `overall_fraction` of copy slots.
- Left: scrollable disk list with checkbox, detail (incl. serial), status color, per-disk `Progress`.
- Right: live log pane (`ui.log` lines) with vertical scroll, visible whenever non-empty **or** whenever `phase != Idle` (empty placeholder ok while waiting for first line).
- Footer: Start when unlocked; Run another batch when `show_another`.
- Keep elevation alert/relaunch above the strip.

- [ ] **Step 4: Run full suite**

Run: `cargo test`  
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/app.rs src/session.rs
git commit -m "feat: adopt disk-first layout with total progress and live log"
```

- [ ] **Step 6: Manual smoke (Review Focus)**

Build debug or release, launch elevated if needed, confirm dark UI, disk-first chrome, serials on rows, live log during a short run, and wipe dialog still works. Note result in the SDD ledger when executing.

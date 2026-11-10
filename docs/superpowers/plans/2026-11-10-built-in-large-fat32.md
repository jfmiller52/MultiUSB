# Built-in Large FAT32 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the Large FAT32 `fat32format.exe` handoff with an in-process MIT `fatfs` format on the assigned volume.

**Architecture:** Diskpart still cleans, converts, creates a primary partition, and assigns a letter — without formatting. `VolumeHost::format_large_fat32` opens the raw volume, locks it, and calls `fatfs::format_volume` with FAT32 + the 11-byte label. Job orchestration and fakes talk only through that method; Windows I/O stays in `windows_host`.

**Tech Stack:** Rust, `fatfs` 0.3 (MIT), existing `windows` crate Win32 volume APIs as needed

**Spec:** `docs/superpowers/specs/2026-11-10-built-in-large-fat32-design.md`

## Global Constraints

- Stay on `main`; no feature branch or PR.
- Do not copy Rufus or any GPLv3 FAT32 formatter code.
- Do not ship or require `fat32format.exe`.
- Regular FAT32 (≤32 GiB) stays on diskpart `format fs=fat32`.
- Large FAT32 label limit remains 11 characters; pad to `[u8; 11]` with spaces for `fatfs`.
- Keep wipe → prepare → concurrent copy → path+size verify behavior for other filesystems unchanged.
- Prefer `PrepareFailure::Command(String)` for format failures (no `MissingFat32Format`).

## Review Focus

- Diskpart script for Large FAT32 has no `format fs=` line and still assigns a letter.
- Volume lock failure surfaces as `PrepareFailure::Command` with a readable message; other selected disks still continue.
- Empty label pads to eleven spaces and still formats.
- After format, Windows Explorer / copy path can write files (volume remounted / lock released).
- Regular FAT32 and NTFS/exFAT prepare paths never call `format_large_fat32`.

## File map

| File | Responsibility |
|------|----------------|
| `Cargo.toml` | Add `fatfs` dependency; extend `windows` features if needed |
| `src/format_plan.rs` | Large FAT32 script = clean/convert/create/assign only |
| `src/commands.rs` | Remove fat32format helpers and their tests |
| `src/job.rs` | `VolumeHost::format_large_fat32`; rewrite `finish_large_fat32`; drop `MissingFat32Format` |
| `src/windows_host.rs` | Implement format via raw volume + `fatfs` |
| Specs/plans/README if they mention fat32format | Point at built-in path |

---

### Task 1: Large FAT32 diskpart script without format

**Files:**
- Modify: `src/format_plan.rs`
- Test: `src/format_plan.rs` (`mod tests`)

**Interfaces:**
- Consumes: existing `prepare_plan(...)`
- Produces: `PreparePlan::LargeFat32 { script }` whose script is exactly:
  `rescan\r\nselect disk {n}\r\nclean\r\nconvert {gpt|mbr}\r\ncreate partition primary\r\nassign\r\nexit\r\n`

- [ ] **Step 1: Write the failing test**

Update `fat32_size_gate_and_large_fat32` so both Large FAT32 cases expect the no-format script (helper may take only `disk` + `convert`, no `format_line`).

- [ ] **Step 2: Run test — expect FAIL**

Run: `cargo test --lib fat32_size_gate_and_large_fat32`  
Expected: FAIL (still embeds `format fs=ntfs quick`)

- [ ] **Step 3: Change Large FAT32 script construction in `prepare_plan`**

Build the assign-only script; leave other filesystems on `build_script` with a format line.

- [ ] **Step 4: Run test — expect PASS**

Run: `cargo test --lib fat32_size_gate_and_large_fat32`  
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/format_plan.rs
git add docs/superpowers/specs/2026-11-10-built-in-large-fat32-design.md
git add docs/superpowers/plans/2026-11-10-built-in-large-fat32.md
git commit -m "feat: drop NTFS handoff from Large FAT32 diskpart script"
```

---

### Task 2: Host API + job path for built-in format

**Files:**
- Modify: `src/job.rs`
- Modify: `src/commands.rs` (remove fat32format helpers/tests only)
- Test: `src/job.rs` (`mod tests`)

**Interfaces:**
- Consumes: `PreparePlan::LargeFat32 { script }`, `VolumeHost`
- Produces:
  - `VolumeHost::format_large_fat32(&mut self, letter: char, label: &str) -> Result<(), String>`
  - `finish_large_fat32`: diskpart → letter → `format_large_fat32` (no `run_command`, no `exe_path` check)
  - Remove `PrepareFailure::MissingFat32Format` and its `failure_text` arm
  - Remove `fat32format_beside_exe` / `fat32format_command` from `commands.rs`

- [ ] **Step 1: Write failing tests**

Replace `large_fat32_runs_ntfs_handoff_then_fat32format_and_label` with:

```rust
#[test]
fn large_fat32_runs_assign_script_then_format_large_fat32() {
    // FakeHost records format_large_fat32 calls as (letter, label).
    // After prepare_all LargeFat32 / MBR / "STORE":
    // scripts == [assign-only script for disk 4]
    // format_calls == [('E', "STORE")]
    // commands is empty
    // Ok prepared letter E
}
```

Replace `large_fat32_missing_sibling_fails_after_the_handoff_script` with:

```rust
#[test]
fn large_fat32_format_error_is_command_failure() {
    // host.format_error = Some("lock failed"); letter E present
    // Done → Err(PrepareFailure::Command("lock failed"))
    // scripts still contain the assign-only script
}
```

Add:

```rust
#[test]
fn other_filesystems_do_not_call_format_large_fat32() {
    // prepare one NTFS disk with letter; assert format_calls.is_empty()
}
```

Update `failure_text` / `missing_letter_wins_over_missing_fat32format` / any `MissingFat32Format` match arms so they compile with the new enum.

- [ ] **Step 2: Run tests — expect FAIL**

Run: `cargo test --lib large_fat32 other_filesystems_do_not_call_format_large_fat32`  
Expected: FAIL (method / expectations missing)

- [ ] **Step 3: Implement trait method, FakeHost recording, and `finish_large_fat32`**

Wire `prepare_all` LargeFat32 arm to the new finish path. Delete fat32format helpers and their unit tests in `commands.rs`. Keep `volume_label_command` (unused by Large FAT32 after this).

- [ ] **Step 4: Run tests — expect PASS**

Run: `cargo test --lib`  
Expected: PASS (all lib tests)

- [ ] **Step 5: Commit**

```bash
git add src/job.rs src/commands.rs
git commit -m "feat: route Large FAT32 through VolumeHost::format_large_fat32"
```

---

### Task 3: Windows `fatfs` format implementation

**Files:**
- Modify: `Cargo.toml` (add `fatfs = "0.3"`; add Win32 features needed for volume lock/open if not already present)
- Modify: `src/windows_host.rs`
- Test: optional pure helper unit test for label padding if extracted; otherwise rely on Task 2 fakes + manual USB check listed below

**Interfaces:**
- Consumes: `VolumeHost::format_large_fat32`
- Produces: Windows implementation that:
  1. Opens `\\.\X:` with read/write sharing appropriate for exclusive format
  2. Locks (and dismounts if required) the volume
  3. Seeks to 0 and calls `fatfs::format_volume` with `FormatVolumeOptions::new().fat_type(FatType::Fat32).volume_label(padded)`
  4. Unlocks / closes so the letter is usable for copy
  5. Maps I/O and `fatfs` errors to `Err(String)`

Label padding helper (same module or small private fn): ASCII bytes of `label`, truncated/padded with spaces to exactly 11 bytes.

- [ ] **Step 1: Add dependency and implement `format_large_fat32` on `WindowsHost`**

Use `std::fs::OpenOptions` / `windows` APIs as needed. Keep `CREATE_NO_WINDOW` behavior for other host methods unchanged.

- [ ] **Step 2: Compile**

Run: `cargo check`  
Expected: SUCCESS

- [ ] **Step 3: Run full test suite**

Run: `cargo test`  
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock src/windows_host.rs
git commit -m "feat: format Large FAT32 in-process with fatfs"
```

- [ ] **Step 5: Manual check on a disposable USB (when available)**

Select Large FAT32, wipe one removable stick >32 GiB if possible (smaller still valid for this path), confirm Explorer shows FAT32 + label, then a short copy/verify batch passes.

---

### Task 4: Docs cleanup

**Files:**
- Modify: `docs/superpowers/specs/2026-11-09-multiusb-gui-design.md` (Large FAT32 / missing-tool bullets)
- Modify: `docs/superpowers/plans/2026-11-09-multiusb-gui.md` only if it still asserts fat32format behavior as current truth — prefer a short note that Large FAT32 is superseded by this plan
- Modify: any user-facing README that mentions `fat32format.exe`

**Interfaces:**
- Produces: docs that say Large FAT32 is built-in via `fatfs`; no sidecar required

- [ ] **Step 1: Update the cited sections**

Replace “always goes through `fat32format.exe`” with the assign-then-`format_large_fat32` flow. Remove “fat32format.exe missing” failure bullet.

- [ ] **Step 2: Commit**

```bash
git add docs README.md
git commit -m "docs: Large FAT32 is built-in; drop fat32format.exe"
```

---

### Task 5: Whole-branch review gate

- [ ] **Step 1: Confirm no remaining fat32format references in `src/`**

Run: `rg -n fat32format src`  
Expected: no matches

- [ ] **Step 2: `cargo test` green**

- [ ] **Step 3: Fresh reviewer pass on the branch diff vs the design spec**

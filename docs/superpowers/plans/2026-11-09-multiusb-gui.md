# MultiUSB GUI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a Windows iced app that wipes checked removable disks, formats them with the chosen filesystem and partition style, copies one source folder onto those disks at the same time, and verifies every file's path and size.

**Architecture:** Pure functions own labels, diskpart script text, source walks, copy, verify, progress, and disk identity. A `VolumeHost` trait is the only door to `diskpart`, `fat32format.exe`, and WMI. The iced window sends a run request to a worker thread and renders events. Copy and verify never call the host.

**Tech Stack:** Rust edition 2021, iced 0.14, rfd for the folder picker, chrono for log timestamps, serde_json for the disk inventory, the `windows` crate for elevation.

**Spec:** `docs/superpowers/specs/2026-11-09-multiusb-gui-design.md`

## Global Constraints

- Workspace is `C:\MultiUSB`. Implement on `main`. Do not create a feature branch, worktree, or pull request.
- Windows only. Automated tests must not run `diskpart` and must not wipe a real disk.
- UI toolkit is iced 0.14. One window titled `MultiUSB`. Default theme. One column. No menus.
- Do not copy `fat32format.exe` into this repo. Look for it beside `MultiUSB.exe` only.
- `src/copy.rs` and `src/verify.rs` must not call `diskpart` or import wipe, format-plan, host, or job modules.
- Filesystem dropdown, partition dropdown, and volume label have no default. Label box starts empty. Empty label is valid.
- Filesystem entries, in order: FAT32, NTFS, exFAT, Large FAT32. Partition entries, in order: MBR, GPT.
- Label characters: letters, digits, space, hyphen, underscore. Limits: FAT32 11, Large FAT32 11, exFAT 15, NTFS 32.
- FAT32 on a disk larger than 32 GiB (`32 * 1024 * 1024 * 1024` bytes) does not call `diskpart`. Large FAT32 uses `fat32format.exe` at every size.
- Prepare disks one at a time. Copy every successfully prepared drive at the same time. No concurrency cap.
- `diskpart` retries: 3 attempts, 3 seconds between tries. Copy retries: 3 attempts, 10 seconds between tries.
- Verification compares file path and size only, case-insensitively. Cap named mismatches at 20.
- Logs go in `Logs` under the process current working directory. Nothing about the form is saved across launches.
- "Run another batch" keeps source folder, filesystem, partition style, and label. It clears checks, progress, and the on-screen log. It does not start a wipe.
- Quick format only. Do not follow directory junctions or symlinks. Do not delete destination files that are absent from the source.

## Review Focus

These are the inputs most likely to destroy data or silently copy the wrong tree. Each one has a test in the task that owns it.

- A removable disk's number changes after the user checks it. The wipe must follow the PNP device id to the new number.
- Two removable disks share a serial and size and have no PNP id. The app must refuse both rather than pick a number.
- A checked disk contains the Windows system volume. The run must stop with no `diskpart` script started.
- The destination already holds a file the source does not. That file must still be there after the copy.
- The source contains a hidden file. The walk and the copy must include it.

## File structure

- `Cargo.toml`, `src/lib.rs`, `src/main.rs` — package `multiusb`, binary name `MultiUSB`.
- `src/label.rs` — filesystem and partition enums, label validation.
- `src/format_plan.rs` — diskpart script text and the Large FAT32 / FAT32 size decision.
- `src/source.rs` — walk the source tree, skip links.
- `src/verify.rs` — path and size check, mismatch summary.
- `src/copy.rs` — copy entries, retries, byte counts. No wipe imports.
- `src/session.rs` — blank form, Start enabled, confirmation text, run-another-batch, overall progress.
- `src/log_file.rs` — `Logs/<stamp>.log` under the current directory.
- `src/disk_id.rs` — rematch and the system-disk gate.
- `src/commands.rs` — retry helper, `fat32format` and `label` command lines, disk JSON parser.
- `src/job.rs` — sequential prepare, then concurrent copy and verify. Defines `VolumeHost`.
- `src/windows_host.rs` — WMI list, `diskpart`, elevation. Implements `VolumeHost`.
- `src/app.rs` — iced state, messages, view.

`src/copy.rs` and `src/verify.rs` must not reference `format_plan`, `disk_id`, `commands`, `job`, or `windows_host`.

---

### Task 1: Label rules

**Files:**
- Create: `Cargo.toml`, `src/lib.rs`, `src/label.rs`, `src/main.rs`
- Test: `src/label.rs`

**Interfaces:**
- Consumes: nothing
- Produces:
  - `pub enum Filesystem { Fat32, Ntfs, ExFat, LargeFat32 }`
  - `impl Filesystem { pub const ALL: [Filesystem; 4]; pub fn as_str(self) -> &'static str; pub fn label_limit(self) -> usize; }`
  - `pub enum PartitionStyle { Mbr, Gpt }`
  - `impl PartitionStyle { pub const ALL: [PartitionStyle; 2]; pub fn as_str(self) -> &'static str; }`
  - `pub enum LabelError { Character { ch: char }, TooLong { len: usize, limit: usize } }`
  - `pub fn validate_label(filesystem: Filesystem, label: &str) -> Result<(), LabelError>`

- [ ] **Step 1: Write the failing test**

Create the package with `cargo init --name multiusb` in `C:\MultiUSB` (the `docs` folder is already there). Add `src/lib.rs` with `pub mod label;`. Put this test in `src/label.rs` under `#[cfg(test)]`:

```rust
#[test]
fn dropdown_order_and_limits() {
    let names: Vec<_> = Filesystem::ALL.iter().map(|fs| fs.as_str()).collect();
    assert_eq!(names, ["FAT32", "NTFS", "exFAT", "Large FAT32"]);
    assert_eq!(Filesystem::Fat32.label_limit(), 11);
    assert_eq!(Filesystem::LargeFat32.label_limit(), 11);
    assert_eq!(Filesystem::ExFat.label_limit(), 15);
    assert_eq!(Filesystem::Ntfs.label_limit(), 32);
    let parts: Vec<_> = PartitionStyle::ALL.iter().map(|p| p.as_str()).collect();
    assert_eq!(parts, ["MBR", "GPT"]);
}

#[test]
fn empty_label_is_valid_and_quotes_are_not() {
    assert!(validate_label(Filesystem::Ntfs, "").is_ok());
    assert!(validate_label(Filesystem::Ntfs, "MCD-NSO-USB").is_ok());
    assert_eq!(
        validate_label(Filesystem::Ntfs, "a\"b"),
        Err(LabelError::Character { ch: '"' })
    );
    assert_eq!(
        validate_label(Filesystem::Ntfs, "a\nb"),
        Err(LabelError::Character { ch: '\n' })
    );
    assert!(validate_label(Filesystem::ExFat, "123456789012345").is_ok());
    assert_eq!(
        validate_label(Filesystem::Fat32, "123456789012345"),
        Err(LabelError::TooLong { len: 15, limit: 11 })
    );
    assert_eq!(
        validate_label(Filesystem::ExFat, "1234567890123456"),
        Err(LabelError::TooLong { len: 16, limit: 15 })
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib label:: -- --test-threads=1`

Expected: FAIL to compile because `Filesystem` and `validate_label` are missing.

- [ ] **Step 3: Implement the enums and `validate_label` in `src/label.rs`**

`as_str` returns the dropdown strings above. `label_limit` returns 11, 32, 15, and 11 for FAT32, NTFS, exFAT, and Large FAT32. `validate_label` accepts an empty string. Every character must be ASCII alphanumeric, space, `-`, or `_`. Length is counted in characters and must be `<= label_limit`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib label:: -- --test-threads=1`

Expected: PASS

- [ ] **Step 5: Commit**

```powershell
git add Cargo.toml src/lib.rs src/main.rs src/label.rs
git commit -m @"
feat: validate volume labels

"@
```

---

### Task 2: Prepare plans

**Files:**
- Create: `src/format_plan.rs`
- Modify: `src/lib.rs`
- Test: `src/format_plan.rs`

**Interfaces:**
- Consumes: `Filesystem`, `PartitionStyle` from `label`
- Produces:
  - `pub const FAT32_MAX_INCLUSIVE_BYTES: u64 = 32 * 1024 * 1024 * 1024;`
  - `pub enum PreparePlan { Script(String), Fat32TooLarge, LargeFat32 { script: String } }`
  - `pub fn prepare_plan(disk_number: u32, style: PartitionStyle, filesystem: Filesystem, label: &str, size_bytes: u64) -> PreparePlan`

- [ ] **Step 1: Write the failing test**

```rust
fn script(disk: u32, convert: &str, format_line: &str) -> String {
    format!("rescan\r\nselect disk {disk}\r\nclean\r\nconvert {convert}\r\ncreate partition primary\r\n{format_line}\r\nassign\r\nexit\r\n")
}

#[test]
fn ntfs_gpt_and_mbr_and_exfat_scripts() {
    assert_eq!(
        prepare_plan(2, PartitionStyle::Gpt, Filesystem::Ntfs, "MCD-NSO-USB", 100),
        PreparePlan::Script(script(2, "gpt", "format fs=ntfs quick label=\"MCD-NSO-USB\""))
    );
    assert_eq!(
        prepare_plan(3, PartitionStyle::Mbr, Filesystem::Ntfs, "", 100),
        PreparePlan::Script(script(3, "mbr", "format fs=ntfs quick label=\"\""))
    );
    assert_eq!(
        prepare_plan(4, PartitionStyle::Gpt, Filesystem::ExFat, "STORE", 100),
        PreparePlan::Script(script(4, "gpt", "format fs=exfat quick label=\"STORE\""))
    );
}

#[test]
fn fat32_size_gate_and_large_fat32() {
    let limit = FAT32_MAX_INCLUSIVE_BYTES;
    assert_eq!(
        prepare_plan(2, PartitionStyle::Gpt, Filesystem::Fat32, "LAB", limit),
        PreparePlan::Script(script(2, "gpt", "format fs=fat32 quick label=\"LAB\""))
    );
    assert_eq!(
        prepare_plan(2, PartitionStyle::Gpt, Filesystem::Fat32, "LAB", limit + 1),
        PreparePlan::Fat32TooLarge
    );
    assert_eq!(
        prepare_plan(2, PartitionStyle::Mbr, Filesystem::LargeFat32, "LAB", limit + 1),
        PreparePlan::LargeFat32 { script: script(2, "mbr", "format fs=ntfs quick") }
    );
    assert_eq!(
        prepare_plan(2, PartitionStyle::Gpt, Filesystem::LargeFat32, "LAB", 1024),
        PreparePlan::LargeFat32 { script: script(2, "gpt", "format fs=ntfs quick") }
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib format_plan:: -- --test-threads=1`

Expected: FAIL to compile because `prepare_plan` is missing.

- [ ] **Step 3: Implement `prepare_plan` in `src/format_plan.rs`**

Return `Fat32TooLarge` when the filesystem is FAT32 and `size_bytes > FAT32_MAX_INCLUSIVE_BYTES`. Large FAT32 always returns `LargeFat32` whose format line is `format fs=ntfs quick` with no label. Every script uses `\r\n`, lowercase `convert gpt` or `convert mbr`, and the line order in the test helper. Do not append `noerr`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib format_plan:: -- --test-threads=1`

Expected: PASS

- [ ] **Step 5: Commit**

```powershell
git add src/lib.rs src/format_plan.rs
git commit -m @"
feat: build diskpart prepare plans

"@
```

---

### Task 3: Walk and verify

**Files:**
- Create: `src/source.rs`, `src/verify.rs`
- Modify: `src/lib.rs`
- Test: `src/source.rs`, `src/verify.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks
- Produces:
  - `pub enum EntryKind { File, Directory }`
  - `pub struct SourceEntry { pub relative: PathBuf, pub kind: EntryKind, pub size: u64 }`
  - `pub struct Walk { pub entries: Vec<SourceEntry>, pub skipped_links: Vec<PathBuf> }`
  - `pub fn walk_source(root: &Path) -> std::io::Result<Walk>`
  - `pub struct VerifyReport { pub checked: usize, pub missing: Vec<PathBuf>, pub size_mismatch: Vec<PathBuf> }`
  - `impl VerifyReport { pub fn passed(&self) -> bool; }`
  - `pub fn verify_tree(dest: &Path, entries: &[SourceEntry]) -> VerifyReport`
  - `pub fn mismatch_summary(report: &VerifyReport) -> String`

- [ ] **Step 1: Write the failing test**

`walk_includes_empty_dir_nested_file_and_hidden_file`: a directory `empty`, a file `nested\a.txt`, and a file marked with `cmd /C attrib +H` are all in `entries`. `walk_does_not_follow_junction`: `cmd /C mklink /J` points at a directory that contains `secret.txt`; `secret.txt` is not in `entries`; the junction's relative path is in `skipped_links`. `verify_is_case_insensitive`: list path `file.txt`, dest file `File.TXT` with the same bytes, `passed()` is true. `verify_reports_missing_and_size`: a missing file and a shorter file set `passed()` false and fill `missing` and `size_mismatch`. `mismatch_summary_caps_at_20`: 25 missing paths produce exactly 20 path lines plus `and 5 more`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib source::`

Expected: FAIL to compile because `walk_source` is missing.

Run: `cargo test --lib verify::`

Expected: FAIL to compile because `verify_tree` is missing.

- [ ] **Step 3: Implement `walk_source`, `verify_tree`, and `mismatch_summary`**

`walk_source` uses `std::fs::read_dir`. Do not follow a directory junction or a symlink; record its relative path in `skipped_links`. Record directories, including empty ones, and regular files with their byte length. Sort entries by relative path. `verify_tree` checks files only. Resolve the dest path by walking components case-insensitively. `mismatch_summary` lists missing paths first, then size mismatches, up to 20 paths, then `and N more` when the combined count exceeds 20.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib source::`

Expected: PASS

Run: `cargo test --lib verify::`

Expected: PASS

- [ ] **Step 5: Commit**

```powershell
git add src/lib.rs src/source.rs src/verify.rs
git commit -m @"
feat: walk source trees and verify copies

"@
```

---

### Task 4: Copy

**Files:**
- Create: `src/copy.rs`
- Modify: `src/lib.rs`
- Test: `src/copy.rs`

**Interfaces:**
- Consumes: `SourceEntry`, `EntryKind`, `walk_source`
- Produces:
  - `pub struct RetryPolicy { pub attempts: u32, pub wait: Duration }`
  - `pub const COPY_RETRY: RetryPolicy = RetryPolicy { attempts: 3, wait: Duration::from_secs(10) };`
  - `pub enum CopyError { File { path: PathBuf, attempts: u32 } }`
  - `pub fn extended_path(path: &Path) -> PathBuf`
  - `pub fn copy_tree(source_root: &Path, dest_root: &Path, entries: &[SourceEntry], policy: RetryPolicy, sleep: &dyn Fn(Duration), on_bytes: &dyn Fn(u64)) -> Result<u64, CopyError>`

- [ ] **Step 1: Write the failing test**

Assert `extended_path` turns `C:\foo` into `\\?\C:\foo` and `\\server\share\a` into `\\?\UNC\server\share\a`. Copy a tree that has an empty directory and a nested file; the empty directory exists at the dest and `on_bytes` is called with the file size. An empty entry list returns `Ok(0)`. A dest file named `extra.txt` that is not in the source is still present after a successful copy. An existing dest file is overwritten with the source bytes. A hidden source file is copied with the same bytes. A file that cannot be copied (destination parent replaced by a file so create fails) calls `sleep` twice with `Duration::from_secs(10)` and returns `CopyError::File { attempts: 3, .. }`. Assert `COPY_RETRY.attempts == 3`.

Also assert `include_str!("copy.rs")` and `include_str!("verify.rs")` do not contain `diskpart`, `format_plan`, `disk_id`, `windows_host`, or `job`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib copy:: -- --test-threads=1`

Expected: FAIL to compile because `copy_tree` is missing.

- [ ] **Step 3: Implement `copy_tree` and `extended_path` in `src/copy.rs`**

Create directories from directory entries. Copy file bytes with `std::fs::copy` on the extended path, then report the new total of bytes copied. Do not delete dest files that are not in `entries`. On failure, sleep `policy.wait` between tries, `policy.attempts` tries total, then return `CopyError::File` and stop. Do not start later files after that error. Pass `COPY_RETRY` from production callers. The test passes a `sleep` that records the duration and does not really wait.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib copy:: -- --test-threads=1`

Expected: PASS

- [ ] **Step 5: Commit**

```powershell
git add src/lib.rs src/copy.rs
git commit -m @"
feat: copy trees with retries and byte progress

"@
```

---

### Task 5: Session, progress, and logs

**Files:**
- Create: `src/session.rs`, `src/log_file.rs`
- Modify: `src/lib.rs`, `Cargo.toml` (`chrono = "0.4"`)
- Test: `src/session.rs`, `src/log_file.rs`

**Interfaces:**
- Consumes: `Filesystem`, `PartitionStyle`, `validate_label`
- Produces:
  - `pub struct BatchForm { pub source: Option<PathBuf>, pub filesystem: Option<Filesystem>, pub partition: Option<PartitionStyle>, pub label: String }`
  - `pub struct DriveRow { pub key: String, pub checked: bool, pub model: String, pub detail: String }`
  - `pub struct BatchUi { pub form: BatchForm, pub rows: Vec<DriveRow>, pub log: Vec<String>, pub show_another: bool }`
  - `pub fn initial_form() -> BatchForm`
  - `pub fn start_enabled(form: &BatchForm, checked: usize) -> bool`
  - `pub fn confirmation_body(form: &BatchForm, rows: &[DriveRow]) -> String`
  - `pub fn run_another_batch(ui: &mut BatchUi)`
  - `pub fn overall_fraction(bytes_copied: &[u64], source_bytes: u64) -> f32`
  - `pub fn write_run_log(cwd: &Path, stamp: &str, lines: &[String]) -> std::io::Result<PathBuf>`

- [ ] **Step 1: Write the failing test**

`initial_form()` has `filesystem: None`, `partition: None`, `label == ""`, `source: None`. `start_enabled` is false when the filesystem is `None`, when the partition is `None`, when the source is `None`, when `checked == 0`, and when `validate_label` would fail. It is true for a real source path that exists in the test, ExFAT, MBR, label `STORE`, and `checked == 1`. `confirmation_body` contains the model of a checked row and does not contain the model of an unchecked row, and it contains `exFAT`, `MBR`, and the label. `run_another_batch` keeps source `D:\USBContents`, ExFAT, MBR, and label `STORE`; every row's `checked` becomes false; `log` is empty; `show_another` is false. `overall_fraction(&[50, 100], 100)` is `0.75`. `write_run_log` creates `cwd/Logs/2026-11-09_13-04-00.log` with the given lines. A second call with the same stamp creates `2026-11-09_13-04-00-2.log`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib session::`

Expected: FAIL to compile because `initial_form` is missing.

Run: `cargo test --lib log_file::`

Expected: FAIL to compile because `write_run_log` is missing.

- [ ] **Step 3: Implement session and log helpers**

`start_enabled` requires `source` to be an existing directory, both dropdowns `Some`, `checked >= 1`, and `validate_label` ok. `overall_fraction` divides the sum of `bytes_copied` by `source_bytes * bytes_copied.len()`. Return `0.0` when the slice is empty or `source_bytes` is 0. `run_another_batch` does not clear `form`. `write_run_log` creates `Logs` if needed and uses `<stamp>-2`, `<stamp>-3`, when the file already exists.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib session::`

Expected: PASS

Run: `cargo test --lib log_file::`

Expected: PASS

- [ ] **Step 5: Commit**

```powershell
git add Cargo.toml src/lib.rs src/session.rs src/log_file.rs
git commit -m @"
feat: track batch settings, progress, and run logs

"@
```

---

### Task 6: Disk identity

**Files:**
- Create: `src/disk_id.rs`
- Modify: `src/lib.rs`
- Test: `src/disk_id.rs`

**Interfaces:**
- Consumes: nothing from format plans
- Produces:
  - `pub struct DiskId { pub pnp_device_id: Option<String>, pub serial: Option<String>, pub size_bytes: u64 }`
  - `pub struct EnumeratedDisk { pub number: u32, pub model: String, pub size_bytes: u64, pub letters: Vec<char>, pub id: DiskId, pub media_type: String, pub contains_system_volume: bool }`
  - `pub enum Rematch { Unique(u32), NotUnique, NotRemovable, Missing }`
  - `pub fn rematch(selected: &DiskId, current: &[EnumeratedDisk]) -> Rematch`
  - `pub enum Gate { Ready(Vec<Result<EnumeratedDisk, Rematch>>), AbortSystemDisk { number: u32, model: String } }`
  - `pub fn gate(selected: &[DiskId], current: &[EnumeratedDisk]) -> Gate`

- [ ] **Step 1: Write the failing test**

Build two current disks. Disk number 5 has PNP `USB\KING`, serial `ABC`, size 10. Disk number 6 has the same serial and size and PNP `None`, media `Removable Media`. A selected id whose PNP is `USB\KING` and whose old number is not stored returns `Rematch::Unique(5)` even when another disk shares the serial. Two selected ids with `pnp_device_id: None`, serial `ABC`, and the same size, against two current disks that both have that serial, size, no PNP, and removable media, each return `NotUnique`. A PNP match whose `media_type` is `Fixed hard disk media` returns `NotRemovable`. No matching PNP returns `Missing`. An id with both PNP and serial `None` returns `NotUnique`. `gate` on a list whose second match has `contains_system_volume: true` returns `AbortSystemDisk` with that disk's number and model. `gate` on one missing id and one unique removable id returns `Ready` with `Err(Rematch::Missing)` then `Ok` of that disk. Media match uses the substring `Removable`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib disk_id:: -- --test-threads=1`

Expected: FAIL to compile because `rematch` is missing.

- [ ] **Step 3: Implement `rematch` and `gate` in `src/disk_id.rs`**

If `pnp_device_id` is `Some`, identity candidates are disks with that PNP id. Otherwise if `serial` is `Some`, candidates share that serial and `size_bytes`. If both are `None`, return `NotUnique` without using size. Zero candidates is `Missing`. Two or more is `NotUnique`. One candidate whose `media_type` does not contain `Removable` is `NotRemovable`. Otherwise `Unique(number)`. `gate` rematches every selected id. If any unique match has `contains_system_volume`, return `AbortSystemDisk` and no `Ready` list. Otherwise return `Ready` with one result per selected id, in order. A non-unique rematch is `Err` of that `Rematch`, not an abort.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib disk_id:: -- --test-threads=1`

Expected: PASS

- [ ] **Step 5: Commit**

```powershell
git add src/lib.rs src/disk_id.rs
git commit -m @"
feat: match removable disks before a wipe

"@
```

---

### Task 7: Commands and disk JSON

**Files:**
- Create: `src/commands.rs`
- Modify: `src/lib.rs`, `Cargo.toml` (`serde = { version = "1", features = ["derive"] }`, `serde_json = "1"`)
- Test: `src/commands.rs`

**Interfaces:**
- Consumes: `EnumeratedDisk`, `DiskId`
- Produces:
  - `pub struct ExternalCommand { pub program: PathBuf, pub args: Vec<String>, pub stdin: String }`
  - `pub fn run_with_retries(attempts: u32, wait: Duration, sleep: &dyn Fn(Duration), op: &mut dyn FnMut() -> Result<(), ()>) -> Result<(), ()>`
  - `pub const DISKPART_ATTEMPTS: u32 = 3;`
  - `pub const DISKPART_WAIT: Duration = Duration::from_secs(3);`
  - `pub fn fat32format_beside_exe(exe: &Path) -> PathBuf`
  - `pub fn fat32format_command(exe: &Path, letter: char) -> ExternalCommand`
  - `pub fn volume_label_command(letter: char, label: &str) -> ExternalCommand`
  - `pub fn parse_disk_json(json: &str) -> Result<Vec<EnumeratedDisk>, String>` keeps only media types that contain `Removable`
  - `pub fn parse_all_disk_json(json: &str) -> Result<Vec<EnumeratedDisk>, String>` keeps every media type

- [ ] **Step 1: Write the failing test**

`run_with_retries(3, DISKPART_WAIT, sleep, op)` where `op` fails twice then succeeds calls `sleep` twice with 3 seconds and returns `Ok`. A third failure returns `Err`. `fat32format_beside_exe(Path::new(r"C:\App\MultiUSB.exe"))` equals `C:\App\fat32format.exe`. `fat32format_command` uses that program, args `["E:"]`, stdin `y\r\n`. `volume_label_command('E', "STORE")` is `cmd.exe` with args `["/C", "label E:STORE"]` and empty stdin. `volume_label_command('E', "")` is `cmd.exe` with args `["/C", "label E:"]` and stdin `\r\n`. `parse_disk_json` on an array of one object with `MediaType` `Removable Media`, `Index` 2, `Model` `Kingston`, `Size` 100, `PNPDeviceID` `USB\\X`, `SerialNumber` `ABC`, `Letters` `["E"]`, `ContainsSystem` false returns that disk, and drops a second object whose `MediaType` is `Fixed hard disk media`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib commands:: -- --test-threads=1`

Expected: FAIL to compile because those functions are missing.

- [ ] **Step 3: Implement the command helpers and JSON parser**

`run_with_retries` runs `op` up to `attempts` times and sleeps only between failures. Both parsers map `Index` to `number`, `Size` to `size_bytes`, and null serial or PNP to `None`. `parse_disk_json` drops media types that do not contain `Removable`. `parse_all_disk_json` keeps them. The fixed-disk object from the test is present only in the `parse_all_disk_json` result.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib commands:: -- --test-threads=1`

Expected: PASS

- [ ] **Step 5: Commit**

```powershell
git add Cargo.toml src/lib.rs src/commands.rs
git commit -m @"
feat: parse disk inventory and format commands

"@
```

---

### Task 8: Prepare, then copy concurrently

**Files:**
- Create: `src/job.rs`
- Modify: `src/lib.rs`
- Test: `src/job.rs`

**Interfaces:**
- Consumes: `prepare_plan`, `PreparePlan`, `gate`, `Gate`, `DiskId`, `EnumeratedDisk`, `Rematch`, `walk_source`, `copy_tree`, `COPY_RETRY`, `verify_tree`, `mismatch_summary`, `fat32format_command`, `volume_label_command`, `fat32format_beside_exe`, `ExternalCommand`
- Produces:
  - `pub trait VolumeHost { fn list_all(&mut self) -> Result<Vec<EnumeratedDisk>, String>; fn diskpart(&mut self, script: &str) -> Result<(), String>; fn letter_for(&mut self, id: &DiskId) -> Result<Option<char>, String>; fn run_command(&mut self, cmd: &ExternalCommand) -> Result<(), String>; fn exe_path(&self) -> PathBuf; }`
  - `pub enum PrepareFailure { Fat32TooLarge, MissingFat32Format, Rematch(Rematch), Diskpart, NoLetter, Command(String) }`
  - `pub struct Prepared { pub id: DiskId, pub letter: char, pub model: String }`
  - `pub enum PrepareAll { AbortSystemDisk { number: u32, model: String }, Done(Vec<Result<Prepared, PrepareFailure>>) }`
  - `pub enum PrepareNotice { Formatting { index: usize, total: usize }, Finished { index: usize } }`
  - `pub fn prepare_all(selected: &[DiskId], filesystem: Filesystem, style: PartitionStyle, label: &str, host: &mut dyn VolumeHost, on_prepare: &mut dyn FnMut(PrepareNotice)) -> PrepareAll`
  - `pub struct CopyTarget { pub source: PathBuf, pub dest: PathBuf, pub entries: Vec<SourceEntry> }`
  - `pub enum DriveOutcome { Passed { checked: usize }, Failed { reason: String } }`
  - `pub fn copy_and_verify_all(targets: Vec<CopyTarget>, on_enter: &(dyn Fn() + Sync), on_bytes: &(dyn Fn(usize, u64, u64) + Sync)) -> Vec<DriveOutcome>`
  - `pub fn failure_text(failure: &PrepareFailure) -> &'static str`

- [ ] **Step 1: Write the failing test**

Use a fake `VolumeHost` that records `diskpart` scripts and returns letters. `prepare_all` when `gate` would abort does not call `diskpart`. A FAT32 selection whose size is `FAT32_MAX_INCLUSIVE_BYTES + 1` records no script and yields `PrepareFailure::Fat32TooLarge`, while a second selected NTFS disk still gets a script. When the first disk's `diskpart` returns an error, the second disk still gets a script. Large FAT32 with `fat32format.exe` present beside `exe_path` records the NTFS handoff script, then `run_command` for `fat32format.exe` and for `label`. When that sibling file is absent, the result is `MissingFat32Format` after the handoff script. `on_prepare` receives `Formatting` for a disk before its script. Two `CopyTarget`s call `on_enter`, which increments a counter and waits until a watcher sets a flag. The watcher sets the flag only after the counter reaches 2, or after 2 seconds. Assert the watcher observed 2 before the timeout. Both outcomes are `Passed { checked: 1 }` for a one-file source, and `on_bytes` is called with `(index, copied, total)`. A third pair, one dest whose parent is a file and one normal dest, finishes `Failed` for the bad dest and `Passed` for the good dest. The failed `reason` starts with `Copy failed after 3 tries:` and does not start with `Verification failed`. `failure_text` returns `Choose Large FAT32. Disk is larger than 32 GB.`, `fat32format.exe was not found beside MultiUSB.exe.`, `Disk could not be matched uniquely.`, `Disk is no longer removable.`, `Disk is no longer present.`, `diskpart failed after 3 tries.`, and `No drive letter after format.` for `Fat32TooLarge`, `MissingFat32Format`, `Rematch(NotUnique)`, `Rematch(NotRemovable)`, `Rematch(Missing)`, `Diskpart`, and `NoLetter`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib job:: -- --test-threads=1`

Expected: FAIL to compile because `prepare_all` is missing.

- [ ] **Step 3: Implement `prepare_all` and `copy_and_verify_all`**

`prepare_all` calls `host.list_all()`, then `gate`. On `AbortSystemDisk`, return immediately and do not call `on_prepare`. Otherwise handle each selected disk in order. Emit `PrepareNotice::Formatting` first. Call `host.list_all()` again immediately before that disk's `diskpart` and `rematch` that disk only. `Fat32TooLarge` skips `diskpart`. Large FAT32 runs the handoff script, reads the letter, checks that `fat32format_beside_exe(host.exe_path())` exists, runs that command, then runs `volume_label_command`. Other filesystems run `PreparePlan::Script` and then read the letter. Emit `PrepareNotice::Finished` after each disk. A `diskpart` error becomes `PrepareFailure::Diskpart` and the loop continues. `copy_and_verify_all` uses `std::thread::scope`, one thread per target, calls `on_enter` at the start of each thread, copies, and reports bytes through `on_bytes` as `(index, bytes_copied, source_total)`. Verify only after `copy_tree` returns `Ok`. Return outcomes in input order. Copy failure reason: `Copy failed after 3 tries: ` plus the file's display path. Verification failure reason: `Verification failed. checked {n}, missing {m}, size mismatches {s}.` followed by `mismatch_summary`. For `PrepareFailure::Command`, the window shows that `String` as the disk's reason. `failure_text` does not cover that variant.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib job:: -- --test-threads=1`

Expected: PASS

- [ ] **Step 5: Commit**

```powershell
git add src/lib.rs src/job.rs
git commit -m @"
feat: run prepare then concurrent copy and verify

"@
```

---

### Task 9: Windows host and iced window

**Files:**
- Create: `src/windows_host.rs`, `src/app.rs`
- Modify: `src/main.rs`, `src/lib.rs`, `Cargo.toml` (iced `0.14`, rfd, `windows` with the features needed for `IsUserAnAdmin` and `ShellExecuteW`)
- Test: `src/app.rs` for status strings only. Host behavior is the manual checklist.

**Interfaces:**
- Consumes: every public type from Tasks 1–8
- Produces: a `MultiUSB` binary. `pub enum DriveStatus { Waiting, Formatting, Ready, Failed, Copying, Verifying, Passed }` with `fn label(self) -> &'static str` returning those words. `WindowsHost` implements `VolumeHost`. `pub fn is_elevated() -> bool`. `pub fn relaunch_elevated(exe: &Path) -> std::io::Result<()>`.

- [ ] **Step 1: Write the failing test**

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib app::status_labels -- --exact`

Expected: FAIL to compile because `DriveStatus` is missing.

- [ ] **Step 3: Implement the host and the window**

`WindowsHost::list_all` runs PowerShell `Get-CimInstance Win32_DiskDrive` and emits the JSON shape `parse_all_disk_json` accepts, including letters and `ContainsSystem` when the disk holds the Windows system partition. `diskpart` writes the script to a temp file, runs `diskpart /s`, uses `run_with_retries` with `DISKPART_ATTEMPTS` and `DISKPART_WAIT`, and waits on the child. Do not call `Child::kill`. Delete the temp script after the process exits. `is_elevated` uses `IsUserAnAdmin`. `relaunch_elevated` uses `ShellExecuteW` with verb `runas`.

The window, top to bottom: elevation notice and relaunch button only when not elevated; source path and Browse (`rfd` folder picker); filesystem dropdown placeholder `Select filesystem`; partition dropdown placeholder `Select partition type`; label text box, empty at launch, and the character limit only after a filesystem is selected; removable checklist with Refresh. Hold `Vec<EnumeratedDisk>` index-aligned with `BatchUi.rows`. Show a row only when `media_type` contains `Removable`. Each row shows disk number, model, size, and letters. None are checked. Start is disabled per `start_enabled`. Start while unelevated does not open confirmation. Start while elevated shows `confirmation_body` in the same window. Confirm runs `prepare_all` then `copy_and_verify_all` on a worker thread. `PrepareNotice` and `on_bytes` update per-drive bars. Map each target `dest` to `{letter}:\`. Dismiss returns to the form with choices intact. During Preparing and Copying, disable the form and hide Start. There is no cancel control. Per-drive status uses `DriveStatus::label`. Overall prepare text is `{done} of {total} disks prepared`. Overall copy bar uses `overall_fraction`, excluding disks that failed prepare. A system-disk abort shows `Stopped before any wipe. Disk {number} ({model}) contains the Windows system volume.` and does not start a copy. When the run finishes, show one result line per selected disk and `Run another batch`, which calls `run_another_batch`, refreshes the removable list, and does not start a wipe. Write the same lines with `write_run_log` using `std::env::current_dir` and a stamp from chrono `%Y-%m-%d_%H-%M-%S`. Closing the window while `diskpart` is running waits for that call to return. Follow iced 0.14's `application` entry point. Boot `App` from `initial_form()`.

- [ ] **Step 4: Run tests and build**

Run: `cargo test --lib`

Expected: PASS

Run: `cargo build`

Expected: exit code 0, binary `target\debug\MultiUSB.exe`

- [ ] **Step 5: Commit**

```powershell
git add Cargo.toml src/lib.rs src/main.rs src/app.rs src/windows_host.rs
git commit -m @"
feat: add the MultiUSB window

"@
```

Manual checklist for a person with two spare removable sticks. Do not run this unattended:

- Both sticks receive the same small folder and pass verification.
- A stick left unchecked is unchanged.
- Removing one copied file and re-running verification logic on that stick fails only that stick.
- FAT32 on a stick larger than 32 GB fails that stick and names Large FAT32.
- Run another batch keeps the folder, filesystem, partition style, and label, and does not wipe until the user checks disks and confirms again.
- A log file appears under `Logs` in the directory from which the program was started.

# MultiUSB GUI

Date: 2026-11-09

## Intent

Technicians use `createmultiusbMCDNSOUSB.cmd` to build several copies of a USB at once. That script wipes every disk Windows marks as removable, forces GPT and the label `MCD-NSO-USB`, copies `USBContents` with robocopy, and treats a robocopy exit code below 8 as success. It can destroy the wrong stick, and it never checks that each file arrived.

MultiUSB is a small Windows desktop app that does the same job with the choices made explicit: which folder, which filesystem, which partition style, which label, and which removable disks. Copies run at the same time. When a copy finishes, the app checks every file's path and size on that stick.

## Goals

- Pick a source folder and copy its contents onto one or more removable disks.
- Choose a filesystem (FAT32, NTFS, exFAT, or Large FAT32), a partition style (MBR or GPT), and a volume label. None of those three is pre-filled.
- Show only disks Windows reports as removable, and wipe only the ones the user checks.
- Wipe the whole selected disk: remove the partition table, create one new partition, quick-format it, then copy.
- Copy to every successfully prepared drive at the same time, with a progress bar per drive and one overall bar.
- After each copy, verify every source file exists on that drive at the same relative path and the same size.
- Keep going when one drive fails. Finish with a pass or fail result for each drive.
- After a run finishes, offer another batch that keeps the source folder, filesystem, partition style, and volume label.

## Non-goals

- An "update the stick" action that copies and verifies without wiping. That comes later. This version only does the full wipe-and-copy job.
- Hashing file contents.
- Copying drives one at a time.
- Wiping disks in parallel.
- Choosing drive letters. Windows assigns the letter after format.
- Reserving letters J through T the way the script does.
- Closing other programs, killing command prompts, or reading a network share.
- Saving the source folder, label, or options to disk. The next launch starts blank. Inheritance happens only through "Run another batch" in the same session.
- Installer, auto-update, or code signing.

## Anti-goals

- A drive list that includes fixed or system disks.
- Drives checked by default.
- A wipe that starts without a second confirmation listing the exact disks.
- A diskpart script built from an unsanitized label.
- Following directory junctions or symlinks out of the source folder.

## Constraints

- Windows only. Wiping uses `diskpart`. Drive listing uses WMI (`Win32_DiskDrive` and related association classes).
- The process must be elevated. `diskpart clean` fails otherwise.
- UI toolkit is iced (current stable crate). The window stays responsive while format, copy, and verify run off the UI thread.
- Workspace is `C:\MultiUSB`. This is a new Rust project.
- Windows `diskpart` and `format` refuse FAT32 on a volume larger than 32 GB. Large FAT32 is a separate choice and always goes through `fat32format.exe`. That program is not in this repo. The app looks for it next to `MultiUSB.exe`. The batch file's copy lives at `C:\UKUT USB\fat32format.exe` and can be placed beside the app.
- The source tree beside the batch file is large (provisioning files, scripts, binaries). Copy progress has to be byte-based so a long copy does not look frozen.

## Approaches

1. **egui.** One compact window, little structure, a single executable. Fastest way to the first form. A later update mode would be another block of immediate-mode code with no separation between screens.
2. **iced.** The same form, with application state and messages separated from the start. More code before the first window runs. The later update mode can be a second path that reuses copy and verify without sharing wipe state by accident.
3. **A web view (Tauri or similar).** Easier to restyle later. Extra runtime for a short admin form that has to run `diskpart`. Set aside.

iced is the choice. Concurrent copies were chosen with it: the batch file already starts one copy process per stick, and a one-at-a-time copy was rejected.

## Decisions

### What the user controls

The window has these fields:

- Source folder, with a browse button. Required. The copy places the *contents* of this folder at the root of each stick, matching `robocopy source dest /E`. Blank at first launch.
- Filesystem, a dropdown with no selection at first launch. The entries are FAT32, NTFS, exFAT, and Large FAT32.
- Partition style, a dropdown with no selection at first launch. The entries are MBR and GPT.
- Volume label, a text box. Blank at first launch. Empty is a valid label. Text that is present must be valid for the chosen filesystem.
- A checklist of removable disks. Nothing is checked when the list loads or refreshes.

A Refresh button re-reads removable disks. Start stays disabled until the source folder is set, a filesystem is selected, a partition style is selected, and at least one disk is checked.

Each row shows disk number, model, size, and any current drive letters, so the stick can be recognized before it is wiped.

### Confirmation and elevation

Start does not wipe anything. It opens a confirmation that lists every checked disk (number, model, size) plus the filesystem, partition style, and label. The confirming button is the only way to begin. Dismissing the confirmation returns to the form with the choices intact.

If the process is not elevated, Start does not run the job. The window says administrator rights are required and offers a button that relaunches the same executable elevated. The unelevated process then exits.

### Which disks may be wiped

A disk appears in the list only when `Win32_DiskDrive.MediaType` contains `Removable`.

Immediately before each `diskpart` run, resolve that disk again by PNP device id (fall back to serial number plus size when PNP id is missing). Use the disk number from that fresh lookup. If the disk cannot be matched uniquely, or it is no longer removable, skip it and mark it failed. Do not fall back to the number captured when the user clicked Start.

If a matched disk contains the Windows system volume, abort the entire run before any `clean`. Do not wipe the remaining selected disks.

### Wipe and format

Prepare disks one at a time. `diskpart` against several disks at once is how drives get missed or half-written. Copies start only after every selected disk has either a new letter or a failure.

For each disk, write a temporary diskpart script and run `diskpart /s` up to three times, with a few seconds between tries, matching the batch file's retry loop:

```
rescan
select disk <n>
clean
convert <gpt|mbr>
create partition primary
format fs=<ntfs|fat32|exfat> quick label="<label>"
assign
exit
```

Use `convert gpt` or `convert mbr` from the user's choice. Quick format only. A full format of several sticks is out of scope for this version.

After `diskpart` exits 0, find the new volume's drive letter by the same disk identity. If no letter appears, that disk fails.

Filesystem behavior:

- **NTFS.** `format fs=ntfs` in the script above. Label limit 32 characters.
- **exFAT.** `format fs=exfat` in the script above. Label limit 15 characters.
- **FAT32.** `format fs=fat32` in the script above. Label limit 11 characters. If the disk is larger than 32 GB, do not call `diskpart`. Fail that disk and tell the user to choose Large FAT32. Other disks continue.
- **Large FAT32.** Always, at any size: `format fs=ntfs quick` and `assign` in diskpart, then run `fat32format.exe <letter>:` with the affirmative answer piped in (the batch file uses `echo y`), then set the label with the `label` command. Label limit 11 characters. Look for `fat32format.exe` next to `MultiUSB.exe` only. If it is missing, fail that disk with a message that names the missing program. Other disks continue. Do not copy `fat32format.exe` into this repo.

Label text written into the diskpart script may contain only letters, digits, space, hyphen, and underscore. Reject anything else before the job starts. Show the character limit beside the field once a filesystem is selected, and refuse Start when the label is too long for that filesystem.

### Copy

Implement the copy in Rust so each drive can report bytes copied. Do not shell out to robocopy.

Walk the source once before copying. Record every regular file's relative path and size, and every directory's relative path. This file list is the work list for every drive and the list verification checks.

Copy behavior, aligned with `robocopy /E` without `/MIR`:

- Create subdirectories, including empty ones.
- Copy regular files. Overwrite a destination file when one already exists.
- Do not delete destination files that are absent from the source. A fresh format makes this a no-op, and the later update mode needs copy-not-mirror.
- Do not follow directory junctions or symlinks. Skip them and record a line in the log. Do not walk out of the source tree.
- Copy hidden and system files.
- Use extended-length paths so deep provisioning trees do not fail at `MAX_PATH`.
- On a file that fails, retry that file 3 times, waiting 10 seconds between tries. Then mark the drive failed and stop copying to that drive. Other drives continue.

Run one copy per successfully formatted drive, all at the same time. No extra concurrency cap.

Preserve last-write time when the standard library copy does so on Windows. Ownership and ACLs are not part of the check and do not need to be forced.

### Verification

When a drive's copy finishes, verify it while other copies keep running.

For every regular file in the precomputed list, the destination file must exist at the same relative path and have the same length. Compare paths case-insensitively, because FAT32, Large FAT32, and exFAT may not preserve case.

A drive passes only when every listed file matches. The result shows files checked, files missing, and files whose size differed. Name the first mismatches in the log (cap the listed names at 20, then give the remainder as a count).

Directory existence is not part of the pass/fail check. Creating directories is still part of the copy.

A copy error fails the drive even if some files would have matched. Do not verify a drive whose copy was aborted.

### Progress and results

While a disk is being prepared, its row shows waiting, formatting, ready, or failed. The overall line shows how many selected disks are prepared.

During copy and verify, each drive has its own progress bar: bytes copied divided by source size, then a verifying state, then passed or failed. The overall bar is total bytes copied across all drives divided by source size times the number of drives that reached the copy stage. A drive that failed formatting does not count in that denominator.

The form fields and the drive checklist are disabled for the duration of the run. There is no cancel button. Closing the window during a `diskpart` run waits for that `diskpart` process to exit, then stops the job. Copies already running stop at the next file boundary. The process does not kill `diskpart`.

When every drive is finished, the window lists one line per selected disk: passed, or failed with the reason (format, copy, or verification counts). Show a "Run another batch" button. It keeps the source folder, filesystem, partition style, and volume label. It clears the drive checks, refreshes the removable list, and clears the on-screen progress and log. It does not start a wipe. The user picks disks and confirms again. The button is available when the run has finished, including when some drives failed.

Write the same lines to a log file in a `Logs` folder under the program's current working directory, one file per run, named with the local date and time. Create `Logs` if it is missing.

### Later update mode

No control for it in this version. Copy and verify must be callable without wipe and format, so a later action can point them at an existing letter. Do not build that action, its picker, or its confirmation now.

## Window

Top to bottom:

1. Elevation notice, only when not administrator, plus the relaunch button.
2. Source folder field and Browse.
3. Filesystem dropdown (no initial selection; FAT32, NTFS, exFAT, Large FAT32) and partition-style dropdown (no initial selection; MBR, GPT).
4. Volume label text box, empty until the user types, with the character limit once a filesystem is selected.
5. Removable-drive checklist and Refresh.
6. Start.
7. Confirmation content replaces the start action until the user confirms or dismisses it. It is not a separate window.
8. Per-drive status and progress, then the overall bar.
9. Scrolling log.
10. "Run another batch", shown once the run has finished.

Use iced's default theme and a single window titled MultiUSB. Keep the layout one column. No menus.

## Errors the user can see

- Source folder missing or not a directory.
- Filesystem or partition style still unselected.
- Label contains a character outside the allowed set, or is longer than the filesystem allows.
- No removable disks, after a successful scan.
- Not elevated, when they press Start.
- FAT32 chosen for a disk larger than 32 GB.
- `fat32format.exe` missing for a Large FAT32 disk.
- `Logs` could not be created in the current working directory.
- Disk disappeared, changed identity, or is no longer removable.
- System disk was selected. The run stops before any wipe.
- `diskpart` failed after three tries.
- No drive letter after format.
- A file failed after three copy tries.
- Verification found missing files or different sizes.

Each of these names the disk when a disk is involved. A failure on one disk does not by itself fail the others, except the system-disk abort above.

## What is left to the builder

- iced version pin, widget spacing, fonts, and the exact message enum.
- How elevation is detected and how the relaunch passes the current executable path.
- The WMI query details and the mapping from disk identity to a new drive letter.
- Temporary directory for diskpart scripts, deleted after each run.
- Unit-test layout.

## Tests

Automated tests do not touch real disks and do not run `diskpart`.

- Diskpart script text for NTFS+GPT, NTFS+MBR, exFAT, FAT32 at or under 32 GB, and the NTFS-then-handoff shape used before `fat32format.exe` for Large FAT32.
- FAT32 on a disk larger than 32 GB is rejected before `diskpart`. Large FAT32 uses `fat32format.exe` at any size.
- Label acceptance and rejection, including a quote, a newline, a 15-character label accepted for exFAT and rejected for FAT32, and a label that is legal for NTFS but too long for exFAT.
- "Run another batch" keeps source folder, filesystem, partition style, and label, and clears the drive checks.
- Copy of a tree with an empty directory, a nested file, and a junction or symlink that must not be followed.
- Verification success, a missing file, a size mismatch, and case-insensitive path match.
- Progress totals: two drives, one source size, overall fraction uses both.

Manual check, once, with two removable sticks the tester can afford to erase: both receive the same small folder, both verify, a stick left unchecked is unchanged, and a forced missing file on one stick fails only that stick.

## Project shape

A single binary crate. Suggested modules: app state and view, disk enumeration and wipe, copy, verify. The copy and verify modules must not call diskpart or import the wipe code.

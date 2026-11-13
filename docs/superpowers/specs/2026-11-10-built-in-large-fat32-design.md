# Built-in FAT32 (>32 GiB) Design

**Goal.** Format FAT32 on disks larger than Windows' diskpart limit inside MultiUSB with no `fat32format.exe` sidecar, and without a separate dropdown choice.

**UI.** Filesystem dropdown is FAT32, NTFS, exFAT. Choosing FAT32 is enough; size decides the path.

**Approach.** Keep diskpart for wipe/partition/assign. For FAT32 ≤32 GiB, diskpart also quick-formats. For FAT32 >32 GiB, diskpart assigns a letter with no `format` line; then MultiUSB formats in-process via MIT `fatfs::format_volume` (`FatType::Fat32` + padded 11-byte volume label).

**OS formatters first.** Try `format.com /FS:FAT32 /Q` then PowerShell `Format-Volume`. On current Windows both still reject ~58 GiB FAT32 (`too big` / StorageWMI `Size Not Supported`).

**Custom format.** Built-in burst writer in `format_fat32.rs` (whole-sector sequential writes, no `fatfs` RMW). Prefer `PhysicalDrive` at the partition offset with no volume lock; then volume lock-only; then lock+dismount; then physical while volume locked. Failures write `Logs/fat32-trace-*.log`.

**Host API.** `VolumeHost::format_large_fat32(disk_number, letter, label) -> Result<(), String>`. Windows host owns lock/dismount/physical/`fatfs`/unlock. Fakes record the call for unit tests.

**Removals.** `fat32format_beside_exe`, `fat32format_command`, `PrepareFailure::MissingFat32Format`, `PrepareFailure::Fat32TooLarge`, the Large FAT32 dropdown entry, and docs that tell users to ship the helper.

**Out of scope.** Copy/verify. Shipping any helper binary.

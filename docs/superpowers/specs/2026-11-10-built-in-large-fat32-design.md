# Built-in FAT32 (>32 GiB) Design

**Goal.** Format FAT32 on disks larger than Windows' diskpart limit inside MultiUSB with no `fat32format.exe` sidecar, and without a separate dropdown choice.

**UI.** Filesystem dropdown is FAT32, NTFS, exFAT. Choosing FAT32 is enough; size decides the path.

**Approach.** Keep diskpart for wipe/partition/assign. For FAT32 ≤32 GiB, diskpart also quick-formats. For FAT32 >32 GiB, diskpart assigns a letter with no `format` line; then MultiUSB formats in-process via MIT `fatfs::format_volume` (`FatType::Fat32` + padded 11-byte volume label).

**Volume access (Rufus-inspired).** Open the letter with exclusive write preferred (`FILE_SHARE_READ` only; escalate to share-write after retries), call `FSCTL_ALLOW_EXTENDED_DASD_IO`, retry `FSCTL_LOCK_VOLUME` for up to 15 s, then `FSCTL_DISMOUNT_VOLUME` (non-fatal if it fails). Writes go through `\\.\PhysicalDriveN` at the partition offset while that volume lock is held — post-diskpart writes to `\\.\X:` were unreliable (os error 433). Unlock when done. No separate `label` command on this path.

**Host API.** `VolumeHost::format_large_fat32(disk_number, letter, label) -> Result<(), String>`. Windows host owns lock/dismount/physical/`fatfs`/unlock. Fakes record the call for unit tests.

**Removals.** `fat32format_beside_exe`, `fat32format_command`, `PrepareFailure::MissingFat32Format`, `PrepareFailure::Fat32TooLarge`, the Large FAT32 dropdown entry, and docs that tell users to ship the helper.

**Out of scope.** Copy/verify. Shipping any helper binary.

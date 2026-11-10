# Built-in Large FAT32 Design

**Goal.** Format Large FAT32 inside MultiUSB with no `fat32format.exe` sidecar.

**Approach.** Keep diskpart for wipe/partition/assign. Drop the NTFS handoff format line. After a drive letter appears, open the raw volume, lock it, and call MIT `fatfs::format_volume` with `FatType::Fat32` and the padded 11-byte volume label. No separate `label` command on this path.

**Host API.** `VolumeHost::format_large_fat32(letter: char, label: &str) -> Result<(), String>`. Windows host owns open/lock/`fatfs`/unlock. Fakes record the call for unit tests.

**Removals.** `fat32format_beside_exe`, `fat32format_command`, `PrepareFailure::MissingFat32Format`, and docs that tell users to ship the helper.

**Out of scope.** Regular ≤32 GiB FAT32 (still diskpart). Copy/verify. Shipping any helper binary.

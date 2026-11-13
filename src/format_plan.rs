use crate::label::{Filesystem, PartitionStyle};

pub const FAT32_MAX_INCLUSIVE_BYTES: u64 = 32 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreparePlan {
    Script(String),
    /// FAT32 above Windows' diskpart limit: wipe/partition only (no letter), then
    /// in-process format and a single letter assign afterward.
    BuiltInFat32 { script: String },
}

fn convert_token(style: PartitionStyle) -> &'static str {
    match style {
        PartitionStyle::Gpt => "gpt",
        PartitionStyle::Mbr => "mbr",
    }
}

fn disk_prep_prefix(disk_number: u32) -> String {
    // online + clear readonly: USB sticks left offline/RO after failed formats
    // otherwise fail `clean`. `noerr` is required — `online disk` returns a
    // Virtual Disk Service error when already online. Re-select after `clean`:
    // cleaning clears the current disk selection on this host.
    format!(
        "rescan\r\nselect disk {disk_number}\r\nonline disk noerr\r\nattributes disk clear readonly noerr\r\nclean\r\nselect disk {disk_number}\r\n"
    )
}

fn build_script(disk_number: u32, style: PartitionStyle, format_line: &str) -> String {
    let convert = convert_token(style);
    format!(
        "{}convert {convert}\r\ncreate partition primary\r\n{format_line}\r\nassign\r\nexit\r\n",
        disk_prep_prefix(disk_number)
    )
}

fn build_assign_only_script(disk_number: u32, style: PartitionStyle) -> String {
    let convert = convert_token(style);
    // MBR: create as FAT32 LBA (0x0C). Do not assign a letter yet — mounting a
    // RAW partition makes Explorer AutoPlay "There's a problem with this drive"
    // and thrash the volume while we format.
    let create = match style {
        PartitionStyle::Mbr => "create partition primary id=0c",
        PartitionStyle::Gpt => "create partition primary",
    };
    format!(
        "{}convert {convert}\r\n{create}\r\nexit\r\n",
        disk_prep_prefix(disk_number)
    )
}

pub fn prepare_plan(
    disk_number: u32,
    style: PartitionStyle,
    filesystem: Filesystem,
    label: &str,
    size_bytes: u64,
) -> PreparePlan {
    if matches!(filesystem, Filesystem::Fat32) && size_bytes > FAT32_MAX_INCLUSIVE_BYTES {
        return PreparePlan::BuiltInFat32 {
            script: build_assign_only_script(disk_number, style),
        };
    }
    let format_line = format_line_for(filesystem, label);
    PreparePlan::Script(build_script(disk_number, style, &format_line))
}

fn format_line_for(filesystem: Filesystem, label: &str) -> String {
    let fs = match filesystem {
        Filesystem::Ntfs => "ntfs",
        Filesystem::ExFat => "exfat",
        Filesystem::Fat32 => "fat32",
    };
    format!("format fs={fs} quick label=\"{label}\"")
}

#[cfg(test)]
mod tests {
    use super::{prepare_plan, FAT32_MAX_INCLUSIVE_BYTES, PreparePlan};
    use crate::label::{Filesystem, PartitionStyle};

    fn script(disk: u32, convert: &str, format_line: &str) -> String {
        format!(
            "rescan\r\nselect disk {disk}\r\nonline disk noerr\r\nattributes disk clear readonly noerr\r\nclean\r\nselect disk {disk}\r\nconvert {convert}\r\ncreate partition primary\r\n{format_line}\r\nassign\r\nexit\r\n"
        )
    }

    fn assign_only_script(disk: u32, convert: &str) -> String {
        let create = if convert == "mbr" {
            "create partition primary id=0c"
        } else {
            "create partition primary"
        };
        format!(
            "rescan\r\nselect disk {disk}\r\nonline disk noerr\r\nattributes disk clear readonly noerr\r\nclean\r\nselect disk {disk}\r\nconvert {convert}\r\n{create}\r\nexit\r\n"
        )
    }

    #[test]
    fn ntfs_gpt_and_mbr_and_exfat_scripts() {
        assert_eq!(
            prepare_plan(2, PartitionStyle::Gpt, Filesystem::Ntfs, "MCD-NSO-USB", 100),
            PreparePlan::Script(script(
                2,
                "gpt",
                "format fs=ntfs quick label=\"MCD-NSO-USB\"",
            ))
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
    fn fat32_uses_diskpart_at_or_under_limit_and_builtin_above() {
        let limit = FAT32_MAX_INCLUSIVE_BYTES;
        assert_eq!(
            prepare_plan(2, PartitionStyle::Gpt, Filesystem::Fat32, "LAB", limit),
            PreparePlan::Script(script(2, "gpt", "format fs=fat32 quick label=\"LAB\""))
        );
        assert_eq!(
            prepare_plan(2, PartitionStyle::Gpt, Filesystem::Fat32, "LAB", limit + 1),
            PreparePlan::BuiltInFat32 {
                script: assign_only_script(2, "gpt")
            }
        );
        assert_eq!(
            prepare_plan(2, PartitionStyle::Mbr, Filesystem::Fat32, "LAB", limit + 1),
            PreparePlan::BuiltInFat32 {
                script: assign_only_script(2, "mbr")
            }
        );
    }
}

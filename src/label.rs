#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filesystem {
    Fat32,
    Ntfs,
    ExFat,
    LargeFat32,
}

impl Filesystem {
    pub const ALL: [Filesystem; 4] = [
        Filesystem::Fat32,
        Filesystem::Ntfs,
        Filesystem::ExFat,
        Filesystem::LargeFat32,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Filesystem::Fat32 => "FAT32",
            Filesystem::Ntfs => "NTFS",
            Filesystem::ExFat => "exFAT",
            Filesystem::LargeFat32 => "Large FAT32",
        }
    }

    pub fn label_limit(self) -> usize {
        match self {
            Filesystem::Fat32 | Filesystem::LargeFat32 => 11,
            Filesystem::ExFat => 15,
            Filesystem::Ntfs => 32,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionStyle {
    Mbr,
    Gpt,
}

impl PartitionStyle {
    pub const ALL: [PartitionStyle; 2] = [PartitionStyle::Mbr, PartitionStyle::Gpt];

    pub fn as_str(self) -> &'static str {
        match self {
            PartitionStyle::Mbr => "MBR",
            PartitionStyle::Gpt => "GPT",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelError {
    Character { ch: char },
    TooLong { len: usize, limit: usize },
}

fn is_allowed_label_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == ' ' || ch == '-' || ch == '_'
}

pub fn validate_label(filesystem: Filesystem, label: &str) -> Result<(), LabelError> {
    if label.chars().count() > filesystem.label_limit() {
        return Err(LabelError::TooLong {
            len: label.chars().count(),
            limit: filesystem.label_limit(),
        });
    }
    for ch in label.chars() {
        if !is_allowed_label_char(ch) {
            return Err(LabelError::Character { ch });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Filesystem, LabelError, PartitionStyle, validate_label};

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
}

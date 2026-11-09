use crate::long_path::extended_path;
use crate::source::{EntryKind, SourceEntry};
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub checked: usize,
    pub missing: Vec<PathBuf>,
    pub size_mismatch: Vec<PathBuf>,
}

impl VerifyReport {
    pub fn passed(&self) -> bool {
        self.missing.is_empty() && self.size_mismatch.is_empty()
    }
}

pub fn verify_tree(dest: &Path, entries: &[SourceEntry]) -> VerifyReport {
    let mut report = VerifyReport {
        checked: 0,
        missing: Vec::new(),
        size_mismatch: Vec::new(),
    };

    for entry in entries {
        if entry.kind != EntryKind::File {
            continue;
        }
        report.checked += 1;
        let Some(resolved) = resolve_case_insensitive(dest, &entry.relative) else {
            report.missing.push(entry.relative.clone());
            continue;
        };
        let resolved = extended_path(&resolved);
        let Ok(meta) = fs::symlink_metadata(&resolved) else {
            report.missing.push(entry.relative.clone());
            continue;
        };
        if !meta.is_file() {
            report.missing.push(entry.relative.clone());
            continue;
        }
        let Ok(meta) = fs::metadata(&resolved) else {
            report.missing.push(entry.relative.clone());
            continue;
        };
        if meta.len() != entry.size {
            report.size_mismatch.push(entry.relative.clone());
        }
    }

    report
}

fn resolve_case_insensitive(base: &Path, relative: &Path) -> Option<PathBuf> {
    let mut current = extended_path(base);
    for component in relative.components() {
        match component {
            Component::Normal(name) => {
                let name = name.to_string_lossy();
                let mut next = None;
                let Ok(entries) = fs::read_dir(extended_path(&current)) else {
                    return None;
                };
                for entry in entries.flatten() {
                    let entry_name = entry.file_name().to_string_lossy().to_string();
                    if entry_name.eq_ignore_ascii_case(name.as_ref()) {
                        next = Some(extended_path(&entry.path()));
                        break;
                    }
                }
                current = next?;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                current = current.parent()?.to_path_buf();
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(current)
}

pub fn mismatch_summary(report: &VerifyReport) -> String {
    let mut paths: Vec<&PathBuf> = report
        .missing
        .iter()
        .chain(report.size_mismatch.iter())
        .collect();
    let total = paths.len();
    let mut lines = Vec::new();
    for path in paths.drain(..total.min(20)) {
        let label = if report.missing.iter().any(|p| p == path) {
            "missing"
        } else {
            "size mismatch"
        };
        lines.push(format!("{label}: {}", path.display()));
    }
    let mut out = lines.join("\n");
    if total > 20 {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("and {} more", total - 20));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{VerifyReport, mismatch_summary, verify_tree};
    use crate::source::{EntryKind, SourceEntry, walk_source};
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn verify_is_case_insensitive() {
        let base = std::env::temp_dir().join(format!("multiusb_verify_ci_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir(&base).unwrap();
        fs::write(base.join("File.TXT"), b"same").unwrap();

        let entries = vec![SourceEntry {
            relative: PathBuf::from("file.txt"),
            kind: EntryKind::File,
            size: 4,
        }];
        let report = verify_tree(&base, &entries);
        let _ = fs::remove_dir_all(&base);

        assert!(report.passed());
        assert_eq!(report.checked, 1);
    }

    #[test]
    fn verify_reports_missing_and_size() {
        let base = std::env::temp_dir().join(format!("multiusb_verify_err_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir(&base).unwrap();
        fs::write(base.join("short.bin"), b"ab").unwrap();

        let entries = vec![
            SourceEntry {
                relative: PathBuf::from("gone.txt"),
                kind: EntryKind::File,
                size: 3,
            },
            SourceEntry {
                relative: PathBuf::from("short.bin"),
                kind: EntryKind::File,
                size: 10,
            },
            SourceEntry {
                relative: PathBuf::from("adir"),
                kind: EntryKind::Directory,
                size: 0,
            },
        ];
        let report = verify_tree(&base, &entries);
        let _ = fs::remove_dir_all(&base);

        assert!(!report.passed());
        assert_eq!(report.missing, vec![PathBuf::from("gone.txt")]);
        assert_eq!(report.size_mismatch, vec![PathBuf::from("short.bin")]);
    }

    #[test]
    fn walk_and_verify_path_longer_than_max_path() {
        let root = std::env::temp_dir().join(format!("multiusb_long_{}", std::process::id()));
        let extended_root = PathBuf::from(format!(r"\\?\{}", root.display()));
        let _ = fs::remove_dir_all(&extended_root);
        fs::create_dir_all(&extended_root).unwrap();

        let segment = "n".repeat(40);
        let mut relative = PathBuf::new();
        let mut full = root.clone();
        let file_relative = loop {
            relative.push(&segment);
            full.push(&segment);
            let extended_dir = PathBuf::from(format!(r"\\?\{}", full.display()));
            fs::create_dir_all(&extended_dir).unwrap();
            if full.to_string_lossy().len() > 260 {
                let file_full = full.join("payload.bin");
                assert!(file_full.to_string_lossy().len() > 260);
                let extended_file = PathBuf::from(format!(r"\\?\{}", file_full.display()));
                fs::write(&extended_file, b"long").unwrap();
                break relative.join("payload.bin");
            }
        };

        let walk = walk_source(&extended_root).expect("walk a path longer than MAX_PATH");
        let report = verify_tree(&extended_root, &walk.entries);
        let plain = walk_source(&root).expect("walk the same tree from its ordinary path");
        let plain_report = verify_tree(&root, &plain.entries);
        let _ = fs::remove_dir_all(&extended_root);

        let file = walk
            .entries
            .iter()
            .find(|entry| entry.relative == file_relative)
            .expect("long file is in the walk");
        assert_eq!(file.kind, EntryKind::File);
        assert_eq!(file.size, 4);
        assert!(report.passed(), "{report:?}");
        assert!(plain_report.passed(), "{plain_report:?}");
        assert!(
            plain
                .entries
                .iter()
                .any(|entry| entry.relative == file_relative)
        );
    }

    #[test]
    fn mismatch_summary_caps_at_20() {
        let missing: Vec<PathBuf> = (0..25)
            .map(|i| PathBuf::from(format!("missing{i}.txt")))
            .collect();
        let report = VerifyReport {
            checked: 25,
            missing,
            size_mismatch: Vec::new(),
        };
        let summary = mismatch_summary(&report);
        let path_lines: Vec<_> = summary
            .lines()
            .filter(|line| line.contains("missing") && line.ends_with(".txt"))
            .collect();
        assert_eq!(path_lines.len(), 20);
        assert!(summary.contains("and 5 more"));
    }
}

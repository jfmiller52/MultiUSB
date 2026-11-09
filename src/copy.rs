use crate::source::{EntryKind, SourceEntry};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    pub attempts: u32,
    pub wait: Duration,
}

pub const COPY_RETRY: RetryPolicy = RetryPolicy {
    attempts: 3,
    wait: Duration::from_secs(10),
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyError {
    File { path: PathBuf, attempts: u32 },
}

pub fn extended_path(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with(r"\\?\") {
        return path.to_path_buf();
    }
    if s.starts_with(r"\\") {
        return PathBuf::from(format!(r"\\?\UNC\{}", &s[2..]));
    }
    PathBuf::from(format!(r"\\?\{}", s))
}

pub fn copy_tree(
    source_root: &Path,
    dest_root: &Path,
    entries: &[SourceEntry],
    policy: RetryPolicy,
    sleep: &dyn Fn(Duration),
    on_bytes: &dyn Fn(u64),
) -> Result<u64, CopyError> {
    let mut total = 0u64;
    for entry in entries {
        match entry.kind {
            EntryKind::Directory => {
                let dest = dest_root.join(&entry.relative);
                let _ = std::fs::create_dir_all(extended_path(&dest));
            }
            EntryKind::File => {
                let src = source_root.join(&entry.relative);
                let dest = dest_root.join(&entry.relative);
                if let Some(parent) = dest.parent() {
                    let _ = std::fs::create_dir_all(extended_path(parent));
                }
                let mut copied = false;
                for attempt in 1..=policy.attempts {
                    match std::fs::copy(extended_path(&src), extended_path(&dest)) {
                        Ok(bytes) => {
                            total += bytes;
                            on_bytes(bytes);
                            copied = true;
                            break;
                        }
                        Err(_) if attempt < policy.attempts => sleep(policy.wait),
                        Err(_) => {}
                    }
                }
                if !copied {
                    return Err(CopyError::File {
                        path: entry.relative.clone(),
                        attempts: policy.attempts,
                    });
                }
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::{copy_tree, extended_path, CopyError, RetryPolicy, COPY_RETRY};
    use crate::source::{walk_source, EntryKind, SourceEntry};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::cell::Cell;
    use std::sync::Mutex;
    use std::time::Duration;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("multiusb_copy_{name}_{}", std::process::id()))
    }

    #[test]
    fn extended_path_prefixes_drive_and_unc() {
        assert_eq!(
            extended_path(Path::new(r"C:\foo")),
            PathBuf::from(r"\\?\C:\foo")
        );
        assert_eq!(
            extended_path(Path::new(r"\\server\share\a")),
            PathBuf::from(r"\\?\UNC\server\share\a")
        );
    }

    #[test]
    fn copy_retry_attempts_is_three() {
        assert_eq!(COPY_RETRY.attempts, 3);
    }

    #[test]
    fn copy_tree_empty_returns_zero() {
        let dest = tmp("empty_dest");
        let _ = fs::remove_dir_all(&dest);
        fs::create_dir_all(&dest).unwrap();
        let total = copy_tree(
            Path::new("."),
            &dest,
            &[],
            COPY_RETRY,
            &|_| {},
            &|_| {},
        )
        .unwrap();
        let _ = fs::remove_dir_all(&dest);
        assert_eq!(total, 0);
    }

    #[test]
    fn copy_tree_creates_empty_dir_and_reports_bytes() {
        let src = tmp("src_tree");
        let dest = tmp("dest_tree");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dest);
        fs::create_dir_all(src.join("empty")).unwrap();
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("nested").join("a.txt"), b"hello").unwrap();
        let walk = walk_source(&src).unwrap();
        let bytes_reported = Cell::new(0u64);
        let total = copy_tree(
            &src,
            &dest,
            &walk.entries,
            COPY_RETRY,
            &|_| {},
            &|n| bytes_reported.set(bytes_reported.get() + n),
        )
        .unwrap();
        let _ = fs::remove_dir_all(&src);
        assert!(dest.join("empty").is_dir());
        assert_eq!(fs::read(dest.join("nested").join("a.txt")).unwrap(), b"hello");
        assert_eq!(total, 5);
        assert_eq!(bytes_reported.get(), 5);
        let _ = fs::remove_dir_all(&dest);
    }

    #[test]
    fn copy_tree_leaves_extra_dest_files() {
        let src = tmp("src_extra");
        let dest = tmp("dest_extra");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dest);
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(src.join("only.txt"), b"x").unwrap();
        fs::write(dest.join("extra.txt"), b"stay").unwrap();
        let walk = walk_source(&src).unwrap();
        copy_tree(
            &src,
            &dest,
            &walk.entries,
            COPY_RETRY,
            &|_| {},
            &|_| {},
        )
        .unwrap();
        assert_eq!(fs::read(dest.join("extra.txt")).unwrap(), b"stay");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dest);
    }

    #[test]
    fn copy_tree_overwrites_existing_dest_file() {
        let src = tmp("src_over");
        let dest = tmp("dest_over");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dest);
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(src.join("data.bin"), b"newbytes").unwrap();
        fs::write(dest.join("data.bin"), b"old").unwrap();
        let walk = walk_source(&src).unwrap();
        copy_tree(
            &src,
            &dest,
            &walk.entries,
            COPY_RETRY,
            &|_| {},
            &|_| {},
        )
        .unwrap();
        assert_eq!(fs::read(dest.join("data.bin")).unwrap(), b"newbytes");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dest);
    }

    #[test]
    fn copy_tree_copies_hidden_file() {
        let src = tmp("src_hidden");
        let dest = tmp("dest_hidden");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dest);
        fs::create_dir_all(&src).unwrap();
        let hidden = src.join("hidden.dat");
        fs::write(&hidden, b"secret").unwrap();
        let status = Command::new("cmd")
            .args(["/C", "attrib", "+H", &hidden.display().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        let walk = walk_source(&src).unwrap();
        copy_tree(
            &src,
            &dest,
            &walk.entries,
            COPY_RETRY,
            &|_| {},
            &|_| {},
        )
        .unwrap();
        assert_eq!(fs::read(dest.join("hidden.dat")).unwrap(), b"secret");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dest);
    }

    #[test]
    fn copy_tree_retries_then_returns_file_error() {
        let src = tmp("src_fail");
        let dest = tmp("dest_fail");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dest);
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(src.join("bad")).unwrap();
        fs::write(src.join("bad").join("file.txt"), b"data").unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("bad"), b"blocker").unwrap();
        let entries = vec![SourceEntry {
            relative: PathBuf::from(r"bad\file.txt"),
            kind: EntryKind::File,
            size: 4,
        }];
        let sleeps: Mutex<Vec<Duration>> = Mutex::new(Vec::new());
        let policy = RetryPolicy {
            attempts: 3,
            wait: Duration::from_secs(10),
        };
        let err = copy_tree(
            &src,
            &dest,
            &entries,
            policy,
            &|d| sleeps.lock().unwrap().push(d),
            &|_| {},
        )
        .unwrap_err();
        let recorded = sleeps.lock().unwrap().clone();
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dest);
        assert_eq!(
            recorded,
            vec![Duration::from_secs(10), Duration::from_secs(10)]
        );
        assert!(matches!(
            err,
            CopyError::File {
                attempts: 3,
                ..
            }
        ));
    }

    #[test]
    fn copy_and_verify_sources_exclude_foreign_modules() {
        const FORBIDDEN: &[&str] = &[
            concat!("disk", "part"),
            concat!("format", "_", "plan"),
            concat!("disk", "_", "id"),
            concat!("windows", "_", "host"),
            concat!("j", "ob"),
        ];
        let copy_src = include_str!("copy.rs");
        let verify_src = include_str!("verify.rs");
        for word in FORBIDDEN {
            assert!(
                !copy_src.contains(word),
                "copy.rs must not reference {word}"
            );
            assert!(
                !verify_src.contains(word),
                "verify.rs must not reference {word}"
            );
        }
    }
}

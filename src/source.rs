use crate::long_path::extended_path;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEntry {
    pub relative: PathBuf,
    pub kind: EntryKind,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walk {
    pub entries: Vec<SourceEntry>,
    pub skipped_links: Vec<PathBuf>,
}

pub fn walk_source(root: &Path) -> std::io::Result<Walk> {
    let mut walk = Walk {
        entries: Vec::new(),
        skipped_links: Vec::new(),
    };
    let root = extended_path(root);
    walk_directory(&root, &root, &mut walk)?;
    walk.entries.sort_by(|a, b| a.relative.cmp(&b.relative));
    walk.skipped_links.sort_by(|a, b| a.cmp(b));
    Ok(walk)
}

fn walk_directory(root: &Path, dir: &Path, walk: &mut Walk) -> std::io::Result<()> {
    let dir = extended_path(dir);
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = extended_path(&entry.path());
        let relative = path
            .strip_prefix(root)
            .map(Path::to_path_buf)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;

        let file_type = fs::symlink_metadata(&path)?.file_type();
        if file_type.is_symlink() {
            walk.skipped_links.push(relative);
            continue;
        }

        if file_type.is_dir() {
            walk.entries.push(SourceEntry {
                relative: relative.clone(),
                kind: EntryKind::Directory,
                size: 0,
            });
            walk_directory(root, &path, walk)?;
        } else if file_type.is_file() {
            let size = fs::metadata(&path)?.len();
            walk.entries.push(SourceEntry {
                relative,
                kind: EntryKind::File,
                size,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{EntryKind, SourceEntry, walk_source};
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    fn rel_paths(entries: &[SourceEntry]) -> Vec<PathBuf> {
        entries.iter().map(|e| e.relative.clone()).collect()
    }

    fn has_entry(entries: &[SourceEntry], relative: &str, kind: EntryKind) -> bool {
        entries
            .iter()
            .any(|e| e.relative == PathBuf::from(relative) && e.kind == kind)
    }

    #[test]
    fn walk_includes_empty_dir_nested_file_and_hidden_file() {
        let base = std::env::temp_dir().join(format!("multiusb_walk_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("empty")).unwrap();
        fs::create_dir_all(base.join("nested")).unwrap();
        fs::write(base.join("nested").join("a.txt"), b"hello").unwrap();
        let hidden = base.join("hidden.dat");
        fs::write(&hidden, b"x").unwrap();
        let status = Command::new("cmd")
            .args(["/C", "attrib", "+H", &hidden.display().to_string()])
            .status()
            .unwrap();
        assert!(status.success());

        let walk = walk_source(&base).unwrap();
        let _ = fs::remove_dir_all(&base);

        assert!(has_entry(&walk.entries, "empty", EntryKind::Directory));
        assert!(has_entry(&walk.entries, "nested", EntryKind::Directory));
        assert!(has_entry(&walk.entries, r"nested\a.txt", EntryKind::File));
        assert!(has_entry(&walk.entries, "hidden.dat", EntryKind::File));
        let a = walk
            .entries
            .iter()
            .find(|e| e.relative == PathBuf::from(r"nested\a.txt"))
            .unwrap();
        assert_eq!(a.size, 5);
    }

    #[test]
    fn walk_does_not_follow_junction() {
        let base = std::env::temp_dir().join(format!("multiusb_junction_{}", std::process::id()));
        let target = base.join("target");
        let link = base.join("link");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir(&base).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(target.join("secret.txt"), b"secret").unwrap();
        let status = Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &link.display().to_string(),
                &target.display().to_string(),
            ])
            .status()
            .unwrap();
        assert!(
            status.success(),
            "mklink /J failed — run as user with symlink privilege"
        );

        let walk = walk_source(&base).unwrap();
        let _ = fs::remove_dir_all(&base);

        let paths = rel_paths(&walk.entries);
        assert!(
            !paths
                .iter()
                .any(|p| p == &PathBuf::from(r"link\secret.txt")),
            "junction contents must not be walked"
        );
        assert!(
            walk.skipped_links
                .iter()
                .any(|p| p == &PathBuf::from("link")),
            "junction relative path must be in skipped_links"
        );
    }
}

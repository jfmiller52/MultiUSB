use std::path::{Path, PathBuf};

pub fn extended_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text.starts_with(r"\\?\") {
        return path.to_path_buf();
    }
    if text.starts_with(r"\\") {
        return PathBuf::from(format!(r"\\?\UNC\{}", &text[2..]));
    }
    PathBuf::from(format!(r"\\?\{text}"))
}

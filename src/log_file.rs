use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub fn write_run_log(cwd: &Path, stamp: &str, lines: &[String]) -> io::Result<PathBuf> {
    let logs_dir = cwd.join("Logs");
    fs::create_dir_all(&logs_dir)?;
    let path = unique_log_path(&logs_dir, stamp)?;
    let mut file = File::create(&path)?;
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            file.write_all(b"\n")?;
        }
        file.write_all(line.as_bytes())?;
    }
    Ok(path)
}

fn unique_log_path(logs_dir: &Path, stamp: &str) -> io::Result<PathBuf> {
    let base = logs_dir.join(format!("{stamp}.log"));
    if !base.exists() {
        return Ok(base);
    }
    let mut n = 2u32;
    loop {
        let candidate = logs_dir.join(format!("{stamp}-{n}.log"));
        if !candidate.exists() {
            return Ok(candidate);
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn write_run_log_creates_file_and_avoids_collisions() {
        let base = std::env::temp_dir().join("multiusb_log_test");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();

        let stamp = "2026-11-09_13-04-00";
        let lines = vec!["first".to_string(), "second".to_string()];
        let path = write_run_log(&base, stamp, &lines).unwrap();
        assert_eq!(
            path,
            base.join("Logs").join("2026-11-09_13-04-00.log")
        );
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "first\nsecond");

        let path2 = write_run_log(&base, stamp, &lines).unwrap();
        assert_eq!(
            path2,
            base.join("Logs").join("2026-11-09_13-04-00-2.log")
        );

        let _ = fs::remove_dir_all(&base);
    }
}

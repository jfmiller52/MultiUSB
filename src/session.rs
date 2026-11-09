use std::path::PathBuf;

use crate::label::{Filesystem, PartitionStyle, validate_label};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchForm {
    pub source: Option<PathBuf>,
    pub filesystem: Option<Filesystem>,
    pub partition: Option<PartitionStyle>,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveRow {
    pub key: String,
    pub checked: bool,
    pub model: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchUi {
    pub form: BatchForm,
    pub rows: Vec<DriveRow>,
    pub log: Vec<String>,
    pub show_another: bool,
}

pub fn initial_form() -> BatchForm {
    BatchForm {
        source: None,
        filesystem: None,
        partition: None,
        label: String::new(),
    }
}

pub fn start_enabled(form: &BatchForm, checked: usize) -> bool {
    if checked == 0 {
        return false;
    }
    let Some(source) = form.source.as_ref() else {
        return false;
    };
    if !source.is_dir() {
        return false;
    }
    let Some(filesystem) = form.filesystem else {
        return false;
    };
    if form.partition.is_none() {
        return false;
    }
    validate_label(filesystem, &form.label).is_ok()
}

pub fn confirmation_body(form: &BatchForm, rows: &[DriveRow]) -> String {
    let filesystem = form
        .filesystem
        .map(|fs| fs.as_str())
        .unwrap_or("(none)");
    let partition = form
        .partition
        .map(|p| p.as_str())
        .unwrap_or("(none)");
    let mut body = format!(
        "Filesystem: {filesystem}\nPartition: {partition}\nLabel: {}",
        form.label
    );
    for row in rows {
        if row.checked {
            body.push('\n');
            body.push_str(&row.model);
        }
    }
    body
}

pub fn run_another_batch(ui: &mut BatchUi) {
    for row in &mut ui.rows {
        row.checked = false;
    }
    ui.log.clear();
    ui.show_another = false;
}

pub fn overall_fraction(bytes_copied: &[u64], source_bytes: u64) -> f32 {
    if bytes_copied.is_empty() || source_bytes == 0 {
        return 0.0;
    }
    let sum: u64 = bytes_copied.iter().sum();
    let total = source_bytes.saturating_mul(bytes_copied.len() as u64);
    sum as f32 / total as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn full_form(source: PathBuf) -> BatchForm {
        BatchForm {
            source: Some(source),
            filesystem: Some(Filesystem::ExFat),
            partition: Some(PartitionStyle::Mbr),
            label: "STORE".to_string(),
        }
    }

    #[test]
    fn initial_form_is_empty() {
        let form = initial_form();
        assert_eq!(form.filesystem, None);
        assert_eq!(form.partition, None);
        assert_eq!(form.label, "");
        assert_eq!(form.source, None);
    }

    #[test]
    fn start_enabled_requires_complete_valid_form() {
        let dir = std::env::temp_dir().join("multiusb_session_src");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let good = full_form(dir.clone());
        assert!(start_enabled(&good, 1));

        assert!(!start_enabled(&BatchForm {
            filesystem: None,
            ..good.clone()
        }, 1));
        assert!(!start_enabled(&BatchForm {
            partition: None,
            ..good.clone()
        }, 1));
        assert!(!start_enabled(&BatchForm {
            source: None,
            ..good.clone()
        }, 1));
        assert!(!start_enabled(&good, 0));
        assert!(!start_enabled(&BatchForm {
            label: "bad\"quote".to_string(),
            ..good
        }, 1));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn confirmation_body_lists_checked_drives_and_settings() {
        let form = BatchForm {
            source: None,
            filesystem: Some(Filesystem::ExFat),
            partition: Some(PartitionStyle::Mbr),
            label: "STORE".to_string(),
        };
        let rows = [
            DriveRow {
                key: "0".to_string(),
                checked: true,
                model: "CheckedModel".to_string(),
                detail: String::new(),
            },
            DriveRow {
                key: "1".to_string(),
                checked: false,
                model: "UncheckedModel".to_string(),
                detail: String::new(),
            },
        ];
        let body = confirmation_body(&form, &rows);
        assert!(body.contains("CheckedModel"));
        assert!(!body.contains("UncheckedModel"));
        assert!(body.contains("exFAT"));
        assert!(body.contains("MBR"));
        assert!(body.contains("STORE"));
    }

    #[test]
    fn run_another_batch_resets_ui_keeps_form() {
        let form = BatchForm {
            source: Some(PathBuf::from(r"D:\USBContents")),
            filesystem: Some(Filesystem::ExFat),
            partition: Some(PartitionStyle::Mbr),
            label: "STORE".to_string(),
        };
        let mut ui = BatchUi {
            form: form.clone(),
            rows: vec![
                DriveRow {
                    key: "a".to_string(),
                    checked: true,
                    model: String::new(),
                    detail: String::new(),
                },
                DriveRow {
                    key: "b".to_string(),
                    checked: true,
                    model: String::new(),
                    detail: String::new(),
                },
            ],
            log: vec!["line".to_string()],
            show_another: true,
        };
        run_another_batch(&mut ui);
        assert_eq!(ui.form, form);
        assert!(ui.rows.iter().all(|r| !r.checked));
        assert!(ui.log.is_empty());
        assert!(!ui.show_another);
    }

    #[test]
    fn overall_fraction_averages_per_drive_progress() {
        assert!((overall_fraction(&[50, 100], 100) - 0.75).abs() < f32::EPSILON);
        assert_eq!(overall_fraction(&[], 100), 0.0);
        assert_eq!(overall_fraction(&[10], 0), 0.0);
    }
}

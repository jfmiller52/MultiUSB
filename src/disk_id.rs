#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskId {
    pub pnp_device_id: Option<String>,
    pub serial: Option<String>,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumeratedDisk {
    pub number: u32,
    pub model: String,
    pub size_bytes: u64,
    pub letters: Vec<char>,
    pub id: DiskId,
    pub media_type: String,
    pub contains_system_volume: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rematch {
    Unique(u32),
    NotUnique,
    NotRemovable,
    Missing,
}

pub fn rematch(selected: &DiskId, current: &[EnumeratedDisk]) -> Rematch {
    let candidates: Vec<&EnumeratedDisk> = if let Some(pnp) = &selected.pnp_device_id {
        current
            .iter()
            .filter(|d| d.id.pnp_device_id.as_deref() == Some(pnp.as_str()))
            .collect()
    } else if let Some(serial) = &selected.serial {
        current
            .iter()
            .filter(|d| {
                d.id.serial.as_deref() == Some(serial.as_str())
                    && d.size_bytes == selected.size_bytes
            })
            .collect()
    } else {
        return Rematch::NotUnique;
    };

    match candidates.len() {
        0 => Rematch::Missing,
        1 => {
            let disk = candidates[0];
            if disk.media_type.contains("Removable") {
                Rematch::Unique(disk.number)
            } else {
                Rematch::NotRemovable
            }
        }
        _ => Rematch::NotUnique,
    }
}

pub enum Gate {
    Ready(Vec<Result<EnumeratedDisk, Rematch>>),
    AbortSystemDisk { number: u32, model: String },
}

pub fn gate(selected: &[DiskId], current: &[EnumeratedDisk]) -> Gate {
    let mut results = Vec::with_capacity(selected.len());

    for id in selected {
        match rematch(id, current) {
            Rematch::Unique(number) => {
                let Some(disk) = current.iter().find(|d| d.number == number) else {
                    results.push(Err(Rematch::Missing));
                    continue;
                };
                if disk.contains_system_volume {
                    return Gate::AbortSystemDisk {
                        number: disk.number,
                        model: disk.model.clone(),
                    };
                }
                results.push(Ok(disk.clone()));
            }
            other => results.push(Err(other)),
        }
    }

    Gate::Ready(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(
        number: u32,
        model: &str,
        pnp: Option<&str>,
        serial: Option<&str>,
        size: u64,
        media: &str,
        system: bool,
    ) -> EnumeratedDisk {
        EnumeratedDisk {
            number,
            model: model.to_string(),
            size_bytes: size,
            letters: vec![],
            id: DiskId {
                pnp_device_id: pnp.map(str::to_string),
                serial: serial.map(str::to_string),
                size_bytes: size,
            },
            media_type: media.to_string(),
            contains_system_volume: system,
        }
    }

    fn id(pnp: Option<&str>, serial: Option<&str>, size: u64) -> DiskId {
        DiskId {
            pnp_device_id: pnp.map(str::to_string),
            serial: serial.map(str::to_string),
            size_bytes: size,
        }
    }

    #[test]
    fn pnp_match_unique_even_when_serial_shared() {
        let current = [
            disk(5, "King", Some("USB\\KING"), Some("ABC"), 10, "Removable Media", false),
            disk(6, "Other", None, Some("ABC"), 10, "Removable Media", false),
        ];
        let selected = id(Some("USB\\KING"), None, 10);
        assert!(matches!(rematch(&selected, &current), Rematch::Unique(5)));
    }

    #[test]
    fn serial_only_two_current_both_not_unique() {
        let current = [
            disk(1, "A", None, Some("ABC"), 10, "Removable Media", false),
            disk(2, "B", None, Some("ABC"), 10, "Removable Media", false),
        ];
        let selected = id(None, Some("ABC"), 10);
        assert!(matches!(rematch(&selected, &current), Rematch::NotUnique));
    }

    #[test]
    fn pnp_match_fixed_media_not_removable() {
        let current = [disk(
            3,
            "Fixed",
            Some("USB\\FIX"),
            None,
            100,
            "Fixed hard disk media",
            false,
        )];
        let selected = id(Some("USB\\FIX"), None, 100);
        assert!(matches!(rematch(&selected, &current), Rematch::NotRemovable));
    }

    #[test]
    fn pnp_no_match_missing() {
        let current = [disk(1, "X", Some("USB\\OTHER"), None, 10, "Removable Media", false)];
        let selected = id(Some("USB\\KING"), None, 10);
        assert!(matches!(rematch(&selected, &current), Rematch::Missing));
    }

    #[test]
    fn no_pnp_no_serial_not_unique() {
        let current = [disk(1, "X", None, Some("S"), 10, "Removable Media", false)];
        let selected = id(None, None, 10);
        assert!(matches!(rematch(&selected, &current), Rematch::NotUnique));
    }

    #[test]
    fn gate_aborts_when_unique_match_is_system_volume() {
        let current = [
            disk(1, "Safe", Some("USB\\A"), None, 10, "Removable Media", false),
            disk(
                2,
                "System USB",
                Some("USB\\B"),
                None,
                20,
                "Removable Media",
                true,
            ),
        ];
        let selected = [id(Some("USB\\A"), None, 10), id(Some("USB\\B"), None, 20)];
        match gate(&selected, &current) {
            Gate::AbortSystemDisk { number, model } => {
                assert_eq!(number, 2);
                assert_eq!(model, "System USB");
            }
            Gate::Ready(_) => panic!("expected abort"),
        }
    }

    #[test]
    fn gate_ready_missing_then_ok() {
        let current = [disk(
            7,
            "Only",
            Some("USB\\ONLY"),
            None,
            10,
            "Removable Media",
            false,
        )];
        let selected = [id(Some("USB\\MISSING"), None, 10), id(Some("USB\\ONLY"), None, 10)];
        match gate(&selected, &current) {
            Gate::Ready(results) => {
                assert_eq!(results.len(), 2);
                assert!(matches!(results[0], Err(Rematch::Missing)));
                assert!(matches!(results[1], Ok(ref d) if d.number == 7));
            }
            Gate::AbortSystemDisk { .. } => panic!("expected ready"),
        }
    }

    #[test]
    fn two_selected_serial_only_both_not_unique() {
        let current = [
            disk(1, "A", None, Some("ABC"), 10, "Removable Media", false),
            disk(2, "B", None, Some("ABC"), 10, "Removable Media", false),
        ];
        let s1 = id(None, Some("ABC"), 10);
        let s2 = id(None, Some("ABC"), 10);
        assert!(matches!(rematch(&s1, &current), Rematch::NotUnique));
        assert!(matches!(rematch(&s2, &current), Rematch::NotUnique));
    }
}

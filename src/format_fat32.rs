//! Built-in large FAT32 formatter.
//!
//! Writes only whole-sector, sequential bursts (no read-modify-write).

use std::io::{self, Read, Seek, SeekFrom, Write};

const BYTES_PER_SECTOR: u32 = 512;
const NUM_FATS: u32 = 2;
const BACKUP_BOOT_SECTOR: u32 = 6;
const BURST_SECTORS: u32 = 128; // 64 KiB
/// Page alignment satisfies FILE_FLAG_NO_BUFFERING physical-sector rules.
const BUFFER_ALIGN: usize = 4096;

/// Page-aligned buffer for `FILE_FLAG_NO_BUFFERING` volume I/O (Ridgecrop).
struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

impl AlignedBuf {
    fn zeros(len: usize) -> io::Result<Self> {
        let mut buf = Self::alloc(len)?;
        buf.as_mut_slice().fill(0);
        Ok(buf)
    }

    fn from_slice(data: &[u8]) -> io::Result<Self> {
        let mut buf = Self::alloc(data.len())?;
        buf.as_mut_slice().copy_from_slice(data);
        Ok(buf)
    }

    fn alloc(len: usize) -> io::Result<Self> {
        let size = len.max(1);
        let layout = std::alloc::Layout::from_size_align(size, BUFFER_ALIGN)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "aligned alloc failed",
            ));
        }
        debug_assert_eq!((ptr as usize) % BUFFER_ALIGN, 0);
        Ok(Self { ptr, len })
    }

    fn as_slice(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        if self.len == 0 {
            return &mut [];
        }
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        let size = self.len.max(1);
        if let Ok(layout) = std::alloc::Layout::from_size_align(size, BUFFER_ALIGN) {
            unsafe { std::alloc::dealloc(self.ptr, layout) };
        }
    }
}

/// Format a partition image as FAT32.
///
/// `device` must accept seeks and writes at relative offset 0 = start of the
/// partition (not the whole disk). All writes are multiples of 512 bytes.
pub fn format_fat32_volume<W: Read + Write + Seek>(
    device: &mut W,
    total_bytes: u64,
    hidden_sectors: u32,
    label: &[u8; 11],
) -> io::Result<()> {
    let bytes_per_sector = u64::from(BYTES_PER_SECTOR);
    if total_bytes < bytes_per_sector * 65_536 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "volume too small for FAT32 (need >= 65536 sectors)",
        ));
    }
    let total_sectors = total_bytes / bytes_per_sector;
    if total_sectors >= u64::from(u32::MAX) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "volume too large for FAT32 (max ~2 TiB with 512-byte sectors)",
        ));
    }
    let total_sectors = total_sectors as u32;

    let cluster_size = default_cluster_size(total_bytes);
    let sectors_per_cluster = cluster_size / BYTES_PER_SECTOR;
    if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid sectors per cluster",
        ));
    }

    // Ridgecrop fat32format uses a fixed 32 reserved sectors (see fat32.txt /
    // fat32format.c). Do not inflate reserved for 1 MiB alignment — that layout
    // differed from the proven Windows-mountable writer.
    let reserved = 32u32;
    let mut fat_size = fat_size_sectors(total_sectors, reserved, sectors_per_cluster, NUM_FATS);
    // fat32format.c: grow the FAT if the closed formula undershoots cluster entries.
    for _ in 0..4 {
        let system_area = reserved + NUM_FATS * fat_size;
        let user_area = total_sectors.checked_sub(system_area).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "system area too large")
        })?;
        let cluster_count = user_area / sectors_per_cluster;
        let fat_needed =
            (u64::from(cluster_count) * 4).div_ceil(u64::from(BYTES_PER_SECTOR)) as u32;
        if fat_needed <= fat_size {
            break;
        }
        fat_size = fat_needed;
    }
    let system_area = reserved + NUM_FATS * fat_size;
    let user_area = total_sectors
        .checked_sub(system_area)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "system area too large"))?;
    let cluster_count = user_area / sectors_per_cluster;
    if cluster_count < 65_536 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "FAT32 needs at least 65536 clusters",
        ));
    }
    if cluster_count > 0x0FFF_FFFF {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many clusters for FAT32",
        ));
    }

    let volume_id = volume_id_now();
    let boot = build_boot_sector(
        total_sectors,
        reserved,
        sectors_per_cluster as u8,
        fat_size,
        hidden_sectors,
        volume_id,
        label,
    );
    let fsinfo = build_fsinfo(cluster_count.saturating_sub(1));
    let mut first_fat = vec![0u8; BYTES_PER_SECTOR as usize];
    {
        let words = as_u32_slice_mut(&mut first_fat);
        words[0] = 0x0FFF_FFF8; // media
        words[1] = 0x0FFF_FFFF; // EOC
        words[2] = 0x0FFF_FFFF; // root dir cluster 2 EOC
    }

    // Clear reserved + FATs + first data cluster (root).
    // Cheap USB controllers often drop earlier cached writes when the buffer
    // fills — if FAT-body zeros are lost but boot/FAT-head (written last) stick,
    // Windows mounts "OK" then returns os error 1392 on real allocation.
    let clear_sectors = system_area + sectors_per_cluster;
    zero_sectors(device, 0, clear_sectors)?;
    device.flush()?;

    write_sectors(device, 0, &boot)?;
    write_sectors(device, 1, &fsinfo)?;
    write_sectors(device, BACKUP_BOOT_SECTOR, &boot)?;
    write_sectors(device, BACKUP_BOOT_SECTOR + 1, &fsinfo)?;
    for fat_index in 0..NUM_FATS {
        let start = reserved + fat_index * fat_size;
        write_sectors(device, start, &first_fat)?;
    }
    device.flush()?;
    verify_fat32_image(device, reserved, fat_size)?;
    Ok(())
}

/// Confirm boot / FSInfo / first FAT sector survived on the media.
/// Catches USB stacks that accept WriteFile then discard the data.
pub fn verify_fat32_image<R: Read + Seek>(
    device: &mut R,
    reserved: u32,
    fat_size: u32,
) -> io::Result<()> {
    let mut boot = [0u8; BYTES_PER_SECTOR as usize];
    read_sectors(device, 0, &mut boot)?;
    if boot[510] != 0x55 || boot[511] != 0xAA {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "boot signature missing after format write",
        ));
    }
    if &boot[82..90] != b"FAT32   " {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "boot FS type not FAT32 after format write",
        ));
    }
    // BS_Reserved1 / CurrentHead — must be 0 or Windows pops Scan and Fix.
    if boot[65] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("boot dirty byte set after format (BS_Reserved1={})", boot[65]),
        ));
    }
    let mut fsinfo = [0u8; BYTES_PER_SECTOR as usize];
    read_sectors(device, 1, &mut fsinfo)?;
    if fsinfo[0..4] != 0x4161_5252u32.to_le_bytes() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "FSInfo lead signature mismatch after format write",
        ));
    }
    let mut fat0 = [0u8; BYTES_PER_SECTOR as usize];
    read_sectors(device, reserved, &mut fat0)?;
    let w0 = u32::from_le_bytes(fat0[0..4].try_into().unwrap());
    let w1 = u32::from_le_bytes(fat0[4..8].try_into().unwrap());
    let w2 = u32::from_le_bytes(fat0[8..12].try_into().unwrap());
    if w0 != 0x0FFF_FFF8 || w1 != 0x0FFF_FFFF || w2 != 0x0FFF_FFFF {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("first FAT entries corrupt after format write ({w0:#x},{w1:#x},{w2:#x})"),
        ));
    }
    // Mirror FAT must match the head sector we just wrote.
    let mut fat1 = [0u8; BYTES_PER_SECTOR as usize];
    read_sectors(device, reserved + fat_size, &mut fat1)?;
    if fat1[..12] != fat0[..12] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "FAT mirror head mismatch after format write",
        ));
    }
    // Sample mid-FAT and near-end sectors — these are the first bytes written
    // during zeroing and the first to vanish from a lying write cache.
    if fat_size > 2 {
        for &rel in &[fat_size / 2, fat_size.saturating_sub(2)] {
            let mut mid = [0u8; BYTES_PER_SECTOR as usize];
            read_sectors(device, reserved + rel, &mut mid)?;
            if mid.iter().any(|b| *b != 0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("FAT body not zero at sector +{rel} after format write"),
                ));
            }
            read_sectors(device, reserved + fat_size + rel, &mut mid)?;
            if mid.iter().any(|b| *b != 0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("FAT mirror body not zero at sector +{rel} after format write"),
                ));
            }
        }
    }
    Ok(())
}

fn read_sectors<R: Read + Seek>(
    device: &mut R,
    start_sector: u32,
    buf: &mut [u8],
) -> io::Result<()> {
    assert_eq!(buf.len() % BYTES_PER_SECTOR as usize, 0);
    let mut aligned = AlignedBuf::zeros(buf.len())?;
    let offset = u64::from(start_sector) * u64::from(BYTES_PER_SECTOR);
    device.seek(SeekFrom::Start(offset))?;
    device.read_exact(aligned.as_mut_slice())?;
    buf.copy_from_slice(aligned.as_slice());
    Ok(())
}

fn default_cluster_size(total_bytes: u64) -> u32 {
    // https://learn.microsoft.com/en-us/windows/win32/fileio/default-cluster-size
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;
    if total_bytes < 64 * MB {
        512
    } else if total_bytes < 128 * MB {
        1 * KB as u32
    } else if total_bytes < 256 * MB {
        2 * KB as u32
    } else if total_bytes < 8 * GB {
        4 * KB as u32
    } else if total_bytes < 16 * GB {
        8 * KB as u32
    } else if total_bytes < 32 * GB {
        16 * KB as u32
    } else if total_bytes < 2 * TB {
        32 * KB as u32
    } else {
        64 * KB as u32
    }
}

fn fat_size_sectors(
    disk_sectors: u32,
    reserved: u32,
    sectors_per_cluster: u32,
    num_fats: u32,
) -> u32 {
    // Ridgecrop fat32format.c `get_fat_size_sectors`.
    let fat_element_size = 4u64;
    let numerator = fat_element_size * (u64::from(disk_sectors) - u64::from(reserved));
    let denominator =
        u64::from(sectors_per_cluster) * u64::from(BYTES_PER_SECTOR) + fat_element_size * u64::from(num_fats);
    (numerator / denominator + 1) as u32
}

fn volume_id_now() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u32)
        .unwrap_or(0x1234_5678);
    nanos
}

fn build_boot_sector(
    total_sectors: u32,
    reserved: u32,
    sectors_per_cluster: u8,
    fat_size: u32,
    hidden_sectors: u32,
    volume_id: u32,
    label: &[u8; 11],
) -> Vec<u8> {
    let mut s = vec![0u8; BYTES_PER_SECTOR as usize];
    s[0] = 0xEB;
    s[1] = 0x58;
    s[2] = 0x90;
    s[3..11].copy_from_slice(b"MSWIN4.1");
    s[11..13].copy_from_slice(&512u16.to_le_bytes());
    s[13] = sectors_per_cluster;
    s[14..16].copy_from_slice(&(reserved as u16).to_le_bytes());
    s[16] = NUM_FATS as u8;
    // root entries / totsec16 / fat16 size = 0 for FAT32
    s[21] = 0xF8; // media
    s[24..26].copy_from_slice(&63u16.to_le_bytes()); // sectors/track placeholder
    s[26..28].copy_from_slice(&255u16.to_le_bytes()); // heads placeholder
    // Sectors preceding this partition — required for Windows to accept the volume.
    s[28..32].copy_from_slice(&hidden_sectors.to_le_bytes());
    s[32..36].copy_from_slice(&total_sectors.to_le_bytes());
    s[36..40].copy_from_slice(&fat_size.to_le_bytes());
    // ext flags / FSVer = 0
    s[44..48].copy_from_slice(&2u32.to_le_bytes()); // root cluster
    s[48..50].copy_from_slice(&1u16.to_le_bytes()); // FSInfo sector
    s[50..52].copy_from_slice(&(BACKUP_BOOT_SECTOR as u16).to_le_bytes());
    s[64] = 0x80; // drive number
    s[66] = 0x29; // boot sig
    s[67..71].copy_from_slice(&volume_id.to_le_bytes());
    s[71..82].copy_from_slice(label);
    s[82..90].copy_from_slice(b"FAT32   ");
    s[510] = 0x55;
    s[511] = 0xAA;
    s
}

fn build_fsinfo(free_clusters: u32) -> Vec<u8> {
    let mut s = vec![0u8; BYTES_PER_SECTOR as usize];
    s[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
    s[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
    s[488..492].copy_from_slice(&free_clusters.to_le_bytes());
    s[492..496].copy_from_slice(&3u32.to_le_bytes()); // next free
    s[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes());
    s
}

fn as_u32_slice_mut(buf: &mut [u8]) -> &mut [u32] {
    assert_eq!(buf.len() % 4, 0);
    let len = buf.len() / 4;
    unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u32, len) }
}

fn write_sectors<W: Write + Seek>(device: &mut W, start_sector: u32, data: &[u8]) -> io::Result<()> {
    assert_eq!(data.len() % BYTES_PER_SECTOR as usize, 0);
    // Always copy into a page-aligned buffer — required for FILE_FLAG_NO_BUFFERING.
    let aligned = AlignedBuf::from_slice(data)?;
    let offset = u64::from(start_sector) * u64::from(BYTES_PER_SECTOR);
    let mut last_err = None;
    for attempt in 0..5u32 {
        match device
            .seek(SeekFrom::Start(offset))
            .and_then(|_| device.write_all(aligned.as_slice()))
        {
            Ok(()) => return Ok(()),
            Err(err) => {
                // USB stacks sometimes return 433/55 mid-burst; brief retry.
                let code = err.raw_os_error();
                last_err = Some(err);
                if !matches!(code, Some(433 | 55 | 21 | 31)) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50 * u64::from(attempt + 1)));
            }
        }
    }
    let detail = last_err
        .map(|err| err.to_string())
        .unwrap_or_else(|| "write_sectors failed without error".to_string());
    Err(io::Error::new(
        io::ErrorKind::Other,
        format!("at partition sector {start_sector} (rel {offset} bytes): {detail}"),
    ))
}

fn zero_sectors<W: Write + Seek>(
    device: &mut W,
    start_sector: u32,
    sector_count: u32,
) -> io::Result<()> {
    let mut remaining = sector_count;
    let mut at = start_sector;
    let burst_len = (BURST_SECTORS * BYTES_PER_SECTOR) as usize;
    let burst = AlignedBuf::zeros(burst_len)?;
    let total_bytes = u64::from(sector_count) * u64::from(BYTES_PER_SECTOR);
    while remaining > 0 {
        let n = remaining.min(BURST_SECTORS);
        let bytes = (n * BYTES_PER_SECTOR) as usize;
        // write_sectors re-copies into an aligned buf; pass the prefix length via slice.
        if let Err(err) = write_sectors(device, at, &burst.as_slice()[..bytes]) {
            let done = u64::from(at.saturating_sub(start_sector)) * u64::from(BYTES_PER_SECTOR);
            return Err(io::Error::new(
                err.kind(),
                format!(
                    "zeroing system area at sector {at} ({done}/{total_bytes} bytes cleared): {err}"
                ),
            ));
        }
        at += n;
        remaining -= n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn formats_512mib_image_with_boot_signature_and_label() {
        // Need >= 65536 clusters after reserved/FAT overhead. 512 MiB @ 4 KiB/cluster.
        let size = 512 * 1024 * 1024usize;
        let mut image = vec![0u8; size];
        {
            let mut cursor = Cursor::new(&mut image[..]);
            format_fat32_volume(&mut cursor, size as u64, 2048, b"MCD-NSO-USB").unwrap();
        }
        assert_eq!(image[510], 0x55);
        assert_eq!(image[511], 0xAA);
        assert_eq!(&image[3..11], b"MSWIN4.1");
        assert_eq!(&image[71..82], b"MCD-NSO-USB");
        assert_eq!(&image[82..90], b"FAT32   ");
        assert_eq!(&image[28..32], &2048u32.to_le_bytes());
        // Backup boot
        assert_eq!(image[6 * 512 + 510], 0x55);
        assert_eq!(image[6 * 512 + 511], 0xAA);
    }

    #[test]
    fn rejects_tiny_volume() {
        let mut image = vec![0u8; 1024];
        let err =
            format_fat32_volume(&mut Cursor::new(&mut image[..]), 1024, 0, b"NO NAME    ")
                .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn fat_size_grows_when_closed_formula_undershoots() {
        // Synthetic numbers where FatNeeded > initial FatSize once.
        let reserved = 32u32;
        let spc = 64u32; // 32 KiB clusters
        let total = 120_000_000u32; // ~58 GiB worth of sectors
        let mut fat_size = fat_size_sectors(total, reserved, spc, NUM_FATS);
        let initial = fat_size;
        for _ in 0..4 {
            let system_area = reserved + NUM_FATS * fat_size;
            let user_area = total - system_area;
            let cluster_count = user_area / spc;
            let fat_needed =
                (u64::from(cluster_count) * 4).div_ceil(u64::from(BYTES_PER_SECTOR)) as u32;
            if fat_needed <= fat_size {
                break;
            }
            fat_size = fat_needed;
        }
        let system_area = reserved + NUM_FATS * fat_size;
        let clusters = (total - system_area) / spc;
        let needed = (u64::from(clusters) * 4).div_ceil(u64::from(BYTES_PER_SECTOR)) as u32;
        assert!(fat_size >= needed, "fat_size={fat_size} needed={needed} initial={initial}");
    }
}

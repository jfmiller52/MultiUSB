use std::cmp::min;
use std::io::{self, Read, Seek, SeekFrom, Write};

/// Adapts a block device so callers may issue arbitrary-sized reads/writes.
///
/// Windows volume handles (`\\.\X:`) require each ReadFile/WriteFile to start on a
/// sector boundary and cover a whole number of sectors. Libraries like `fatfs`
/// write boot/FAT/dir fields in small chunks, which surface as os error 87
/// (`ERROR_INVALID_PARAMETER`) without this adapter.
pub struct SectorIo<T> {
    inner: T,
    sector_size: u64,
    pos: u64,
    buf: Vec<u8>,
}

impl<T> SectorIo<T> {
    pub fn new(inner: T, sector_size: u64) -> Self {
        assert!(sector_size.is_power_of_two() && sector_size > 0);
        Self {
            inner,
            sector_size,
            pos: 0,
            buf: vec![0; sector_size as usize],
        }
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: Read + Write + Seek> SectorIo<T> {
    fn read_sector(&mut self, sector_index: u64) -> io::Result<()> {
        let offset = sector_index * self.sector_size;
        self.inner.seek(SeekFrom::Start(offset))?;
        self.inner.read_exact(&mut self.buf)?;
        Ok(())
    }

    /// Load a sector for read-modify-write. Blank/RAW media may reject reads;
    /// treat that as an all-zero sector so formatting can proceed.
    fn load_sector_for_write(&mut self, sector_index: u64) -> io::Result<()> {
        match self.read_sector(sector_index) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.buf.fill(0);
                Ok(())
            }
        }
    }

    fn write_sector(&mut self, sector_index: u64) -> io::Result<()> {
        let offset = sector_index * self.sector_size;
        self.inner.seek(SeekFrom::Start(offset))?;
        self.inner.write_all(&self.buf)?;
        Ok(())
    }
}

impl<T: Read + Write + Seek> Read for SectorIo<T> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let sector_index = self.pos / self.sector_size;
        let offset_in_sector = (self.pos % self.sector_size) as usize;
        self.read_sector(sector_index)?;
        let take = min(out.len(), self.sector_size as usize - offset_in_sector);
        out[..take].copy_from_slice(&self.buf[offset_in_sector..offset_in_sector + take]);
        self.pos += take as u64;
        Ok(take)
    }
}

impl<T: Read + Write + Seek> Write for SectorIo<T> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let sector_index = self.pos / self.sector_size;
        let offset_in_sector = (self.pos % self.sector_size) as usize;
        let take = min(data.len(), self.sector_size as usize - offset_in_sector);
        let whole_sector = offset_in_sector == 0 && take == self.sector_size as usize;
        if whole_sector {
            self.buf.fill(0);
        } else {
            self.load_sector_for_write(sector_index)?;
        }
        self.buf[offset_in_sector..offset_in_sector + take].copy_from_slice(&data[..take]);
        self.write_sector(sector_index)?;
        self.pos += take as u64;
        Ok(take)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<T: Seek> Seek for SectorIo<T> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => {
                let end = self.inner.seek(SeekFrom::End(0))? as i64;
                end + n
            }
            SeekFrom::Current(n) => self.pos as i64 + n,
        };
        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start of volume",
            ));
        }
        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::SectorIo;
    use std::io::{Read, Seek, SeekFrom, Write};

    /// Backend that rejects any I/O that is not a full sector at a sector boundary.
    struct StrictSectors {
        data: Vec<u8>,
        pos: u64,
        sector_size: u64,
    }

    impl StrictSectors {
        fn new(bytes: usize, sector_size: u64) -> Self {
            Self {
                data: vec![0; bytes],
                pos: 0,
                sector_size,
            }
        }

        fn reject_unaligned(&self, len: usize) -> std::io::Result<()> {
            if self.pos % self.sector_size != 0 || len as u64 % self.sector_size != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "parameter is incorrect",
                ));
            }
            Ok(())
        }
    }

    impl Read for StrictSectors {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reject_unaligned(buf.len())?;
            let start = self.pos as usize;
            let end = start + buf.len();
            buf.copy_from_slice(&self.data[start..end]);
            self.pos = end as u64;
            Ok(buf.len())
        }
    }

    impl Write for StrictSectors {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.reject_unaligned(buf.len())?;
            let start = self.pos as usize;
            let end = start + buf.len();
            self.data[start..end].copy_from_slice(buf);
            self.pos = end as u64;
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Seek for StrictSectors {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            let new_pos = match pos {
                SeekFrom::Start(n) => n as i64,
                SeekFrom::End(n) => self.data.len() as i64 + n,
                SeekFrom::Current(n) => self.pos as i64 + n,
            };
            if new_pos < 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "seek before start",
                ));
            }
            self.pos = new_pos as u64;
            Ok(self.pos)
        }
    }

    #[test]
    fn small_writes_succeed_against_strict_sector_backend() {
        let backend = StrictSectors::new(4096, 512);
        let mut io = SectorIo::new(backend, 512);
        io.write_all(&[0xEB, 0x58, 0x90]).unwrap();
        io.write_all(b"MSDOS5.0").unwrap();
        io.seek(SeekFrom::Start(510)).unwrap();
        io.write_all(&[0x55, 0xAA]).unwrap();

        let mut check = [0u8; 11];
        io.seek(SeekFrom::Start(0)).unwrap();
        io.read_exact(&mut check).unwrap();
        assert_eq!(&check[..3], &[0xEB, 0x58, 0x90]);
        assert_eq!(&check[3..], b"MSDOS5.0");

        let mut sig = [0u8; 2];
        io.seek(SeekFrom::Start(510)).unwrap();
        io.read_exact(&mut sig).unwrap();
        assert_eq!(sig, [0x55, 0xAA]);
    }

    #[test]
    fn strict_backend_rejects_raw_small_write() {
        let mut backend = StrictSectors::new(512, 512);
        let err = backend.write_all(&[1, 2, 3]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn write_succeeds_when_backend_reads_fail() {
        struct WriteOnly {
            data: Vec<u8>,
            pos: u64,
        }
        impl Read for WriteOnly {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "device does not exist",
                ))
            }
        }
        impl Write for WriteOnly {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let start = self.pos as usize;
                let end = start + buf.len();
                self.data[start..end].copy_from_slice(buf);
                self.pos = end as u64;
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl Seek for WriteOnly {
            fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
                let new_pos = match pos {
                    SeekFrom::Start(n) => n as i64,
                    SeekFrom::End(n) => self.data.len() as i64 + n,
                    SeekFrom::Current(n) => self.pos as i64 + n,
                };
                self.pos = new_pos as u64;
                Ok(self.pos)
            }
        }

        let mut backend = WriteOnly {
            data: vec![0xff; 1024],
            pos: 0,
        };
        {
            let mut io = SectorIo::new(&mut backend, 512);
            io.write_all(&[0xEB, 0x58, 0x90]).unwrap();
        }
        assert_eq!(&backend.data[..3], &[0xEB, 0x58, 0x90]);
        assert_eq!(backend.data[3], 0);
    }

    #[test]
    fn fatfs_format_volume_on_strict_sector_backend() {
        let size = 64 * 1024 * 1024usize;
        let mut strict = StrictSectors::new(size, 512);
        {
            let mut io = SectorIo::new(&mut strict, 512);
            let options = fatfs::FormatVolumeOptions::new()
                .fat_type(fatfs::FatType::Fat32)
                .bytes_per_sector(512)
                .total_sectors((size as u64 / 512) as u32)
                .volume_label(*b"MCD-NSO-USB");
            fatfs::format_volume(&mut io, options).expect("format should succeed");
        }
        assert_eq!(strict.data[510], 0x55);
        assert_eq!(strict.data[511], 0xAA);
        // fatfs default OEM name is MSWIN4.1
        assert_eq!(&strict.data[3..11], b"MSWIN4.1");
        assert_eq!(&strict.data[71..82], b"MCD-NSO-USB");
    }
}

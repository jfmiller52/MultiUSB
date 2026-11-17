use std::cmp::min;
use std::io::{self, Read, Seek, SeekFrom, Write};

/// Views a byte range of a larger block device as a standalone volume.
///
/// Used so FAT32 can be written through `\\.\PhysicalDriveN` at the partition
/// starting offset while the volume letter stays locked/dismounted.
pub struct PartitionView<T> {
    inner: T,
    base: u64,
    length: u64,
    pos: u64,
}

impl<T> PartitionView<T> {
    pub fn new(inner: T, base: u64, length: u64) -> Self {
        Self {
            inner,
            base,
            length,
            pos: 0,
        }
    }

    pub fn into_inner(self) -> T {
        self.inner
    }

    fn absolute(&self, relative: u64) -> io::Result<u64> {
        let abs = self
            .base
            .checked_add(relative)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "partition offset overflow"))?;
        Ok(abs)
    }
}

impl<T: Read + Seek> Read for PartitionView<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.length {
            return Ok(0);
        }
        let remaining = (self.length - self.pos) as usize;
        let take = min(buf.len(), remaining);
        let abs = self.absolute(self.pos)?;
        self.inner.seek(SeekFrom::Start(abs))?;
        let n = self.inner.read(&mut buf[..take])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<T: Write + Seek> Write for PartitionView<T> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        if self.pos >= self.length {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "write past end of partition",
            ));
        }
        let remaining = (self.length - self.pos) as usize;
        let take = min(data.len(), remaining);
        let abs = self.absolute(self.pos)?;
        self.inner.seek(SeekFrom::Start(abs))?;
        let n = self.inner.write(&data[..take])?;
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<T: Seek> Seek for PartitionView<T> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => self.length as i64 + n,
            SeekFrom::Current(n) => self.pos as i64 + n,
        };
        if new_pos < 0 || new_pos as u64 > self.length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek outside partition",
            ));
        }
        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::PartitionView;
    use std::io::{Read, Seek, SeekFrom, Write};

    #[test]
    fn partition_view_maps_relative_io_to_base_offset() {
        let mut disk = vec![0u8; 4096];
        {
            let mut view = PartitionView::new(std::io::Cursor::new(&mut disk[..]), 1024, 512);
            view.write_all(&[1, 2, 3, 4]).unwrap();
            view.seek(SeekFrom::Start(0)).unwrap();
            let mut buf = [0u8; 4];
            view.read_exact(&mut buf).unwrap();
            assert_eq!(buf, [1, 2, 3, 4]);
        }
        assert_eq!(&disk[1024..1028], &[1, 2, 3, 4]);
        assert_eq!(disk[0], 0);
    }
}

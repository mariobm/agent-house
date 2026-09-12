//! Bounded single-client NBD transport for the guest-disk experiment.
//! Export only through a protected local Unix socket. No multi-connection,
//! trim, write-zeroes, structured replies or online reconnect are advertised.
use crate::{Error, Volume};
use std::io::{self, Read, Write};

// Legacy EXPORT_NAME clients use the protocol default 32-MiB maximum.
// GO clients also receive this explicit limit. Never allocate beyond it.
pub const MAX_REQUEST: usize = 32 * 1024 * 1024;
const OPT_MAGIC: u64 = 0x49484156454f5054;
const FLAGS: u16 = 1 | 4 | 8; // HAS_FLAGS, SEND_FLUSH, SEND_FUA
const INVALID: u32 = 22;
const IO_ERROR: u32 = 5;

/// Sequential execution is the ordering barrier: flush/FUA cannot overtake any
/// earlier write. Flush success follows the selected backend durability contract
/// (local journal for LocalDisk, remote publication for Volume).
pub trait Disk {
    fn size(&self) -> u64;
    fn read(&mut self, offset: u64, data: &mut [u8]) -> crate::Result<()>;
    fn write(&mut self, offset: u64, data: &[u8]) -> crate::Result<()>;
    fn flush(&mut self) -> crate::Result<()>;
}
impl Disk for Volume {
    fn size(&self) -> u64 {
        self.size()
    }
    fn read(&mut self, offset: u64, data: &mut [u8]) -> crate::Result<()> {
        Volume::read(self, offset, data)
    }
    fn write(&mut self, offset: u64, data: &[u8]) -> crate::Result<()> {
        Volume::write(self, offset, data)
    }
    fn flush(&mut self) -> crate::Result<()> {
        self.commit().map(|_| ())
    }
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid NBD request")
}
fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes(bytes[at..at + 2].try_into().unwrap())
}
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap())
}
fn option_reply(stream: &mut impl Write, option: u32, kind: u32, data: &[u8]) -> io::Result<()> {
    stream.write_all(&0x3e889045565a9u64.to_be_bytes())?;
    stream.write_all(&option.to_be_bytes())?;
    stream.write_all(&kind.to_be_bytes())?;
    stream.write_all(&(data.len() as u32).to_be_bytes())?;
    stream.write_all(data)
}
fn handshake(stream: &mut (impl Read + Write), size: u64) -> io::Result<bool> {
    stream.write_all(&0x4e42444d41474943u64.to_be_bytes())?;
    stream.write_all(&OPT_MAGIC.to_be_bytes())?;
    stream.write_all(&3u16.to_be_bytes())?; // FIXED_NEWSTYLE, NO_ZEROES
    stream.flush()?;
    let mut client = [0; 4];
    stream.read_exact(&mut client)?;
    let flags = u32::from_be_bytes(client);
    if flags & !3 != 0 || flags & 1 == 0 {
        return Err(invalid());
    }
    for _ in 0..32 {
        let mut header = [0; 16];
        stream.read_exact(&mut header)?;
        if u64_at(&header, 0) != OPT_MAGIC {
            return Err(invalid());
        }
        let option = u32_at(&header, 8);
        let length = u32_at(&header, 12) as usize;
        if length > 4096 {
            return Err(invalid());
        }
        let mut data = vec![0; length];
        stream.read_exact(&mut data)?;
        match option {
            1 => {
                // EXPORT_NAME: only the unnamed, single-volume export
                if !data.is_empty() {
                    return Err(invalid());
                }
                stream.write_all(&size.to_be_bytes())?;
                stream.write_all(&FLAGS.to_be_bytes())?;
                if flags & 2 == 0 {
                    stream.write_all(&[0; 124])?;
                }
                stream.flush()?;
                return Ok(true);
            }
            2 if data.is_empty() => {
                option_reply(stream, option, 1, &[])?;
                stream.flush()?;
                return Ok(false);
            }
            6 | 7 => {
                if data.len() < 6 {
                    return Err(invalid());
                }
                let name_len = u32_at(&data, 0) as usize;
                if name_len > data.len() - 6 {
                    return Err(invalid());
                }
                let n = u16_at(&data, 4 + name_len) as usize;
                if data.len() != 6 + name_len + 2 * n {
                    return Err(invalid());
                }
                if name_len != 0 {
                    option_reply(stream, option, 0x80000006, &[])?;
                } else {
                    let mut info = 0u16.to_be_bytes().to_vec();
                    info.extend(size.to_be_bytes());
                    info.extend(FLAGS.to_be_bytes());
                    option_reply(stream, option, 3, &info)?;
                    let mut block = 3u16.to_be_bytes().to_vec();
                    block.extend(512u32.to_be_bytes());
                    block.extend(65536u32.to_be_bytes());
                    block.extend((MAX_REQUEST as u32).to_be_bytes());
                    option_reply(stream, option, 3, &block)?;
                    option_reply(stream, option, 1, &[])?;
                    stream.flush()?;
                    if option == 7 {
                        return Ok(true);
                    }
                }
            }
            _ => option_reply(stream, option, 0x80000001, &[])?,
        }
        stream.flush()?;
    }
    Err(invalid())
}

/// One connection owns the disk until disconnect. Caller must retire the handle
/// on disconnect, never reconnect a different client to pending volatile writes.
pub fn serve(stream: &mut (impl Read + Write), disk: &mut impl Disk) -> io::Result<()> {
    if !handshake(stream, disk.size())? {
        return Ok(());
    }
    loop {
        let mut header = [0; 28];
        stream.read_exact(&mut header)?;
        if u32_at(&header, 0) != 0x25609513 {
            return Err(invalid());
        }
        let flags = u16_at(&header, 4);
        let command = u16_at(&header, 6);
        let offset = u64_at(&header, 16);
        let length = u32_at(&header, 24) as usize;
        if length > MAX_REQUEST {
            return Err(invalid());
        }
        if command == 2 {
            return Ok(());
        } // DISC never implies a successful flush
        let mut data = vec![0; length];
        if command == 1 {
            stream.read_exact(&mut data)?;
        }
        let valid = match command {
            0 | 1 => {
                flags & !(if command == 1 { 1 } else { 0 }) == 0
                    && length > 0
                    && length.is_multiple_of(512)
                    && offset.is_multiple_of(512)
                    && offset
                        .checked_add(length as u64)
                        .is_some_and(|end| end <= disk.size())
            }
            3 => flags == 0 && offset == 0 && length == 0,
            _ => false,
        };
        let result = if !valid {
            Err(Error::InvalidInput)
        } else {
            match command {
                0 => disk.read(offset, &mut data),
                1 => disk.write(offset, &data).and_then(|()| {
                    if flags & 1 != 0 {
                        disk.flush()
                    } else {
                        Ok(())
                    }
                }),
                3 => disk.flush(),
                _ => unreachable!(),
            }
        };
        let errno = match result {
            Ok(()) => 0,
            Err(Error::InvalidInput) => INVALID,
            Err(_) => IO_ERROR,
        };
        stream.write_all(&0x67446698u32.to_be_bytes())?;
        stream.write_all(&errno.to_be_bytes())?;
        stream.write_all(&header[8..16])?;
        if command == 0 && errno == 0 {
            stream.write_all(&data)?;
        }
        stream.flush()?;
    }
}

#[cfg(test)]
mod tests;

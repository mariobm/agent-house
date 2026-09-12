use super::*;
use std::io::Cursor;
#[derive(Default)]
struct Wire {
    input: Cursor<Vec<u8>>,
    output: Vec<u8>,
}
impl Read for Wire {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.input.read(b)
    }
}
impl Write for Wire {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.output.extend(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct Fake {
    data: Vec<u8>,
    durable: Vec<u8>,
    fail: bool,
    events: Vec<&'static str>,
}
impl Fake {
    fn new() -> Self {
        Self {
            data: vec![0; 4096],
            durable: vec![0; 4096],
            fail: false,
            events: vec![],
        }
    }
}
impl Disk for Fake {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }
    fn read(&mut self, o: u64, b: &mut [u8]) -> crate::Result<()> {
        b.copy_from_slice(&self.data[o as usize..o as usize + b.len()]);
        Ok(())
    }
    fn write(&mut self, o: u64, b: &[u8]) -> crate::Result<()> {
        self.events.push("write");
        self.data[o as usize..o as usize + b.len()].copy_from_slice(b);
        Ok(())
    }
    fn flush(&mut self) -> crate::Result<()> {
        self.events.push("flush");
        if self.fail {
            Err(Error::Store)
        } else {
            self.durable.clone_from(&self.data);
            Ok(())
        }
    }
}
fn option(input: &mut Vec<u8>, opt: u32, data: &[u8]) {
    input.extend(OPT_MAGIC.to_be_bytes());
    input.extend(opt.to_be_bytes());
    input.extend((data.len() as u32).to_be_bytes());
    input.extend(data);
}
fn start() -> Vec<u8> {
    let mut b = 3u32.to_be_bytes().to_vec();
    option(&mut b, 1, &[]);
    b
}
fn request(input: &mut Vec<u8>, cmd: u16, flags: u16, offset: u64, len: u32) {
    input.extend(0x25609513u32.to_be_bytes());
    input.extend(flags.to_be_bytes());
    input.extend(cmd.to_be_bytes());
    input.extend(42u64.to_be_bytes());
    input.extend(offset.to_be_bytes());
    input.extend(len.to_be_bytes());
}
fn run(mut input: Vec<u8>, disk: &mut Fake) -> Wire {
    request(&mut input, 2, 0, 0, 0);
    let mut wire = Wire {
        input: Cursor::new(input),
        output: vec![],
    };
    serve(&mut wire, disk).unwrap();
    wire
}
#[test]
fn ordinary_write_is_volatile_but_flush_commits_before_ack() {
    let mut input = start();
    request(&mut input, 1, 0, 0, 512);
    input.extend(vec![7; 512]);
    let mut disk = Fake::new();
    run(input.clone(), &mut disk);
    assert_eq!(disk.durable[0], 0);
    request(&mut input, 3, 0, 0, 0);
    let out = run(input, &mut disk).output;
    assert_eq!(disk.durable[0], 7);
    assert_eq!(&disk.events[disk.events.len() - 2..], ["write", "flush"]);
    assert_eq!(u32_at(&out, 28 + 16 + 4), 0);
    assert_eq!(u16_at(&out, 26), FLAGS); // no trim/zeroes/multi-conn promises
}
#[test]
fn fua_and_flush_failures_never_return_success() {
    for fua in [true, false] {
        let mut input = start();
        request(&mut input, 1, u16::from(fua), 0, 512);
        input.extend(vec![8; 512]);
        if !fua {
            request(&mut input, 3, 0, 0, 0);
        }
        let mut disk = Fake::new();
        disk.fail = true;
        let out = run(input, &mut disk).output;
        let at = 28 + if fua { 0 } else { 16 };
        assert_eq!(u32_at(&out, at + 4), IO_ERROR);
        assert_eq!(disk.durable[0], 0);
        assert_eq!(disk.events, ["write", "flush"]);
    }
}
#[test]
fn unsupported_flags_ranges_and_trim_are_rejected_without_mutation() {
    let mut input = start();
    request(&mut input, 1, 2, 0, 512);
    input.extend(vec![1; 512]);
    request(&mut input, 1, 0, u64::MAX - 511, 512);
    input.extend(vec![1; 512]);
    request(&mut input, 4, 0, 0, 512);
    request(&mut input, 0, 0, 1, 512);
    let mut disk = Fake::new();
    let out = run(input, &mut disk).output;
    assert!(disk.events.is_empty());
    for i in 0..4 {
        assert_eq!(u32_at(&out, 28 + 16 * i + 4), INVALID);
    }
}
#[test]
fn oversized_requests_disconnect_before_reading_payload() {
    let mut input = start();
    request(&mut input, 1, 0, 0, MAX_REQUEST as u32 + 1);
    let mut wire = Wire {
        input: Cursor::new(input),
        output: vec![],
    };
    let mut disk = Fake::new();
    assert_eq!(
        serve(&mut wire, &mut disk).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert!(disk.events.is_empty());
}
#[test]
fn go_negotiates_size_and_limits_and_unknown_option_does_not_desync() {
    let mut input = 3u32.to_be_bytes().to_vec();
    option(&mut input, 999, b"ignored");
    option(&mut input, 7, &[0, 0, 0, 0, 0, 1, 0, 3]);
    let out = run(input, &mut Fake::new()).output;
    assert_eq!(u32_at(&out, 18 + 12), 0x80000001);
    let first = 18 + 20;
    assert_eq!(u32_at(&out, first + 12), 3);
    assert_eq!(u64_at(&out, first + 22), 4096);
    let block = first + 20 + 12;
    assert_eq!(u16_at(&out, block + 20), 3);
    assert_eq!(u32_at(&out, block + 22), 512);
    assert_eq!(u32_at(&out, block + 30), MAX_REQUEST as u32);
}

#[test]
fn legacy_export_accepts_requests_larger_than_one_mib() {
    let length = 2 * 1024 * 1024;
    let mut disk = Fake::new();
    disk.data.resize(length, 0);
    disk.durable.resize(length, 0);
    let mut input = start();
    request(&mut input, 1, 1, 0, length as u32);
    input.extend(vec![3; length]);
    let out = run(input, &mut disk).output;
    assert_eq!(u32_at(&out, 28 + 4), 0);
    assert!(disk.durable.iter().all(|b| *b == 3));
}

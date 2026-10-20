//! A streaming zip writer for folder downloads: entries are stored (not compressed), written
//! front to back with data descriptors (the CRC is known only after the data), so the output
//! never needs seeking. ZIP64 records are added where a size, an offset or the number of
//! entries needs them; names are UTF-8; times are kept as DOS time plus the Unix (0x5455) and
//! NTFS (0x000a) extra fields.

use std::io::{self, Read, Write};

const LOCAL_SIG: u32 = 0x0403_4b50;
const DESCRIPTOR_SIG: u32 = 0x0807_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const ZIP64_EOCD_SIG: u32 = 0x0606_4b50;
const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;

/// General purpose flags: sizes and CRC in a data descriptor, UTF-8 names.
const FLAG_DESCRIPTOR: u16 = 0x0008;
const FLAG_UTF8: u16 = 0x0800;
const VERSION_ZIP64: u16 = 45;
const VERSION_DEFAULT: u16 = 20;
/// Made by: Unix (3), spec 4.5.
const MADE_BY: u16 = (3 << 8) | VERSION_ZIP64;

/// The CRC-32 of zip (IEEE 802.3), table driven.
#[derive(Clone, Copy)]
pub(crate) struct Crc32(u32);

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

impl Crc32 {
    pub fn new() -> Self {
        Crc32(0xFFFF_FFFF)
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut c = self.0;
        for &b in data {
            c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }

    pub fn finish(self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

/// What an entry needs in the central directory.
struct Central {
    name: Vec<u8>,
    dir: bool,
    crc: u32,
    size: u64,
    offset: u64,
    mtime_s: i64,
    /// Written with a data descriptor (and ZIP64 sizes in it).
    descriptor: bool,
    zip64_local: bool,
}

/// Counts what goes through.
struct Counting<W> {
    inner: W,
    written: u64,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub(crate) struct ZipWriter<W: Write> {
    out: Counting<W>,
    entries: Vec<Central>,
    /// Sizes and offsets from this value on use ZIP64 fields (0xFFFF_FFFF normally; tests
    /// lower it to exercise the ZIP64 paths with small files).
    zip64_from: u64,
    /// Entry counts from this value on need ZIP64 end records (0xFFFF normally).
    zip64_entries_from: usize,
}

/// Seconds since 1970 → DOS date and time (UTC; DOS times have no zone). Clamped to
/// 1980..=2107.
fn dos_time(mtime_s: i64) -> (u16, u16) {
    let days = mtime_s.div_euclid(86_400);
    let secs = mtime_s.rem_euclid(86_400);
    // Civil from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    if year < 1980 {
        return ((1 << 5) | 1, 0);
    }
    if year > 2107 {
        return ((127 << 9) | (12 << 5) | 31, (23 << 11) | (59 << 5) | 29);
    }
    let date = (((year - 1980) as u16) << 9) | ((month as u16) << 5) | day as u16;
    let time = (((secs / 3600) as u16) << 11)
        | ((((secs / 60) % 60) as u16) << 5)
        | ((secs % 60) / 2) as u16;
    (date, time)
}

/// Seconds since 1970 → Windows FILETIME (100 ns since 1601).
fn filetime(mtime_s: i64) -> u64 {
    ((mtime_s + 11_644_473_600).max(0) as u64) * 10_000_000
}

fn put16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}

impl<W: Write> ZipWriter<W> {
    pub fn new(out: W) -> Self {
        ZipWriter {
            out: Counting {
                inner: out,
                written: 0,
            },
            entries: Vec::new(),
            zip64_from: 0xFFFF_FFFF,
            zip64_entries_from: 0xFFFF,
        }
    }

    #[cfg(test)]
    fn with_zip64_from(mut self, bytes: u64, entries: usize) -> Self {
        self.zip64_from = bytes;
        self.zip64_entries_from = entries;
        self
    }

    /// A folder entry; `name` is the archive path without the trailing slash.
    pub fn add_dir(&mut self, name: &str, mtime_s: i64) -> io::Result<()> {
        let name = format!("{}/", name.trim_end_matches('/')).into_bytes();
        let offset = self.out.written;
        let zip64 = offset >= self.zip64_from;
        self.write_local(&name, mtime_s, false, zip64)?;
        self.entries.push(Central {
            name,
            dir: true,
            crc: 0,
            size: 0,
            offset,
            mtime_s,
            descriptor: false,
            zip64_local: false,
        });
        Ok(())
    }

    /// A file of exactly `size` bytes from `data`. Fails (with the archive unusable) if `data`
    /// ends early; bytes beyond `size` are not read. `progress` gets the bytes copied so far.
    pub fn add_file(
        &mut self,
        name: &str,
        mtime_s: i64,
        size: u64,
        data: &mut dyn Read,
        progress: &mut dyn FnMut(u64),
    ) -> io::Result<()> {
        let name = name.as_bytes().to_vec();
        let offset = self.out.written;
        let zip64 = size >= self.zip64_from;
        self.write_local(&name, mtime_s, true, zip64)?;
        let mut crc = Crc32::new();
        let mut left = size;
        let mut buf = vec![0u8; 256 << 10];
        while left > 0 {
            let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
            let n = match data.read(&mut buf[..want]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the file got shorter while it was zipped",
                    ));
                }
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            crc.update(&buf[..n]);
            self.out.write_all(&buf[..n])?;
            left -= n as u64;
            progress(size - left);
        }
        let crc = crc.finish();
        let mut d = Vec::with_capacity(24);
        put32(&mut d, DESCRIPTOR_SIG);
        put32(&mut d, crc);
        if zip64 {
            put64(&mut d, size);
            put64(&mut d, size);
        } else {
            put32(&mut d, size as u32);
            put32(&mut d, size as u32);
        }
        self.out.write_all(&d)?;
        self.entries.push(Central {
            name,
            dir: false,
            crc,
            size,
            offset,
            mtime_s,
            descriptor: true,
            zip64_local: zip64,
        });
        Ok(())
    }

    fn write_local(
        &mut self,
        name: &[u8],
        mtime_s: i64,
        descriptor: bool,
        zip64: bool,
    ) -> io::Result<()> {
        let (date, time) = dos_time(mtime_s);
        let mut h = Vec::with_capacity(30 + name.len() + 20);
        put32(&mut h, LOCAL_SIG);
        put16(
            &mut h,
            if zip64 {
                VERSION_ZIP64
            } else {
                VERSION_DEFAULT
            },
        );
        put16(
            &mut h,
            FLAG_UTF8 | if descriptor { FLAG_DESCRIPTOR } else { 0 },
        );
        put16(&mut h, 0); // stored
        put16(&mut h, time);
        put16(&mut h, date);
        put32(&mut h, 0); // crc: in the descriptor
        let sizes = if zip64 { 0xFFFF_FFFF } else { 0 };
        put32(&mut h, sizes);
        put32(&mut h, sizes);
        put16(&mut h, name.len() as u16);
        put16(&mut h, if zip64 { 20 } else { 0 });
        h.extend_from_slice(name);
        if zip64 {
            // ZIP64 extra with zero sizes: the real ones follow in the (8-byte) descriptor.
            put16(&mut h, 0x0001);
            put16(&mut h, 16);
            put64(&mut h, 0);
            put64(&mut h, 0);
        }
        self.out.write_all(&h)
    }

    /// Writes the central directory and end records; returns the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        let cd_start = self.out.written;
        for e in &self.entries {
            let big_size = e.size >= self.zip64_from || e.zip64_local;
            let big_offset = e.offset >= self.zip64_from;
            let (date, time) = dos_time(e.mtime_s);
            let mut zip64 = Vec::new();
            if big_size {
                put64(&mut zip64, e.size);
                put64(&mut zip64, e.size);
            }
            if big_offset {
                put64(&mut zip64, e.offset);
            }
            let mut extra = Vec::new();
            if !zip64.is_empty() {
                put16(&mut extra, 0x0001);
                put16(&mut extra, zip64.len() as u16);
                extra.extend_from_slice(&zip64);
            }
            // Extended timestamp: modification time.
            put16(&mut extra, 0x5455);
            put16(&mut extra, 5);
            extra.push(1);
            put32(&mut extra, e.mtime_s.clamp(0, u32::MAX as i64) as u32);
            // NTFS: modification, access and creation time.
            put16(&mut extra, 0x000a);
            put16(&mut extra, 32);
            put32(&mut extra, 0);
            put16(&mut extra, 1);
            put16(&mut extra, 24);
            let ft = filetime(e.mtime_s);
            put64(&mut extra, ft);
            put64(&mut extra, ft);
            put64(&mut extra, ft);

            let mut c = Vec::with_capacity(46 + e.name.len() + extra.len());
            put32(&mut c, CENTRAL_SIG);
            put16(&mut c, MADE_BY);
            put16(
                &mut c,
                if big_size || big_offset {
                    VERSION_ZIP64
                } else {
                    VERSION_DEFAULT
                },
            );
            put16(
                &mut c,
                FLAG_UTF8 | if e.descriptor { FLAG_DESCRIPTOR } else { 0 },
            );
            put16(&mut c, 0);
            put16(&mut c, time);
            put16(&mut c, date);
            put32(&mut c, e.crc);
            let size32 = if big_size { 0xFFFF_FFFF } else { e.size as u32 };
            put32(&mut c, size32);
            put32(&mut c, size32);
            put16(&mut c, e.name.len() as u16);
            put16(&mut c, extra.len() as u16);
            put16(&mut c, 0); // comment
            put16(&mut c, 0); // disk
            put16(&mut c, 0); // internal attributes
            let mode: u32 = if e.dir { 0o040_755 } else { 0o100_644 };
            put32(&mut c, (mode << 16) | if e.dir { 0x10 } else { 0 });
            put32(
                &mut c,
                if big_offset {
                    0xFFFF_FFFF
                } else {
                    e.offset as u32
                },
            );
            c.extend_from_slice(&e.name);
            c.extend_from_slice(&extra);
            self.out.write_all(&c)?;
        }
        let cd_end = self.out.written;
        let cd_size = cd_end - cd_start;
        let count = self.entries.len();
        let need64 = count >= self.zip64_entries_from
            || cd_start >= self.zip64_from
            || cd_size >= self.zip64_from;
        let mut t = Vec::new();
        if need64 {
            put32(&mut t, ZIP64_EOCD_SIG);
            put64(&mut t, 44);
            put16(&mut t, MADE_BY);
            put16(&mut t, VERSION_ZIP64);
            put32(&mut t, 0);
            put32(&mut t, 0);
            put64(&mut t, count as u64);
            put64(&mut t, count as u64);
            put64(&mut t, cd_size);
            put64(&mut t, cd_start);
            put32(&mut t, ZIP64_LOCATOR_SIG);
            put32(&mut t, 0);
            put64(&mut t, cd_end);
            put32(&mut t, 1);
        }
        put32(&mut t, EOCD_SIG);
        put16(&mut t, 0);
        put16(&mut t, 0);
        let count16 = if need64 { 0xFFFF } else { count as u16 };
        put16(&mut t, count16);
        put16(&mut t, count16);
        put32(&mut t, if need64 { 0xFFFF_FFFF } else { cd_size as u32 });
        put32(&mut t, if need64 { 0xFFFF_FFFF } else { cd_start as u32 });
        put16(&mut t, 0);
        self.out.write_all(&t)?;
        self.out.flush()?;
        Ok(self.out.inner)
    }
}

/// A minimal zip reader for tests: every entry (name, data) via the central directory,
/// checking local headers, descriptors and CRCs.
#[cfg(test)]
pub(crate) fn read_zip(zip: &[u8]) -> Vec<(String, Option<Vec<u8>>, u32)> {
    let u16_at = |o: usize| u16::from_le_bytes([zip[o], zip[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes(zip[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(zip[o..o + 8].try_into().unwrap());
    let eocd = zip.len() - 22;
    assert_eq!(u32_at(eocd), EOCD_SIG);
    let (mut count, mut cd_off) = (u16_at(eocd + 10) as u64, u32_at(eocd + 16) as u64);
    if count == 0xFFFF || cd_off == 0xFFFF_FFFF {
        let loc = eocd - 20;
        assert_eq!(u32_at(loc), ZIP64_LOCATOR_SIG);
        let z = u64_at(loc + 8) as usize;
        assert_eq!(u32_at(z), ZIP64_EOCD_SIG);
        count = u64_at(z + 32);
        cd_off = u64_at(z + 48);
    }
    let mut out = Vec::new();
    let mut p = cd_off as usize;
    for _ in 0..count {
        assert_eq!(u32_at(p), CENTRAL_SIG);
        let flags = u16_at(p + 8);
        assert!(flags & FLAG_UTF8 != 0);
        let crc = u32_at(p + 16);
        let mut size = u32_at(p + 24) as u64;
        let name_len = u16_at(p + 28) as usize;
        let extra_len = u16_at(p + 30) as usize;
        let mut offset = u32_at(p + 42) as u64;
        let name = String::from_utf8(zip[p + 46..p + 46 + name_len].to_vec()).unwrap();
        let mut e = p + 46 + name_len;
        let extra_end = e + extra_len;
        let mut saw_ut = false;
        while e < extra_end {
            let (id, len) = (u16_at(e), u16_at(e + 2) as usize);
            let mut f = e + 4;
            if id == 0x0001 {
                if size == 0xFFFF_FFFF {
                    size = u64_at(f);
                    f += 16;
                }
                if offset == 0xFFFF_FFFF {
                    offset = u64_at(f);
                }
            }
            saw_ut |= id == 0x5455;
            e += 4 + len;
        }
        assert!(saw_ut);
        let l = offset as usize;
        assert_eq!(u32_at(l), LOCAL_SIG);
        let local_name_len = u16_at(l + 26) as usize;
        let local_extra = u16_at(l + 28) as usize;
        assert_eq!(&zip[l + 30..l + 30 + local_name_len], name.as_bytes());
        let data_at = l + 30 + local_name_len + local_extra;
        let data = if name.ends_with('/') {
            None
        } else {
            let data = zip[data_at..data_at + size as usize].to_vec();
            let mut c = Crc32::new();
            c.update(&data);
            assert_eq!(c.finish(), crc, "{name}: crc");
            let d = data_at + size as usize;
            assert_eq!(u32_at(d), DESCRIPTOR_SIG);
            assert_eq!(u32_at(d + 4), crc);
            Some(data)
        };
        out.push((name, data, crc));
        p = extra_end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_values() {
        let mut c = Crc32::new();
        c.update(b"123456789");
        assert_eq!(c.finish(), 0xCBF4_3926);
        assert_eq!(Crc32::new().finish(), 0);
    }

    #[test]
    fn dos_times() {
        // 2024-10-17 10:13:20 UTC
        let (date, time) = dos_time(1_729_160_000);
        assert_eq!(date >> 9, 2024 - 1980);
        assert_eq!((date >> 5) & 0xF, 10);
        assert_eq!(date & 0x1F, 17);
        assert_eq!(time >> 11, 10);
        assert_eq!((time >> 5) & 0x3F, 13);
        assert_eq!(time & 0x1F, 10);
        assert_eq!(dos_time(0).0, (1 << 5) | 1, "before 1980");
    }

    fn build(zip64_from: u64, entries_from: usize) -> Vec<u8> {
        let mut z = ZipWriter::new(Vec::new()).with_zip64_from(zip64_from, entries_from);
        z.add_dir("Photos", 1_729_160_000).unwrap();
        let mut seen = 0;
        z.add_file(
            "Photos/a.txt",
            1_729_160_000,
            5,
            &mut &b"hello"[..],
            &mut |n| seen = n,
        )
        .unwrap();
        assert_eq!(seen, 5);
        z.add_file("Photos/empty", 0, 0, &mut &b""[..], &mut |_| {})
            .unwrap();
        z.add_dir("Photos/empty dir", 0).unwrap();
        z.add_file(
            "Photos/ünï.bin",
            1,
            300_000,
            &mut std::io::repeat(7).take(300_000),
            &mut |_| {},
        )
        .unwrap();
        z.finish().unwrap()
    }

    fn check(zip: &[u8]) {
        let entries = read_zip(zip);
        let names: Vec<&str> = entries.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(
            names,
            [
                "Photos/",
                "Photos/a.txt",
                "Photos/empty",
                "Photos/empty dir/",
                "Photos/ünï.bin"
            ]
        );
        assert_eq!(entries[1].1.as_deref(), Some(&b"hello"[..]));
        assert_eq!(entries[2].1.as_deref(), Some(&b""[..]));
        assert_eq!(entries[4].1.as_ref().unwrap().len(), 300_000);
    }

    #[test]
    fn plain_archive() {
        let zip = build(0xFFFF_FFFF, 0xFFFF);
        check(&zip);
        if let Ok(dir) = std::env::var("RD_ZIP_SAMPLES") {
            std::fs::write(format!("{dir}/plain.zip"), &zip).unwrap();
        }
    }

    #[test]
    fn zip64_archive() {
        // Every size and offset from 1 byte on, and the entry count, go through ZIP64 fields.
        let zip = build(1, 1);
        check(&zip);
        if let Ok(dir) = std::env::var("RD_ZIP_SAMPLES") {
            std::fs::write(format!("{dir}/zip64.zip"), &zip).unwrap();
        }
    }

    #[test]
    fn short_data_fails() {
        let mut z = ZipWriter::new(Vec::new());
        let e = z
            .add_file("x", 0, 10, &mut &b"abc"[..], &mut |_| {})
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
        // Longer data is cut at the size.
        let mut z = ZipWriter::new(Vec::new());
        z.add_file("y", 0, 2, &mut &b"abcdef"[..], &mut |_| {})
            .unwrap();
        let zip = z.finish().unwrap();
        assert_eq!(read_zip(&zip)[0].1.as_deref(), Some(&b"ab"[..]));
    }
}

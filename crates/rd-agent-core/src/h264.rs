//! H.264 helpers: avcC parsing/building, WebCodecs codec strings, Annex-B <-> AVCC.

use bytes::Bytes;

const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;

/// Profiles whose avcC carries the chroma format / bit depth extension (ISO/IEC 14496-15).
const HIGH_PROFILES: [u8; 4] = [100, 110, 122, 244];

/// WebCodecs codec string from an avcC record: `avc1.PPCCLL` (profile, constraint flags, level).
/// Returns None if the record is too short or not version 1.
pub fn codec_string_from_avcc(avcc: &[u8]) -> Option<String> {
    if avcc.len() < 4 || avcc[0] != 1 {
        return None;
    }
    Some(format!(
        "avc1.{:02x}{:02x}{:02x}",
        avcc[1], avcc[2], avcc[3]
    ))
}

/// Build an avcC record from one SPS and one PPS (NAL units without start codes / length).
///
/// Returns None if `sps` / `pps` are not an SPS / PPS NAL unit or longer than 65535 bytes. For
/// the High profiles the chroma format / bit depth extension is appended when the SPS can be
/// parsed that far (it is optional for WebCodecs).
pub fn build_avcc(sps: &[u8], pps: &[u8]) -> Option<Bytes> {
    if sps.len() < 4 || nal_type(sps) != Some(NAL_SPS) || nal_type(pps) != Some(NAL_PPS) {
        return None;
    }
    let sps_len = u16::try_from(sps.len()).ok()?;
    let pps_len = u16::try_from(pps.len()).ok()?;
    let profile = sps[1];
    let mut out = Vec::with_capacity(16 + sps.len() + pps.len());
    // version, profile, profile compatibility, level, 6 reserved bits + lengthSizeMinusOne = 3,
    // 3 reserved bits + one SPS.
    out.extend_from_slice(&[1, profile, sps[2], sps[3], 0xfc | 3, 0xe0 | 1]);
    out.extend_from_slice(&sps_len.to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1);
    out.extend_from_slice(&pps_len.to_be_bytes());
    out.extend_from_slice(pps);
    if HIGH_PROFILES.contains(&profile)
        && let Some(ext) = high_profile_extension(sps)
    {
        out.extend_from_slice(&ext);
    }
    Some(Bytes::from(out))
}

/// Extract SPS NAL units from an avcC record.
///
/// Returns an empty list for anything that is not a version 1 record; stops at the first SPS
/// that does not fit in the record.
pub fn avcc_sps_list(avcc: &[u8]) -> Vec<&[u8]> {
    if avcc.len() < 6 || avcc[0] != 1 {
        return Vec::new();
    }
    let count = usize::from(avcc[5] & 0x1f);
    let mut rest = &avcc[6..];
    let mut list = Vec::with_capacity(count);
    for _ in 0..count {
        let Some((nal, tail)) = split_u16_prefixed(rest) else {
            break;
        };
        if !nal.is_empty() {
            list.push(nal);
        }
        rest = tail;
    }
    list
}

/// Convert Annex-B (start codes) to AVCC (4-byte lengths), dropping SPS/PPS/AUD NALs.
///
/// Accepts 3- and 4-byte start codes; bytes before the first start code and zero padding
/// between NAL units are ignored. Input without any start code yields an empty buffer.
pub fn annexb_to_avcc(annexb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(annexb.len() + 16);
    for nal in annexb_nals(annexb) {
        if matches!(nal_type(nal), Some(NAL_SPS | NAL_PPS | NAL_AUD)) {
            continue;
        }
        // An Annex-B buffer handed to us is far below 4 GiB; anything else is not a frame.
        let Ok(len) = u32::try_from(nal.len()) else {
            continue;
        };
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// Iterate NAL units of an Annex-B byte stream (3- or 4-byte start codes). Bytes before the
/// first start code, trailing zero bytes and empty NAL units are skipped.
pub fn annexb_nals(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut next = find_start_code(data, 0);
    std::iter::from_fn(move || {
        loop {
            let start = next?;
            let end = find_start_code(data, start);
            next = end;
            // The NAL ends where the next start code begins (`end - 3`); the extra zero of a
            // 4-byte start code (and any trailing_zero_8bits) is trimmed: a NAL unit never ends
            // in 0x00.
            let until = end.map_or(data.len(), |e| e - 3);
            let nal = trim_trailing_zeros(&data[start..until]);
            if !nal.is_empty() {
                return Some(nal);
            }
        }
    })
}

/// Iterate NAL units of an AVCC buffer with 4-byte length prefixes.
pub fn avcc_nals(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = data;
    std::iter::from_fn(move || {
        if rest.len() < 4 {
            return None;
        }
        let n = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        if rest.len() < 4 + n {
            return None;
        }
        let nal = &rest[4..4 + n];
        rest = &rest[4 + n..];
        Some(nal)
    })
}

fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1f)
}

/// Index of the first byte after the next `00 00 01` at or after `from`.
fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    data.get(from..)?
        .windows(3)
        .position(|w| w == [0, 0, 1])
        .map(|pos| from + pos + 3)
}

fn trim_trailing_zeros(nal: &[u8]) -> &[u8] {
    let len = nal.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    &nal[..len]
}

/// Split `[len u16 BE][len bytes]` off the front of `data`.
fn split_u16_prefixed(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let (len, rest) = data.split_first_chunk::<2>()?;
    let len = usize::from(u16::from_be_bytes(*len));
    (rest.len() >= len).then(|| rest.split_at(len))
}

/// The 4-byte High profile avcC extension: chroma_format, bit_depth_luma_minus8,
/// bit_depth_chroma_minus8 (each with its reserved bits set) and no SPS extensions.
/// None if the SPS is truncated or carries out-of-range values.
fn high_profile_extension(sps: &[u8]) -> Option<[u8; 4]> {
    let rbsp = unescape_rbsp(sps.get(1..)?);
    let mut bits = BitReader::new(&rbsp);
    bits.skip(24)?; // profile_idc, constraint flags, level_idc
    let sps_id = bits.ue()?;
    let chroma_format_idc = bits.ue()?;
    if sps_id > 31 || chroma_format_idc > 3 {
        return None;
    }
    if chroma_format_idc == 3 {
        bits.skip(1)?; // separate_colour_plane_flag
    }
    let luma = bits.ue()?;
    let chroma = bits.ue()?;
    if luma > 6 || chroma > 6 {
        return None;
    }
    // All three values are range-checked above, so the casts are lossless.
    Some([
        0xfc | chroma_format_idc as u8,
        0xf8 | luma as u8,
        0xf8 | chroma as u8,
        0,
    ])
}

/// Remove emulation-prevention bytes (`00 00 03` -> `00 00`).
fn unescape_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// MSB-first bit reader with Exp-Golomb decoding.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(u32::from(bit))
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        if self.pos + n > self.data.len() * 8 {
            return None;
        }
        self.pos += n;
        Some(())
    }

    /// Unsigned Exp-Golomb `ue(v)` (values up to 2^31 - 2).
    fn ue(&mut self) -> Option<u32> {
        let mut leading_zeros = 0;
        while self.bit()? == 0 {
            leading_zeros += 1;
            if leading_zeros > 30 {
                return None;
            }
        }
        let mut suffix = 0;
        for _ in 0..leading_zeros {
            suffix = (suffix << 1) | self.bit()?;
        }
        Some((1 << leading_zeros) - 1 + suffix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// `(name, sps, pps, avcC)` from real encoder output: 1728x1118 testsrc encoded by ffmpeg
    /// (libx264 / h264_videotoolbox), avcC as written by ffmpeg's MP4 muxer.
    const SAMPLES: [(&str, &str, &str, &str); 5] = [
        (
            "x264 High 4.2",
            "6764002aacd9406c0237eb0110000003001000000780f1831960",
            "68ebe3cb22c0",
            "0164002affe1001a6764002aacd9406c0237eb0110000003001000000780f183196001000668ebe3cb22c0fdf8f800",
        ),
        (
            "x264 Constrained Baseline 4.2",
            "6742c02ad9006c0237eb0110000003001000000780f1832480",
            "68cb83cb20",
            "0142c02affe100196742c02ad9006c0237eb0110000003001000000780f183248001000568cb83cb20",
        ),
        (
            "x264 High 10",
            "676e002aa6cd9406c0237eb011000003000100000300780f183196",
            "68ebe3cb22c0",
            "016e002affe1001b676e002aa6cd9406c0237eb011000003000100000300780f18319601000668ebe3cb22c0fdfafa00",
        ),
        (
            "x264 High 4:4:4 Predictive",
            "67f4002a919b280d8046fde022000003000200000300f01e30632c",
            "68ebe3c44844",
            "01f4002affe1001b67f4002a919b280d8046fde022000003000200000300f01e30632c01000668ebe3c44844fff8f800",
        ),
        (
            "VideoToolbox High 4.0",
            "27640028ac56806c0237e9",
            "28ee3cb0",
            "01640028ffe1000b27640028ac56806c0237e901000428ee3cb0fdf8f800",
        ),
    ];

    #[test]
    fn build_avcc_matches_reference_muxer() {
        for (name, sps, pps, avcc) in SAMPLES {
            let built = build_avcc(&hex(sps), &hex(pps)).expect(name);
            assert_eq!(built.as_ref(), hex(avcc).as_slice(), "{name}");
        }
    }

    #[test]
    fn codec_strings() {
        let expected = [
            "avc1.64002a",
            "avc1.42c02a",
            "avc1.6e002a",
            "avc1.f4002a",
            "avc1.640028",
        ];
        for ((name, sps, pps, _), codec) in SAMPLES.iter().zip(expected) {
            let avcc = build_avcc(&hex(sps), &hex(pps)).expect(name);
            assert_eq!(
                codec_string_from_avcc(&avcc).as_deref(),
                Some(codec),
                "{name}"
            );
        }
        assert_eq!(codec_string_from_avcc(&[1, 0x64, 0]), None);
        assert_eq!(codec_string_from_avcc(&[2, 0x64, 0, 0x28]), None);
    }

    #[test]
    fn build_avcc_rejects_bad_input() {
        let (_, sps, pps, _) = SAMPLES[0];
        let (sps, pps) = (hex(sps), hex(pps));
        assert!(build_avcc(&sps[..3], &pps).is_none(), "too short");
        assert!(build_avcc(&pps, &sps).is_none(), "swapped");
        assert!(build_avcc(&sps, &[]).is_none(), "empty pps");
        let mut huge = sps.clone();
        huge.resize(70_000, 0x55);
        assert!(build_avcc(&huge, &pps).is_none(), "longer than u16");
    }

    #[test]
    fn high_profile_extension_is_omitted_when_sps_is_truncated() {
        // High profile header only: profile/level present, ue() fields missing.
        let sps = [0x67, 0x64, 0x00, 0x28];
        let pps = [0x68, 0xee, 0x3c, 0xb0];
        let avcc = build_avcc(&sps, &pps).unwrap();
        assert_eq!(avcc.len(), 6 + 2 + sps.len() + 1 + 2 + pps.len());
        assert_eq!(avcc_sps_list(&avcc), [&sps[..]]);
    }

    #[test]
    fn sps_list_from_built_and_multi_sps_records() {
        for (name, sps, pps, _) in SAMPLES {
            let avcc = build_avcc(&hex(sps), &hex(pps)).expect(name);
            assert_eq!(avcc_sps_list(&avcc), [hex(sps).as_slice()], "{name}");
        }
        let record = [
            1, 0x64, 0, 0x28, 0xff, 0xe2, // two SPS
            0, 2, 0x67, 0xaa, //
            0, 3, 0x67, 0xbb, 0xcc, //
            1, 0, 1, 0x68,
        ];
        assert_eq!(
            avcc_sps_list(&record),
            [&[0x67, 0xaa][..], &[0x67, 0xbb, 0xcc]]
        );
    }

    #[test]
    fn sps_list_handles_malformed_records() {
        assert!(avcc_sps_list(&[]).is_empty());
        assert!(avcc_sps_list(&[1, 0x64, 0, 0x28, 0xff]).is_empty());
        assert!(avcc_sps_list(&[0, 0x64, 0, 0x28, 0xff, 0xe1, 0, 1, 0x67]).is_empty());
        // Second SPS claims 255 bytes: only the first is returned.
        let truncated = [1, 0x64, 0, 0x28, 0xff, 0xe2, 0, 1, 0x67, 0, 0xff, 0x67];
        assert_eq!(avcc_sps_list(&truncated), [&[0x67][..]]);
        // Length prefix itself cut off.
        assert!(avcc_sps_list(&[1, 0x64, 0, 0x28, 0xff, 0xe1, 0]).is_empty());
    }

    #[test]
    fn annexb_to_avcc_mixed_start_codes() {
        let aud = [0x09, 0xf0];
        let sps = hex(SAMPLES[0].1);
        let pps = hex(SAMPLES[0].2);
        let sei = [0x06, 0x05, 0x01, 0x80];
        let idr = [0x65, 0x88, 0x84, 0x00, 0x00, 0x03, 0x01, 0x21];
        let slice = [0x41, 0x9a, 0x02];
        let mut annexb = vec![0x00]; // leading_zero_8bits
        for (nal, start_code) in [
            (&aud[..], &[0, 0, 0, 1][..]),
            (&sps, &[0, 0, 0, 1]),
            (&pps, &[0, 0, 1]),
            (&sei, &[0, 0, 1]),
            (&idr, &[0, 0, 0, 1]),
            (&slice, &[0, 0, 1]),
        ] {
            annexb.extend_from_slice(start_code);
            annexb.extend_from_slice(nal);
        }
        annexb.extend_from_slice(&[0, 0]); // trailing_zero_8bits
        let avcc = annexb_to_avcc(&annexb);
        let nals: Vec<&[u8]> = avcc_nals(&avcc).collect();
        assert_eq!(nals, [&sei[..], &idr, &slice]);
        let mut expected = vec![0, 0, 0, 4];
        expected.extend_from_slice(&sei);
        expected.extend_from_slice(&[0, 0, 0, 8]);
        expected.extend_from_slice(&idr);
        expected.extend_from_slice(&[0, 0, 0, 3]);
        expected.extend_from_slice(&slice);
        assert_eq!(avcc, expected);
    }

    #[test]
    fn annexb_nals_edge_cases() {
        assert_eq!(annexb_nals(&[]).count(), 0);
        assert_eq!(annexb_nals(&[0x65, 0x88]).count(), 0, "no start code");
        assert_eq!(annexb_nals(&[0, 0, 1]).count(), 0, "start code only");
        // Consecutive start codes (empty NALs) and garbage before the first one.
        let data = [
            0xaa, 0, 0, 1, 0, 0, 0, 1, 0x41, 0x01, 0, 0, 1, 0, 0, 1, 0x41, 0x02, 0, 0,
        ];
        let nals: Vec<&[u8]> = annexb_nals(&data).collect();
        assert_eq!(nals, [&[0x41, 0x01][..], &[0x41, 0x02]]);
        // A start code split across the end is just trailing bytes.
        let nals: Vec<&[u8]> = annexb_nals(&[0, 0, 1, 0x41, 0x05, 0, 0]).collect();
        assert_eq!(nals, [&[0x41, 0x05][..]]);
    }

    #[test]
    fn annexb_without_slices_is_empty() {
        let sps = hex(SAMPLES[4].1);
        let pps = hex(SAMPLES[4].2);
        let mut annexb = vec![0, 0, 0, 1];
        annexb.extend_from_slice(&sps);
        annexb.extend_from_slice(&[0, 0, 0, 1]);
        annexb.extend_from_slice(&pps);
        assert!(annexb_to_avcc(&annexb).is_empty());
        assert!(annexb_to_avcc(&[0x65, 0x88]).is_empty());
    }

    #[test]
    fn rbsp_unescaping() {
        assert_eq!(unescape_rbsp(&[0, 0, 3, 1]), [0, 0, 1]);
        assert_eq!(unescape_rbsp(&[0, 0, 3, 0, 0, 3]), [0, 0, 0, 0]);
        assert_eq!(unescape_rbsp(&[0, 3, 0, 0, 3]), [0, 3, 0, 0]);
        assert_eq!(unescape_rbsp(&[0, 0, 0, 3]), [0, 0, 0]);
    }

    #[test]
    fn exp_golomb() {
        // 1 | 010 | 011 | 00100 | 00101 -> 0, 1, 2, 3, 4
        let data = [0b1010_0110, 0b0100_0010, 0b1000_0000];
        let mut r = BitReader::new(&data);
        let values: Vec<u32> = (0..5).map(|_| r.ue().unwrap()).collect();
        assert_eq!(values, [0, 1, 2, 3, 4]);
        assert_eq!(
            BitReader::new(&[0, 0, 0, 0, 0]).ue(),
            None,
            "too many leading zeros"
        );
        assert_eq!(
            BitReader::new(&[0b0000_0001]).ue(),
            None,
            "suffix truncated"
        );
        assert_eq!(BitReader::new(&[0xff]).skip(9), None);
    }
}

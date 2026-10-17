//! H.264 helpers: avcC parsing/building, WebCodecs codec strings, Annex-B <-> AVCC.

use bytes::Bytes;

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
pub fn build_avcc(sps: &[u8], pps: &[u8]) -> Option<Bytes> {
    let _ = (sps, pps);
    todo!("rd-agent-core: build_avcc (needed for Windows; mac gets avcC from VideoToolbox)")
}

/// Extract SPS NAL units from an avcC record.
pub fn avcc_sps_list(avcc: &[u8]) -> Vec<&[u8]> {
    let _ = avcc;
    todo!("rd-agent-core: avcc_sps_list")
}

/// Convert Annex-B (start codes) to AVCC (4-byte lengths), dropping SPS/PPS/AUD NALs.
pub fn annexb_to_avcc(annexb: &[u8]) -> Vec<u8> {
    let _ = annexb;
    todo!("rd-agent-core: annexb_to_avcc")
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

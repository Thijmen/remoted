//! SPS VUI fix-up. Browsers' H.264 decoders only output each frame as soon as it is decoded
//! when the SPS declares `bitstream_restriction_flag = 1` with `max_num_reorder_frames = 0`;
//! otherwise they may hold up to a full DPB of frames. VideoToolbox's standard rate control
//! omits the restriction (the low-latency one includes it), so when it is missing it is added to
//! the SPS inside the avcC record. VideoToolbox's AVCC samples carry no in-band parameter sets,
//! so the avcC is the only copy a decoder sees. Only valid because the session never reorders
//! frames (`AllowFrameReordering = false`).

use anyhow::{Context as _, anyhow, ensure};
use h264_reader::nal::sps::{BitstreamRestrictions, SeqParameterSet, VuiParameters};
use h264_reader::nal::{Nal, NalHeader, RefNal, WritableNal};

/// A rewritten avcC record if any SPS in `avcc` permits frame reordering; `None` if every SPS
/// already declares `max_num_reorder_frames = 0`.
pub(crate) fn without_reordering(avcc: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    let record = AvccRecord::parse(avcc)?;
    let mut changed = false;
    let mut sps_list = Vec::with_capacity(record.sps.len());
    for sps in &record.sps {
        match rewrite_sps(sps)? {
            Some(rewritten) => {
                changed = true;
                sps_list.push(rewritten);
            }
            None => sps_list.push(sps.to_vec()),
        }
    }
    Ok(changed.then(|| record.rebuild(&sps_list)))
}

/// The SPS NAL with a `max_num_reorder_frames = 0` bitstream restriction, or `None` if it
/// already has one.
fn rewrite_sps(nal: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    let header = NalHeader::new(*nal.first().context("empty SPS")?)
        .map_err(|e| anyhow!("bad SPS NAL header: {e:?}"))?;
    let mut sps = SeqParameterSet::from_bits(RefNal::new(nal, &[], true).rbsp_bits())
        .map_err(|e| anyhow!("cannot parse SPS: {e:?}"))?;
    // Without reordering the DPB only has to hold the reference frames.
    let max_dec_frame_buffering = sps.max_num_ref_frames.max(1);
    let vui = sps
        .vui_parameters
        .get_or_insert_with(VuiParameters::default);
    let restrictions = match vui.bitstream_restrictions.take() {
        Some(r) if r.max_num_reorder_frames == 0 => return Ok(None),
        Some(r) => BitstreamRestrictions {
            max_num_reorder_frames: 0,
            ..r
        },
        // The values the spec infers when the flag is absent, except for the reordering.
        None => BitstreamRestrictions {
            motion_vectors_over_pic_boundaries_flag: true,
            max_bytes_per_pic_denom: 2,
            max_bits_per_mb_denom: 1,
            log2_max_mv_length_horizontal: 15,
            log2_max_mv_length_vertical: 15,
            max_num_reorder_frames: 0,
            max_dec_frame_buffering,
        },
    };
    vui.bitstream_restrictions = Some(restrictions);
    Ok(Some(sps.to_vec_with_header(header)))
}

/// AVCDecoderConfigurationRecord (ISO/IEC 14496-15 5.3.3.1), split into its parts.
struct AvccRecord<'a> {
    /// configurationVersion, profile, compatibility, level, lengthSizeMinusOne byte.
    head: &'a [u8],
    sps: Vec<&'a [u8]>,
    pps: Vec<&'a [u8]>,
    /// High-profile extension (chroma format, bit depths, SPS extensions), kept verbatim.
    tail: &'a [u8],
}

impl<'a> AvccRecord<'a> {
    fn parse(avcc: &'a [u8]) -> anyhow::Result<Self> {
        ensure!(
            avcc.len() >= 7 && avcc[0] == 1,
            "not an avcC version 1 record"
        );
        let mut rest = &avcc[6..];
        let sps = take_param_sets(&mut rest, usize::from(avcc[5] & 0x1f))?;
        let (&pps_count, after) = rest.split_first().context("avcC truncated before PPS")?;
        rest = after;
        let pps = take_param_sets(&mut rest, usize::from(pps_count))?;
        Ok(AvccRecord {
            head: &avcc[..5],
            sps,
            pps,
            tail: rest,
        })
    }

    fn rebuild(&self, sps: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(self.head);
        out.push(0xe0 | sps.len() as u8);
        for nal in sps {
            out.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            out.extend_from_slice(nal);
        }
        out.push(self.pps.len() as u8);
        for nal in &self.pps {
            out.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            out.extend_from_slice(nal);
        }
        out.extend_from_slice(self.tail);
        out
    }
}

fn take_param_sets<'a>(rest: &mut &'a [u8], count: usize) -> anyhow::Result<Vec<&'a [u8]>> {
    let mut sets = Vec::with_capacity(count);
    for _ in 0..count {
        ensure!(rest.len() >= 2, "avcC truncated in a parameter set length");
        let len = usize::from(u16::from_be_bytes([rest[0], rest[1]]));
        ensure!(rest.len() >= 2 + len, "avcC truncated in a parameter set");
        sets.push(&rest[2..2 + len]);
        *rest = &rest[2 + len..];
    }
    Ok(sets)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// VideoToolbox, standard rate control, 1728x1118: no bitstream restriction.
    const STANDARD: &str = "01640c2affe1001027640c2aac56806c0237ea6a0202020401000428ee3cb0fdf8f800";
    /// VideoToolbox, low-latency rate control, 1728x1118: restriction present.
    const LOW_LATENCY: &str =
        "01640c2affe1001527640c2aac1314501b008dfa9a80808083c201096001000428ee3cb0fdf8f800";

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn first_sps(avcc: &[u8]) -> SeqParameterSet {
        let record = AvccRecord::parse(avcc).unwrap();
        SeqParameterSet::from_bits(RefNal::new(record.sps[0], &[], true).rbsp_bits()).unwrap()
    }

    #[test]
    fn adds_bitstream_restriction_and_keeps_everything_else() {
        let original = hex(STANDARD);
        let rewritten = without_reordering(&original)
            .unwrap()
            .expect("needs a rewrite");
        assert_eq!(
            rewritten[..5],
            original[..5],
            "profile/level bytes unchanged"
        );

        let (before, after) = (first_sps(&original), first_sps(&rewritten));
        let restriction = after
            .vui_parameters
            .as_ref()
            .and_then(|v| v.bitstream_restrictions.as_ref())
            .expect("restriction added");
        assert_eq!(restriction.max_num_reorder_frames, 0);
        assert_eq!(
            restriction.max_dec_frame_buffering,
            before.max_num_ref_frames.max(1)
        );

        let mut expected = before.clone();
        expected
            .vui_parameters
            .as_mut()
            .unwrap()
            .bitstream_restrictions = Some(restriction.clone());
        assert_eq!(after, expected);
        assert_eq!(after.pixel_dimensions().unwrap(), (1728, 1118));

        let (old, new) = (
            AvccRecord::parse(&original).unwrap(),
            AvccRecord::parse(&rewritten).unwrap(),
        );
        assert_eq!(old.pps, new.pps);
        assert_eq!(old.tail, new.tail);
        assert_eq!(without_reordering(&rewritten).unwrap(), None, "idempotent");
    }

    #[test]
    fn leaves_low_latency_sps_alone() {
        assert_eq!(without_reordering(&hex(LOW_LATENCY)).unwrap(), None);
    }

    #[test]
    fn rejects_malformed_records() {
        assert!(without_reordering(&[]).is_err());
        assert!(without_reordering(&[1, 0x64, 0, 0x2a, 0xff, 0xe1, 0x00, 0x20, 0x27]).is_err());
        let mut truncated = hex(STANDARD);
        truncated.truncate(25);
        assert!(without_reordering(&truncated).is_err());
    }
}

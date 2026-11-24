pub fn rbsp_from_ebsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut i = 0;
    while i < nal.len() {
        if i + 2 < nal.len() && nal[i] == 0 && nal[i + 1] == 0 && nal[i + 2] == 3 {
            out.extend_from_slice(&[0, 0]);
            i += 3;
        } else {
            out.push(nal[i]);
            i += 1;
        }
    }
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len() * 8 - self.pos
    }

    fn bits(&mut self, n: usize) -> Option<u32> {
        if n > self.remaining() {
            return None;
        }
        let mut v = 0u32;
        for _ in 0..n {
            let bit = (self.data[self.pos >> 3] >> (7 - (self.pos & 7))) & 1;
            v = (v << 1) | bit as u32;
            self.pos += 1;
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while self.bits(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros as usize)?)
    }

    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k % 2 == 1 {
            ((k + 1) / 2) as i32
        } else {
            -((k / 2) as i32)
        })
    }
}

fn skip_scaling_list(r: &mut BitReader, size: usize) -> Option<()> {
    let mut last = 8i32;
    let mut next = 8i32;
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se()? + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

pub fn h264_sps_dimensions(sps_nal: &[u8]) -> Option<(u32, u32)> {
    if sps_nal.len() < 4 {
        return None;
    }
    let rbsp = rbsp_from_ebsp(&sps_nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let profile = r.bits(8)?;
    r.bits(8)?;
    r.bits(8)?;
    r.ue()?;
    let mut chroma = 1u32;
    let mut separate = 0u32;
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma = r.ue()?;
        if chroma == 3 {
            separate = r.bits(1)?;
        }
        r.ue()?;
        r.ue()?;
        r.bits(1)?;
        if r.bits(1)? == 1 {
            let count = if chroma != 3 { 8 } else { 12 };
            for idx in 0..count {
                if r.bits(1)? == 1 {
                    skip_scaling_list(&mut r, if idx < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue()?;
    let pic_order = r.ue()?;
    if pic_order == 0 {
        r.ue()?;
    } else if pic_order == 1 {
        r.bits(1)?;
        r.se()?;
        r.se()?;
        for _ in 0..r.ue()? {
            r.se()?;
        }
    }
    r.ue()?;
    r.bits(1)?;
    let width_mbs = r.ue()?;
    let height_mbs = r.ue()?;
    let frame_mbs_only = r.bits(1)?;
    if frame_mbs_only == 0 {
        r.ue()?;
    }
    r.ue()?;
    r.bits(1)?;
    let crop = if r.bits(1)? == 1 {
        [r.ue()?, r.ue()?, r.ue()?, r.ue()?]
    } else {
        [0, 0, 0, 0]
    };
    let sub_w = if (chroma == 1 || chroma == 2) && separate == 0 {
        2
    } else {
        1
    };
    let sub_h = (if chroma == 1 && separate == 0 { 2 } else { 1 })
        * (2 - frame_mbs_only);
    let width = (width_mbs + 1)
        .checked_mul(16)?
        .checked_sub(sub_w * (crop[0] + crop[1]))?;
    let height = (height_mbs + 1)
        .checked_mul(16)?
        .checked_sub(sub_h * (crop[2] + crop[3]))?;
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

pub fn flv_h264_dimensions(buf: &[u8]) -> Option<(u32, u32)> {
    if buf.len() < 13 || &buf[0..3] != b"FLV" {
        return None;
    }
    let header_size = u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]) as usize;
    let mut pos = header_size.checked_add(4)?;
    while pos + 11 <= buf.len() {
        let tag_type = buf[pos];
        let data_size = ((buf[pos + 1] as usize) << 16)
            | ((buf[pos + 2] as usize) << 8)
            | buf[pos + 3] as usize;
        let body = pos + 11;
        if tag_type == 9 && data_size >= 8 {
            let frame_type = buf[body] >> 4;
            let codec_id = buf[body] & 0x0f;
            if frame_type == 1 && codec_id == 7 && buf[body + 1] == 0 {
                let rec = &buf[body + 5..];
                if rec.len() < 8 {
                    return None;
                }
                let sps_count = (rec[5] & 0x1f) as usize;
                if sps_count == 0 {
                    return None;
                }
                let sps_len = ((rec[6] as usize) << 8) | rec[7] as usize;
                if sps_len == 0 || rec.len() < 8 + sps_len {
                    return None;
                }
                return h264_sps_dimensions(&rec[8..8 + sps_len]);
            }
        }
        pos = body.checked_add(data_size)?.checked_add(4)?;
    }
    None
}

#[cfg(test)]
mod flv_probe_tests {
    use super::{flv_h264_dimensions, h264_sps_dimensions};

    const OR4_SPS: &str = "2764002aac131aa0780227e59b808080a0000003002000000f01e2c4d4";
    const PORTRAIT_SPS: &str = "67640028acd9404403c797c044000003000400000300c83c60c658";

    #[test]
    fn real_landscape_sps_is_landscape() {
        let nal = hex(OR4_SPS);
        let (w, h) = h264_sps_dimensions(&nal).expect("dimensions");
        assert!(w > h, "expected landscape, got {}x{}", w, h);
        assert_eq!((w, h), (1920, 1080));
    }

    #[test]
    fn portrait_sps_is_portrait() {
        let nal = hex(PORTRAIT_SPS);
        let (w, h) = h264_sps_dimensions(&nal).expect("dimensions");
        assert!(h > w, "expected portrait, got {}x{}", w, h);
        assert_eq!((w, h), (1080, 1920));
    }

    #[test]
    fn truncated_sps_yields_none() {
        let nal = hex(OR4_SPS);
        assert!(h264_sps_dimensions(&nal[..6]).is_none());
        assert!(h264_sps_dimensions(&[]).is_none());
    }

    fn flv_with_tags(tags: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut out = vec![b'F', b'L', b'V', 1, 5, 0, 0, 0, 9, 0, 0, 0, 0];
        for (tag_type, body) in tags {
            out.push(*tag_type);
            out.extend_from_slice(&[(body.len() >> 16) as u8, (body.len() >> 8) as u8, body.len() as u8]);
            out.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]);
            out.extend_from_slice(body);
            out.extend_from_slice(&((11 + body.len()) as u32).to_be_bytes());
        }
        out
    }

    fn avc_seq_body(sps: &[u8]) -> Vec<u8> {
        let mut body = vec![0x17, 0, 0, 0, 0, 1, sps[1], sps[2], sps[3], 0xfc | 1, 0xe0 | 1];
        body.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        body.extend_from_slice(sps);
        body.push(0);
        body
    }

    #[test]
    fn flv_walk_finds_seq_header_after_other_tags() {
        let sps = hex(OR4_SPS);
        let audio = vec![0xaf, 0x00, 0x12, 0x10];
        let metadata = vec![0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let flv = flv_with_tags(&[(8, audio), (18, metadata), (9, avc_seq_body(&sps))]);
        assert_eq!(flv_h264_dimensions(&flv), Some((1920, 1080)));
    }

    #[test]
    fn flv_without_avc_seq_header_yields_none() {
        let flv = flv_with_tags(&[(9, vec![0x27, 0x01, 0, 0, 0, 0, 0, 0])]);
        assert!(flv_h264_dimensions(&flv).is_none());
        assert!(flv_h264_dimensions(b"NOTFLV").is_none());
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }
}
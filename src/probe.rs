use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// 读取音频文件时长（秒）。支持 MP3 / FLAC / M4A(MP4)。
/// 不依赖外部工具（ffprobe），用于校验"下到的到底是不是整曲"。
pub fn duration_of(path: &Path) -> Result<f64> {
    let f = File::open(path).with_context(|| format!("打开音频失败: {}", path.display()))?;
    let len = f.metadata()?.len();
    if len == 0 {
        bail!("音频文件为空");
    }
    let mut r = BufReader::new(f);
    let mut head = [0u8; 12];
    let n = r.read(&mut head)?;

    if n >= 8 && &head[..4] == b"fLaC" {
        return flac_duration(&mut r);
    }
    if n >= 12 && &head[4..8] == b"ftyp" {
        return mp4_duration(&mut r, len);
    }
    mp3_duration(&mut r, len)
}

fn flac_duration<R: Read + Seek>(r: &mut R) -> Result<f64> {
    r.seek(SeekFrom::Start(4))?;
    loop {
        let mut hdr = [0u8; 4];
        if r.read_exact(&mut hdr).is_err() {
            bail!("FLAC 元数据不完整");
        }
        let last = hdr[0] & 0x80 != 0;
        let ty = hdr[0] & 0x7f;
        let len = ((hdr[1] as usize) << 16) | ((hdr[2] as usize) << 8) | hdr[3] as usize;
        if ty == 0 {
            let mut si = [0u8; 34];
            r.read_exact(&mut si)?;
            let v = u64::from_be_bytes(si[10..18].try_into().unwrap());
            let sample_rate = (v >> 44) & 0xFFFFF;
            let total_samples = v & 0xF_FFFF_FFFF;
            if sample_rate == 0 || total_samples == 0 {
                bail!("FLAC STREAMINFO 无有效时长");
            }
            return Ok(total_samples as f64 / sample_rate as f64);
        }
        r.seek(SeekFrom::Current(len as i64))?;
        if last {
            break;
        }
    }
    bail!("FLAC 缺少 STREAMINFO");
}

fn mp4_duration<R: Read + Seek>(r: &mut R, len: u64) -> Result<f64> {
    r.seek(SeekFrom::Start(0))?;
    let mut buf = Vec::new();
    r.take(len).read_to_end(&mut buf)?;
    let pos = find(&buf, b"mvhd").context("M4A 缺少 mvhd")?;
    let version = *buf.get(pos + 4).context("mvhd 截断")?;
    let (timescale, duration) = if version == 1 {
        let ts = be_u32(&buf, pos + 24)?;
        let du = be_u64(&buf, pos + 28)?;
        (ts, du)
    } else {
        let ts = be_u32(&buf, pos + 16)?;
        let du = be_u32(&buf, pos + 20)? as u64;
        (ts, du)
    };
    if timescale == 0 {
        bail!("M4A mvhd timescale 为 0");
    }
    Ok(duration as f64 / timescale as f64)
}

fn mp3_duration<R: Read + Seek>(r: &mut R, len: u64) -> Result<f64> {
    let mut pos: u64 = 0;
    r.seek(SeekFrom::Start(0))?;
    let mut head = [0u8; 10];
    if r.read(&mut head)? == 10 && &head[..3] == b"ID3" {
        let size = ((head[6] as u64 & 0x7f) << 21)
            | ((head[7] as u64 & 0x7f) << 14)
            | ((head[8] as u64 & 0x7f) << 7)
            | (head[9] as u64 & 0x7f);
        pos = 10 + size + if head[5] & 0x10 != 0 { 10 } else { 0 };
    }

    let mut seconds = 0.0f64;
    let mut misses = 0u64;
    while pos + 4 <= len {
        r.seek(SeekFrom::Start(pos))?;
        let mut h = [0u8; 4];
        if r.read_exact(&mut h).is_err() {
            break;
        }
        match parse_frame_header(&h) {
            Some((frame_len, sample_rate, samples_per_frame)) => {
                if frame_len == 0 {
                    pos += 1;
                    continue;
                }
                seconds += samples_per_frame as f64 / sample_rate as f64;
                pos += frame_len as u64;
            }
            None => {
                pos += 1;
                misses += 1;
                if misses > 4_000_000 {
                    break;
                }
            }
        }
    }
    if seconds <= 0.0 {
        bail!("不是有效的 MP3/FLAC/M4A 音频");
    }
    Ok(seconds)
}

fn parse_frame_header(h: &[u8; 4]) -> Option<(usize, u32, u32)> {
    if h[0] != 0xFF || (h[1] & 0xE0) != 0xE0 {
        return None;
    }
    let version = (h[1] >> 3) & 0x3; // 3=MPEG1, 2=MPEG2, 0=MPEG2.5
    let layer = (h[1] >> 1) & 0x3; // 1 = Layer III
    if layer != 1 {
        return None;
    }
    let br_idx = ((h[2] >> 4) & 0xF) as usize;
    let sr_idx = ((h[2] >> 2) & 0x3) as usize;
    let pad = ((h[2] >> 1) & 1) as usize;
    if br_idx == 0 || br_idx == 15 || sr_idx == 3 {
        return None;
    }
    const BR_V1: [u32; 16] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0];
    const BR_V2: [u32; 16] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0];
    const SR: [u32; 3] = [44100, 48000, 32000];

    let (bitrate, sample_rate, samples_per_frame) = match version {
        3 => (BR_V1[br_idx] * 1000, SR[sr_idx], 1152),
        2 => (BR_V2[br_idx] * 1000, SR[sr_idx] / 2, 576),
        0 => (BR_V2[br_idx] * 1000, SR[sr_idx] / 4, 576),
        _ => return None,
    };
    if bitrate == 0 || sample_rate == 0 {
        return None;
    }
    let frame_len = if version == 3 {
        144 * bitrate as usize / sample_rate as usize + pad
    } else {
        72 * bitrate as usize / sample_rate as usize + pad
    };
    Some((frame_len, sample_rate, samples_per_frame))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn be_u32(buf: &[u8], at: usize) -> Result<u32> {
    let b: [u8; 4] = buf.get(at..at + 4).context("mp4 字段截断")?.try_into().unwrap();
    Ok(u32::from_be_bytes(b))
}

fn be_u64(buf: &[u8], at: usize) -> Result<u64> {
    let b: [u8; 8] = buf.get(at..at + 8).context("mp4 字段截断")?.try_into().unwrap();
    Ok(u64::from_be_bytes(b))
}

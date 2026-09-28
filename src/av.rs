//! Audio/video container metadata, read with small dependency-free parsers:
//! ISO base media (MP4, MOV, M4A), Matroska/WebM and WAV.
//!
//! Only headers are parsed — duration, frame size, codecs and frame rate — so
//! this works without ffmpeg. Decoding frames lives in [`crate::ffmpeg`].

/// Metadata of an audio or video file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AvInfo {
    /// Seconds.
    pub duration: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    /// Frames per second (video only).
    pub frame_rate: Option<f64>,
}

impl AvInfo {
    /// Whether the file has a video track.
    pub fn has_video(&self) -> bool {
        self.video_codec.is_some() || self.width.is_some()
    }

    /// The most descriptive codec name (video first).
    pub fn codec(&self) -> Option<&str> {
        self.video_codec.as_deref().or(self.audio_codec.as_deref())
    }
}

/// Parses whichever supported container `bytes` holds.
pub fn info(bytes: &[u8]) -> Option<AvInfo> {
    let is = |offset: usize, magic: &[u8]| bytes.get(offset..offset + magic.len()) == Some(magic);
    if is(4, b"ftyp") || is(4, b"moov") || is(4, b"mdat") || is(4, b"free") {
        parse_mp4(bytes)
    } else if is(0, b"\x1a\x45\xdf\xa3") {
        parse_matroska(bytes)
    } else if is(0, b"RIFF") && is(8, b"WAVE") {
        parse_wav(bytes, false)
    } else {
        None
    }
}

/// Reads metadata from a file without loading it all: MP4 files are walked
/// box by box and only `moov` is read; other containers read their header
/// region.
pub fn info_from_file(path: &std::path::Path) -> Option<AvInfo> {
    use std::io::{Read, Seek, SeekFrom};

    const HEADER_REGION: u64 = 16 * 1024 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let mut head = [0u8; 12];
    file.read_exact(&mut head).ok()?;
    file.seek(SeekFrom::Start(0)).ok()?;

    if &head[4..8] == b"ftyp" || &head[4..8] == b"moov" || &head[4..8] == b"mdat" {
        // Walk top-level boxes; `mdat` (the media data) is skipped by seeking.
        let mut pos = 0u64;
        while pos + 8 <= length {
            let mut header = [0u8; 16];
            file.seek(SeekFrom::Start(pos)).ok()?;
            file.read_exact(&mut header[..8]).ok()?;
            let mut size = u64::from(u32::from_be_bytes(header[..4].try_into().ok()?));
            let mut header_len = 8;
            if size == 1 {
                file.read_exact(&mut header[8..16]).ok()?;
                size = u64::from_be_bytes(header[8..16].try_into().ok()?);
                header_len = 16;
            } else if size == 0 {
                size = length - pos;
            }
            if size < header_len {
                return None;
            }
            if &header[4..8] == b"moov" {
                let mut moov = vec![0u8; usize::try_from(size).ok()?];
                file.seek(SeekFrom::Start(pos)).ok()?;
                file.read_exact(&mut moov).ok()?;
                return parse_mp4(&moov);
            }
            pos = pos.checked_add(size)?;
        }
        None
    } else if head[..4] == [0x1a, 0x45, 0xdf, 0xa3] {
        let mut region = Vec::new();
        file.take(HEADER_REGION).read_to_end(&mut region).ok()?;
        parse_matroska(&region)
    } else if &head[..4] == b"RIFF" && &head[8..12] == b"WAVE" {
        let mut region = Vec::new();
        file.take(HEADER_REGION.min(length))
            .read_to_end(&mut region)
            .ok()?;
        parse_wav(&region, length > HEADER_REGION)
    } else {
        None
    }
}

/// `m:ss` (or `h:mm:ss`), e.g. `0:04`, `12:30`, `1:02:03`.
pub fn format_duration(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

// ---------------------------------------------------------------------------
// ISO base media file format (MP4 / MOV / M4A)
// ---------------------------------------------------------------------------

fn be_u16(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn be_u32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn be_u64(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

/// Iterates `(type, payload)` over consecutive ISO-BMFF boxes.
fn boxes(data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        let size = be_u32(data, pos)? as u64;
        let kind: [u8; 4] = data.get(pos + 4..pos + 8)?.try_into().ok()?;
        let (header, size) = match size {
            0 => (8, (data.len() - pos) as u64),
            1 => (16, be_u64(data, pos + 8)?),
            n => (8, n),
        };
        let end = pos.checked_add(usize::try_from(size).ok()?)?;
        if size < header as u64 || end > data.len() {
            return None;
        }
        let payload = &data[pos + header..end];
        pos = end;
        Some((kind, payload))
    })
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(data)
        .find(|(k, _)| k == kind)
        .map(|(_, payload)| payload)
}

/// `(timescale, duration)` from an `mvhd` or `mdhd` payload.
fn timescale_and_duration(payload: &[u8]) -> Option<(u32, u64)> {
    match payload.first()? {
        1 => Some((be_u32(payload, 20)?, be_u64(payload, 24)?)),
        _ => Some((be_u32(payload, 12)?, u64::from(be_u32(payload, 16)?))),
    }
}

fn seconds((timescale, duration): (u32, u64)) -> Option<f64> {
    (timescale > 0 && duration > 0 && duration != u64::from(u32::MAX))
        .then(|| duration as f64 / f64::from(timescale))
}

fn mp4_codec_name(fourcc: &[u8]) -> String {
    match fourcc {
        b"avc1" | b"avc3" => "H.264".into(),
        b"hvc1" | b"hev1" => "HEVC".into(),
        b"av01" => "AV1".into(),
        b"vp09" => "VP9".into(),
        b"vp08" => "VP8".into(),
        b"mp4v" => "MPEG-4".into(),
        b"jpeg" | b"mjpa" | b"mjpb" => "MJPEG".into(),
        b"apch" | b"apcn" | b"apcs" | b"apco" | b"ap4h" => "ProRes".into(),
        b"mp4a" => "AAC".into(),
        b"Opus" => "Opus".into(),
        b"fLaC" => "FLAC".into(),
        b"alac" => "ALAC".into(),
        b"ac-3" => "AC-3".into(),
        b"ec-3" => "E-AC-3".into(),
        b".mp3" => "MP3".into(),
        other => String::from_utf8_lossy(other).trim().to_string(),
    }
}

/// Parses a buffer holding (at least) the `moov` box.
fn parse_mp4(data: &[u8]) -> Option<AvInfo> {
    let moov = child(data, b"moov")?;
    let mut info = AvInfo {
        duration: child(moov, b"mvhd")
            .and_then(timescale_and_duration)
            .and_then(seconds),
        ..AvInfo::default()
    };

    for (kind, trak) in boxes(moov) {
        if &kind != b"trak" {
            continue;
        }
        let Some(mdia) = child(trak, b"mdia") else {
            continue;
        };
        let handler = child(mdia, b"hdlr").and_then(|h| h.get(8..12));
        let track_time = child(mdia, b"mdhd").and_then(timescale_and_duration);
        let stbl = child(mdia, b"minf").and_then(|minf| child(minf, b"stbl"));
        let sample_entry = stbl
            .and_then(|stbl| child(stbl, b"stsd"))
            .and_then(|stsd| stsd.get(8..));
        let codec = sample_entry
            .and_then(|entry| entry.get(4..8))
            .map(mp4_codec_name);

        match handler {
            Some(b"vide") if info.video_codec.is_none() => {
                info.video_codec = codec;
                // Visual sample entry: width/height at bytes 32..36.
                let entry_size =
                    sample_entry.and_then(|entry| Some((be_u16(entry, 32)?, be_u16(entry, 34)?)));
                let header_size = child(trak, b"tkhd").and_then(|tkhd| {
                    let at = if tkhd.first() == Some(&1) { 88 } else { 76 };
                    Some((
                        (be_u32(tkhd, at)? >> 16) as u16,
                        (be_u32(tkhd, at + 4)? >> 16) as u16,
                    ))
                });
                if let Some((w, h)) = entry_size
                    .filter(|(w, h)| *w > 0 && *h > 0)
                    .or(header_size.filter(|(w, h)| *w > 0 && *h > 0))
                {
                    info.width = Some(u32::from(w));
                    info.height = Some(u32::from(h));
                }
                // Frame rate = samples / track duration.
                let samples: Option<u64> = stbl.and_then(|stbl| child(stbl, b"stts")).map(|stts| {
                    let entries = be_u32(stts, 4).unwrap_or(0) as usize;
                    (0..entries)
                        .filter_map(|i| be_u32(stts, 8 + i * 8))
                        .map(u64::from)
                        .sum()
                });
                if let (Some(samples), Some(duration)) = (samples, track_time.and_then(seconds)) {
                    info.frame_rate = (samples > 0).then(|| samples as f64 / duration);
                }
                if info.duration.is_none() {
                    info.duration = track_time.and_then(seconds);
                }
            }
            Some(b"soun") if info.audio_codec.is_none() => {
                info.audio_codec = codec;
                if info.duration.is_none() {
                    info.duration = track_time.and_then(seconds);
                }
            }
            _ => {}
        }
    }
    Some(info)
}

// ---------------------------------------------------------------------------
// Matroska / WebM (EBML)
// ---------------------------------------------------------------------------

const EBML_SEGMENT: u32 = 0x1853_8067;
const EBML_INFO: u32 = 0x1549_A966;
const EBML_TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
const EBML_DURATION: u32 = 0x4489;
const EBML_TRACKS: u32 = 0x1654_AE6B;
const EBML_TRACK_ENTRY: u32 = 0xAE;
const EBML_TRACK_TYPE: u32 = 0x83;
const EBML_CODEC_ID: u32 = 0x86;
const EBML_DEFAULT_DURATION: u32 = 0x23_E383;
const EBML_VIDEO: u32 = 0xE0;
const EBML_PIXEL_WIDTH: u32 = 0xB0;
const EBML_PIXEL_HEIGHT: u32 = 0xBA;
const EBML_CLUSTER: u32 = 0x1F43_B675;

/// Reads an element ID (marker bits kept). Returns `(id, length)`.
fn ebml_id(data: &[u8], pos: usize) -> Option<(u32, usize)> {
    let first = *data.get(pos)?;
    let length = first.leading_zeros() as usize + 1;
    if length > 4 {
        return None;
    }
    let bytes = data.get(pos..pos + length)?;
    Some((
        bytes.iter().fold(0u32, |acc, b| (acc << 8) | u32::from(*b)),
        length,
    ))
}

/// Reads an element size. `None` size means "unknown" (extends to the end).
fn ebml_size(data: &[u8], pos: usize) -> Option<(Option<u64>, usize)> {
    let first = *data.get(pos)?;
    let length = first.leading_zeros() as usize + 1;
    if length > 8 {
        return None;
    }
    let bytes = data.get(pos..pos + length)?;
    let mask = if length == 8 { 0 } else { 0xffu8 >> length };
    let value = bytes[1..]
        .iter()
        .fold(u64::from(first & mask), |acc, b| (acc << 8) | u64::from(*b));
    let all_ones = (1u64 << (7 * length)) - 1;
    Some(((value != all_ones).then_some(value), length))
}

/// Iterates `(id, payload)` over consecutive EBML elements.
fn elements(data: &[u8]) -> impl Iterator<Item = (u32, &[u8])> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        let (id, id_len) = ebml_id(data, pos)?;
        let (size, size_len) = ebml_size(data, pos + id_len)?;
        let start = pos + id_len + size_len;
        let end = match size {
            Some(size) => start
                .checked_add(usize::try_from(size).ok()?)?
                .min(data.len()),
            None => data.len(),
        };
        if start > end {
            return None;
        }
        pos = end;
        Some((id, &data[start..end]))
    })
}

fn ebml_uint(payload: &[u8]) -> Option<u64> {
    (payload.len() <= 8).then(|| {
        payload
            .iter()
            .fold(0u64, |acc, b| (acc << 8) | u64::from(*b))
    })
}

fn ebml_float(payload: &[u8]) -> Option<f64> {
    match payload.len() {
        4 => Some(f64::from(f32::from_be_bytes(payload.try_into().ok()?))),
        8 => Some(f64::from_be_bytes(payload.try_into().ok()?)),
        _ => None,
    }
}

fn matroska_codec_name(id: &str) -> String {
    match id {
        "V_MPEG4/ISO/AVC" => "H.264".into(),
        "V_MPEGH/ISO/HEVC" => "HEVC".into(),
        "V_VP8" => "VP8".into(),
        "V_VP9" => "VP9".into(),
        "V_AV1" => "AV1".into(),
        "V_MPEG4/ISO/ASP" | "V_MPEG4/ISO/SP" => "MPEG-4".into(),
        "A_OPUS" => "Opus".into(),
        "A_VORBIS" => "Vorbis".into(),
        "A_FLAC" => "FLAC".into(),
        "A_MPEG/L3" => "MP3".into(),
        id if id.starts_with("A_AAC") => "AAC".into(),
        other => other.trim_start_matches(['V', 'A', '_']).to_string(),
    }
}

fn parse_matroska(data: &[u8]) -> Option<AvInfo> {
    let (_, segment) = elements(data).find(|(id, _)| *id == EBML_SEGMENT)?;
    let mut info = AvInfo::default();
    let mut scale = 1_000_000u64; // nanoseconds per timestamp unit
    let mut raw_duration = None;
    let (mut saw_info, mut saw_tracks) = (false, false);

    for (id, payload) in elements(segment) {
        match id {
            EBML_INFO => {
                saw_info = true;
                for (id, value) in elements(payload) {
                    match id {
                        EBML_TIMESTAMP_SCALE => scale = ebml_uint(value).unwrap_or(scale),
                        EBML_DURATION => raw_duration = ebml_float(value),
                        _ => {}
                    }
                }
            }
            EBML_TRACKS => {
                saw_tracks = true;
                for (_, entry) in elements(payload).filter(|(id, _)| *id == EBML_TRACK_ENTRY) {
                    let mut kind = None;
                    let mut codec = None;
                    let mut frame_ns = None;
                    let mut size = (None, None);
                    for (id, value) in elements(entry) {
                        match id {
                            EBML_TRACK_TYPE => kind = ebml_uint(value),
                            EBML_CODEC_ID => {
                                codec = Some(matroska_codec_name(
                                    String::from_utf8_lossy(value).trim_end_matches('\0'),
                                ))
                            }
                            EBML_DEFAULT_DURATION => frame_ns = ebml_uint(value),
                            EBML_VIDEO => {
                                for (id, value) in elements(value) {
                                    match id {
                                        EBML_PIXEL_WIDTH => size.0 = ebml_uint(value),
                                        EBML_PIXEL_HEIGHT => size.1 = ebml_uint(value),
                                        _ => {}
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    match kind {
                        Some(1) if info.video_codec.is_none() => {
                            info.video_codec = codec;
                            info.width = size.0.and_then(|w| u32::try_from(w).ok());
                            info.height = size.1.and_then(|h| u32::try_from(h).ok());
                            info.frame_rate =
                                frame_ns.filter(|ns| *ns > 0).map(|ns| 1e9 / ns as f64);
                        }
                        Some(2) if info.audio_codec.is_none() => info.audio_codec = codec,
                        _ => {}
                    }
                }
            }
            EBML_CLUSTER if saw_info && saw_tracks => break,
            _ => {}
        }
    }
    info.duration = raw_duration
        .filter(|d| *d > 0.0)
        .map(|d| d * scale as f64 / 1e9);
    Some(info)
}

// ---------------------------------------------------------------------------
// WAV
// ---------------------------------------------------------------------------

/// `partial` means `data` is only the start of a longer file, so the `data`
/// chunk's declared size is trusted rather than clamped to what was read.
fn parse_wav(data: &[u8], partial: bool) -> Option<AvInfo> {
    let le_u32 = |at: usize| Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?));
    let (mut byte_rate, mut format, mut data_size) = (None, None, None);
    let mut pos = 12;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let size = le_u32(pos + 4)? as usize;
        match id {
            b"fmt " => {
                format = Some(u16::from_le_bytes([
                    *data.get(pos + 8)?,
                    *data.get(pos + 9)?,
                ]));
                byte_rate = le_u32(pos + 16);
            }
            b"data" => {
                data_size = Some(if partial {
                    size
                } else {
                    size.min(data.len() - pos - 8)
                });
                break;
            }
            _ => {}
        }
        pos += 8 + size + (size & 1);
    }
    Some(AvInfo {
        duration: match (data_size, byte_rate) {
            (Some(size), Some(rate)) if rate > 0 => Some(size as f64 / f64::from(rate)),
            _ => None,
        },
        audio_codec: format.map(|f| match f {
            1 => "PCM".to_string(),
            3 => "PCM float".to_string(),
            6 => "A-law".to_string(),
            7 => "µ-law".to_string(),
            other => format!("WAV format {other}"),
        }),
        ..AvInfo::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    /// A header-only MP4: 10 s movie, one 640×360 H.264 track with 250
    /// frames, one AAC track.
    fn synthetic_mp4() -> Vec<u8> {
        let mut mvhd = vec![0u8; 100];
        mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
        mvhd[16..20].copy_from_slice(&10_000u32.to_be_bytes());

        let mut mdhd = vec![0u8; 24];
        mdhd[12..16].copy_from_slice(&25_000u32.to_be_bytes());
        mdhd[16..20].copy_from_slice(&250_000u32.to_be_bytes());
        let mut hdlr = vec![0u8; 24];
        hdlr[8..12].copy_from_slice(b"vide");

        let mut entry = vec![0u8; 86];
        entry[0..4].copy_from_slice(&86u32.to_be_bytes());
        entry[4..8].copy_from_slice(b"avc1");
        entry[32..34].copy_from_slice(&640u16.to_be_bytes());
        entry[34..36].copy_from_slice(&360u16.to_be_bytes());
        let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd.extend(entry);
        let mut stts = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stts.extend(250u32.to_be_bytes());
        stts.extend(1000u32.to_be_bytes());
        let stbl = [mp4_box(b"stsd", &stsd), mp4_box(b"stts", &stts)].concat();
        let minf = mp4_box(b"stbl", &stbl);
        let mdia = [
            mp4_box(b"mdhd", &mdhd),
            mp4_box(b"hdlr", &hdlr),
            mp4_box(b"minf", &minf),
        ]
        .concat();
        let video = mp4_box(b"trak", &mp4_box(b"mdia", &mdia));

        let mut audio_hdlr = vec![0u8; 24];
        audio_hdlr[8..12].copy_from_slice(b"soun");
        let mut audio_entry = vec![0u8; 36];
        audio_entry[4..8].copy_from_slice(b"mp4a");
        let mut audio_stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
        audio_stsd.extend(audio_entry);
        let audio_minf = mp4_box(b"stbl", &mp4_box(b"stsd", &audio_stsd));
        let audio_mdia = [mp4_box(b"hdlr", &audio_hdlr), mp4_box(b"minf", &audio_minf)].concat();
        let audio = mp4_box(b"trak", &mp4_box(b"mdia", &audio_mdia));

        let moov = mp4_box(b"moov", &[mp4_box(b"mvhd", &mvhd), video, audio].concat());
        [
            mp4_box(b"ftyp", b"isom\0\0\0\0isom"),
            moov,
            mp4_box(b"mdat", &[0; 16]),
        ]
        .concat()
    }

    #[test]
    fn parses_mp4_headers() {
        let info = info(&synthetic_mp4()).unwrap();
        assert_eq!(info.duration, Some(10.0));
        assert_eq!((info.width, info.height), (Some(640), Some(360)));
        assert_eq!(info.video_codec.as_deref(), Some("H.264"));
        assert_eq!(info.audio_codec.as_deref(), Some("AAC"));
        assert_eq!(info.frame_rate, Some(25.0));
        assert!(info.has_video());
    }

    fn ebml(id: &[u8], payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() < 0x4000);
        let mut out = id.to_vec();
        out.extend_from_slice(&(0x4000u16 | payload.len() as u16).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn parses_matroska_headers() {
        let info_el = ebml(
            &[0x15, 0x49, 0xA9, 0x66],
            &[
                ebml(&[0x2A, 0xD7, 0xB1], &1_000_000u32.to_be_bytes()),
                ebml(&[0x44, 0x89], &4500.0f64.to_be_bytes()),
            ]
            .concat(),
        );
        let video_track = ebml(
            &[0xAE],
            &[
                ebml(&[0x83], &[1]),
                ebml(&[0x86], b"V_VP9"),
                ebml(&[0x23, 0xE3, 0x83], &41_666_667u32.to_be_bytes()),
                ebml(
                    &[0xE0],
                    &[ebml(&[0xB0], &[0x05, 0x00]), ebml(&[0xBA], &[0x02, 0xD0])].concat(),
                ),
            ]
            .concat(),
        );
        let audio_track = ebml(
            &[0xAE],
            &[ebml(&[0x83], &[2]), ebml(&[0x86], b"A_OPUS")].concat(),
        );
        let tracks = ebml(
            &[0x16, 0x54, 0xAE, 0x6B],
            &[video_track, audio_track].concat(),
        );
        // Segment with an unknown size, as live-written files have.
        let mut file = ebml(&[0x1A, 0x45, 0xDF, 0xA3], b"\x42\x82\x84webm");
        file.extend_from_slice(&[
            0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        ]);
        file.extend(info_el);
        file.extend(tracks);

        let info = info(&file).unwrap();
        assert_eq!(info.duration, Some(4.5));
        assert_eq!((info.width, info.height), (Some(1280), Some(720)));
        assert_eq!(info.video_codec.as_deref(), Some("VP9"));
        assert_eq!(info.audio_codec.as_deref(), Some("Opus"));
        assert!((info.frame_rate.unwrap() - 24.0).abs() < 0.01);
    }

    #[test]
    fn parses_wav_headers() {
        // 1 s of 8 kHz mono 16-bit PCM.
        let mut wav = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes()); // PCM
        wav.extend(1u16.to_le_bytes()); // channels
        wav.extend(8000u32.to_le_bytes()); // sample rate
        wav.extend(16_000u32.to_le_bytes()); // byte rate
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(16_000u32.to_le_bytes());
        wav.extend(vec![0u8; 16_000]);
        let info = info(&wav).unwrap();
        assert_eq!(info.duration, Some(1.0));
        assert_eq!(info.audio_codec.as_deref(), Some("PCM"));
        assert!(!info.has_video());
    }

    #[test]
    fn reads_mp4_metadata_from_files_with_moov_last() {
        // Put a large `mdat` before `moov`, as non-"faststart" files have.
        let full = synthetic_mp4();
        let ftyp = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
        let moov_start = ftyp.len();
        let moov_len =
            u32::from_be_bytes(full[moov_start..moov_start + 4].try_into().unwrap()) as usize;
        let moov = &full[moov_start..moov_start + moov_len];
        let file_bytes = [ftyp, mp4_box(b"mdat", &vec![7u8; 100_000]), moov.to_vec()].concat();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("late-moov.mp4");
        std::fs::write(&path, &file_bytes).unwrap();
        let from_file = info_from_file(&path).unwrap();
        assert_eq!(from_file, info(&file_bytes).unwrap());
        assert_eq!(from_file.duration, Some(10.0));
        assert_eq!(info_from_file(&dir.path().join("missing.mp4")), None);
    }

    /// A `trak` box with the given handler, codec and (optional) media
    /// header `(timescale, duration)` and track-header size.
    fn trak(
        handler: &[u8; 4],
        codec: &[u8; 4],
        entry_size: (u16, u16),
        mdhd: Option<(u32, u32)>,
        tkhd: Option<(u16, u16)>,
    ) -> Vec<u8> {
        let mut hdlr = vec![0u8; 24];
        hdlr[8..12].copy_from_slice(handler);
        let mut entry = vec![0u8; 86];
        entry[4..8].copy_from_slice(codec);
        entry[32..34].copy_from_slice(&entry_size.0.to_be_bytes());
        entry[34..36].copy_from_slice(&entry_size.1.to_be_bytes());
        let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd.extend(entry);
        let mut mdia = mp4_box(b"hdlr", &hdlr);
        if let Some((scale, duration)) = mdhd {
            let mut payload = vec![0u8; 24];
            payload[12..16].copy_from_slice(&scale.to_be_bytes());
            payload[16..20].copy_from_slice(&duration.to_be_bytes());
            mdia.extend(mp4_box(b"mdhd", &payload));
        }
        mdia.extend(mp4_box(
            b"minf",
            &mp4_box(b"stbl", &mp4_box(b"stsd", &stsd)),
        ));
        let mut trak = Vec::new();
        if let Some((w, h)) = tkhd {
            let mut payload = vec![0u8; 84];
            payload[76..80].copy_from_slice(&(u32::from(w) << 16).to_be_bytes());
            payload[80..84].copy_from_slice(&(u32::from(h) << 16).to_be_bytes());
            trak.extend(mp4_box(b"tkhd", &payload));
        }
        trak.extend(mp4_box(b"mdia", &mdia));
        mp4_box(b"trak", &trak)
    }

    #[test]
    fn mp4_track_fallbacks_and_header_versions() {
        // No movie header: the duration comes from the tracks, and a zero
        // sample-entry size falls back to the track header. Tracks without
        // media and non-A/V tracks are skipped.
        let moov = mp4_box(
            b"moov",
            &[
                mp4_box(b"trak", &[]),
                trak(b"text", b"tx3g", (0, 0), None, None),
                trak(
                    b"vide",
                    b"hvc1",
                    (0, 0),
                    Some((1000, 3000)),
                    Some((1920, 1080)),
                ),
            ]
            .concat(),
        );
        let parsed = info(&moov).unwrap();
        assert_eq!(parsed.duration, Some(3.0));
        assert_eq!((parsed.width, parsed.height), (Some(1920), Some(1080)));
        assert_eq!(parsed.video_codec.as_deref(), Some("HEVC"));

        let audio = mp4_box(
            b"moov",
            &trak(b"soun", b"Opus", (0, 0), Some((48_000, 96_000)), None),
        );
        let parsed = info(&audio).unwrap();
        assert_eq!(parsed.duration, Some(2.0));
        assert_eq!(parsed.audio_codec.as_deref(), Some("Opus"));
        assert!(!parsed.has_video());
        assert_eq!(parsed.codec(), Some("Opus"));

        // Version-1 headers carry 64-bit durations.
        let mut mvhd = vec![0u8; 112];
        mvhd[0] = 1;
        mvhd[20..24].copy_from_slice(&600u32.to_be_bytes());
        mvhd[24..32].copy_from_slice(&1800u64.to_be_bytes());
        let parsed = info(&mp4_box(b"moov", &mp4_box(b"mvhd", &mvhd))).unwrap();
        assert_eq!(parsed.duration, Some(3.0));

        // An all-ones duration means "unknown".
        let mut mvhd = vec![0u8; 100];
        mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
        mvhd[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        let parsed = info(&mp4_box(b"moov", &mp4_box(b"mvhd", &mvhd))).unwrap();
        assert_eq!(parsed.duration, None);
    }

    #[test]
    fn mp4_box_sizes_in_memory_and_in_files() {
        // A 64-bit ("largesize") box before `moov`.
        let mut large = 1u32.to_be_bytes().to_vec();
        large.extend(b"free");
        large.extend(24u64.to_be_bytes());
        large.extend([0u8; 8]);
        let moov = mp4_box(
            b"moov",
            &trak(b"vide", b"avc1", (320, 240), Some((1, 5)), None),
        );
        let bytes = [mp4_box(b"ftyp", b"isom"), large.clone(), moov].concat();
        let parsed = info(&bytes).unwrap();
        assert_eq!((parsed.width, parsed.duration), (Some(320), Some(5.0)));

        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        assert_eq!(info_from_file(&write("large.mp4", &bytes)), Some(parsed));

        // A last box that extends to the end of the file ("size 0").
        let mut to_end = 0u32.to_be_bytes().to_vec();
        to_end.extend(b"mdat");
        to_end.extend([1u8; 32]);
        assert_eq!(boxes(&to_end).count(), 1);
        let no_moov = [mp4_box(b"ftyp", b"isom"), to_end].concat();
        assert_eq!(info_from_file(&write("no-moov.mp4", &no_moov)), None);

        // A box smaller than its own header is corrupt.
        let corrupt = [
            mp4_box(b"ftyp", b"isom"),
            vec![0, 0, 0, 4, b'f', b'r', b'e', b'e'],
        ]
        .concat();
        assert_eq!(info_from_file(&write("corrupt.mp4", &corrupt)), None);
        assert_eq!(info_from_file(&write("tiny.mp4", b"abc")), None);
        assert_eq!(
            info_from_file(&write("other.avi", b"RIFF\0\0\0\0AVI LIST")),
            None
        );
    }

    #[test]
    fn codec_names() {
        for (fourcc, name) in [
            (b"avc3", "H.264"),
            (b"hev1", "HEVC"),
            (b"av01", "AV1"),
            (b"vp09", "VP9"),
            (b"vp08", "VP8"),
            (b"mp4v", "MPEG-4"),
            (b"mjpa", "MJPEG"),
            (b"apch", "ProRes"),
            (b"Opus", "Opus"),
            (b"fLaC", "FLAC"),
            (b"alac", "ALAC"),
            (b"ac-3", "AC-3"),
            (b"ec-3", "E-AC-3"),
            (b".mp3", "MP3"),
            (b"xyz ", "xyz"),
        ] {
            assert_eq!(mp4_codec_name(fourcc), name);
        }
        for (id, name) in [
            ("V_MPEG4/ISO/AVC", "H.264"),
            ("V_MPEGH/ISO/HEVC", "HEVC"),
            ("V_VP8", "VP8"),
            ("V_AV1", "AV1"),
            ("V_MPEG4/ISO/ASP", "MPEG-4"),
            ("A_VORBIS", "Vorbis"),
            ("A_FLAC", "FLAC"),
            ("A_MPEG/L3", "MP3"),
            ("A_AAC/MPEG4/LC", "AAC"),
            ("V_THEORA", "THEORA"),
        ] {
            assert_eq!(matroska_codec_name(id), name);
        }
    }

    #[test]
    fn matroska_edge_cases_and_files() {
        // Float32 duration, unknown elements at every level, a subtitle
        // track, and a cluster that ends the header scan.
        let info_el = ebml(
            &[0x15, 0x49, 0xA9, 0x66],
            &[
                ebml(&[0x7B, 0xA9], b"title"),
                ebml(&[0x44, 0x89], &2500.0f32.to_be_bytes()),
            ]
            .concat(),
        );
        let video = ebml(
            &[0xAE],
            &[
                ebml(&[0xD7], &[1]),
                ebml(&[0x83], &[1]),
                ebml(&[0x86], b"V_MPEG4/ISO/AVC\0"),
                ebml(
                    &[0xE0],
                    &[ebml(&[0x54, 0xB0], &[1]), ebml(&[0xB0], &[0x80])].concat(),
                ),
            ]
            .concat(),
        );
        let subtitles = ebml(
            &[0xAE],
            &[ebml(&[0x83], &[17]), ebml(&[0x86], b"S_TEXT/UTF8")].concat(),
        );
        let audio = ebml(
            &[0xAE],
            &[ebml(&[0x83], &[2]), ebml(&[0x86], b"A_AAC")].concat(),
        );
        let tracks = ebml(
            &[0x16, 0x54, 0xAE, 0x6B],
            &[video, subtitles, audio].concat(),
        );
        let late_tracks = ebml(
            &[0x16, 0x54, 0xAE, 0x6B],
            &ebml(
                &[0xAE],
                &[ebml(&[0x83], &[1]), ebml(&[0x86], b"V_VP8")].concat(),
            ),
        );
        let segment = [
            ebml(&[0x11, 0x4D, 0x9B, 0x74], b"seek"),
            info_el,
            tracks,
            ebml(&[0x1F, 0x43, 0xB6, 0x75], b"frames"),
            late_tracks,
        ]
        .concat();
        let file = [
            ebml(&[0x1A, 0x45, 0xDF, 0xA3], b"\x42\x82\x84webm"),
            ebml(&[0x18, 0x53, 0x80, 0x67], &segment),
        ]
        .concat();
        let parsed = info(&file).unwrap();
        assert_eq!(parsed.duration, Some(2.5));
        assert_eq!(parsed.video_codec.as_deref(), Some("H.264"));
        assert_eq!(parsed.audio_codec.as_deref(), Some("AAC"));
        assert_eq!((parsed.width, parsed.height), (Some(128), None));
        assert_eq!(parsed.frame_rate, None);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.mkv");
        std::fs::write(&path, &file).unwrap();
        assert_eq!(info_from_file(&path), Some(parsed));

        // Malformed element headers and floats.
        assert_eq!(
            ebml_id(&[0x08, 0, 0, 0, 0], 0),
            None,
            "IDs are at most 4 bytes"
        );
        assert_eq!(ebml_size(&[0x00; 9], 0), None, "sizes are at most 8 bytes");
        assert_eq!(ebml_float(&[1, 2]), None);
        assert_eq!(ebml_uint(&[1; 9]), None);
        assert!(
            parse_matroska(&ebml(&[0x1A, 0x45, 0xDF, 0xA3], b"")).is_none(),
            "no segment"
        );
    }

    /// A WAV header with the given format code, then `data`.
    fn wav(format: u16, chunks_before: &[u8], data_size: u32, data: &[u8]) -> Vec<u8> {
        let mut out = b"RIFF\0\0\0\0WAVE".to_vec();
        out.extend(chunks_before);
        out.extend(b"fmt ");
        out.extend(16u32.to_le_bytes());
        out.extend(format.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(8000u32.to_le_bytes());
        out.extend(8000u32.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(8u16.to_le_bytes());
        out.extend(b"data");
        out.extend(data_size.to_le_bytes());
        out.extend(data);
        out
    }

    #[test]
    fn wav_variants() {
        for (format, codec) in [
            (3, "PCM float"),
            (6, "A-law"),
            (7, "µ-law"),
            (85, "WAV format 85"),
        ] {
            let parsed = info(&wav(format, b"", 8, &[0; 8])).unwrap();
            assert_eq!(parsed.audio_codec.as_deref(), Some(codec));
        }
        // An odd-sized chunk before `fmt ` is padded to an even length.
        let list = [&b"LIST"[..], &3u32.to_le_bytes(), b"abc\0"].concat();
        let parsed = info(&wav(1, &list, 4000, &[0; 4000])).unwrap();
        assert_eq!(parsed.duration, Some(0.5));

        // A truncated read trusts the declared size only when partial.
        let truncated = wav(1, b"", 16_000, &[0; 800]);
        assert_eq!(parse_wav(&truncated, false).unwrap().duration, Some(0.1));
        assert_eq!(parse_wav(&truncated, true).unwrap().duration, Some(2.0));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        std::fs::write(&path, wav(1, b"", 8000, &[0; 8000])).unwrap();
        assert_eq!(info_from_file(&path).unwrap().duration, Some(1.0));
    }

    #[test]
    fn rejects_garbage_and_formats_durations() {
        assert_eq!(info(b"not a media file"), None);
        assert_eq!(info(&[0, 0, 0, 0x20, b'f', b't', b'y', b'p']), None);
        assert_eq!(format_duration(4.4), "0:04");
        assert_eq!(format_duration(754.0), "12:34");
        assert_eq!(format_duration(3723.0), "1:02:03");
    }
}

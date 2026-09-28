//! Multimodal helpers: recognising media stored in binary columns (or
//! referenced by file path), reading image and audio/video metadata, making
//! thumbnails, video poster frames and filmstrips, and a simple colour
//! descriptor that makes images and videos searchable with `vector_search`.

use std::borrow::Cow;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use image::{DynamicImage, ImageFormat, ImageReader};

use crate::av::{self, AvInfo};
use crate::ffmpeg;

/// Dimension of [`color_vector`].
pub const COLOR_DIM: usize = 16;

/// Broad media category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Image,
    Audio,
    Video,
    Document,
}

impl MediaKind {
    pub fn label(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Audio => "audio",
            MediaKind::Video => "video",
            MediaKind::Document => "document",
        }
    }
}

/// A recognised media format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Format {
    pub kind: MediaKind,
    /// Short name, e.g. `PNG`.
    pub name: &'static str,
    pub mime: &'static str,
    pub extension: &'static str,
}

const fn format(
    kind: MediaKind,
    name: &'static str,
    mime: &'static str,
    extension: &'static str,
) -> Format {
    Format {
        kind,
        name,
        mime,
        extension,
    }
}

const PNG: Format = format(MediaKind::Image, "PNG", "image/png", "png");
const JPEG: Format = format(MediaKind::Image, "JPEG", "image/jpeg", "jpg");
const GIF: Format = format(MediaKind::Image, "GIF", "image/gif", "gif");
const WEBP: Format = format(MediaKind::Image, "WebP", "image/webp", "webp");
const BMP: Format = format(MediaKind::Image, "BMP", "image/bmp", "bmp");
const TIFF: Format = format(MediaKind::Image, "TIFF", "image/tiff", "tiff");
const WAV: Format = format(MediaKind::Audio, "WAV", "audio/wav", "wav");
const MP3: Format = format(MediaKind::Audio, "MP3", "audio/mpeg", "mp3");
const OGG: Format = format(MediaKind::Audio, "Ogg", "audio/ogg", "ogg");
const FLAC: Format = format(MediaKind::Audio, "FLAC", "audio/flac", "flac");
const M4A: Format = format(MediaKind::Audio, "M4A", "audio/mp4", "m4a");
const MP4: Format = format(MediaKind::Video, "MP4", "video/mp4", "mp4");
const MOV: Format = format(MediaKind::Video, "QuickTime", "video/quicktime", "mov");
const WEBM: Format = format(MediaKind::Video, "WebM/MKV", "video/webm", "webm");
const PDF: Format = format(MediaKind::Document, "PDF", "application/pdf", "pdf");

/// Recognises a media format from its leading bytes ("magic numbers").
pub fn sniff(bytes: &[u8]) -> Option<Format> {
    let starts = |magic: &[u8]| bytes.starts_with(magic);
    let at = |offset: usize, magic: &[u8]| bytes.get(offset..offset + magic.len()) == Some(magic);

    if starts(b"\x89PNG\r\n\x1a\n") {
        Some(PNG)
    } else if starts(b"\xff\xd8\xff") {
        Some(JPEG)
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        Some(GIF)
    } else if starts(b"RIFF") && at(8, b"WEBP") {
        Some(WEBP)
    } else if starts(b"RIFF") && at(8, b"WAVE") {
        Some(WAV)
    } else if starts(b"BM") && bytes.len() >= 26 && is_bmp_header(bytes) {
        Some(BMP)
    } else if starts(b"II*\0") || starts(b"MM\0*") {
        Some(TIFF)
    } else if starts(b"%PDF") {
        Some(PDF)
    } else if starts(b"OggS") {
        Some(OGG)
    } else if starts(b"fLaC") {
        Some(FLAC)
    } else if starts(b"ID3") || (bytes.len() > 2 && bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0) {
        Some(MP3)
    } else if at(4, b"ftyp") {
        match bytes.get(8..12) {
            Some(b"M4A ") | Some(b"M4B ") => Some(M4A),
            Some(b"qt  ") => Some(MOV),
            _ => Some(MP4),
        }
    } else if starts(b"\x1a\x45\xdf\xa3") {
        Some(WEBM)
    } else {
        None
    }
}

/// BMP's info-header size field only takes a handful of values.
fn is_bmp_header(bytes: &[u8]) -> bool {
    let size = u32::from_le_bytes([bytes[14], bytes[15], bytes[16], bytes[17]]);
    matches!(size, 12 | 40 | 52 | 56 | 64 | 108 | 124)
}

/// Recognises a media format from a file extension.
pub fn from_extension(path: &Path) -> Option<Format> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => PNG,
        "jpg" | "jpeg" => JPEG,
        "gif" => GIF,
        "webp" => WEBP,
        "bmp" => BMP,
        "tif" | "tiff" => TIFF,
        "wav" => WAV,
        "mp3" => MP3,
        "ogg" | "oga" => OGG,
        "flac" => FLAC,
        "m4a" => M4A,
        "mp4" | "m4v" => MP4,
        "mov" => MOV,
        "webm" | "mkv" => WEBM,
        "pdf" => PDF,
        _ => return None,
    })
}

/// Width and height of an encoded image, read from its header only.
pub fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// One-line description shown in grid cells, e.g. `PNG 120×180 · 12.3 KB`.
pub fn describe(bytes: &[u8]) -> String {
    let size = crate::results::human_bytes(bytes.len());
    match sniff(bytes) {
        Some(format) if format.kind == MediaKind::Image => match image_dimensions(bytes) {
            Some((w, h)) => format!("{} {w}×{h} · {size}", format.name),
            None => format!("{} image · {size}", format.name),
        },
        Some(format) if matches!(format.kind, MediaKind::Video | MediaKind::Audio) => {
            match av::info(bytes) {
                Some(info) => format!("{} · {size}", describe_av(format, &info)),
                None => format!("{} {} · {size}", format.name, format.kind.label()),
            }
        }
        Some(format) => format!("{} {} · {size}", format.name, format.kind.label()),
        None => {
            let preview: String = bytes.iter().take(8).map(|b| format!("{b:02x}")).collect();
            let more = if bytes.len() > 8 { "…" } else { "" };
            format!("0x{preview}{more} · {size}")
        }
    }
}

/// e.g. `MP4 1280×720 · H.264 · 0:12` or `WAV audio · PCM · 0:03`.
pub fn describe_av(format: Format, info: &AvInfo) -> String {
    let mut parts = vec![match (info.width, info.height) {
        (Some(w), Some(h)) => format!("{} {w}×{h}", format.name),
        _ => format!("{} {}", format.name, format.kind.label()),
    }];
    if let Some(codec) = info.codec() {
        parts.push(codec.to_string());
    }
    if let Some(duration) = info.duration {
        parts.push(av::format_duration(duration));
    }
    parts.join(" · ")
}

/// Decodes an image (any compiled-in codec).
pub fn decode(bytes: &[u8]) -> Option<DynamicImage> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()
}

fn shrink(image: &DynamicImage, max_side: u32) -> DynamicImage {
    if image.width() <= max_side && image.height() <= max_side {
        image.clone()
    } else {
        image.thumbnail(max_side, max_side)
    }
}

/// Encodes an image as PNG.
pub fn encode_png(image: &DynamicImage) -> Option<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    image.write_to(&mut out, ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// A PNG thumbnail (for storing next to large originals).
pub fn thumbnail_png(image: &DynamicImage, max_side: u32) -> Option<Vec<u8>> {
    encode_png(&shrink(image, max_side))
}

/// A 16-d colour descriptor: 12 hue bins weighted by saturation × value, plus
/// 4 lightness bins for greys, L2-normalised. Similar palettes → nearby
/// vectors, so `vector_search` finds visually similar images. (A colour
/// histogram, not a semantic embedding.)
pub fn color_vector(image: &DynamicImage) -> [f32; COLOR_DIM] {
    let small = image.thumbnail(64, 64).to_rgb8();
    let mut bins = [0f32; COLOR_DIM];
    for pixel in small.pixels() {
        let [r, g, b] = pixel.0.map(|c| f32::from(c) / 255.0);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let chroma = max - min;
        let saturation = if max > 0.0 { chroma / max } else { 0.0 };
        let value = max;
        if chroma > 0.0 {
            let hue = if max == r {
                ((g - b) / chroma).rem_euclid(6.0)
            } else if max == g {
                (b - r) / chroma + 2.0
            } else {
                (r - g) / chroma + 4.0
            } * 60.0;
            // Soft-assign to the two nearest hue-bin centres (15°, 45°, …),
            // wrapping around so that 350° and 5° land in neighbouring bins.
            let position = (hue - 15.0).rem_euclid(360.0) / 30.0;
            let lower = position.floor() as usize % 12;
            let upper = (lower + 1) % 12;
            let fraction = position.fract();
            let weight = saturation * value;
            bins[lower] += weight * (1.0 - fraction);
            bins[upper] += weight * fraction;
        }
        let grey = ((value * 4.0) as usize).min(3);
        bins[12 + grey] += 1.0 - saturation;
    }
    let norm = bins.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        bins.map(|v| v / norm)
    } else {
        bins
    }
}

/// If `text` is a path to an existing local image or video file, returns it.
pub fn visual_path(text: &str) -> Option<(PathBuf, Format)> {
    let text = text.trim();
    let path = Path::new(text.strip_prefix("file://").unwrap_or(text));
    let format = from_extension(path)?;
    (matches!(format.kind, MediaKind::Image | MediaKind::Video) && path.is_file())
        .then(|| (path.to_path_buf(), format))
}

/// Files referenced by path are only read into memory up to this size.
pub const MAX_READ_BYTES: u64 = 256 * 1024 * 1024;

/// A media value: bytes from a binary column, or a local file named by a
/// text column.
#[derive(Debug, Clone)]
pub enum MediaValue {
    Bytes(Vec<u8>),
    File(PathBuf),
}

impl MediaValue {
    pub fn format(&self) -> Option<Format> {
        match self {
            MediaValue::Bytes(bytes) => sniff(bytes),
            MediaValue::File(path) => from_extension(path),
        }
    }

    pub fn kind(&self) -> Option<MediaKind> {
        self.format().map(|format| format.kind)
    }

    /// The raw bytes (files larger than [`MAX_READ_BYTES`] are not read).
    pub fn bytes(&self) -> Option<Cow<'_, [u8]>> {
        match self {
            MediaValue::Bytes(bytes) => Some(Cow::Borrowed(bytes)),
            MediaValue::File(path) => {
                if std::fs::metadata(path).ok()?.len() > MAX_READ_BYTES {
                    return None;
                }
                std::fs::read(path).ok().map(Cow::Owned)
            }
        }
    }

    /// Audio/video metadata (files are read only as far as needed).
    pub fn av_info(&self) -> Option<AvInfo> {
        match self {
            MediaValue::Bytes(bytes) => av::info(bytes),
            MediaValue::File(path) => av::info_from_file(path),
        }
    }

    /// One-line description, as shown in grid cells and gallery captions.
    pub fn describe(&self) -> String {
        let path = match self {
            MediaValue::Bytes(bytes) => return describe(bytes),
            MediaValue::File(path) => path,
        };
        let size = std::fs::metadata(path).map_or(0, |m| m.len() as usize);
        let size = crate::results::human_bytes(size);
        let Some(format) = self.format() else {
            return format!("file · {size}");
        };
        match format.kind {
            MediaKind::Image => {
                let dims = ImageReader::open(path)
                    .ok()
                    .and_then(|reader| reader.with_guessed_format().ok())
                    .and_then(|reader| reader.into_dimensions().ok());
                match dims {
                    Some((w, h)) => format!("{} {w}×{h} · {size}", format.name),
                    None => format!("{} image · {size}", format.name),
                }
            }
            MediaKind::Video | MediaKind::Audio => match self.av_info() {
                Some(info) => format!("{} · {size}", describe_av(format, &info)),
                None => format!("{} {} · {size}", format.name, format.kind.label()),
            },
            MediaKind::Document => format!("{} document · {size}", format.name),
        }
    }

    /// Runs `f` with a seekable file holding the video (spilling bytes to a
    /// temporary file when needed).
    fn with_file<T>(&self, f: impl FnOnce(&Path) -> T) -> Option<T> {
        match self {
            MediaValue::File(path) => Some(f(path)),
            MediaValue::Bytes(bytes) => {
                let extension = self.format().map_or("bin", |format| format.extension);
                let spilled = ffmpeg::SpilledVideo::new(bytes, extension)?;
                Some(f(spilled.path()))
            }
        }
    }

    /// A still picture: the image itself, or a video's poster frame (video
    /// frames need ffmpeg). Scaled to fit `max_side`.
    pub fn still(&self, max_side: u32) -> Option<DynamicImage> {
        match self.kind()? {
            MediaKind::Image => Some(shrink(&decode(&self.bytes()?)?, max_side)),
            MediaKind::Video if ffmpeg::available() => {
                let time = ffmpeg::poster_time(self.av_info().and_then(|info| info.duration));
                self.with_file(|path| ffmpeg::frame_at(path, time, max_side))?
            }
            _ => None,
        }
    }

    /// `count` evenly spaced frames of a video (needs ffmpeg).
    pub fn filmstrip(&self, count: usize, max_side: u32) -> Vec<DynamicImage> {
        if self.kind() != Some(MediaKind::Video) || !ffmpeg::available() {
            return Vec::new();
        }
        let Some(duration) = self.av_info().and_then(|info| info.duration) else {
            return Vec::new();
        };
        let times = ffmpeg::filmstrip_times(duration, count);
        self.with_file(|path| ffmpeg::frames_at(path, &times, max_side))
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    fn png(width: u32, height: u32, color: [u8; 3]) -> Vec<u8> {
        let image = DynamicImage::ImageRgb8(RgbImage::from_pixel(width, height, Rgb(color)));
        encode_png(&image).unwrap()
    }

    #[test]
    fn sniffs_common_formats() {
        assert_eq!(sniff(&png(2, 2, [0, 0, 0])), Some(PNG));
        assert_eq!(sniff(b"\xff\xd8\xff\xe0rest"), Some(JPEG));
        assert_eq!(sniff(b"GIF89a...."), Some(GIF));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some(WEBP));
        assert_eq!(sniff(b"RIFF\0\0\0\0WAVEfmt "), Some(WAV));
        assert_eq!(sniff(b"%PDF-1.7"), Some(PDF));
        assert_eq!(sniff(b"\0\0\0\x18ftypisom"), Some(MP4));
        assert_eq!(sniff(b"\0\0\0\x18ftypM4A "), Some(M4A));
        assert_eq!(sniff(b"ID3\x04"), Some(MP3));
        assert_eq!(sniff(b"BM not really a bitmap"), None);
        assert_eq!(sniff(b"hello"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn describes_images_and_blobs() {
        assert_eq!(image_dimensions(&png(12, 7, [1, 2, 3])), Some((12, 7)));
        assert!(describe(&png(12, 7, [1, 2, 3])).starts_with("PNG 12×7 · "));
        assert_eq!(describe(&[0xde, 0xad, 0xbe, 0xef]), "0xdeadbeef · 4 B");
        assert!(describe(b"%PDF-1.7 ...").starts_with("PDF document · "));
    }

    #[test]
    fn thumbnails_fit_the_bounding_box() {
        let thumb = MediaValue::Bytes(png(400, 100, [9, 9, 9]))
            .still(80)
            .unwrap();
        assert_eq!((thumb.width(), thumb.height()), (80, 20));
        let small = MediaValue::Bytes(png(10, 10, [9, 9, 9])).still(80).unwrap();
        assert_eq!((small.width(), small.height()), (10, 10));
    }

    #[test]
    fn color_vectors_separate_hues() {
        let red = color_vector(&decode(&png(8, 8, [220, 20, 20])).unwrap());
        let crimson = color_vector(&decode(&png(8, 8, [200, 10, 40])).unwrap());
        let blue = color_vector(&decode(&png(8, 8, [20, 40, 220])).unwrap());
        let dot = |a: &[f32; 16], b: &[f32; 16]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        assert!((dot(&red, &red) - 1.0).abs() < 1e-5);
        assert!(dot(&red, &crimson) > dot(&red, &blue));
    }

    #[test]
    fn describes_audio_and_video() {
        let wav = [
            &b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0"[..],
            &8000u32.to_le_bytes(),
            &16_000u32.to_le_bytes(),
            b"\x02\0\x10\0data",
            &32_000u32.to_le_bytes(),
            &[0u8; 32_000],
        ]
        .concat();
        assert!(describe(&wav).starts_with("WAV audio · PCM · 0:02 · "));
        let value = MediaValue::Bytes(wav);
        assert_eq!(value.kind(), Some(MediaKind::Audio));
        assert!(value.still(64).is_none());
        assert!(value.filmstrip(4, 64).is_empty());
    }

    #[test]
    fn video_stills_when_ffmpeg_exists() {
        if !ffmpeg::available() {
            eprintln!("skipping: ffmpeg not found");
            return;
        }
        let clip =
            ffmpeg::synthesize_clip(2.0, (160, 90), [0x223344, 0xaa5500, 0x0055aa], 3).unwrap();
        assert!(describe(&clip).starts_with("MP4 160×90 · "));
        let value = MediaValue::Bytes(clip);
        let still = value.still(80).unwrap();
        assert_eq!((still.width(), still.height()), (80, 45));
        assert_eq!(value.filmstrip(4, 40).len(), 4);
    }

    #[test]
    fn extensions_and_paths() {
        assert_eq!(from_extension(Path::new("a/B.JPEG")), Some(JPEG));
        assert_eq!(from_extension(Path::new("notes.txt")), None);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.png");
        std::fs::write(&file, png(1, 1, [0, 0, 0])).unwrap();
        assert_eq!(
            visual_path(file.to_str().unwrap()),
            Some((file.clone(), PNG))
        );
        assert_eq!(
            visual_path(&format!("file://{}", file.display())),
            Some((file, PNG))
        );
        assert_eq!(visual_path("/definitely/missing.png"), None);
        assert_eq!(visual_path("/tmp"), None);
    }
}

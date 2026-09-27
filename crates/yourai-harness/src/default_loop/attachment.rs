//! Attachment intake: one [`UserAttachment`] in → one genai [`ContentPart`] out.
//!
//! Two source forms (see [`yourai_core::protocol::AttachmentData`]):
//! - **Base64** media: images are normalized first (opencode `image.ts`:
//!   5 MB base64 / 2000×2000 ceilings, auto-resize before rejection), audio
//!   and PDF pass through — their sizes are provider-side concerns.
//! - **File** references (opencode FilePart semantics): the harness reads the
//!   file so frontends never inline content. Text files become bounded text
//!   parts (optional 1-based line window, char cap), images go through the
//!   same normalization ladder, directories expand to a first-level listing,
//!   audio/PDF become Binary parts.

use std::path::{Path, PathBuf};
use yourai_core::prelude::*;

use base64::{
    engine::{
        general_purpose::{GeneralPurpose, GeneralPurposeConfig},
        DecodePaddingMode,
    },
    Engine as _,
};

/// Standard alphabet, tolerant of missing padding on decode (frontends may
/// emit unpadded base64); always encodes with padding.
static BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// opencode JPEG quality ladder (80 deliberately before 85).
const JPEG_QUALITIES: [u8; 5] = [80, 85, 70, 55, 40];

/// Resolve one attachment into a model-visible content part.
pub(super) fn resolve_attachment(
    att: &UserAttachment,
    config: &super::LoopConfig,
) -> Result<ContentPart, YourAiError> {
    match &att.data {
        AttachmentData::Base64(data) => {
            let ct = att.content_type.trim().to_ascii_lowercase();
            if !(ct.starts_with("image/") || ct.starts_with("audio/") || ct == "application/pdf") {
                return Err(ErrorKind::Config(format!(
                    "unsupported attachment type '{ct}'; only image/*, audio/*, and application/pdf are accepted"
                ))
                .into());
            }
            let (mime, payload) = if ct.starts_with("image/") {
                normalize_image(data, &ct, &config.attachment_image)?
            } else {
                (ct, data.clone())
            };
            binary_part(mime, payload, att.name.clone())
        }
        AttachmentData::File(file) => resolve_file(file, config),
    }
}

// ── base64 media ───────────────────────────────────────────────────────

/// How genai handles Binary per provider:
///
/// | Provider  | image (base64)          | audio (base64)             | PDF / other (base64)         |
/// |-----------|-------------------------|----------------------------|------------------------------|
/// | OpenAI    | `image_url` + data URL  | `input_audio`              | `file` + file_data (data URL)|
/// | Anthropic | `image` + base64 source | `document` + base64 source | `document` + base64 source   |
///
/// Binary parts are only processed in user-role messages (genai adapters
/// ignore them in assistant/tool roles); we only emit `BinarySource::Base64`,
/// so both providers are covered (URL-sourced binaries have provider gaps).
fn binary_part(
    mime: String,
    data: String,
    name: Option<String>,
) -> Result<ContentPart, YourAiError> {
    Ok(ContentPart::from_binary_base64(mime, data.as_str(), name))
}

fn size_error(
    width: u32,
    height: u32,
    bytes: usize,
    cfg: &super::AttachmentImageConfig,
) -> YourAiError {
    ErrorKind::Config(format!(
        "Image {width}x{height} with base64 size {bytes} exceeds configured limits and could not be resized below {}x{}/{} bytes",
        cfg.max_width, cfg.max_height, cfg.max_base64_bytes
    ))
    .into()
}

/// Decode, size-check and (if needed) resize one image payload.
/// Returns the effective MIME type and base64 data.
fn normalize_image(
    data: &str,
    ct: &str,
    cfg: &super::AttachmentImageConfig,
) -> Result<(String, String), YourAiError> {
    let bytes = BASE64
        .decode(data.as_bytes())
        .map_err(|_| ErrorKind::Config("attachment image is not valid base64".into()))?;
    let image = image::load_from_memory(&bytes)
        .map_err(|_| ErrorKind::Config("attachment image could not be decoded".into()))?;
    let (width, height) = (image.width(), image.height());
    if width <= cfg.max_width && height <= cfg.max_height && data.len() <= cfg.max_base64_bytes {
        // Fast path: within limits, pass the original payload through
        // untouched (no re-encode, no quality loss).
        return Ok((ct.to_owned(), data.to_owned()));
    }
    if !cfg.auto_resize {
        return Err(size_error(width, height, data.len(), cfg));
    }
    let scale = 1f64
        .min(cfg.max_width as f64 / width as f64)
        .min(cfg.max_height as f64 / height as f64);
    let mut size = (
        ((width as f64 * scale).round() as u32).max(1),
        ((height as f64 * scale).round() as u32).max(1),
    );
    // At most 32 candidate sizes, shrinking ×0.75 each step (opencode ladder).
    for _ in 0..32 {
        let resized = image.resize_exact(size.0, size.1, image::imageops::FilterType::Lanczos3);
        let mut candidates = vec![("image/png", encode_png(&resized))];
        for quality in JPEG_QUALITIES {
            candidates.push(("image/jpeg", encode_jpeg(&resized, quality)));
        }
        let encoded = candidates
            .into_iter()
            .map(|(mime, result)| result.map(|data| (mime, data)))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some((mime, data)) = encoded
            .into_iter()
            .find(|(_, data)| data.len() <= cfg.max_base64_bytes)
        {
            return Ok((mime.into(), data));
        }
        let next = (shrink_step(size.0), shrink_step(size.1));
        if next == size {
            break;
        }
        size = next;
    }
    Err(size_error(width, height, data.len(), cfg))
}

fn shrink_step(dim: u32) -> u32 {
    if dim == 1 {
        1
    } else {
        ((dim as f64 * 0.75).floor() as u32).max(1)
    }
}

fn encode_png(image: &image::DynamicImage) -> Result<String, YourAiError> {
    let mut buf = Vec::new();
    image
        .to_rgb8()
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .map_err(|e| ErrorKind::Config(format!("png re-encode failed: {e}")))?;
    Ok(BASE64.encode(&buf))
}

fn encode_jpeg(image: &image::DynamicImage, quality: u8) -> Result<String, YourAiError> {
    let mut buf = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    image
        .to_rgb8()
        .write_with_encoder(encoder)
        .map_err(|e| ErrorKind::Config(format!("jpeg re-encode failed: {e}")))?;
    Ok(BASE64.encode(&buf))
}

// ── file references ────────────────────────────────────────────────────

/// How a referenced path is turned into model content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Directory,
    Image,
    Audio,
    Pdf,
    Text,
    /// Known-binary extension that we cannot represent (archives, media
    /// containers outside the audio set, executables, …).
    Other,
}

const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "avif", "bmp", "tiff", "tif",
];
const AUDIO_EXTS: &[&str] = &["mp3", "wav", "ogg", "m4a", "flac", "aac", "opus"];
const TEXT_EXTS: &[&str] = &[
    "rs",
    "go",
    "py",
    "js",
    "ts",
    "tsx",
    "jsx",
    "java",
    "c",
    "cpp",
    "h",
    "hpp",
    "cs",
    "rb",
    "php",
    "swift",
    "kt",
    "scala",
    "clj",
    "ex",
    "exs",
    "erl",
    "lua",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "bat",
    "cmd",
    "sql",
    "graphql",
    "proto",
    "thrift",
    "toml",
    "yaml",
    "yml",
    "json",
    "xml",
    "html",
    "css",
    "scss",
    "sass",
    "less",
    "md",
    "markdown",
    "rst",
    "txt",
    "log",
    "ini",
    "cfg",
    "conf",
    "env",
    "gitignore",
    "dockerignore",
    "dockerfile",
    "makefile",
    "cmake",
    "gradle",
    "csv",
    "tsv",
    "lock",
    "diff",
    "patch",
    "vim",
    "el",
    "lisp",
    "hs",
    "ml",
    "nim",
    "zig",
    "v",
    "dart",
    "r",
    "jl",
    "pl",
    "asm",
    "s",
    "wasm",
    "wat",
];

fn classify_path(path: &Path) -> FileKind {
    if path.is_dir() {
        return FileKind::Directory;
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if ext.is_empty() {
        // Extensionless files (README, Makefile, LICENSE) are read as text.
        return FileKind::Text;
    }
    if IMAGE_EXTS.contains(&ext.as_str()) {
        FileKind::Image
    } else if AUDIO_EXTS.contains(&ext.as_str()) {
        FileKind::Audio
    } else if ext == "pdf" {
        FileKind::Pdf
    } else if TEXT_EXTS.contains(&ext.as_str()) {
        FileKind::Text
    } else {
        FileKind::Other
    }
}

fn resolve_file(file: &FileRef, config: &super::LoopConfig) -> Result<ContentPart, YourAiError> {
    let path = PathBuf::from(&file.path);
    if !path.exists() {
        return Err(ErrorKind::Config(format!("attachment file not found: {}", file.path)).into());
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let display = &file.path;
    match classify_path(&path) {
        FileKind::Directory => directory_part(&path, display),
        FileKind::Image => {
            let data = read_file_base64(&path, display)?;
            let (mime, payload) =
                normalize_image(&data, "image/png", &config.attachment_image)?;
            binary_part(mime, payload, path.file_name().and_then(|n| n.to_str()).map(String::from))
        }
        FileKind::Audio => {
            let data = read_file_base64(&path, display)?;
            let mime = format!("audio/{ext}");
            binary_part(
                mime,
                data,
                path.file_name().and_then(|n| n.to_str()).map(String::from),
            )
        }
        FileKind::Pdf => {
            let data = read_file_base64(&path, display)?;
            binary_part(
                "application/pdf".into(),
                data,
                path.file_name().and_then(|n| n.to_str()).map(String::from),
            )
        }
        FileKind::Text => text_part(&path, &ext, file.lines, config.attachment_text_max_chars, display),
        FileKind::Other => Err(ErrorKind::Config(format!(
            "unsupported attachment file '{display}': unrecognized extension '{ext}' (only text, images, audio, and PDF are supported)"
        ))
        .into()),
    }
}

fn read_file_base64(path: &Path, display: &str) -> Result<String, YourAiError> {
    let bytes = std::fs::read(path)
        .map_err(|e| ErrorKind::Config(format!("attachment file read failed ({display}): {e}")))?;
    Ok(BASE64.encode(&bytes))
}

/// Read a text file, apply the optional line window and char cap, and format
/// it as a fenced block introduced by a provenance note.
fn text_part(
    path: &Path,
    ext: &str,
    lines: Option<(u32, u32)>,
    max_chars: usize,
    display: &str,
) -> Result<ContentPart, YourAiError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| ErrorKind::Config(format!("attachment file read failed ({display}): {e}")))?;
    let total = content.lines().count() as u32;
    let (selected, range) = match lines {
        Some((start, end)) => {
            let start = start.max(1);
            if end < start {
                return Err(ErrorKind::Config(format!(
                    "attachment line range {start}-{end} is empty"
                ))
                .into());
            }
            if start > total {
                return Err(ErrorKind::Config(format!(
                    "attachment line range {start}-{end} exceeds file length ({total} lines)"
                ))
                .into());
            }
            let end = end.min(total);
            let window: Vec<&str> = content
                .lines()
                .skip(start as usize - 1)
                .take(end as usize - start as usize + 1)
                .collect();
            (window.join("\n"), format!("lines {start}-{end}"))
        }
        None => (content.trim_end().to_owned(), "full file".to_owned()),
    };
    let char_count = selected.chars().count();
    let (body, truncation) = if char_count > max_chars {
        (
            selected.chars().take(max_chars).collect::<String>(),
            format!(", truncated to {max_chars} chars"),
        )
    } else {
        (selected, String::new())
    };
    // A body containing ``` would close a plain fence; escalate to 4 backticks.
    let fence = if body.contains("```") { "````" } else { "```" };
    let block = format!(
        "[Attached file {display} ({range}, {char_count} chars{truncation})]\n{fence}{ext}\n{body}\n{fence}"
    );
    Ok(ContentPart::from_text(block))
}

/// First-level directory listing, formatted like opencode's Read tool output.
fn directory_part(path: &Path, display: &str) -> Result<ContentPart, YourAiError> {
    let entries = std::fs::read_dir(path).map_err(|e| {
        ErrorKind::Config(format!("attachment directory read failed ({display}): {e}"))
    })?;
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            dirs.push(format!("{name}/"));
        } else {
            files.push(name);
        }
    }
    dirs.sort();
    files.sort();
    let total = dirs.len() + files.len();
    let mut listing = format!(
        "[Attached directory {display}]\n<path>{}</path>\n<type>directory</type>\n<entries>\n",
        path.display()
    );
    for name in dirs.iter().chain(files.iter()) {
        listing.push_str(name);
        listing.push('\n');
    }
    listing.push_str(&format!("({total} entries)\n</entries>"));
    Ok(ContentPart::from_text(listing))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> super::super::LoopConfig {
        super::super::LoopConfig::default()
    }

    fn att(content_type: &str, data: &str) -> UserAttachment {
        UserAttachment::base64(content_type, data, Some("test.bin".into()))
    }

    fn limits() -> super::super::AttachmentImageConfig {
        super::super::AttachmentImageConfig {
            auto_resize: true,
            max_width: 64,
            max_height: 64,
            max_base64_bytes: 5 * 1024 * 1024,
        }
    }

    fn blank_png_base64(width: u32, height: u32) -> String {
        let img = image::DynamicImage::new_rgb8(width, height);
        encode_png(&img).unwrap()
    }

    fn noisy_png_base64(width: u32, height: u32) -> String {
        let mut buf = image::RgbImage::new(width, height);
        for (x, y, pixel) in buf.enumerate_pixels_mut() {
            let v =
                ((x.wrapping_mul(31) as u64 + y.wrapping_mul(17) as u64 + (x as u64) * (y as u64))
                    % 256) as u8;
            *pixel = image::Rgb([v, v.wrapping_mul(3), v.wrapping_mul(7)]);
        }
        encode_png(&image::DynamicImage::from(buf)).unwrap()
    }

    fn part_dims(part: &ContentPart) -> (u32, u32) {
        let binary = part.as_binary().unwrap();
        let BinarySource::Base64(data) = &binary.source else {
            panic!("expected base64 source");
        };
        let bytes = BASE64.decode(data.as_bytes()).unwrap();
        let img = image::load_from_memory(&bytes).unwrap();
        (img.width(), img.height())
    }

    fn first_text(part: &ContentPart) -> String {
        match part {
            ContentPart::Text(text) => text.clone(),
            _ => panic!("expected text part"),
        }
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ── base64 media ───────────────────────────────────────────────────

    #[test]
    fn image_attachment_within_limits_passes_through_unchanged() {
        let data = blank_png_base64(8, 8);
        let part = resolve_attachment(&att("image/png", &data), &config()).unwrap();
        let binary = part.as_binary().unwrap();
        assert!(binary.is_image());
        assert_eq!(binary.content_type, "image/png");
        assert_eq!(binary.name.as_deref(), Some("test.bin"));
        assert!(
            matches!(&binary.source, BinarySource::Base64(b) if b.as_ref() == data),
            "within-limit image must not be re-encoded"
        );
    }

    #[test]
    fn oversized_image_is_resized_by_default() {
        let mut cfg = config();
        cfg.attachment_image = limits();
        let part =
            resolve_attachment(&att("image/png", &noisy_png_base64(100, 100)), &cfg).unwrap();
        let (w, h) = part_dims(&part);
        assert!(w <= 64 && h <= 64, "resized to {w}x{h}");
        assert!(part.as_binary().unwrap().is_image());
    }

    #[test]
    fn byte_limit_drives_further_shrinking() {
        let mut cfg = config();
        cfg.attachment_image = super::super::AttachmentImageConfig {
            auto_resize: true,
            max_width: 64,
            max_height: 64,
            max_base64_bytes: 200,
        };
        let part =
            resolve_attachment(&att("image/png", &noisy_png_base64(200, 200)), &cfg).unwrap();
        let (w, h) = part_dims(&part);
        assert!(w <= 64 && h <= 64);
        let BinarySource::Base64(data) = &part.as_binary().unwrap().source else {
            panic!()
        };
        assert!(data.len() <= 200, "base64 len {} exceeds limit", data.len());
    }

    #[test]
    fn oversized_image_without_auto_resize_is_rejected() {
        let mut cfg = config();
        cfg.attachment_image = super::super::AttachmentImageConfig {
            auto_resize: false,
            ..limits()
        };
        let err =
            resolve_attachment(&att("image/png", &noisy_png_base64(100, 100)), &cfg).unwrap_err();
        assert!(err.to_string().contains("exceeds configured limits"));
    }

    #[test]
    fn invalid_image_data_is_rejected() {
        let err = resolve_attachment(&att("image/png", "aGVsbG8="), &config()).unwrap_err();
        assert!(err.to_string().contains("could not be decoded"));
    }

    #[test]
    fn pdf_and_audio_pass_through_without_size_checks() {
        let pdf = resolve_attachment(&att("application/pdf", "JVBERi0="), &config()).unwrap();
        assert!(pdf.as_binary().unwrap().is_pdf());
        let audio = resolve_attachment(&att("audio/wav", "UklGRiQ="), &config()).unwrap();
        assert!(audio.as_binary().unwrap().is_audio());
    }

    #[test]
    fn unsupported_content_type_is_rejected() {
        let err = resolve_attachment(&att("text/plain", "aGVsbG8="), &config()).unwrap_err();
        assert!(err.to_string().contains("unsupported attachment type"));
    }

    #[test]
    fn content_type_is_case_insensitive_and_trimmed() {
        let data = blank_png_base64(8, 8);
        let part = resolve_attachment(&att("  IMAGE/PNG  ", &data), &config()).unwrap();
        assert_eq!(part.as_binary().unwrap().content_type, "image/png");
    }

    // ── file references ────────────────────────────────────────────────

    #[test]
    fn file_ref_text_becomes_bounded_fenced_block() {
        let dir = tempdir("yourai_att_text");
        std::fs::write(dir.join("main.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        let att = UserAttachment::file(dir.join("main.rs").to_string_lossy().into_owned(), None);
        let part = resolve_attachment(&att, &config()).unwrap();
        let text = first_text(&part);
        assert!(text.contains("[Attached file"), "{text}");
        assert!(text.contains("full file"), "{text}");
        assert!(text.contains("```rs\nfn a() {}"), "{text}");
        assert!(text.contains("fn c() {}"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_ref_line_window_selects_range() {
        let dir = tempdir("yourai_att_lines");
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let path = dir.join("a.txt").to_string_lossy().into_owned();
        let att = UserAttachment::file(path, Some((2, 3)));
        let text = first_text(&resolve_attachment(&att, &config()).unwrap());
        assert!(text.contains("lines 2-3"), "{text}");
        assert!(text.contains("two"), "{text}");
        assert!(text.contains("three"), "{text}");
        assert!(!text.contains("one\n"), "{text}");
        assert!(!text.contains("four"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_ref_line_window_clamps_end_and_rejects_out_of_range() {
        let dir = tempdir("yourai_att_clamp");
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        let path = dir.join("a.txt").to_string_lossy().into_owned();
        let att = UserAttachment::file(path.clone(), Some((2, 99)));
        let text = first_text(&resolve_attachment(&att, &config()).unwrap());
        assert!(text.contains("lines 2-2"), "{text}");
        let att = UserAttachment::file(path.clone(), Some((9, 10)));
        assert!(resolve_attachment(&att, &config())
            .unwrap_err()
            .to_string()
            .contains("exceeds file length"));
        let att = UserAttachment::file(path, Some((3, 2)));
        assert!(resolve_attachment(&att, &config())
            .unwrap_err()
            .to_string()
            .contains("empty"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_ref_text_truncates_to_cap() {
        let dir = tempdir("yourai_att_trunc");
        let big = "x".repeat(70_000);
        std::fs::write(dir.join("big.log"), &big).unwrap();
        let att = UserAttachment::file(dir.join("big.log").to_string_lossy().into_owned(), None);
        let text = first_text(&resolve_attachment(&att, &config()).unwrap());
        assert!(text.contains("truncated to 50000 chars"), "{text}");
        assert!(text.chars().count() < 55_000, "text not truncated");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_ref_image_normalizes_like_base64() {
        let dir = tempdir("yourai_att_img");
        let img = image::DynamicImage::new_rgb8(100, 100);
        let mut png = Vec::new();
        img.to_rgb8()
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        std::fs::write(dir.join("shot.png"), &png).unwrap();
        let mut cfg = config();
        cfg.attachment_image = limits();
        let att = UserAttachment::file(dir.join("shot.png").to_string_lossy().into_owned(), None);
        let part = resolve_attachment(&att, &cfg).unwrap();
        let (w, h) = part_dims(&part);
        assert!(w <= 64 && h <= 64, "file image normalized to {w}x{h}");
        assert_eq!(part.as_binary().unwrap().name.as_deref(), Some("shot.png"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_ref_unknown_binary_extension_is_rejected() {
        let dir = tempdir("yourai_att_other");
        std::fs::write(dir.join("archive.zip"), b"PK\x03\x04").unwrap();
        let att =
            UserAttachment::file(dir.join("archive.zip").to_string_lossy().into_owned(), None);
        assert!(resolve_attachment(&att, &config())
            .unwrap_err()
            .to_string()
            .contains("unrecognized extension"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_ref_pdf_and_directory() {
        let dir = tempdir("yourai_att_pdfdir");
        std::fs::write(dir.join("doc.pdf"), b"%PDF-1.4 fake").unwrap();
        let att = UserAttachment::file(dir.join("doc.pdf").to_string_lossy().into_owned(), None);
        let part = resolve_attachment(&att, &config()).unwrap();
        assert!(part.as_binary().unwrap().is_pdf());

        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("alpha.rs"), "").unwrap();
        let att = UserAttachment::file(dir.to_string_lossy().into_owned(), None);
        let text = first_text(&resolve_attachment(&att, &config()).unwrap());
        assert!(text.contains("[Attached directory"), "{text}");
        assert!(text.contains("<type>directory</type>"), "{text}");
        assert!(text.contains("sub/"), "{text}");
        assert!(text.contains("doc.pdf"), "{text}");
        assert!(text.contains("(3 entries)"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_ref_missing_and_unreadable_fail_clearly() {
        let att = UserAttachment::file("/nonexistent_yourai_att/x.rs", None);
        assert!(resolve_attachment(&att, &config())
            .unwrap_err()
            .to_string()
            .contains("not found"));
    }

    #[test]
    fn file_ref_extensionless_is_text() {
        let dir = tempdir("yourai_att_noext");
        std::fs::write(dir.join("Makefile"), "all:\n\techo hi\n").unwrap();
        let att = UserAttachment::file(dir.join("Makefile").to_string_lossy().into_owned(), None);
        let text = first_text(&resolve_attachment(&att, &config()).unwrap());
        assert!(text.contains("echo hi"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

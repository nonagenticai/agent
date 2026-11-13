//! Validation of an untrusted upload (SW-9 design §2/§6; integration-owner conditions 1, 2 and 5). The one entry is
//! [`accept`]; nothing else produces an [`Accepted`], and only an [`Accepted`] reaches the store.
//!
//! Order, cheapest and most hostile-input-resistant first:
//! 1. size (≤ 10 MB) and a magic-byte sniff (JPEG, PNG, WebP, PDF) — never the extension or a declared type;
//! 2. images: the declared dimensions are read from the header with a small bounds-checked parser BEFORE any decoder
//!    allocates (≤ 4096 × 4096 and ≤ 16 777 216 px), animated / multi-frame input is refused, the decode runs under
//!    `image::Limits`, and the pixels are RE-ENCODED (JPEG q85, PNG, lossless WebP), which drops every metadata block
//!    (EXIF, GPS, ICC, XMP, trailing data) and any polyglot payload. EXIF orientation is applied first so a phone
//!    photo does not turn sideways once the tag is gone;
//! 3. PDFs: `%PDF-` header, a name-token scan of the raw bytes (with `#xx` escapes decoded) AND a walk of every parsed
//!    object (so names inside compressed object streams are seen) for the active-content keys, parsed by `lopdf` on a
//!    worker thread under an object-count, decompressed-size and time budget. Any parser error refuses; an encrypted
//!    file is refused (its object streams cannot be inspected). An accepted PDF is stored byte-identical.
//!
//! [`accept`] is CPU-bound (up to a 16 Mpx decode and encode): call it from `spawn_blocking`.
//!
//! Refusal reasons are a closed set of `&'static str` ([`reason`]); they are the metric label and the API's machine
//! code. Names and bytes are never logged or labelled.
use std::io::Cursor;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;
use std::time::Duration;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::codecs::webp::WebPEncoder;
use image::error::ImageError;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};

use crate::runtime::resolve::contains_id_pattern;

/// The largest file accepted, and the largest result stored (10 MiB).
pub const MAX_BYTES: usize = 10 * 1024 * 1024;
/// The longest image side accepted (pixels).
pub const MAX_SIDE: u32 = 4096;
/// The most pixels an image may declare (4096 × 4096).
pub const MAX_PIXELS: u64 = 16_777_216;
/// The longest display name (characters), extension included.
pub const MAX_NAME_CHARS: usize = 80;
/// The decoder's allocation ceiling: a 4096 × 4096 RGBA frame is 64 MiB.
const DECODE_MAX_ALLOC: u64 = 256 * 1024 * 1024;
/// JPEG re-encode quality.
const JPEG_QUALITY: u8 = 85;

/// The most indirect objects a PDF may hold.
const PDF_MAX_OBJECTS: usize = 50_000;
/// The most bytes any one PDF stream the loader decodes (object / xref streams) may inflate to.
const PDF_MAX_DECOMPRESSED: usize = 32 * 1024 * 1024;
/// The wall-clock budget of the PDF parse. On expiry the upload is refused (the worker is left to finish alone).
const PDF_TIME_BUDGET: Duration = Duration::from_secs(5);
/// Deepest object nesting the walk follows (lopdf itself stops at 100).
const PDF_MAX_DEPTH: usize = 200;
/// Longest name token the raw scan compares.
const PDF_MAX_NAME: usize = 64;

/// Why an upload was refused. A closed set: each is also a metric label value.
pub mod reason {
    /// Not JPEG / PNG / WebP / PDF by its magic bytes (including an empty body).
    pub const UNSUPPORTED_TYPE: &str = "unsupported_type";
    /// Over 10 MiB (before or after re-encoding).
    pub const TOO_LARGE: &str = "too_large";
    /// More than 4096 px on a side, more than 16 777 216 px, or a decoder limit tripped.
    pub const TOO_MANY_PIXELS: &str = "too_many_pixels";
    /// Animated or multi-frame (APNG, animated WebP).
    pub const ANIMATED: &str = "animated";
    /// An image that does not decode (or whose header lies about its size).
    pub const MALFORMED_IMAGE: &str = "malformed_image";
    /// A PDF the parser could not read within its limits.
    pub const MALFORMED_PDF: &str = "malformed_pdf";
    /// A PDF with JavaScript, a launch action, an embedded file, rich media, XFA or additional actions.
    pub const ACTIVE_PDF: &str = "active_pdf";
    /// An encrypted PDF.
    pub const ENCRYPTED_PDF: &str = "encrypted_pdf";
    /// The tenant's attachment quota is used up (raised by the store, counted here for one series).
    pub const QUOTA: &str = "quota";
    /// artifact_service's scanner convicted the file (raised by the store, not by [`accept`](super::accept)).
    pub const MALWARE: &str = "malware";
    /// The scanner could not run: fail closed (raised by the store).
    pub const SCAN_UNAVAILABLE: &str = "scan_unavailable";
    /// artifact_service could not keep or serve the bytes (no licence, auth refused, down, slow, unreadable): fail
    /// closed (raised by the store).
    pub const STORAGE_UNAVAILABLE: &str = "storage_unavailable";
}

/// An upload that was refused, with its [`reason`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal(pub &'static str);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "attachment refused: {}", self.0)
    }
}

impl std::error::Error for Refusal {}

/// An upload that passed: the bytes to store (re-encoded for an image, identical for a PDF) and what to show.
#[derive(Clone, Debug)]
pub struct Accepted {
    /// `image/jpeg`, `image/png`, `image/webp` or `application/pdf`.
    pub media_type: &'static str,
    pub bytes: Vec<u8>,
    /// Dimensions of the stored image; `None` for a PDF.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Sanitised, ≤ 80 characters, carries no path, no control character and no id ([`sanitize_display_name`]).
    pub display_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Jpeg,
    Png,
    Webp,
    Pdf,
}

impl Kind {
    fn media_type(self) -> &'static str {
        match self {
            Kind::Jpeg => "image/jpeg",
            Kind::Png => "image/png",
            Kind::Webp => "image/webp",
            Kind::Pdf => "application/pdf",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Kind::Jpeg => "jpg",
            Kind::Png => "png",
            Kind::Webp => "webp",
            Kind::Pdf => "pdf",
        }
    }

    fn default_name(self) -> &'static str {
        match self {
            Kind::Jpeg => "photo.jpg",
            Kind::Png => "image.png",
            Kind::Webp => "image.webp",
            Kind::Pdf => "document.pdf",
        }
    }
}

/// Validate and neutralise `bytes`; `declared_name` is only ever a hint for the display name.
pub fn accept(bytes: &[u8], declared_name: &str) -> Result<Accepted, Refusal> {
    let out = accept_inner(bytes, declared_name);
    match &out {
        Ok(a) => {
            metrics::counter!("agent_attachments_accepted_total", "media_type" => a.media_type)
                .increment(1)
        }
        Err(r) => {
            metrics::counter!("agent_attachments_refused_total", "reason" => r.0).increment(1)
        }
    }
    out
}

fn accept_inner(bytes: &[u8], declared_name: &str) -> Result<Accepted, Refusal> {
    if bytes.len() > MAX_BYTES {
        return Err(Refusal(reason::TOO_LARGE));
    }
    let kind = sniff(bytes).ok_or(Refusal(reason::UNSUPPORTED_TYPE))?;
    let display_name = sanitize_display_name(declared_name, kind);
    if kind == Kind::Pdf {
        check_pdf(bytes)?;
        return Ok(Accepted {
            media_type: kind.media_type(),
            bytes: bytes.to_vec(),
            width: None,
            height: None,
            display_name,
        });
    }
    let (out, width, height) = reencode_image(kind, bytes)?;
    if out.len() > MAX_BYTES {
        return Err(Refusal(reason::TOO_LARGE));
    }
    Ok(Accepted {
        media_type: kind.media_type(),
        bytes: out,
        width: Some(width),
        height: Some(height),
        display_name,
    })
}

fn sniff(b: &[u8]) -> Option<Kind> {
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(Kind::Jpeg)
    } else if b.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some(Kind::Png)
    } else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some(Kind::Webp)
    } else if b.starts_with(b"%PDF-") {
        Some(Kind::Pdf)
    } else {
        None
    }
}

// ---- images --------------------------------------------------------------------------------------------------------

fn be16(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_be_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
    ])))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
        *b.get(at + 2)?,
        *b.get(at + 3)?,
    ]))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
        *b.get(at + 2)?,
        *b.get(at + 3)?,
    ]))
}

fn le24(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
        *b.get(at + 2)?,
        0,
    ]))
}

fn le16(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_le_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
    ])))
}

/// The size an image's header claims, read without a decoder (so a hostile size is refused before anything is
/// allocated). `None` = no readable header.
fn declared_dimensions(kind: Kind, b: &[u8]) -> Option<(u32, u32)> {
    match kind {
        Kind::Png => {
            if b.get(12..16)? != b"IHDR" {
                return None;
            }
            Some((be32(b, 16)?, be32(b, 20)?))
        }
        Kind::Jpeg => jpeg_dimensions(b),
        Kind::Webp => webp_dimensions(b),
        Kind::Pdf => None,
    }
}

/// The first SOFn marker of a JPEG (every baseline / extended / progressive / lossless frame header).
fn jpeg_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            return None;
        }
        let m = b[i + 1];
        match m {
            0xFF => i += 1,
            0x01 | 0xD0..=0xD7 => i += 2,
            0x00 | 0xD8 | 0xD9 | 0xDA => return None,
            0xC0..=0xCF if !matches!(m, 0xC4 | 0xC8 | 0xCC) => {
                return Some((be16(b, i + 7)?, be16(b, i + 5)?));
            }
            _ => {
                let len = be16(b, i + 2)? as usize;
                if len < 2 {
                    return None;
                }
                i += 2 + len;
            }
        }
    }
    None
}

/// The first chunk of a WebP (`VP8X` canvas, `VP8L` lossless, `VP8 ` lossy).
fn webp_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    match b.get(12..16)? {
        b"VP8X" => Some((1 + le24(b, 24)?, 1 + le24(b, 27)?)),
        b"VP8L" => {
            if *b.get(20)? != 0x2F {
                return None;
            }
            let bits = le32(b, 21)?;
            Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
        }
        b"VP8 " => {
            if b.get(23..26)? != [0x9D, 0x01, 0x2A] {
                return None;
            }
            Some((le16(b, 26)? & 0x3FFF, le16(b, 28)? & 0x3FFF))
        }
        _ => None,
    }
}

/// APNG: an `acTL` chunk ahead of the first `IDAT`.
fn png_is_animated(b: &[u8]) -> bool {
    let mut pos = 8usize;
    while let (Some(len), Some(ty)) = (be32(b, pos), b.get(pos + 4..pos + 8)) {
        match ty {
            b"acTL" => return true,
            b"IDAT" | b"IEND" => return false,
            _ => {}
        }
        pos = match pos
            .checked_add(12)
            .and_then(|p| p.checked_add(len as usize))
        {
            Some(p) => p,
            None => return false,
        };
    }
    false
}

/// Animated WebP: the `VP8X` animation flag, or an `ANIM` / `ANMF` chunk anywhere.
fn webp_is_animated(b: &[u8]) -> bool {
    let mut pos = 12usize;
    while let (Some(ty), Some(size)) = (b.get(pos..pos + 4), le32(b, pos + 4)) {
        match ty {
            b"ANIM" | b"ANMF" => return true,
            b"VP8X" if b.get(pos + 8).is_some_and(|f| f & 0x02 != 0) => return true,
            _ => {}
        }
        let padded = (size as usize).saturating_add(size as usize & 1);
        pos = match pos.checked_add(8).and_then(|p| p.checked_add(padded)) {
            Some(p) => p,
            None => return false,
        };
    }
    false
}

fn image_error(e: &ImageError) -> Refusal {
    match e {
        ImageError::Limits(_) => Refusal(reason::TOO_MANY_PIXELS),
        _ => Refusal(reason::MALFORMED_IMAGE),
    }
}

/// Decode `bytes` under limits and write a metadata-free copy. Returns the new bytes and their dimensions.
fn reencode_image(kind: Kind, bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), Refusal> {
    let (dw, dh) = declared_dimensions(kind, bytes).ok_or(Refusal(reason::MALFORMED_IMAGE))?;
    if dw == 0 || dh == 0 {
        return Err(Refusal(reason::MALFORMED_IMAGE));
    }
    if dw > MAX_SIDE || dh > MAX_SIDE || u64::from(dw) * u64::from(dh) > MAX_PIXELS {
        return Err(Refusal(reason::TOO_MANY_PIXELS));
    }
    let animated = match kind {
        Kind::Png => png_is_animated(bytes),
        Kind::Webp => webp_is_animated(bytes),
        _ => false,
    };
    if animated {
        return Err(Refusal(reason::ANIMATED));
    }
    let format = match kind {
        Kind::Jpeg => ImageFormat::Jpeg,
        Kind::Png => ImageFormat::Png,
        _ => ImageFormat::WebP,
    };
    // A decoder panic on hostile input is a refusal, not a crashed worker.
    catch_unwind(AssertUnwindSafe(|| {
        decode_and_encode(kind, format, bytes, dw, dh)
    }))
    .unwrap_or(Err(Refusal(reason::MALFORMED_IMAGE)))
}

#[allow(clippy::field_reassign_with_default)] // `Limits` is non_exhaustive: no struct-update syntax
fn decode_and_encode(
    kind: Kind,
    format: ImageFormat,
    bytes: &[u8],
    declared_w: u32,
    declared_h: u32,
) -> Result<(Vec<u8>, u32, u32), Refusal> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(DECODE_MAX_ALLOC);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|e| image_error(&e))?;
    // The tag is about to be stripped: apply it to the pixels so the photo keeps its upright orientation.
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| image_error(&e))?;
    img.apply_orientation(orientation);
    let (w, h) = (img.width(), img.height());
    // A header that lied about its size is not an image we understand.
    let same = (w, h) == (declared_w, declared_h) || (h, w) == (declared_w, declared_h);
    if w == 0 || h == 0 || !same {
        return Err(Refusal(reason::MALFORMED_IMAGE));
    }
    let mut out = Vec::new();
    let encoded = match kind {
        Kind::Jpeg => DynamicImage::ImageRgb8(img.into_rgb8())
            .write_with_encoder(JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)),
        Kind::Png => normalised(img).write_with_encoder(PngEncoder::new_with_quality(
            &mut out,
            CompressionType::Default,
            FilterType::Adaptive,
        )),
        _ => normalised(img).write_with_encoder(WebPEncoder::new_lossless(&mut out)),
    };
    encoded.map_err(|e| image_error(&e))?;
    Ok((out, w, h))
}

/// 8-bit RGB, or RGBA when the source has an alpha channel: what the PNG and lossless WebP encoders take.
fn normalised(img: DynamicImage) -> DynamicImage {
    if img.color().has_alpha() {
        DynamicImage::ImageRgba8(img.into_rgba8())
    } else {
        DynamicImage::ImageRgb8(img.into_rgb8())
    }
}

// ---- PDF -----------------------------------------------------------------------------------------------------------

/// Names that make a PDF active, can launch or fetch something, or can smuggle a file in. Compared exactly (PDF names
/// are case-sensitive), after `#xx` decoding.
const ACTIVE_NAMES: &[&[u8]] = &[
    b"JavaScript",
    b"JS",
    b"Launch",
    b"OpenAction",
    b"EmbeddedFile",
    b"EmbeddedFiles",
    b"RichMedia",
    b"XFA",
    b"AA",
    b"GoToR",
    b"GoToE",
    b"SubmitForm",
    b"ImportData",
    b"EF",
    b"FileAttachment",
];

fn name_refusal(name: &[u8]) -> Result<(), &'static str> {
    if name == b"Encrypt" {
        Err(reason::ENCRYPTED_PDF)
    } else if ACTIVE_NAMES.contains(&name) {
        Err(reason::ACTIVE_PDF)
    } else {
        Ok(())
    }
}

fn is_pdf_delimiter(c: u8) -> bool {
    c.is_ascii_whitespace()
        || c == 0
        || matches!(
            c,
            b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
        )
}

fn hex_val(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// Every `/Name` token in the raw bytes, `#xx` escapes decoded (`/Java#53cript` is `/JavaScript`).
fn scan_names(b: &[u8]) -> Result<(), &'static str> {
    let mut i = 0;
    let mut name: Vec<u8> = Vec::with_capacity(PDF_MAX_NAME);
    while i < b.len() {
        if b[i] != b'/' {
            i += 1;
            continue;
        }
        i += 1;
        name.clear();
        while i < b.len() && !is_pdf_delimiter(b[i]) {
            let c = b[i];
            let decoded = if c == b'#' {
                match (
                    b.get(i + 1).copied().and_then(hex_val),
                    b.get(i + 2).copied().and_then(hex_val),
                ) {
                    (Some(h), Some(l)) => {
                        i += 2;
                        (h << 4) | l
                    }
                    _ => c,
                }
            } else {
                c
            };
            if name.len() < PDF_MAX_NAME {
                name.push(decoded);
            }
            i += 1;
        }
        name_refusal(&name)?;
    }
    Ok(())
}

fn walk_dict(d: &lopdf::Dictionary, depth: usize) -> Result<(), &'static str> {
    for (key, value) in d.iter() {
        name_refusal(key)?;
        walk(value, depth + 1)?;
    }
    Ok(())
}

fn walk(o: &lopdf::Object, depth: usize) -> Result<(), &'static str> {
    if depth > PDF_MAX_DEPTH {
        return Err(reason::MALFORMED_PDF);
    }
    match o {
        lopdf::Object::Name(n) => name_refusal(n),
        lopdf::Object::Array(items) => items.iter().try_for_each(|x| walk(x, depth + 1)),
        lopdf::Object::Dictionary(d) => walk_dict(d, depth),
        lopdf::Object::Stream(s) => walk_dict(&s.dict, depth),
        _ => Ok(()),
    }
}

/// Parse and walk the document. Runs on the worker thread.
fn analyse_pdf(bytes: &[u8]) -> Result<(), &'static str> {
    let options = lopdf::LoadOptions {
        max_decompressed_size: Some(PDF_MAX_DECOMPRESSED),
        ..Default::default()
    };
    let doc = lopdf::Document::load_mem_with_options(bytes, options)
        .map_err(|_| reason::MALFORMED_PDF)?;
    if doc.is_encrypted() || doc.trailer.has(b"Encrypt") {
        return Err(reason::ENCRYPTED_PDF);
    }
    if doc.objects.is_empty() || doc.objects.len() > PDF_MAX_OBJECTS {
        return Err(reason::MALFORMED_PDF);
    }
    // A document is a catalog that reaches at least one page; a header followed by garbage is neither.
    if doc.catalog().is_err() || doc.get_pages().is_empty() {
        return Err(reason::MALFORMED_PDF);
    }
    walk_dict(&doc.trailer, 0)?;
    for object in doc.objects.values() {
        walk(object, 0)?;
    }
    Ok(())
}

fn check_pdf(bytes: &[u8]) -> Result<(), Refusal> {
    scan_names(bytes).map_err(Refusal)?;
    let (tx, rx) = mpsc::sync_channel::<Result<(), &'static str>>(1);
    let owned = bytes.to_vec();
    let spawned = std::thread::Builder::new()
        .name("pdf-check".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let verdict = catch_unwind(AssertUnwindSafe(|| analyse_pdf(&owned)))
                .unwrap_or(Err(reason::MALFORMED_PDF));
            // The receiver is gone after a timeout; the verdict is then moot.
            let _ = tx.send(verdict);
        });
    if spawned.is_err() {
        return Err(Refusal(reason::MALFORMED_PDF));
    }
    match rx.recv_timeout(PDF_TIME_BUDGET) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(r)) => Err(Refusal(r)),
        // timed out, or the worker died without a verdict: never allow
        Err(_) => Err(Refusal(reason::MALFORMED_PDF)),
    }
}

// ---- display name --------------------------------------------------------------------------------------------------

/// Characters that are invisible or reorder text (zero-width, bidi overrides and isolates, BOM): dropped.
fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// `RET-12`, `shp 99`, `cust123`, `C-77`, `PAY1234`, `cs42`: one of the demo estate's id prefixes, case-insensitive,
/// at a word start, an optional `-` or `_`, then two or more digits.
fn has_id_prefix(s: &str) -> bool {
    const PREFIXES: [&str; 6] = ["ret", "shp", "pay", "cs", "cust", "c"];
    let chars: Vec<char> = s.chars().map(|c| c.to_ascii_lowercase()).collect();
    for start in 0..chars.len() {
        if start > 0 && chars[start - 1].is_alphabetic() {
            continue;
        }
        for p in PREFIXES {
            let p: Vec<char> = p.chars().collect();
            if chars.len() < start + p.len() || chars[start..start + p.len()] != p[..] {
                continue;
            }
            let mut k = start + p.len();
            if matches!(chars.get(k), Some('-' | '_')) {
                k += 1;
            }
            let digits = chars[k..].iter().take_while(|c| c.is_ascii_digit()).count();
            if digits >= 2 {
                return true;
            }
        }
    }
    false
}

/// The name a customer or staff member sees (design §6): no path, no control or invisible characters, the extension
/// forced to the real type, ≤ 80 characters, and NEVER an id — a name that looks like a record id, a UUID or a long
/// digit run (a camera's `IMG_20261027_093011` included) is replaced by the type's default (`photo.jpg`, `image.png`,
/// `image.webp`, `document.pdf`).
pub fn sanitize_display_name_for(declared: &str, media_type: &str) -> String {
    let kind = match media_type {
        "image/jpeg" => Kind::Jpeg,
        "image/png" => Kind::Png,
        "image/webp" => Kind::Webp,
        _ => Kind::Pdf,
    };
    sanitize_display_name(declared, kind)
}

fn sanitize_display_name(declared: &str, kind: Kind) -> String {
    let base = declared.rsplit(['/', '\\']).next().unwrap_or("");
    let mut cleaned = String::with_capacity(base.len());
    let mut last_space = false;
    for c in base.chars() {
        if c.is_control() || is_invisible(c) {
            continue;
        }
        let c = if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
            '_'
        } else {
            c
        };
        if c.is_whitespace() {
            if !last_space {
                cleaned.push(' ');
            }
            last_space = true;
        } else {
            cleaned.push(c);
            last_space = false;
        }
    }
    let cleaned = cleaned.trim_matches(|c: char| c.is_whitespace() || c == '.');
    // the supplied extension is never trusted: the stored type decides it
    let stem = match cleaned.rfind('.') {
        Some(dot) => &cleaned[..dot],
        None => cleaned,
    };
    let stem = stem.trim_matches(|c: char| c.is_whitespace() || c == '.');
    let ext = kind.extension();
    let room = MAX_NAME_CHARS - ext.len() - 1;
    let stem: String = stem.chars().take(room).collect();
    let stem = stem.trim_end_matches(|c: char| c.is_whitespace() || c == '.');
    if stem.is_empty() || contains_id_pattern(stem) || has_id_prefix(stem) {
        return kind.default_name().to_owned();
    }
    format!("{stem}.{ext}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::RgbImage;

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    fn png_chunk(ty: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut body = ty.to_vec();
        body.extend_from_slice(data);
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
        out
    }

    fn sample() -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(64, 48, |x, y| {
            image::Rgb([(x * 4) as u8, (y * 5) as u8, 128])
        }))
    }

    fn encode(img: &DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, format).expect("encode sample");
        out.into_inner()
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    fn refused(bytes: &[u8]) -> &'static str {
        accept(bytes, "x").expect_err("must be refused").0
    }

    // ---- images

    #[test]
    fn jpeg_round_trip_strips_exif() {
        let plain = encode(&sample(), ImageFormat::Jpeg);
        // an APP1 EXIF segment right after SOI, carrying a GPS-looking payload
        let payload = b"Exif\0\0SECRET-GPS-52.3676N-4.9041E";
        let mut with_exif = vec![0xFF, 0xD8, 0xFF, 0xE1];
        with_exif.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        with_exif.extend_from_slice(payload);
        with_exif.extend_from_slice(&plain[2..]);
        assert!(contains(&with_exif, b"SECRET-GPS"));

        let a = accept(&with_exif, "../../IMG.JPG").expect("a valid jpeg is accepted");
        assert_eq!(a.media_type, "image/jpeg");
        assert_eq!((a.width, a.height), (Some(64), Some(48)));
        assert_eq!(a.display_name, "IMG.jpg");
        assert!(a.bytes.starts_with(&[0xFF, 0xD8, 0xFF]));
        assert!(!contains(&a.bytes, b"SECRET-GPS"));
        assert!(!contains(&a.bytes, b"Exif"));
        let back = image::load_from_memory(&a.bytes).expect("the output decodes");
        assert_eq!((back.width(), back.height()), (64, 48));
    }

    #[test]
    fn png_round_trip_strips_text_chunks() {
        let plain = encode(&sample(), ImageFormat::Png);
        // after the signature (8) and IHDR (25): a tEXt chunk with a secret
        let mut with_text = plain[..33].to_vec();
        with_text.extend(png_chunk(b"tEXt", b"Comment\0SECRET-LOCATION-HOME"));
        with_text.extend_from_slice(&plain[33..]);
        assert!(contains(&with_text, b"SECRET-LOCATION"));

        let a = accept(&with_text, "shot.png").expect("a valid png is accepted");
        assert_eq!(a.media_type, "image/png");
        assert_eq!((a.width, a.height), (Some(64), Some(48)));
        assert!(!contains(&a.bytes, b"SECRET-LOCATION"));
        assert!(!contains(&a.bytes, b"tEXt"));
        assert_eq!(
            image::load_from_memory(&a.bytes).expect("decodes").width(),
            64
        );
    }

    #[test]
    fn webp_round_trip() {
        let src = encode(&sample(), ImageFormat::WebP);
        let a = accept(&src, "pic.webp").expect("a valid webp is accepted");
        assert_eq!(a.media_type, "image/webp");
        assert_eq!((a.width, a.height), (Some(64), Some(48)));
        assert_eq!(
            image::load_from_memory(&a.bytes).expect("decodes").height(),
            48
        );
    }

    #[test]
    fn png_header_claiming_a_huge_image_is_refused_before_any_decode() {
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&100_000u32.to_be_bytes());
        ihdr.extend_from_slice(&100_000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend(png_chunk(b"IHDR", &ihdr));
        // no IDAT at all: a decoder would have to allocate ~30 GB to even start
        assert_eq!(refused(&png), reason::TOO_MANY_PIXELS);
    }

    #[test]
    fn pixel_and_side_limits() {
        // 4097 wide: over the side limit although the pixel count is small
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&4097u32.to_be_bytes());
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend(png_chunk(b"IHDR", &ihdr));
        assert_eq!(refused(&png), reason::TOO_MANY_PIXELS);

        // a JPEG frame header claiming 5000 x 5000
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
        jpeg.extend_from_slice(&5000u16.to_be_bytes());
        jpeg.extend_from_slice(&5000u16.to_be_bytes());
        jpeg.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        assert_eq!(refused(&jpeg), reason::TOO_MANY_PIXELS);
    }

    #[test]
    fn apng_is_refused_as_animated() {
        let plain = encode(&sample(), ImageFormat::Png);
        let mut apng = plain[..33].to_vec();
        apng.extend(png_chunk(b"acTL", &[0, 0, 0, 2, 0, 0, 0, 0]));
        apng.extend_from_slice(&plain[33..]);
        assert_eq!(refused(&apng), reason::ANIMATED);
    }

    #[test]
    fn animated_webp_is_refused() {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&[0; 4]);
        v.extend_from_slice(b"WEBP");
        v.extend_from_slice(b"VP8X");
        v.extend_from_slice(&10u32.to_le_bytes());
        v.extend_from_slice(&[0x02, 0, 0, 0, 9, 0, 0, 9, 0, 0]);
        v.extend_from_slice(b"ANIM");
        v.extend_from_slice(&6u32.to_le_bytes());
        v.extend_from_slice(&[0; 6]);
        assert_eq!(refused(&v), reason::ANIMATED);
    }

    #[test]
    fn unknown_empty_and_oversize_bytes() {
        assert_eq!(
            refused(b"MZ\x90\x00 an executable"),
            reason::UNSUPPORTED_TYPE
        );
        assert_eq!(refused(b""), reason::UNSUPPORTED_TYPE);
        assert_eq!(refused(b"GIF89a....."), reason::UNSUPPORTED_TYPE);
        // the declared type is irrelevant: a JPEG named .pdf is still a JPEG, a script named .jpg is refused
        assert_eq!(
            refused(b"<script>alert(1)</script>"),
            reason::UNSUPPORTED_TYPE
        );
        let mut big = vec![0u8; 11 * 1024 * 1024];
        big[..3].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
        assert_eq!(refused(&big), reason::TOO_LARGE);
    }

    #[test]
    fn truncated_image_is_malformed() {
        let mut png = encode(&sample(), ImageFormat::Png);
        png.truncate(60);
        assert_eq!(refused(&png), reason::MALFORMED_IMAGE);
        assert_eq!(refused(&[0xFF, 0xD8, 0xFF, 0xE0]), reason::MALFORMED_IMAGE);
    }

    // ---- PDF

    /// A one-page PDF with a correct xref table; `root_extra` goes into the catalog, `extra` become objects 4, 5, ...
    fn pdf(root_extra: &str, extra: &[&str]) -> Vec<u8> {
        let mut objs = vec![
            format!("<< /Type /Catalog /Pages 2 0 R {root_extra} >>"),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>".to_owned(),
        ];
        objs.extend(extra.iter().map(|s| (*s).to_owned()));
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes(),
        );
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    #[test]
    fn minimal_pdf_is_accepted_byte_identical() {
        let src = pdf("", &[]);
        let a = accept(&src, "C:\\docs\\receipt.PDF").expect("a clean pdf is accepted");
        assert_eq!(a.media_type, "application/pdf");
        assert_eq!(a.bytes, src);
        assert_eq!((a.width, a.height), (None, None));
        assert_eq!(a.display_name, "receipt.pdf");
    }

    #[test]
    fn active_pdfs_are_refused() {
        let cases: [(&str, Vec<u8>); 8] = [
            (
                "javascript action",
                pdf(
                    "/OpenAction 4 0 R",
                    &["<< /S /JavaScript /JS (app.alert(1)) >>"],
                ),
            ),
            ("open action alone", pdf("/OpenAction [3 0 R /Fit]", &[])),
            (
                "embedded file",
                pdf(
                    "",
                    &["<< /Type /EmbeddedFile /Length 3 >>\nstream\nabc\nendstream"],
                ),
            ),
            (
                "xfa",
                pdf("/AcroForm << /XFA [ (template) 4 0 R ] >>", &["<< >>"]),
            ),
            ("launch", pdf("", &["<< /S /Launch /F (calc.exe) >>"])),
            (
                "additional actions",
                pdf("/AA << /O 4 0 R >>", &["<< /S /GoTo >>"]),
            ),
            ("rich media", pdf("", &["<< /Subtype /RichMedia >>"])),
            // `/Java#53cript` is `/JavaScript` once the escape is decoded
            (
                "hex-escaped name",
                pdf("", &["<< /S /Java#53cript /JS (x) >>"]),
            ),
        ];
        for (what, bytes) in cases {
            assert_eq!(
                accept(&bytes, "x.pdf").expect_err(what).0,
                reason::ACTIVE_PDF,
                "{what}"
            );
        }
    }

    #[test]
    fn near_miss_names_are_not_active() {
        // /JSON and /Launcher are other names; only the exact tokens are keys
        let src = pdf("/Lang (en)", &["<< /JSON 1 /Launcher 2 >>"]);
        assert!(accept(&src, "x.pdf").is_ok());
    }

    #[test]
    fn broken_pdfs_are_malformed() {
        assert_eq!(
            refused(b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog"),
            reason::MALFORMED_PDF
        );
        assert_eq!(refused(b"%PDF-"), reason::MALFORMED_PDF);
        let mut garbage = b"%PDF-1.7\n".to_vec();
        garbage.extend((0..4096u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8 | 0x80));
        assert_eq!(refused(&garbage), reason::MALFORMED_PDF);
        // a catalog that reaches no page
        let empty = String::from_utf8(pdf("", &[]))
            .expect("ascii")
            .replace("[3 0 R]", "[     ]")
            .replace("/Count 1", "/Count 0");
        let no_pages = empty.into_bytes();
        assert_eq!(refused(&no_pages), reason::MALFORMED_PDF);
    }

    // ---- display name

    #[test]
    fn display_name_basics() {
        let n = |s: &str, k| sanitize_display_name(s, k);
        assert_eq!(n("../../etc/passwd.jpg", Kind::Jpeg), "passwd.jpg");
        assert_eq!(n("C:\\Users\\bob\\scan.png", Kind::Png), "scan.png");
        assert_eq!(
            n("holiday photo 2026.jpg", Kind::Jpeg),
            "holiday photo 2026.jpg"
        );
        // the stored type decides the extension
        assert_eq!(n("evil.exe", Kind::Jpeg), "evil.jpg");
        assert_eq!(n("noext", Kind::Pdf), "noext.pdf");
        // empty, dots and control characters fall back to the default
        assert_eq!(n("", Kind::Jpeg), "photo.jpg");
        assert_eq!(n("...", Kind::Png), "image.png");
        assert_eq!(n("\u{0}\u{7}\n", Kind::Pdf), "document.pdf");
        assert_eq!(n("   ", Kind::Webp), "image.webp");
    }

    #[test]
    fn display_name_strips_control_and_bidi_characters() {
        let name = sanitize_display_name("inv\u{202E}oice\u{200B}\t\n.pdf\u{0}", Kind::Pdf);
        assert_eq!(name, "invoice.pdf");
        let name = sanitize_display_name("a<b>:c|d?.jpg", Kind::Jpeg);
        assert_eq!(name, "a_b__c_d_.jpg");
    }

    #[test]
    fn display_name_is_capped() {
        let long = format!("{}.jpg", "x".repeat(300));
        let name = sanitize_display_name(&long, Kind::Jpeg);
        assert_eq!(name.chars().count(), MAX_NAME_CHARS);
        assert!(name.ends_with(".jpg"));
        let wide = format!("{}.pdf", "é".repeat(200));
        assert_eq!(
            sanitize_display_name(&wide, Kind::Pdf).chars().count(),
            MAX_NAME_CHARS
        );
    }

    #[test]
    fn display_name_never_carries_an_id() {
        for bad in [
            "RET-0042.jpg",
            "ret-42.jpg",
            "SHP-9102 damaged.png",
            "PAY-77.pdf",
            "CS-3.pdf",
            "cust123.jpg",
            "cust-123.jpg",
            "C-77.png",
            "order ORD-LATE-01.pdf",
            "AB12345.jpg",
            "3f2b8c1e-9d4a-4e6b-8a1c-0b7d5e9f2a34.jpg",
            "IMG_20261027_093011.jpg",
            "1234567.pdf",
        ] {
            let kind = if bad.ends_with(".pdf") {
                Kind::Pdf
            } else if bad.ends_with(".png") {
                Kind::Png
            } else {
                Kind::Jpeg
            };
            assert_eq!(
                sanitize_display_name(bad, kind),
                kind.default_name(),
                "{bad} must fall back to the default name"
            );
        }
        // a path is dropped before the name is judged
        assert_eq!(
            sanitize_display_name("RET-12/kitchen.jpg", Kind::Jpeg),
            "kitchen.jpg"
        );
        // an id late in a long stem is still found
        assert_eq!(
            sanitize_display_name(&format!("{} RET-12.jpg", "a".repeat(40)), Kind::Jpeg),
            "photo.jpg"
        );
    }

    #[test]
    fn id_prefix_needs_a_word_start() {
        assert!(has_id_prefix("RET-12"));
        assert!(has_id_prefix("my cust99"));
        assert!(!has_id_prefix("document1"));
        assert!(!has_id_prefix("receipt-1"));
        assert!(!has_id_prefix("c"));
    }
}

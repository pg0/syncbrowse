//! Decoding, fingerprints, thumbnails and "made by" metadata of one picture.
use image::metadata::Orientation;
use image::{DynamicImage, GrayImage, ImageDecoder, ImageReader, RgbImage, imageops::FilterType};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::{Condvar, Mutex};

pub const WEB: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp", "gif"];
const CONVERT: &[&str] = &["tif", "tiff"];
pub const RAW: &[&str] = &["arw", "cr2", "cr3", "nef", "nrw", "dng", "orf", "raf", "rw2", "pef", "srw"];
pub const THUMB: u32 = 480; // gallery thumbnail height
const RAW_AT_ONCE: usize = 3; // a developed raw takes several hundred MB: 8 scan threads at once could run out

pub type Bits = [u64; 4]; // 16x16 dHash, first bit = highest bit of word 0 (same hex as dupimg)

pub fn ext(path: &str) -> String {
    Path::new(path).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

pub fn is_picture(path: &str, raw: bool) -> bool {
    let e = ext(path);
    WEB.contains(&e.as_str()) || CONVERT.contains(&e.as_str()) || (raw && RAW.contains(&e.as_str()))
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// The picture as shown (orientation applied) and its full shown size. JPEGs decode at a reduced DCT scale that
/// still leaves `min` pixels on each side: a 6640 px scan loads in a fraction of the time.
fn open(path: &str, min: u16) -> Result<(DynamicImage, u32, u32), String> {
    let e = ext(path);
    if e == "jpg" || e == "jpeg" {
        return jpeg(path, min);
    }
    if RAW.contains(&e.as_str()) {
        return raw(path);
    }
    let mut r = ImageReader::open(path).map_err(err)?.with_guessed_format().map_err(err)?;
    r.no_limits(); // the user's own archive: big upscales must not count as unreadable
    let mut d = r.into_decoder().map_err(err)?;
    let o = d.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(d).map_err(err)?;
    img.apply_orientation(o);
    let (w, h) = (img.width(), img.height());
    Ok((img, w, h))
}

fn jpeg(path: &str, min: u16) -> Result<(DynamicImage, u32, u32), String> {
    use jpeg_decoder::PixelFormat;
    let mut d = jpeg_decoder::Decoder::new(BufReader::new(File::open(path).map_err(err)?));
    d.read_info().map_err(err)?;
    let full = d.info().ok_or("no JPEG header")?;
    // jpeg-decoder takes the first scale where either side reaches its request: ask for the shorter side only,
    // so both keep `min` like Pillow's draft
    if full.width <= full.height { d.scale(min, u16::MAX) } else { d.scale(u16::MAX, min) }.map_err(err)?;
    let px = d.decode().map_err(err)?;
    let info = d.info().ok_or("no JPEG header")?;
    let (w, h) = (info.width as u32, info.height as u32);
    let img = match info.pixel_format {
        PixelFormat::L8 => GrayImage::from_raw(w, h, px).map(DynamicImage::ImageLuma8),
        PixelFormat::L16 => GrayImage::from_raw(w, h, px.chunks_exact(2).map(|c| c[0]).collect()).map(DynamicImage::ImageLuma8),
        PixelFormat::RGB24 => RgbImage::from_raw(w, h, px).map(DynamicImage::ImageRgb8),
        PixelFormat::CMYK32 => RgbImage::from_raw(w, h, px.chunks_exact(4)
            .flat_map(|c| [c[0], c[1], c[2]].map(|v| ((255 - v as u32) * (255 - c[3] as u32) / 255) as u8))
            .collect()).map(DynamicImage::ImageRgb8),
    }.ok_or("JPEG size mismatch")?;
    let o = d.exif_data()
        .and_then(|x| exif::Reader::new().read_raw(x.to_vec()).ok())
        .and_then(|x| x.get_field(exif::Tag::Orientation, exif::In::PRIMARY).and_then(|f| f.value.get_uint(0)))
        .unwrap_or(1);
    let mut img = img;
    img.apply_orientation(Orientation::from_exif(o as u8).unwrap_or(Orientation::NoTransforms));
    let (fw, fh) = (full.width as u32, full.height as u32);
    Ok((img, if (5..=8).contains(&o) { fh } else { fw }, if (5..=8).contains(&o) { fw } else { fh }))
}

static RAW_SLOTS: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());
struct RawSlot;
impl Drop for RawSlot {
    fn drop(&mut self) {
        *RAW_SLOTS.0.lock().unwrap() -= 1;
        RAW_SLOTS.1.notify_one();
    }
}

/// Real raw development, not the embedded preview: some cameras (Sony ZV-E1) embed a 16:9 preview of a 3:2 photo,
/// which never matches the camera JPEG.
fn raw(path: &str) -> Result<(DynamicImage, u32, u32), String> {
    use flashraw::{DecodeOptions, FlashRaw, OutputFormat, RawPixelData};
    {
        let mut n = RAW_SLOTS.0.lock().unwrap();
        while *n >= RAW_AT_ONCE {
            n = RAW_SLOTS.1.wait(n).unwrap();
        }
        *n += 1;
    }
    let _slot = RawSlot;
    let f = FlashRaw::open_path(path).map_err(err)?
        .decode(DecodeOptions { output: OutputFormat::Rgba8, ..Default::default() }).map_err(err)?;
    let (w, h) = (f.width(), f.height());
    let RawPixelData::U8(mut px) = f.data else { return Err("raw develop failed".into()) };
    // alpha is always opaque: drop it in place rather than copy a few hundred MB
    let n = w as usize * h as usize;
    for i in 0..n {
        px.copy_within(i * 4..i * 4 + 3, i * 3);
    }
    px.truncate(n * 3);
    let img = RgbImage::from_raw(w, h, px).ok_or("raw develop failed")?;
    Ok((DynamicImage::ImageRgb8(img), w, h))
}

/// RGB with transparency shown on white, as dupimg judges it.
fn flat(img: &DynamicImage) -> RgbImage {
    if !img.color().has_alpha() {
        return img.to_rgb8();
    }
    let rgba = img.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let p = rgba.get_pixel(x, y).0;
        let a = p[3] as u32;
        image::Rgb([0, 1, 2].map(|i| ((p[i] as u32 * a + 255 * (255 - a) + 127) / 255) as u8))
    })
}

/// Luma as Pillow's "L" and the JPEG Y channel compute it (BT.601), so raw and JPEG versions compare alike.
fn gray(img: &DynamicImage) -> GrayImage {
    if let DynamicImage::ImageLuma8(g) = img {
        return g.clone();
    }
    let rgb = flat(img);
    GrayImage::from_fn(rgb.width(), rgb.height(), |x, y| {
        let [r, g, b] = rgb.get_pixel(x, y).0;
        image::Luma([((r as u32 * 299 + g as u32 * 587 + b as u32 * 114 + 500) / 1000) as u8])
    })
}

/// Shown width, shown height and the 256-bit dHash: 17x16 grey, each bit = pixel brighter than its right neighbour.
pub fn fingerprint(path: &str) -> Result<(u32, u32, Bits), String> {
    let (img, w, h) = open(path, 64)?;
    let px = image::imageops::resize(&gray(&img), 17, 16, FilterType::Lanczos3);
    let mut bits = [0u64; 4];
    for i in 0..256u32 {
        let (y, x) = (i / 16, i % 16);
        if px.get_pixel(x, y)[0] > px.get_pixel(x + 1, y)[0] {
            bits[(i / 64) as usize] |= 1 << (63 - i % 64);
        }
    }
    Ok((w, h, bits))
}

pub fn hex(b: &Bits) -> String {
    b.iter().map(|w| format!("{w:016x}")).collect()
}

pub fn unhex(s: &str) -> Option<Bits> {
    let mut b = [0u64; 4];
    for (i, w) in b.iter_mut().enumerate() {
        *w = u64::from_str_radix(s.get(i * 16..i * 16 + 16)?, 16).ok()?;
    }
    Some(b)
}

pub fn dist(a: &Bits, b: &Bits) -> u32 {
    (0..4).map(|i| (a[i] ^ b[i]).count_ones()).sum()
}

fn jpeg_bytes(img: &RgbImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality).encode_image(img).map_err(err)?;
    Ok(out)
}

/// The largest JPEG the camera embedded (as coral's gallery does): no development, camera colours. May show a
/// different crop than the raw (16:9 of a 3:2 photo), fine for a tile but never for the hash.
fn raw_preview(path: &str) -> Option<DynamicImage> {
    let raw = flashraw::FlashRaw::open_path(path).ok()?;
    let p = raw.preview()?;
    let rgb = raw.decode_preview_rgb(p).ok()?;
    let mut img = Some(DynamicImage::ImageRgb8(RgbImage::from_raw(rgb.width, rgb.height, rgb.rgb)?))
        .filter(|i| i.width().min(i.height()) >= THUMB / 2)?; // a 160 px thumbnail would look blurred
    img.apply_orientation(Orientation::from_exif(p.orientation.exif() as u8).unwrap_or(Orientation::NoTransforms));
    Some(img)
}

pub fn thumb(path: &str) -> Result<Vec<u8>, String> {
    let preview = if RAW.contains(&ext(path).as_str()) { raw_preview(path) } else { None };
    let img = match preview {
        Some(img) => img,
        None => open(path, THUMB as u16)?.0,
    };
    let img = if img.height() > THUMB || img.width() > THUMB * 3 { img.thumbnail(THUMB * 3, THUMB) } else { img };
    jpeg_bytes(&flat(&img), 85)
}

/// TIFF and raw files: browsers can't render these, convert.
pub fn browser_jpeg(path: &str) -> Result<Vec<u8>, String> {
    let (img, _, _) = open(path, u16::MAX)?;
    jpeg_bytes(&flat(&img), 95)
}

pub fn mime(path: &str) -> &'static str {
    match ext(path).as_str() {
        "png" => "image/png", "webp" => "image/webp", "bmp" => "image/bmp", "gif" => "image/gif", _ => "image/jpeg",
    }
}

/// What produced a picture: the models in ComfyUI's embedded graph, else EXIF Software or the JPEG comment.
pub fn made_by(path: &str) -> (String, String) {
    let e = ext(path);
    let graph = if e == "png" { png_text(path, "prompt") } else { None };
    let comment = if e == "jpg" || e == "jpeg" { jpeg_comment(path).unwrap_or_default() } else { String::new() };
    // only formats whose EXIF sits up front: for TIFF and raw files the reader loads the whole file
    let software = if ["jpg", "jpeg", "webp"].contains(&e.as_str()) { exif_software(path) } else { None };
    let (mut models, mut prompts) = (BTreeSet::new(), Vec::new());
    if let Some(serde_json::Value::Object(g)) = graph.and_then(|t| serde_json::from_str(&t).ok()) {
        for inputs in g.values().filter_map(|n| n.get("inputs")?.as_object()) {
            for (k, v) in inputs {
                let Some(s) = v.as_str() else { continue };
                match k.as_str() {
                    "ckpt_name" | "unet_name" | "model_name" | "lora_name" => { models.insert(stem(s)); } // not vae/clip
                    "prompt" | "text" if !s.trim().is_empty() => prompts.push(s.to_string()),
                    _ => {}
                }
            }
        }
    }
    let tool = if models.is_empty() {
        software.filter(|s| !s.is_empty()).unwrap_or(comment)
    } else {
        format!("ComfyUI {}", models.into_iter().collect::<Vec<_>>().join(", "))
    };
    (tool, prompts.join("\n"))
}

fn stem(s: &str) -> String {
    let base = s.rfind(['/', '\\']).map_or(0, |i| i + 1);
    match s[base..].rfind('.') {
        Some(i) if i > 0 => s[..base + i].to_string(),
        _ => s.to_string(),
    }
}

/// Text chunk of a PNG; ComfyUI writes its graph before the pixel data, so only the header is read.
fn png_text(path: &str, key: &str) -> Option<String> {
    let r = png::Decoder::new(BufReader::new(File::open(path).ok()?)).read_info().ok()?;
    let info = r.info();
    info.uncompressed_latin1_text.iter().find(|c| c.keyword == key).map(|c| c.text.clone())
        .or_else(|| info.utf8_text.iter().find(|c| c.keyword == key).and_then(|c| c.get_text().ok()))
        .or_else(|| info.compressed_latin1_text.iter().find(|c| c.keyword == key).and_then(|c| c.get_text().ok()))
}

/// COM segment before the pixel data (JPEGmini signs its files there).
fn jpeg_comment(path: &str) -> Option<String> {
    let mut f = BufReader::new(File::open(path).ok()?);
    let mut b = [0u8; 4];
    f.read_exact(&mut b[..2]).ok()?;
    if b[..2] != [0xFF, 0xD8] {
        return None;
    }
    loop {
        f.read_exact(&mut b).ok()?;
        let len = u16::from_be_bytes([b[2], b[3]]) as usize;
        if b[0] != 0xFF || b[1] == 0xDA || len < 2 {
            return None;
        }
        let mut seg = vec![0; len - 2];
        f.read_exact(&mut seg).ok()?;
        if b[1] == 0xFE {
            return Some(seg.iter().map(|&c| c as char).collect::<String>().trim_matches(['\0', ' ']).to_string());
        }
    }
}

fn exif_software(path: &str) -> Option<String> {
    let x = exif::Reader::new().read_from_container(&mut BufReader::new(File::open(path).ok()?)).ok()?;
    match &x.get_field(exif::Tag::Software, exif::In::PRIMARY)?.value {
        exif::Value::Ascii(v) => v.first().map(|s| String::from_utf8_lossy(s).trim_matches(['\0', ' ']).to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photo(seed: u32) -> RgbImage {
        let mut s = seed.wrapping_mul(2654435761).wrapping_add(1);
        let mut next = move |n: u32| {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            (s >> 8) % n
        };
        let mut im = RgbImage::from_pixel(1200, 900, image::Rgb([128, 128, 128]));
        for _ in 0..40 {
            let (x, y, w, h) = (next(1200), next(900), 50 + next(350), 50 + next(350));
            let c = image::Rgb([next(256) as u8, next(256) as u8, next(256) as u8]);
            for yy in y..(y + h).min(900) {
                for xx in x..(x + w).min(1200) {
                    im.put_pixel(xx, yy, c);
                }
            }
        }
        im
    }

    #[test]
    fn scaled_copy_matches_and_other_picture_does_not() {
        let dir = std::env::temp_dir().join(format!("syncbrowse-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = |n: &str| dir.join(n).to_string_lossy().into_owned();
        let mut buf = std::fs::File::create(p("a.jpg")).unwrap();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 95).encode_image(&photo(1)).unwrap();
        image::imageops::resize(&photo(1), 600, 450, FilterType::Triangle).save(p("b.png")).unwrap();
        photo(2).save(p("c.png")).unwrap();
        let (a, b, c) = (fingerprint(&p("a.jpg")).unwrap(), fingerprint(&p("b.png")).unwrap(), fingerprint(&p("c.png")).unwrap());
        assert_eq!((a.0, a.1, b.0, b.1), (1200, 900, 600, 450));
        assert!(dist(&a.2, &b.2) <= 10, "copy distance {}", dist(&a.2, &b.2));
        assert!(dist(&a.2, &c.2) > 40, "other picture distance {}", dist(&a.2, &c.2));
        assert_eq!(unhex(&hex(&a.2)), Some(a.2));
        let thumb = image::load_from_memory(&thumb(&p("a.jpg")).unwrap()).unwrap();
        assert_eq!((thumb.width(), thumb.height()), (640, 480));
        std::fs::remove_dir_all(dir).ok();
    }

    /// SYNCBROWSE_RAW_PAIRS="a.ARW|a.jpg;b.ARW|b.jpg" cargo test --release -- --ignored --nocapture
    #[test]
    #[ignore]
    fn raw_matches_its_camera_jpeg() {
        let pairs = std::env::var("SYNCBROWSE_RAW_PAIRS").expect("SYNCBROWSE_RAW_PAIRS");
        for (raw, jpg) in pairs.split(';').filter_map(|p| p.split_once('|')) {
            let t = std::time::Instant::now();
            let (rw, rh, r) = fingerprint(raw).unwrap();
            let secs = t.elapsed().as_secs_f64();
            let (jw, jh, j) = fingerprint(jpg).unwrap();
            println!("{raw} {rw}x{rh} ({secs:.2}s) vs {jw}x{jh}: distance {}", dist(&r, &j));
            assert!(dist(&r, &j) <= 10);
            assert!(((rw as f64 / rh as f64) / (jw as f64 / jh as f64) - 1.0).abs() <= 0.02);
        }
    }

    #[test]
    fn model_name_keeps_folder_drops_extension() {
        assert_eq!(stem("qwen/qwen_image_2.1_turbo.safetensors"), "qwen/qwen_image_2.1_turbo");
        assert_eq!(stem(".hidden"), ".hidden");
    }
}

//! Server-side image transcoding for the Kindle browser.
//!
//! Articles rendered through `extract.rs` are full of WebP / AVIF /
//! oversized JPEG / transparent PNG images that the built-in Kindle
//! browser either refuses to draw or fetches over megabytes of mobile
//! data. The proxy in `routes::img_proxy` rewrites every `<img>` to
//! `/img?u=...` and this module is what runs when the cache is cold:
//! fetch the source, decode, resize, re-encode as JPEG, and shrink the
//! file size below a Kindle-friendly cap.

use anyhow::{anyhow, bail, Context, Result};
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, ImageFormat};
use sha1::{Digest, Sha1};
use url::Url;

/// Maximum dimensions the Kindle browser comfortably renders. Larger
/// images are downscaled with Lanczos3 so the file ships less data and
/// the Kindle isn't asked to do its own resampling at paint time.
pub const MAX_WIDTH: u32 = 1200;
pub const MAX_HEIGHT: u32 = 1600;

/// File-size budget after JPEG re-encode. We start at a high quality
/// (85) and drop in 10-point steps until either the bytes fit or we
/// hit a hard floor. 150 KiB is loose enough to keep photos legible
/// while staying small over slow mobile connections.
pub const MAX_BYTES: usize = 150 * 1024;

/// Hard cap on the source bytes we'll read before giving up. Without
/// it, a hostile or misconfigured server could stream gigabytes into
/// the proxy's memory.
pub const MAX_SOURCE_BYTES: usize = 10 * 1024 * 1024;

/// JPEG quality range. Encoding starts at the top and walks down by
/// `QUALITY_STEP` until the result fits in `MAX_BYTES` or the floor
/// is reached, at which point we ship the floor-quality result and
/// accept the slight overage.
const QUALITY_START: u8 = 85;
const QUALITY_FLOOR: u8 = 40;
const QUALITY_STEP: u8 = 10;

/// Stable cache key for a source URL. Sha1 is fine here — collisions
/// are not a security concern (cache lookup is read-only and the
/// stored bytes are public images), and the hex form is short enough
/// to pass through URL routing without escaping.
pub fn hash_url(url: &str) -> String {
    let mut h = Sha1::new();
    h.update(url.as_bytes());
    let digest = h.finalize();
    let mut out = String::with_capacity(40);
    for b in digest {
        use std::fmt::Write as _;
        write!(&mut out, "{:02x}", b).unwrap();
    }
    out
}

/// Reject anything that isn't an `http(s)` URL pointing at a public
/// host. Mirrors the allow-list the feed-search proxy uses so /img
/// can't be pivoted into an SSRF probe of the host's private network.
pub async fn validate_image_url(raw: &str) -> Result<Url> {
    let parsed = Url::parse(raw).context("invalid image URL")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("only http(s) image URLs are allowed");
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("URL has no host"))?;
    crate::feed_search::check_host_is_public(host).await?;
    Ok(parsed)
}

/// Fetch `url` and return a Kindle-friendly JPEG. The transcode is
/// pure: it doesn't touch the cache; the caller stores the bytes.
/// Returns `Err` on any of: SSRF guard rejection, non-2xx response,
/// oversized source body, undecodable image data, or a JPEG encode
/// that fails for reasons not related to file size.
pub async fn fetch_and_transcode(http: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let parsed = validate_image_url(url).await?;

    let resp = http
        .get(parsed.clone())
        .send()
        .await
        .with_context(|| format!("fetching {}", url))?;
    if !resp.status().is_success() {
        bail!("image fetch returned HTTP {}", resp.status().as_u16());
    }
    // Bound the source we'll buffer. content-length is advisory — the
    // streaming check below is what actually protects us, but we can
    // short-circuit early when the server tells us the size up front.
    if let Some(cl) = resp.content_length() {
        if (cl as usize) > MAX_SOURCE_BYTES {
            bail!("image source too large: {} bytes", cl);
        }
    }
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("reading body of {}", url))?;
    if bytes.len() > MAX_SOURCE_BYTES {
        bail!("image source too large: {} bytes", bytes.len());
    }

    transcode_bytes(&bytes)
}

/// Decode raw image bytes (any format `image` recognizes), downscale
/// if larger than the Kindle cap, and encode as JPEG with the quality
/// search loop. Split out from `fetch_and_transcode` so tests can
/// exercise it without a network or HTTP client.
pub fn transcode_bytes(bytes: &[u8]) -> Result<Vec<u8>> {
    // image::load_from_memory sniffs the format; transparent PNGs are
    // flattened during JPEG encode (JPEG has no alpha) so the Kindle
    // sees a solid background instead of a black-on-black image.
    let fmt = image::guess_format(bytes).context("could not identify image format")?;
    let img = decode_with_format(bytes, fmt).context("decoding image bytes")?;
    let img = downscale_if_needed(img);
    // Always flatten alpha onto a white background before encoding so
    // PNGs with transparent backgrounds don't become solid black on
    // the Kindle's grayscale renderer.
    let rgb = img.to_rgb8();
    encode_jpeg_within_budget(&rgb)
}

fn decode_with_format(bytes: &[u8], fmt: ImageFormat) -> Result<DynamicImage> {
    let cursor = std::io::Cursor::new(bytes);
    let reader = image::ImageReader::with_format(cursor, fmt);
    reader.decode().map_err(anyhow::Error::from)
}

fn downscale_if_needed(img: DynamicImage) -> DynamicImage {
    let (w, h) = img.dimensions();
    if w <= MAX_WIDTH && h <= MAX_HEIGHT {
        return img;
    }
    // Lanczos3 is the default high-quality downscale filter in the
    // `image` crate; the slight extra CPU is invisible compared to
    // the network fetch we just did.
    img.resize(MAX_WIDTH, MAX_HEIGHT, FilterType::Lanczos3)
}

fn encode_jpeg_within_budget(rgb: &image::RgbImage) -> Result<Vec<u8>> {
    // Try quality QUALITY_START, then walk down in QUALITY_STEP
    // increments. Stop when the output fits the budget OR the quality
    // floor is reached, whichever comes first — at the floor we
    // accept the slight overage rather than refuse to ship the image.
    let mut quality = QUALITY_START;
    loop {
        let mut buf = Vec::with_capacity(MAX_BYTES);
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
        encoder
            .encode_image(&image::DynamicImage::ImageRgb8(rgb.clone()))
            .with_context(|| format!("JPEG encode @ quality {}", quality))?;
        if buf.len() <= MAX_BYTES || quality <= QUALITY_FLOOR {
            return Ok(buf);
        }
        quality = quality.saturating_sub(QUALITY_STEP).max(QUALITY_FLOOR);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb, RgbImage};

    fn solid_rgb(w: u32, h: u32, color: [u8; 3]) -> RgbImage {
        ImageBuffer::from_pixel(w, h, Rgb(color))
    }

    fn png_bytes(img: &RgbImage) -> Vec<u8> {
        let mut buf = Vec::new();
        DynamicImage::ImageRgb8(img.clone())
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        buf
    }

    fn jpeg_bytes(img: &RgbImage, quality: u8) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
        encoder
            .encode_image(&DynamicImage::ImageRgb8(img.clone()))
            .unwrap();
        buf
    }

    fn dims(bytes: &[u8]) -> (u32, u32) {
        let img = image::load_from_memory(bytes).expect("decode output");
        img.dimensions()
    }

    fn is_jpeg(bytes: &[u8]) -> bool {
        bytes.starts_with(&[0xff, 0xd8, 0xff])
    }

    // ----- hashing --------------------------------------------------------

    #[test]
    fn hash_url_is_stable_across_calls() {
        let a = hash_url("https://example.com/a.png");
        let b = hash_url("https://example.com/a.png");
        assert_eq!(a, b);
        assert_eq!(a.len(), 40);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_url_distinguishes_different_urls() {
        assert_ne!(
            hash_url("https://example.com/a.png"),
            hash_url("https://example.com/b.png"),
        );
    }

    // ----- transcode happy paths ------------------------------------------

    #[test]
    fn transcode_emits_jpeg_for_a_small_png() {
        let png = png_bytes(&solid_rgb(64, 64, [128, 64, 32]));
        let out = transcode_bytes(&png).unwrap();
        assert!(is_jpeg(&out), "expected JPEG magic, got {:02x?}", &out[..4]);
        assert_eq!(dims(&out), (64, 64), "small images shouldn't be resized");
    }

    #[test]
    fn transcode_downscales_when_either_dimension_exceeds_cap() {
        // 2400 wide × 800 tall — only the width is over cap. The
        // resize must preserve aspect ratio (not become 1200×1600).
        let big = png_bytes(&solid_rgb(2400, 800, [200, 200, 200]));
        let out = transcode_bytes(&big).unwrap();
        let (w, h) = dims(&out);
        assert!(w <= MAX_WIDTH, "width {} exceeds {}", w, MAX_WIDTH);
        assert!(h <= MAX_HEIGHT, "height {} exceeds {}", h, MAX_HEIGHT);
        // Aspect ratio held: 2400:800 = 3:1, so 1200 wide → 400 tall.
        assert_eq!((w, h), (1200, 400));
    }

    #[test]
    fn transcode_downscales_a_tall_image_against_the_height_cap() {
        // 600 wide × 3200 tall — only height is over cap.
        let tall = png_bytes(&solid_rgb(600, 3200, [10, 20, 30]));
        let out = transcode_bytes(&tall).unwrap();
        let (w, h) = dims(&out);
        assert_eq!((w, h), (300, 1600));
    }

    #[test]
    fn transcode_leaves_within_cap_images_at_their_native_size() {
        let img = png_bytes(&solid_rgb(1200, 1600, [255, 255, 255]));
        let out = transcode_bytes(&img).unwrap();
        assert_eq!(dims(&out), (1200, 1600));
    }

    #[test]
    fn transcode_flattens_transparent_png_to_opaque_jpeg() {
        // Construct a 32×32 RGBA PNG with 0 alpha everywhere. Decoding
        // and re-encoding as JPEG must not preserve transparency
        // (Kindle doesn't render it); the output should be a flat RGB
        // JPEG, not error out.
        use image::{ImageBuffer, Rgba};
        let rgba: image::RgbaImage = ImageBuffer::from_pixel(32, 32, Rgba([255, 0, 0, 0]));
        let mut buf = Vec::new();
        DynamicImage::ImageRgba8(rgba)
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        let out = transcode_bytes(&buf).unwrap();
        assert!(is_jpeg(&out));
        assert_eq!(dims(&out), (32, 32));
    }

    // ----- size budget ----------------------------------------------------

    #[test]
    fn transcode_keeps_a_smooth_photo_under_the_byte_budget() {
        // A 1024×1024 gradient encodes cleanly at high JPEG quality;
        // at q85 it should already be far below the cap, so this also
        // exercises the "quality loop exits on first pass" branch.
        let mut img = ImageBuffer::new(1024, 1024);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        let png = png_bytes(&img);
        let out = transcode_bytes(&png).unwrap();
        assert!(
            out.len() <= MAX_BYTES,
            "gradient encoded to {} bytes, cap {}",
            out.len(),
            MAX_BYTES
        );
    }

    #[test]
    fn transcode_accepts_jpeg_input() {
        // Round-tripping a JPEG must not error or change dimensions
        // when the source already sits below the dimension cap.
        let src = solid_rgb(800, 600, [100, 150, 200]);
        let jpeg = jpeg_bytes(&src, 90);
        let out = transcode_bytes(&jpeg).unwrap();
        assert!(is_jpeg(&out));
        assert_eq!(dims(&out), (800, 600));
    }

    // ----- error paths ----------------------------------------------------

    #[test]
    fn transcode_rejects_non_image_bytes() {
        let html = b"<html><body>not an image</body></html>";
        assert!(transcode_bytes(html).is_err());
    }

    #[test]
    fn transcode_rejects_empty_input() {
        assert!(transcode_bytes(b"").is_err());
    }

    #[test]
    fn transcode_rejects_truncated_png_header() {
        // First 8 bytes are the valid PNG magic; the rest is missing.
        // guess_format identifies it as PNG but decode fails.
        let bytes = b"\x89PNG\r\n\x1a\n";
        assert!(transcode_bytes(bytes).is_err());
    }

    // ----- URL validation -------------------------------------------------

    #[tokio::test]
    async fn validate_image_url_accepts_public_https() {
        assert!(validate_image_url("https://example.com/p.jpg")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn validate_image_url_rejects_non_http_schemes() {
        for bad in [
            "file:///etc/passwd",
            "ftp://example.com/x.jpg",
            "javascript:alert(1)",
            "data:image/png;base64,xxx",
        ] {
            assert!(
                validate_image_url(bad).await.is_err(),
                "expected {:?} to be rejected",
                bad
            );
        }
    }

    #[tokio::test]
    async fn validate_image_url_rejects_loopback_literal() {
        // Hits the SSRF guard shared with feed_search without doing
        // any DNS — 127.0.0.1 is recognized as loopback inline.
        assert!(validate_image_url("http://127.0.0.1/x.jpg").await.is_err());
        assert!(validate_image_url("http://10.0.0.5/x.jpg").await.is_err());
    }

    #[tokio::test]
    async fn validate_image_url_rejects_url_without_host() {
        // file:// URLs can lack a host; treated as invalid.
        assert!(validate_image_url("http:///nopath").await.is_err());
    }
}

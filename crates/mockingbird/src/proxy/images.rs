//! Placeholder images for `proxy.images` hosts: a solid-colour PNG per
//! path, generated without an LLM.

use std::io::Write as _;

use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use flate2::Compression;
use flate2::write::ZlibEncoder;
use mb_protocols::http_rest::problem;
use sha2::{Digest, Sha256};

/// `(width, height)` when the path has no `w<width>` segment.
pub const DEFAULT_IMAGE_SIZE: (u32, u32) = (300, 450);

/// Widest image a `w<width>` segment may ask for.
const MAX_WIDTH: u32 = 2000;

/// From the first `w<digits>` segment of `path` with a width in
/// `1..=2000`: `(w, w * 3 / 2)` (a poster's aspect ratio); else
/// [`DEFAULT_IMAGE_SIZE`].
pub fn image_size(path: &str) -> (u32, u32) {
    path.split('/')
        .filter_map(|seg| seg.strip_prefix('w'))
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .find_map(|digits| digits.parse::<u32>().ok())
        .filter(|w| (1..=MAX_WIDTH).contains(w))
        .map_or(DEFAULT_IMAGE_SIZE, |w| (w, w * 3 / 2))
}

/// The placeholder's colour: the first three bytes of SHA-256 of `path`.
pub fn color_for(path: &str) -> [u8; 3] {
    let digest = Sha256::digest(path.as_bytes());
    [digest[0], digest[1], digest[2]]
}

/// An 8-bit RGB PNG of [`image_size`] filled with [`color_for`].
pub fn placeholder_png(path: &str) -> Vec<u8> {
    let (width, height) = image_size(path);
    let color = color_for(path);
    let mut row = Vec::with_capacity(1 + width as usize * 3);
    row.push(0); // filter type: none
    for _ in 0..width {
        row.extend_from_slice(&color);
    }
    let mut zlib = ZlibEncoder::new(Vec::new(), Compression::default());
    for _ in 0..height {
        // Writing to a Vec cannot fail.
        let _ = zlib.write_all(&row);
    }
    let idat = zlib.finish().unwrap_or_default();

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, RGB, deflate, no filter, no interlace

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &idat);
    chunk(&mut png, b"IEND", &[]);
    png
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend_from_slice(kind);
    png.extend_from_slice(data);
    let mut crc = flate2::Crc::new();
    crc.update(&png[start..]);
    png.extend_from_slice(&crc.sum().to_be_bytes());
}

/// `GET`: 200 with the [`placeholder_png`] for `path`; `HEAD`: the same
/// headers without a body; anything else: 405 problem+json.
pub fn placeholder_response(method: &Method, path: &str) -> Response {
    if method != Method::GET && method != Method::HEAD {
        let mut resp = problem(
            405,
            "Method Not Allowed",
            format!("placeholder images answer GET and HEAD, not {method}"),
        );
        resp.headers_mut()
            .insert(header::ALLOW, header::HeaderValue::from_static("GET, HEAD"));
        return resp;
    }
    let png = placeholder_png(path);
    let headers = [
        (header::CONTENT_TYPE, "image/png".to_owned()),
        (header::CACHE_CONTROL, "public, max-age=86400".to_owned()),
        (header::CONTENT_LENGTH, png.len().to_string()),
    ];
    if method == Method::HEAD {
        (StatusCode::OK, headers).into_response()
    } else {
        (StatusCode::OK, headers, png).into_response()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn size_comes_from_the_w_segment() {
        assert_eq!(image_size("/t/p/w92/abc.jpg"), (92, 138));
        assert_eq!(image_size("/t/p/original/abc.jpg"), (300, 450));
        assert_eq!(image_size("/t/p/w99999/abc.jpg"), (300, 450));
    }

    #[test]
    fn png_is_well_formed_and_solid() {
        let png = placeholder_png("/t/p/w4/x.jpg");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 4);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 6);
        let mut crc = flate2::Crc::new();
        crc.update(&png[12..29]);
        assert_eq!(
            crc.sum(),
            u32::from_be_bytes(png[29..33].try_into().unwrap())
        );
        let idat_len = u32::from_be_bytes(png[33..37].try_into().unwrap()) as usize;
        assert_eq!(&png[37..41], b"IDAT");
        let mut raw = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::ZlibDecoder::new(&png[41..41 + idat_len]),
            &mut raw,
        )
        .unwrap();
        assert_eq!(raw.len(), 6 * (1 + 4 * 3));
        assert_eq!(&raw[1..4], &color_for("/t/p/w4/x.jpg"));
        assert!(png.ends_with(b"IEND\xaeB`\x82"));
        assert_ne!(color_for("/a.jpg"), color_for("/b.jpg"));
    }
}

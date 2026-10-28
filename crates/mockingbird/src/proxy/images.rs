//! Placeholder images for `proxy.images` entries (a host, or a host and a
//! path prefix such as `movies-proxy.vercel.app/ipx`): a solid-colour PNG
//! per path, generated without an LLM.

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

/// One `proxy.images` entry: requests to `host` whose path lies under
/// `prefix` (any path without one) get placeholder images.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageRule {
    pub host: String,
    pub prefix: Option<String>,
}

impl ImageRule {
    /// Parses `host` or `host/path/prefix` (lowercased host, no port).
    pub fn parse(entry: &str) -> Result<ImageRule, String> {
        let (host, prefix) = match entry.split_once('/') {
            Some((host, rest)) => (host, Some(format!("/{}", rest.trim_end_matches('/')))),
            None => (entry, None),
        };
        let host = host.to_ascii_lowercase();
        mb_store::validate_hosts(std::slice::from_ref(&host)).map_err(|e| e.to_string())?;
        let prefix = prefix.filter(|p| p != "/");
        if let Some(p) = &prefix
            && mb_store::validate_route_prefix(p).is_err()
        {
            return Err(format!(
                "{entry:?}: the path after the host must look like /seg/seg"
            ));
        }
        Ok(ImageRule { host, prefix })
    }

    /// Whether a request for `path` on `host` gets a placeholder.
    pub fn matches(&self, host: &str, path: &str) -> bool {
        self.host == host
            && self.prefix.as_deref().is_none_or(|prefix| {
                path.strip_prefix(prefix)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
            })
    }
}

/// Checks every `proxy.images` entry.
pub fn validate_image_entries(entries: &[String]) -> Result<(), String> {
    entries
        .iter()
        .try_for_each(|e| ImageRule::parse(e).map(drop))
}

/// The size a path asks for: a `w<digits>` segment (TMDB's `w500`) gives
/// `(w, w * 3 / 2)` (a poster's aspect ratio); an ipx-style modifier
/// segment (`s_400x600`, or `w_400` among `&`/`,`-separated modifiers)
/// gives that size. Dimensions must be in `1..=2000`; otherwise
/// [`DEFAULT_IMAGE_SIZE`].
pub fn image_size(path: &str) -> (u32, u32) {
    let ok = |v: u32| (1..=MAX_WIDTH).contains(&v);
    let num = |s: &str| {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u32>().ok())
            .flatten()
            .filter(|v| ok(*v))
    };
    for seg in path.split('/') {
        if let Some(w) = seg.strip_prefix('w').and_then(num) {
            return (w, w * 3 / 2);
        }
        for modifier in seg.split(['&', ',']) {
            if let Some((w, h)) = modifier
                .strip_prefix("s_")
                .and_then(|s| s.split_once('x'))
                .and_then(|(w, h)| Some((num(w)?, num(h)?)))
            {
                return (w, h);
            }
            if let Some(w) = modifier.strip_prefix("w_").and_then(num) {
                return (w, w * 3 / 2);
            }
        }
    }
    DEFAULT_IMAGE_SIZE
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
    fn rules_match_hosts_and_path_prefixes() {
        let any = ImageRule::parse("Image.TMDB.org").unwrap();
        assert!(any.matches("image.tmdb.org", "/t/p/w92/a.jpg"));
        let ipx = ImageRule::parse("movies-proxy.vercel.app/ipx/").unwrap();
        assert_eq!(ipx.prefix.as_deref(), Some("/ipx"));
        assert!(ipx.matches("movies-proxy.vercel.app", "/ipx/s_400x600/tmdb/a.jpg"));
        assert!(!ipx.matches("movies-proxy.vercel.app", "/tmdb/movie/1"));
        assert!(!ipx.matches("movies-proxy.vercel.app", "/ipxx/a.jpg"));
        assert!(ImageRule::parse("bad host/x").is_err());
        assert!(ImageRule::parse("a.test/sp ace").is_err());
    }

    #[test]
    fn size_comes_from_the_w_segment() {
        assert_eq!(image_size("/ipx/s_400x600/tmdb/a.jpg"), (400, 600));
        assert_eq!(image_size("/ipx/f_webp&w_200/tmdb/a.jpg"), (200, 300));
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

//! A bounded pre-scan of a PDF's streams, run before `lopdf` sees the bytes.
//!
//! `lopdf` inflates FlateDecode and LZWDecode streams (object streams and
//! cross-reference streams while loading, content streams while extracting)
//! with no size limit. A one-megabyte "decompression bomb" can therefore ask
//! for gigabytes, and an allocation failure aborts the whole process: no
//! `catch_unwind` can contain it. This scan decodes every stream the way
//! `lopdf` could, with streaming decoders and a cumulative cap, and refuses
//! the PDF before any unbounded decoding happens.
//!
//! The scan does not trust the stream dictionaries. It starts at every
//! `stream` keyword and tries each filter `lopdf` implements (Flate with its
//! raw-deflate fallback, LZW with and without early change, ASCII85) on the
//! bytes that follow, then the same filters on each output, up to
//! [`MAX_FILTERS`] layers. A `/Filter` array longer than that is refused
//! outright, so every chain `lopdf` would decode is covered. What is counted
//! is growth: output bytes beyond the input bytes they came from. Encrypted
//! PDFs are refused up front, because their streams are only decoded after
//! decryption.

use flate2::{Decompress, FlushDecompress, Status};
use weezl::{BitOrder, LzwStatus, decode::Decoder as LzwDecoder};

/// Total growth allowed across all streams and filter layers.
pub(crate) const MAX_PDF_EXPANSION: usize = 64 * 1024 * 1024;
/// The longest filter chain decoded (and allowed in a `/Filter` array).
const MAX_FILTERS: usize = 8;
/// Decoder runs allowed for one PDF.
const MAX_ATTEMPTS: usize = 1_000_000;
/// Input bytes the decoders may read in total for one PDF.
const MAX_WORK: usize = 512 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Decoding would grow past [`MAX_PDF_EXPANSION`].
    Expands,
    /// Too many streams, filters or decoded bytes to scan.
    TooComplex,
    Encrypted,
}

/// Refuse a PDF that is encrypted, that chains too many filters, or whose
/// streams would expand past [`MAX_PDF_EXPANSION`] when decoded.
pub(crate) fn check(bytes: &[u8]) -> Result<(), Refusal> {
    check_names(bytes)?;
    let mut scan = Scan::new();
    for start in stream_starts(bytes) {
        scan.layer(bytes.get(start..).unwrap_or_default(), 0)?;
    }
    Ok(())
}

/// `lopdf`'s whitespace (PDF 32000-1 §7.2.2).
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\0' | 0x0C)
}

fn is_delimiter(c: u8) -> bool {
    matches!(
        c,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// Parse the name starting at `bytes[at] == b'/'` the way `lopdf` does
/// (`#hh` escapes decoded). Returns the name and the index after it.
fn name_at(bytes: &[u8], at: usize) -> (Vec<u8>, usize) {
    let mut name = Vec::new();
    let mut i = at + 1;
    while let Some(&c) = bytes.get(i) {
        if c == b'#' {
            match (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            ) {
                (Some(h), Some(l)) => {
                    name.push(h << 4 | l);
                    i += 3;
                }
                _ => break,
            }
        } else if is_space(c) || is_delimiter(c) {
            break;
        } else {
            name.push(c);
            i += 1;
        }
    }
    (name, i)
}

/// Skip whitespace and comments.
fn skip_space(bytes: &[u8], mut i: usize) -> usize {
    while let Some(&c) = bytes.get(i) {
        if is_space(c) {
            i += 1;
        } else if c == b'%' {
            while bytes.get(i).is_some_and(|&c| c != b'\r' && c != b'\n') {
                i += 1;
            }
        } else {
            break;
        }
    }
    i
}

/// Refuse `/Encrypt` anywhere, and any `/Filter` array of more than
/// [`MAX_FILTERS`] names. Names are found by byte scanning, so names in
/// strings or binary data count too; that only errs on the safe side.
fn check_names(bytes: &[u8]) -> Result<(), Refusal> {
    let mut i = 0;
    while let Some(off) = bytes
        .get(i..)
        .and_then(|b| b.iter().position(|&c| c == b'/'))
    {
        let (name, next) = name_at(bytes, i + off);
        i = next.max(i + off + 1);
        if name == b"Encrypt" {
            return Err(Refusal::Encrypted);
        }
        if name != b"Filter" {
            continue;
        }
        let mut j = skip_space(bytes, next);
        if bytes.get(j) != Some(&b'[') {
            continue;
        }
        j += 1;
        let mut count = 0usize;
        loop {
            j = skip_space(bytes, j);
            match bytes.get(j) {
                Some(b'/') => {
                    count += 1;
                    if count > MAX_FILTERS {
                        return Err(Refusal::TooComplex);
                    }
                    let (_, after) = name_at(bytes, j);
                    j = after.max(j + 1);
                }
                Some(b']') | None => break,
                Some(_) => j += 1,
            }
        }
    }
    Ok(())
}

/// Where stream data would start for every `stream` keyword: after the
/// keyword, optional spaces or tabs and one end-of-line, as `lopdf` parses
/// it. `endstream` is skipped.
fn stream_starts(bytes: &[u8]) -> Vec<usize> {
    const KEYWORD: &[u8] = b"stream";
    let mut starts = Vec::new();
    let mut i = 0;
    while let Some(off) = bytes
        .get(i..)
        .and_then(|b| b.windows(KEYWORD.len()).position(|w| w == KEYWORD))
    {
        let at = i + off;
        i = at + KEYWORD.len();
        if at >= 3 && bytes.get(at - 3..at) == Some(b"end") {
            continue;
        }
        let mut j = i;
        while matches!(bytes.get(j), Some(b' ' | b'\t')) {
            j += 1;
        }
        match (bytes.get(j), bytes.get(j + 1)) {
            (Some(b'\r'), Some(b'\n')) => starts.push(j + 2),
            (Some(b'\r' | b'\n'), _) => starts.push(j + 1),
            _ => {}
        }
    }
    starts
}

#[derive(Clone, Copy)]
enum Filter {
    Flate,
    LzwEarly,
    LzwLate,
    Ascii85,
}

const FILTERS: [Filter; 4] = [
    Filter::Flate,
    Filter::LzwEarly,
    Filter::LzwLate,
    Filter::Ascii85,
];

struct Scan {
    growth_left: usize,
    work_left: usize,
    attempts: usize,
    zlib: Decompress,
    raw: Decompress,
    lzw_early: LzwDecoder,
    lzw_late: LzwDecoder,
    buf: Vec<u8>,
}

/// Output collected under the growth cap.
struct Sink<'a> {
    out: Vec<u8>,
    growth_left: &'a usize,
}

impl Sink<'_> {
    /// Append `chunk`, refusing once the output exceeds the input consumed
    /// so far by more than the remaining growth.
    fn push(&mut self, chunk: &[u8], consumed: usize) -> Result<(), Refusal> {
        self.out.extend_from_slice(chunk);
        self.check(consumed)
    }

    fn check(&self, consumed: usize) -> Result<(), Refusal> {
        if self.out.len() > consumed.saturating_add(*self.growth_left) {
            Err(Refusal::Expands)
        } else {
            Ok(())
        }
    }
}

impl Scan {
    fn new() -> Self {
        Scan {
            growth_left: MAX_PDF_EXPANSION,
            work_left: MAX_WORK,
            attempts: 0,
            zlib: Decompress::new(true),
            raw: Decompress::new(false),
            // lopdf: `MIN_BITS - 1` = 8-bit symbols, MSB first.
            lzw_early: LzwDecoder::with_tiff_size_switch(BitOrder::Msb, 8),
            lzw_late: LzwDecoder::new(BitOrder::Msb, 8),
            buf: vec![0; CHUNK],
        }
    }

    /// Decode `data` with every filter, then each non-empty output again,
    /// until `depth` reaches [`MAX_FILTERS`].
    fn layer(&mut self, data: &[u8], depth: usize) -> Result<(), Refusal> {
        for filter in FILTERS {
            let out = self.decode(filter, data)?;
            if !out.is_empty() && depth + 1 < MAX_FILTERS {
                self.layer(&out, depth + 1)?;
            }
        }
        Ok(())
    }

    fn decode(&mut self, filter: Filter, data: &[u8]) -> Result<Vec<u8>, Refusal> {
        self.attempts += 1;
        if self.attempts > MAX_ATTEMPTS {
            return Err(Refusal::TooComplex);
        }
        let (out, consumed) = match filter {
            Filter::Flate => self.flate(data)?,
            Filter::LzwEarly | Filter::LzwLate => self.lzw(filter, data)?,
            Filter::Ascii85 => ascii85(data, &self.growth_left)?,
        };
        self.work_left = self
            .work_left
            .checked_sub(consumed)
            .ok_or(Refusal::TooComplex)?;
        self.growth_left = self
            .growth_left
            .checked_sub(out.len().saturating_sub(consumed))
            .ok_or(Refusal::Expands)?;
        Ok(out)
    }

    /// lopdf: zlib; when that fails with no output, raw deflate after the
    /// two header bytes. Here the fallback runs whenever zlib gives nothing.
    fn flate(&mut self, data: &[u8]) -> Result<(Vec<u8>, usize), Refusal> {
        let (out, consumed) = inflate(&mut self.zlib, true, data, &self.growth_left)?;
        if out.is_empty() && data.len() > 2 {
            let rest = data.get(2..).unwrap_or_default();
            let (out, raw_consumed) = inflate(&mut self.raw, false, rest, &self.growth_left)?;
            return Ok((out, consumed.max(raw_consumed + 2)));
        }
        Ok((out, consumed))
    }

    fn lzw(&mut self, filter: Filter, data: &[u8]) -> Result<(Vec<u8>, usize), Refusal> {
        let decoder = match filter {
            Filter::LzwEarly => &mut self.lzw_early,
            _ => &mut self.lzw_late,
        };
        decoder.reset();
        let mut sink = Sink {
            out: Vec::new(),
            growth_left: &self.growth_left,
        };
        let mut consumed = 0usize;
        loop {
            let input = data.get(consumed..).unwrap_or_default();
            let result = decoder.decode_bytes(input, &mut self.buf);
            consumed += result.consumed_in;
            let chunk = self.buf.get(..result.consumed_out).unwrap_or_default();
            sink.push(chunk, consumed)?;
            match result.status {
                Ok(LzwStatus::Ok) if result.consumed_in + result.consumed_out > 0 => {}
                _ => break,
            }
        }
        Ok((sink.out, consumed))
    }
}

/// Inflate `data` (zlib or raw) under the growth cap. Stops at the end of
/// the deflate stream or at the first error; partial output counts.
fn inflate(
    d: &mut Decompress,
    zlib: bool,
    data: &[u8],
    growth_left: &usize,
) -> Result<(Vec<u8>, usize), Refusal> {
    d.reset(zlib);
    let mut sink = Sink {
        out: Vec::new(),
        growth_left,
    };
    loop {
        let consumed = usize::try_from(d.total_in()).unwrap_or(usize::MAX);
        let input = data.get(consumed..).unwrap_or_default();
        if sink.out.capacity() - sink.out.len() < CHUNK {
            sink.out.reserve(CHUNK);
        }
        let before = (d.total_in(), d.total_out());
        let status = d.decompress_vec(input, &mut sink.out, FlushDecompress::None);
        let consumed = usize::try_from(d.total_in()).unwrap_or(usize::MAX);
        sink.check(consumed)?;
        match status {
            Ok(Status::Ok) if (d.total_in(), d.total_out()) != before => {}
            _ => return Ok((sink.out, consumed)),
        }
    }
}

/// lopdf's ASCII85 decoder: whitespace skipped, `z` is four zero bytes,
/// the first other byte outside `!`..=`u` ends the data.
fn ascii85(data: &[u8], growth_left: &usize) -> Result<(Vec<u8>, usize), Refusal> {
    let mut sink = Sink {
        out: Vec::new(),
        growth_left,
    };
    let mut group: u32 = 0;
    let mut count = 0usize;
    let mut consumed = 0usize;
    for &c in data {
        if c == b'z' {
            if count != 0 {
                // lopdf fails the whole stream here.
                break;
            }
            sink.push(&[0; 4], consumed + 1)?;
        } else if (b'!'..=b'u').contains(&c) {
            group = group.wrapping_mul(85).wrapping_add(u32::from(c - b'!'));
            count += 1;
            if count == 5 {
                sink.push(&group.to_be_bytes(), consumed + 1)?;
                group = 0;
                count = 0;
            }
        } else if !c.is_ascii_whitespace() {
            break;
        }
        consumed += 1;
    }
    if count > 1 {
        for _ in count..5 {
            group = group.wrapping_mul(85).wrapping_add(84);
        }
        let bytes = group.to_be_bytes();
        sink.push(bytes.get(..count - 1).unwrap_or_default(), consumed)?;
    }
    Ok((sink.out, consumed))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn stream_starts_follow_lopdf() {
        let pdf = b"<<>>stream\r\nAB\nendstream <<>> stream \nCD endstream\nstreamX";
        let starts = stream_starts(pdf);
        assert_eq!(starts.len(), 2);
        assert_eq!(pdf.get(starts[0]..starts[0] + 2), Some(&b"AB"[..]));
        assert_eq!(pdf.get(starts[1]..starts[1] + 2), Some(&b"CD"[..]));
    }

    #[test]
    fn names_are_decoded() {
        assert_eq!(
            check_names(b"<< /Encr#79pt 5 0 R >>"),
            Err(Refusal::Encrypted)
        );
        assert_eq!(check_names(b"<< /Filter [/A /B] >>"), Ok(()));
        let long = format!("<< /Fil#74er % c\n[{}] >>", "/A85 ".repeat(9));
        assert_eq!(check_names(long.as_bytes()), Err(Refusal::TooComplex));
    }

    #[test]
    fn ascii85_z_growth_is_counted() {
        let data = vec![b'z'; 1000];
        let (out, consumed) = ascii85(&data, &usize::MAX).unwrap();
        assert_eq!((out.len(), consumed), (4000, 1000));
        assert_eq!(ascii85(&data, &100), Err(Refusal::Expands));
    }

    #[test]
    fn lzw_growth_is_capped() {
        let zeros = vec![0u8; 1024 * 1024];
        let encoded = weezl::encode::Encoder::with_tiff_size_switch(BitOrder::Msb, 8)
            .encode(&zeros)
            .unwrap();
        let mut scan = Scan::new();
        let (out, _) = scan.lzw(Filter::LzwEarly, &encoded).unwrap();
        assert_eq!(out, zeros);
        scan.growth_left = 1000;
        assert_eq!(scan.layer(&encoded, 0).map(|_| ()), Err(Refusal::Expands));
    }

    #[test]
    fn ascii85_matches_lopdf() {
        // "Man " in ASCII85, then the end marker.
        let (out, _) = ascii85(b"9jqo^~>", &usize::MAX).unwrap();
        assert_eq!(out, b"Man ");
    }
}

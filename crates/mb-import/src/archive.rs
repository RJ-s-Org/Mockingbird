//! Multi-file contract archives: zip and gzip-compressed tar.
//!
//! Archives are unpacked in memory only. Entry paths are validated (no
//! absolute paths, `..`, backslashes, NULs or drive letters; no links or
//! devices) and every byte of content is read through a limiting reader, so
//! a hostile archive cannot escape the upload or expand without bound.

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::path::Path;

use crate::{ImportError, SourceFile};

/// Largest accepted archive upload.
pub const MAX_ARCHIVE_BYTES: usize = 20 * 1024 * 1024;
/// Most member files an archive may hold.
pub const MAX_ARCHIVE_FILES: usize = 500;
/// Most bytes of member content an archive may unpack to.
pub const MAX_ARCHIVE_UNPACKED_BYTES: u64 = 64 * 1024 * 1024;

/// The magic bytes of a zip local file header.
const ZIP_MAGIC: &[u8] = b"PK\x03\x04";
/// The magic bytes of a gzip stream.
const GZIP_MAGIC: &[u8] = &[0x1f, 0x8b];

/// Unix file-type bits of a mode.
const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;

/// Whether `bytes` start like a zip or gzip archive.
pub(crate) fn is_archive(bytes: &[u8]) -> bool {
    bytes.starts_with(ZIP_MAGIC) || bytes.starts_with(GZIP_MAGIC)
}

/// Unpack a zip (`PK\x03\x04`) or gzip-compressed tar (`1f 8b`) archive into
/// its member files, sorted by path. Directories are skipped, as are
/// `__MACOSX/` entries and hidden (dot-prefixed) files and directories, and
/// a leading `./` is stripped; when every file shares one top-level
/// directory, that directory is stripped too.
pub fn unpack_archive(bytes: &[u8]) -> Result<Vec<SourceFile>, ImportError> {
    let mut members = Members::default();
    if bytes.starts_with(ZIP_MAGIC) {
        unpack_zip(bytes, &mut members)?;
    } else if bytes.starts_with(GZIP_MAGIC) {
        unpack_tar_gz(bytes, &mut members)?;
    } else {
        return Err(ImportError::Unsupported(
            "not a zip or gzip-compressed tar archive".into(),
        ));
    }
    Ok(members.finish())
}

/// The files read so far and the budgets they consume.
#[derive(Default)]
struct Members {
    files: BTreeMap<String, Vec<u8>>,
    unpacked: u64,
}

impl Members {
    /// Read one file's content from `reader`, within the remaining budget.
    fn add(&mut self, raw_name: &str, path: String, reader: impl Read) -> Result<(), ImportError> {
        if self.files.len() >= MAX_ARCHIVE_FILES {
            return Err(invalid(
                "/",
                format!("the archive holds more than {MAX_ARCHIVE_FILES} files"),
            ));
        }
        if self.files.contains_key(&path) {
            return Err(invalid(raw_name, "duplicate archive entry"));
        }
        let remaining = MAX_ARCHIVE_UNPACKED_BYTES - self.unpacked;
        let mut bytes = Vec::new();
        reader
            .take(remaining + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| invalid(raw_name, format!("cannot read the entry: {e}")))?;
        self.unpacked += bytes.len() as u64;
        if self.unpacked > MAX_ARCHIVE_UNPACKED_BYTES {
            return Err(too_large());
        }
        self.files.insert(path, bytes);
        Ok(())
    }

    /// The files sorted by path, with a shared top-level directory stripped.
    fn finish(self) -> Vec<SourceFile> {
        let shared_dir = {
            let mut tops = self.files.keys().map(|p| p.split_once('/').map(|(d, _)| d));
            match tops.next() {
                Some(Some(first)) if tops.all(|t| t == Some(first)) => Some(format!("{first}/")),
                _ => None,
            }
        };
        self.files
            .into_iter()
            .map(|(path, bytes)| SourceFile {
                path: match &shared_dir {
                    Some(dir) => path.strip_prefix(dir.as_str()).unwrap_or(&path).to_owned(),
                    None => path,
                },
                bytes,
            })
            .collect()
    }
}

fn invalid(pointer: &str, message: impl Into<String>) -> ImportError {
    ImportError::Invalid {
        pointer: pointer.to_owned(),
        message: message.into(),
    }
}

fn too_large() -> ImportError {
    invalid(
        "/",
        format!("the archive unpacks to more than {MAX_ARCHIVE_UNPACKED_BYTES} bytes"),
    )
}

/// Whether a normalised entry path is operating-system metadata rather
/// than content: under `__MACOSX/` (AppleDouble `._*` files), or with a
/// hidden (dot-prefixed) component such as `.DS_Store` or `.git/`. These
/// entries are dropped, as [`pack_dir`] skips hidden files.
fn is_metadata(path: &str) -> bool {
    path.split('/')
        .any(|part| part == "__MACOSX" || part.starts_with('.'))
}

/// Validate an entry name and normalise it to a `/`-separated relative path.
/// `None` means the entry names the archive root and is skipped.
fn entry_path(name: &str) -> Result<Option<String>, ImportError> {
    let reject = |why: &str| Err(invalid(name, format!("unsafe archive entry path: {why}")));
    if name.contains('\0') {
        return reject("contains NUL");
    }
    if name.contains('\\') {
        return reject("contains a backslash");
    }
    if name.starts_with('/') {
        return reject("absolute path");
    }
    let b = name.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return reject("drive letter");
    }
    let mut parts = Vec::new();
    for part in name.split('/') {
        match part {
            ".." => return reject("contains `..`"),
            "" | "." => {}
            p => parts.push(p),
        }
    }
    Ok((!parts.is_empty()).then(|| parts.join("/")))
}

fn unpack_zip(bytes: &[u8], members: &mut Members) -> Result<(), ImportError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| ImportError::Parse(format!("not a valid zip archive: {e}")))?;
    for i in 0..archive.len() {
        let file = archive
            .by_index(i)
            .map_err(|e| ImportError::Parse(format!("zip entry {i}: {e}")))?;
        let name = file
            .name()
            .map_err(|e| ImportError::Parse(format!("zip entry {i}: {e}")))?
            .into_owned();
        let path = entry_path(&name)?;
        let file_type = file.unix_mode().map_or(0, |m| m & S_IFMT);
        if file_type != 0 && file_type != S_IFREG && file_type != S_IFDIR {
            return Err(invalid(&name, "links and devices are not allowed"));
        }
        if file.is_dir() || file_type == S_IFDIR {
            continue;
        }
        let Some(path) = path else {
            return Err(invalid(&name, "unsafe archive entry path: empty"));
        };
        if is_metadata(&path) {
            continue;
        }
        members.add(&name, path, file)?;
    }
    Ok(())
}

fn unpack_tar_gz(bytes: &[u8], members: &mut Members) -> Result<(), ImportError> {
    // Everything decompressed, including headers and skipped entries, is
    // capped too, so padding or skipped data cannot expand without bound.
    let stream = flate2::read::GzDecoder::new(bytes).take(2 * MAX_ARCHIVE_UNPACKED_BYTES);
    let mut archive = tar::Archive::new(stream);
    let entries = archive
        .entries()
        .map_err(|e| ImportError::Parse(format!("not a valid tar.gz archive: {e}")))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| ImportError::Parse(format!("not a valid tar.gz archive: {e}")))?;
        let raw = entry.path_bytes();
        let name = std::str::from_utf8(&raw)
            .map_err(|_| invalid(&String::from_utf8_lossy(&raw), "entry path is not UTF-8"))?
            .to_owned();
        let path = entry_path(&name)?;
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            continue;
        }
        if kind.is_pax_global_extensions() || kind.is_pax_local_extensions() {
            continue;
        }
        if !(kind.is_file() || kind.is_contiguous()) {
            return Err(invalid(&name, "links and devices are not allowed"));
        }
        let Some(path) = path else {
            return Err(invalid(&name, "unsafe archive entry path: empty"));
        };
        if is_metadata(&path) {
            continue;
        }
        members.add(&name, path, entry)?;
    }
    Ok(())
}

/// Pack a directory as a deterministic `tar.gz`: sorted relative paths,
/// hidden entries skipped, mtime 0, mode 0644, uid/gid 0, and a gzip header
/// with mtime 0. Packing the same tree twice gives identical bytes.
pub fn pack_dir(dir: &Path) -> Result<Vec<u8>, ImportError> {
    let mut files = BTreeMap::new();
    collect(dir, "", &mut files)?;
    let gz = flate2::GzBuilder::new()
        .mtime(0)
        .operating_system(255)
        .write(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(gz);
    builder.mode(tar::HeaderMode::Deterministic);
    for (path, bytes) in &files {
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        builder
            .append_data(&mut header, path, bytes.as_slice())
            .map_err(|e| io_error(&dir.join(path), &e))?;
    }
    let gz = builder.into_inner().map_err(|e| io_error(dir, &e))?;
    gz.finish().map_err(|e| io_error(dir, &e))
}

/// Read every non-hidden file under `dir` into `files`, keyed by its
/// `/`-separated path relative to the packed root. Symbolic links to files
/// are followed; links to directories are skipped, so a cycle cannot loop.
fn collect(
    dir: &Path,
    prefix: &str,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), ImportError> {
    let entries = std::fs::read_dir(dir).map_err(|e| io_error(dir, &e))?;
    for entry in entries {
        let entry = entry.map_err(|e| io_error(dir, &e))?;
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(invalid(
                &path.display().to_string(),
                "file name is not UTF-8",
            ));
        };
        if name.starts_with('.') {
            continue;
        }
        let rel = format!("{prefix}{name}");
        let file_type = entry.file_type().map_err(|e| io_error(&path, &e))?;
        if file_type.is_dir() {
            collect(&path, &format!("{rel}/"), files)?;
        } else if file_type.is_file()
            || (file_type.is_symlink() && std::fs::metadata(&path).is_ok_and(|m| m.is_file()))
        {
            let bytes = std::fs::read(&path).map_err(|e| io_error(&path, &e))?;
            files.insert(rel, bytes);
        }
    }
    Ok(())
}

fn io_error(path: &Path, e: &std::io::Error) -> ImportError {
    invalid(&path.display().to_string(), format!("cannot read: {e}"))
}

/// The contract at `path`: a file's bytes, or [`pack_dir`] for a directory.
pub fn read_source(path: &Path) -> Result<Vec<u8>, ImportError> {
    let meta = std::fs::metadata(path).map_err(|e| io_error(path, &e))?;
    if meta.is_dir() {
        pack_dir(path)
    } else {
        std::fs::read(path).map_err(|e| io_error(path, &e))
    }
}

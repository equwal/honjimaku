//! How the files of an entry are kept on the disk: as they are, or compressed with zstd.
//!
//! The copy of another site is mostly text subtitles, and zstd makes them about 3 (`.srt`)
//! to 9 (`.ass`) times smaller. Such a file is kept as `name.zst` in place of `name`: one
//! standard zstd frame with the size before compression and a checksum, so that `zstd -d`
//! also reads it. Each part of the server that reads the folder of an entry asks this
//! module, so the pages, the downloads, the API and a zip of many files show `name`, the
//! size before compression, and the contents after decompression.
//!
//! Only text subtitles are compressed. An archive, a book, an audiobook or a video is
//! compressed already. Deduplication was measured on 3,185 copied entries: files with the
//! same contents were 0.4% of the bytes and fonts 0.5%, so this module does not do it.

use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

/// The end of the name of a compressed file.
pub const SUFFIX: &str = ".zst";

/// The zstd level. On the copy of jimaku.cc, level 9 makes `.ass` 8.7 and `.srt` 3.0 times
/// smaller at 25 MiB/s. Level 19 makes them 9.5 and 3.2 times smaller at 2 MiB/s.
const LEVEL: i32 = 9;

/// True for a file that is worth compressing: a text subtitle.
pub fn is_compressible(name: &str) -> bool {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    matches!(extension.as_deref(), Some("srt" | "ass" | "ssa" | "vtt"))
}

/// The name that the site shows for a file on the disk: `a.srt` for `a.srt.zst`.
/// `None` for a file that is not compressed.
pub fn shown_name(stored: &str) -> Option<&str> {
    stored.strip_suffix(SUFFIX).filter(|name| is_compressible(name))
}

/// True if the file on the disk is a compressed subtitle.
pub fn is_compressed(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(shown_name)
        .is_some()
}

fn compressed_path(folder: &Path, name: &str) -> PathBuf {
    folder.join(format!("{name}{SUFFIX}"))
}

/// The file on the disk that holds `name` in `folder`: `name` itself, else `name.zst`.
pub fn find(folder: &Path, name: &str) -> Option<PathBuf> {
    let plain = folder.join(name);
    if plain.is_file() {
        return Some(plain);
    }
    let compressed = compressed_path(folder, name);
    (is_compressible(name) && compressed.is_file()).then_some(compressed)
}

/// True if `folder` has a file `name`, compressed or not.
pub fn exists(folder: &Path, name: &str) -> bool {
    folder.join(name).exists() || (is_compressible(name) && compressed_path(folder, name).exists())
}

/// The size of the file before compression.
pub fn size(path: &Path) -> io::Result<u64> {
    if !is_compressed(path) {
        return Ok(fs::metadata(path)?.len());
    }
    // The frame header says the size. It is 18 bytes at most.
    let mut header = Vec::with_capacity(18);
    fs::File::open(path)?.take(18).read_to_end(&mut header)?;
    match zstd::zstd_safe::get_frame_content_size(&header) {
        Ok(Some(size)) => Ok(size),
        // A frame without its size: count the bytes.
        _ => io::copy(
            &mut zstd::stream::read::Decoder::new(fs::File::open(path)?)?,
            &mut io::sink(),
        ),
    }
}

/// The contents of the file, decompressed.
pub fn read(path: &Path) -> io::Result<Vec<u8>> {
    if is_compressed(path) {
        zstd::decode_all(fs::File::open(path)?)
    } else {
        fs::read(path)
    }
}

/// A reader of the contents of the file, decompressed.
pub fn open(path: &Path) -> io::Result<Box<dyn Read + Send>> {
    let file = fs::File::open(path)?;
    if is_compressed(path) {
        Ok(Box::new(zstd::stream::read::Decoder::new(file)?))
    } else {
        Ok(Box::new(file))
    }
}

/// The zstd frame of `data`, or `None` if zstd does not make it smaller. The frame is read
/// back before it is used: a fault then keeps the original.
fn compress(data: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let mut compressor = zstd::bulk::Compressor::new(LEVEL)?;
    compressor.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))?;
    let packed = compressor.compress(data)?;
    if packed.len() >= data.len() {
        return Ok(None);
    }
    if zstd::bulk::decompress(&packed, data.len())? != data {
        return Err(io::Error::other("the compressed file does not give back the original"));
    }
    Ok(Some(packed))
}

/// Writes a file into `folder` as `name`: a text subtitle compressed, else as it is. The
/// bytes go to a new file in `staging` (a folder on the same disk) first, and then move into
/// place, so a reader never sees half a file. The file gets the date `modified`. The other
/// form of the same name (plain or compressed) is removed. Returns the bytes on the disk.
pub fn put(folder: &Path, name: &str, data: &[u8], modified: SystemTime, staging: &Path) -> io::Result<u64> {
    let packed = if is_compressible(name) { compress(data)? } else { None };
    let (target, other, bytes) = match &packed {
        Some(packed) => (compressed_path(folder, name), folder.join(name), packed.as_slice()),
        None => (folder.join(name), compressed_path(folder, name), data),
    };
    fs::create_dir_all(staging)?;
    let part = staging.join(format!("{}.part", random_name()?));
    let written = (|| {
        let mut file = fs::File::create(&part)?;
        file.write_all(bytes)?;
        file.set_modified(modified)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&part, &target)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&part);
        return Err(e);
    }
    if is_compressible(name) && other.exists() {
        fs::remove_file(other)?;
    }
    Ok(bytes.len() as u64)
}

/// Compresses the plain text subtitle at `path` in place. Returns the bytes saved: 0 if the
/// file is not a text subtitle, or if zstd does not make it smaller.
pub fn compress_in_place(path: &Path, staging: &Path) -> io::Result<u64> {
    let (Some(folder), Some(name)) = (path.parent(), path.file_name().and_then(|name| name.to_str())) else {
        return Ok(0);
    };
    if !is_compressible(name) {
        return Ok(0);
    }
    let data = fs::read(path)?;
    let modified = fs::metadata(path)?.modified()?;
    let stored = put(folder, name, &data, modified, staging)?;
    Ok(data.len() as u64 - stored)
}

/// Compresses each plain text subtitle in `folder`. Returns how many files were compressed
/// and the bytes saved.
pub fn compress_folder(folder: &Path, staging: &Path) -> io::Result<(usize, u64)> {
    let mut files = 0;
    let mut saved = 0;
    for entry in fs::read_dir(folder)? {
        let path = entry?.path();
        let plain = path.is_file() && path.file_name().and_then(|n| n.to_str()).is_some_and(is_compressible);
        if plain {
            let bytes = match compress_in_place(&path, staging) {
                Ok(bytes) => bytes,
                // The copy or an editor moved the file away in the meantime.
                Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
                Err(e) => return Err(e),
            };
            if bytes > 0 {
                files += 1;
                saved += bytes;
            }
        }
    }
    Ok((files, saved))
}

/// Moves the file `from` in `from_folder` to `to` in `to_folder`. A compressed subtitle stays
/// compressed. If it gets a name that is not a text subtitle, it is decompressed.
pub fn rename(from_folder: &Path, from: &str, to_folder: &Path, to: &str) -> io::Result<()> {
    if exists(to_folder, to) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a file with this name is there",
        ));
    }
    let source = find(from_folder, from).ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
    if !is_compressed(&source) {
        return fs::rename(source, to_folder.join(to));
    }
    if is_compressible(to) {
        return fs::rename(source, compressed_path(to_folder, to));
    }
    let data = read(&source)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to_folder.join(to))?;
    file.write_all(&data)?;
    fs::remove_file(source)
}

/// The type of a decompressed download: the same one that a plain file gets.
pub fn content_type(name: &str) -> &'static str {
    match name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("srt") => "application/x-subrip",
        Some("vtt") => "text/vtt",
        _ => "application/octet-stream",
    }
}

fn random_name() -> io::Result<String> {
    let mut random = [0u8; 12];
    getrandom::getrandom(&mut random).map_err(io::Error::other)?;
    Ok(random.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("honjimaku-store-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(path.join("entry")).unwrap();
            Self(path)
        }
        fn entry(&self) -> PathBuf {
            self.0.join("entry")
        }
        fn staging(&self) -> PathBuf {
            self.0.join(".staging")
        }
        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(self.entry())
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Text like a subtitle: it repeats, so zstd makes it smaller.
    fn subtitle(lines: usize) -> Vec<u8> {
        (0..lines)
            .map(|i| {
                format!(
                    "{}\n00:00:{:02},000 --> 00:00:{:02},500\n吾輩は猫である。名前はまだ無い。\n\n",
                    i + 1,
                    i % 60,
                    i % 60
                )
            })
            .collect::<String>()
            .into_bytes()
    }

    #[test]
    fn only_text_subtitles_are_compressed() {
        for name in ["a.srt", "b.ASS", "c.ssa", "d.vtt", "x.y.srt"] {
            assert!(is_compressible(name), "{name}");
        }
        for name in ["a.7z", "b.zip", "c.sup", "d.epub", "e.m4b", "srt", "a.srt.zst", ""] {
            assert!(!is_compressible(name), "{name}");
        }
        assert_eq!(shown_name("ep 01.srt.zst"), Some("ep 01.srt"));
        assert_eq!(shown_name("pack.7z.zst"), None, "only a subtitle is ever compressed");
        assert_eq!(shown_name("ep 01.srt"), None);
        assert_eq!(content_type("a.SRT"), "application/x-subrip");
        assert_eq!(content_type("a.ass"), "application/octet-stream");
    }

    #[test]
    fn a_subtitle_goes_in_compressed_and_comes_out_the_same() {
        let folder = Folder::new("round-trip");
        let data = subtitle(500);
        let modified = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let stored = put(&folder.entry(), "ep 01.srt", &data, modified, &folder.staging()).unwrap();
        assert!(
            stored * 3 < data.len() as u64,
            "{stored} bytes on the disk for {}",
            data.len()
        );
        assert_eq!(folder.names(), ["ep 01.srt.zst"]);
        let path = find(&folder.entry(), "ep 01.srt").unwrap();
        assert!(is_compressed(&path));
        assert_eq!(size(&path).unwrap(), data.len() as u64);
        assert_eq!(read(&path).unwrap(), data);
        let mut streamed = Vec::new();
        open(&path).unwrap().read_to_end(&mut streamed).unwrap();
        assert_eq!(streamed, data);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        // The zstd command reads the file too: one standard frame.
        assert_eq!(zstd::decode_all(fs::File::open(&path).unwrap()).unwrap(), data);
        assert_eq!(
            fs::read_dir(folder.staging()).unwrap().count(),
            0,
            "nothing is left in staging"
        );
    }

    #[test]
    fn other_files_and_small_files_stay_as_they_are() {
        let folder = Folder::new("plain");
        let epoch = SystemTime::UNIX_EPOCH;
        put(&folder.entry(), "pack.7z", b"7z bytes", epoch, &folder.staging()).unwrap();
        put(&folder.entry(), "tiny.srt", b"1", epoch, &folder.staging()).unwrap();
        assert_eq!(folder.names(), ["pack.7z", "tiny.srt"]);
        assert_eq!(read(&find(&folder.entry(), "tiny.srt").unwrap()).unwrap(), b"1");
        assert_eq!(size(&find(&folder.entry(), "pack.7z").unwrap()).unwrap(), 8);
    }

    #[test]
    fn a_folder_is_compressed_in_place_and_again_changes_nothing() {
        let folder = Folder::new("in-place");
        let data = subtitle(300);
        fs::write(folder.entry().join("a.srt"), &data).unwrap();
        fs::write(folder.entry().join("b.ass"), subtitle(200)).unwrap();
        fs::write(folder.entry().join("c.7z"), b"archive").unwrap();
        let (files, saved) = compress_folder(&folder.entry(), &folder.staging()).unwrap();
        assert_eq!(files, 2);
        assert!(saved > data.len() as u64 / 2, "{saved}");
        assert_eq!(folder.names(), ["a.srt.zst", "b.ass.zst", "c.7z"]);
        assert_eq!(read(&find(&folder.entry(), "a.srt").unwrap()).unwrap(), data);
        assert_eq!(compress_folder(&folder.entry(), &folder.staging()).unwrap(), (0, 0));
    }

    #[test]
    fn a_new_copy_replaces_the_other_form() {
        let folder = Folder::new("replace");
        let epoch = SystemTime::UNIX_EPOCH;
        fs::write(folder.entry().join("a.srt"), b"old plain").unwrap();
        put(&folder.entry(), "a.srt", &subtitle(100), epoch, &folder.staging()).unwrap();
        assert_eq!(folder.names(), ["a.srt.zst"]);
        put(&folder.entry(), "a.srt", b"2", epoch, &folder.staging()).unwrap();
        assert_eq!(
            folder.names(),
            ["a.srt"],
            "too small to compress: the plain file replaces the old frame"
        );
    }

    #[test]
    fn a_file_is_found_and_renamed_in_either_form() {
        let folder = Folder::new("rename");
        let entry = folder.entry();
        let data = subtitle(100);
        put(&entry, "a.srt", &data, SystemTime::UNIX_EPOCH, &folder.staging()).unwrap();
        fs::write(entry.join("b.7z"), b"archive").unwrap();
        assert!(exists(&entry, "a.srt") && exists(&entry, "b.7z") && !exists(&entry, "c.srt"));

        rename(&entry, "a.srt", &entry, "a2.srt").unwrap();
        assert_eq!(folder.names(), ["a2.srt.zst", "b.7z"]);
        let error = rename(&entry, "a2.srt", &entry, "b.7z").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            rename(&entry, "none.srt", &entry, "x.srt").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        // A subtitle that gets a name of another kind is decompressed.
        rename(&entry, "a2.srt", &entry, "a2.txt").unwrap();
        assert_eq!(folder.names(), ["a2.txt", "b.7z"]);
        assert_eq!(fs::read(entry.join("a2.txt")).unwrap(), data);

        let other = folder.0.join("other");
        fs::create_dir_all(&other).unwrap();
        put(&entry, "c.srt", &data, SystemTime::UNIX_EPOCH, &folder.staging()).unwrap();
        rename(&entry, "c.srt", &other, "c.srt").unwrap();
        assert!(other.join("c.srt.zst").is_file());
        assert_eq!(read(&find(&other, "c.srt").unwrap()).unwrap(), data);
    }

    #[test]
    fn a_damaged_file_is_an_error_not_wrong_contents() {
        let folder = Folder::new("damaged");
        let data = subtitle(300);
        put(
            &folder.entry(),
            "a.srt",
            &data,
            SystemTime::UNIX_EPOCH,
            &folder.staging(),
        )
        .unwrap();
        let path = find(&folder.entry(), "a.srt").unwrap();
        let mut bytes = fs::read(&path).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xff;
        fs::write(&path, bytes).unwrap();
        assert!(read(&path).is_err(), "the checksum finds the fault");
    }
}

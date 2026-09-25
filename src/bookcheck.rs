//! The check that an uploaded book, audiobook or video is what its name says.
//!
//! These files are not subtitles. An entry keeps them beside the subtitles so that a person can
//! review the subtitles against them: the book (`.epub`, `.pdf`), the audiobook (`.m4b`,
//! `.opus`) or the video (`.mp4`, `.mkv`). They pass the tests of subtitles where they apply:
//! the text of an EPUB must be in the language of the entry, and an audiobook or a video must
//! be as long as a whole recording. A PDF is only checked to be a PDF, because its text cannot
//! be read without a large library.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use lofty::config::ParseOptions;
use lofty::file::AudioFile;

use crate::subcheck::{self, Script};

/// A book with its pictures can be large. A book of text alone is a few MB.
pub const MAX_BOOK_BYTES: u64 = 200 * 1024 * 1024;
/// 40 hours at 64 kbit/s is 1.1 GB.
pub const MAX_AUDIO_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Two hours at 2 Mbit/s is 1.8 GB.
pub const MAX_VIDEO_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// The largest file that `check` accepts.
pub const MAX_FILE_BYTES: u64 = if MAX_AUDIO_BYTES > MAX_VIDEO_BYTES {
    MAX_AUDIO_BYTES
} else {
    MAX_VIDEO_BYTES
};
/// The language test reads this much text at most. More text does not change the result.
const MAX_TEXT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MEMBERS: usize = 20_000;
/// `%%EOF` is in the last bytes of a PDF. Some tools write a few lines after it.
const PDF_TAIL_BYTES: u64 = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Epub,
    Pdf,
    M4b,
    Opus,
    Mp4,
    Mkv,
}

impl Kind {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "epub" => Some(Self::Epub),
            "pdf" => Some(Self::Pdf),
            "m4b" => Some(Self::M4b),
            "opus" => Some(Self::Opus),
            "mp4" => Some(Self::Mp4),
            "mkv" => Some(Self::Mkv),
            _ => None,
        }
    }

    pub fn max_bytes(self) -> u64 {
        match self {
            Self::Epub | Self::Pdf => MAX_BOOK_BYTES,
            Self::M4b | Self::Opus => MAX_AUDIO_BYTES,
            Self::Mp4 | Self::Mkv => MAX_VIDEO_BYTES,
        }
    }
}

/// Checks the file at `path`. It reads the file from the disk, because an audiobook or a
/// video is too large to keep in memory.
pub fn check(path: &Path, kind: Kind, script: Script) -> anyhow::Result<()> {
    let file = std::fs::File::open(path)?;
    match kind {
        Kind::Epub => check_epub(file, script),
        Kind::Pdf => check_pdf(file),
        Kind::M4b | Kind::Opus | Kind::Mp4 | Kind::Mkv => check_recording(file, kind),
    }
}

fn check_epub<R: Read + Seek>(reader: R, script: Script) -> anyhow::Result<()> {
    let Ok(mut archive) = zip::ZipArchive::new(reader) else {
        bail!("the file is not an EPUB (it is not a zip)");
    };
    if archive.len() > MAX_MEMBERS {
        bail!("the EPUB holds more than {MAX_MEMBERS} files");
    }
    let mut mimetype = String::new();
    match archive.by_name("mimetype") {
        Ok(member) => {
            member.take(64).read_to_string(&mut mimetype)?;
        }
        Err(_) => bail!("the file is not an EPUB (it has no mimetype file)"),
    }
    if mimetype.trim() != "application/epub+zip" {
        bail!("the file is not an EPUB (its mimetype is not application/epub+zip)");
    }
    if archive.by_name("META-INF/container.xml").is_err() {
        bail!("the EPUB has no META-INF/container.xml");
    }

    let mut text = String::new();
    let mut budget = MAX_TEXT_BYTES;
    for index in 0..archive.len() {
        if budget == 0 {
            break;
        }
        let member = archive.by_index(index)?;
        let name = member.name().to_ascii_lowercase();
        if !(name.ends_with(".xhtml") || name.ends_with(".html") || name.ends_with(".htm")) {
            continue;
        }
        // Read at most the budget, so that a zip bomb cannot fill the memory.
        let mut page = Vec::new();
        member
            .take(budget)
            .read_to_end(&mut page)
            .with_context(|| format!("cannot read {name} in the EPUB"))?;
        budget -= page.len() as u64;
        text.push_str(&body_text(&String::from_utf8_lossy(&page)));
    }
    if let Some(problem) = subcheck::wrong_script(text.chars(), script, "book") {
        bail!("{problem}");
    }
    if text.chars().all(char::is_whitespace) {
        bail!("there are no words in the book");
    }
    Ok(())
}

/// The text in the `<body>` of a page. The `<head>` holds a title and styles, not the book.
fn body_text(page: &str) -> String {
    let body = match page.find("<body") {
        Some(start) => &page[start..],
        None => page,
    };
    let mut out = String::with_capacity(body.len() / 2);
    let mut in_tag = false;
    let mut chars = body.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            // An entity such as `&nbsp;` is one character, not a word of Latin letters.
            '&' if !in_tag => {
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next == ';' || next.is_whitespace() {
                        break;
                    }
                }
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

/// A PDF starts with `%PDF-` and ends with `%%EOF`. What is between them is for the
/// person who reviews the subtitles.
fn check_pdf<R: Read + Seek>(mut file: R) -> anyhow::Result<()> {
    let mut head = Vec::new();
    (&mut file).take(8).read_to_end(&mut head)?;
    if !head.starts_with(b"%PDF-") {
        bail!("the file is not a PDF (it does not start with %PDF-)");
    }
    let length = file.seek(SeekFrom::End(0))?;
    let tail_length = length.min(PDF_TAIL_BYTES);
    file.seek(SeekFrom::End(-(tail_length as i64)))?;
    let mut tail = Vec::new();
    file.take(tail_length).read_to_end(&mut tail)?;
    if !tail.windows(5).any(|bytes| bytes == b"%%EOF") {
        bail!("the file is not a whole PDF (it does not end with %%EOF)");
    }
    Ok(())
}

/// The length of a recording and the number of its audio channels, as its container says them.
fn recording(file: &mut std::fs::File, kind: Kind) -> anyhow::Result<(Duration, u64)> {
    let options = ParseOptions::new().read_tags(false).read_cover_art(false);
    match kind {
        Kind::M4b => match lofty::mp4::Mp4File::read_from(file, options) {
            Ok(mp4) => Ok((mp4.properties().duration(), channels(mp4.properties().channels()))),
            Err(_) => bail!("the file is not an M4B audiobook (an MP4 file with audio)"),
        },
        Kind::Opus => match lofty::ogg::OpusFile::read_from(file, options) {
            Ok(opus) => Ok((opus.properties().duration(), u64::from(opus.properties().channels()))),
            Err(_) => bail!("the file is not Opus audio in an Ogg file"),
        },
        // The properties of an MP4 are those of its first audio track.
        Kind::Mp4 => match lofty::mp4::Mp4File::read_from(file, options) {
            Ok(mp4) => Ok((mp4.properties().duration(), channels(mp4.properties().channels()))),
            Err(_) => bail!("the file is not an MP4 video with audio"),
        },
        Kind::Mkv => {
            // A seek table that points to the wrong place makes the reader panic. Such a file
            // is refused, and the server goes on.
            let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| matroska::Matroska::open(file)));
            let Ok(Ok(mkv)) = opened else {
                bail!("the file is not a Matroska (MKV) video");
            };
            let channels = mkv
                .audio_tracks()
                .map(|track| match &track.settings {
                    matroska::Settings::Audio(audio) => audio.channels,
                    _ => 0,
                })
                .max()
                .unwrap_or(0);
            Ok((mkv.info.duration.unwrap_or_default(), channels))
        }
        Kind::Epub | Kind::Pdf => unreachable!("a book is not a recording"),
    }
}

fn channels(count: Option<u8>) -> u64 {
    u64::from(count.unwrap_or(0))
}

fn check_recording(mut file: std::fs::File, kind: Kind) -> anyhow::Result<()> {
    let (duration, channels) = recording(&mut file, kind)?;
    let video = matches!(kind, Kind::Mp4 | Kind::Mkv);
    if channels == 0 {
        bail!(if video {
            "the video has no audio"
        } else {
            "the file holds no audio"
        });
    }
    let seconds = duration.as_secs_f64();
    if seconds < subcheck::MIN_DURATION_SECONDS {
        let (what, whole) = if video {
            ("video", "recording")
        } else {
            ("audio", "audiobook")
        };
        bail!(
            "the {what} is {:.0} minutes long. Is this the whole {whole}?",
            seconds / 60.0
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    fn epub(mimetype: &str, pages: &[&str]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("mimetype", stored).unwrap();
        zip.write_all(mimetype.as_bytes()).unwrap();
        zip.start_file("META-INF/container.xml", stored).unwrap();
        zip.write_all(b"<container/>").unwrap();
        for (i, page) in pages.iter().enumerate() {
            zip.start_file(format!("OEBPS/p{i}.xhtml"), zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(page.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn page(text: &str) -> String {
        format!(
            "<html><head><title>Cat</title><style>p {{ margin: 0 }}</style></head><body><p>{text}</p></body></html>"
        )
    }

    fn why(result: anyhow::Result<()>) -> String {
        result.expect_err("the file must be refused").to_string()
    }

    #[test]
    fn a_japanese_epub_passes() {
        let book = epub(
            "application/epub+zip",
            &[&page(
                "吾輩は&nbsp;猫である。<ruby>名前<rt>なまえ</rt></ruby>はまだ無い。",
            )],
        );
        check_epub(Cursor::new(book), Script::Japanese).unwrap();
    }

    #[test]
    fn an_epub_in_the_wrong_language_is_refused() {
        let book = epub("application/epub+zip", &[&page("It was a dark and stormy night.")]);
        assert!(why(check_epub(Cursor::new(book.clone()), Script::Japanese)).contains("not Japanese"));
        check_epub(Cursor::new(book), Script::Any).unwrap();
        let chinese = epub("application/epub+zip", &[&page("我是猫，还没有名字。")]);
        check_epub(Cursor::new(chinese), Script::Chinese).unwrap();
    }

    #[test]
    fn what_is_not_an_epub_is_refused() {
        assert!(why(check_epub(Cursor::new(b"<html></html>".to_vec()), Script::Any)).contains("not a zip"));
        let odt = epub("application/vnd.oasis.opendocument.text", &[&page("吾輩は猫である。")]);
        assert!(why(check_epub(Cursor::new(odt), Script::Any)).contains("mimetype"));
        let empty = epub("application/epub+zip", &[&page("")]);
        assert!(why(check_epub(Cursor::new(empty), Script::Any)).contains("no words"));
    }

    #[test]
    fn the_head_of_a_page_is_not_its_text() {
        assert_eq!(body_text(&page("猫")).trim(), "猫");
        assert_eq!(
            body_text("<body>a&amp;b</body>").split_whitespace().collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    /// The smallest PDF: one empty page. The bytes between the header and `%%EOF` are not read.
    fn pdf(header: &[u8], footer: &[u8]) -> Vec<u8> {
        let mut bytes = header.to_vec();
        bytes.extend_from_slice(b"\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        bytes.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
        bytes.extend_from_slice(b"trailer\n<< /Root 1 0 R >>\n");
        bytes.extend_from_slice(footer);
        bytes
    }

    #[test]
    fn a_pdf_passes_and_what_is_not_a_pdf_is_refused() {
        check_pdf(Cursor::new(pdf(b"%PDF-1.4", b"%%EOF\n"))).unwrap();
        // A PDF that a tool wrote a note after, and one with a Windows line end.
        check_pdf(Cursor::new(pdf(b"%PDF-2.0", b"%%EOF\r\n% made by a tool\r\n"))).unwrap();
        assert!(why(check_pdf(Cursor::new(pdf(b"%!PS-Adobe-3.0", b"%%EOF")))).contains("not a PDF"));
        assert!(why(check_pdf(Cursor::new("吾輩は猫である。".repeat(100).into_bytes()))).contains("not a PDF"));
        assert!(why(check_pdf(Cursor::new(b"%PDF".to_vec()))).contains("not a PDF"));
        assert!(why(check_pdf(Cursor::new(pdf(b"%PDF-1.7", b"")))).contains("not a whole PDF"));
        // `%%EOF` is looked for in the last bytes only, so a PDF with much after it is refused.
        let mut cut = pdf(b"%PDF-1.7", b"%%EOF\n");
        cut.extend_from_slice(&vec![b' '; PDF_TAIL_BYTES as usize]);
        assert!(why(check_pdf(Cursor::new(cut))).contains("not a whole PDF"));
    }

    #[test]
    fn what_is_not_a_recording_is_refused() {
        let dir = std::env::temp_dir().join(format!("bookcheck-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fake.m4b");
        std::fs::write(&path, "吾輩は猫である。".repeat(1000)).unwrap();
        assert!(why(check(&path, Kind::M4b, Script::Any)).contains("not an M4B"));
        assert!(why(check(&path, Kind::Opus, Script::Any)).contains("not Opus"));
        assert!(why(check(&path, Kind::Mp4, Script::Any)).contains("not an MP4"));
        assert!(why(check(&path, Kind::Mkv, Script::Any)).contains("not a Matroska"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The fixtures are silence and black frames that ffmpeg made: 1 second, and 10 minutes
    /// and 1 second.
    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    #[test]
    fn whole_audiobooks_pass() {
        check(&fixture("silence-10m.m4b"), Kind::M4b, Script::Japanese).unwrap();
        check(&fixture("silence-10m.opus"), Kind::Opus, Script::Japanese).unwrap();
    }

    #[test]
    fn whole_videos_with_audio_pass() {
        check(&fixture("video-10m.mp4"), Kind::Mp4, Script::Japanese).unwrap();
        check(&fixture("video-10m.mkv"), Kind::Mkv, Script::Japanese).unwrap();
    }

    #[test]
    fn a_short_recording_or_the_wrong_container_is_refused() {
        assert!(why(check(&fixture("silence-1s.opus"), Kind::Opus, Script::Any)).contains("0 minutes long"));
        assert!(why(check(&fixture("video-1s.mkv"), Kind::Mkv, Script::Any)).contains("0 minutes long"));
        assert!(why(check(&fixture("silence-10m.opus"), Kind::M4b, Script::Any)).contains("not an M4B"));
        assert!(why(check(&fixture("silence-10m.m4b"), Kind::Opus, Script::Any)).contains("not Opus"));
        assert!(why(check(&fixture("video-10m.mkv"), Kind::Mp4, Script::Any)).contains("not an MP4"));
        assert!(why(check(&fixture("video-10m.mp4"), Kind::Mkv, Script::Any)).contains("not a Matroska"));
    }

    #[test]
    fn a_video_without_audio_is_refused() {
        assert!(why(check(&fixture("video-mute-10m.mp4"), Kind::Mp4, Script::Any)).contains("audio"));
    }
}

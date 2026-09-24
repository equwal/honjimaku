//! The check that an uploaded book (`.epub`) or audiobook (`.m4b`, `.opus`) is what its name says.
//!
//! A book or an audiobook must pass the same tests as subtitles where they apply: the text of a
//! book must be in the language of the entry, and an audiobook must be as long as a whole
//! recording. The upload route also asks for subtitles that pass `subcheck` in the same entry.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, bail};
use lofty::config::ParseOptions;
use lofty::file::AudioFile;

use crate::subcheck::{self, Script};

/// A book with its pictures can be large. A book of text alone is a few MB.
pub const MAX_BOOK_BYTES: u64 = 200 * 1024 * 1024;
/// 40 hours at 64 kbit/s is 1.1 GB.
pub const MAX_AUDIO_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// The language test reads this much text at most. More text does not change the result.
const MAX_TEXT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MEMBERS: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Epub,
    M4b,
    Opus,
}

impl Kind {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "epub" => Some(Self::Epub),
            "m4b" => Some(Self::M4b),
            "opus" => Some(Self::Opus),
            _ => None,
        }
    }

    pub fn max_bytes(self) -> u64 {
        match self {
            Self::Epub => MAX_BOOK_BYTES,
            Self::M4b | Self::Opus => MAX_AUDIO_BYTES,
        }
    }
}

/// Checks the file at `path`. It reads the file from the disk, because an audiobook is too
/// large to keep in memory.
pub fn check(path: &Path, kind: Kind, script: Script) -> anyhow::Result<()> {
    let file = std::fs::File::open(path)?;
    match kind {
        Kind::Epub => check_epub(file, script),
        Kind::M4b | Kind::Opus => check_audio(file, kind),
    }
}

fn check_epub<R: Read + std::io::Seek>(reader: R, script: Script) -> anyhow::Result<()> {
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

fn check_audio(mut file: std::fs::File, kind: Kind) -> anyhow::Result<()> {
    let options = ParseOptions::new().read_tags(false).read_cover_art(false);
    let (duration, channels) = match kind {
        Kind::M4b => match lofty::mp4::Mp4File::read_from(&mut file, options) {
            Ok(mp4) => (mp4.properties().duration(), mp4.properties().channels().unwrap_or(0)),
            Err(_) => bail!("the file is not an M4B audiobook (an MP4 file with audio)"),
        },
        Kind::Opus => match lofty::ogg::OpusFile::read_from(&mut file, options) {
            Ok(opus) => (opus.properties().duration(), opus.properties().channels()),
            Err(_) => bail!("the file is not Opus audio in an Ogg file"),
        },
        Kind::Epub => unreachable!("an EPUB is not audio"),
    };
    if channels == 0 {
        bail!("the file holds no audio");
    }
    let seconds = duration.as_secs_f64();
    if seconds < subcheck::MIN_DURATION_SECONDS {
        bail!(
            "the audio is {:.0} minutes long. Is this the whole audiobook?",
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

    #[test]
    fn what_is_not_audio_is_refused() {
        let dir = std::env::temp_dir().join(format!("bookcheck-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fake.m4b");
        std::fs::write(&path, "吾輩は猫である。".repeat(1000)).unwrap();
        assert!(why(check(&path, Kind::M4b, Script::Any)).contains("not an M4B"));
        assert!(why(check(&path, Kind::Opus, Script::Any)).contains("not Opus"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The fixtures are silence that ffmpeg made: 1 second, and 10 minutes and 1 second.
    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    #[test]
    fn whole_audiobooks_pass() {
        check(&fixture("silence-10m.m4b"), Kind::M4b, Script::Japanese).unwrap();
        check(&fixture("silence-10m.opus"), Kind::Opus, Script::Japanese).unwrap();
    }

    #[test]
    fn a_short_recording_or_the_wrong_container_is_refused() {
        assert!(why(check(&fixture("silence-1s.opus"), Kind::Opus, Script::Any)).contains("0 minutes long"));
        assert!(why(check(&fixture("silence-10m.opus"), Kind::M4b, Script::Any)).contains("not an M4B"));
        assert!(why(check(&fixture("silence-10m.m4b"), Kind::Opus, Script::Any)).contains("not Opus"));
    }
}

//! The check that an uploaded file is a set of subtitles for a whole recording: an audiobook, an episode.
//!
//! An upload used to pass when its file name ended in `.srt`. Anything could
//! be behind that name: an empty file, a web page, subtitles of a 24-minute
//! episode, text in the wrong encoding. This module reads the file and says
//! what is wrong with it in words the uploader can act on.
//!
//! It has no dependency on the rest of the server, so it is tested alone.

use std::fmt;

/// A book or an episode is long. A file shorter than this is a sample, or something else.
const MIN_DURATION_SECONDS: f64 = 10.0 * 60.0;
const MIN_CUES: usize = 30;
/// A book of 40 hours is about 6 MB of subtitles.
pub const MAX_BYTES: usize = 25 * 1024 * 1024;
const MAX_CUE_CHARS: usize = 2_000;
/// Lines longer than that which a file may have.
const MAX_PAGES: usize = 3;
/// Blocks that cannot be read, or cues that end before they start.
const MAX_BAD_SHARE: f64 = 0.02;
/// Cues that start before the cue ahead of them.
const MAX_OUT_OF_ORDER_SHARE: f64 = 0.02;
/// Speech is about 5 to 12 characters a second. Far outside that, the times
/// and the text do not belong together.
const MIN_CHARS_PER_SECOND: f64 = 0.5;
const MAX_CHARS_PER_SECOND: f64 = 40.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Srt,
    Vtt,
    Ass,
}

impl Format {
    /// The format for a file extension, or `None` for a file this module cannot read
    /// (an archive, image-based subtitles).
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "srt" => Some(Self::Srt),
            "vtt" => Some(Self::Vtt),
            "ass" | "ssa" => Some(Self::Ass),
            _ => None,
        }
    }
}

/// The language of the entry. The text of an upload must be mostly in its script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Script {
    Japanese,
    Chinese,
    Any,
}

impl Script {
    pub fn from_code(code: &str) -> Self {
        match code {
            "ja" => Self::Japanese,
            "zh" => Self::Chinese,
            _ => Self::Any,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// What a good file holds. Shown to the uploader and kept in the audit log.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub cues: usize,
    pub duration_seconds: f64,
    /// Lines that an aligner marked as not found in the book (they start with `＊`).
    pub unmatched_share: f64,
    /// Lines that hold a page of text: what an aligner found no place for.
    pub pages: usize,
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let minutes = (self.duration_seconds / 60.0).round() as u64;
        write!(f, "{} lines over {}h{:02}m", self.cues, minutes / 60, minutes % 60)?;
        if self.unmatched_share >= 0.005 {
            write!(f, ", {:.0}% not matched to the book", self.unmatched_share * 100.0)?;
        }
        if self.pages > 0 {
            write!(
                f,
                ", {} overlong line{}",
                self.pages,
                if self.pages == 1 { "" } else { "s" }
            )?;
        }
        Ok(())
    }
}

/// Why a file is refused. One value holds each problem found, not only the first.
#[derive(Debug, Clone, PartialEq)]
pub struct Refused(pub Vec<String>);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.join(" "))
    }
}

impl std::error::Error for Refused {}

/// Checks the bytes of one subtitle file.
pub fn check(bytes: &[u8], format: Format, script: Script) -> Result<Summary, Refused> {
    let refuse = |why: &str| Err(Refused(vec![why.to_owned()]));
    if bytes.is_empty() {
        return refuse("The file is empty.");
    }
    if bytes.len() > MAX_BYTES {
        return refuse("The file is larger than 25 MB, which is more than any subtitles.");
    }
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return refuse("The file is UTF-16. Save it as UTF-8 and upload it again.");
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return refuse("The file is not UTF-8 text (Shift_JIS, perhaps). Save it as UTF-8 and upload it again.");
    };
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    if text.chars().any(|c| c == '\0') {
        return refuse("The file is not text.");
    }

    let (cues, bad_blocks) = match format {
        Format::Srt | Format::Vtt => parse_srt(text),
        Format::Ass => parse_ass(text),
    };
    if cues.is_empty() {
        return refuse("No subtitle lines with times could be read from the file.");
    }

    let mut problems = Vec::new();
    let total = cues.len() + bad_blocks;
    let backwards = cues.iter().filter(|c| c.end <= c.start).count();
    if (bad_blocks + backwards) as f64 > MAX_BAD_SHARE * total as f64 {
        problems.push(format!(
            "{} of {} blocks cannot be read, or end before they start.",
            bad_blocks + backwards,
            total
        ));
    }
    if cues.len() < MIN_CUES {
        problems.push(format!(
            "The file has {} lines. A whole book or episode has many more.",
            cues.len()
        ));
    }
    let out_of_order = cues.windows(2).filter(|w| w[1].start < w[0].start).count();
    if out_of_order as f64 > MAX_OUT_OF_ORDER_SHARE * cues.len() as f64 {
        problems.push(format!("{out_of_order} lines start before the line ahead of them."));
    }
    let duration = cues.iter().map(|c| c.end).fold(0.0, f64::max);
    if duration < MIN_DURATION_SECONDS {
        problems.push(format!(
            "The subtitles end at {:.0} minutes. Is this the whole recording?",
            duration / 60.0
        ));
    }
    // An aligner puts the text it found no place for (an afterword, the colophon) into one
    // line, most often the last. One in 12 of the good files on the site has such a line, so
    // a few are let through and named in the summary. Many of them are a different fault.
    let is_page = |c: &&Cue| c.text.chars().count() > MAX_CUE_CHARS;
    let pages = cues.iter().filter(is_page).count();
    if pages > MAX_PAGES {
        let first = cues.iter().find(is_page).map(|c| stamp(c.start)).unwrap_or_default();
        problems.push(format!(
            "{pages} lines hold more than {MAX_CUE_CHARS} characters each (the first at {first}). Those are pages, not subtitles."
        ));
    }

    let lines = || cues.iter().filter(|c| c.text.chars().count() <= MAX_CUE_CHARS);
    let characters: usize = lines()
        .map(|c| c.text.chars().filter(|ch| !ch.is_whitespace()).count())
        .sum();
    let spoken: f64 = lines().map(|c| (c.end - c.start).max(0.0)).sum();
    if spoken > 0.0 {
        let rate = characters as f64 / spoken;
        if !(MIN_CHARS_PER_SECOND..=MAX_CHARS_PER_SECOND).contains(&rate) {
            problems.push(format!(
                "The text has {rate:.1} characters for each second of speech. The times and the text do not belong together."
            ));
        }
    }
    if let Some(problem) = wrong_script(&cues, script) {
        problems.push(problem);
    }

    if !problems.is_empty() {
        return Err(Refused(problems));
    }
    let unmatched = cues.iter().filter(|c| c.text.starts_with('＊')).count();
    Ok(Summary {
        cues: cues.len(),
        duration_seconds: duration,
        unmatched_share: unmatched as f64 / cues.len() as f64,
        pages,
    })
}

fn wrong_script(cues: &[Cue], script: Script) -> Option<String> {
    if script == Script::Any {
        return None;
    }
    let (mut letters, mut kana, mut han) = (0usize, 0usize, 0usize);
    for ch in cues.iter().flat_map(|c| c.text.chars()).filter(|ch| ch.is_alphabetic()) {
        letters += 1;
        match ch as u32 {
            0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F => kana += 1,
            0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FA1F => han += 1,
            _ => {}
        }
    }
    if letters == 0 {
        return Some("The subtitles hold no words.".to_owned());
    }
    let share = |n: usize| n as f64 / letters as f64;
    match script {
        // Japanese text always has kana. Chinese text has none.
        Script::Japanese if share(kana) < 0.20 || share(kana + han) < 0.60 => {
            Some("The text is not Japanese. This site is for Japanese subtitles.".to_owned())
        }
        Script::Chinese if share(han) < 0.60 || share(kana) > 0.05 => {
            Some("The text is not Chinese. This site is for Chinese subtitles.".to_owned())
        }
        _ => None,
    }
}

fn stamp(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// `1:02:03,456`, `02:03.4`, `1:02:03.45`: hours are optional, and so are digits of the fraction.
fn seconds(stamp: &str) -> Option<f64> {
    let stamp = stamp.trim();
    let (clock, fraction) = match stamp.split_once([',', '.']) {
        Some((clock, fraction)) => (clock, fraction),
        None => (stamp, "0"),
    };
    if fraction.is_empty() || fraction.len() > 6 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let parts: Vec<&str> = clock.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let mut total = 0.0;
    for part in &parts {
        if part.is_empty() || part.len() > 4 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        total = total * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(total + format!("0.{fraction}").parse::<f64>().ok()?)
}

/// Cues, and the count of blocks that look like cues but cannot be read. Also reads WebVTT.
fn parse_srt(text: &str) -> (Vec<Cue>, usize) {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut cues = Vec::new();
    let mut bad = 0;
    for block in text.split("\n\n") {
        let lines: Vec<&str> = block.lines().collect();
        let Some(at) = lines.iter().position(|l| l.contains("-->")) else {
            // A header, a note, or stray text. A stray number or word is not a broken cue.
            continue;
        };
        let Some((from, to)) = lines[at].split_once("-->") else {
            continue;
        };
        // WebVTT can put position settings after the end time.
        let to = to.trim().split_whitespace().next().unwrap_or("");
        let body = lines[at + 1..].join("\n");
        let body = strip_tags(&body);
        match (seconds(from), seconds(to)) {
            (Some(start), Some(end)) if !body.trim().is_empty() => cues.push(Cue {
                start,
                end,
                text: body.trim().to_owned(),
            }),
            _ => bad += 1,
        }
    }
    (cues, bad)
}

fn parse_ass(text: &str) -> (Vec<Cue>, usize) {
    let mut cues = Vec::new();
    let mut bad = 0;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("Dialogue:") else {
            continue;
        };
        // Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
        let fields: Vec<&str> = rest.splitn(10, ',').collect();
        if fields.len() < 10 {
            bad += 1;
            continue;
        }
        let body = strip_braces(fields[9]).replace("\\N", "\n").replace("\\n", "\n");
        match (seconds(fields[1]), seconds(fields[2])) {
            (Some(start), Some(end)) if !body.trim().is_empty() => cues.push(Cue {
                start,
                end,
                text: body.trim().to_owned(),
            }),
            _ => bad += 1,
        }
    }
    (cues, bad)
}

fn strip_tags(s: &str) -> String {
    strip_between(s, '<', '>')
}

fn strip_braces(s: &str) -> String {
    strip_between(s, '{', '}')
}

fn strip_between(s: &str, open: char, close: char) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for ch in s.chars() {
        if ch == open {
            depth += 1;
        } else if ch == close && depth > 0 {
            depth -= 1;
        } else if depth == 0 {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srt_stamp(t: f64) -> String {
        let ms = (t * 1000.0).round() as u64;
        format!(
            "{:02}:{:02}:{:02},{:03}",
            ms / 3_600_000,
            ms / 60_000 % 60,
            ms / 1000 % 60,
            ms % 1000
        )
    }

    /// A book: `n` lines of `line`, four seconds each.
    fn book(n: usize, line: &str) -> String {
        (0..n)
            .map(|i| {
                let t = i as f64 * 4.0;
                format!("{}\n{} --> {}\n{}\n", i + 1, srt_stamp(t), srt_stamp(t + 3.5), line)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn reasons(result: Result<Summary, Refused>) -> String {
        result.expect_err("the file must be refused").to_string()
    }

    #[test]
    fn a_japanese_audiobook_passes_and_is_summed_up() {
        let mut text = book(400, "吾輩は猫である。名前はまだ無い。");
        text.push_str(&format!(
            "\n401\n{} --> {}\n＊ごせいちょうありがとうございました\n",
            srt_stamp(1600.0),
            srt_stamp(1603.0)
        ));
        let summary = check(text.as_bytes(), Format::Srt, Script::Japanese).unwrap();
        assert_eq!(summary.cues, 401);
        assert!((summary.duration_seconds - 1603.0).abs() < 0.01);
        assert!((summary.unmatched_share - 1.0 / 401.0).abs() < 1e-9);
        assert_eq!(summary.to_string(), "401 lines over 0h27m");
    }

    #[test]
    fn a_byte_order_mark_and_windows_line_ends_are_fine() {
        let text = format!("\u{FEFF}{}", book(300, "そうだ、京都へ行こう。").replace('\n', "\r\n"));
        assert!(check(text.as_bytes(), Format::Srt, Script::Japanese).is_ok());
    }

    #[test]
    fn what_is_not_subtitles_is_refused_with_the_reason() {
        assert!(reasons(check(b"", Format::Srt, Script::Any)).contains("empty"));
        assert!(reasons(check(
            b"<!doctype html><html><body>404</body></html>",
            Format::Srt,
            Script::Any
        ))
        .contains("No subtitle lines"));
        assert!(reasons(check(&[0xFF, 0xFE, b'1', 0], Format::Srt, Script::Any)).contains("UTF-16"));
        // Shift_JIS bytes of 吾輩
        assert!(reasons(check(&[0x8C, 0xE1, 0x94, 0x79, b'\n'], Format::Srt, Script::Any)).contains("not UTF-8"));
        assert!(reasons(check(
            b"1\n00:00:01,000 --> 00:00:02,000\nab\0cd\n",
            Format::Srt,
            Script::Any
        ))
        .contains("not text"));
    }

    #[test]
    fn an_episode_is_not_an_audiobook() {
        // 24 lines, 96 seconds: both too few and too short, and both are said.
        let why = reasons(check(
            book(24, "こんにちは、世界。").as_bytes(),
            Format::Srt,
            Script::Japanese,
        ));
        assert!(why.contains("24 lines"), "{why}");
        assert!(why.contains("2 minutes"), "{why}");
    }

    #[test]
    fn the_wrong_language_is_refused() {
        let english = book(400, "It was a dark and stormy night.");
        assert!(reasons(check(english.as_bytes(), Format::Srt, Script::Japanese)).contains("not Japanese"));
        assert!(check(english.as_bytes(), Format::Srt, Script::Any).is_ok());
        let chinese = book(400, "我是猫，还没有名字。");
        assert!(reasons(check(chinese.as_bytes(), Format::Srt, Script::Japanese)).contains("not Japanese"));
        assert!(check(chinese.as_bytes(), Format::Srt, Script::Chinese).is_ok());
        let japanese = book(400, "吾輩は猫である。");
        assert!(reasons(check(japanese.as_bytes(), Format::Srt, Script::Chinese)).contains("not Chinese"));
    }

    #[test]
    fn times_that_make_no_sense_are_refused() {
        // Each line ends before it starts.
        let backwards: String = (0..300)
            .map(|i| {
                format!(
                    "{}
{} --> {}
吾輩は猫である。

",
                    i + 1,
                    srt_stamp(i as f64 * 4.0 + 3.5),
                    srt_stamp(i as f64 * 4.0)
                )
            })
            .collect();
        assert!(reasons(check(backwards.as_bytes(), Format::Srt, Script::Any)).contains("end before they start"));

        let mut shuffled: Vec<String> = book(300, "吾輩は猫である。").split("\n\n").map(str::to_owned).collect();
        shuffled.reverse();
        assert!(reasons(check(shuffled.join("\n\n").as_bytes(), Format::Srt, Script::Any)).contains("start before"));

        // A whole page in each line: far more text than anyone can say in the time.
        let crammed = book(300, &"吾輩は猫である。".repeat(40));
        assert!(reasons(check(crammed.as_bytes(), Format::Srt, Script::Any)).contains("characters for each second"));
    }

    /// From the site: 走れメロス [B00YT6SNSW].srt ends with the rest of the book in one line,
    /// and the first version of this check refused it and 10 more of 144 good files.
    #[test]
    fn the_rest_of_the_book_in_the_last_line_is_let_through_and_named() {
        let tail = format!(
            "
301
{} --> {}
{}
",
            srt_stamp(1200.0),
            srt_stamp(1203.0),
            "メロスは激怒した。".repeat(400)
        );
        let text = book(300, "吾輩は猫である。") + &tail;
        let summary = check(text.as_bytes(), Format::Srt, Script::Japanese).unwrap();
        assert_eq!(summary.pages, 1);
        assert!(summary.to_string().ends_with(", 1 overlong line"), "{summary}");

        let many = book(300, "吾輩は猫である。") + &tail.repeat(4);
        assert!(reasons(check(many.as_bytes(), Format::Srt, Script::Japanese)).contains("pages, not subtitles"));
    }

    #[test]
    fn a_few_broken_blocks_are_forgiven_and_many_are_not() {
        let good = book(300, "吾輩は猫である。");
        let few = format!("{good}\n301\n00:20:00,000 --> soon\nいつか\n");
        assert!(check(few.as_bytes(), Format::Srt, Script::Any).is_ok());
        let many = format!("{good}\n{}", "9\n00:20:00,000 --> soon\nいつか\n\n".repeat(30));
        assert!(reasons(check(many.as_bytes(), Format::Srt, Script::Any)).contains("cannot be read"));
    }

    #[test]
    fn webvtt_and_ass_are_read() {
        let vtt = format!(
            "WEBVTT\n\nNOTE made by hand\n\n{}",
            book(300, "<i>吾輩</i>は猫である。").replace(',', ".")
        );
        let summary = check(vtt.as_bytes(), Format::Vtt, Script::Japanese).unwrap();
        assert_eq!(summary.cues, 300);

        let mut ass = String::from("[Script Info]\nTitle: x\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n");
        for i in 0..300 {
            let t = i * 4;
            ass.push_str(&format!(
                "Dialogue: 0,{}:{:02}:{:02}.00,{}:{:02}:{:02}.50,Default,,0,0,0,,{{\\an8}}吾輩は猫である、名前は\\Nまだ無い。\n",
                t / 3600, t / 60 % 60, t % 60, (t + 3) / 3600, (t + 3) / 60 % 60, (t + 3) % 60
            ));
        }
        let summary = check(ass.as_bytes(), Format::Ass, Script::Japanese).unwrap();
        assert_eq!(summary.cues, 300);
    }

    #[test]
    fn stamps() {
        assert_eq!(seconds("00:00:01,500"), Some(1.5));
        assert_eq!(seconds(" 1:02:03.45 "), Some(3723.45));
        assert_eq!(seconds("02:03.4"), Some(123.4));
        assert_eq!(seconds("10:00:00"), Some(36000.0));
        for bad in ["", "soon", "1", "1:2:3:4", "00:00:01,", "00:0a:01,000", "-1:00:00,000"] {
            assert_eq!(seconds(bad), None, "{bad}");
        }
    }

    /// Any bytes at all: the check answers, and does not panic.
    #[test]
    fn no_input_makes_it_panic() {
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let pieces: [&[u8]; 12] = [
            b"1\n",
            b"00:00:01,000",
            b" --> ",
            b"\n\n",
            b"\r\n",
            "吾輩は猫".as_bytes(),
            b"Dialogue: 0,0:00:01.00,0:00:02.00,,,,,,,",
            b"<i>",
            b"{\\an8",
            &[0xFF],
            b":",
            b",",
        ];
        for _ in 0..3000 {
            let mut bytes = Vec::new();
            for _ in 0..(next() % 40) {
                bytes.extend_from_slice(pieces[(next() % pieces.len() as u64) as usize]);
            }
            for format in [Format::Srt, Format::Vtt, Format::Ass] {
                let _ = check(&bytes, format, Script::Japanese);
            }
        }
    }
}

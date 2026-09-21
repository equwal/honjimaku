//! Entries for books.
//!
//! An anime has an AniList page and a drama has a TMDB page, and the site makes
//! an entry from that page. A book has neither, so a user names it: the title,
//! and the identifier of the audiobook where there is one (an Audible ASIN, an
//! audiobook.jp number). This module says which names are acceptable, and
//! which two names are the same book.

/// The title as typed, made fit to keep: trimmed, inner white space made single.
pub fn clean_title(raw: &str) -> Result<String, &'static str> {
    let title = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        return Err("Give the title of the book.");
    }
    if title.chars().count() > 200 {
        return Err("The title cannot be longer than 200 characters.");
    }
    if title.chars().any(char::is_control) {
        return Err("The title holds characters that cannot be shown.");
    }
    Ok(title)
}

/// An identifier of the audiobook: `B0BPXSSWVF`, `audiobook.jp 259377`.
pub fn clean_book_id(raw: &str) -> Result<String, &'static str> {
    let id = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let fine = |c: char| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '-' | '_');
    if id.is_empty() || id.len() > 40 || !id.chars().all(fine) {
        return Err("The audiobook ID can hold letters, digits, spaces, dots and dashes, 40 at most.");
    }
    Ok(id)
}

/// Two titles with the same key are the same book: `１Ｑ８４ BOOK1` and `1q84book1`.
/// Width, case, white space and punctuation do not count. Volume numbers do.
pub fn title_key(title: &str) -> String {
    title
        .chars()
        .map(|c| match c as u32 {
            // Full-width ASCII to ASCII.
            0xFF01..=0xFF5E => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            _ => c,
        })
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The key of a directory name on disk, which can carry what a title does not: a number
/// in brackets at the front (`[01] `, the place in a series) and an identifier in brackets
/// at the end (` [B0BPXSSWVF]`). Both are dropped. The title itself says which volume it is.
pub fn directory_key(name: &str) -> String {
    let mut name = name.trim();
    while name.starts_with('[') {
        match name.find("] ") {
            Some(at) if at + 2 < name.len() => name = name[at + 2..].trim_start(),
            _ => break,
        }
    }
    let without_id = match (name.rfind(" ["), name.ends_with(']')) {
        (Some(at), true) if at > 0 => &name[..at],
        _ => name,
    };
    title_key(without_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_cleaned_and_bad_ones_refused() {
        assert_eq!(clean_title("  吾輩は　猫である \n").unwrap(), "吾輩は 猫である");
        assert!(clean_title("   ").is_err());
        assert!(clean_title(&"あ".repeat(201)).is_err());
        assert!(clean_title("bell\u{7}").is_err());
    }

    #[test]
    fn book_ids() {
        assert_eq!(clean_book_id(" B0BPXSSWVF ").unwrap(), "B0BPXSSWVF");
        assert_eq!(clean_book_id("audiobook.jp  259377").unwrap(), "audiobook.jp 259377");
        for bad in ["", "../../etc", "a]b", "<script>", &"1".repeat(41)] {
            assert!(clean_book_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_same_book_has_the_same_key_however_it_is_typed() {
        assert_eq!(
            title_key("１Ｑ８４―ＢＯＯＫ１〈４月－６月〉前編"),
            title_key("1q84 book1 4月6月 前編")
        );
        assert_eq!(title_key("Fate∕strange Fake(1)"), title_key("fate strange fake 1"));
        assert_ne!(title_key("ＧＪ部"), title_key("ＧＪ部中等部"));
        assert_ne!(title_key("精霊の守り人 1"), title_key("精霊の守り人 2"));
        assert_eq!(title_key("！？…"), "");
    }

    #[test]
    fn a_directory_name_is_matched_without_its_identifier() {
        assert_eq!(
            directory_key("[01] １．精霊の守り人 [B0BQ8Z1ZHT]"),
            title_key("1.精霊の守り人")
        );
        assert_eq!(
            directory_key("[01] [1巻・上] リビルドワールドI〈上〉 誘う亡霊 [B0CBK1JPGF]"),
            title_key("リビルドワールドI 上 誘う亡霊")
        );
        assert_eq!(directory_key("走れメロス [B00YT6SNSW]"), title_key("走れメロス"));
        assert_eq!(directory_key("さぶ"), title_key("さぶ"));
        assert_eq!(directory_key("[drama]"), title_key("drama"));
    }
}

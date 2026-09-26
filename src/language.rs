//! The languages of the subtitles. An entry holds the subtitles of one language, named by
//! its ISO 639-1 code: `ja`, `zh`, `en`. The crate isolang gives the codes and their
//! English names, so the site does not keep a table of its own.

use std::sync::OnceLock;

use isolang::Language;

/// The language of the site. The entries that existed before the languages are in it, and
/// a request that names no language asks for it.
pub const DEFAULT: &str = "ja";

/// The error for a language that is not an ISO 639-1 code.
pub const UNKNOWN: &str = "Unknown language. Give an ISO 639-1 code, for example \"zh\".";

/// The ISO 639-1 code in what a user typed, in lower case. `None` if it is not a code.
pub fn code(raw: &str) -> Option<&'static str> {
    Language::from_639_1(&raw.trim().to_ascii_lowercase()).and_then(|l| l.to_639_1())
}

/// The code in an optional parameter of a request. A parameter that is missing or empty is
/// the default language. `None` if the parameter is not a code.
pub fn code_or_default(raw: Option<&str>) -> Option<&'static str> {
    match raw {
        Some(raw) if !raw.trim().is_empty() => code(raw),
        _ => Some(DEFAULT),
    }
}

/// The English name of the language of a code. An unknown code is its own name.
pub fn name(code: &str) -> &str {
    match Language::from_639_1(code) {
        Some(language) => language.to_name(),
        None => code,
    }
}

/// Each language that has an ISO 639-1 code, as (code, name), in the order of the names.
pub fn all() -> &'static [(&'static str, &'static str)] {
    static ALL: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut all: Vec<_> = isolang::languages()
            .filter_map(|l| Some((l.to_639_1()?, l.to_name())))
            .collect();
        all.sort_by_key(|&(_, name)| name);
        all
    })
}

/// Each language that has an ISO 639-1 code, as (code, name, count), with the count from
/// `count`. The language with the most entries is first. Languages with the same count are
/// in the order of their names.
pub fn by_count(count: impl Fn(&str) -> usize) -> Vec<(&'static str, &'static str, usize)> {
    let mut languages: Vec<_> = all().iter().map(|&(code, name)| (code, name, count(code))).collect();
    languages.sort_by_key(|&(_, name, n)| (std::cmp::Reverse(n), name));
    languages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_listed_code_is_a_code() {
        assert!(all().len() > 180, "{}", all().len());
        for &(code, language) in all() {
            assert_eq!(super::code(code), Some(code));
            assert_eq!(super::code(&format!(" {} ", code.to_uppercase())), Some(code));
            assert_eq!(code_or_default(Some(code)), Some(code));
            assert_eq!(name(code), language);
        }
        assert!(all().windows(2).all(|w| w[0].1 <= w[1].1), "sorted by name");
        assert_eq!(name(DEFAULT), "Japanese");
    }

    #[test]
    fn what_is_not_a_code_is_refused() {
        for not in ["x", "jpn", "zz", "japanese", "j a", "ja-JP"] {
            assert_eq!(code(not), None, "{not}");
            assert_eq!(code_or_default(Some(not)), None, "{not}");
        }
        assert_eq!(name("zz"), "zz");
    }

    #[test]
    fn a_missing_or_empty_code_is_the_default() {
        assert_eq!(code_or_default(None), Some(DEFAULT));
        assert_eq!(code_or_default(Some("")), Some(DEFAULT));
        assert_eq!(code_or_default(Some("  ")), Some(DEFAULT));
    }

    #[test]
    fn languages_with_more_entries_are_first() {
        let count = |code: &str| match code {
            "ja" => 5,
            "zh" | "en" => 2,
            _ => 0,
        };
        let languages = by_count(count);
        assert_eq!(languages.len(), all().len());
        let first: Vec<_> = languages.iter().take(3).map(|&(code, _, n)| (code, n)).collect();
        assert_eq!(
            first,
            [("ja", 5), ("zh", 2), ("en", 2)],
            "Chinese is before English by name"
        );
        assert!(
            languages
                .iter()
                .all(|&(code, name, n)| name == super::name(code) && n == count(code))
        );
        assert!(languages.windows(2).all(|w| (w[0].2, w[1].1) >= (w[1].2, w[0].1)));
    }
}

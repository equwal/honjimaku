//! The languages of the entries. A code is an ISO 639-1 code: `ja`, `en`, `de`.
//!
//! The site has a language of its own (`subtitle_language`). An entry with no
//! language of its own is in that language.

use std::sync::OnceLock;

use isolang::Language;

/// The ISO 639-1 code in what a user typed, in lower case. `None` if it is not one.
pub fn code(raw: &str) -> Option<&'static str> {
    Language::from_639_1(&raw.trim().to_ascii_lowercase()).and_then(|l| l.to_639_1())
}

/// The English name of the language. An unknown code is named by itself.
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

/// The words of a name in lower case, without the part in brackets:
/// "Modern Greek (1453-)" is ["modern", "greek"].
fn words(name: &str) -> Vec<String> {
    let name = name.split('(').next().unwrap_or(name);
    name.split(|c: char| c == '_' || c.is_whitespace())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// True if Audible's name of a language (`japanese`, `mandarin_chinese`) is the language of the code.
/// The words of the shorter name must all be in the longer name.
pub fn is_audible_language(code: &str, audible: &str) -> bool {
    let wanted = words(name(code));
    let spoken = words(audible);
    let (short, long) = if wanted.len() <= spoken.len() {
        (&wanted, &spoken)
    } else {
        (&spoken, &wanted)
    };
    !short.is_empty() && short.iter().all(|w| long.contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_listed_code_is_a_code() {
        assert!(all().len() > 180);
        for &(code, language) in all() {
            assert_eq!(super::code(code), Some(code));
            assert_eq!(super::code(&format!(" {} ", code.to_uppercase())), Some(code));
            assert_eq!(name(code), language);
        }
        assert!(all().windows(2).all(|w| w[0].1 <= w[1].1));
    }

    #[test]
    fn what_is_not_a_code_is_refused() {
        for not in ["", "x", "jpn", "zz", "japanese", "j a"] {
            assert_eq!(code(not), None, "{not}");
        }
        assert_eq!(name("zz"), "zz");
    }

    #[test]
    fn audible_names_are_matched() {
        assert!(is_audible_language("ja", "japanese"));
        assert!(is_audible_language("en", "english"));
        assert!(is_audible_language("de", "german"));
        assert!(is_audible_language("zh", "mandarin_chinese"));
        assert!(is_audible_language("el", "greek"));
        assert!(!is_audible_language("ja", "english"));
        assert!(!is_audible_language("ms", "malayalam"));
        assert!(!is_audible_language("en", ""));
    }
}

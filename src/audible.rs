//! The verifier of a site for books: the Audible catalog.
//!
//! An anime entry is verified by its AniList page. A book has none, so the site
//! asks Audible instead. An Audible audiobook has an ASIN (`B0BPXSSWVF`), and the
//! catalog API of Audible gives its title, authors, language and series without
//! a key. An ASIN that Audible does not know gives a product with no title.
//!
//! Audible has one catalog for each country, and a catalog does not know the
//! books of the others. The site asks each catalog, the one of the language first.

use serde::Deserialize;

/// The domains of the Audible shops, after `audible.`.
const MARKETS: [&str; 10] = ["co.jp", "com", "co.uk", "de", "fr", "it", "es", "ca", "com.au", "in"];

/// The shop that most likely has a book in the language of the code.
fn home_market(language: &str) -> &'static str {
    match language {
        "ja" => "co.jp",
        "de" => "de",
        "fr" => "fr",
        "it" => "it",
        "es" => "es",
        "hi" => "in",
        _ => "com",
    }
}

/// The shops in the order that the site asks them.
fn markets(language: &str) -> impl Iterator<Item = &'static str> {
    let home = home_market(language);
    std::iter::once(home).chain(MARKETS.into_iter().filter(move |&m| m != home))
}

/// The ASIN in what a user typed: the ASIN itself, or an Audible URL that ends with it.
/// An ASIN of an audiobook is ten characters, the first a `B`.
pub fn asin(raw: &str) -> Option<String> {
    let raw = raw.trim().trim_end_matches('/');
    let raw = raw.split('?').next().unwrap_or(raw);
    let tail = raw.rsplit('/').next().unwrap_or(raw);
    let looks_like_asin =
        tail.len() == 10 && tail.starts_with('B') && tail.chars().all(|c| c.is_ascii_digit() || c.is_ascii_uppercase());
    let is_audible = raw == tail || raw.contains("audible.");
    (looks_like_asin && is_audible).then(|| tail.to_owned())
}

/// The page of the audiobook in the shop.
pub fn url(market: &str, asin: &str) -> String {
    format!("https://www.audible.{market}/pd/{asin}")
}

/// The page of the audiobook in the shop of its language. Another shop may have it instead.
pub fn page(language: &str, asin: &str) -> String {
    url(home_market(language), asin)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audiobook {
    pub asin: String,
    /// The shop that has the audiobook, as in [`MARKETS`].
    pub market: &'static str,
    /// The title as the shop writes it. The folder on disk is named after it.
    pub title: String,
    pub authors: Vec<String>,
    /// The language as Audible names it: `japanese`, `english`.
    pub language: Option<String>,
    pub adult: bool,
    /// The series and the place of this book in it, when the book is part of one.
    pub series: Option<(String, Option<String>)>,
}

impl Audiobook {
    /// A note for the page of the entry: where the book is, who wrote it.
    pub fn note(&self) -> String {
        let mut note = format!("Audiobook: [{}]({})", self.asin, url(self.market, &self.asin));
        if !self.authors.is_empty() {
            note.push_str(". Author: ");
            note.push_str(&self.authors.join(", "));
        }
        if let Some((series, sequence)) = &self.series {
            note.push_str(". Series: ");
            note.push_str(series);
            if let Some(sequence) = sequence {
                note.push_str(" (");
                note.push_str(sequence);
                note.push(')');
            }
        }
        note
    }
}

#[derive(Deserialize)]
struct Response {
    product: Product,
}

#[derive(Deserialize)]
struct Product {
    asin: String,
    title: Option<String>,
    #[serde(default)]
    authors: Vec<Named>,
    language: Option<String>,
    #[serde(default)]
    is_adult_product: bool,
    #[serde(default)]
    series: Vec<Series>,
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

#[derive(Deserialize)]
struct Series {
    title: String,
    sequence: Option<String>,
}

/// What the catalog of a shop answered. `None` when the shop does not know the ASIN.
fn read(market: &'static str, json: &str) -> anyhow::Result<Option<Audiobook>> {
    let product = serde_json::from_str::<Response>(json)?.product;
    let Some(title) = product.title else {
        return Ok(None);
    };
    Ok(Some(Audiobook {
        asin: product.asin,
        market,
        title,
        authors: product.authors.into_iter().map(|a| a.name).collect(),
        language: product.language,
        adult: product.is_adult_product,
        series: product.series.into_iter().next().map(|s| (s.title, s.sequence)),
    }))
}

/// Asks one shop about an ASIN.
async fn lookup_in(client: &reqwest::Client, market: &'static str, asin: &str) -> anyhow::Result<Option<Audiobook>> {
    let response = client
        .get(format!("https://api.audible.{market}/1.0/catalog/products/{asin}"))
        .query(&[("response_groups", "product_desc,product_attrs,contributors,series")])
        .send()
        .await?
        .error_for_status()?;
    read(market, &response.text().await?)
}

/// Asks each shop about an ASIN at the same time. The first shop in the order of
/// [`markets`] that knows it answers. An error is a network or server fault of
/// each shop that did not know it, not an unknown ASIN.
pub async fn lookup(client: &reqwest::Client, asin: &str, language: &str) -> anyhow::Result<Option<Audiobook>> {
    let answers = futures::future::join_all(markets(language).map(|m| lookup_in(client, m, asin))).await;
    let mut fault = None;
    for answer in answers {
        match answer {
            Ok(Some(book)) => return Ok(Some(book)),
            Ok(None) => {}
            Err(e) => fault = Some(e),
        }
    }
    match fault {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_asin_is_read_from_what_a_user_types() {
        assert_eq!(asin(" B0BPXSSWVF ").as_deref(), Some("B0BPXSSWVF"));
        assert_eq!(
            asin("https://www.audible.co.jp/pd/%E3%83%AA%E3%83%93-%E3%82%AA/B0CBK1JPGF?ref=a_library").as_deref(),
            Some("B0CBK1JPGF")
        );
        assert_eq!(
            asin("https://www.audible.com/pd/B0CBK1JPGF/").as_deref(),
            Some("B0CBK1JPGF")
        );
        for not in [
            "",
            "audiobook.jp 259377",
            "b0bpxsswvf",
            "B0BPXSSWV",
            "B0BPXSSWVFX",
            "https://example.com/B0CBK1JPGF",
        ] {
            assert_eq!(asin(not), None, "{not}");
        }
    }

    #[test]
    fn the_answer_of_the_catalog_is_read() {
        // What api.audible.co.jp answered for B0BPXSSWVF on 2026-09-21, shortened.
        let json = r#"{"product":{"asin":"B0BPXSSWVF","authors":[{"asin":"B000AP7AFI","name":"村上 春樹"}],
            "is_adult_product":false,"language":"japanese","narrators":[{"name":"高橋 一生"}],
            "series":[{"asin":"B0BQ59H6YQ","sequence":"１Ｑ８４―ＢＯＯＫ１〈４月－６月〉前編","title":"１Ｑ８４","url":"/pd/x/B0BQ59H6YQ"}],
            "title":"１Ｑ８４―ＢＯＯＫ１〈４月－６月〉前編"},"response_groups":["product_desc"]}"#;
        let book = read("co.jp", json).unwrap().unwrap();
        assert_eq!(book.title, "１Ｑ８４―ＢＯＯＫ１〈４月－６月〉前編");
        assert_eq!(book.authors, vec!["村上 春樹"]);
        assert_eq!(book.language.as_deref(), Some("japanese"));
        assert!(!book.adult);
        assert_eq!(book.series.as_ref().map(|s| s.0.as_str()), Some("１Ｑ８４"));
        assert!(book.note().contains("村上 春樹"));
        assert!(book.note().contains("https://www.audible.co.jp/pd/B0BPXSSWVF"));

        // An ASIN that Audible does not know: a product with no title.
        let unknown = r#"{"product":{"asin":"B0000000XX","asset_details":[],"is_vvab":false},"response_groups":["always-returned"]}"#;
        assert_eq!(read("co.jp", unknown).unwrap(), None);
        assert!(read("co.jp", "not json").is_err());
    }

    #[test]
    fn each_shop_is_asked_once_and_the_shop_of_the_language_first() {
        for language in ["ja", "en", "de", "hi", "ko", ""] {
            let order: Vec<_> = markets(language).collect();
            assert_eq!(order[0], home_market(language), "{language}");
            let mut sorted = order.clone();
            sorted.sort_unstable();
            let mut all = MARKETS.to_vec();
            all.sort_unstable();
            assert_eq!(sorted, all, "{language}");
        }
    }
}

//! The verifier of a site for books: the Audible catalog.
//!
//! An anime entry is verified by its AniList page. A book has none, so the site
//! asks Audible instead. An Audible audiobook has an ASIN (`B0BPXSSWVF`), and the
//! catalog API of Audible gives its title, authors, language and series without
//! a key. An ASIN that Audible does not know gives a product with no title.
//!
//! Audible has one catalog for each country. The books here are Japanese, so the
//! site asks the Japanese one.

use serde::Deserialize;

const CATALOG: &str = "https://api.audible.co.jp/1.0/catalog/products";

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
pub fn url(asin: &str) -> String {
    format!("https://www.audible.co.jp/pd/{asin}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audiobook {
    pub asin: String,
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
        let mut note = format!("Audiobook: [{}]({})", self.asin, url(&self.asin));
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

/// What the catalog answered. `None` when Audible does not know the ASIN.
fn read(json: &str) -> anyhow::Result<Option<Audiobook>> {
    let product = serde_json::from_str::<Response>(json)?.product;
    let Some(title) = product.title else {
        return Ok(None);
    };
    Ok(Some(Audiobook {
        asin: product.asin,
        title,
        authors: product.authors.into_iter().map(|a| a.name).collect(),
        language: product.language,
        adult: product.is_adult_product,
        series: product.series.into_iter().next().map(|s| (s.title, s.sequence)),
    }))
}

/// Asks Audible about an ASIN. An error is a network or server fault, not an unknown ASIN.
pub async fn lookup(client: &reqwest::Client, asin: &str) -> anyhow::Result<Option<Audiobook>> {
    let response = client
        .get(format!("{CATALOG}/{asin}"))
        .query(&[("response_groups", "product_desc,product_attrs,contributors,series")])
        .send()
        .await?
        .error_for_status()?;
    read(&response.text().await?)
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
        let book = read(json).unwrap().unwrap();
        assert_eq!(book.title, "１Ｑ８４―ＢＯＯＫ１〈４月－６月〉前編");
        assert_eq!(book.authors, vec!["村上 春樹"]);
        assert_eq!(book.language.as_deref(), Some("japanese"));
        assert!(!book.adult);
        assert_eq!(book.series.as_ref().map(|s| s.0.as_str()), Some("１Ｑ８４"));
        assert!(book.note().contains("村上 春樹") && book.note().contains(&url("B0BPXSSWVF")));

        // An ASIN that Audible does not know: a product with no title.
        let unknown = r#"{"product":{"asin":"B0000000XX","asset_details":[],"is_vvab":false},"response_groups":["always-returned"]}"#;
        assert_eq!(read(unknown).unwrap(), None);
        assert!(read("not json").is_err());
    }
}

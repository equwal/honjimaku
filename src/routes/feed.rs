//! RSS feeds. Each listing has a feed of its entries with the newest files, so that a feed
//! reader shows each upload. On a site for books, each language has a listing and a feed.

use askama::Template;
use axum::{
    Router,
    extract::{Query, State},
    http::{StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};

use super::{Listing, ListingQuery};
use crate::{AppState, Config, filters, models::DirectoryEntry};

/// The largest number of entries in a feed.
const MAX_ITEMS: usize = 50;

#[derive(Template)]
#[template(path = "feed.xml")]
struct FeedTemplate<'a> {
    title: String,
    description: String,
    /// The canonical URL of the site, with no slash at the end.
    base: String,
    /// The path of the page of the listing.
    page: String,
    /// The path of the feed.
    path: String,
    /// The ISO 639-1 code of the language of the listing.
    language: &'a str,
    /// The entries of the listing with the newest files, the newest first.
    entries: Vec<&'a DirectoryEntry>,
}

impl<'a> FeedTemplate<'a> {
    fn new(config: &'a Config, listing: Listing<'a>, entries: impl Iterator<Item = &'a DirectoryEntry>) -> Self {
        let mut entries: Vec<_> = entries.filter(|e| listing.shows(config, e)).collect();
        entries.sort_by_key(|e| std::cmp::Reverse(e.last_updated_at));
        entries.truncate(MAX_ITEMS);
        Self {
            title: listing.feed_title(config),
            description: format!("{} with new files, the newest first", listing.name(config)),
            base: config.canonical_url(),
            page: listing.page(config),
            path: listing.feed_path(config),
            language: listing.language(config),
            entries,
        }
    }
}

async fn respond(state: &AppState, listing: Listing<'_>) -> Response {
    let entries = state.directory_entries().await;
    match FeedTemplate::new(state.config(), listing, entries.iter()).render() {
        Ok(xml) => ([(CONTENT_TYPE, "application/rss+xml; charset=utf-8")], xml).into_response(),
        Err(error) => {
            tracing::error!(%error, "Failed to render a feed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// The feed of the listing at "/". A site for books reads the language and the kind from the
/// query, as the listing does. An unknown language or kind gets 404, because a feed reader must
/// not follow the feed of another listing.
async fn main_feed(State(state): State<AppState>, Query(query): Query<ListingQuery>) -> Response {
    match Listing::main(state.config(), &query) {
        Some(listing) => respond(&state, listing).await,
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The feed of the listing at "/dramas". A site with one listing sends the reader to the feed
/// of its live action shows, as "/dramas" does.
async fn dramas_feed(State(state): State<AppState>) -> Response {
    let config = state.config();
    if config.single_listing() {
        let to = if config.book_site {
            "/feed.xml?kind=drama"
        } else {
            "/feed.xml"
        };
        return Redirect::permanent(to).into_response();
    }
    respond(&state, Listing::Dramas).await
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/feed.xml", get(main_feed))
        .route("/dramas/feed.xml", get(dramas_feed))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use proptest::prelude::*;
    use quick_xml::{Reader, escape::resolve_predefined_entity, events::Event};
    use time::{OffsetDateTime, format_description::well_known::Rfc2822};

    use super::*;
    use crate::models::Kind;

    /// A config for a local site. The name of the site must be escaped in XML.
    fn config(book_site: bool) -> Config {
        let mut config = Config::new().unwrap();
        config.book_site = book_site;
        config.site_name = String::from("Books & <Subs>");
        config
    }

    fn query(lang: Option<&str>, kind: Option<&str>) -> ListingQuery {
        ListingQuery {
            lang: lang.map(str::to_owned),
            kind: kind.map(str::to_owned),
        }
    }

    fn entry(
        id: i64,
        name: &str,
        seconds: i64,
        language: Option<&str>,
        kind: Option<Kind>,
        anime: bool,
    ) -> DirectoryEntry {
        let mut entry = DirectoryEntry::temporary(name.to_owned());
        entry.id = id;
        entry.last_updated_at = OffsetDateTime::from_unix_timestamp(seconds).unwrap();
        entry.language = language.map(str::to_owned);
        entry.kind = kind;
        entry.flags.set_anime(anime);
        entry
    }

    /// True if XML 1.0 allows the character (the production "Char").
    fn is_xml_char(c: char) -> bool {
        matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..)
    }

    #[derive(Debug, Default)]
    struct Item {
        title: String,
        link: String,
        guid: String,
        date: String,
    }

    /// The title and the link of the channel, and the items of a feed, as an XML reader sees them.
    /// Panics if the feed is not well-formed.
    fn parse(xml: &str) -> (Item, Vec<Item>) {
        let mut reader = Reader::from_str(xml);
        let mut path: Vec<String> = Vec::new();
        let mut channel = Item::default();
        let mut items: Vec<Item> = Vec::new();
        loop {
            let text = match reader.read_event().unwrap() {
                Event::Start(element) => {
                    let name = String::from_utf8(element.name().as_ref().to_vec()).unwrap();
                    if name == "item" {
                        items.push(Item::default());
                    }
                    path.push(name);
                    continue;
                }
                Event::End(_) => {
                    path.pop();
                    continue;
                }
                Event::Text(text) => text.xml10_content().unwrap().into_owned(),
                Event::GeneralRef(reference) => match reference.resolve_char_ref().unwrap() {
                    Some(c) => c.to_string(),
                    None => resolve_predefined_entity(&reference.decode().unwrap())
                        .unwrap()
                        .to_owned(),
                },
                Event::Eof => break,
                _ => continue,
            };
            let at: Vec<&str> = path.iter().map(String::as_str).collect();
            let field = match at[..] {
                ["rss", "channel", "title"] => &mut channel.title,
                ["rss", "channel", "link"] => &mut channel.link,
                ["rss", "channel", "item", "title"] => &mut items.last_mut().unwrap().title,
                ["rss", "channel", "item", "link"] => &mut items.last_mut().unwrap().link,
                ["rss", "channel", "item", "guid"] => &mut items.last_mut().unwrap().guid,
                ["rss", "channel", "item", "pubDate"] => &mut items.last_mut().unwrap().date,
                _ => continue,
            };
            field.push_str(&text);
        }
        assert!(path.is_empty(), "elements with no end: {path:?}");
        (channel, items)
    }

    fn main(language: &str, kind: Kind) -> Listing<'_> {
        Listing::Main { language, kind }
    }

    #[test]
    fn only_a_site_for_books_has_a_listing_for_each_language_and_kind() {
        let books = config(true);
        assert_eq!(Listing::main(&books, &query(None, None)), Some(main("ja", Kind::Book)));
        assert_eq!(
            Listing::main(&books, &query(Some(" DE "), Some("Anime"))),
            Some(main("de", Kind::Anime))
        );
        assert_eq!(Listing::main(&books, &query(Some("zz"), None)), None);
        assert_eq!(Listing::main(&books, &query(None, Some("manga"))), None);
        let anime = config(false);
        let first = Some(main("ja", Kind::Book));
        assert_eq!(Listing::main(&anime, &query(Some("de"), Some("drama"))), first);
        assert_eq!(Listing::main(&anime, &query(Some("zz"), Some("manga"))), first);
    }

    #[test]
    fn each_listing_has_a_feed_beside_its_page() {
        let books = config(true);
        let pages = [
            (main("ja", Kind::Book), "/", "/feed.xml", "Books in Japanese"),
            (
                main("de", Kind::Book),
                "/?lang=de",
                "/feed.xml?lang=de",
                "Books in German",
            ),
            (
                main("ja", Kind::Anime),
                "/?kind=anime",
                "/feed.xml?kind=anime",
                "Anime in Japanese",
            ),
            (
                main("de", Kind::Drama),
                "/?lang=de&kind=drama",
                "/feed.xml?lang=de&kind=drama",
                "Live Action in German",
            ),
        ];
        for (listing, page, feed, name) in pages {
            assert_eq!(listing.page(&books), page);
            assert_eq!(listing.feed_path(&books), feed);
            assert_eq!(listing.feed_title(&books), format!("Books & <Subs>: {name}"));
        }
        let anime = config(false);
        assert_eq!(main("ja", Kind::Book).feed_path(&anime), "/feed.xml");
        assert_eq!(main("ja", Kind::Book).feed_title(&anime), "Books & <Subs>: Anime");
        assert_eq!(Listing::Dramas.page(&anime), "/dramas");
        assert_eq!(Listing::Dramas.feed_path(&anime), "/dramas/feed.xml");
        assert_eq!(Listing::Dramas.feed_title(&anime), "Books & <Subs>: Live Action");
    }

    proptest! {
        /// A feed is well-formed XML. It holds the entries of its listing with the newest files,
        /// the newest first, each with its name, its link and its date.
        #[test]
        fn a_feed_holds_the_newest_entries_of_its_listing(
            rows in prop::collection::vec(
                (
                    // any::<String>() makes no control characters, so the second pattern adds them,
                    // with the characters that XML escapes and the two that XML does not allow.
                    prop_oneof![any::<String>(), "[\\x00-\\x20<>&'\"a-z\\x7F-\\x9F\\x{FFFE}\\x{FFFF}]{0,12}"],
                    prop_oneof![946_684_800i64..4_102_444_800, 1_000_000_000i64..1_000_000_010],
                    prop::option::of(prop::sample::select(vec!["ja", "de", "en"])),
                    prop::option::of(prop::sample::select(Kind::ALL.to_vec())),
                    any::<bool>(),
                ),
                0..2 * MAX_ITEMS,
            ),
            book_site in any::<bool>(),
            language in prop::sample::select(vec!["ja", "de", "en"]),
            kind in prop::sample::select(Kind::ALL.to_vec()),
            dramas in any::<bool>(),
        ) {
            let config = config(book_site);
            let entries: Vec<_> = rows
                .iter()
                .zip(1..)
                .map(|((name, seconds, language, kind, anime), id)| {
                    entry(id, name, *seconds, *language, *kind, *anime)
                })
                .collect();
            let listing = if dramas {
                Listing::Dramas
            } else {
                Listing::main(&config, &query(Some(language), Some(kind.as_str()))).unwrap()
            };

            let xml = FeedTemplate::new(&config, listing, entries.iter()).render().unwrap();
            prop_assert!(xml.chars().all(is_xml_char));
            let (channel, items) = parse(&xml);
            prop_assert_eq!(channel.title, listing.feed_title(&config));
            prop_assert_eq!(channel.link, format!("{}{}", config.canonical_url(), listing.page(&config)));
            let shown: Vec<&DirectoryEntry> = entries.iter().filter(|e| listing.shows(&config, e)).collect();
            prop_assert_eq!(items.len(), shown.len().min(MAX_ITEMS));

            let prefix = format!("{}/entry/", config.canonical_url());
            let mut ids = HashSet::new();
            let mut oldest = None;
            for item in &items {
                let id: i64 = item.link.strip_prefix(&prefix).unwrap().parse().unwrap();
                let entry = &entries[usize::try_from(id - 1).unwrap()];
                prop_assert!(listing.shows(&config, entry));
                prop_assert!(ids.insert(id), "entry {} is in the feed two times", id);
                prop_assert_eq!(&item.title, &filters::xml_text(&entry.name, askama::NO_VALUES).unwrap());
                prop_assert_eq!(&item.guid, &format!("{}#{}", item.link, entry.last_updated_at.unix_timestamp()));
                let date = OffsetDateTime::parse(&item.date, &Rfc2822).unwrap();
                prop_assert_eq!(date, entry.last_updated_at);
                prop_assert!(oldest.is_none_or(|oldest| date <= oldest), "an entry is before a newer one");
                oldest = Some(date);
            }
            // An entry of the listing that is not in the feed is not newer than the entries in it.
            if let Some(oldest) = oldest {
                prop_assert!(shown.iter().all(|e| ids.contains(&e.id) || e.last_updated_at <= oldest));
            }
        }
    }
}

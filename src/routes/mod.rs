use crate::{
    cached::BodyCache,
    error::{ApiError, InternalError},
    filters,
    flash::Flashes,
    headers::{AcceptEncoding, UserAgent},
    models::{Account, AccountCheck, Kind},
    utils::HtmlPage,
};
use askama::Template;
use axum::{
    Extension, Router,
    extract::{Path, Query, RawQuery, State},
    response::{IntoResponse, Redirect},
    routing::get,
};
use reqwest::header::{CONTENT_TYPE, USER_AGENT};

use crate::{AppState, Config, models::DirectoryEntry};

mod admin;
mod api;
mod audit;
mod auth;
mod entry;
mod feed;
mod notification;
mod opensearch;
mod relations;
mod report;

pub use api::{ApiToken, SearchQuery, copy_api_token};
pub(crate) use entry::{PathIds, directory_entry_path};
pub(crate) use report::RichReport;

#[derive(Template)]
#[template(path = "index.html")]
struct ListingTemplate<'a, It>
where
    It: Iterator<Item = &'a DirectoryEntry> + Clone,
{
    account: Option<Account>,
    entries: It,
    flashes: Flashes,
    url: String,
    anime: bool,
    editor: bool,
    /// On a site for books: the ISO 639-1 code of the language that the page lists.
    language: &'a str,
    /// On a site for books: a tab for each language that has entries.
    tabs: Vec<Tab<'a>>,
    /// On a site for books: each language as (code, name, number of entries), the language with the most entries first.
    languages: Vec<(&'static str, &'static str, usize)>,
    /// On a site for books: a tab for each kind of entry that the language has (books, anime, live action).
    kinds: Vec<Tab<'a>>,
    /// True if the page lists books. Such a page has the form to add one.
    books: bool,
    /// True if the editor may import a ZIP here. On a site for books, an import makes a book,
    /// so only the page of the books has it.
    zip_import: bool,
    /// The path of the RSS feed of the listing.
    feed_path: String,
    /// The title of the RSS feed of the listing.
    feed_title: String,
}

struct Tab<'a> {
    href: String,
    name: &'a str,
    active: bool,
}

#[derive(serde::Deserialize)]
struct ListingQuery {
    lang: Option<String>,
    kind: Option<String>,
}

/// The address of a listing on a site for books. The books in the language of the site are at "/".
fn listing_href(config: &crate::Config, language: &str, kind: Kind) -> String {
    let mut query = Vec::with_capacity(2);
    if language != config.default_language() {
        query.push(format!("lang={language}"));
    }
    if kind != Kind::Book {
        query.push(format!("kind={}", kind.as_str()));
    }
    if query.is_empty() {
        String::from("/")
    } else {
        format!("/?{}", query.join("&"))
    }
}

/// A listing of entries. Each listing is a tab of the site and has an RSS feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Listing<'a> {
    /// The listing at "/". On a site for books, it holds the entries of one kind in one language.
    Main { language: &'a str, kind: Kind },
    /// The listing at "/dramas", of live action shows. Only a site with two listings has it.
    Dramas,
}

impl<'a> Listing<'a> {
    /// The listing at "/" that the query asks for. Only a site for books reads the query.
    /// `None` if the query names a language that is not an ISO 639-1 code, or an unknown kind.
    fn main(config: &'a Config, query: &ListingQuery) -> Option<Self> {
        if !config.book_site {
            return Some(Self::Main {
                language: config.default_language(),
                kind: Kind::Book,
            });
        }
        let language = match query.lang.as_deref() {
            None => config.default_language(),
            Some(raw) => crate::language::code(raw)?,
        };
        let kind = match query.kind.as_deref() {
            None => Kind::Book,
            Some(raw) => Kind::parse(raw)?,
        };
        Some(Self::Main { language, kind })
    }

    /// The ISO 639-1 code of the language of the listing.
    fn language(self, config: &'a Config) -> &'a str {
        match self {
            Self::Main { language, .. } => language,
            Self::Dramas => config.default_language(),
        }
    }

    /// The kind of entry of the listing. Only a site for books uses it.
    fn kind(self) -> Kind {
        match self {
            Self::Main { kind, .. } => kind,
            Self::Dramas => Kind::Drama,
        }
    }

    /// True if the listing shows the entry.
    fn shows(self, config: &Config, entry: &DirectoryEntry) -> bool {
        match self {
            Self::Main { language, kind } if config.book_site => {
                entry.language_code(config) == language && entry.kind_of(config) == kind
            }
            Self::Main { .. } => config.drama_site || entry.flags.is_anime(),
            Self::Dramas => !entry.flags.is_anime(),
        }
    }

    /// The name of the listing: "Anime", "Live Action", "Dramas" on a site for dramas,
    /// or "Books in German" and "Anime in Japanese" on a site for books.
    fn name(self, config: &Config) -> String {
        match self {
            Self::Main { language, kind } if config.book_site => {
                format!("{} in {}", kind.label(), crate::language::name(language))
            }
            Self::Main { .. } if config.drama_site => String::from("Dramas"),
            Self::Main { .. } => String::from("Anime"),
            Self::Dramas => String::from("Live Action"),
        }
    }

    /// The path of the page of the listing.
    fn page(self, config: &Config) -> String {
        match self {
            Self::Main { language, kind } => listing_href(config, language, kind),
            Self::Dramas => String::from("/dramas"),
        }
    }

    /// The path of the RSS feed of the listing: the path of its page with "feed.xml" after the
    /// first slash, so "/?lang=de" has "/feed.xml?lang=de".
    fn feed_path(self, config: &Config) -> String {
        match self {
            Self::Main { .. } => format!("/feed.xml{}", self.page(config).trim_start_matches('/')),
            Self::Dramas => String::from("/dramas/feed.xml"),
        }
    }

    /// The title of the RSS feed of the listing: "Jimaku: Anime" or "本字幕: Books in German".
    fn feed_title(self, config: &Config) -> String {
        format!("{}: {}", config.site_name, self.name(config))
    }
}

async fn index(
    State(state): State<AppState>,
    account: Option<Account>,
    flashes: Flashes,
    encoding: AcceptEncoding,
    Query(query): Query<ListingQuery>,
    Extension(cacher): Extension<BodyCache>,
) -> axum::response::Response {
    let config = state.config();
    // A site for books has a tab for each language, and under it a tab for each kind of
    // entry. The books in the language of the site are at "/".
    let Some(listing) = Listing::main(config, &query) else {
        return Redirect::to("/").into_response();
    };
    let (language, kind) = (listing.language(config), listing.kind());
    let entries = state.directory_entries().await;
    let mut bypass_cache = account.is_some();
    let mut tabs = Vec::new();
    let mut kinds = Vec::new();
    let mut languages = Vec::new();
    if config.book_site {
        let mut counts = std::collections::HashMap::new();
        for entry in entries.iter() {
            *counts.entry(entry.language_code(config)).or_insert(0) += 1;
        }
        languages = crate::language::by_count(|code| counts.get(code).copied().unwrap_or(0));
        let mut codes: Vec<&str> = entries.iter().map(|e| e.language_code(config)).collect();
        codes.push(config.default_language());
        codes.push(language);
        codes.sort_unstable();
        codes.dedup();
        // The language of the site is the first tab. The others follow in the order of their names.
        codes.sort_by_key(|&code| (code != config.default_language(), crate::language::name(code)));
        let has = |code: &str, wanted: Kind| {
            entries
                .iter()
                .any(|e| e.language_code(config) == code && e.kind_of(config) == wanted)
        };
        // The tab of a language opens the same kind of entry, if that language has it. Else it opens the books.
        tabs = codes
            .into_iter()
            .map(|code| Tab {
                href: listing_href(config, code, if has(code, kind) { kind } else { Kind::Book }),
                name: crate::language::name(code),
                active: code == language,
            })
            .collect();
        // A tab for each kind, also when the language has no entry of that kind yet: each
        // tab has the form to add one (AniList verifies an anime, TMDB a live action show).
        kinds = Kind::ALL
            .into_iter()
            .map(|k| Tab {
                href: listing_href(config, language, k),
                name: k.label(),
                active: k == kind,
            })
            .collect();
    }
    // The cache holds the pages of the language of the site. A copy of jimaku.cc makes its
    // anime and live action pages as large as the ones of jimaku.cc, so they are kept too.
    let cache_key = match kind {
        _ if language != config.default_language() => None,
        Kind::Book => Some("index"),
        Kind::Anime => Some("index-anime"),
        Kind::Drama => Some("index-drama"),
    };
    bypass_cache |= cache_key.is_none();
    let book_site = config.book_site;
    let editor = account.flags().is_editor();
    // A site for dramas lists every entry here, and its form asks for a TMDB page.
    let drama_site = config.drama_site;
    let href = listing_href(config, language, kind);
    let url = if href == "/" {
        config.canonical_url()
    } else {
        config.url_to(href)
    };
    let template = ListingTemplate {
        account,
        entries: entries.iter().filter(|e| listing.shows(config, e)),
        flashes,
        url,
        anime: if book_site { kind != Kind::Drama } else { !drama_site },
        editor,
        language,
        tabs,
        languages,
        kinds,
        books: book_site && kind == Kind::Book,
        zip_import: editor && (!book_site || kind == Kind::Book),
        feed_path: listing.feed_path(config),
        feed_title: listing.feed_title(config),
    };
    cacher
        .cache_template(cache_key.unwrap_or("index"), template, encoding, bypass_cache)
        .await
        .into_response()
}

async fn dramas(
    State(state): State<AppState>,
    account: Option<Account>,
    flashes: Flashes,
    encoding: AcceptEncoding,
    RawQuery(query): RawQuery,
    Extension(cacher): Extension<BodyCache>,
) -> axum::response::Response {
    // A site for books or for dramas has one listing. On a site for books the live action
    // shows are a kind of entry under each language.
    if state.config().single_listing() {
        let mut to = if state.config().book_site {
            String::from("/?kind=drama")
        } else {
            String::from("/")
        };
        if let Some(query) = query {
            to.push(if to.contains('?') { '&' } else { '?' });
            to.push_str(&query);
        }
        return Redirect::permanent(&to).into_response();
    }
    let config = state.config();
    let entries = state.directory_entries().await;
    let bypass_cache = account.is_some();
    let editor = account.flags().is_editor();
    let template = ListingTemplate {
        account,
        entries: entries.iter().filter(|e| Listing::Dramas.shows(config, e)),
        flashes,
        url: config.url_to("/dramas"),
        anime: false,
        editor,
        language: config.default_language(),
        tabs: Vec::new(),
        languages: Vec::new(),
        kinds: Vec::new(),
        books: false,
        zip_import: editor,
        feed_path: Listing::Dramas.feed_path(config),
        feed_title: Listing::Dramas.feed_title(config),
    };
    cacher
        .cache_template("dramas", template, encoding, bypass_cache)
        .await
        .into_response()
}

/// The web app manifest, named after the site.
async fn webmanifest(State(state): State<AppState>) -> impl IntoResponse {
    let name = &state.config().site_name;
    let manifest = serde_json::json!({
        "name": name,
        "short_name": name,
        "icons": [
            {"src": "/static/icons/android-chrome-192x192.png", "sizes": "192x192", "type": "image/png"},
            {"src": "/static/icons/android-chrome-512x512.png", "sizes": "512x512", "type": "image/png"}
        ],
        "theme_color": "#091624",
        "background_color": "#091624",
        "display": "standalone"
    });
    ([(CONTENT_TYPE, "application/manifest+json")], manifest.to_string())
}

#[derive(Template)]
#[template(path = "help.html")]
struct HelpTemplate {
    account: Option<Account>,
    /// The sites that this site keeps a copy of. The help names them.
    mirrors: &'static [crate::mirror::Mirror],
}

async fn help_page(account: Option<Account>) -> impl IntoResponse {
    let mirrors = crate::CONFIG.get().map(|c| c.mirrors.as_slice()).unwrap_or(&[]);
    HtmlPage(HelpTemplate { account, mirrors })
}

#[derive(Template)]
#[template(path = "contact.html")]
struct ContactTemplate {
    account: Option<Account>,
}

async fn contact_page(account: Option<Account>) -> impl IntoResponse {
    HtmlPage(ContactTemplate { account })
}

#[derive(serde::Deserialize)]
struct BypassCorsDownloadZip {
    url: String,
}

async fn bypass_download_zip_cors(
    State(state): State<AppState>,
    account: Account,
    user_agent: UserAgent,
    Query(query): Query<BypassCorsDownloadZip>,
) -> Result<impl IntoResponse, ApiError> {
    if !account.flags.is_editor() {
        return Err(ApiError::forbidden());
    }

    let response = state
        .client
        .get(query.url)
        .header(USER_AGENT, &user_agent.0)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(ApiError::new(format!(
            "URL responded with {}",
            response.status().as_u16()
        )));
    }

    match response.headers().get(CONTENT_TYPE) {
        None => return Err(ApiError::new("URL did not provide a content-type header")),
        Some(header) => {
            if header.as_bytes() != b"application/zip" && header.as_bytes() != b"application/octet-stream" {
                return Err(ApiError::new("URL did not provide an appropriate content-type header"));
            }
        }
    };

    Ok(response.bytes().await?)
}

#[derive(Template)]
#[template(path = "anilist.html")]
struct AniListTemplate {
    account: Option<Account>,
    user_name: String,
}

async fn show_anilist_page(account: Option<Account>, Path(user_name): Path<String>) -> impl IntoResponse {
    HtmlPage(AniListTemplate { account, user_name })
}

async fn backup(State(state): State<AppState>) -> Result<Redirect, InternalError> {
    let Some(url) = state.get_backup_url().await else {
        return Err(anyhow::Error::msg("no backup URL has been set").into());
    };
    Ok(Redirect::to(&url))
}

pub fn all() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/dramas", get(dramas))
        .route("/site.webmanifest", get(webmanifest))
        .route("/help", get(help_page))
        .route("/backup", get(backup))
        .route("/contact", get(contact_page))
        .route("/download-zip", get(bypass_download_zip_cors))
        .route("/anilist/{name}", get(show_anilist_page))
        .merge(auth::routes())
        .merge(entry::routes())
        .merge(feed::routes())
        .merge(admin::routes())
        .merge(audit::routes())
        .merge(relations::routes())
        .merge(opensearch::routes())
        .merge(notification::routes())
        .merge(report::routes())
        .nest("/api", api::routes())
}

/// The upload routes. They take a body of up to `MAX_BOOK_UPLOAD_SIZE` and have one hour to
/// read it, because an audiobook or a video is large. All other routes keep the small limits of `main`.
pub fn uploads() -> Router<AppState> {
    Router::new()
        .merge(entry::upload_routes())
        .nest("/api", api::upload_routes())
        .layer(axum::extract::DefaultBodyLimit::max(crate::utils::MAX_BOOK_UPLOAD_SIZE))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            crate::utils::MAX_BOOK_UPLOAD_SIZE,
        ))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(3600),
        ))
}

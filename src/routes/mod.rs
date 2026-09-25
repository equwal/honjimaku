use crate::{
    cached::BodyCache,
    error::{ApiError, InternalError},
    filters,
    flash::Flashes,
    headers::{AcceptEncoding, UserAgent},
    models::{Account, AccountCheck},
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

use crate::{AppState, models::DirectoryEntry};

mod admin;
mod api;
mod audit;
mod auth;
mod entry;
mod notification;
mod opensearch;
mod relations;
mod report;

pub use api::{ApiToken, SearchQuery, copy_api_token};
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
    /// On a site for books: a tab for each language that has books.
    tabs: Vec<LanguageTab<'a>>,
    /// On a site for books: each language as (code, name, number of entries), the language with the most entries first.
    languages: Vec<(&'static str, &'static str, usize)>,
}

struct LanguageTab<'a> {
    href: String,
    name: &'a str,
    active: bool,
}

#[derive(serde::Deserialize)]
struct ListingQuery {
    lang: Option<String>,
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
    // A site for books has a tab for each language. The language of the site is at "/".
    let language = match query.lang.as_deref().filter(|_| config.book_site) {
        None => config.default_language(),
        Some(raw) => match crate::language::code(raw) {
            Some(code) => code,
            None => return Redirect::to("/").into_response(),
        },
    };
    let entries = state.directory_entries().await;
    let mut bypass_cache = account.is_some();
    let mut tabs = Vec::new();
    let mut languages = Vec::new();
    if config.book_site {
        let mut counts = std::collections::HashMap::new();
        for entry in entries.iter().filter(|e| e.flags.is_anime()) {
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
        tabs = codes
            .into_iter()
            .map(|code| LanguageTab {
                href: if code == config.default_language() {
                    String::from("/")
                } else {
                    format!("/?lang={code}")
                },
                name: crate::language::name(code),
                active: code == language,
            })
            .collect();
        // The cache holds one page, the page of the language of the site.
        bypass_cache |= language != config.default_language();
    }
    let book_site = config.book_site;
    let editor = account.flags().is_editor();
    // A site for dramas lists every entry here, and its form asks for a TMDB page.
    let drama_site = config.drama_site;
    let url = if language == config.default_language() {
        config.canonical_url()
    } else {
        config.url_to(format!("/?lang={language}"))
    };
    let template = ListingTemplate {
        account,
        entries: entries
            .iter()
            .filter(|e| drama_site || e.flags.is_anime())
            .filter(|e| !book_site || e.language_code(config) == language),
        flashes,
        url,
        anime: !drama_site,
        editor,
        language,
        tabs,
        languages,
    };
    cacher
        .cache_template("index", template, encoding, bypass_cache)
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
    // A site for books or for dramas has one listing.
    if state.config().single_listing() {
        let to = match query {
            Some(query) => format!("/?{query}"),
            None => String::from("/"),
        };
        return Redirect::permanent(&to).into_response();
    }
    let entries = state.directory_entries().await;
    let bypass_cache = account.is_some();
    let editor = account.flags().is_editor();
    let template = ListingTemplate {
        account,
        entries: entries.iter().filter(|e| !e.flags.is_anime()),
        flashes,
        url: state.config().url_to("/dramas"),
        anime: false,
        editor,
        language: state.config().default_language(),
        tabs: Vec::new(),
        languages: Vec::new(),
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
}

async fn help_page(account: Option<Account>) -> impl IntoResponse {
    HtmlPage(HelpTemplate { account })
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

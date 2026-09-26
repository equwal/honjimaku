use std::collections::HashMap;

use crate::{
    cached::BodyCache,
    error::{ApiError, InternalError},
    filters,
    flash::Flashes,
    headers::{AcceptEncoding, UserAgent},
    language,
    models::{Account, AccountCheck},
    utils::HtmlPage,
};
use askama::Template;
use axum::{
    Extension, Router,
    extract::{Path, Query, State},
    response::{IntoResponse, Redirect, Response},
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
    /// The ISO 639-1 code of the language of the listing.
    language: &'static str,
    /// Each language as (code, name, number of entries in this listing), the most entries first.
    languages: Vec<(&'static str, &'static str, usize)>,
    /// The query that keeps the language in a link to the other listing. It is empty for
    /// the default language.
    language_query: String,
}

#[derive(serde::Deserialize)]
struct ListingQuery {
    /// The ISO 639-1 code of the language to list. The default is Japanese.
    #[serde(default)]
    lang: Option<String>,
}

/// The listing of the anime (at "/") or of the live action shows (at "/dramas"), in one language.
async fn listing(
    state: AppState,
    account: Option<Account>,
    flashes: Flashes,
    encoding: AcceptEncoding,
    cacher: BodyCache,
    query: ListingQuery,
    anime: bool,
) -> Response {
    let path = if anime { "/" } else { "/dramas" };
    let Some(language) = language::code_or_default(query.lang.as_deref()) else {
        // A language that is not an ISO 639-1 code shows the listing in the default language.
        return Redirect::to(path).into_response();
    };
    let entries = state.directory_entries().await;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for entry in entries.iter().filter(|e| e.flags.is_anime() == anime) {
        *counts.entry(entry.language.as_str()).or_default() += 1;
    }
    let languages = language::by_count(|code| counts.get(code).copied().unwrap_or_default());
    let language_query = if language == language::DEFAULT {
        String::new()
    } else {
        format!("?lang={language}")
    };
    let url = if anime && language_query.is_empty() {
        state.config().canonical_url()
    } else {
        state.config().url_to(format!("{path}{language_query}"))
    };
    // The cache holds few pages, and a listing in another language has few entries. So
    // only the listings in the default language are cached.
    let bypass_cache = account.is_some() || language != language::DEFAULT;
    let editor = account.flags().is_editor();
    let template = ListingTemplate {
        account,
        entries: entries
            .iter()
            .filter(move |e| e.flags.is_anime() == anime && e.language == language),
        flashes,
        url,
        anime,
        editor,
        language,
        languages,
        language_query,
    };
    let key = if anime { "index" } else { "dramas" };
    cacher
        .cache_template(key, template, encoding, bypass_cache)
        .await
        .into_response()
}

async fn index(
    State(state): State<AppState>,
    account: Option<Account>,
    flashes: Flashes,
    encoding: AcceptEncoding,
    Extension(cacher): Extension<BodyCache>,
    Query(query): Query<ListingQuery>,
) -> Response {
    listing(state, account, flashes, encoding, cacher, query, true).await
}

async fn dramas(
    State(state): State<AppState>,
    account: Option<Account>,
    flashes: Flashes,
    encoding: AcceptEncoding,
    Extension(cacher): Extension<BodyCache>,
    Query(query): Query<ListingQuery>,
) -> Response {
    listing(state, account, flashes, encoding, cacher, query, false).await
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

mod auth;
mod entries;
pub mod utils;

use crate::{AppState, filters, models::Account, ratelimit::RateLimit, utils::HtmlPage};
use askama::Template;
use axum::{
    Json, Router,
    extract::State,
    http::{
        Method,
        header::{AUTHORIZATION, CONTENT_TYPE, USER_AGENT},
    },
    routing::{get, post},
};
use tower_http::cors::{AllowOrigin, CorsLayer};
use utoipa::{
    Modify, OpenApi,
    openapi::security::{ApiKey, ApiKeyValue, SecurityScheme},
};

use crate::error::ApiError;
pub use auth::{ApiToken, copy_api_token};
pub use entries::SearchQuery;

#[derive(OpenApi)]
#[openapi(
    info(
        title = "",
        description = include_str!("../../../templates/api_description.md"),
        version = "beta"
    ),
    paths(
        entries::get_entry_by_id,
        entries::get_entry_files,
        entries::search_entries,
        entries::create_entry,
        entries::upload_files,
    ),
    components(
        schemas(
            ApiError,
            crate::models::EntryFlags,
            crate::models::DirectoryEntry,
            crate::routes::entry::FileEntry,
            crate::routes::entry::UploadResult,
        ),
        responses(utils::RateLimitResponse),
    ),
    modifiers(&RequiredAuthentication),
    tags(
        (name = "entries", description = "Working with entries on the site")
    )
)]
pub struct Schema;

struct RequiredAuthentication;

impl Modify for RequiredAuthentication {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "api_key",
                SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new("Authorization"))),
            )
        }
    }
}

#[derive(Template)]
#[template(path = "api.html")]
struct ApiDocumentation {
    api_key: String,
}

async fn spec(State(state): State<AppState>) -> Json<utoipa::openapi::OpenApi> {
    // The API is named after the site.
    let mut spec = Schema::openapi();
    spec.info.title = state.config().site_name.clone();
    Json(spec)
}

async fn docs(State(state): State<AppState>, account: Option<Account>) -> HtmlPage<ApiDocumentation> {
    let api_key = if let Some(acc) = &account {
        state.get_api_key(acc.id).await.unwrap_or_default()
    } else {
        String::new()
    };
    HtmlPage(ApiDocumentation { api_key })
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/openapi.json", get(spec))
        .route("/docs", get(docs))
        .route("/entries/{id}", get(entries::get_entry_by_id))
        .route("/entries/{id}/files", get(entries::get_entry_files))
        .route("/entries/search", get(entries::search_entries))
        .route("/entries", post(entries::create_entry))
        .route_layer(RateLimit::default().quota(25, 60.0).build())
        .route_layer(cors())
}

/// The upload route of the API. It is apart from the others because an audiobook needs a
/// larger body limit and a longer timeout (see `routes::uploads`).
pub fn upload_routes() -> Router<AppState> {
    Router::new()
        .route("/entries/{id}/upload", post(entries::upload_files))
        .route_layer(RateLimit::default().quota(25, 60.0).build())
        .route_layer(cors())
}

fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_methods([Method::GET, Method::POST])
        .allow_credentials(true)
        .allow_origin(AllowOrigin::mirror_request())
        // Content-Type: a page on another site (subread.space) sends JSON to make an entry.
        .allow_headers([AUTHORIZATION, USER_AGENT, CONTENT_TYPE])
}

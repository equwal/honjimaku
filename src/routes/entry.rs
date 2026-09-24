use crate::anilist::{self, MediaTitle};
use crate::bookcheck;
use crate::database::{Table, is_unique_constraint_violation};
use crate::download::{DownloadResponse, validate_path};
use crate::error::{ApiError, ApiErrorCode, InternalError};
use crate::flash::{FlashMessage, Flasher, Flashes};
use crate::headers::Referrer;
use crate::models::{Account, AccountCheck, DirectoryEntry, EntryFlags, Report, ReportPayload};
use crate::ratelimit::RateLimit;
use crate::subcheck::{self, Script};
use crate::utils::{FRAGMENT, HtmlPage, is_over_length};
use crate::{AppState, tmdb};
use crate::{audit, filters};
use anyhow::{Context, bail};
use askama::Template;
use axum::body::{Body, Bytes};
use axum::extract::multipart::Field;
use axum::extract::{Json, Multipart, Query};
use axum::http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue};
use axum::response::Redirect;
use axum::routing::{delete, get, post, put};
use axum::{
    Router,
    extract::{Form, Path, Request, State},
    response::{IntoResponse, Response},
};
use percent_encoding::percent_encode;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;
use time::OffsetDateTime;
use tokio::task::JoinSet;
use tower::ServiceExt;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeFile;
use utoipa::ToSchema;

/// Represents a file entry, e.g. a subtitle or a ZIP file or whatever else.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct FileEntry {
    /// The file's download URL.
    pub(crate) url: String,
    /// The file's name.
    pub(crate) name: String,
    /// The file's size in bytes.
    pub(crate) size: u64,
    /// The date the file was last modified, in UTC, as an RFC3339 string.
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) last_modified: OffsetDateTime,
}

#[derive(Template)]
#[template(path = "entry.html")]
struct EntryTemplate {
    account: Option<Account>,
    entry: DirectoryEntry,
    bookmarked: bool,
    files: Vec<FileEntry>,
    flashes: Flashes,
}

pub(crate) fn get_file_entries(entry_id: i64, path: &std::path::Path) -> std::io::Result<Vec<FileEntry>> {
    let mut entries = Vec::new();
    for file in path.read_dir()? {
        let entry = file?;
        let filename = entry.file_name();
        let Some(filename) = filename.to_str() else {
            continue;
        };

        let Ok(metadata) = entry.metadata() else { continue };
        let last_modified = if let Ok(time) = metadata.modified() {
            time.into()
        } else {
            OffsetDateTime::UNIX_EPOCH
        };

        let url = format!(
            "/entry/{entry_id}/download/{}",
            percent_encode(filename.as_bytes(), FRAGMENT)
        );
        entries.push(FileEntry {
            url,
            name: filename.into(),
            size: metadata.len(),
            last_modified,
        });
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

async fn get_entry(
    State(state): State<AppState>,
    Path(entry_id): Path<i64>,
    account: Option<Account>,
    flashes: Flashes,
) -> Result<Response, InternalError> {
    let Some(entry) = state.get_directory_entry(entry_id).await else {
        return Ok(Redirect::to("/").into_response());
    };
    let files = get_file_entries(entry_id, &entry.path)?;
    let bookmarked = match account.as_ref() {
        Some(acc) => state.is_bookmarked(acc.id, entry_id).await,
        None => false,
    };
    Ok(HtmlPage(EntryTemplate {
        account,
        entry,
        bookmarked,
        files,
        flashes,
    })
    .into_response())
}

async fn download_entry(
    State(state): State<AppState>,
    Path((entry_id, filename)): Path<(i64, String)>,
    req: Request,
) -> DownloadResponse {
    let Some(base) = state.get_directory_entry_path(entry_id).await else {
        return DownloadResponse::NotFound;
    };

    let Some(path) = validate_path(&base, filename.as_str()) else {
        return DownloadResponse::NotFound;
    };

    let mut service = ServeFile::new(path);
    let ready_service = ServiceExt::<Request>::ready(&mut service).await.unwrap(); // Infallible

    match ready_service.try_call(req).await {
        Ok(res) => DownloadResponse::File(res.map(axum::body::Body::new)),
        Err(_) => DownloadResponse::NotFound,
    }
}

#[derive(Debug, Deserialize)]
struct CreateDirectoryEntry {
    #[serde(deserialize_with = "crate::utils::empty_string_is_none")]
    #[serde(default)]
    anilist_url: Option<String>,
    #[serde(deserialize_with = "crate::utils::empty_string_is_none")]
    #[serde(default)]
    tmdb_url: Option<String>,
    #[serde(deserialize_with = "crate::utils::empty_string_is_none")]
    #[serde(default)]
    name: Option<String>,
    /// On a site for books: the identifier of the audiobook (an Audible ASIN, an audiobook.jp number).
    #[serde(deserialize_with = "crate::utils::empty_string_is_none")]
    #[serde(default)]
    book_id: Option<String>,
    /// On a site for Chinese shows: the Bangumi page or subject number.
    #[serde(deserialize_with = "crate::utils::empty_string_is_none")]
    #[serde(default)]
    bangumi_url: Option<String>,
    /// On a site for books: the ISO 639-1 code of the language of the book.
    #[serde(deserialize_with = "crate::utils::empty_string_is_none")]
    #[serde(default)]
    language: Option<String>,
    #[serde(default = "crate::utils::default_true")]
    anime: bool,
}

#[derive(Debug, Default)]
pub struct PendingDirectoryEntry {
    pub book_id: Option<String>,
    /// On a site for books: the ISO 639-1 code of the language of the book. None is the language of the site.
    pub language: Option<String>,
    pub bangumi_id: Option<u32>,
    pub anilist_id: Option<u32>,
    pub tmdb_id: Option<tmdb::Id>,
    pub name: Option<String>,
    pub titles: Option<MediaTitle>,
    pub flags: Option<EntryFlags>,
    pub notes: Option<String>,
    pub anime: bool,
}

impl From<CreateDirectoryEntry> for PendingDirectoryEntry {
    fn from(value: CreateDirectoryEntry) -> Self {
        Self {
            anilist_id: value.anilist_url.as_deref().and_then(crate::utils::get_anilist_id),
            tmdb_id: value.tmdb_url.as_deref().and_then(tmdb::get_tmdb_id),
            name: value.name,
            book_id: value.book_id,
            language: value.language,
            bangumi_id: value.bangumi_url.as_deref().and_then(crate::bangumi::subject_id),
            anime: value.anime,
            notes: None,
            titles: None,
            flags: None,
        }
    }
}

impl PendingDirectoryEntry {
    async fn get_info(&mut self, state: &AppState) -> anyhow::Result<Option<(MediaTitle, EntryFlags)>> {
        if let Some((title, flags)) = self.titles.as_ref().zip(self.flags) {
            return Ok(Some((title.clone(), flags)));
        }
        match self.anilist_id {
            Some(id) => {
                let media = anilist::search_by_id(&state.client, id)
                    .await
                    .with_context(|| "AniList returned an error. Please try again later.".to_owned())?
                    .with_context(|| "AniList did not return results for this URL.".to_owned())?;
                let mut flags = EntryFlags::new();
                flags.set_anime(self.anime);
                flags.set_movie(media.is_movie());
                flags.set_adult(media.adult);
                Ok(Some((media.title, flags)))
            }
            None => {
                if let Some(id) = self.tmdb_id {
                    let info = tmdb::get_media_info(&state.client, &state.config().tmdb_api_key, id)
                        .await
                        .with_context(|| "TMDB returned an error. Please try again later.".to_owned())?
                        .with_context(|| "TMDB did not return results for this URL.".to_owned())?;

                    let mut flags = EntryFlags::new();
                    flags.set_anime(self.anime);
                    flags.set_movie(id.is_movie());
                    flags.set_adult(info.is_adult());
                    Ok(Some((info.titles(), flags)))
                } else if let Some(id) = self.bangumi_id {
                    // A Chinese show: Bangumi names it and says whether it is animated.
                    let show = crate::bangumi::lookup(&state.client, id)
                        .await
                        .with_context(|| "Bangumi did not answer. Try again later.".to_owned())?
                        .map_err(anyhow::Error::msg)?;
                    let mut flags = EntryFlags::new();
                    flags.set_anime(show.animated);
                    flags.set_adult(show.nsfw);
                    let title = show.title().to_owned();
                    let native = (show.name != title).then(|| show.name.clone());
                    self.notes.get_or_insert_with(|| show.note());
                    let titles = MediaTitle {
                        romaji: title,
                        english: None,
                        native,
                    };
                    Ok(Some((titles, flags)))
                } else {
                    Ok(None)
                }
            }
        }
    }

    fn path(&self, name: &str, anime: bool, state: &AppState) -> PathBuf {
        let ids = PathIds {
            anilist_id: self.anilist_id,
            tmdb_id: self.tmdb_id,
            book_id: self.book_id.as_deref(),
            bangumi_id: self.bangumi_id,
        };
        directory_entry_path(ids, name, anime, state)
    }
}

/// The IDs that go into the name of the folder of an entry. The first ID that is set is used.
pub struct PathIds<'a> {
    pub anilist_id: Option<u32>,
    pub tmdb_id: Option<tmdb::Id>,
    pub book_id: Option<&'a str>,
    pub bangumi_id: Option<u32>,
}

pub fn directory_entry_path(ids: PathIds<'_>, name: &str, anime: bool, state: &AppState) -> PathBuf {
    let PathIds {
        anilist_id,
        tmdb_id,
        book_id,
        bangumi_id,
    } = ids;
    // Series names aren't unique but directory names are
    // So try to give it some noise depending on the anilist ID or tmdb ID
    // This ordeal could also be entirely avoided by just using numeric folder names
    // But having human readable folder names is fine

    // A prefix is used for the flat directory structure since it's easier to
    // reason about in the code.
    let prefix = if !anime { "[drama] " } else { "" };
    let directory_name = if let Some(id) = anilist_id {
        sanitise_file_name::sanitise(&format!("{prefix}{name} [{id}]"))
    } else if let Some(id) = tmdb_id {
        sanitise_file_name::sanitise(&format!("{prefix}{name} [{id}]"))
    } else if let Some(id) = book_id {
        sanitise_file_name::sanitise(&format!("{name} [{id}]"))
    } else if let Some(id) = bangumi_id {
        sanitise_file_name::sanitise(&format!("{name} [bgm-{id}]"))
    } else {
        // Avoid the extra allocation if possible
        if anime {
            sanitise_file_name::sanitise(name)
        } else {
            sanitise_file_name::sanitise(&format!("[drama] {name}"))
        }
    };

    state.config().subtitle_path.join(directory_name)
}

pub async fn raw_create_directory_entry(
    state: &AppState,
    account: Account,
    pending: PendingDirectoryEntry,
    api: bool,
) -> Result<(i64, PathBuf), ApiError> {
    let creator_id = account.id;

    if account.flags.is_restricted() {
        return Err(ApiError::new("Account is restricted from uploading").with_code(ApiErrorCode::NoPermissions));
    }

    let mut pending = pending;
    let book_site = state.config().book_site;
    if book_site && pending.anilist_id.is_none() && pending.tmdb_id.is_none() && pending.titles.is_none() {
        // A book: the user names it. Say so if the site has it already, so that the
        // subtitles of one book do not end up in two places.
        let mut title = crate::book::clean_title(pending.name.as_deref().unwrap_or("")).map_err(ApiError::new)?;
        // The book goes in the tab of its language.
        let language = match pending.language.as_deref() {
            None => state.config().default_language(),
            Some(raw) => crate::language::code(raw)
                .ok_or_else(|| ApiError::new(format!("\"{raw}\" is not an ISO 639-1 language code.")))?,
        }
        .to_owned();
        // An Audible ASIN is verified: Audible says the book exists, what it is named, and
        // in which language it is read. The entry takes the name of the shop and is verified.
        // The user may paste the Audible URL; only the ASIN in it is kept.
        let asin = pending.book_id.as_deref().and_then(crate::audible::asin);
        if let Some(id) = pending.book_id.take() {
            pending.book_id = Some(match &asin {
                Some(asin) => asin.clone(),
                None => crate::book::clean_book_id(&id).map_err(ApiError::new)?,
            });
        }
        let entries = state.directory_entries().await;
        // One audiobook, one entry: the identifier says which book it is better than the title does.
        if let Some(id) = &pending.book_id {
            if let Some(same) = entries.iter().find(|e| e.book_id.as_ref() == Some(id)) {
                return Err(ApiError::new(format!(
                    "This audiobook is here already: \"{}\" (/entry/{}). Upload your subtitles there.",
                    same.name, same.id
                ))
                .with_code(ApiErrorCode::EntryAlreadyExists));
            }
        }
        let audiobook = match &asin {
            Some(asin) => {
                let audiobook = crate::audible::lookup(&state.client, asin, &language)
                    .await
                    .map_err(|e| {
                        tracing::warn!(error = %e, asin, "Audible did not answer");
                        ApiError::new("Audible did not answer. Try again later.").with_code(ApiErrorCode::ServerError)
                    })?
                    .ok_or_else(|| ApiError::new(format!("Audible does not know an audiobook {asin}.")))?;
                if let Some(spoken) = audiobook.language.as_deref() {
                    if !crate::language::is_audible_language(&language, spoken) {
                        return Err(ApiError::new(format!(
                            "That audiobook is in {spoken}, not in {}. Choose its language, then add it again.",
                            crate::language::name(&language)
                        )));
                    }
                }
                title = crate::book::clean_title(&audiobook.title).map_err(ApiError::new)?;
                Some(audiobook)
            }
            None => None,
        };
        let key = crate::book::title_key(&title);
        if key.is_empty() {
            return Err(ApiError::new("Give the title of the book."));
        }
        if let Some(same) = entries.iter().find(|e| crate::book::directory_key(&e.name) == key) {
            return Err(ApiError::new(format!(
                "This book is here already: \"{}\" (/entry/{}). Upload your subtitles there.",
                same.name, same.id
            ))
            .with_code(ApiErrorCode::EntryAlreadyExists));
        }
        pending.anime = true; // the listing on the front page
        match audiobook {
            Some(audiobook) => {
                let mut flags = EntryFlags::new();
                flags.set_adult(audiobook.adult);
                pending.flags = Some(flags);
                pending.titles = Some(MediaTitle {
                    romaji: title.clone(),
                    english: None,
                    native: Some(title.clone()),
                });
                pending.notes.get_or_insert_with(|| audiobook.note());
            }
            None => {
                pending.notes.get_or_insert_with(|| match &pending.book_id {
                    Some(id) => format!("It is a book. Audiobook: {id}"),
                    None => "It is a book".to_owned(),
                });
            }
        }
        pending.name = Some(title);
        pending.language = Some(language);
    } else {
        pending.language = None;
    }

    // One show, one entry: say where it is if the site has it already.
    if let Some(id) = pending.bangumi_id {
        let entries = state.directory_entries().await;
        if let Some(same) = entries.iter().find(|e| e.bangumi_id == Some(id)) {
            return Err(ApiError::new(format!(
                "This show is here already: \"{}\" (/entry/{}). Upload your subtitles there.",
                same.name, same.id
            ))
            .with_code(ApiErrorCode::EntryAlreadyExists));
        }
    }
    if state.config().drama_site && pending.bangumi_id.is_none() && !account.flags.is_editor() {
        return Err(ApiError::new(
            "Give the Bangumi page of the show (https://bgm.tv/subject/...).",
        ));
    }

    let (names, flags) = match pending.get_info(state).await? {
        Some(title) => title,
        None if account.flags.is_editor() || book_site => {
            if let Some(name) = pending.name.clone() {
                let mut flags = EntryFlags::new();
                flags.set_anime(pending.anime);
                // An editor has looked at what an editor makes. Nobody has looked at the rest yet.
                flags.set_unverified(!account.flags.is_editor());
                (MediaTitle::new(name), flags)
            } else {
                return Err(ApiError::new("Missing name, anilist_id, or tmdb_id for directory."));
            }
        }
        None => return Err(ApiError::new("Missing anilist_id or tmdb_id for directory.")),
    };

    let path = pending.path(&names.romaji, pending.anime, state);
    if path.exists() {
        return Err(ApiError::new("Path already exists.").with_code(ApiErrorCode::EntryAlreadyExists));
    }

    let Some(path_string) = path.to_str() else {
        return Err(ApiError::new("Resulting path was not UTF-8."));
    };

    let query = r#"
        INSERT INTO directory_entry(path, creator_id, tmdb_id, anilist_id, flags, notes, name, english_name, japanese_name, book_id, bangumi_id, language)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        RETURNING id;
    "#;
    let path_string = path_string.to_owned();
    let romaji = names.romaji.clone();
    let book_id = pending.book_id.clone();
    let response = state
        .database()
        .call(move |con| -> Result<(i64, PathBuf), ApiError> {
            let tx = con.transaction()?;
            let result: rusqlite::Result<i64> = {
                let mut stmt = tx.prepare_cached(query)?;
                stmt.query_row(
                    (
                        path_string.to_owned(),
                        creator_id,
                        pending.tmdb_id,
                        pending.anilist_id,
                        flags,
                        pending.notes,
                        names.romaji,
                        names.english,
                        names.native,
                        pending.book_id,
                        pending.bangumi_id,
                        pending.language,
                    ),
                    |row| row.get("id"),
                )
            };

            let url = match result {
                Ok(entry_id) => {
                    std::fs::create_dir(&path).map_err(|_| {
                        ApiError::new(format!("Could not create directory {}", path.display()))
                            .with_code(ApiErrorCode::ServerError)
                    })?;
                    (entry_id, path)
                }
                Err(e) if is_unique_constraint_violation(&e) => {
                    return Err(ApiError::new("Entry already exists.").with_code(ApiErrorCode::EntryAlreadyExists));
                }
                Err(e) => return Err(e.into()),
            };

            tx.commit()?;
            Ok(url)
        })
        .await;

    if let Ok((entry_id, _)) = &response {
        let audit_data = audit::CreateEntry {
            anime: pending.anime,
            api,
            name: romaji.clone(),
            tmdb_id: pending.tmdb_id,
            anilist_id: pending.anilist_id,
        };
        state
            .audit(audit::AuditLogEntry::full(audit_data, *entry_id, account.id))
            .await;
        let anilist_url = match pending.anilist_id {
            Some(id) => format!("https://anilist.co/anime/{id}"),
            None => String::from("Unknown"),
        };
        let tmdb_url = match pending.tmdb_id {
            Some(id) => id.url(),
            None => String::from("Unknown"),
        };
        let title = if api {
            format!("[API] New Entry: {romaji}")
        } else {
            format!("New Entry: {romaji}")
        };
        let alert = crate::discord::Alert::success(title)
            .url(format!("/entry/{entry_id}"))
            .account(account);
        let alert = if book_site {
            alert.field("Audiobook", book_id.unwrap_or_else(|| String::from("Unknown")))
        } else if state.config().drama_site {
            let bangumi = pending.bangumi_id.map(crate::bangumi::url);
            alert.field("Bangumi", bangumi.unwrap_or_else(|| String::from("Unknown")))
        } else {
            alert
                .field("Anime", pending.anime)
                .field("AniList URL", anilist_url)
                .field("TMDB URL", tmdb_url)
        };
        state.send_alert(alert);
        state.cached_directories().invalidate().await;
    }
    response
}

async fn create_directory_entry(
    State(state): State<AppState>,
    account: Account,
    flasher: Flasher,
    Referrer(url): Referrer,
    Form(payload): Form<CreateDirectoryEntry>,
) -> Response {
    let response = raw_create_directory_entry(&state, account, payload.into(), false).await;
    match response {
        Ok((entry_id, _)) => Redirect::to(&format!("/entry/{entry_id}")).into_response(),
        Err(e) => flasher.add(e.error.as_ref()).bail(&url),
    }
}

#[derive(Deserialize)]
struct EditDirectoryEntry {
    name: String,
    #[serde(deserialize_with = "crate::utils::empty_string_is_none")]
    japanese_name: Option<String>,
    #[serde(deserialize_with = "crate::utils::empty_string_is_none")]
    english_name: Option<String>,
    #[serde(deserialize_with = "anilist_id_or_url")]
    anilist_id: Option<u32>,
    #[serde(deserialize_with = "crate::utils::empty_string_is_none")]
    notes: Option<String>,
    #[serde(rename = "tmdb_url", deserialize_with = "tmdb_url")]
    tmdb_id: Option<tmdb::Id>,
    /// On a site for books: the identifier of the audiobook. An ASIN is verified against Audible.
    #[serde(default, deserialize_with = "crate::utils::empty_string_is_none")]
    book_id: Option<String>,
    /// On a site for Chinese shows: the Bangumi page or subject number, verified against Bangumi.
    #[serde(default, deserialize_with = "bangumi_id_or_url")]
    bangumi_id: Option<u32>,
    #[serde(default)]
    unverified: bool,
    #[serde(default)]
    adult: bool,
    #[serde(default)]
    movie: bool,
    #[serde(default)]
    anime: bool,
}

impl EditDirectoryEntry {
    fn apply_flags(&self, mut flags: EntryFlags) -> EntryFlags {
        flags.set_unverified(self.unverified);
        flags.set_adult(self.adult);
        flags.set_movie(self.movie);
        flags.set_anime(self.anime);
        flags
    }

    fn titles(self) -> MediaTitle {
        MediaTitle {
            romaji: self.name,
            english: self.english_name,
            native: self.japanese_name,
        }
    }

    fn validate(&self) -> Vec<&'static str> {
        let mut errors = Vec::new();
        if self.name.len() > 1024 {
            errors.push("Name cannot be more than 1024 bytes.");
        }
        if is_over_length(&self.english_name, 1024) {
            errors.push("English name cannot be more than 1024 bytes.");
        }

        if is_over_length(&self.japanese_name, 1024) {
            errors.push("Japanese name cannot be more than 1024 bytes.");
        }

        if is_over_length(&self.notes, 2048) {
            errors.push("Notes cannot be more than 2048 bytes.");
        }
        errors
    }
}

fn anilist_id_or_url<'de, D>(de: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(de)?;
    let opt = opt.as_deref();
    match opt {
        None | Some("") => Ok(None),
        Some(s) => {
            if s.chars().all(|x| x.is_ascii_digit()) {
                s.parse::<u32>().map(Some).map_err(serde::de::Error::custom)
            } else {
                crate::utils::get_anilist_id(s)
                    .ok_or_else(|| serde::de::Error::custom("Invalid anilist ID or URL provided"))
                    .map(Some)
            }
        }
    }
}

fn bangumi_id_or_url<'de, D>(de: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(de)?;
    match opt.as_deref() {
        None | Some("") => Ok(None),
        Some(s) => crate::bangumi::subject_id(s)
            .ok_or_else(|| serde::de::Error::custom("Invalid Bangumi subject number or URL provided"))
            .map(Some),
    }
}

fn tmdb_url<'de, D>(de: D) -> Result<Option<tmdb::Id>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(de)?;
    let opt = opt.as_deref();
    match opt {
        None | Some("") => Ok(None),
        Some(s) => tmdb::get_tmdb_id(s)
            .ok_or_else(|| serde::de::Error::custom("Invalid TMDB URL provided"))
            .map(Some),
    }
}

async fn edit_directory_entry(
    State(state): State<AppState>,
    Path(entry_id): Path<i64>,
    account: Account,
    flasher: Flasher,
    Referrer(url): Referrer,
    Form(payload): Form<EditDirectoryEntry>,
) -> Response {
    if !account.flags.is_editor() {
        return flasher.add("You do not have permissions to edit this.").bail(&url);
    }

    let Some(entry) = state.get_directory_entry(entry_id).await else {
        return flasher.add("Directory entry not found.").bail(&url);
    };

    let errors = payload.validate();
    if !errors.is_empty() {
        for error in errors {
            flasher.add(error);
        }
        return Redirect::to(&url).into_response();
    }

    let mut payload = payload;
    if let Some(id) = payload.book_id.take() {
        // A new ASIN must be one that Audible knows. The user may paste the Audible URL.
        payload.book_id = match crate::audible::asin(&id) {
            Some(asin) if entry.book_id.as_deref() != Some(&asin) => {
                let language = entry.language_code(state.config());
                match crate::audible::lookup(&state.client, &asin, language).await {
                    Ok(Some(_)) => Some(asin),
                    Ok(None) => {
                        return flasher
                            .add(format!("Audible does not know an audiobook {asin}."))
                            .bail(&url);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, asin, "Audible did not answer");
                        return flasher.add("Audible did not answer. Try again later.").bail(&url);
                    }
                }
            }
            Some(asin) => Some(asin),
            None => match crate::book::clean_book_id(&id) {
                Ok(id) => Some(id),
                Err(e) => return flasher.add(e).bail(&url),
            },
        };
    }
    // A new Bangumi subject must be one that Bangumi knows.
    if let Some(id) = payload.bangumi_id.filter(|id| entry.bangumi_id != Some(*id)) {
        match crate::bangumi::lookup(&state.client, id).await {
            Ok(Ok(_)) => {}
            Ok(Err(why)) => return flasher.add(why).bail(&url),
            Err(e) => {
                tracing::warn!(error = %e, id, "Bangumi did not answer");
                return flasher.add("Bangumi did not answer. Try again later.").bail(&url);
            }
        }
    }

    // maybe refactor this?
    let mut columns = Vec::with_capacity(11);
    let mut params: Vec<Box<dyn rusqlite::ToSql + Send>> = Vec::with_capacity(12);
    let mut audit_data = audit::EditEntry::default();
    let flags = payload.apply_flags(entry.flags);
    let mut changed_path: Option<PathBuf> = None;

    if !flags.is_external() && (entry.anilist_id != payload.anilist_id || entry.tmdb_id != payload.tmdb_id) {
        // Change the internal path if the path bound data is changed...
        columns.push("path");
        // A book or a Bangumi entry keeps its ID in the folder name, the same as when it was made.
        let ids = PathIds {
            anilist_id: payload.anilist_id,
            tmdb_id: payload.tmdb_id,
            book_id: payload.book_id.as_deref().or(entry.book_id.as_deref()),
            bangumi_id: payload.bangumi_id.or(entry.bangumi_id),
        };
        let path = directory_entry_path(ids, payload.name.as_str(), flags.is_anime(), &state);
        if path.exists() {
            return flasher.add("Path already exists").bail(&url);
        }

        let path_string = path.to_str().map(|p| p.to_owned());
        let Some(path_string) = path_string else {
            return flasher.add("Resulting path was somehow not UTF-8").bail(&url);
        };

        changed_path = Some(path);
        params.push(Box::new(path_string));
    }

    if entry.name != payload.name {
        columns.push("name");
        audit_data.before.name = Some(entry.name);
        audit_data.after.name = Some(payload.name.clone());
        params.push(Box::new(payload.name));
    }
    if entry.japanese_name != payload.japanese_name {
        columns.push("japanese_name");
        audit_data.before.japanese_name = entry.japanese_name;
        audit_data.after.japanese_name = payload.japanese_name.clone();
        params.push(Box::new(payload.japanese_name));
    }
    if entry.english_name != payload.english_name {
        columns.push("english_name");
        audit_data.before.english_name = entry.english_name;
        audit_data.after.english_name = payload.english_name.clone();
        params.push(Box::new(payload.english_name));
    }
    if entry.anilist_id != payload.anilist_id {
        columns.push("anilist_id");
        audit_data.before.anilist_id = entry.anilist_id;
        audit_data.after.anilist_id = payload.anilist_id;
        params.push(Box::new(payload.anilist_id));
    }
    if entry.tmdb_id != payload.tmdb_id {
        columns.push("tmdb_id");
        audit_data.before.tmdb_id = entry.tmdb_id;
        audit_data.after.tmdb_id = payload.tmdb_id;
        params.push(Box::new(payload.tmdb_id));
    }
    if entry.book_id != payload.book_id {
        columns.push("book_id");
        audit_data.before.book_id = entry.book_id;
        audit_data.after.book_id = payload.book_id.clone();
        params.push(Box::new(payload.book_id));
    }
    if entry.bangumi_id != payload.bangumi_id {
        columns.push("bangumi_id");
        audit_data.before.bangumi_id = entry.bangumi_id;
        audit_data.after.bangumi_id = payload.bangumi_id;
        params.push(Box::new(payload.bangumi_id));
    }
    if entry.notes != payload.notes {
        columns.push("notes");
        audit_data.before.notes = entry.notes;
        audit_data.after.notes = payload.notes.clone();
        params.push(Box::new(payload.notes));
    }
    if entry.flags != flags {
        columns.push("flags");
        audit_data.before.flags = Some(entry.flags);
        audit_data.after.flags = Some(flags);
        params.push(Box::new(flags));
    }

    if !columns.is_empty() {
        params.push(Box::new(entry_id));
        let query = DirectoryEntry::update_query(&columns);
        audit_data.changed = columns.into_iter().map(String::from).collect();
        match state
            .database()
            .execute(query, rusqlite::params_from_iter(params))
            .await
        {
            Ok(_) => {
                if let Some(path) = changed_path {
                    // Potential issue when this fails and the database points to the new path
                    // For now, just ignore the error. In the future, consider rewriting this
                    // to use a transaction instead should it become an issue.
                    let _ = tokio::fs::rename(entry.path, path).await;
                }
                state.cached_directories().invalidate().await;
                state
                    .audit(audit::AuditLogEntry::full(audit_data, entry_id, account.id))
                    .await;
                flasher.add(FlashMessage::success("Successfully edited entry."));
                Redirect::to(&url).into_response()
            }
            Err(rusqlite::Error::SqliteFailure(error, Some(s)))
                if error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
            {
                if let Some(suffix) = s.strip_prefix("UNIQUE constraint failed: directory_entry.") {
                    flasher
                        .add(format!("An entry already exists with this {suffix} field."))
                        .bail(&url)
                } else {
                    flasher
                        .add("An entry already exists with one of these fields.")
                        .bail(&url)
                }
            }
            Err(e) => flasher.add(format!("SQL Error: {e}")).bail(&url),
        }
    } else {
        Redirect::to(&url).into_response()
    }
}

#[derive(Deserialize)]
struct SearchQueryParams {
    #[serde(default)]
    anilist_id: Option<u32>,
    #[serde(default)]
    tmdb_id: Option<String>,
    #[serde(default)]
    book_id: Option<String>,
    #[serde(default)]
    bangumi_id: Option<u32>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Serialize)]
struct SearchResult {
    entry_id: i64,
}

async fn search_directory_entries(
    State(state): State<AppState>,
    account: Account,
    Query(params): Query<SearchQueryParams>,
) -> Result<Json<SearchResult>, ApiError> {
    if !account.flags.is_editor() {
        return Err(ApiError::forbidden());
    }

    if params.anilist_id.is_none()
        && params.name.is_none()
        && params.tmdb_id.is_none()
        && params.book_id.is_none()
        && params.bangumi_id.is_none()
    {
        return Err(ApiError::new("Missing search parameter"));
    }

    let path = params
        .name
        .as_deref()
        .map(sanitise_file_name::sanitise)
        .and_then(|x| state.config().subtitle_path.join(x).to_str().map(String::from));
    let book_id = params
        .book_id
        .as_deref()
        .and_then(crate::audible::asin)
        .or(params.book_id);

    let entry = state
        .database()
        .get_row(
            "SELECT id FROM directory_entry WHERE anilist_id = ? OR tmdb_id = ? OR book_id = ? OR bangumi_id = ? OR name = ? OR path = ?",
            (params.anilist_id, params.tmdb_id, book_id, params.bangumi_id, params.name, path),
            |row| row.get(0),
        )
        .await
        .optional()?;
    match entry {
        Some(entry_id) => Ok(Json(SearchResult { entry_id })),
        None => Err(ApiError::not_found("Entry not found.")),
    }
}

#[derive(Deserialize)]
struct MoveDirectoryEntries {
    #[serde(default)]
    anilist_id: Option<u32>,
    #[serde(default, rename = "tmdb")]
    tmdb_id: Option<tmdb::Id>,
    #[serde(default)]
    book_id: Option<String>,
    #[serde(default)]
    bangumi_id: Option<u32>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    entry_id: Option<i64>,
    #[serde(default = "crate::utils::default_true")]
    anime: bool,
    files: Vec<String>,
}

#[derive(Serialize)]
struct BulkFileOperationResponse {
    entry_id: i64,
    success: usize,
    failed: usize,
}

async fn move_directory_entries(
    State(state): State<AppState>,
    Path(from_entry_id): Path<i64>,
    account: Account,
    Json(payload): Json<MoveDirectoryEntries>,
) -> Result<Json<BulkFileOperationResponse>, ApiError> {
    if !account.flags.is_editor() {
        return Err(ApiError::forbidden());
    }

    let Some(entry) = state.get_directory_entry_path(from_entry_id).await else {
        return Err(ApiError::not_found("Directory entry not found."));
    };
    let (entry_id, path, mut audit_data) = match payload.entry_id {
        Some(entry_id) => {
            let Some(path) = state.get_directory_entry_path(entry_id).await else {
                return Err(ApiError::not_found(format!("Directory entry {entry_id} not found.")));
            };
            (entry_id, path, audit::MoveEntry::new(entry_id))
        }
        None => {
            let (entry_id, path) = raw_create_directory_entry(
                &state,
                account.clone(),
                PendingDirectoryEntry {
                    book_id: payload.book_id.clone(),
                    language: None,
                    bangumi_id: payload.bangumi_id,
                    anilist_id: payload.anilist_id,
                    tmdb_id: payload.tmdb_id,
                    name: payload.name.clone(),
                    anime: payload.anime,
                    titles: None,
                    flags: None,
                    notes: None,
                },
                false,
            )
            .await?;
            let audit_data = audit::MoveEntry {
                anime: payload.anime,
                name: payload.name.clone(),
                tmdb_id: payload.tmdb_id,
                anilist_id: payload.anilist_id,
                entry_id,
                created: true,
                files: Vec::new(),
            };
            (entry_id, path, audit_data)
        }
    };

    let mut success = 0;
    let mut failed = 0;
    audit_data.files.reserve(payload.files.len());
    for file in payload.files {
        let from = entry.join(&file);
        let to = path.join(&file);
        let error = to.exists() || tokio::fs::rename(from, to).await.is_err();
        audit_data.add_file(file, error);
        if error {
            failed += 1;
        } else {
            success += 1;
        }
    }

    let _ = state
        .database()
        .execute(
            "UPDATE directory_entry SET last_updated_at = CURRENT_TIMESTAMP WHERE id = ?",
            [entry_id],
        )
        .await;

    state.cached_directories().invalidate().await;
    state
        .audit(audit::AuditLogEntry::full(audit_data, from_entry_id, account.id))
        .await;
    Ok(Json(BulkFileOperationResponse {
        entry_id,
        success,
        failed,
    }))
}

#[derive(Deserialize)]
struct BulkFilesPayload {
    files: Vec<String>,
    #[serde(default)]
    delete_parent: bool,
    #[serde(default)]
    reason: Option<String>,
}

async fn bulk_delete_files(
    State(state): State<AppState>,
    Path(entry_id): Path<i64>,
    account: Account,
    Json(payload): Json<BulkFilesPayload>,
) -> Result<Json<BulkFileOperationResponse>, ApiError> {
    if !account.flags.is_editor() {
        return Err(ApiError::forbidden());
    }

    let Some(entry) = state.get_directory_entry_path(entry_id).await else {
        return Err(ApiError::not_found("Directory entry not found."));
    };

    if !account.flags.is_admin() && payload.reason.is_none() {
        return Err(ApiError::new("Reason must be provided"));
    }

    if let Some(reason) = payload.reason.as_deref() {
        if reason.is_empty() {
            return Err(ApiError::new("Reason cannot be empty"));
        }
        if reason.len() > 512 {
            return Err(ApiError::new("Reason can only be up to 512 characters long"));
        }
    }

    let mut success = 0;
    let mut failed = 0;
    if payload.delete_parent {
        if !account.flags.is_admin() {
            return Err(ApiError::forbidden());
        }
        let name = state
            .database()
            .get_row(
                "DELETE FROM directory_entry WHERE id = ? RETURNING name",
                [entry_id],
                |r| r.get("name"),
            )
            .await?;
        state.cached_directories().invalidate().await;
        // state
        //     .database()
        //     .execute("UPDATE report SET entry_id = NULL WHERE entry_id = ?", [entry_id])
        //     .await?;
        let result = tokio::fs::remove_dir_all(entry).await;
        state
            .audit(
                audit::AuditLogEntry::new(audit::DeleteEntry {
                    name,
                    failed: result.is_err(),
                })
                .with_account(account.id),
            )
            .await;
        result?;
    } else {
        let trash = crate::trash::Trash::new()?;
        let mut audit_data = audit::DeleteFiles {
            permanent: account.flags.is_admin(),
            files: Vec::with_capacity(payload.files.len()),
            reason: payload.reason.clone(),
        };
        let total = payload.files.len();
        let description = crate::utils::join_iter("\n", payload.files.iter().map(|x| format!("- {x}")).take(25));
        for file in payload.files {
            let path = entry.join(&file);
            let result = if account.flags.is_admin() {
                tokio::fs::remove_file(path).await
            } else {
                trash.put(path, entry_id, payload.reason.clone()).await
            };
            audit_data.add_file(file, result.is_err());
            match result {
                Ok(_) => success += 1,
                Err(_) => failed += 1,
            }
        }
        state
            .audit(audit::AuditLogEntry::full(audit_data, entry_id, account.id))
            .await;
        state.send_alert(
            crate::discord::Alert::error("Deleted Files")
                .url(format!("/logs?entry_id={entry_id}"))
                .description(description)
                .account(account)
                .field("Reason", payload.reason.as_deref().unwrap_or("None"))
                .field("Total", total)
                .field("Failed", failed),
        );
    }

    Ok(Json(BulkFileOperationResponse {
        entry_id,
        success,
        failed,
    }))
}

#[derive(Deserialize)]
struct ReportEntryPayload {
    files: Vec<String>,
    reason: String,
}

async fn report_entry(
    State(state): State<AppState>,
    Path(entry_id): Path<i64>,
    account: Account,
    Json(payload): Json<ReportEntryPayload>,
) -> Result<(), ApiError> {
    let Some(entry) = state.get_directory_entry(entry_id).await else {
        return Err(ApiError::not_found("Directory entry not found."));
    };

    if payload.reason.is_empty() {
        return Err(ApiError::new("Reason cannot be empty"));
    }
    if payload.reason.len() > 512 {
        return Err(ApiError::new("Reason can only be up to 512 characters long"));
    }

    if payload.files.len() > 100 {
        return Err(ApiError::new("You can only report up to 100 files"));
    }

    let account_id = account.id;
    let report = Report::full(
        payload.reason,
        ReportPayload {
            files: payload.files,
            name: entry.name.clone(),
        },
        entry_id,
        account_id,
    );

    let mut alert = crate::discord::Alert::error(format!("Entry Reported: {}", entry.name))
        .url(format!("/entry/{entry_id}"))
        .field("Reason", &report.reason)
        .account(account);

    if report.payload.files.is_empty() {
        state
            .audit(audit::AuditLogEntry::full(
                audit::ReportEntry {
                    report_id: Some(report.id),
                    name: entry.name,
                    reason: report.reason.clone(),
                },
                entry_id,
                account_id,
            ))
            .await;
    } else {
        let description = crate::utils::join_iter("\n", report.payload.files.iter().map(|x| format!("- {x}")).take(25));
        alert = alert.description(description);
        state
            .audit(audit::AuditLogEntry::full(
                audit::ReportFiles {
                    report_id: Some(report.id),
                    files: report.payload.files.clone(),
                    reason: report.reason.clone(),
                },
                entry_id,
                account_id,
            ))
            .await;
    }

    state.send_alert(alert);
    state
        .database()
        .execute(
            "INSERT INTO report(id, account_id, entry_id, status, reason, payload) VALUES (?, ?, ?, ?, ?, ?)",
            (
                report.id,
                report.account_id,
                report.entry_id,
                report.status,
                report.reason,
                report.payload,
            ),
        )
        .await?;
    state.notifications.notify_new_report(report.id);

    Ok(())
}

#[derive(Deserialize)]
struct RenameFileRequest {
    from: String,
    to: String,
}

async fn bulk_rename_files(
    State(state): State<AppState>,
    Path(entry_id): Path<i64>,
    account: Account,
    Json(files): Json<Vec<RenameFileRequest>>,
) -> Result<Json<BulkFileOperationResponse>, ApiError> {
    if !account.flags.is_editor() {
        return Err(ApiError::forbidden());
    }

    let Some(entry) = state.get_directory_entry_path(entry_id).await else {
        return Err(ApiError::not_found("Directory entry not found."));
    };

    let mut data = audit::RenameFiles {
        files: Vec::with_capacity(files.len()),
    };
    let mut success = 0;
    let mut failed = 0;
    for file in files {
        let from = entry.join(&file.from);
        let to = entry.join(&file.to);
        let errored = to.exists() || tokio::fs::rename(from, to).await.is_err();
        data.add_file(file.from, file.to, errored);
        if errored {
            failed += 1;
        } else {
            success += 1;
        }
    }

    state
        .audit(audit::AuditLogEntry::full(data, entry_id, account.id))
        .await;
    Ok(Json(BulkFileOperationResponse {
        entry_id,
        success,
        failed,
    }))
}

/// A file that an upload wrote to the staging folder. Drop deletes the file. After
/// `write_to_disk`, the entry holds a second link to the same data, so the upload stays.
#[derive(Debug)]
struct Staged(PathBuf);

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Debug)]
enum Contents {
    /// A subtitle file or a zip: small, so it is kept in memory.
    Bytes(Bytes),
    /// A book or an audiobook: too large for memory, so it is on the disk.
    Staged(Staged),
}

#[derive(Debug)]
struct ProcessedFile {
    path: PathBuf,
    contents: Contents,
    /// Subtitles that passed `subcheck`. A book or an audiobook needs such a file.
    passed_subcheck: bool,
    /// A book or an audiobook.
    book: bool,
}

impl ProcessedFile {
    fn write_to_disk(self) -> std::io::Result<()> {
        match self.contents {
            Contents::Bytes(bytes) => {
                let mut fp = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(self.path)?;
                fp.write_all(&bytes)?;
            }
            // A hard link does not replace a file that is already there, the same as `create_new`.
            Contents::Staged(staged) => std::fs::hard_link(&staged.0, self.path)?,
        }
        Ok(())
    }
}

/// Why a book or an audiobook is refused when no subtitles in the entry pass the check.
const NEEDS_SUBTITLES: &str = "a book or an audiobook needs subtitles in this entry that pass the check. \
    Upload the .srt first, or in the same upload.";

/// True when a subtitle file in the entry folder passes `subcheck`.
fn has_good_subtitles(entry_path: &std::path::Path, script: Script) -> bool {
    let Ok(dir) = entry_path.read_dir() else {
        return false;
    };
    dir.flatten().any(|file| {
        let path = file.path();
        let Some(format) = path
            .extension()
            .and_then(|ext| ext.to_str())
            .and_then(subcheck::Format::from_extension)
        else {
            return false;
        };
        file.metadata()
            .is_ok_and(|m| m.is_file() && m.len() <= subcheck::MAX_BYTES as u64)
            && std::fs::read(&path).is_ok_and(|bytes| subcheck::check(&bytes, format, script).is_ok())
    })
}

/// Reads a field into memory, and refuses it when it is larger than `limit`.
async fn read_capped(mut field: Field<'_>, limit: u64) -> anyhow::Result<Bytes> {
    let mut bytes = Vec::new();
    while let Some(chunk) = field.chunk().await? {
        if (bytes.len() + chunk.len()) as u64 > limit {
            bail!("the file is larger than {} MB", limit / 1024 / 1024);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes.into())
}

/// Writes a field to a new file in `staging`, and refuses it when it is larger than `limit`.
async fn stage(staging: &std::path::Path, mut field: Field<'_>, limit: u64) -> anyhow::Result<Staged> {
    use tokio::io::AsyncWriteExt;
    tokio::fs::create_dir_all(staging).await?;
    let mut random = [0u8; 12];
    getrandom::getrandom(&mut random)?;
    let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let staged = Staged(staging.join(format!("{name}.part")));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged.0)
        .await?;
    let mut written = 0u64;
    while let Some(chunk) = field.chunk().await? {
        written += chunk.len() as u64;
        if written > limit {
            bail!("the file is larger than {} MB", limit / 1024 / 1024);
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(staged)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingFileEntry {
    pub name: String,
    #[serde(with = "crate::utils::base64_bytes")]
    pub data: Vec<u8>,
}

impl PendingFileEntry {
    pub fn write_to_disk(&self, base_path: PathBuf) -> std::io::Result<()> {
        let path = base_path.join(sanitise_file_name::sanitise(&self.name));
        let mut fp = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
        fp.write_all(&self.data)?;
        Ok(())
    }
}

struct ProcessedFiles {
    files: Vec<ProcessedFile>,
    skipped: usize,
    /// Why each skipped file was skipped, for the uploader to read.
    problems: Vec<String>,
}

/// The members of a zip are checked like files uploaded one by one. A zip may hold
/// subtitle files only, so that nothing else gets onto the site inside one.
fn verify_zip(bytes: &[u8], script: Script) -> anyhow::Result<()> {
    const MAX_MEMBERS: usize = 500;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    if archive.len() > MAX_MEMBERS {
        bail!("the zip holds more than {MAX_MEMBERS} files");
    }
    let mut subtitles = 0;
    for index in 0..archive.len() {
        let member = archive.by_index(index)?;
        if member.is_dir() {
            continue;
        }
        let name = member.name().to_owned();
        let extension = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        match subcheck::Format::from_extension(&extension) {
            Some(format) => {
                // Read one byte past the limit: enough to know that it is too large, and no more.
                let mut contents = Vec::new();
                member.take(subcheck::MAX_BYTES as u64 + 1).read_to_end(&mut contents)?;
                if let Err(why) = subcheck::check(&contents, format, script) {
                    bail!("{name} in the zip: {why}");
                }
                subtitles += 1;
            }
            None if matches!(extension.as_str(), "sub" | "sup" | "idx") => subtitles += 1,
            None => bail!("{name} in the zip is not a subtitle file"),
        }
    }
    if subtitles == 0 {
        bail!("the zip holds no subtitle files");
    }
    Ok(())
}

async fn verify_file(
    entry_path: &std::path::Path,
    staging: &std::path::Path,
    file_name: PathBuf,
    field: Field<'_>,
    script: Script,
) -> anyhow::Result<ProcessedFile> {
    let extension = file_name
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase);
    let path = entry_path.join(file_name);
    if let Some(kind) = extension.as_deref().and_then(bookcheck::Kind::from_extension) {
        if path.exists() {
            bail!("a file with this name is already there")
        }
        let staged = stage(staging, field, kind.max_bytes()).await?;
        let (checked, staged) =
            tokio::task::spawn_blocking(move || (bookcheck::check(&staged.0, kind, script), staged)).await?;
        checked?;
        return Ok(ProcessedFile {
            path,
            contents: Contents::Staged(staged),
            passed_subcheck: false,
            book: true,
        });
    }
    match extension.as_deref() {
        Some(ext @ ("srt" | "vtt" | "ass" | "ssa" | "zip" | "sub" | "sup" | "idx" | "7z")) => {
            if path.exists() {
                bail!("a file with this name is already there")
            }
            let bytes = read_capped(field, crate::MAX_UPLOAD_SIZE).await?;
            // The name says what the file claims to be. The contents say what it is.
            let mut passed_subcheck = false;
            if let Some(format) = subcheck::Format::from_extension(ext) {
                subcheck::check(&bytes, format, script)?;
                passed_subcheck = true;
            } else if ext == "zip" {
                verify_zip(&bytes, script)?;
            }
            Ok(ProcessedFile {
                path,
                contents: Contents::Bytes(bytes),
                passed_subcheck,
                book: false,
            })
        }
        _ => bail!(
            "not a subtitle, book or audiobook file (srt, vtt, ass, ssa, sub, sup, idx, zip, 7z, epub, m4b, opus)"
        ),
    }
}

async fn process_files(
    entry_path: &std::path::Path,
    staging: &std::path::Path,
    mut multipart: Multipart,
    script: Script,
) -> anyhow::Result<ProcessedFiles> {
    let mut files = Vec::new();
    let mut skipped = 0;
    let mut problems = Vec::new();
    while let Some(field) = multipart.next_field().await? {
        let Some(name) = field.file_name().map(sanitise_file_name::sanitise).map(PathBuf::from) else {
            tracing::debug!("Skipped file due to missing filename");
            skipped += 1;
            continue;
        };

        let shown = name.display().to_string();
        match verify_file(entry_path, staging, name, field, script).await {
            Ok(file) => files.push(file),
            Err(e) => {
                tracing::debug!(error=%e, "Skipped file due to validation issue");
                problems.push(format!("{shown}: {e}"));
                skipped += 1
            }
        }
    }

    // A book or an audiobook goes only where subtitles pass the check: in this upload, or in the entry.
    if files.iter().any(|f| f.book) && !files.iter().any(|f| f.passed_subcheck) {
        let folder = entry_path.to_path_buf();
        if !tokio::task::spawn_blocking(move || has_good_subtitles(&folder, script)).await? {
            let (books, rest): (Vec<_>, Vec<_>) = files.into_iter().partition(|f| f.book);
            for book in books {
                let shown = book.path.file_name().unwrap_or_default().to_string_lossy();
                problems.push(format!("{shown}: {NEEDS_SUBTITLES}"));
                skipped += 1;
            }
            files = rest;
        }
    }
    Ok(ProcessedFiles {
        files,
        skipped,
        problems,
    })
}

/// The result of an upload operation.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UploadResult {
    /// The number of files that did not succeed due to a filesystem error.
    errors: usize,
    /// The number of files that were processed.
    total: usize,
    /// The number of files that were skipped due to some reason
    skipped: usize,
    /// Why each skipped file was skipped.
    problems: Vec<String>,
}

impl UploadResult {
    pub fn is_success(&self) -> bool {
        self.total > 0 && self.errors == 0 && self.skipped == 0
    }

    pub fn is_error(&self) -> bool {
        self.total == self.errors
    }

    pub fn successful(&self) -> usize {
        self.total - self.errors
    }
}

pub async fn raw_upload_file(
    state: AppState,
    entry_id: i64,
    account: Account,
    multipart: Multipart,
    api: bool,
) -> Result<UploadResult, ApiError> {
    if account.flags.is_restricted() {
        return Err(ApiError::new("Account is restricted from uploading").with_code(ApiErrorCode::NoPermissions));
    }

    let Some(entry) = state.get_directory_entry(entry_id).await else {
        return Err(ApiError::not_found("Entry not found"));
    };

    // The text of an upload must be in the language of the entry, or else of the site.
    let language = entry
        .language
        .as_deref()
        .or(state.config().subtitle_language.as_deref());
    let script = Script::from_code(language.unwrap_or(""));
    let entry = entry.path;
    // A book or an audiobook is written here first. The folder is on the same file system as
    // the entries, so a hard link can move the file into place. The sync skips it, because its
    // name starts with a dot.
    let staging = state.config().subtitle_path.join(".incoming");
    let Ok(processed) = process_files(&entry, &staging, multipart, script).await else {
        return Err(ApiError::new("Internal error when processing files").with_code(ApiErrorCode::ServerError));
    };

    if processed.files.is_empty() {
        if processed.problems.is_empty() {
            return Err(ApiError::new("Did not upload any files."));
        }
        return Err(ApiError::new(format!(
            "No file was accepted. {}",
            processed.problems.join(" ")
        )));
    }

    let mut errored = 0usize;
    let total = processed.files.len();
    let requested = total + processed.skipped;
    let mut data = audit::Upload {
        files: Vec::with_capacity(total),
        api,
    };
    let mut set = JoinSet::new();
    for file in processed.files.into_iter() {
        set.spawn_blocking(move || {
            let name = file.path.file_name().and_then(|x| x.to_str()).unwrap().to_owned();
            let failed = file.write_to_disk().is_err();
            audit::FileOperation { name, failed }
        });
    }

    while let Some(task) = set.join_next().await {
        match task {
            Ok(op) => {
                errored += op.failed as usize;
                data.files.push(op);
            }
            _ => errored += 1,
        }
    }

    let successful = total > 0 && errored == 0 && processed.skipped != requested;
    if successful && errored != total {
        let _ = state
            .database()
            .execute(
                "UPDATE directory_entry SET last_updated_at = CURRENT_TIMESTAMP WHERE id = ?",
                [entry_id],
            )
            .await;
        state.cached_directories().invalidate().await;

        // Unfortunate copy required
        let files = data
            .files
            .iter()
            .filter(|f| !f.failed)
            .map(|f| f.name.clone())
            .collect();

        state.notifications.notify_new_subtitles(entry_id, files);
    }

    state
        .audit(audit::AuditLogEntry::full(data, entry_id, account.id))
        .await;

    Ok(UploadResult {
        errors: errored,
        total,
        skipped: processed.skipped,
        problems: processed.problems,
    })
}

async fn upload_file(
    State(state): State<AppState>,
    Path(entry_id): Path<i64>,
    Referrer(url): Referrer,
    account: Account,
    flasher: Flasher,
    multipart: Multipart,
) -> Response {
    let result = match raw_upload_file(state, entry_id, account, multipart, false).await {
        Ok(result) => result,
        Err(msg) => return flasher.add(msg.error.as_ref()).bail(&url),
    };
    let message = if result.is_success() {
        FlashMessage::success("Upload successful.")
    } else if result.is_error() {
        FlashMessage::error("Upload failed.")
    } else {
        let successful = result.successful();
        FlashMessage::warning(format!(
            "Uploaded {successful} file{}, {} {} skipped and {} failed. {}",
            if successful == 1 { "" } else { "s" },
            result.skipped,
            if result.skipped == 1 { "was" } else { "were" },
            result.errors,
            result.problems.join(" "),
        ))
    };
    flasher.add(message).bail(&url)
}

async fn bulk_download(
    State(state): State<AppState>,
    Path(entry_id): Path<i64>,
    Json(payload): Json<BulkFilesPayload>,
) -> Result<Response, ApiError> {
    let Some(entry) = state.get_directory_entry(entry_id).await else {
        return Err(ApiError::not_found("Directory entry not found."));
    };

    // The zip is made in memory. An audiobook is too large for that, so it is downloaded alone.
    const MAX_BULK_BYTES: u64 = 64 * 1024 * 1024;
    let mut total = 0;
    for file in &payload.files {
        if let Ok(metadata) = tokio::fs::metadata(entry.path.join(file)).await {
            total += metadata.len();
        }
    }
    if total > MAX_BULK_BYTES {
        return Err(ApiError::new(
            "The files are larger than 64 MB together. Download the large files one by one.",
        ));
    }

    let filename = sanitise_file_name::sanitise(&format!("{}.zip", &entry.name));
    let buffer = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
        let options = zip::write::SimpleFileOptions::default();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));

        for file in payload.files {
            let path = entry.path.join(&file);
            let Ok(contents) = std::fs::read(&path) else {
                continue;
            };
            zip.start_file(file, options)?;
            zip.write_all(&contents)?;
        }
        let mut buffer = zip.finish()?.into_inner();
        buffer.shrink_to_fit();
        Ok(Bytes::from(buffer))
    })
    .await??;

    let body = Body::from(buffer);
    let headers = [
        (CONTENT_TYPE, "application/zip"),
        (CONTENT_DISPOSITION, &format!("attachment; filename=\"{filename}\"")),
        (HeaderName::from_static("x-jimaku-filename"), &filename),
    ];
    Ok((headers, body).into_response())
}

async fn add_bookmark(
    State(state): State<AppState>,
    Path(entry_id): Path<i64>,
    account: Account,
) -> Result<axum::http::StatusCode, ApiError> {
    state
        .database()
        .execute(
            "INSERT INTO bookmark(user_id, entry_id) VALUES (?, ?) ON CONFLICT DO NOTHING",
            (account.id, entry_id),
        )
        .await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn remove_bookmark(
    State(state): State<AppState>,
    Path(entry_id): Path<i64>,
    account: Account,
) -> Result<axum::http::StatusCode, ApiError> {
    state
        .database()
        .execute(
            "DELETE FROM bookmark WHERE user_id = ? AND entry_id = ?",
            (account.id, entry_id),
        )
        .await?;
    Ok(axum::http::StatusCode::OK)
}

#[derive(Deserialize)]
struct RelationsRequest {
    anilist_ids: Vec<u32>,
}

async fn relations(
    State(state): State<AppState>,
    Json(requested): Json<RelationsRequest>,
) -> Result<Json<Vec<DirectoryEntry>>, ApiError> {
    if requested.anilist_ids.len() > 250 {
        return Err(ApiError::new("Can only request up to 250 AniList IDs"));
    }
    let mut query = "SELECT * FROM directory_entry WHERE anilist_id IN (".to_string();
    for _ in &requested.anilist_ids {
        query.push('?');
        query.push(',');
    }
    if query.ends_with(',') {
        query.pop();
    }
    query.push(')');
    let entries = state
        .database()
        .all(query, rusqlite::params_from_iter(requested.anilist_ids))
        .await?;

    Ok(Json(entries))
}

#[derive(Deserialize)]
struct BulkTmdbLookupRequest {
    tmdb_ids: Vec<tmdb::Id>,
}

async fn bulk_tmdb_lookup(
    State(state): State<AppState>,
    Json(requested): Json<BulkTmdbLookupRequest>,
) -> Result<Json<Vec<DirectoryEntry>>, ApiError> {
    if requested.tmdb_ids.len() > 250 {
        return Err(ApiError::new("Can only request up to 250 TMDB IDs"));
    }
    let mut query = "SELECT * FROM directory_entry WHERE tmdb_id IN (".to_string();
    for _ in &requested.tmdb_ids {
        query.push('?');
        query.push(',');
    }
    if query.ends_with(',') {
        query.pop();
    }
    query.push(')');
    let entries = state
        .database()
        .all(query, rusqlite::params_from_iter(requested.tmdb_ids))
        .await?;

    Ok(Json(entries))
}

#[derive(Serialize)]
struct EntryWithFiles {
    entry: DirectoryEntry,
    files: Vec<FileEntry>,
    bookmarked: Option<bool>,
}

async fn get_full_data_from_anilist(
    State(state): State<AppState>,
    account: Option<Account>,
    Json(requested): Json<RelationsRequest>,
) -> Result<Json<Vec<EntryWithFiles>>, ApiError> {
    if requested.anilist_ids.len() > 250 {
        return Err(ApiError::new("Can only request up to 250 AniList IDs"));
    }
    let mut query = "SELECT * FROM directory_entry WHERE anilist_id IN (".to_string();
    for _ in &requested.anilist_ids {
        query.push('?');
        query.push(',');
    }
    if query.ends_with(',') {
        query.pop();
    }
    query.push(')');
    let entries: Vec<DirectoryEntry> = state
        .database()
        .all(query, rusqlite::params_from_iter(requested.anilist_ids))
        .await?;

    let bookmarks = match &account {
        Some(account) => state.bookmarked_ids(account.id).await?,
        None => Default::default(),
    };

    let mut result = Vec::with_capacity(entries.len());
    for entry in entries.into_iter() {
        let mut bookmarked = Some(bookmarks.contains(&entry.id));
        if account.is_none() {
            bookmarked.take();
        }
        result.push(EntryWithFiles {
            files: get_file_entries(entry.id, &entry.path).unwrap_or_default(),
            entry,
            bookmarked,
        });
    }
    Ok(Json(result))
}

#[derive(Deserialize)]
struct TmdbQuery {
    id: tmdb::Id,
}

#[derive(Serialize)]
struct TmdbInfo {
    title: MediaTitle,
    adult: bool,
    movie: bool,
}

async fn tmdb_lookup(
    State(state): State<AppState>,
    account: Account,
    Query(query): Query<TmdbQuery>,
) -> Result<Json<Option<TmdbInfo>>, ApiError> {
    if !account.flags.is_editor() {
        return Err(ApiError::forbidden());
    }

    Ok(Json(
        tmdb::get_media_info(&state.client, &state.config().tmdb_api_key, query.id)
            .await?
            .map(|info| TmdbInfo {
                title: info.titles(),
                adult: info.is_adult(),
                movie: query.id.is_movie(),
            }),
    ))
}

#[derive(Deserialize)]
struct ImportEntry {
    anime: bool,
    name: String,
}

#[derive(Template)]
#[template(path = "entry_import.html")]
struct ImportEntryTemplate {
    account: Option<Account>,
    flashes: Flashes,
    pending: DirectoryEntry,
    anime: bool,
}

async fn get_pending_directory_entry(state: &AppState, anime: bool, name: String) -> DirectoryEntry {
    let mut temporary = DirectoryEntry::temporary(name.clone());
    temporary.flags.set_anime(anime);
    if anime {
        let media = anilist::search(&state.client, &name)
            .await
            .map(|m| m.into_iter().next());
        if let Ok(Some(media)) = media {
            temporary.anilist_id = Some(media.id);
            temporary.flags.set_movie(media.is_movie());
            temporary.flags.set_adult(media.adult);
            temporary.name = media.title.romaji;
            temporary.japanese_name = media.title.native;
            temporary.english_name = media.title.english;
        }
    } else {
        let info = tmdb::find_match(&state.client, &state.config().tmdb_api_key, &name).await;
        if let Ok(Some(info)) = info {
            temporary.tmdb_id = Some(info.id);
            temporary.flags.set_movie(info.id.is_movie());
            temporary.flags.set_adult(info.is_adult());
            let titles = info.titles();
            temporary.name = titles.romaji;
            temporary.japanese_name = titles.native;
            temporary.english_name = titles.english;
        }
    }
    temporary
}

async fn import_entry(
    State(state): State<AppState>,
    account: Account,
    flashes: Flashes,
    flasher: Flasher,
    Form(payload): Form<ImportEntry>,
) -> Response {
    if !account.flags.is_editor() {
        return flasher.add("You do not have permissions to do this.").bail("/");
    }

    let pending = get_pending_directory_entry(&state, payload.anime, payload.name).await;
    let mut response = HtmlPage(ImportEntryTemplate {
        account: Some(account),
        flashes,
        pending,
        anime: payload.anime,
    })
    .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response
}

#[derive(Deserialize)]
struct ImportQuery {
    anime: bool,
}

#[derive(Serialize)]
struct ImportResult {
    entry_id: i64,
    errors: usize,
}

#[derive(Deserialize)]
struct CreateImportedEntry {
    files: Vec<PendingFileEntry>,
    #[serde(flatten)]
    inner: EditDirectoryEntry,
}

async fn create_imported_entry(
    State(state): State<AppState>,
    account: Account,
    Query(query): Query<ImportQuery>,
    Json(payload): Json<CreateImportedEntry>,
) -> Result<Json<ImportResult>, ApiError> {
    if !account.flags.is_editor() {
        return Err(ApiError::forbidden());
    }

    let validation_errors = payload.inner.validate();
    if !validation_errors.is_empty() {
        return Err(ApiError::new(validation_errors.join("\n")));
    }

    let mut flags = payload.inner.apply_flags(EntryFlags::new());
    flags.set_anime(query.anime);
    let pending = PendingDirectoryEntry {
        book_id: None,
        language: None,
        bangumi_id: payload.inner.bangumi_id,
        anilist_id: payload.inner.anilist_id,
        tmdb_id: payload.inner.tmdb_id,
        name: None,
        flags: Some(flags),
        anime: query.anime,
        notes: payload.inner.notes.clone(),
        titles: Some(payload.inner.titles()),
    };

    // Unfortunately have to pay this cost twice
    let path = pending.path(pending.titles.as_ref().unwrap().romaji.as_str(), pending.anime, &state);
    let anilist_id = pending.anilist_id;
    let tmdb_id = pending.tmdb_id;
    let account_id = account.id;

    let (id, path) = match raw_create_directory_entry(&state, account, pending, false).await {
        Ok(p) => p,
        Err(e) if e.code == ApiErrorCode::EntryAlreadyExists => state
            .database()
            .get_row(
                "SELECT id, path FROM directory_entry WHERE path = ? OR anilist_id = ? OR tmdb_id = ?",
                (path.display().to_string(), anilist_id, tmdb_id),
                |row| Ok((row.get("id")?, PathBuf::from(row.get::<_, String>("path")?))),
            )
            .await
            .optional()?
            .ok_or(e)?,
        Err(e) => return Err(e),
    };

    let mut set = JoinSet::new();
    let mut data = audit::Upload {
        files: Vec::with_capacity(payload.files.len()),
        api: false,
    };
    for file in payload.files {
        let p = path.clone();
        set.spawn_blocking(move || {
            let failed = file.write_to_disk(p).is_err();
            audit::FileOperation {
                name: file.name,
                failed,
            }
        });
    }

    let mut errors = 0;
    while let Some(task) = set.join_next().await {
        match task {
            Ok(op) => {
                errors += op.failed as usize;
                data.files.push(op);
            }
            _ => errors += 1,
        }
    }

    state.audit(audit::AuditLogEntry::full(data, id, account_id)).await;

    Ok(Json(ImportResult { entry_id: id, errors }))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/entry/{id}", get(get_entry))
        .route(
            "/entry/{id}/download/{*path}",
            get(download_entry).layer(CorsLayer::permissive()),
        )
        .route(
            "/entry/create",
            post(create_directory_entry).layer(RateLimit::default().quota(5, 30.0).build()),
        )
        .route("/entry/{id}/edit", post(edit_directory_entry))
        .route("/entry/{id}/move", post(move_directory_entries))
        .route("/entry/{id}/rename", post(bulk_rename_files))
        .route("/entry/{id}", delete(bulk_delete_files))
        .route(
            "/entry/{id}/report",
            post(report_entry).layer(RateLimit::default().build()),
        )
        .route("/entry/search", get(search_directory_entries))
        .route(
            "/entry/{id}/bulk",
            post(bulk_download).layer(RateLimit::default().build()),
        )
        .route("/entry/relations", post(relations))
        .route("/entry/relations/tmdb", post(bulk_tmdb_lookup))
        .route(
            "/entry/relations/full",
            post(get_full_data_from_anilist).layer(RateLimit::default().build()),
        )
        .route("/entry/tmdb", get(tmdb_lookup))
        .route("/entry/import", post(import_entry))
        .route("/entry/import/create", post(create_imported_entry))
        .route(
            "/entry/{id}/bookmark",
            put(add_bookmark)
                .delete(remove_bookmark)
                .layer(RateLimit::default().build()),
        )
}

/// The upload route. It is apart from the others because an audiobook needs a larger body
/// limit and a longer timeout (see `routes::uploads`).
pub fn upload_routes() -> Router<AppState> {
    Router::new().route(
        "/entry/{id}/upload",
        post(upload_file).layer(RateLimit::default().build()),
    )
}

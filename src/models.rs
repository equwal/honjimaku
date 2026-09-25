use std::path::PathBuf;

use rusqlite::{
    ToSql,
    types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef},
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::{PartialSchema, ToSchema};

use crate::{database::Table, key::SecretKey, tmdb, token::Token};

#[derive(Deserialize, Serialize, PartialEq, Eq, Clone, Copy)]
pub struct EntryFlags(u32);

impl FromSql for EntryFlags {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let value = u32::column_result(value)?;
        Ok(Self(value))
    }
}

impl ToSql for EntryFlags {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.0.into())
    }
}

// At the API level, we expand the flags to a dict to make it easier to work
// with for consumers
impl PartialSchema for EntryFlags {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        ExpandedEntryFlags::schema()
    }
}

impl ToSchema for EntryFlags {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("EntryFlags")
    }

    fn schemas(schemas: &mut Vec<(String, utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>)>) {
        ExpandedEntryFlags::schemas(schemas);
    }
}

impl EntryFlags {
    const ANIME: u32 = 1 << 0;
    const UNVERIFIED: u32 = 1 << 1;
    const EXTERNAL: u32 = 1 << 2;
    const MOVIE: u32 = 1 << 3;
    const ADULT: u32 = 1 << 4;
    const REVIEWED: u32 = 1 << 5;

    pub const fn new() -> Self {
        Self(Self::ANIME)
    }

    #[inline]
    fn has_flag(&self, val: u32) -> bool {
        (self.0 & val) == val
    }

    #[inline]
    fn toggle_flag(&mut self, val: u32, toggle: bool) {
        if toggle {
            self.0 |= val;
        } else {
            self.0 &= !val;
        }
    }

    pub fn is_anime(&self) -> bool {
        self.has_flag(Self::ANIME)
    }

    pub fn set_anime(&mut self, toggle: bool) {
        self.toggle_flag(Self::ANIME, toggle)
    }

    pub fn is_unverified(&self) -> bool {
        self.has_flag(Self::UNVERIFIED)
    }

    pub fn set_unverified(&mut self, toggle: bool) {
        self.toggle_flag(Self::UNVERIFIED, toggle)
    }

    pub fn is_external(&self) -> bool {
        self.has_flag(Self::EXTERNAL)
    }

    pub fn set_external(&mut self, toggle: bool) {
        self.toggle_flag(Self::EXTERNAL, toggle)
    }

    pub fn is_movie(&self) -> bool {
        self.has_flag(Self::MOVIE)
    }

    pub fn set_movie(&mut self, toggle: bool) {
        self.toggle_flag(Self::MOVIE, toggle)
    }

    pub fn is_adult(&self) -> bool {
        self.has_flag(Self::ADULT)
    }

    pub fn set_adult(&mut self, toggle: bool) {
        self.toggle_flag(Self::ADULT, toggle)
    }

    pub fn is_reviewed(&self) -> bool {
        self.has_flag(Self::REVIEWED)
    }

    pub fn set_reviewed(&mut self, toggle: bool) {
        self.toggle_flag(Self::REVIEWED, toggle)
    }
}

impl Default for EntryFlags {
    fn default() -> Self {
        Self(Self::ANIME)
    }
}

/// Flags associated with the given entry.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, ToSchema)]
pub struct ExpandedEntryFlags {
    /// The entry is for an anime.
    #[serde(default)]
    anime: bool,
    /// The entry is unverified and has not been checked by editors.
    #[schema(example = false)]
    #[serde(alias = "low_quality")]
    #[serde(default)]
    unverified: bool,
    /// The entry comes from an external source.
    #[schema(example = false)]
    #[serde(default)]
    external: bool,
    /// The entry is a movie.
    #[schema(example = false)]
    #[serde(default)]
    movie: bool,
    /// The entry is meant for adult audiences.
    #[schema(example = false)]
    #[serde(default)]
    adult: bool,
    /// A person has reviewed the subtitles against the book, the audiobook or the video.
    #[schema(example = false)]
    #[serde(default)]
    reviewed: bool,
}

impl From<EntryFlags> for ExpandedEntryFlags {
    fn from(value: EntryFlags) -> Self {
        Self {
            anime: value.is_anime(),
            unverified: value.is_unverified(),
            external: value.is_external(),
            movie: value.is_movie(),
            adult: value.is_adult(),
            reviewed: value.is_reviewed(),
        }
    }
}

impl From<ExpandedEntryFlags> for EntryFlags {
    fn from(value: ExpandedEntryFlags) -> Self {
        let mut flags = Self::new();
        flags.set_anime(value.anime);
        flags.set_unverified(value.unverified);
        flags.set_external(value.external);
        flags.set_movie(value.movie);
        flags.set_adult(value.adult);
        flags.set_reviewed(value.reviewed);
        flags
    }
}

impl std::fmt::Debug for EntryFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectoryFlags")
            .field("value", &self.0)
            .field("anime", &self.is_anime())
            .field("unverified", &self.is_unverified())
            .field("external", &self.is_external())
            .field("movie", &self.is_movie())
            .field("adult", &self.is_adult())
            .field("reviewed", &self.is_reviewed())
            .finish()
    }
}

pub mod expand_flags {
    use super::{EntryFlags, ExpandedEntryFlags};
    use serde::{self, Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(value: &EntryFlags, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ExpandedEntryFlags::from(*value).serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<EntryFlags, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(EntryFlags::from(ExpandedEntryFlags::deserialize(deserializer)?))
    }

    pub mod option {
        use super::*;

        pub fn serialize<S>(value: &Option<EntryFlags>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            value.map(ExpandedEntryFlags::from).serialize(serializer)
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<EntryFlags>, D::Error>
        where
            D: Deserializer<'de>,
        {
            Ok(Option::<ExpandedEntryFlags>::deserialize(deserializer)?.map(EntryFlags::from))
        }
    }
}

/// What an entry is: a book, an anime, or a live action show.
///
/// A site for books has a tab for each kind that has entries in the language of the page:
/// the books are its own, the shows are a copy of another site (see `mirror`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Book,
    Anime,
    /// A live action show: a drama or a film.
    Drama,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Book, Kind::Anime, Kind::Drama];

    /// The kind in a URL and in the database: `book`, `anime`, `drama`.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Book => "book",
            Kind::Anime => "anime",
            Kind::Drama => "drama",
        }
    }

    /// The kind that a URL or a row names. `None` if it names no kind.
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str().eq_ignore_ascii_case(raw))
    }

    /// The name of the tab of the kind.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Book => "Books",
            Kind::Anime => "Anime",
            Kind::Drama => "Live Action",
        }
    }
}

impl FromSql for Kind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Self::parse(text).ok_or_else(|| FromSqlError::Other(format!("\"{text}\" is not a kind of entry").into()))
    }
}

impl ToSql for Kind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

/// An entry that contains subtitles.
///
/// These are typically backed by e.g. an anilist or tmdb entry to
/// facilitate some features.
#[derive(Debug, Serialize, PartialEq, Eq, Clone, ToSchema)]
#[schema(as = Entry)]
pub struct DirectoryEntry {
    /// The ID of the entry.
    pub id: i64,
    /// The physical exact path where this entry belongs in the filesystem.
    #[serde(skip)]
    pub path: PathBuf,
    /// The romaji name of the entry.
    #[schema(example = "Sousou no Frieren")]
    pub name: String,
    /// The flags associated with this entry.
    #[serde(with = "expand_flags")]
    pub flags: EntryFlags,
    /// The date of the newest uploaded file as an RFC3339 timestamp.
    #[serde(rename = "last_modified")]
    #[serde(with = "time::serde::rfc3339")]
    pub last_updated_at: OffsetDateTime,
    /// The account ID that created this entry
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator_id: Option<i64>,
    /// The anilist ID of this entry.
    #[schema(example = 154587)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anilist_id: Option<u32>,
    /// The TMDB ID of this entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(pattern = r#"(tv|movie):(\d+)"#, value_type = Option<String>, example = "tv:12345")]
    pub tmdb_id: Option<tmdb::Id>,
    /// On a site for books: the identifier of the audiobook. An Audible ASIN is verified against Audible.
    #[schema(example = "B0BPXSSWVF")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub book_id: Option<String>,
    /// On a site for Chinese shows: the Bangumi (bgm.tv) subject number, verified against Bangumi.
    #[schema(example = 258207)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bangumi_id: Option<u32>,
    /// The ISO 639-1 code of the language of the entry. None is the language of the site.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// What the entry is: `book`, `anime` or `drama` (a live action show). None is what the site is for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    /// Extra notes that the entry might have.
    ///
    /// Supports a limited set of markdown. Can only be set by editors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// The English name of the entry.
    #[schema(example = "Frieren: Beyond Journey’s End")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub english_name: Option<String>,
    /// The Japanese name of the entry, i.e. with kanji and kana.
    #[schema(example = "葬送のフリーレン")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub japanese_name: Option<String>,
}

impl Table for DirectoryEntry {
    const NAME: &'static str = "directory_entry";

    const COLUMNS: &'static [&'static str] = &[
        "id",
        "path",
        "flags",
        "last_updated_at",
        "creator_id",
        "anilist_id",
        "tmdb_id",
        "book_id",
        "bangumi_id",
        "language",
        "kind",
        "notes",
        "english_name",
        "japanese_name",
        "name",
    ];

    type Id = i64;

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let path: String = row.get("path")?;
        Ok(Self {
            id: row.get("id")?,
            path: PathBuf::from(path),
            name: row.get("name")?,
            flags: row.get("flags")?,
            last_updated_at: row.get("last_updated_at")?,
            creator_id: row.get("creator_id")?,
            anilist_id: row.get("anilist_id")?,
            tmdb_id: row.get("tmdb_id")?,
            book_id: row.get("book_id")?,
            bangumi_id: row.get("bangumi_id")?,
            language: row.get("language")?,
            kind: row.get("kind")?,
            notes: row.get("notes")?,
            english_name: row.get("english_name")?,
            japanese_name: row.get("japanese_name")?,
        })
    }
}

/// A specific model used for backup purposes with different (de)serialization requirements.
#[derive(Debug, Serialize, Deserialize)]
pub struct DirectoryEntryBackup {
    pub id: i64,
    pub path: PathBuf,
    pub name: String,
    pub flags: EntryFlags,
    #[serde(rename = "last_modified")]
    #[serde(with = "time::serde::rfc3339")]
    pub last_updated_at: OffsetDateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anilist_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmdb_id: Option<tmdb::Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub book_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bangumi_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub english_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub japanese_name: Option<String>,
}

/// Data that is passed around from the server to the frontend JavaScript
#[derive(Debug, Clone, Serialize)]
pub struct DirectoryEntryData<'a> {
    /// The romaji name of the entry.
    pub name: &'a str,
    /// The flags associated with this entry
    pub flags: EntryFlags,
    /// When the entry was last updated
    #[serde(rename = "last_modified")]
    #[serde(with = "time::serde::timestamp")]
    pub last_updated_at: &'a OffsetDateTime,
    /// The anilist ID of this entry.
    pub anilist_id: Option<u32>,
    /// The TMDB ID of this entry.
    pub tmdb_id: Option<tmdb::Id>,
    /// The identifier of the audiobook, on a site for books.
    pub book_id: &'a Option<String>,
    /// The Bangumi subject number, on a site for Chinese shows.
    pub bangumi_id: Option<u32>,
    /// The English name of the entry.
    pub english_name: &'a Option<String>,
    /// The Japanese name of the entry, i.e. with kanji and kana.
    pub japanese_name: &'a Option<String>,
}

impl DirectoryEntry {
    /// Returns a temporary DirectoryEntry suitable for editing.
    ///
    /// The [`DirectoryEntry::id`] and [`DirectoryEntry::path`] fields
    /// are filled with nonsense values, so do not rely on them.
    pub fn temporary(name: String) -> Self {
        Self {
            id: 0,
            name,
            path: Default::default(),
            flags: Default::default(),
            last_updated_at: OffsetDateTime::now_utc(),
            creator_id: Default::default(),
            anilist_id: Default::default(),
            tmdb_id: Default::default(),
            book_id: Default::default(),
            bangumi_id: Default::default(),
            language: Default::default(),
            kind: Default::default(),
            notes: Default::default(),
            english_name: Default::default(),
            japanese_name: Default::default(),
        }
    }

    /// The ISO 639-1 code of the language of the entry. An entry with none is in the language of the site.
    pub fn language_code<'a>(&'a self, config: &'a crate::Config) -> &'a str {
        self.language.as_deref().unwrap_or(config.default_language())
    }

    /// What the entry is. An entry with no kind of its own is what the site is for: a book
    /// on a site for books, else a show, animated or not as its flag says. A book has no
    /// AniList, TMDB or Bangumi page, so an entry with one is a show on any site.
    pub fn kind_of(&self, config: &crate::Config) -> Kind {
        let show = self.anilist_id.is_some() || self.tmdb_id.is_some() || self.bangumi_id.is_some();
        match self.kind {
            Some(kind) => kind,
            None if config.book_site && !show => Kind::Book,
            None if self.flags.is_anime() => Kind::Anime,
            None => Kind::Drama,
        }
    }

    /// Returns data safe for embedding into the frontend
    pub fn data(&self) -> DirectoryEntryData<'_> {
        DirectoryEntryData {
            name: &self.name,
            flags: self.flags,
            last_updated_at: &self.last_updated_at,
            anilist_id: self.anilist_id,
            tmdb_id: self.tmdb_id,
            book_id: &self.book_id,
            bangumi_id: self.bangumi_id,
            english_name: &self.english_name,
            japanese_name: &self.japanese_name,
        }
    }

    pub fn backup(self) -> DirectoryEntryBackup {
        DirectoryEntryBackup {
            id: self.id,
            path: self.path,
            name: self.name,
            flags: self.flags,
            last_updated_at: self.last_updated_at,
            anilist_id: self.anilist_id,
            tmdb_id: self.tmdb_id,
            book_id: self.book_id,
            bangumi_id: self.bangumi_id,
            language: self.language,
            kind: self.kind,
            notes: self.notes,
            english_name: self.english_name,
            japanese_name: self.japanese_name,
        }
    }

    /// Returns an appropriate description for the og:description meta tag
    pub fn description(&self) -> String {
        let language = crate::CONFIG.get().map(|c| c.language_label()).unwrap_or("Japanese");
        let mut base = format!("Download {language} subtitles for ");
        base.push_str(&self.name);
        base.push_str(". ");
        if let Some(english) = self.english_name.as_deref() {
            base.push_str("Also known as ");
            base.push_str(english);
            base.push_str(" in English");
        }

        if let Some(japanese) = self.japanese_name.as_deref() {
            if self.english_name.is_none() {
                base.push_str("Also known as ");
            } else {
                base.push_str(" or ");
            }
            base.push_str(japanese);
            base.push_str(" in Japanese");
        }
        if let Some(ch) = base.as_bytes().last() {
            if *ch == b' ' {
                base.pop();
            } else {
                base.push('.');
            }
        }
        base
    }
}

impl From<DirectoryEntryBackup> for DirectoryEntry {
    fn from(value: DirectoryEntryBackup) -> Self {
        Self {
            id: value.id,
            path: value.path,
            name: value.name,
            flags: value.flags,
            last_updated_at: value.last_updated_at,
            creator_id: None,
            anilist_id: value.anilist_id,
            tmdb_id: value.tmdb_id,
            book_id: value.book_id,
            bangumi_id: value.bangumi_id,
            language: value.language,
            kind: value.kind,
            notes: value.notes,
            english_name: value.english_name,
            japanese_name: value.japanese_name,
        }
    }
}

#[derive(Deserialize, Serialize, Default, PartialEq, Eq, Clone, Copy)]
pub struct AccountFlags(u32);

impl FromSql for AccountFlags {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let value = u32::column_result(value)?;
        Ok(Self(value))
    }
}

impl ToSql for AccountFlags {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.0.into())
    }
}

impl AccountFlags {
    const ADMIN: u32 = 1 << 0;
    const EDITOR: u32 = 1 << 1;
    const RESTRICTED: u32 = 1 << 2;

    pub const fn new() -> Self {
        Self(0)
    }

    #[inline]
    fn has_flag(&self, val: u32) -> bool {
        (self.0 & val) == val
    }

    #[inline]
    fn toggle_flag(&mut self, val: u32, toggle: bool) {
        if toggle {
            self.0 |= val;
        } else {
            self.0 &= !val;
        }
    }

    pub fn is_admin(&self) -> bool {
        self.has_flag(Self::ADMIN)
    }

    pub fn set_admin(&mut self, toggle: bool) {
        self.toggle_flag(Self::ADMIN, toggle)
    }

    pub fn is_editor(&self) -> bool {
        self.is_admin() || self.has_flag(Self::EDITOR)
    }

    pub fn set_editor(&mut self, toggle: bool) {
        self.toggle_flag(Self::EDITOR, toggle)
    }

    pub fn is_restricted(&self) -> bool {
        self.has_flag(Self::RESTRICTED)
    }

    pub fn set_restricted(&mut self, toggle: bool) {
        self.toggle_flag(Self::RESTRICTED, toggle)
    }
}

impl std::fmt::Debug for AccountFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountFlags")
            .field("value", &self.0)
            .field("editor", &self.is_editor())
            .field("admin", &self.is_admin())
            .field("restricted", &self.is_restricted())
            .finish()
    }
}

/// A registered account.
///
/// This server implements a rather simple authentication scheme.
/// Passwords are hashed using Argon2. No emails are stored.
///
/// Authentication is also done using [`crate::token::Token`] instead of
/// maintaining a session database.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Account {
    /// The account ID.
    pub id: i64,
    /// The username of the account.
    ///
    /// Usernames are all lowercase, and can only contain [a-z0-9._\-] characters.
    pub name: String,
    /// The Argon hashed password.
    pub password: String,
    /// The account flags associated with this account.
    pub flags: AccountFlags,
    /// The AniList username associated with this account
    pub anilist_username: Option<String>,
    /// The last timestamp that a notification was acked
    pub notification_ack: Option<i64>,
}

impl Table for Account {
    const NAME: &'static str = "account";

    const COLUMNS: &'static [&'static str] = &[
        "id",
        "name",
        "password",
        "flags",
        "anilist_username",
        "notification_ack",
    ];

    type Id = i64;

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            name: row.get("name")?,
            password: row.get("password")?,
            flags: row.get("flags")?,
            anilist_username: row.get("anilist_username")?,
            notification_ack: row.get("notification_ack")?,
        })
    }
}

impl Account {
    pub async fn get_bookmarks(&self, database: &crate::Database) -> rusqlite::Result<Vec<DirectoryEntry>> {
        let query = r#"
            SELECT * FROM directory_entry
            INNER JOIN bookmark ON bookmark.entry_id = directory_entry.id
            WHERE bookmark.user_id = ?
        "#;
        database.all(query, (self.id,)).await
    }
}

/// A bookmark on an entry that a user has done.
///
/// This is essentially just a way for users to follow
/// an entry and get notified when a new file was uploaded
/// to that entry.
///
/// This is represented as a many-to-many relationship in the
/// SQL table.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Bookmark {
    /// The account ID of the user who bookmarked the entry.
    pub user_id: i64,
    /// The entry ID that was bookmarked
    pub entry_id: i64,
}

impl Table for Bookmark {
    const NAME: &'static str = "bookmark";

    const COLUMNS: &'static [&'static str] = &["user_id", "entry_id"];

    // This table doesn't actually have an `id` column so this is unused
    type Id = (i64, i64);

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            user_id: row.get("user_id")?,
            entry_id: row.get("entry_id")?,
        })
    }
}

/// A trait for getting some information out of the account.
///
/// This works with `Option<Account>` as well. It's basically
/// just a cleaner way of doing `map` followed by `unwrap_or_default`.
pub trait AccountCheck {
    fn flags(&self) -> AccountFlags;
}

impl AccountCheck for Account {
    fn flags(&self) -> AccountFlags {
        self.flags
    }
}

impl AccountCheck for Option<Account> {
    fn flags(&self) -> AccountFlags {
        self.as_ref().map(|t| t.flags).unwrap_or_default()
    }
}

pub fn is_valid_username(s: &str) -> bool {
    s.len() >= 3
        && s.len() <= 32
        && s.as_bytes()
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'.' || *c == b'_' || *c == b'-')
}

/// An authentication session.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Session {
    /// The session ID.
    pub id: String,
    /// The account ID.
    pub account_id: i64,
    /// When the session was created
    pub created_at: OffsetDateTime,
    /// The description associated with this session
    pub description: Option<String>,
    /// Whether the session is an API key.
    pub api_key: bool,
}

impl Table for Session {
    const NAME: &'static str = "session";

    const COLUMNS: &'static [&'static str] = &["id", "account_id", "created_at", "description", "api_key"];

    type Id = String;

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            account_id: row.get("account_id")?,
            created_at: row.get("created_at")?,
            description: row.get("description")?,
            api_key: row.get("api_key")?,
        })
    }
}

impl Session {
    /// A human readable label used for the user.
    pub fn label(&self) -> &str {
        self.description.as_deref().unwrap_or("No description")
    }

    pub fn signed(&self, key: &SecretKey) -> Option<String> {
        Token::from_base64(&self.id).map(|t| t.signed(key))
    }
}

#[derive(Debug, PartialEq, Default, Eq, Clone, Copy, Hash, Serialize, Deserialize)]
#[repr(u8)]
#[serde(try_from = "u8", into = "u8")]
pub enum ReportStatus {
    #[default]
    Pending = 0,
    Rejected = 1,
    Solved = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InvalidReportStatus;

impl std::fmt::Display for InvalidReportStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid report status")
    }
}

impl std::error::Error for InvalidReportStatus {}

impl TryFrom<u8> for ReportStatus {
    type Error = InvalidReportStatus;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Pending),
            1 => Ok(Self::Rejected),
            2 => Ok(Self::Solved),
            _ => Err(InvalidReportStatus),
        }
    }
}

// This is explicit because I don't want to support ReportStatus::from(u8)
// but source.into() u8 is okay
#[allow(clippy::from_over_into)]
impl Into<u8> for ReportStatus {
    fn into(self) -> u8 {
        self as u8
    }
}

impl FromSql for ReportStatus {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let value = u8::column_result(value)?;
        Self::try_from(value).map_err(|e| rusqlite::types::FromSqlError::Other(Box::new(e)))
    }
}

impl ToSql for ReportStatus {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        let value = *self as u8;
        Ok(rusqlite::types::ToSqlOutput::Owned(value.into()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReportPayload {
    /// The files of the entry that got reported
    pub files: Vec<String>,
    /// The name of the entry that got reported
    pub name: String,
}

/// A report that a user has made
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct Report {
    /// The ID of the report. Represented as milliseconds since Unix Epoch UTC.
    pub id: i64,
    /// The user ID of the user who made the report. Can be `None` if the user got
    /// deleted.
    pub account_id: Option<i64>,
    /// The entry ID of the entry being reported. Can be `None` if the entry got deleted.
    pub entry_id: Option<i64>,
    /// The status of the report.
    pub status: ReportStatus,
    /// The reason for the report
    pub reason: String,
    /// The response message for the report
    pub response: Option<String>,
    /// Extra information pertaining to the report
    pub payload: ReportPayload,
}

crate::utils::sql_json_bridge!(ReportPayload);

impl Table for Report {
    const NAME: &'static str = "report";

    const COLUMNS: &'static [&'static str] = &[
        "id",
        "account_id",
        "entry_id",
        "status",
        "reason",
        "response",
        "payload",
    ];

    type Id = i64;

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            account_id: row.get("account_id")?,
            entry_id: row.get("entry_id")?,
            status: row.get("status")?,
            reason: row.get("reason")?,
            response: row.get("response")?,
            payload: row.get("payload")?,
        })
    }
}

impl Report {
    pub fn new(reason: String) -> Self {
        Self {
            id: crate::utils::unix_now_ms(),
            account_id: None,
            entry_id: None,
            status: ReportStatus::Pending,
            reason,
            response: None,
            payload: ReportPayload::default(),
        }
    }

    pub fn full(reason: String, payload: ReportPayload, entry_id: i64, account_id: i64) -> Self {
        Self {
            id: crate::utils::unix_now_ms(),
            account_id: Some(account_id),
            entry_id: Some(entry_id),
            status: ReportStatus::Pending,
            response: None,
            reason,
            payload,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reviewed_flag_goes_through_the_expanded_form_and_the_api() {
        let mut flags = EntryFlags::new();
        assert!(!flags.is_reviewed());
        flags.set_reviewed(true);
        let expanded = ExpandedEntryFlags::from(flags);
        assert!(expanded.reviewed && expanded.anime && !expanded.unverified);
        assert_eq!(EntryFlags::from(expanded), flags);
        assert!(serde_json::to_string(&expanded).unwrap().contains("\"reviewed\":true"));
        // An API client that does not know the flag leaves it out, and that means false.
        let old: ExpandedEntryFlags = serde_json::from_str(r#"{"anime":true}"#).unwrap();
        assert!(!EntryFlags::from(old).is_reviewed());
    }

    #[test]
    fn a_kind_is_named_in_a_url_and_in_the_database() {
        for kind in Kind::ALL {
            assert_eq!(Kind::parse(kind.as_str()), Some(kind));
            assert_eq!(Kind::parse(&format!(" {} ", kind.as_str().to_uppercase())), Some(kind));
            assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{}\"", kind.as_str()));
            assert_eq!(
                serde_json::from_str::<Kind>(&format!("\"{}\"", kind.as_str())).unwrap(),
                kind
            );
        }
        assert_eq!(Kind::parse("film"), None);
        assert_eq!(Kind::parse(""), None);
        assert_eq!(Kind::Drama.label(), "Live Action");

        let connection = rusqlite::Connection::open_in_memory().unwrap();
        let back: Kind = connection
            .query_row("SELECT ?", [Kind::Drama], |row| row.get(0))
            .unwrap();
        assert_eq!(back, Kind::Drama);
        let none: Option<Kind> = connection.query_row("SELECT NULL", [], |row| row.get(0)).unwrap();
        assert_eq!(none, None);
        let bad: rusqlite::Result<Kind> = connection.query_row("SELECT 'film'", [], |row| row.get(0));
        assert!(bad.is_err());
    }

    #[test]
    fn an_entry_without_a_kind_is_what_the_site_is_for() {
        let mut config = crate::Config::new().unwrap();
        let mut entry = DirectoryEntry::temporary("x".to_owned());
        config.book_site = true;
        assert_eq!(entry.kind_of(&config), Kind::Book);
        config.book_site = false;
        assert_eq!(entry.kind_of(&config), Kind::Anime);
        entry.flags.set_anime(false);
        assert_eq!(entry.kind_of(&config), Kind::Drama);
        entry.kind = Some(Kind::Anime);
        config.book_site = true;
        assert_eq!(entry.kind_of(&config), Kind::Anime);
    }

    #[test]
    fn a_show_on_a_site_for_books_is_not_a_book() {
        let mut config = crate::Config::new().unwrap();
        config.book_site = true;
        let mut anime = DirectoryEntry::temporary("x".to_owned());
        anime.anilist_id = Some(154587);
        assert_eq!(anime.kind_of(&config), Kind::Anime);
        let mut drama = DirectoryEntry::temporary("x".to_owned());
        drama.flags.set_anime(false);
        drama.tmdb_id = Some(crate::tmdb::Id::Tv { id: 1 });
        assert_eq!(drama.kind_of(&config), Kind::Drama);
        let mut chinese = DirectoryEntry::temporary("x".to_owned());
        chinese.flags.set_anime(false);
        chinese.bangumi_id = Some(258207);
        assert_eq!(chinese.kind_of(&config), Kind::Drama);
        let mut book = DirectoryEntry::temporary("x".to_owned());
        book.book_id = Some("B0BPXSSWVF".to_owned());
        assert_eq!(book.kind_of(&config), Kind::Book);
    }
}

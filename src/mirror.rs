//! A live copy of another jimaku site.
//!
//! This site holds books. The Japanese shows are on jimaku.cc, the Chinese shows on
//! dung.live. When one of those sites goes down, its subtitles go with it. So this site
//! keeps a copy of each site named in `mirrors` of the config: once an hour it asks the
//! API of the site for its entries, makes an entry here for each one that is new (in the
//! language of that site, as an anime or as a live action show), and copies the files
//! that it does not have yet. An entry whose copy is complete is looked at again only
//! when the site has a newer file in it. Nothing is deleted here when the site deletes.
//!
//! The API of a jimaku site answers an API key only, and 25 requests a minute. The first
//! copy of a large site takes hours. After a restart the copy goes on where it stopped,
//! because the table `mirror` says which entries are complete.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, bail};
use futures_util::StreamExt;
use percent_encoding::percent_encode;
use reqwest::header::{AUTHORIZATION, HeaderMap, USER_AGENT};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use time::OffsetDateTime;
use tokio::{io::AsyncWriteExt, task::JoinSet};
use tracing::{info, warn};

use crate::{
    AppState, Config, Database,
    cached::TimedCachedValue,
    database::{Table, is_unique_constraint_violation},
    models::{DirectoryEntry, EntryFlags, Kind},
    routes::{PathIds, directory_entry_path},
    tmdb,
    utils::FRAGMENT,
};

/// A site that this site keeps a copy of. From the config.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Mirror {
    /// The site, with its scheme: "https://jimaku.cc".
    pub url: String,
    /// The ISO 639-1 code of the language of the subtitles of the site: "ja", "zh".
    pub language: String,
    /// The API key of an account on the site.
    pub api_key: String,
}

impl Mirror {
    /// The site without its scheme: "jimaku.cc". The table `mirror` and the notes name it so.
    pub fn host(&self) -> &str {
        let url = self.url.trim_end_matches('/');
        url.split_once("://").map(|(_, rest)| rest).unwrap_or(url)
    }

    /// The address of a page or an API route of the site.
    fn at(&self, path: &str) -> String {
        format!("{}{path}", self.url.trim_end_matches('/'))
    }
}

/// What the copy needs: the parts of `AppState` that it uses, so that a test can make
/// them without the rest of the server.
pub struct Site<'a> {
    pub config: &'a Config,
    pub database: &'a Database,
    pub client: &'a reqwest::Client,
    /// The listing of the front page. It is made again after the copy changes an entry.
    pub cache: &'a TimedCachedValue<Vec<DirectoryEntry>>,
}

/// An entry as the API of a jimaku site answers it.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteEntry {
    pub id: i64,
    pub name: String,
    #[serde(with = "crate::models::expand_flags")]
    pub flags: EntryFlags,
    /// The date of the newest file of the entry.
    #[serde(with = "time::serde::rfc3339")]
    pub last_modified: OffsetDateTime,
    #[serde(default)]
    pub anilist_id: Option<u32>,
    #[serde(default)]
    pub tmdb_id: Option<tmdb::Id>,
    #[serde(default)]
    pub bangumi_id: Option<u32>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub english_name: Option<String>,
    #[serde(default)]
    pub japanese_name: Option<String>,
}

impl RemoteEntry {
    /// The kind of the copy: an anime, or a live action show.
    fn kind(&self) -> Kind {
        if self.flags.is_anime() {
            Kind::Anime
        } else {
            Kind::Drama
        }
    }

    /// The date of the newest file, as nanoseconds since 1970. The table `mirror` keeps it
    /// so, because the site gives it to the nanosecond, and a date that lost a part of it
    /// would look older than the site's date at each pass.
    fn stamp(&self) -> i64 {
        i64::try_from(self.last_modified.unix_timestamp_nanos()).unwrap_or(i64::MAX)
    }

    /// The notes of the copy: what the site says, and where the copy is from.
    fn notes(&self, mirror: &Mirror) -> String {
        let source = format!(
            "Mirror of [{host}/entry/{id}]({url}).",
            host = mirror.host(),
            id = self.id,
            url = mirror.at(&format!("/entry/{}", self.id))
        );
        match self.notes.as_deref().map(str::trim).filter(|notes| !notes.is_empty()) {
            Some(notes) => format!("{notes}\n\n{source}"),
            None => source,
        }
    }

    /// What names the entry, to find the entry here that is the same show.
    fn keys(&self) -> Vec<Key> {
        keys(self.anilist_id, self.tmdb_id, self.bangumi_id, self.kind(), &self.name)
    }
}

/// A file of an entry, as the API of a jimaku site answers it.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteFile {
    pub name: String,
    pub size: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub last_modified: OffsetDateTime,
}

/// What one pass of the copy did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassReport {
    /// Entries that the site has.
    pub entries: usize,
    /// Entries made here in this pass.
    pub made: usize,
    /// Entries whose files were copied in this pass.
    pub copied: usize,
    /// Files copied in this pass.
    pub files: usize,
    /// Entries that could not be copied. The next pass tries them again.
    pub failed: usize,
    /// True if the pass stopped because the disk is nearly full.
    pub disk_full: bool,
}

/// The entry here that is the copy of an entry of the site, from the table `mirror`.
#[derive(Debug, Clone)]
struct Copied {
    entry_id: i64,
    path: PathBuf,
    /// See the table `mirror`.
    last_modified: Option<i64>,
}

/// What names an entry, to find the entry here that is the same show as one of the site.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    AniList(u32),
    Tmdb(tmdb::Id),
    Bangumi(u32),
    /// An entry with no ID is named by its kind and its name.
    Named(Kind, String),
}

/// The IDs of an entry, or its kind and name if it has no ID. The name is not used when
/// there is an ID: two shows can have one name.
fn keys(
    anilist_id: Option<u32>,
    tmdb_id: Option<tmdb::Id>,
    bangumi_id: Option<u32>,
    kind: Kind,
    name: &str,
) -> Vec<Key> {
    let mut keys = Vec::with_capacity(1);
    keys.extend(anilist_id.map(Key::AniList));
    keys.extend(tmdb_id.map(Key::Tmdb));
    keys.extend(bangumi_id.map(Key::Bangumi));
    if keys.is_empty() {
        keys.push(Key::Named(kind, name.to_owned()));
    }
    keys
}

/// What a pass knows about the entries here.
struct Known {
    /// The copies of the entries of the site, by the ID of the entry there.
    copies: HashMap<i64, Copied>,
    /// Each entry here, by its ID.
    entries: HashMap<i64, DirectoryEntry>,
    /// The entries here that can become the copy of an entry of the site: shows in the
    /// language of the site that are not the copy of another entry.
    adoptable: HashMap<Key, i64>,
}

/// The API of a jimaku site answers with the User-Agent of the program.
const USER_AGENT_VALUE: &str = "honjimaku/0.1 (https://github.com/equwal/honjimaku)";
/// Files are copied this many at a time.
const DOWNLOADS_AT_ONCE: usize = 2;
/// Time between two passes over a site.
const PAUSE_BETWEEN_PASSES: Duration = Duration::from_secs(3600);
/// The copy stops when the disk of the subtitles has less than this free, so that the
/// other services on the machine keep room to work.
const KEEP_FREE_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// The bytes that are free on the disk of `path`. `None` where the system does not say.
#[cfg(unix)]
fn free_bytes(path: &Path) -> Option<u64> {
    let stat = rustix::fs::statvfs(path).ok()?;
    Some(stat.f_bavail.saturating_mul(stat.f_frsize))
}

/// The bytes that are free on the disk of `path`. `None` where the system does not say.
#[cfg(not(unix))]
fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// True if the copy can write more: the disk has at least `KEEP_FREE_BYTES` free, or the
/// system does not say how much it has.
fn room_left(free: Option<u64>) -> bool {
    free.is_none_or(|free| free >= KEEP_FREE_BYTES)
}

/// The API of one site. It waits when the site says that the limit of requests is reached.
struct Api<'a> {
    client: &'a reqwest::Client,
    mirror: &'a Mirror,
}

impl Api<'_> {
    /// One request to the API. When the site says that no request is left, this waits
    /// until the limit resets, so that the next request is not refused.
    async fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let url = self.mirror.at(path);
        for _ in 0..5 {
            let response = self
                .client
                .get(&url)
                .header(AUTHORIZATION, self.mirror.api_key.as_str())
                .header(USER_AGENT, USER_AGENT_VALUE)
                .send()
                .await
                .with_context(|| format!("could not reach {url}"))?;
            let pause = pause_after(response.headers());
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                tokio::time::sleep(pause.unwrap_or(Duration::from_secs(5))).await;
                continue;
            }
            let value = response
                .error_for_status()
                .with_context(|| format!("{url} refused the request"))?
                .json()
                .await
                .with_context(|| format!("{url} did not answer as expected"))?;
            if let Some(pause) = pause {
                tokio::time::sleep(pause).await;
            }
            return Ok(value);
        }
        bail!("{url} refused five times: the limit of requests did not reset")
    }
}

/// How long to wait before the next request, from the rate limit headers of a jimaku
/// site: when no request is left, until the limit resets. `None` when a request is left.
fn pause_after(headers: &HeaderMap) -> Option<Duration> {
    let number = |name: &str| headers.get(name)?.to_str().ok()?.trim().parse::<f64>().ok();
    if number("x-ratelimit-remaining")? > 0.0 {
        return None;
    }
    let seconds = number("x-ratelimit-reset-after").unwrap_or(60.0).clamp(0.0, 120.0);
    Some(Duration::from_secs_f64(seconds + 0.5))
}

/// True if the name of a file can be a file in the folder of an entry, and nowhere else.
fn is_safe_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
}

/// One pass over the site: makes the entries that are new, copies the files that are new.
/// A fault in one entry is logged and counted, and the pass goes on with the next entry.
pub async fn sync_once(site: &Site<'_>, mirror: &Mirror) -> anyhow::Result<PassReport> {
    let api = Api {
        client: site.client,
        mirror,
    };
    let mut remote: Vec<RemoteEntry> = api.get("/api/entries/search?anime=true").await?;
    remote.extend(api.get::<Vec<RemoteEntry>>("/api/entries/search?anime=false").await?);
    remote.sort_by_key(|entry| entry.id);
    remote.dedup_by_key(|entry| entry.id);
    let mut report = PassReport {
        entries: remote.len(),
        ..Default::default()
    };

    let host = mirror.host().to_owned();
    let mut known = known(site, mirror, &host).await?;
    let staging = site.config.subtitle_path.join(".mirror");
    for entry in remote {
        let copy = match copy_of(site, mirror, &host, &entry, &mut known, &mut report).await {
            Ok(copy) => copy,
            Err(e) => {
                report.failed += 1;
                warn!(site = host, entry = entry.id, error = %e, "could not make the copy of an entry");
                continue;
            }
        };
        if copy.last_modified.is_some_and(|copied| copied >= entry.stamp()) {
            continue;
        }
        if !room_left(free_bytes(&site.config.subtitle_path)) {
            report.disk_full = true;
            warn!(
                site = host,
                "the disk is nearly full: the copy stops until the next pass"
            );
            break;
        }
        match copy_files(site, &api, &host, &entry, &copy, &staging).await {
            Ok(files) => {
                report.copied += 1;
                report.files += files;
                if let Some(copy) = known.copies.get_mut(&entry.id) {
                    copy.last_modified = Some(entry.stamp());
                }
            }
            Err(e) => {
                report.failed += 1;
                warn!(site = host, entry = entry.id, error = %e, "could not copy the files of an entry");
            }
        }
    }
    Ok(report)
}

/// Reads what the pass must know about the entries here.
async fn known(site: &Site<'_>, mirror: &Mirror, host: &str) -> anyhow::Result<Known> {
    let host = host.to_owned();
    let (copies, any_copy) = site
        .database
        .call(move |conn| -> rusqlite::Result<(HashMap<i64, Copied>, HashSet<i64>)> {
            let mut stmt = conn.prepare(
                "SELECT mirror.site, mirror.remote_id, mirror.entry_id, mirror.last_modified, directory_entry.path
                 FROM mirror INNER JOIN directory_entry ON directory_entry.id = mirror.entry_id",
            )?;
            let mut copies = HashMap::new();
            let mut any_copy = HashSet::new();
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let entry_id: i64 = row.get("entry_id")?;
                any_copy.insert(entry_id);
                if row.get::<_, String>("site")? == host {
                    let path: String = row.get("path")?;
                    let copy = Copied {
                        entry_id,
                        path: PathBuf::from(path),
                        last_modified: row.get("last_modified")?,
                    };
                    copies.insert(row.get("remote_id")?, copy);
                }
            }
            Ok((copies, any_copy))
        })
        .await?;
    let entries: Vec<DirectoryEntry> = site.database.all("SELECT * FROM directory_entry", []).await?;
    let entries: HashMap<i64, DirectoryEntry> = entries.into_iter().map(|entry| (entry.id, entry)).collect();
    // A show that is here already in the language of the site, made by hand or by an older
    // copy, becomes the copy: one show in one language is one entry. A book is not a show.
    let mut adoptable = HashMap::new();
    for entry in entries.values() {
        let kind = entry.kind_of(site.config);
        if kind == Kind::Book || any_copy.contains(&entry.id) || entry.language_code(site.config) != mirror.language {
            continue;
        }
        for key in keys(entry.anilist_id, entry.tmdb_id, entry.bangumi_id, kind, &entry.name) {
            adoptable.insert(key, entry.id);
        }
    }
    Ok(Known {
        copies,
        entries,
        adoptable,
    })
}

/// The entry here that is the copy of the entry of the site. Makes it if there is none,
/// and writes what the site says about the entry over what the entry here says.
async fn copy_of(
    site: &Site<'_>,
    mirror: &Mirror,
    host: &str,
    entry: &RemoteEntry,
    known: &mut Known,
    report: &mut PassReport,
) -> anyhow::Result<Copied> {
    let copy = match known.copies.get(&entry.id) {
        Some(copy) => copy.clone(),
        None => {
            let adopted = entry.keys().iter().find_map(|key| known.adoptable.get(key).copied());
            let copy = match adopted.and_then(|id| known.entries.get(&id)) {
                Some(local) => adopt(site.database, host, entry, local).await?,
                None => {
                    let local = make(site, mirror, host, entry).await?;
                    report.made += 1;
                    let copy = Copied {
                        entry_id: local.id,
                        path: local.path.clone(),
                        last_modified: None,
                    };
                    known.entries.insert(local.id, local);
                    copy
                }
            };
            known.adoptable.retain(|_, id| *id != copy.entry_id);
            known.copies.insert(entry.id, copy.clone());
            copy
        }
    };
    if let Some(local) = known.entries.get_mut(&copy.entry_id)
        && update_details(site, mirror, entry, local).await?
    {
        site.cache.invalidate().await;
    }
    Ok(copy)
}

/// Takes an entry that is here already as the copy of the entry of the site.
async fn adopt(database: &Database, host: &str, entry: &RemoteEntry, local: &DirectoryEntry) -> anyhow::Result<Copied> {
    database
        .execute(
            "INSERT INTO mirror(site, remote_id, entry_id, last_modified) VALUES (?, ?, ?, NULL)",
            (host.to_owned(), entry.id, local.id),
        )
        .await?;
    info!(
        site = host,
        entry = entry.id,
        local = local.id,
        "an entry that was here is the copy"
    );
    Ok(Copied {
        entry_id: local.id,
        path: local.path.clone(),
        last_modified: None,
    })
}

/// The folders that the copy of an entry can have, in the order to try them: the folder
/// the site itself would make, then the same with the language, then with the ID too.
fn folder_candidates(config: &Config, mirror: &Mirror, entry: &RemoteEntry) -> [PathBuf; 3] {
    let ids = PathIds {
        anilist_id: entry.anilist_id,
        tmdb_id: entry.tmdb_id,
        book_id: None,
        bangumi_id: entry.bangumi_id,
    };
    let base = directory_entry_path(ids, &entry.name, entry.flags.is_anime(), config);
    let with = |suffix: String| {
        let mut name = base.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(suffix);
        base.with_file_name(name)
    };
    let with_language = with(format!(" [{}]", mirror.language));
    let with_id = with(format!(" [{}] {}", mirror.language, entry.id));
    [base, with_language, with_id]
}

/// Makes the entry here, and its folder.
async fn make(site: &Site<'_>, mirror: &Mirror, host: &str, entry: &RemoteEntry) -> anyhow::Result<DirectoryEntry> {
    let candidates = folder_candidates(site.config, mirror, entry);
    let values = (
        entry.last_modified,
        entry.flags,
        entry.anilist_id,
        entry.tmdb_id,
        entry.bangumi_id,
        entry.notes(mirror),
        entry.name.clone(),
        known_name(&entry.english_name),
        known_name(&entry.japanese_name),
        mirror.language.clone(),
        entry.kind(),
    );
    let host = host.to_owned();
    let remote_id = entry.id;
    let local = site
        .database
        .call(move |conn| -> anyhow::Result<DirectoryEntry> {
            let (last_modified, flags, anilist_id, tmdb_id, bangumi_id, notes, name, english, japanese, language, kind) =
                values;
            let tx = conn.transaction()?;
            let mut made: Option<DirectoryEntry> = None;
            for path in candidates {
                let path = path
                    .to_str()
                    .with_context(|| format!("the path {} is not UTF-8", path.display()))?
                    .to_owned();
                let result = tx.query_row(
                    "INSERT INTO directory_entry(path, last_updated_at, flags, anilist_id, tmdb_id, bangumi_id, notes, name,
                                                 english_name, japanese_name, language, kind)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                     RETURNING *",
                    rusqlite::params![
                        path,
                        last_modified,
                        flags,
                        anilist_id,
                        tmdb_id,
                        bangumi_id,
                        notes,
                        name,
                        english,
                        japanese,
                        language,
                        kind
                    ],
                    DirectoryEntry::from_row,
                );
                match result {
                    Ok(entry) => {
                        made = Some(entry);
                        break;
                    }
                    // Another entry has this folder. Try the next name.
                    Err(e) if is_unique_constraint_violation(&e) && e.to_string().contains("directory_entry.path") => {}
                    Err(e) => return Err(e.into()),
                }
            }
            let local = made.context("each folder name for the entry is taken")?;
            tx.execute(
                "INSERT INTO mirror(site, remote_id, entry_id, last_modified) VALUES (?, ?, ?, NULL)",
                (host, remote_id, local.id),
            )?;
            tx.commit()?;
            Ok(local)
        })
        .await?;
    tokio::fs::create_dir_all(&local.path)
        .await
        .with_context(|| format!("could not make the folder {}", local.path.display()))?;
    site.cache.invalidate().await;
    Ok(local)
}

/// A name that the site has: not missing, not empty.
fn known_name(name: &Option<String>) -> Option<String> {
    name.as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// Writes what the site says about the entry over what the entry here says, when it
/// differs: the names, the flags, the IDs, the notes. Returns true if something changed.
///
/// Two things stay as they are here. The kind: the site gives it when the copy is made,
/// then an editor here can put the copy in the other tab. dung.live, for one, calls each
/// entry that it made from a folder an anime. And a name that the site does not have:
/// an editor here can add the English name of a show that has none on the site.
async fn update_details(
    site: &Site<'_>,
    mirror: &Mirror,
    entry: &RemoteEntry,
    local: &mut DirectoryEntry,
) -> anyhow::Result<bool> {
    let mut flags = entry.flags;
    // An editor here marks the subtitles that a person reviewed. The site does not know that mark.
    flags.set_reviewed(local.flags.is_reviewed());
    let kind = local.kind.or(Some(entry.kind()));
    flags.set_anime(kind == Some(Kind::Anime));
    let notes = Some(entry.notes(mirror));
    let language = Some(mirror.language.clone());
    let english_name = known_name(&entry.english_name).or_else(|| local.english_name.clone());
    let japanese_name = known_name(&entry.japanese_name).or_else(|| local.japanese_name.clone());
    let same = local.name == entry.name
        && local.english_name == english_name
        && local.japanese_name == japanese_name
        && local.flags == flags
        && local.anilist_id == entry.anilist_id
        && local.tmdb_id == entry.tmdb_id
        && local.bangumi_id == entry.bangumi_id
        && local.notes == notes
        && local.language == language
        && local.kind == kind;
    if same {
        return Ok(false);
    }
    site.database
        .execute(
            "UPDATE directory_entry
             SET name = ?, english_name = ?, japanese_name = ?, flags = ?, anilist_id = ?, tmdb_id = ?,
                 bangumi_id = ?, notes = ?, language = ?, kind = ?
             WHERE id = ?",
            (
                entry.name.clone(),
                english_name.clone(),
                japanese_name.clone(),
                flags,
                entry.anilist_id,
                entry.tmdb_id,
                entry.bangumi_id,
                notes.clone(),
                language.clone(),
                kind,
                local.id,
            ),
        )
        .await?;
    local.name = entry.name.clone();
    local.english_name = english_name;
    local.japanese_name = japanese_name;
    local.flags = flags;
    local.anilist_id = entry.anilist_id;
    local.tmdb_id = entry.tmdb_id;
    local.bangumi_id = entry.bangumi_id;
    local.notes = notes;
    local.language = language;
    local.kind = kind;
    Ok(true)
}

/// Copies the files of the entry that are not here yet, then marks the copy complete.
/// Returns how many files were copied.
async fn copy_files(
    site: &Site<'_>,
    api: &Api<'_>,
    host: &str,
    entry: &RemoteEntry,
    copy: &Copied,
    staging: &Path,
) -> anyhow::Result<usize> {
    let files: Vec<RemoteFile> = api.get(&format!("/api/entries/{}/files", entry.id)).await?;
    tokio::fs::create_dir_all(&copy.path)
        .await
        .with_context(|| format!("could not make the folder {}", copy.path.display()))?;
    tokio::fs::create_dir_all(staging).await?;
    let mut files: Vec<RemoteFile> = files
        .into_iter()
        .filter(|file| {
            let safe = is_safe_file_name(&file.name);
            if !safe {
                warn!(
                    site = host,
                    entry = entry.id,
                    name = file.name,
                    "a file with such a name is not copied"
                );
            }
            safe
        })
        .collect();
    // A file that is here with the same size (before compression) is not copied again.
    let folder = copy.path.clone();
    let wanted = tokio::task::spawn_blocking(move || {
        files.retain(|file| {
            let here = crate::store::find(&folder, &file.name).and_then(|path| crate::store::size(&path).ok());
            here != Some(file.size)
        });
        files
    })
    .await?;

    let mut set = JoinSet::new();
    let mut wanted = wanted.into_iter().enumerate();
    let mut copied = 0;
    loop {
        while set.len() < DOWNLOADS_AT_ONCE {
            let Some((index, file)) = wanted.next() else { break };
            if !room_left(free_bytes(staging)) {
                bail!("the disk is nearly full");
            }
            // The address is made here, not taken from the answer: the copy asks the site only.
            let url = api.mirror.at(&format!(
                "/entry/{}/download/{}",
                entry.id,
                percent_encode(file.name.as_bytes(), FRAGMENT)
            ));
            let part = staging.join(format!("{}-{index}.part", copy.entry_id));
            set.spawn(download(
                site.client.clone(),
                url,
                file,
                copy.path.clone(),
                part,
                staging.to_path_buf(),
            ));
        }
        match set.join_next().await {
            Some(result) => {
                result??;
                copied += 1;
            }
            None => break,
        }
    }

    site.database
        .execute(
            "UPDATE directory_entry SET last_updated_at = MAX(last_updated_at, ?) WHERE id = ?",
            (entry.last_modified, copy.entry_id),
        )
        .await?;
    site.database
        .execute(
            "UPDATE mirror SET last_modified = ? WHERE site = ? AND remote_id = ?",
            (entry.stamp(), host.to_owned(), entry.id),
        )
        .await?;
    site.cache.invalidate().await;
    Ok(copied)
}

/// Copies one file: to `part` first, then into the folder of the entry when it is whole.
/// A file that is half here is never listed. A text subtitle is kept compressed (see `store`).
async fn download(
    client: reqwest::Client,
    url: String,
    file: RemoteFile,
    folder: PathBuf,
    part: PathBuf,
    staging: PathBuf,
) -> anyhow::Result<()> {
    let response = client
        .get(&url)
        .header(USER_AGENT, USER_AGENT_VALUE)
        .send()
        .await
        .with_context(|| format!("could not reach {url}"))?
        .error_for_status()
        .with_context(|| format!("{url} was refused"))?;
    let mut out = tokio::fs::File::create(&part)
        .await
        .with_context(|| format!("could not make {}", part.display()))?;
    let mut stream = response.bytes_stream();
    let mut written = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        written += chunk.len() as u64;
        out.write_all(&chunk).await?;
    }
    out.flush().await?;
    drop(out);
    if written != file.size {
        let _ = tokio::fs::remove_file(&part).await;
        bail!("{}: got {written} bytes, the site says {}", file.name, file.size);
    }
    // The date of the file here is the date of the file there, as the listing shows it.
    let modified = file.last_modified.into();
    if crate::store::is_compressible(&file.name) {
        let stored = tokio::task::spawn_blocking(move || {
            let data = std::fs::read(&part)?;
            let _ = std::fs::remove_file(&part);
            crate::store::put(&folder, &file.name, &data, modified, &staging)
        })
        .await?;
        stored.context("could not keep a subtitle")?;
        return Ok(());
    }
    std::fs::File::options()
        .write(true)
        .open(&part)?
        .set_modified(modified)?;
    tokio::fs::rename(&part, folder.join(&file.name))
        .await
        .with_context(|| format!("could not move {} into {}", file.name, folder.display()))?;
    Ok(())
}

/// Compresses the text subtitles that the copies of the other sites hold as plain files.
/// The files that the copy wrote before the store compressed them become smaller. Returns
/// how many files were compressed and the bytes saved.
pub async fn compress_copies(database: &Database, staging: &Path) -> anyhow::Result<(usize, u64)> {
    let folders: Vec<String> = database
        .call(|conn| -> rusqlite::Result<Vec<String>> {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT directory_entry.path FROM mirror
                 INNER JOIN directory_entry ON directory_entry.id = mirror.entry_id",
            )?;
            let rows = stmt.query_map([], |row| row.get(0))?;
            rows.collect()
        })
        .await?;
    let (mut files, mut saved) = (0, 0);
    for folder in folders {
        let staging = staging.to_path_buf();
        let result =
            tokio::task::spawn_blocking(move || crate::store::compress_folder(Path::new(&folder), &staging)).await?;
        match result {
            Ok((count, bytes)) => {
                files += count;
                saved += bytes;
            }
            Err(e) => warn!(error = %e, "could not compress the subtitles of a copy"),
        }
    }
    Ok((files, saved))
}

/// Runs `compress_copies` once, when the server starts, next to the copy.
pub async fn compress_copies_at_start(state: AppState) {
    let staging = state.config().subtitle_path.join(".mirror");
    match compress_copies(state.database(), &staging).await {
        Ok((files, saved)) => info!(files, saved_bytes = saved, "compressed the subtitles of the copies"),
        Err(e) => warn!(error = %e, "could not compress the subtitles of the copies"),
    }
}

/// Copies the site when the server starts, then once an hour.
pub async fn mirror_loop(state: AppState, mut mirror: Mirror) {
    let Some(language) = crate::language::code(&mirror.language) else {
        warn!(
            site = mirror.host(),
            language = mirror.language,
            "the language of a mirror is not an ISO 639-1 code: the site is not copied"
        );
        return;
    };
    mirror.language = language.to_owned();
    let site = Site {
        config: state.config(),
        database: state.database(),
        client: &state.client,
        cache: state.cached_directories(),
    };
    loop {
        match sync_once(&site, &mirror).await {
            Ok(report) => info!(site = mirror.host(), ?report, "mirrored the site"),
            Err(e) => warn!(site = mirror.host(), error = %e, "could not mirror the site"),
        }
        tokio::time::sleep(PAUSE_BETWEEN_PASSES).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{Path as AxumPath, Query, State},
        routing::get,
    };
    use std::sync::{Arc, Mutex};

    fn mirror(url: &str, language: &str) -> Mirror {
        Mirror {
            url: url.to_owned(),
            language: language.to_owned(),
            api_key: "key".to_owned(),
        }
    }

    #[test]
    fn the_site_is_named_without_its_scheme() {
        let mirror = mirror("https://jimaku.cc/", "ja");
        assert_eq!(mirror.host(), "jimaku.cc");
        assert_eq!(mirror.at("/api/entries/1"), "https://jimaku.cc/api/entries/1");
    }

    #[test]
    fn the_answer_of_the_api_is_read() {
        // What jimaku.cc answers, shortened.
        let json = r#"[{"id":1,"name":"Sousou no Frieren","flags":{"anime":true,"unverified":false,"external":false,"movie":false,"adult":false},"last_modified":"2025-02-05T22:26:56.816055878Z","anilist_id":154587,"english_name":"Frieren","japanese_name":"葬送のフリーレン"},
                      {"id":2,"name":"Alice in Borderland","flags":{"anime":false,"unverified":true,"external":true,"movie":false,"adult":false},"last_modified":"2024-01-01T00:00:00Z","tmdb_id":"tv:110316","notes":"S2 in the other folder"}]"#;
        let entries: Vec<RemoteEntry> = serde_json::from_str(json).unwrap();
        assert_eq!(entries[0].kind(), Kind::Anime);
        assert_eq!(entries[0].anilist_id, Some(154587));
        assert_eq!(
            entries[0].stamp() % 1_000_000_000,
            816_055_878,
            "the nanoseconds are kept"
        );
        assert_eq!(entries[0].keys(), [Key::AniList(154587)]);
        assert_eq!(entries[1].kind(), Kind::Drama);
        assert_eq!(entries[1].tmdb_id, Some(tmdb::Id::Tv { id: 110316 }));
        assert!(entries[1].flags.is_unverified() && entries[1].flags.is_external());
        let mirror = mirror("https://jimaku.cc", "ja");
        assert_eq!(
            entries[1].notes(&mirror),
            "S2 in the other folder\n\nMirror of [jimaku.cc/entry/2](https://jimaku.cc/entry/2)."
        );
        assert_eq!(
            entries[0].notes(&mirror),
            "Mirror of [jimaku.cc/entry/1](https://jimaku.cc/entry/1)."
        );

        let file = r#"{"url":"https://jimaku.cc/entry/1/download/%5BJPJ%5D%20ep01.srt","name":"[JPJ] ep01.srt","size":144746,"last_modified":"2025-02-05T22:26:56.816055878Z"}"#;
        let file: RemoteFile = serde_json::from_str(file).unwrap();
        assert_eq!(file.size, 144746);
        assert_eq!(file.last_modified.year(), 2025);
    }

    #[test]
    fn a_name_finds_a_show_only_when_there_is_no_id() {
        let with_id = keys(Some(1), None, None, Kind::Anime, "Monster");
        assert_eq!(with_id, [Key::AniList(1)]);
        let without = keys(None, None, None, Kind::Drama, "Monster");
        assert_eq!(without, [Key::Named(Kind::Drama, "Monster".to_owned())]);
        assert_ne!(
            keys(None, None, None, Kind::Anime, "Monster"),
            without,
            "an anime and a live action show with one name are two shows"
        );
    }

    #[test]
    fn the_wait_comes_from_the_rate_limit_headers() {
        let mut headers = HeaderMap::new();
        assert_eq!(pause_after(&headers), None, "no headers, no wait");
        headers.insert("x-ratelimit-remaining", "3".parse().unwrap());
        assert_eq!(pause_after(&headers), None);
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        headers.insert("x-ratelimit-reset-after", "2.5".parse().unwrap());
        assert_eq!(pause_after(&headers), Some(Duration::from_secs_f64(3.0)));
        headers.remove("x-ratelimit-reset-after");
        assert_eq!(pause_after(&headers), Some(Duration::from_secs_f64(60.5)));
        headers.insert("x-ratelimit-reset-after", "9999".parse().unwrap());
        assert_eq!(pause_after(&headers), Some(Duration::from_secs_f64(120.5)));
    }

    #[test]
    fn the_copy_stops_before_the_disk_is_full() {
        assert!(room_left(None), "a system that does not say is not stopped");
        assert!(room_left(Some(KEEP_FREE_BYTES)));
        assert!(!room_left(Some(KEEP_FREE_BYTES - 1)));
        assert!(!room_left(Some(0)));
    }

    #[test]
    fn only_a_plain_file_name_is_copied() {
        assert!(is_safe_file_name("[JPJ] ep01.srt"));
        assert!(is_safe_file_name("葬送のフリーレン 01.ass"));
        for bad in [
            "",
            ".",
            "..",
            "../x.srt",
            "a/b.srt",
            "a\\b.srt",
            "a\nb.srt",
            &"x".repeat(256),
        ] {
            assert!(!is_safe_file_name(bad), "{bad:?}");
        }
    }

    /// An entry of a fake jimaku site, and its files: (name, bytes, date).
    #[derive(Clone)]
    struct FakeEntry {
        id: i64,
        name: String,
        anime: bool,
        anilist_id: Option<u32>,
        tmdb_id: Option<&'static str>,
        notes: Option<&'static str>,
        english_name: Option<&'static str>,
        japanese_name: Option<&'static str>,
        last_modified: &'static str,
        files: Vec<(&'static str, &'static [u8], &'static str)>,
    }

    impl FakeEntry {
        fn new(id: i64, name: &str, anime: bool, last_modified: &'static str) -> Self {
            Self {
                id,
                name: name.to_owned(),
                anime,
                anilist_id: None,
                tmdb_id: None,
                notes: None,
                english_name: None,
                japanese_name: None,
                last_modified,
                files: Vec::new(),
            }
        }
    }

    #[derive(Default)]
    struct Fake {
        entries: Vec<FakeEntry>,
        /// Each request that the fake site answered.
        calls: Vec<String>,
    }

    type Shared = Arc<Mutex<Fake>>;

    #[derive(Deserialize)]
    struct Anime {
        anime: bool,
    }

    async fn search(State(fake): State<Shared>, Query(query): Query<Anime>) -> Json<serde_json::Value> {
        let mut fake = fake.lock().unwrap();
        fake.calls.push(format!("search?anime={}", query.anime));
        let entries: Vec<_> = fake
            .entries
            .iter()
            .filter(|entry| entry.anime == query.anime)
            .map(|entry| {
                serde_json::json!({
                    "id": entry.id, "name": entry.name,
                    "flags": {"anime": entry.anime, "unverified": false, "external": false, "movie": false, "adult": false},
                    "last_modified": entry.last_modified, "anilist_id": entry.anilist_id, "tmdb_id": entry.tmdb_id,
                    "notes": entry.notes, "creator_id": 7,
                    "english_name": entry.english_name, "japanese_name": entry.japanese_name,
                })
            })
            .collect();
        Json(serde_json::Value::Array(entries))
    }

    async fn files(State(fake): State<Shared>, AxumPath(id): AxumPath<i64>) -> Json<serde_json::Value> {
        let mut fake = fake.lock().unwrap();
        fake.calls.push(format!("files/{id}"));
        let files: Vec<_> = fake
            .entries
            .iter()
            .filter(|entry| entry.id == id)
            .flat_map(|entry| entry.files.iter())
            .map(|(name, body, date)| {
                // The address that a real site gives. The copy must not use it.
                serde_json::json!({"url": "http://elsewhere.invalid/x", "name": name, "size": body.len(), "last_modified": date})
            })
            .collect();
        Json(serde_json::Value::Array(files))
    }

    async fn file(State(fake): State<Shared>, AxumPath((id, name)): AxumPath<(i64, String)>) -> Vec<u8> {
        let mut fake = fake.lock().unwrap();
        fake.calls.push(format!("download/{id}/{name}"));
        fake.entries
            .iter()
            .filter(|entry| entry.id == id)
            .flat_map(|entry| entry.files.iter())
            .find(|(n, _, _)| *n == name)
            .map(|(_, body, _)| body.to_vec())
            .unwrap_or_default()
    }

    /// Starts the fake site on a free port. Returns its address.
    async fn serve(fake: Shared) -> String {
        let app = Router::new()
            .route("/api/entries/search", get(search))
            .route("/api/entries/{id}/files", get(files))
            .route("/entry/{id}/download/{name}", get(file))
            .with_state(fake);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }

    struct TestSite {
        root: PathBuf,
        config: Config,
        database: Database,
        client: reqwest::Client,
        cache: TimedCachedValue<Vec<DirectoryEntry>>,
    }

    impl TestSite {
        async fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("honjimaku-mirror-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("subtitles")).unwrap();
            let mut config = Config::new().unwrap();
            config.book_site = true;
            config.subtitle_language = Some("ja".to_owned());
            config.subtitle_path = root.join("subtitles");
            // One connection: two that switch a new, empty database to WAL at the same
            // time can lock each other out. A database in use is in WAL already.
            let database = Database::file(root.join("main.db"))
                .connections(1)
                .with_init(crate::database::init)
                .open()
                .await
                .unwrap();
            Self {
                root,
                config,
                database,
                client: reqwest::Client::new(),
                cache: TimedCachedValue::new(Duration::from_secs(60)),
            }
        }

        fn site(&self) -> Site<'_> {
            Site {
                config: &self.config,
                database: &self.database,
                client: &self.client,
                cache: &self.cache,
            }
        }

        async fn entries(&self) -> Vec<DirectoryEntry> {
            self.database
                .all("SELECT * FROM directory_entry ORDER BY id", [])
                .await
                .unwrap()
        }

        async fn add(&self, sql: &'static str, folder: &str) {
            let path = self.config.subtitle_path.join(folder).to_str().unwrap().to_owned();
            self.database.execute(sql, [path]).await.unwrap();
        }
    }

    fn calls(fake: &Shared) -> Vec<String> {
        std::mem::take(&mut fake.lock().unwrap().calls)
    }

    #[tokio::test]
    async fn the_site_is_copied_then_only_what_changes() {
        let mut frieren = FakeEntry::new(10, "Sousou no Frieren", true, "2025-02-05T22:26:56.816055878Z");
        frieren.anilist_id = Some(154587);
        frieren.notes = Some("The site says so");
        // A subtitle large enough to compress, like the typesetting of an opening.
        let opening: &'static [u8] = Box::leak(
            "Dialogue: 0,0:00:01.00,0:00:02.00,OP,,0,0,0,,{\\pos(640,360)\\fad(200,200)}葬送のフリーレン\n"
                .repeat(600)
                .into_bytes()
                .into_boxed_slice(),
        );
        frieren.files = vec![
            ("ep01.srt", b"first", "2025-02-01T00:00:00Z"),
            ("ep 02 [JPJ].srt", b"second!", "2025-02-05T22:26:56Z"),
            ("op.ass", opening, "2025-02-01T00:00:00Z"),
        ];
        let mut alice = FakeEntry::new(11, "Alice in Borderland", false, "2024-01-01T00:00:00Z");
        alice.tmdb_id = Some("tv:110316");
        alice.files = vec![
            ("s01e01.srt", b"drama", "2024-01-01T00:00:00Z"),
            ("../evil.srt", b"no", "2024-01-01T00:00:00Z"),
        ];
        let named = FakeEntry::new(12, "Named by an editor", true, "2024-06-01T00:00:00Z");
        let fake: Shared = Arc::new(Mutex::new(Fake {
            entries: vec![frieren, alice, named],
            calls: Vec::new(),
        }));
        let base = serve(fake.clone()).await;
        let test = TestSite::new("copy").await;
        let mirror = mirror(&base, "zh");
        // A show that a user made here in 2020, before the copy, in the language of the
        // site, and reviewed by an editor here.
        test.add(
            "INSERT INTO directory_entry(path, last_updated_at, flags, name, anilist_id, language)
             VALUES (?, '2020-01-01 00:00:00', 33, 'Named by hand', 154587, 'zh')",
            "Named by hand",
        )
        .await;
        // A book with the name of a show is not the show.
        test.add(
            "INSERT INTO directory_entry(path, flags, name, language) VALUES (?, 1, 'Named by an editor', 'zh')",
            "a book",
        )
        .await;

        let report = sync_once(&test.site(), &mirror).await.unwrap();
        assert_eq!(
            report,
            PassReport {
                entries: 3,
                made: 2,
                copied: 3,
                files: 4,
                failed: 0,
                disk_full: false,
            }
        );
        let entries = test.entries().await;
        assert_eq!(entries.len(), 4, "{entries:#?}");
        let copy = &entries[0];
        assert_eq!(copy.name, "Sousou no Frieren", "the entry made by hand is the copy now");
        assert_eq!(copy.kind, Some(Kind::Anime));
        assert_eq!(copy.language.as_deref(), Some("zh"));
        assert!(copy.flags.is_reviewed(), "the mark of the editor here stays");
        let notes = format!(
            "The site says so\n\nMirror of [{}/entry/10]({base}/entry/10).",
            mirror.host()
        );
        assert_eq!(copy.notes.as_deref(), Some(notes.as_str()));
        assert_eq!(copy.last_updated_at.year(), 2025, "the newer date of the site");
        assert_eq!(std::fs::read(copy.path.join("ep 02 [JPJ].srt")).unwrap(), b"second!");
        let modified: OffsetDateTime = std::fs::metadata(copy.path.join("ep01.srt"))
            .unwrap()
            .modified()
            .unwrap()
            .into();
        assert_eq!(modified.month() as u8, 2, "the file has the date of the site");
        // The large subtitle is kept compressed, and it gives back the bytes of the site.
        assert!(copy.path.join("op.ass.zst").is_file() && !copy.path.join("op.ass").exists());
        let kept = crate::store::find(&copy.path, "op.ass").unwrap();
        assert!(std::fs::metadata(&kept).unwrap().len() * 5 < opening.len() as u64);
        assert_eq!(crate::store::read(&kept).unwrap(), opening);
        assert_eq!(crate::store::size(&kept).unwrap(), opening.len() as u64);
        let modified: OffsetDateTime = std::fs::metadata(&kept).unwrap().modified().unwrap().into();
        assert_eq!(
            modified.month() as u8,
            2,
            "the compressed file has the date of the site too"
        );
        assert_eq!(entries[1].kind_of(&test.config), Kind::Book, "the book is left alone");
        let alice = entries.iter().find(|e| e.tmdb_id.is_some()).unwrap();
        assert_eq!(alice.kind, Some(Kind::Drama));
        assert_eq!(
            alice.last_updated_at.year(),
            2024,
            "a new entry has the date of the site"
        );
        // The folder is named as the site names it: the ":" of "tv:110316" cannot be in a file name.
        let folder = alice.path.file_name().unwrap().to_str().unwrap();
        assert_eq!(folder, "[drama] Alice in Borderland [tv_110316]");
        assert!(alice.path.join("s01e01.srt").is_file());
        assert!(!test.config.subtitle_path.join("evil.srt").exists());
        let named = entries.iter().find(|e| e.id == 4).unwrap();
        assert_eq!(named.name, "Named by an editor");
        assert_eq!(named.kind, Some(Kind::Anime));
        let staged = std::fs::read_dir(test.config.subtitle_path.join(".mirror"))
            .unwrap()
            .count();
        assert_eq!(staged, 0, "nothing is left half copied");
        assert!(calls(&fake).iter().all(|call| !call.contains("elsewhere")));

        // Nothing changed: the second pass asks for the lists only.
        let report = sync_once(&test.site(), &mirror).await.unwrap();
        assert_eq!((report.made, report.copied, report.failed), (0, 0, 0));
        assert_eq!(calls(&fake), ["search?anime=true", "search?anime=false"]);

        // A new file on the site: only that entry is looked at, only that file is copied.
        {
            let mut fake = fake.lock().unwrap();
            let entry = fake.entries.iter_mut().find(|e| e.id == 10).unwrap();
            entry.last_modified = "2025-03-01T00:00:00Z";
            entry.files.push(("ep03.srt", b"third", "2025-03-01T00:00:00Z"));
            entry.name = "Sousou no Frieren (renamed)".into();
        }
        let report = sync_once(&test.site(), &mirror).await.unwrap();
        assert_eq!((report.copied, report.files, report.made), (1, 1, 0));
        let now = calls(&fake);
        assert_eq!(now.iter().filter(|c| c.starts_with("download/")).count(), 1, "{now:?}");
        assert!(
            now.contains(&"files/10".to_owned()) && !now.contains(&"files/11".to_owned()),
            "{now:?}"
        );
        let copy = test.entries().await.into_iter().next().unwrap();
        assert_eq!(copy.name, "Sousou no Frieren (renamed)");
        assert_eq!(std::fs::read(copy.path.join("ep03.srt")).unwrap(), b"third");
        assert_eq!(copy.last_updated_at.month() as u8, 3);

        // A subtitle that an older copy kept plain is compressed at the next start.
        std::fs::write(copy.path.join("old.srt"), opening).unwrap();
        std::fs::write(copy.path.join("pack.7z"), opening).unwrap();
        // The book is not the copy of a site: its files stay as they are.
        let book = test.config.subtitle_path.join("a book");
        std::fs::create_dir_all(&book).unwrap();
        std::fs::write(book.join("book.srt"), opening).unwrap();
        let staging = test.config.subtitle_path.join(".mirror");
        let (files, saved) = compress_copies(&test.database, &staging).await.unwrap();
        assert_eq!(files, 1, "only the text subtitle of the copy");
        assert!(saved > opening.len() as u64 / 2, "{saved}");
        assert!(copy.path.join("old.srt.zst").is_file() && !copy.path.join("old.srt").exists());
        assert!(copy.path.join("pack.7z").is_file());
        assert!(book.join("book.srt").is_file() && !book.join("book.srt.zst").exists());
        assert_eq!(crate::store::read(&copy.path.join("old.srt.zst")).unwrap(), opening);
        assert_eq!(compress_copies(&test.database, &staging).await.unwrap(), (0, 0));

        std::fs::remove_dir_all(&test.root).unwrap();
    }

    /// dung.live calls each entry that it made from a folder an anime, and has no English
    /// name for it. An editor here puts a live action show in the Live Action tab and adds
    /// names. The next passes keep that, and still take a name that the site gives.
    #[tokio::test]
    async fn the_copy_keeps_the_tab_and_the_names_that_are_given_here() {
        let mut drama = FakeEntry::new(20, "Meng Qi Shi Shen", true, "2024-01-01T00:00:00Z");
        drama.english_name = Some("");
        drama.japanese_name = Some("Meng Qi Shi Shen");
        let fake: Shared = Arc::new(Mutex::new(Fake {
            entries: vec![drama],
            calls: Vec::new(),
        }));
        let base = serve(fake.clone()).await;
        let test = TestSite::new("sorted").await;
        let mirror = mirror(&base, "zh");

        sync_once(&test.site(), &mirror).await.unwrap();
        let copy = test.entries().await.remove(0);
        assert_eq!(copy.kind, Some(Kind::Anime), "the site says that it is an anime");
        assert!(copy.flags.is_anime());
        assert_eq!(copy.english_name, None, "an empty name is no name");

        test.database
            .execute(
                "UPDATE directory_entry SET kind = 'drama', flags = flags & ~1,
                        english_name = 'Cinderella Chef', other_names = '萌妻食神' WHERE id = ?",
                [copy.id],
            )
            .await
            .unwrap();
        sync_once(&test.site(), &mirror).await.unwrap();
        let copy = test.entries().await.remove(0);
        assert_eq!(copy.kind_of(&test.config), Kind::Drama, "the tab of the editor stays");
        assert!(!copy.flags.is_anime(), "the flag agrees with the tab");
        assert_eq!(copy.english_name.as_deref(), Some("Cinderella Chef"));
        assert_eq!(copy.other_names, ["萌妻食神"]);

        {
            let mut fake = fake.lock().unwrap();
            let entry = &mut fake.entries[0];
            entry.name = "Meng Qi Shi Shen 2".into();
            entry.english_name = Some("Cinderella Chef 2");
        }
        sync_once(&test.site(), &mirror).await.unwrap();
        let copy = test.entries().await.remove(0);
        assert_eq!(copy.name, "Meng Qi Shi Shen 2");
        assert_eq!(
            copy.english_name.as_deref(),
            Some("Cinderella Chef 2"),
            "a name of the site wins"
        );
        assert_eq!(copy.kind, Some(Kind::Drama));
        assert_eq!(
            copy.other_names,
            ["萌妻食神"],
            "the copy does not write the other names"
        );
        std::fs::remove_dir_all(&test.root).unwrap();
    }

    #[tokio::test]
    async fn a_folder_that_is_taken_gets_the_language_in_its_name() {
        let mut monster = FakeEntry::new(5, "Monster", true, "2024-01-01T00:00:00Z");
        monster.anilist_id = Some(19);
        let fake: Shared = Arc::new(Mutex::new(Fake {
            entries: vec![monster],
            calls: Vec::new(),
        }));
        let base = serve(fake).await;
        let test = TestSite::new("taken").await;
        // The Japanese copy of the same anime has the plain folder already.
        test.add(
            "INSERT INTO directory_entry(path, flags, name, anilist_id, language, kind) VALUES (?, 1, 'Monster', 19, 'ja', 'anime')",
            "Monster [19]",
        )
        .await;
        let report = sync_once(&test.site(), &mirror(&base, "zh")).await.unwrap();
        assert_eq!((report.made, report.failed), (1, 0));
        let entries = test.entries().await;
        assert_eq!(entries.len(), 2);
        let chinese = &entries[1];
        assert_eq!(chinese.language.as_deref(), Some("zh"));
        assert!(
            chinese.path.ends_with("Monster [19] [zh]"),
            "{}",
            chinese.path.display()
        );
        assert_eq!(chinese.anilist_id, Some(19), "one AniList ID for each language");
        assert!(chinese.path.is_dir());
        std::fs::remove_dir_all(&test.root).unwrap();
    }

    #[tokio::test]
    async fn a_site_that_is_down_changes_nothing() {
        let test = TestSite::new("down").await;
        // Nothing listens on port 9 of this machine.
        let result = sync_once(&test.site(), &mirror("http://127.0.0.1:9", "ja")).await;
        assert!(result.is_err());
        assert!(test.entries().await.is_empty());
        std::fs::remove_dir_all(&test.root).unwrap();
    }
}

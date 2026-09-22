//! The verifier of a site for Chinese shows: Bangumi (bgm.tv).
//!
//! TMDB and AniList know few Chinese dramas and donghua. Bangumi knows them, and its
//! API answers without a key: the names, the kind of subject, the date. A subject is
//! named by its number, as in `https://bgm.tv/subject/258207`.

use serde::Deserialize;

const API: &str = "https://api.bgm.tv/v0/subjects";
/// Bangumi refuses a request without a User-Agent that names the program.
const USER_AGENT: &str = "honjimaku/0.1 (https://github.com/equwal/honjimaku)";

/// The subject number in what a user typed: the number itself, or a Bangumi URL
/// (`bgm.tv`, `bangumi.tv`, `chii.in`) that ends with it.
pub fn subject_id(raw: &str) -> Option<u32> {
    let raw = raw.trim().trim_end_matches('/');
    let raw = raw.split('?').next().unwrap_or(raw);
    let tail = raw.rsplit('/').next().unwrap_or(raw);
    let id: u32 = tail.parse().ok().filter(|id| *id > 0)?;
    let is_bangumi = raw == tail
        || ["bgm.tv/subject/", "bangumi.tv/subject/", "chii.in/subject/"]
            .iter()
            .any(|h| raw.contains(h));
    is_bangumi.then_some(id)
}

/// The page of the subject.
pub fn url(id: u32) -> String {
    format!("https://bgm.tv/subject/{id}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    pub id: u32,
    /// The name in the original language.
    pub name: String,
    /// The Chinese name, when Bangumi has one.
    pub name_cn: Option<String>,
    /// True for an anime or a donghua (Bangumi type 2), false for a live action show (type 6).
    pub animated: bool,
    pub date: Option<String>,
    pub nsfw: bool,
}

impl Subject {
    /// The name the site lists: the Chinese one when there is one.
    pub fn title(&self) -> &str {
        self.name_cn.as_deref().filter(|s| !s.is_empty()).unwrap_or(&self.name)
    }

    /// A note for the page of the entry.
    pub fn note(&self) -> String {
        let mut note = format!("Bangumi: [{}]({})", self.id, url(self.id));
        if let Some(date) = &self.date {
            note.push_str(". First aired ");
            note.push_str(date);
        }
        note
    }
}

#[derive(Deserialize)]
struct Raw {
    id: u32,
    #[serde(rename = "type")]
    kind: u8,
    name: String,
    #[serde(default)]
    name_cn: Option<String>,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    nsfw: bool,
}

/// What Bangumi answered. The inner `Err` is the reason a subject is refused, for the
/// user: a book, a game or music is not a show.
fn read(json: &str) -> anyhow::Result<Result<Subject, String>> {
    let raw: Raw = serde_json::from_str(json)?;
    let animated = match raw.kind {
        2 => true,
        6 => false,
        _ => return Ok(Err(format!("Bangumi subject {} is not a show.", raw.id))),
    };
    Ok(Ok(Subject {
        id: raw.id,
        name: raw.name,
        name_cn: raw.name_cn,
        animated,
        date: raw.date.filter(|d| !d.is_empty()),
        nsfw: raw.nsfw,
    }))
}

/// Asks Bangumi about a subject. The outer `Err` is a network or server fault. The
/// inner `Err` is the reason for the user: Bangumi has no such subject, or it is not a show.
pub async fn lookup(client: &reqwest::Client, id: u32) -> anyhow::Result<Result<Subject, String>> {
    let response = client
        .get(format!("{API}/{id}"))
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(Err(format!("Bangumi does not know a subject {id}.")));
    }
    let text = response.error_for_status()?.text().await?;
    read(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subject_number_is_read_from_what_a_user_types() {
        assert_eq!(subject_id(" 258207 "), Some(258207));
        assert_eq!(subject_id("https://bgm.tv/subject/258207?tab=ep"), Some(258207));
        assert_eq!(subject_id("https://bangumi.tv/subject/258207/"), Some(258207));
        assert_eq!(subject_id("http://chii.in/subject/1"), Some(1));
        for not in [
            "",
            "0",
            "abc",
            "https://example.com/subject/258207",
            "https://bgm.tv/user/258207",
        ] {
            assert_eq!(subject_id(not), None, "{not}");
        }
    }

    #[test]
    fn the_answer_of_bangumi_is_read() {
        // What api.bgm.tv answered for 258207 on 2026-09-21, shortened.
        let drama = r#"{"date":"2019-06-27","platform":"电视剧","name":"陈情令","name_cn":"","id":258207,"type":6,"nsfw":false,"eps":50}"#;
        let show = read(drama).unwrap().unwrap();
        assert_eq!(show.title(), "陈情令");
        assert!(!show.animated && !show.nsfw);
        assert!(show.note().contains("2019-06-27") && show.note().contains(&url(258207)));

        let anime = r#"{"date":"2019-07-07","name":"からかい上手の高木さん②","name_cn":"擅长捉弄的高木同学 第二季","id":271151,"type":2,"nsfw":false}"#;
        let show = read(anime).unwrap().unwrap();
        assert_eq!(show.title(), "擅长捉弄的高木同学 第二季");
        assert!(show.animated);

        let book = r#"{"name":"a book","id":5,"type":1}"#;
        assert!(read(book).unwrap().unwrap_err().contains("not a show"));
        assert!(read("not json").is_err());
    }
}

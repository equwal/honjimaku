//! Other names of an entry, and the import of names from a file (the `names` command).

use std::fmt;

use rusqlite::OptionalExtension;
use serde::Deserialize;

use crate::{database::Table, models::DirectoryEntry};

/// The largest size in bytes of the other names of an entry, as the column stores them.
/// The edit form has the same limit.
pub const MAX_OTHER_NAMES_LENGTH: usize = 4096;

/// The largest size in bytes of an English name. The edit form has the same limit.
const MAX_ENGLISH_NAME_LENGTH: usize = 1024;

/// Returns true if the two names are the same when case is ignored.
fn same_name(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// Cleans a list of names.
///
/// Each name is trimmed. Empty names are removed. A name is also removed if it is the same
/// (case is ignored) as a name before it or as a name in `skip`. The order stays the same.
pub fn normalize<'a>(names: impl IntoIterator<Item = &'a str>, skip: &[&str]) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    for name in names {
        let name = name.trim();
        let known = skip
            .iter()
            .copied()
            .chain(result.iter().map(String::as_str))
            .any(|other| same_name(other, name));
        if !name.is_empty() && !known {
            result.push(name.to_owned());
        }
    }
    result
}

/// Reads names from text with one name on each line, e.g. the column or the edit form.
///
/// The rules of [`normalize`] apply.
pub fn parse(text: &str, skip: &[&str]) -> Vec<String> {
    // Browsers send the lines of a textarea with "\r\n".
    normalize(text.split(['\r', '\n']), skip)
}

/// Returns the text of the `other_names` column for these names. No names is NULL.
pub fn join(names: &[String]) -> Option<String> {
    (!names.is_empty()).then(|| names.join("\n"))
}

/// A record of the file that the `names` command reads.
///
/// Other fields in the file are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct NameRecord {
    /// The ID of the entry.
    pub id: i64,
    /// The name that the entry must have. If the name is different, the record is skipped.
    pub name: String,
    /// The English name to set if the entry has none.
    #[serde(default)]
    pub english_name: Option<String>,
    /// The other names to add to the entry.
    #[serde(default)]
    pub other_names: Vec<String>,
}

/// The result of a record for one entry.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The entry has a different name, so the record can be for a different entry.
    NameChanged,
    /// The entry has all the names already.
    Unchanged,
    /// The entry gets new names.
    Changed {
        /// The English name to set. `None` keeps the English name.
        english_name: Option<String>,
        /// All the other names after the change.
        other_names: Vec<String>,
        /// The number of other names that were added.
        added: usize,
    },
}

/// Returns the change that a record makes to an entry.
///
/// An English name is only set if the entry has none. It is never replaced.
/// Other names are added after the current ones.
/// The limits of the edit form apply, so that the edit form can still save the entry.
pub fn change(entry: &DirectoryEntry, record: &NameRecord) -> Outcome {
    if entry.name != record.name {
        return Outcome::NameChanged;
    }

    let has_english = entry.english_name.as_deref().is_some_and(|s| !s.trim().is_empty());
    // The English name is one line on the edit form, so a line break becomes a space.
    let english_name = record
        .english_name
        .as_deref()
        .map(|name| name.replace(['\r', '\n'], " ").trim().to_owned())
        .filter(|name| !has_english && !name.is_empty() && name.len() <= MAX_ENGLISH_NAME_LENGTH);

    let mut skip = vec![entry.name.as_str()];
    skip.extend(english_name.as_deref().or(entry.english_name.as_deref()));
    skip.extend(entry.japanese_name.as_deref());
    skip.extend(entry.other_names.iter().map(String::as_str));
    // The column holds one name on each line, so a name with a line break is split as the
    // column is read. Else the name comes back as two names, and the next import adds it again.
    let new_names = normalize(
        record.other_names.iter().flat_map(|name| name.split(['\r', '\n'])),
        &skip,
    );

    let mut other_names = entry.other_names.clone();
    let mut added = 0;
    for name in new_names {
        // The size of the column text after the name is added, with one line break per name.
        let length = other_names.iter().map(|n| n.len() + 1).sum::<usize>() + name.len();
        if length <= MAX_OTHER_NAMES_LENGTH {
            other_names.push(name);
            added += 1;
        }
    }

    if english_name.is_none() && added == 0 {
        return Outcome::Unchanged;
    }

    Outcome::Changed {
        english_name,
        other_names,
        added,
    }
}

/// The counts that the `names` command prints.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub entries_changed: usize,
    pub english_names_added: usize,
    pub other_names_added: usize,
    pub no_such_entry: usize,
    pub name_changed: usize,
    pub unchanged: usize,
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Entries changed:     {}", self.entries_changed)?;
        writeln!(f, "English names added: {}", self.english_names_added)?;
        writeln!(f, "Other names added:   {}", self.other_names_added)?;
        writeln!(f, "No such entry:       {}", self.no_such_entry)?;
        writeln!(f, "Name changed:        {}", self.name_changed)?;
        write!(f, "Unchanged:           {}", self.unchanged)
    }
}

/// Applies the records to the entries in the database.
///
/// The caller gives a transaction and decides if it is committed.
pub fn import(conn: &rusqlite::Connection, records: &[NameRecord]) -> rusqlite::Result<Summary> {
    let mut summary = Summary::default();
    let mut select = conn.prepare("SELECT * FROM directory_entry WHERE id = ?")?;
    for record in records {
        let Some(entry) = select.query_row([record.id], DirectoryEntry::from_row).optional()? else {
            summary.no_such_entry += 1;
            continue;
        };

        match change(&entry, record) {
            Outcome::NameChanged => summary.name_changed += 1,
            Outcome::Unchanged => summary.unchanged += 1,
            Outcome::Changed {
                english_name,
                other_names,
                added,
            } => {
                if let Some(english_name) = english_name {
                    conn.execute(
                        "UPDATE directory_entry SET english_name = ? WHERE id = ?",
                        (english_name, entry.id),
                    )?;
                    summary.english_names_added += 1;
                }
                if added > 0 {
                    conn.execute(
                        "UPDATE directory_entry SET other_names = ? WHERE id = ?",
                        (join(&other_names), entry.id),
                    )?;
                    summary.other_names_added += added;
                }
                summary.entries_changed += 1;
            }
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> DirectoryEntry {
        DirectoryEntry::temporary(name.to_owned())
    }

    fn record(name: &str, english_name: Option<&str>, other_names: &[&str]) -> NameRecord {
        NameRecord {
            id: 0,
            name: name.to_owned(),
            english_name: english_name.map(String::from),
            other_names: other_names.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn normalize_trims_and_removes_empty_and_duplicate_names() {
        let names = normalize(
            [
                "  Frieren ",
                "",
                "   ",
                "frieren",
                "Sousou no Frieren",
                "FRIEREN",
                "Sousou no Frieren",
            ],
            &[],
        );
        assert_eq!(names, ["Frieren", "Sousou no Frieren"]);
    }

    #[test]
    fn normalize_removes_names_in_skip() {
        let names = normalize(
            ["Frieren", "Sousou no Frieren", "葬送のフリーレン"],
            &["sousou no frieren "],
        );
        assert_eq!(names, ["Frieren", "葬送のフリーレン"]);
    }

    #[test]
    fn parse_reads_one_name_on_each_line() {
        let names = parse(
            "Frieren\r\n\r\n Beyond Journey's End, Part 1 \nfrieren\rSousou no Frieren\n",
            &[],
        );
        assert_eq!(names, ["Frieren", "Beyond Journey's End, Part 1", "Sousou no Frieren"]);
        assert_eq!(parse("Frieren\nSousou no Frieren", &["SOUSOU NO FRIEREN"]), ["Frieren"]);
        assert!(parse("", &[]).is_empty());
    }

    /// Each text of up to 6 characters from a small alphabet: the names that are read have no
    /// empty name, no space at an end, no line break and no duplicate, and they are the same
    /// after a write and a read.
    #[test]
    fn parse_and_join_round_trip() {
        const ALPHABET: [char; 5] = ['a', 'A', ' ', '\n', '\r'];
        let mut texts = vec![String::new()];
        for _ in 0..6 {
            texts = texts
                .iter()
                .flat_map(|text| ALPHABET.iter().map(move |c| format!("{text}{c}")))
                .collect();
            for text in &texts {
                let names = parse(text, &[]);
                for (i, name) in names.iter().enumerate() {
                    assert!(!name.is_empty() && name.trim() == name, "{text:?}");
                    assert!(!name.contains(['\n', '\r']), "{text:?}");
                    assert!(!names[..i].iter().any(|n| same_name(n, name)), "{text:?}");
                }
                let written = join(&names);
                assert_eq!(written.is_none(), names.is_empty(), "{text:?}");
                assert_eq!(parse(written.as_deref().unwrap_or_default(), &[]), names, "{text:?}");
            }
        }
    }

    #[test]
    fn change_skips_an_entry_with_a_different_name() {
        let entry = entry("Sousou no Frieren");
        let outcome = change(&entry, &record("Sousou no Frieren 2", Some("Frieren"), &["x"]));
        assert_eq!(outcome, Outcome::NameChanged);
        // The compare is exact.
        let outcome = change(&entry, &record("sousou no frieren", Some("Frieren"), &["x"]));
        assert_eq!(outcome, Outcome::NameChanged);
    }

    #[test]
    fn change_sets_an_english_name_only_when_the_entry_has_none() {
        let mut entry = entry("Sousou no Frieren");
        for current in [None, Some(""), Some("  ")] {
            entry.english_name = current.map(String::from);
            let outcome = change(&entry, &record("Sousou no Frieren", Some(" Frieren "), &[]));
            assert_eq!(
                outcome,
                Outcome::Changed {
                    english_name: Some("Frieren".to_owned()),
                    other_names: vec![],
                    added: 0
                },
                "{current:?}"
            );
        }

        entry.english_name = Some("Frieren: Beyond Journey's End".to_owned());
        let outcome = change(&entry, &record("Sousou no Frieren", Some("Frieren"), &[]));
        assert_eq!(outcome, Outcome::Unchanged);

        entry.english_name = None;
        let outcome = change(&entry, &record("Sousou no Frieren", Some("  "), &[]));
        assert_eq!(outcome, Outcome::Unchanged);
    }

    #[test]
    fn change_adds_other_names_after_the_current_ones() {
        let mut entry = entry("Sousou no Frieren");
        entry.japanese_name = Some("葬送のフリーレン".to_owned());
        entry.other_names = vec!["Frieren".to_owned()];
        let record = record(
            "Sousou no Frieren",
            Some("Frieren: Beyond Journey's End"),
            &[
                " 葬送的芙莉莲 ",
                "",
                "FRIEREN",
                "sousou no frieren",
                "葬送のフリーレン",
                "frieren: beyond journey's end",
                "葬送的芙莉莲",
                "Frieren at the Funeral",
            ],
        );
        assert_eq!(
            change(&entry, &record),
            Outcome::Changed {
                english_name: Some("Frieren: Beyond Journey's End".to_owned()),
                other_names: vec![
                    "Frieren".to_owned(),
                    "葬送的芙莉莲".to_owned(),
                    "Frieren at the Funeral".to_owned()
                ],
                added: 2
            }
        );
    }

    #[test]
    fn change_skips_a_name_equal_to_the_current_english_name() {
        let mut entry = entry("Sousou no Frieren");
        entry.english_name = Some("Frieren".to_owned());
        let outcome = change(&entry, &record("Sousou no Frieren", Some("Other"), &["frieren"]));
        assert_eq!(outcome, Outcome::Unchanged);
    }

    #[test]
    fn change_twice_changes_nothing_the_second_time() {
        let mut entry = entry("Sousou no Frieren");
        let record = record("Sousou no Frieren", Some("Frieren"), &["葬送的芙莉莲", "Frieren"]);
        let Outcome::Changed {
            english_name,
            other_names,
            ..
        } = change(&entry, &record)
        else {
            panic!("the first change must change the entry");
        };
        entry.english_name = english_name;
        entry.other_names = other_names;
        assert_eq!(change(&entry, &record), Outcome::Unchanged);
    }

    /// The column holds one name on each line. An imported name with a line break must be
    /// split the same way, or the next import adds it again and the entry name gets in.
    #[test]
    fn change_splits_a_name_with_a_line_break() {
        let mut entry = entry("Sousou no Frieren");
        let record = record(
            "Sousou no Frieren",
            Some("Frieren:\r\nBeyond Journey's End"),
            &["Frieren\nSousou no Frieren", "Sousou\rno Frieren"],
        );
        let outcome = change(&entry, &record);
        assert_eq!(
            outcome,
            Outcome::Changed {
                english_name: Some("Frieren:  Beyond Journey's End".to_owned()),
                other_names: vec!["Frieren".to_owned(), "Sousou".to_owned(), "no Frieren".to_owned()],
                added: 3
            }
        );
        let Outcome::Changed {
            english_name,
            other_names,
            ..
        } = outcome
        else {
            unreachable!()
        };
        entry.english_name = english_name;
        // The names as a read of the column gives them back.
        entry.other_names = parse(&join(&other_names).unwrap(), &[]);
        assert_eq!(change(&entry, &record), Outcome::Unchanged);
    }

    /// The edit form refuses more than 4096 bytes of other names and more than 1024 bytes of
    /// English name. The import keeps to the same limits, so that the form can still save the entry.
    #[test]
    fn change_keeps_the_limits_of_the_edit_form() {
        let mut entry = entry("a");
        entry.other_names = vec!["x".repeat(4000)];
        let long_english = "e".repeat(MAX_ENGLISH_NAME_LENGTH + 1);
        let some_fit = record("a", Some(&long_english), &[&"y".repeat(100), "short", "z"]);
        let Outcome::Changed {
            english_name,
            other_names,
            added,
        } = change(&entry, &some_fit)
        else {
            panic!("the short names must be added");
        };
        assert_eq!(english_name, None);
        assert_eq!(added, 2);
        assert_eq!(other_names[1..], ["short", "z"]);
        assert!(join(&other_names).unwrap().len() <= MAX_OTHER_NAMES_LENGTH);

        let full = record("a", Some(&long_english), &[&"y".repeat(100)]);
        assert_eq!(change(&entry, &full), Outcome::Unchanged);
    }

    #[test]
    fn records_ignore_other_fields() {
        let records: Vec<NameRecord> = serde_json::from_str(
            r#"[
                {"id": 1, "name": "a", "english_name": "A", "english_source": "anilist", "other_names": ["b"]},
                {"id": 2, "name": "c"},
                {"id": 3, "name": "d", "english_name": null}
            ]"#,
        )
        .unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].english_name.as_deref(), Some("A"));
        assert_eq!(records[0].other_names, ["b"]);
        assert!(records[1].english_name.is_none() && records[1].other_names.is_empty());
        assert!(records[2].english_name.is_none());
    }

    #[test]
    fn import_writes_the_changes_to_the_database() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        for migration in [
            include_str!("../sql/0.sql"),
            include_str!("../sql/1.sql"),
            include_str!("../sql/2.sql"),
            include_str!("../sql/3.sql"),
            include_str!("../sql/4.sql"),
        ] {
            conn.execute_batch(migration).unwrap();
        }
        conn.execute_batch(
            "INSERT INTO directory_entry(id, path, name, english_name) VALUES
                (1, 'a', 'Sousou no Frieren', NULL),
                (2, 'b', 'Kusuriya no Hitorigoto', 'The Apothecary Diaries'),
                (3, 'c', 'Dungeon Meshi', NULL);",
        )
        .unwrap();
        let records = [
            NameRecord {
                id: 1,
                ..record("Sousou no Frieren", Some("Frieren"), &["葬送的芙莉莲"])
            },
            NameRecord {
                id: 2,
                ..record("Kusuriya no Hitorigoto", Some("Other"), &["药屋少女的呢喃"])
            },
            NameRecord {
                id: 3,
                ..record("Dungeon Meshi 2", Some("Delicious in Dungeon"), &[])
            },
            NameRecord {
                id: 4,
                ..record("Missing", Some("Missing"), &[])
            },
        ];

        let tx = conn.transaction().unwrap();
        let summary = import(&tx, &records).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            summary,
            Summary {
                entries_changed: 2,
                english_names_added: 1,
                other_names_added: 2,
                no_such_entry: 1,
                name_changed: 1,
                unchanged: 0,
            }
        );

        let get = |id: i64| {
            conn.query_row(
                "SELECT * FROM directory_entry WHERE id = ?",
                [id],
                DirectoryEntry::from_row,
            )
            .unwrap()
        };
        let frieren = get(1);
        assert_eq!(frieren.english_name.as_deref(), Some("Frieren"));
        assert_eq!(frieren.other_names, ["葬送的芙莉莲"]);
        let apothecary = get(2);
        assert_eq!(apothecary.english_name.as_deref(), Some("The Apothecary Diaries"));
        assert_eq!(apothecary.other_names, ["药屋少女的呢喃"]);
        let dungeon = get(3);
        assert_eq!(dungeon.english_name, None);
        assert!(dungeon.other_names.is_empty());

        // A second run with the same records changes nothing.
        let summary = import(&conn, &records).unwrap();
        assert_eq!(
            summary,
            Summary {
                no_such_entry: 1,
                name_changed: 1,
                unchanged: 2,
                ..Default::default()
            }
        );
    }
}

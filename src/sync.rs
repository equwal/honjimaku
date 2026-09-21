//! Keeps the list of books the same as the folders on disk.
//!
//! On a site for books the operator also adds books by hand: a folder of subtitles is put
//! into the subtitle directory. Nothing told the database, so the site did not show the
//! book until someone wrote the row. Now the server looks at the directory when it starts
//! and once an hour, makes an entry for each folder that has none, and shows it at once.

use std::collections::HashSet;
use std::path::Path;

use crate::models::EntryFlags;
use crate::AppState;

/// The names of the folders in `root` that no entry has yet.
fn folders_without_entry(root: &Path, known: &HashSet<String>) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut missing = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let (Some(name), Some(full)) = (path.file_name().and_then(|n| n.to_str()), path.to_str()) else {
            continue; // a name that is not UTF-8 cannot be an entry
        };
        if name.starts_with('.') || known.contains(full) {
            continue;
        }
        missing.push((full.to_owned(), name.to_owned()));
    }
    missing.sort();
    missing
}

/// Makes an entry for each folder that has none. Returns how many were made.
pub async fn sync_books(state: &AppState) -> anyhow::Result<usize> {
    let root = state.config().subtitle_path.clone();
    let added = state
        .database()
        .call(move |con| -> rusqlite::Result<usize> {
            let known: HashSet<String> = {
                let mut stmt = con.prepare("SELECT path FROM directory_entry")?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            let missing = folders_without_entry(&root, &known);
            if missing.is_empty() {
                return Ok(0);
            }
            let mut flags = EntryFlags::new();
            flags.set_anime(true); // the listing on the front page
            let tx = con.transaction()?;
            {
                let mut insert = tx.prepare(
                    "INSERT INTO directory_entry(path, flags, notes, name, japanese_name) VALUES (?, ?, 'It is a book', ?, ?)",
                )?;
                for (path, name) in &missing {
                    insert.execute((path, flags, name, name))?;
                }
            }
            tx.commit()?;
            Ok(missing.len())
        })
        .await?;
    if added > 0 {
        tracing::info!("made entries for {added} new folders of subtitles");
        state.cached_directories().invalidate().await;
    }
    Ok(added)
}

/// Looks when the server starts, then once an hour.
pub async fn sync_loop(state: AppState) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600));
    loop {
        interval.tick().await;
        if let Err(e) = sync_books(&state).await {
            tracing::warn!(error = %e, "could not look for new folders of subtitles");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_new_folders_are_found() {
        let root = std::env::temp_dir().join(format!("honjimaku-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for name in ["吾輩は猫である [B0TEST]", "known book", ".hidden"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        std::fs::write(root.join("a file.srt"), "x").unwrap();

        let known: HashSet<String> = [root.join("known book").to_str().unwrap().to_owned()].into();
        let missing = folders_without_entry(&root, &known);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].1, "吾輩は猫である [B0TEST]");
        assert_eq!(missing[0].0, root.join("吾輩は猫である [B0TEST]").to_str().unwrap());

        std::fs::remove_dir_all(&root).unwrap();
        assert!(folders_without_entry(&root, &known).is_empty(), "a directory that is not there is no error");
    }
}

-- An entry now holds subtitles in one language. The language is an ISO 639-1 code
-- ("ja", "zh", "en"). The entries that exist are Japanese.
--
-- One show can have an entry in more than one language. So an AniList ID and a TMDB ID
-- are unique for one language, not for the site. SQLite cannot drop the UNIQUE
-- constraint of a column, so the table is made again, as
-- https://sqlite.org/lang_altertable.html#otheralter says. `init_db` keeps the foreign
-- keys off while this runs: with them on, DROP TABLE would delete the bookmarks and the
-- notifications of each entry.
CREATE TABLE directory_entry_new (
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL UNIQUE,
  last_updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  creator_id INTEGER REFERENCES account(id) ON DELETE SET NULL,
  flags INTEGER NOT NULL DEFAULT 1,
  anilist_id INTEGER,
  tmdb_id TEXT,
  notes TEXT,
  english_name TEXT,
  japanese_name TEXT,
  name TEXT NOT NULL,
  language TEXT NOT NULL DEFAULT 'ja'
);

INSERT INTO directory_entry_new (id, path, last_updated_at, creator_id, flags, anilist_id, tmdb_id, notes,
                                 english_name, japanese_name, name)
  SELECT id, path, last_updated_at, creator_id, flags, anilist_id, tmdb_id, notes,
         english_name, japanese_name, name
  FROM directory_entry;

DROP TABLE directory_entry;
ALTER TABLE directory_entry_new RENAME TO directory_entry;

CREATE INDEX IF NOT EXISTS directory_entry_path_idx ON directory_entry(path);
CREATE INDEX IF NOT EXISTS directory_entry_flags_idx ON directory_entry(flags);
CREATE INDEX IF NOT EXISTS directory_entry_creator_id_idx ON directory_entry(creator_id);
CREATE UNIQUE INDEX IF NOT EXISTS directory_entry_anilist_id_idx ON directory_entry(anilist_id, language);
CREATE UNIQUE INDEX IF NOT EXISTS directory_entry_tmdb_id_idx ON directory_entry(tmdb_id, language);

PRAGMA user_version = 5;

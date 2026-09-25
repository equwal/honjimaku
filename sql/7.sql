-- A live copy of another jimaku site (jimaku.cc, dung.live) brings shows in the language
-- of that site next to the books. Two things change for that.
--
-- 1. The kind of an entry: 'book', 'anime', or 'drama' (a live action show). NULL is the
--    kind that the site is for: a book on a site for books, else a show, animated or not
--    as the anime flag says.
-- 2. One show can be here in more than one language. So an AniList ID, a TMDB ID and a
--    Bangumi ID are unique for one language, not for the site. SQLite cannot drop the
--    UNIQUE constraint of a column, so the table is made again, as
--    https://sqlite.org/lang_altertable.html#otheralter says. `database::init` keeps the
--    foreign keys off while this runs: with them on, DROP TABLE would delete the rows of
--    the other tables that point at the old table.
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
  book_id TEXT,
  bangumi_id INTEGER,
  language TEXT,
  kind TEXT
);

INSERT INTO directory_entry_new (id, path, last_updated_at, creator_id, flags, anilist_id, tmdb_id, notes,
                                 english_name, japanese_name, name, book_id, bangumi_id, language)
  SELECT id, path, last_updated_at, creator_id, flags, anilist_id, tmdb_id, notes,
         english_name, japanese_name, name, book_id, bangumi_id, language
  FROM directory_entry;

DROP TABLE directory_entry;
ALTER TABLE directory_entry_new RENAME TO directory_entry;

CREATE INDEX IF NOT EXISTS directory_entry_path_idx ON directory_entry(path);
CREATE INDEX IF NOT EXISTS directory_entry_flags_idx ON directory_entry(flags);
CREATE INDEX IF NOT EXISTS directory_entry_creator_id_idx ON directory_entry(creator_id);
-- NULL counts as distinct in a unique index, so a language that is not set is compared as ''.
CREATE UNIQUE INDEX IF NOT EXISTS directory_entry_anilist_id_idx ON directory_entry(anilist_id, COALESCE(language, ''));
CREATE UNIQUE INDEX IF NOT EXISTS directory_entry_tmdb_id_idx ON directory_entry(tmdb_id, COALESCE(language, ''));
CREATE UNIQUE INDEX IF NOT EXISTS directory_entry_bangumi_id_idx ON directory_entry(bangumi_id, COALESCE(language, ''));
-- An audiobook is one recording in one language, so its identifier stays unique for the site.
CREATE UNIQUE INDEX IF NOT EXISTS directory_entry_book_id_idx ON directory_entry(book_id);

-- Which entry here is the copy of which entry of another site. `last_modified` is the date
-- of the newest file of the entry there when its files were last copied in full, as
-- nanoseconds since 1970. NULL means that the files are not copied yet.
CREATE TABLE IF NOT EXISTS mirror (
  site TEXT NOT NULL,
  remote_id INTEGER NOT NULL,
  entry_id INTEGER NOT NULL REFERENCES directory_entry(id) ON DELETE CASCADE,
  last_modified INTEGER,
  PRIMARY KEY (site, remote_id)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS mirror_entry_id_idx ON mirror(entry_id);

PRAGMA user_version = 8;

-- A site for Chinese shows verifies each entry against Bangumi (bgm.tv). The
-- subject number is the identifier of the show, as the AniList ID is for an anime.
ALTER TABLE directory_entry ADD COLUMN bangumi_id INTEGER;
CREATE UNIQUE INDEX IF NOT EXISTS directory_entry_bangumi_id_idx ON directory_entry(bangumi_id);

PRAGMA user_version = 6;

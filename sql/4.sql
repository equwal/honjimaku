-- Other names of an entry, one name on each line. For example, other romaji spellings or
-- the titles in other languages. The search also matches these names. NULL means none.
ALTER TABLE directory_entry ADD COLUMN other_names TEXT;

PRAGMA user_version = 5;

-- Other names of an entry, one on each line: the title in romaji or in pinyin, the title
-- in Chinese or in Japanese, another English title. The search finds the entry by each
-- of them. The copy of another site (see `mirror`) does not write this column, so a name
-- that is added here stays.
ALTER TABLE directory_entry ADD COLUMN other_names TEXT;

-- An empty name is no name. Some entries have '' where they have no English name. Then a
-- user who prefers English names sees an empty name in the list, not the name.
UPDATE directory_entry SET english_name = NULL WHERE trim(english_name) = '';
UPDATE directory_entry SET japanese_name = NULL WHERE trim(japanese_name) = '';

PRAGMA user_version = 9;

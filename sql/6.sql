-- A site for books holds books in more than one language. The language of an
-- entry is an ISO 639-1 code ("ja", "en"). NULL is the language of the site.
ALTER TABLE directory_entry ADD COLUMN language TEXT;

PRAGMA user_version = 7;

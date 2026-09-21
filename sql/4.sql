-- A book has no AniList or TMDB ID. Its identifier is the ASIN of the audiobook,
-- which the site verifies against the Audible catalog. Up to now the ASIN lived
-- only in the name of the folder, "title [B0BPXSSWVF]", which the path ends with.
ALTER TABLE directory_entry ADD COLUMN book_id TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS directory_entry_book_id_idx ON directory_entry(book_id);

-- Two folders with one ASIN (a test site) would break the index, so only an ASIN
-- that one folder has is copied.
UPDATE directory_entry
SET book_id = substr(path, -11, 10)
WHERE book_id IS NULL
  AND path LIKE '%[B0________]'
  AND substr(path, -11, 10) IN (
    SELECT substr(path, -11, 10) FROM directory_entry
    WHERE path LIKE '%[B0________]'
    GROUP BY substr(path, -11, 10) HAVING count(*) = 1
  );

PRAGMA user_version = 5;

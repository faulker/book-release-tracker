-- Author name from ABS metadata, for the series page and search/filter.
-- Backfilled naturally on the next check run's ABS sync.
ALTER TABLE owned_books ADD COLUMN author TEXT;

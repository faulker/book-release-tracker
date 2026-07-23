-- Hardcover series slug, captured at match time so the dashboard can link
-- straight to https://hardcover.app/series/{slug}. NULL until (re-)matched.
ALTER TABLE series ADD COLUMN hardcover_slug TEXT;

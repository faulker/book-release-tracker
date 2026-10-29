-- Series that vanished from ABS on the last sync. Archived series keep their
-- match but lose their owned/known books; a later sync that sees the series
-- again clears this and repopulates the books. NULL = active.
ALTER TABLE series ADD COLUMN archived_at TEXT;

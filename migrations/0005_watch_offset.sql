-- Reminders can now fire after a release too (e.g. "1 week after" for
-- audiobooks that lag the print date). remind_days_before becomes a signed
-- remind_offset_days: negative = days before release, positive = days after,
-- 0 = on release day. Existing "N days before" rows become -N.
ALTER TABLE watches RENAME COLUMN remind_days_before TO remind_offset_days;
UPDATE watches SET remind_offset_days = -remind_offset_days;

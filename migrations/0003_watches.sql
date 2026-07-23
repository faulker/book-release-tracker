-- Watch/reminder list: releases the user asked to be reminded about.
-- One row per watched release; the reminder fires once when
-- release_date - remind_days_before arrives, then is stamped.
CREATE TABLE watches (
  id INTEGER PRIMARY KEY,
  known_release_id INTEGER NOT NULL UNIQUE REFERENCES known_releases(id) ON DELETE CASCADE,
  remind_days_before INTEGER NOT NULL DEFAULT 1,
  reminder_notified_at TEXT,         -- set once when the reminder is delivered
  created_at TEXT NOT NULL
);

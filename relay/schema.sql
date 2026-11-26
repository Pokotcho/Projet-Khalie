CREATE TABLE IF NOT EXISTS events (
  seq         INTEGER PRIMARY KEY AUTOINCREMENT,
  event_id    TEXT    NOT NULL UNIQUE,
  received_at INTEGER NOT NULL,
  event_type  TEXT    NOT NULL,
  body        TEXT    NOT NULL
);

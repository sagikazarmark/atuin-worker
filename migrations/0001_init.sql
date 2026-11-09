-- The whole schema of a single-user atuin-worker: encrypted records and the head of each series.
-- There is no user_id anywhere and no server-generated ID: the server holds one person's store.

CREATE TABLE store (
  host      TEXT    NOT NULL,
  tag       TEXT    NOT NULL,
  idx       INTEGER NOT NULL,
  client_id TEXT    NOT NULL,   -- Record.id
  timestamp INTEGER NOT NULL,   -- nanoseconds; bound and read as text
  version   TEXT    NOT NULL,
  data      TEXT    NOT NULL,   -- encrypted payload
  cek       TEXT    NOT NULL    -- wrapped content key
);
CREATE UNIQUE INDEX store_series ON store (host, tag, idx);

CREATE TABLE store_head (
  host TEXT    NOT NULL,
  tag  TEXT    NOT NULL,
  idx  INTEGER NOT NULL,
  PRIMARY KEY (host, tag)
) WITHOUT ROWID;

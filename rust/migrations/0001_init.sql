CREATE TABLE items (
  id            TEXT PRIMARY KEY,
  repo          TEXT NOT NULL,
  kind          TEXT NOT NULL,
  title         TEXT NOT NULL,
  body          TEXT NOT NULL,
  refs          TEXT NOT NULL DEFAULT '',
  tags          TEXT NOT NULL DEFAULT '',
  source        TEXT NOT NULL DEFAULT '',
  tag           TEXT NOT NULL,
  evidence      TEXT NOT NULL DEFAULT '',
  as_of         TEXT NOT NULL,
  status        TEXT NOT NULL DEFAULT 'active',
  superseded_by TEXT,
  content_hash  TEXT NOT NULL
);

CREATE UNIQUE INDEX items_dedupe ON items(repo, content_hash) WHERE status = 'active';

CREATE VIRTUAL TABLE items_fold USING fts5(
  refs,
  title,
  tags,
  body,
  tokenize='unicode61'
);

CREATE VIRTUAL TABLE items_raw USING fts5(
  title,
  body,
  tokenize='unicode61 remove_diacritics 0'
);

CREATE TRIGGER items_ai AFTER INSERT ON items BEGIN
  INSERT INTO items_fold(rowid, refs, title, tags, body)
  VALUES (new.rowid, sm_fold(new.refs), sm_fold(new.title), sm_fold(new.tags), sm_fold(new.body));
  INSERT INTO items_raw(rowid, title, body)
  VALUES (new.rowid, new.title, new.body);
END;

CREATE TRIGGER items_ad AFTER DELETE ON items BEGIN
  DELETE FROM items_fold WHERE rowid = old.rowid;
  DELETE FROM items_raw WHERE rowid = old.rowid;
END;

CREATE TRIGGER items_au AFTER UPDATE ON items BEGIN
  DELETE FROM items_fold WHERE rowid = old.rowid;
  DELETE FROM items_raw WHERE rowid = old.rowid;
  INSERT INTO items_fold(rowid, refs, title, tags, body)
  VALUES (new.rowid, sm_fold(new.refs), sm_fold(new.title), sm_fold(new.tags), sm_fold(new.body));
  INSERT INTO items_raw(rowid, title, body)
  VALUES (new.rowid, new.title, new.body);
END;

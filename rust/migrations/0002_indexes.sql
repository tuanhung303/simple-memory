CREATE INDEX IF NOT EXISTS items_status_repo ON items(status, repo);
CREATE INDEX IF NOT EXISTS items_source ON items(source);
CREATE INDEX IF NOT EXISTS items_superseded_by ON items(superseded_by) WHERE superseded_by IS NOT NULL;

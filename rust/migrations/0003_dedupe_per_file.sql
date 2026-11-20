DROP INDEX IF EXISTS items_dedupe;
CREATE UNIQUE INDEX items_dedupe ON items(
  repo,
  content_hash,
  CASE WHEN substr(source, 1, 5) = 'task:' OR substr(source, 1, 8) = 'handoff:'
    OR substr(source, 1, 13) = 'amended:task:' OR substr(source, 1, 16) = 'amended:handoff:'
    THEN source ELSE '' END
) WHERE status = 'active';

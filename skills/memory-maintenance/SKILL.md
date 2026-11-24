---
name: memory-maintenance
description: >-
  Audit, deduplicate, prune, re-scope, and compact a simple-memory (sm) SQLite store safely using sm CLI commands and read-only diagnostics.
  Use when maintaining, optimizing, cleaning up, or troubleshooting memory.db bloat, duplicate notes, stale plans, or noisy prompt-hook recall.
---

# Memory Maintenance

Keep `memory.db` compact, accurately scoped, and high-signal without breaking file-backed sync or losing provenance.

## Contract

1. **Snapshot before writes.** Run `sm backup`, copy the newest snapshot to a dedicated pre-maintenance file outside auto-rotation, and verify integrity before changing anything.
2. **Diagnose read-only first.** Run `scripts/audit.py`, `sm doctor --imports`, `sm upkeep --report`, and `sm stats` before proposing or running fixes.
3. **Mutate items through `sm` CLI.** Change items exclusively via `sm amend`, `sm supersede`, `sm retire`, `sm doctor --backfill-refs`, and `sm upkeep` so `sm` keeps FTS rows, supersede chains, and dedupe slots consistent; reserve direct SQLite writes for FTS5 `optimize`, WAL checkpointing, and `VACUUM`.
4. **Sync file-backed items from disk.** Items whose `source` starts with `automemory:` mirror local `.md` files under `[automemory] root`: back up and edit or archive the `.md` file first, then run `sm import automemory`. Note that `sm amend` on `task:`/`handoff:`/`automemory:` rewrites or blanks `source`.
5. **Separate safe hygiene from policy choices.**
   - **Tier 1 (Safe Hygiene — execute after inspecting targets with `sm show`):** defects the store itself proves (dry-run `backfill-refs` `changed > 0`, bare-tag stub titles, exact duplicates, delimiter-marked self-retired stubs, typo/deleted-sandbox repos, stale plans/guesses where disk or newer store items settle them) that remain recoverable from the Phase 1 snapshot. Supply `--evidence` when upgrading an `n/a` item to `executed` or `verified`.
   - **Tier 2 (Policy Pruning — present exact counts and impact for approval first):** retention, deletion, or ownership choices (ephemeral TTL, bulk `same-session` handoff consolidation, bulk `unassigned` reassignment, retiring never-opened noise).

## Workflow

### Phase 1: Snapshot

Run `sm backup` (which runs `PRAGMA quick_check` before persisting) and copy the resulting file outside `sm backup` rotation:

```sh
HOME_DIR="${SIMPLE_MEMORY_HOME:-$HOME/.simple-memory}"
STAMP="$(date +%Y%m%d-%H%M%S)"
sm backup
LATEST_BKP="$(python3 -c 'import pathlib, sys; files = sorted(pathlib.Path(sys.argv[1], "backups").glob("memory-*-auto.db"), key=lambda p: p.stat().st_mtime, reverse=True); sys.exit(1) if not files else print(files[0])' "$HOME_DIR")"
cp -p "$LATEST_BKP" "$HOME_DIR/backups/memory-$STAMP-pre-opt.db"
sm stats --format json > "/tmp/sm-stats-$STAMP-baseline.json"
```

Done when: `memory-$STAMP-pre-opt.db` exists and baseline stats are recorded.

### Phase 2: Diagnose

Run the read-only audit (from this skill's directory, or using the path to `scripts/audit.py` beside this `SKILL.md`) and built-in `sm` diagnostics:

```sh
python3 -B scripts/audit.py --format markdown
sm doctor --imports
sm upkeep --report --limit 50
sm doctor --backfill-refs --dry-run
sm stats
```

Done when: every candidate finding has an item ID, repo, `kind`/`tag`, `source` prefix, and exact counts, and `fts_aligned` is checked.

### Phase 3: Classify and Execute Safe Repairs

Read [optimization-methods.md](references/optimization-methods.md) for classification details and `sm` CLI caveats.

1. Back up any `automemory:` `.md` files before editing or archiving them on disk.
2. Inspect every Tier-1 target with `sm show ID [ID ...]` (and `sm related ID` for supersede chains).
3. Apply safe `sm` mutations (`sm doctor --backfill-refs`, title/tag repairs, exact duplicate merges, obsolete retirements, repo typo fixes, `sm import automemory`).
4. Run SQLite compaction if `freelist_bytes`, WAL size, or large writes warrant it, then verify FTS rowid alignment:

```sh
HOME_DIR="${SIMPLE_MEMORY_HOME:-$HOME/.simple-memory}"
python3 - <<'PY' "$HOME_DIR"
import pathlib, sqlite3, sys
uri = pathlib.Path(sys.argv[1], "memory.db").resolve().as_uri() + "?mode=rw"
con = sqlite3.connect(uri, uri=True, timeout=15.0)
con.execute("INSERT INTO items_fold(items_fold) VALUES('optimize')")
con.execute("INSERT INTO items_raw(items_raw) VALUES('optimize')")
con.commit()
con.execute("PRAGMA wal_checkpoint(TRUNCATE)")
con.execute("VACUUM")
con.execute("PRAGMA wal_checkpoint(TRUNCATE)")
ic = con.execute("PRAGMA integrity_check").fetchone()[0]
total, matched = con.execute("""
    SELECT (SELECT count(*) FROM items),
           (SELECT count(*) FROM items i JOIN items_raw r ON r.rowid = i.rowid
            WHERE r.title = i.title AND r.body = i.body)
""").fetchone()
con.close()
print(f"integrity={ic} items={total} fts_raw_matched={matched}")
if total != matched:
    raise SystemExit("FTS rowid drift detected; run: sm doctor --rebuild-fts")
PY
```

Done when: all Tier-1 commands exit `0`, `integrity=ok`, and `items == fts_raw_matched` (or `sm doctor --rebuild-fts` passes).

### Phase 4: Verify & Report

Re-run the Phase 2 commands (`scripts/audit.py`, `sm doctor --imports`, `sm upkeep --report`, `sm stats`). Confirm `sm doctor --imports` shows `not_stored=0` and `automemory: files=... not_stored=0`.

Report:
- Before/after metrics (`db_bytes`, `freelist_bytes`, active/superseded/retired counts, `bare_tag_titles`, `stale_count`, `exact_title_dups_groups`, `unassigned_active`, `fts_aligned`).
- Applied Tier-1 changes, named backup path, and rollback instructions.
- Tier-2 recommendations ranked by impact with post-Tier-1 counts and exact commands.

## Resources

- **Execute** `python3 -B scripts/audit.py` for read-only diagnostics (`--home`, `--format json|markdown`).
- **Read** [optimization-methods.md](references/optimization-methods.md) before classifying edge cases, tuning recall noise, or proposing Tier-2 pruning.

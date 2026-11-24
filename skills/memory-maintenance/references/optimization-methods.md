# Memory Optimization Methods (`simple-memory`)

Use this reference with `scripts/audit.py`, `sm doctor --imports`, `sm upkeep --report`, and `sm stats` to separate Tier-1 safe hygiene from Tier-2 policy pruning.

## Method Summary

| # | Method | Tier | Primary Commands | Primary Benefit |
|---|---|---|---|---|
| 1 | Reference backfill | Tier 1 | `sm doctor --backfill-refs` | Improves exact `ref_hits` recall |
| 2 | Stub title & metadata repair | Tier 1 | `sm show`, `sm amend --title --tag` | Fixes broken snippets and tag weights |
| 3 | Exact & near-duplicate merge | Tier 1 (exact) / Tier 2 (fuzzy) | `sm upkeep --report`, `sm supersede` | Removes competing search hits |
| 4 | Same-session `handoff` -> `task` | Tier 2 (Tier 1 if `task` covers all) | `sm show`, `sm supersede` | Stops ephemeral handoffs shadowing tasks |
| 5 | Stale `planned`/`assumed` triage | Tier 1 (settled) / Tier 2 (open) | `sm show`, `sm amend`, `sm retire` | Prevents outdated guesses from ranking |
| 6 | Self-declared obsolete pruning | Tier 1 | `sm retire --reason` (or disk archive) | Clears `RETIRED:`/`SUPERSEDED:` noise |
| 7 | Repo & scope hygiene | Tier 1 (typos) / Tier 2 (bulk `unassigned`) | `sm amend --repo`, `[scopes]` edit | Fixes cross-repo recall misses |
| 8 | Recall noise feedback loop | Tier 2 | `sm stats`, `sm show`, `sm retire`/`sm amend` | Raises prompt-hook precision |
| 9 | Ephemeral TTL & SQLite compaction | Tier 2 (TTL) / Tier 1 (compaction) | `sm retire`, `FTS5 optimize`, `VACUUM` | Reclaims freelist & trims cold noise |

---

## 1. Reference Backfill

- **When:** `sm doctor --backfill-refs --dry-run` reports `changed > 0` (note: many items legitimately contain no reference tokens, so `empty_refs_active > 0` alone is normal).
- **Action:** Run `sm doctor --backfill-refs`.
- **Why:** `sm` ranks exact reference matches ahead of BM25 (`(active, ref_hits, phrase_hit, ...)`). Populating ticket IDs, SHAs, and paths moves direct lookups to the top.
- **Rollback:** Atomic per transaction; reversible only from the Phase 1 snapshot.

## 2. Bare-Tag Title & Metadata Repair

- **When:** `bare_tag_titles_count > 0` (title collapsed to a bare evidence tag such as `decided`, `executed`, `ok`) or legitimate short titles need inspection (`short_titles_count`).
- **Action:** Inspect full body with `sm show <id>`. Repair real notes with `sm amend <id> --title "..." [--tag ... --evidence "..."]` (supply `--evidence` when upgrading from `n/a` to `executed` or `verified`). Retire zero-value stubs (`ok`) with `sm retire <id> --reason "no durable signal"`.
- **File-backed check:** When `source` begins with `automemory:`, repair `description:` or `metadata.tag:` in the `.md` file on disk and run `sm import automemory`.

## 3. Exact & Near-Duplicate Consolidation

- **When:** `exact_body_dups_groups > 0`, `exact_title_dups_groups > 0`, or `sm upkeep --report` prints `title_groups` or high-cosine `pairs`.
- **Action:**
  1. Inspect every member with `sm show <id1> <id2> ...` and check links with `sm related <id>`.
  2. Keep the richer item in the concrete repo (`task` over `handoff`, concrete repo over `unassigned`) and run `sm supersede <old_id> <keep_id>`.
- **Caveat:** If two items share a title prefix but carry distinct facts or open tasks, amend the second item's title rather than superseding it.

## 4. Same-Session `handoff` -> `task` Consolidation

- **When:** `same_session_handoff_task_count > 0` (`source_session_key` matches a `handoff:YYYYMMDD-HHMMSS-<8hex>-<uuid>.md` and a `task:YYYYMMDD-HHMMSS-<8hex>-...` item in the **same repo** that are both `active`).
- **Action:** Sample 5–10 pairs via `sm show` to verify whether the `task` record captures the handoff's open items. Where confirmed (or on owner approval for bulk sweeps), run `sm supersede <handoff_id> <task_id>`, then verify `sm doctor --imports` keeps `not_stored=0` (superseded items with `superseded_by` set remain counted as stored for that file).

## 5. Stale `planned` / `assumed` Triage (`> 30` days)

- **When:** `stale_count > 0` in `audit.py` or `sm stats`.
- **Action:** Inspect each item with `sm show` and search recent work with `sm query`:
  - **Confirmed durable rule stored as `assumed`** (often because an `.md` file omitted `metadata.tag:` or wrote `tag: decision` instead of `decided`): fix `tag:` to `decided` or `verified` (edit `.md` on disk + `sm import automemory` if file-backed; otherwise `sm amend`).
  - **Completed or abandoned plan:** supersede with the completed `task`/`decision` or `sm retire <id> --reason "plan completed/abandoned"`.
  - **Still pending:** leave untouched unless re-verified.

## 6. Self-Declared Obsolete Pruning

- **When:** `self_declared_retired_count > 0` (titles starting with `RETIRED:`, `SUPERSEDED:`, `DEPRECATED:`, `OBSOLETE:`, or `History only:`).
- **Why:** Prefixing an active title with `SUPERSEDED` leaves `status = 'active'`, so FTS5 still matches it in default recall.
- **Action:**
  - Non-file-backed: confirm with `sm show` and run `sm retire <id> --reason "self-declared obsolete"` (or `sm supersede <id> <replacement_id>`).
  - File-backed (`automemory:`): back up the `.md` file, remove or move it out of `[automemory] root`, update any index link, and run `sm import automemory` so catch-up retires the database item.

## 7. Repo & Scope Hygiene

- **When:** `orphan_repos_le_5` holds a clear typo (e.g., `acme-internl` for `acme-internal`) or a deleted sandbox repo, `unassigned_matching_repo` is non-zero, or new active repos are missing from `[scopes]` in `~/.simple-memory/config.toml`.
- **Action:**
  - **Tier 1:** Fix individual typo/sandbox items (non-file-backed) with `sm amend <id> --repo <target>`, or retire obsolete dead-repo notes.
  - **Tier 2 (owner decision):** Bulk reassignment of `unassigned` items. **Warning:** `sm amend` adds a new row, rewrites `task:`/`handoff:` sources to `amended:...`, and blanks `automemory:` sources (detaching them from disk `.md` sync). Note also that `[scopes]` code matchers (unlike `repo:` matchers) already match `unassigned` items by text (`unassigned_mentioning_scope_term`), whereas `repo:` matchers match `repo = ?` only.

## 8. Recall Noise Feedback Loop

- **When:** `never_opened` in `audit.py` or `sm stats` lists active items injected repeatedly without `sm show` opens, or hook open rates are low.
- **Caveat:** "Opened" tracks subsequent `sm show` calls, so a self-contained snippet that fully answers in-place never counts as opened. Treat `never_opened` as a review queue, not an automatic deletion trigger.
- **Action (Tier 2):** Inspect top items with `sm show`. Fix genuine noise by tightening the repo scope (`sm amend --repo`), sharpening the title, retiring stale rules (`sm retire`), or tuning `[prompt_hook] extra_stopwords` / `min_idf` in `config.toml`.

## 9. Ephemeral TTL, Logs & SQLite Compaction

- **Ephemeral TTL (Tier 2):** `sm retire` is a soft state transition (`status = 'superseded'`, `superseded_by = NULL`) — it never deletes rows from SQLite, and `sm` has no un-retire command. Confirm with the owner before retiring unclaimed `handoff` or `episode` items by age; note that default query bias already demotes `episode` and `handoff` items relative to durable `fact`/`decision`/`learning` items.
- **Logs:** `usage.log` self-trims above `5 MB` (keeping the newest `5,000` rows) and `catchup.log` self-trims above `1 MB` (keeping the newest `500` rows); `hook.log` grows append-only, and Method 8 joins `hook.log` with `usage.log` (`never_opened` tracking).
- **SQLite compaction (safe in a lock-free window):** Run `INSERT INTO items_fold(items_fold) VALUES('optimize')` and `INSERT INTO items_raw(items_raw) VALUES('optimize')`, followed by `PRAGMA wal_checkpoint(TRUNCATE)`, `VACUUM`, a trailing `PRAGMA wal_checkpoint(TRUNCATE)`, and the `items` vs `items_raw` rowid/content check. Note that `items_fold` and `items_raw` store indexed text for all `items` rows (`active` + `superseded` + `retired`), so `VACUUM` reclaims freelist pages and index fragmentation rather than shrinking active-corpus FTS overhead.

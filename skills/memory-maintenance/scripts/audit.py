#!/usr/bin/env python3
"""Audit a simple-memory (sm) home directory without writing memory.db."""

from __future__ import annotations

import argparse
import collections
from datetime import datetime, timedelta, timezone
import json
import os
import pathlib
import re
import sqlite3
import sys

try:
    import tomllib
except ImportError:  # pragma: no cover - Python 3.11+ includes tomllib
    tomllib = None  # type: ignore[assignment]

STALE_TAGS = ("planned", "assumed")
STUB_EXACT_TITLES = frozenset(
    {
        "ok",
        "executed",
        "verified",
        "decided",
        "client-stated",
        "derived",
        "assumed",
        "prior",
        "planned",
    }
)
SELF_RETIRED_RE = re.compile(
    r"^(?:history only\b|"
    r"(?:retired|superseded|deprecated|obsolete)\b"
    r"(?:\s*[:\-(\[\d,]|\s+(?:by|on|in|for|rule|routing|design)\b))",
    re.IGNORECASE,
)
SESSION_SOURCE_RE = re.compile(r"^(task|handoff):(\d{8}-\d{6})-([0-9a-f]{8})\b")


def resolve_home(cli_home: str | None) -> pathlib.Path:
    if cli_home:
        return pathlib.Path(cli_home).expanduser()
    env_home = os.environ.get("SIMPLE_MEMORY_HOME")
    if env_home:
        return pathlib.Path(env_home).expanduser()
    return pathlib.Path.home() / ".simple-memory"


def parse_iso_dt(value: object) -> datetime | None:
    if not isinstance(value, str):
        return None
    text = value.strip()
    if not text:
        return None
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    try:
        dt = datetime.fromisoformat(text)
    except ValueError:
        return None
    if dt.tzinfo is None:
        return dt.replace(tzinfo=timezone.utc)
    return dt


def parse_session_key(source: object) -> tuple[str, str] | None:
    if not isinstance(source, str) or not source:
        return None
    m = SESSION_SOURCE_RE.match(source)
    if not m:
        return None
    return m.group(1), m.group(3)


def load_config_scopes(home: pathlib.Path) -> tuple[dict[str, list[str]], dict[str, str], bool]:
    cfg_path = home / "config.toml"
    if tomllib is None or not cfg_path.is_file():
        return {}, {}, False
    try:
        data = tomllib.loads(cfg_path.read_text(encoding="utf-8", errors="replace"))
    except (OSError, tomllib.TOMLDecodeError):
        return {}, {}, False
    raw_scopes = data.get("scopes")
    scopes: dict[str, list[str]] = {}
    if isinstance(raw_scopes, dict):
        for k, v in raw_scopes.items():
            if isinstance(v, list):
                scopes[str(k)] = [str(x) for x in v if isinstance(x, str)]
    raw_aliases = data.get("repo_aliases")
    aliases: dict[str, str] = {}
    if isinstance(raw_aliases, dict):
        for k, v in raw_aliases.items():
            if isinstance(v, str):
                aliases[str(k)] = v
    return scopes, aliases, True


def file_stats(path: pathlib.Path) -> dict[str, int]:
    if not path.is_file():
        return {"exists": 0, "bytes": 0, "lines": 0}
    try:
        size = path.stat().st_size
        with path.open("rb") as fh:
            lines = sum(1 for _ in fh)
        return {"exists": 1, "bytes": size, "lines": lines}
    except OSError:
        return {"exists": 0, "bytes": 0, "lines": 0}


def parse_jsonl_rows(path: pathlib.Path) -> tuple[list[dict], int]:
    if not path.is_file():
        return [], 0
    rows: list[dict] = []
    bad = 0
    try:
        with path.open("r", encoding="utf-8", errors="replace") as fh:
            for line in fh:
                s = line.strip()
                if not s:
                    continue
                try:
                    obj = json.loads(s)
                except json.JSONDecodeError:
                    bad += 1
                    continue
                if isinstance(obj, dict):
                    rows.append(obj)
                else:
                    bad += 1
    except OSError:
        return [], 0
    return rows, bad


def normalize_ws(text: str | None) -> str:
    return re.sub(r"\s+", " ", (text or "").strip().lower())


def source_prefix(source: str | None) -> str:
    if not source:
        return "(empty)"
    if ":" in source:
        return source.split(":", 1)[0] + ":"
    return source[:20]


def is_file_backed(source: str | None) -> bool:
    return bool(source and source.startswith("automemory:"))


def _open_readonly(db_path: pathlib.Path) -> tuple[sqlite3.Connection, int]:
    """Open memory.db without creating WAL/SHM sidecars when no WAL frames exist."""
    db_uri = db_path.resolve().as_uri()
    wal_path = pathlib.Path(str(db_path) + "-wal")
    wal_bytes = wal_path.stat().st_size if wal_path.is_file() else 0
    if wal_bytes == 0:
        con = sqlite3.connect(db_uri + "?mode=ro&immutable=1", uri=True)
    else:
        try:
            con = sqlite3.connect(db_uri + "?mode=ro", uri=True)
        except sqlite3.OperationalError:
            con = sqlite3.connect(db_uri + "?mode=ro&immutable=1", uri=True)
    con.row_factory = sqlite3.Row
    con.execute("PRAGMA query_only = ON")
    return con, wal_bytes


def audit_home(
    home: pathlib.Path,
    *,
    now: datetime | None = None,
    stale_days: int = 30,
    handoff_days: int = 14,
    episode_days: int = 30,
    oversized_bytes: int = 4096,
    noise_min_injections: int = 5,
    limit: int = 20,
) -> dict:
    db_path = home / "memory.db"
    if not db_path.is_file():
        raise FileNotFoundError(f"memory.db not found at {db_path}")

    ref_now = now or datetime.now().astimezone()
    if ref_now.tzinfo is None:
        ref_now = ref_now.replace(tzinfo=timezone.utc)

    con, wal_bytes = _open_readonly(db_path)
    try:
        return _run_audit(
            con,
            home,
            db_path,
            wal_bytes=wal_bytes,
            ref_now=ref_now,
            stale_days=stale_days,
            handoff_days=handoff_days,
            episode_days=episode_days,
            oversized_bytes=oversized_bytes,
            noise_min_injections=noise_min_injections,
            limit=limit,
        )
    finally:
        con.close()


def _run_audit(
    con: sqlite3.Connection,
    home: pathlib.Path,
    db_path: pathlib.Path,
    *,
    wal_bytes: int,
    ref_now: datetime,
    stale_days: int,
    handoff_days: int,
    episode_days: int,
    oversized_bytes: int,
    noise_min_injections: int,
    limit: int,
) -> dict:
    db_bytes = db_path.stat().st_size
    shm_path = pathlib.Path(str(db_path) + "-shm")
    shm_bytes = shm_path.stat().st_size if shm_path.is_file() else 0

    page_count = int(con.execute("PRAGMA page_count").fetchone()[0])
    page_size = int(con.execute("PRAGMA page_size").fetchone()[0])
    freelist_count = int(con.execute("PRAGMA freelist_count").fetchone()[0])
    journal_mode = str(con.execute("PRAGMA journal_mode").fetchone()[0])
    # SQLite ?mode=ro&immutable=1 reports 'delete' even for WAL DBs; check format bytes 18-19.
    if journal_mode == "delete":
        with db_path.open("rb") as fh:
            hdr = fh.read(20)
        if len(hdr) >= 20 and (hdr[18] == 2 or hdr[19] == 2):
            journal_mode = "wal"
    freelist_bytes = freelist_count * page_size

    status_counts: dict[str, dict[str, int]] = {}
    for r in con.execute(
        """
        SELECT
          CASE
            WHEN status = 'active' THEN 'active'
            WHEN superseded_by IS NOT NULL AND superseded_by != '' THEN 'superseded'
            ELSE 'retired'
          END AS bucket,
          count(*) AS cnt,
          coalesce(sum(length(CAST(body AS BLOB))), 0) AS body_bytes
        FROM items
        GROUP BY bucket
        """
    ):
        status_counts[r["bucket"]] = {"count": int(r["cnt"]), "body_bytes": int(r["body_bytes"])}

    # Integrity & FTS rowid alignment checks for both raw and folded FTS5 tables
    fts_items_total, fts_raw_matched, fts_fold_joined, fts_fold_count = con.execute(
        """
        SELECT
          (SELECT count(*) FROM items),
          (SELECT count(*) FROM items i JOIN items_raw r ON r.rowid = i.rowid
           WHERE r.title = i.title AND r.body = i.body),
          (SELECT count(*) FROM items i JOIN items_fold f ON f.rowid = i.rowid),
          (SELECT count(*) FROM items_fold)
        """
    ).fetchone()
    active_with_superseded_by = int(
        con.execute(
            "SELECT count(*) FROM items WHERE status = 'active' AND superseded_by IS NOT NULL AND superseded_by != ''"
        ).fetchone()[0]
    )
    dangling_superseded_by = int(
        con.execute(
            """
            SELECT count(*) FROM items i
            LEFT JOIN items t ON t.id = i.superseded_by
            WHERE i.superseded_by IS NOT NULL AND i.superseded_by != '' AND t.id IS NULL
            """
        ).fetchone()[0]
    )
    fts_aligned = (
        int(fts_items_total)
        == int(fts_raw_matched)
        == int(fts_fold_joined)
        == int(fts_fold_count)
    )

    active_rows = con.execute(
        """
        SELECT id, repo, kind, title, body, refs, tags, source, tag, evidence, as_of,
               length(CAST(body AS BLOB)) AS body_bytes
        FROM items
        WHERE status = 'active'
        ORDER BY as_of DESC, id ASC
        """
    ).fetchall()

    total_active = len(active_rows)
    legacy_active = sum(
        1
        for r in active_rows
        if r["id"].startswith("legacy:")
        or (r["source"] or "").startswith("legacy:")
        or r["tag"] == "n/a"
    )
    native_active = total_active - legacy_active

    logs = {
        "hook.log": file_stats(home / "hook.log"),
        "usage.log": file_stats(home / "usage.log"),
        "catchup.log": file_stats(home / "catchup.log"),
    }

    empty_refs_active = 0
    bare_tag_titles: list[dict] = []
    short_titles: list[dict] = []
    self_declared_retired: list[dict] = []
    stale_items: list[dict] = []
    oversized_items: list[dict] = []
    oversized_by_source: dict[str, dict[str, int]] = collections.defaultdict(
        lambda: {"count": 0, "body_bytes": 0}
    )

    repo_counts: collections.Counter[str] = collections.Counter()
    kind_counts: collections.Counter[str] = collections.Counter()
    tag_counts: collections.Counter[str] = collections.Counter()
    source_counts: collections.Counter[str] = collections.Counter()
    unassigned_source_kind: collections.Counter[tuple[str, str]] = collections.Counter()

    norm_body_groups: dict[str, list[sqlite3.Row]] = collections.defaultdict(list)
    norm_title_groups: dict[str, list[sqlite3.Row]] = collections.defaultdict(list)
    session_tasks: dict[tuple[str, str], list[sqlite3.Row]] = collections.defaultdict(list)
    session_handoffs: dict[tuple[str, str], list[sqlite3.Row]] = collections.defaultdict(list)

    stale_cutoff = ref_now - timedelta(days=stale_days)
    handoff_cutoff = ref_now - timedelta(days=handoff_days)
    handoff_30_cutoff = ref_now - timedelta(days=30)
    episode_cutoff = ref_now - timedelta(days=episode_days)

    handoffs_older = 0
    handoffs_older_30d = 0
    episodes_older = 0

    for r in active_rows:
        repo = r["repo"]
        kind = r["kind"]
        tag = r["tag"]
        title = r["title"] or ""
        body = r["body"] or ""
        src = r["source"] or ""
        refs = (r["refs"] or "").strip()
        blen = int(r["body_bytes"])
        spref = source_prefix(src)

        repo_counts[repo] += 1
        kind_counts[kind] += 1
        tag_counts[tag] += 1
        source_counts[spref] += 1
        if repo == "unassigned":
            unassigned_source_kind[(spref, kind)] += 1

        if not refs or refs == "[]":
            empty_refs_active += 1

        t_trim = title.strip()
        t_low = t_trim.lower()
        stub_entry = {
            "id": r["id"],
            "repo": repo,
            "kind": kind,
            "tag": tag,
            "title": title,
            "as_of": r["as_of"],
            "source": src,
            "source_prefix": spref,
            "file_backed": is_file_backed(src),
        }
        if not t_trim or t_low in STUB_EXACT_TITLES:
            bare_tag_titles.append(stub_entry)
        elif len(t_trim) < 15:
            short_titles.append(stub_entry)

        if SELF_RETIRED_RE.search(t_trim):
            self_declared_retired.append(
                {
                    "id": r["id"],
                    "repo": repo,
                    "kind": kind,
                    "tag": tag,
                    "as_of": r["as_of"],
                    "source": src,
                    "source_prefix": spref,
                    "file_backed": is_file_backed(src),
                    "title": title[:120],
                }
            )

        nb = normalize_ws(body)
        if len(nb) >= 30:
            norm_body_groups[nb].append(r)

        nt = normalize_ws(title)
        if len(nt) >= 20:
            norm_title_groups[nt].append(r)

        sk = parse_session_key(src)
        if sk is not None:
            sk_kind, sk_id = sk
            if sk_kind == "task":
                session_tasks[(repo, sk_id)].append(r)
            elif sk_kind == "handoff":
                session_handoffs[(repo, sk_id)].append(r)

        dt = parse_iso_dt(r["as_of"])
        if dt is not None:
            if tag in STALE_TAGS and dt < stale_cutoff:
                stale_items.append(
                    {
                        "id": r["id"],
                        "repo": repo,
                        "kind": kind,
                        "tag": tag,
                        "as_of": r["as_of"],
                        "source": src,
                        "source_prefix": spref,
                        "file_backed": is_file_backed(src),
                        "title": title[:120],
                    }
                )
            if kind == "handoff":
                if dt < handoff_cutoff:
                    handoffs_older += 1
                if dt < handoff_30_cutoff:
                    handoffs_older_30d += 1
            if kind == "episode" and dt < episode_cutoff:
                episodes_older += 1

        if blen > oversized_bytes:
            oversized_by_source[spref]["count"] += 1
            oversized_by_source[spref]["body_bytes"] += blen
            oversized_items.append(
                {
                    "id": r["id"],
                    "repo": repo,
                    "kind": kind,
                    "tag": tag,
                    "body_bytes": blen,
                    "source": src,
                    "source_prefix": spref,
                    "file_backed": is_file_backed(src),
                    "title": title[:100],
                }
            )

    oversized_items.sort(key=lambda x: (-x["body_bytes"], x["id"]))

    exact_body_dups = [
        {
            "count": len(grp),
            "repos": dict(collections.Counter(x["repo"] for x in grp)),
            "ids": [x["id"] for x in grp],
            "sources": [source_prefix(x["source"]) for x in grp],
            "title": (grp[0]["title"] or "")[:100],
        }
        for grp in norm_body_groups.values()
        if len(grp) > 1
    ]
    exact_body_dups.sort(key=lambda x: (-x["count"], x["ids"][0]))

    exact_title_dups = []
    cross_repo_unassigned_dups = 0
    for grp in norm_title_groups.values():
        if len(grp) > 1:
            repos = collections.Counter(x["repo"] for x in grp)
            if "unassigned" in repos and len(repos) > 1:
                cross_repo_unassigned_dups += 1
            exact_title_dups.append(
                {
                    "count": len(grp),
                    "repos": dict(repos),
                    "ids": [x["id"] for x in grp],
                    "sources": [source_prefix(x["source"]) for x in grp],
                    "title": (grp[0]["title"] or "")[:100],
                }
            )
    exact_title_dups.sort(key=lambda x: (-x["count"], x["ids"][0]))

    same_session_overlaps: list[dict] = []
    for (repo, sid), hrows in sorted(session_handoffs.items()):
        trows = session_tasks.get((repo, sid))
        if not trows:
            continue
        for hr in hrows:
            same_session_overlaps.append(
                {
                    "repo": repo,
                    "session_prefix": sid,
                    "handoff_id": hr["id"],
                    "handoff_tag": hr["tag"],
                    "handoff_source_prefix": source_prefix(hr["source"]),
                    "task_ids": [tr["id"] for tr in trows],
                    "handoff_title": (hr["title"] or "")[:90],
                }
            )

    unassigned_count = repo_counts.get("unassigned", 0)
    orphan_repos = [
        {"repo": repo, "count": cnt}
        for repo, cnt in sorted(repo_counts.items(), key=lambda kv: (kv[1], kv[0]))
        if repo not in ("global", "unassigned") and cnt <= 5
    ]

    scopes, _aliases, scopes_loaded = load_config_scopes(home)
    scope_code_matchers: dict[str, list[re.Pattern[str]]] = {}
    scope_repo_names: dict[str, set[str]] = collections.defaultdict(set)
    for scope_path, matchers in scopes.items():
        top_scope = scope_path.split("/", 1)[0]
        for m in matchers:
            if m.startswith("repo:"):
                name = m[len("repo:") :].strip()
                if len(name) >= 4:
                    scope_repo_names[top_scope].add(name)
            elif len(m) >= 3:
                scope_code_matchers.setdefault(top_scope, []).append(
                    re.compile(rf"\b{re.escape(m)}\b")
                )

    concrete_repos = {
        r for r in repo_counts if r not in ("global", "unassigned") and len(r) >= 4
    }
    for names in scope_repo_names.values():
        concrete_repos.update(names)
    concrete_repo_pats = {
        r: re.compile(rf"(?i)\b{re.escape(r)}\b") for r in sorted(concrete_repos)
    }

    unassigned_mentioning_scope_term: collections.Counter[str] = collections.Counter()
    unassigned_matching_repo: collections.Counter[str] = collections.Counter()
    for r in active_rows:
        if r["repo"] != "unassigned":
            continue
        haystack = f"{r['title'] or ''}\n{r['body'] or ''}"
        for scope_key, pats in scope_code_matchers.items():
            if any(pat.search(haystack) for pat in pats):
                unassigned_mentioning_scope_term[scope_key] += 1
        for repo_name, pat in concrete_repo_pats.items():
            if pat.search(haystack):
                unassigned_matching_repo[repo_name] += 1

    recall_summary = _summarize_recall_logs(
        con, home, ref_now=ref_now, min_injections=noise_min_injections, limit=limit
    )

    return {
        "home": str(home),
        "as_of": ref_now.isoformat(),
        "storage": {
            "db_bytes": db_bytes,
            "wal_bytes": wal_bytes,
            "shm_bytes": shm_bytes,
            "journal_mode": journal_mode,
            "page_count": page_count,
            "page_size": page_size,
            "freelist_count": freelist_count,
            "freelist_bytes": freelist_bytes,
            "total_items": sum(v["count"] for v in status_counts.values()),
            "statuses": status_counts,
            "active_legacy": legacy_active,
            "active_native": native_active,
            "fts_aligned": fts_aligned,
            "fts_items_total": int(fts_items_total),
            "fts_raw_matched": int(fts_raw_matched),
            "fts_fold_joined": int(fts_fold_joined),
            "fts_fold_count": int(fts_fold_count),
            "logs": logs,
        },
        "hygiene": {
            "empty_refs_active": empty_refs_active,
            "bare_tag_titles_count": len(bare_tag_titles),
            "bare_tag_titles": bare_tag_titles[:limit],
            "short_titles_count": len(short_titles),
            "short_titles": short_titles[:limit],
            "exact_body_dups_groups": len(exact_body_dups),
            "exact_body_dups": exact_body_dups[:limit],
            "exact_title_dups_groups": len(exact_title_dups),
            "cross_repo_unassigned_dups": cross_repo_unassigned_dups,
            "exact_title_dups": exact_title_dups[:limit],
            "same_session_handoff_task_count": len(same_session_overlaps),
            "same_session_handoff_task": same_session_overlaps[:limit],
            "self_declared_retired_count": len(self_declared_retired),
            "self_declared_retired": self_declared_retired[:limit],
            "stale_days": stale_days,
            "stale_count": len(stale_items),
            "stale_items": stale_items[:limit],
            "active_with_superseded_by": active_with_superseded_by,
            "dangling_superseded_by": dangling_superseded_by,
        },
        "scope_health": {
            "scopes_loaded": scopes_loaded,
            "unassigned_active": unassigned_count,
            "unassigned_pct": round((unassigned_count / total_active * 100.0), 1)
            if total_active
            else 0.0,
            "unassigned_by_source_kind": {
                f"{sp}|{k}": c for (sp, k), c in unassigned_source_kind.most_common(15)
            },
            "orphan_repos_le_5": orphan_repos,
            "unassigned_mentioning_scope_term": dict(
                unassigned_mentioning_scope_term.most_common()
            ),
            "unassigned_matching_repo": dict(unassigned_matching_repo.most_common(15)),
            "top_repos": dict(repo_counts.most_common(15)),
            "kinds": dict(kind_counts.most_common()),
            "tags": dict(tag_counts.most_common()),
            "sources": dict(source_counts.most_common()),
        },
        "accumulation": {
            "handoff_days": handoff_days,
            "active_handoffs_older": handoffs_older,
            "active_handoffs_older_30d": handoffs_older_30d,
            "episode_days": episode_days,
            "active_episodes_older": episodes_older,
            "oversized_bytes": oversized_bytes,
            "oversized_count": len(oversized_items),
            "oversized_by_source": dict(
                sorted(oversized_by_source.items(), key=lambda kv: -kv[1]["body_bytes"])
            ),
            "oversized_top": oversized_items[:limit],
        },
        "recall_noise": recall_summary,
    }


def _summarize_recall_logs(
    con: sqlite3.Connection,
    home: pathlib.Path,
    *,
    ref_now: datetime,
    min_injections: int,
    limit: int,
) -> dict:
    hook_rows, hook_bad = parse_jsonl_rows(home / "hook.log")
    usage_rows, usage_bad = parse_jsonl_rows(home / "usage.log")
    since_7d = ref_now - timedelta(days=7)

    hook_7d = 0
    with_inj_7d = 0
    local_7d = 0
    injections_7d: list[tuple[datetime, str]] = []
    for row in hook_rows:
        dt = parse_iso_dt(row.get("ts"))
        if dt is None:
            hook_bad += 1
            continue
        if dt < since_7d or "catch_up_error" in row:
            continue
        hook_7d += 1
        raw_ids = row.get("ids") if isinstance(row.get("ids"), list) else []
        ids = [i for i in raw_ids if isinstance(i, str)]
        if ids:
            with_inj_7d += 1
        if row.get("path") == "local":
            local_7d += 1
        for item_id in ids:
            injections_7d.append((dt, item_id))

    tracked_since_7d: datetime | None = None
    shown_times_7d: dict[str, list[datetime]] = collections.defaultdict(list)
    usage_rows_7d = 0
    for row in usage_rows:
        dt = parse_iso_dt(row.get("ts"))
        if dt is None:
            usage_bad += 1
            continue
        if dt < since_7d:
            continue
        usage_rows_7d += 1
        if tracked_since_7d is None or dt < tracked_since_7d:
            tracked_since_7d = dt
        if row.get("cmd") == "show":
            ids = row.get("ids") if isinstance(row.get("ids"), list) else []
            for item_id in ids:
                if isinstance(item_id, str):
                    shown_times_7d[item_id].append(dt)

    first_tracked: dict[str, datetime] = {}
    tracked_counts: collections.Counter[str] = collections.Counter()
    if tracked_since_7d is not None:
        for dt, item_id in injections_7d:
            if dt >= tracked_since_7d:
                if item_id not in first_tracked or dt < first_tracked[item_id]:
                    first_tracked[item_id] = dt
                tracked_counts[item_id] += 1

    opened_ids = {
        iid
        for iid, first_dt in first_tracked.items()
        if any(ts >= first_dt for ts in shown_times_7d.get(iid, ()))
    }
    never_opened_candidates = [
        (iid, cnt)
        for iid, cnt in tracked_counts.most_common()
        if cnt >= min_injections and iid not in shown_times_7d
    ]

    never_opened_top: list[dict] = []
    if never_opened_candidates:
        cand_ids = [iid for iid, _ in never_opened_candidates]
        meta: dict[str, sqlite3.Row] = {}
        chunk = 500
        for i in range(0, len(cand_ids), chunk):
            part = cand_ids[i : i + chunk]
            placeholders = ",".join("?" for _ in part)
            for r in con.execute(
                f"SELECT id, repo, kind, tag, status, source, title FROM items WHERE status = 'active' AND id IN ({placeholders})",
                part,
            ):
                meta[r["id"]] = r
        for iid, cnt in never_opened_candidates:
            r = meta.get(iid)
            if r is None:
                continue
            never_opened_top.append(
                {
                    "id": iid,
                    "injections": cnt,
                    "repo": r["repo"],
                    "kind": r["kind"],
                    "tag": r["tag"],
                    "status": r["status"],
                    "source": r["source"],
                    "source_prefix": source_prefix(r["source"]),
                    "title": (r["title"] or "")[:90],
                }
            )
            if len(never_opened_top) >= limit:
                break

    return {
        "hook_runs_7d": hook_7d,
        "with_injection_7d": with_inj_7d,
        "local_fallback_7d": local_7d,
        "usage_rows_7d": usage_rows_7d,
        "tracked_since_7d": tracked_since_7d.isoformat() if tracked_since_7d else None,
        "tracked_ids_7d": len(first_tracked),
        "opened_ids_7d": len(opened_ids),
        "opened_pct_7d": round((len(opened_ids) / len(first_tracked) * 100.0), 1)
        if first_tracked
        else 0.0,
        "bad_log_rows": {"hook.log": hook_bad, "usage.log": usage_bad},
        "never_opened_min_injections": min_injections,
        "never_opened": never_opened_top,
    }


def fmt_bytes(num: int) -> str:
    if num >= 1024 * 1024:
        return f"{num / (1024 * 1024):.2f} MiB"
    if num >= 1024:
        return f"{num / 1024:.1f} KiB"
    return f"{num} B"


def render_markdown(report: dict) -> str:
    st = report["storage"]
    hy = report["hygiene"]
    sc = report["scope_health"]
    ac = report["accumulation"]
    rn = report["recall_noise"]
    statuses = st["statuses"]
    active_cnt = statuses.get("active", {}).get("count", 0)
    sup_cnt = statuses.get("superseded", {}).get("count", 0)
    ret_cnt = statuses.get("retired", {}).get("count", 0)

    lines = [
        f"# simple-memory Audit ({report['as_of'][:19]})",
        "",
        f"Home: `{report['home']}`",
        "",
        "## 1. Storage & Logs",
        "",
        f"- **DB**: {fmt_bytes(st['db_bytes'])} (`journal={st['journal_mode']}`, `freelist={fmt_bytes(st['freelist_bytes'])}`, `wal={fmt_bytes(st['wal_bytes'])}`, `fts_aligned={st['fts_aligned']}`)",
        f"- **Items**: total={st['total_items']} | active={active_cnt} (native={st['active_native']}, legacy={st['active_legacy']}) | superseded={sup_cnt} | retired={ret_cnt}",
        (
            "- **Body bytes**: "
            f"active={fmt_bytes(statuses.get('active', {}).get('body_bytes', 0))}, "
            f"superseded={fmt_bytes(statuses.get('superseded', {}).get('body_bytes', 0))}, "
            f"retired={fmt_bytes(statuses.get('retired', {}).get('body_bytes', 0))}"
        ),
        (
            "- **Logs**: "
            + ", ".join(
                f"{name}={fmt_bytes(info['bytes'])} ({info['lines']} rows)"
                for name, info in st["logs"].items()
            )
        ),
        "",
        "## 2. Hygiene Findings (Tier-1 Candidates)",
        "",
        f"- **Bare-tag / empty titles**: {hy['bare_tag_titles_count']} (benign short titles `<15` chars: {hy['short_titles_count']})",
        f"- **Exact normalized body duplicate groups**: {hy['exact_body_dups_groups']}",
        f"- **Exact normalized title duplicate groups**: {hy['exact_title_dups_groups']} (cross-repo with `unassigned`: {hy['cross_repo_unassigned_dups']})",
        f"- **Self-declared retired/superseded active items**: {hy['self_declared_retired_count']}",
        f"- **Stale `planned`/`assumed` (`>{hy['stale_days']}d`)**: {hy['stale_count']}",
        f"- **Chain anomalies**: `active_with_superseded_by`={hy['active_with_superseded_by']}, `dangling_superseded_by`={hy['dangling_superseded_by']}",
        f"- **Active items with empty refs**: {hy['empty_refs_active']} (run `sm doctor --backfill-refs --dry-run` to check extractable count)",
        "",
        "## 3. Scope, Accumulation & Recall Signal",
        "",
        f"- **`unassigned` active**: {sc['unassigned_active']} ({sc['unassigned_pct']}% of active) | top source/kind: {sc['unassigned_by_source_kind']}",
        f"- **`unassigned` naming a concrete repo**: {sc['unassigned_matching_repo'] or '{}'}",
        f"- **`unassigned` matching `[scopes]` codes** (loaded={sc['scopes_loaded']}): {sc['unassigned_mentioning_scope_term'] or '{}'}",
        f"- **Orphan/small repos (`<=5` active)**: {len(sc['orphan_repos_le_5'])}",
        f"- **Same-session active `handoff` + `task` overlaps**: {hy['same_session_handoff_task_count']}",
        f"- **Ephemeral active**: `handoff > {ac['handoff_days']}d` = {ac['active_handoffs_older']} (`>30d` = {ac['active_handoffs_older_30d']}); `episode > {ac['episode_days']}d` = {ac['active_episodes_older']}",
        f"- **Oversized active (`>{fmt_bytes(ac['oversized_bytes'])}`)**: {ac['oversized_count']}",
        (
            f"- **Recall 7d**: hook_runs={rn['hook_runs_7d']}, with_injection={rn['with_injection_7d']}, "
            f"local_fallback={rn['local_fallback_7d']}, open_rate_7d={rn['opened_pct_7d']}% ({rn['opened_ids_7d']}/{rn['tracked_ids_7d']})"
        ),
    ]

    if hy["bare_tag_titles"]:
        lines += ["", "### Bare-Tag / Empty Titles", ""]
        for item in hy["bare_tag_titles"]:
            lines.append(
                f"- `{item['id']}` [{item['repo']}/{item['kind']}/{item['tag']}] src={item['source_prefix']} title=`{item['title']}` (file_backed={item['file_backed']})"
            )

    if hy["short_titles"]:
        lines += ["", "### Short Titles (<15 chars)", ""]
        for item in hy["short_titles"]:
            lines.append(
                f"- `{item['id']}` [{item['repo']}/{item['kind']}/{item['tag']}] src={item['source_prefix']} title=`{item['title']}`"
            )

    if hy["exact_body_dups"]:
        lines += ["", "### Exact Body Duplicate Groups", ""]
        for grp in hy["exact_body_dups"]:
            lines.append(
                f"- x{grp['count']} repos={grp['repos']} src={grp['sources']} ids={grp['ids']}: {grp['title']}"
            )

    if hy["exact_title_dups"]:
        lines += ["", "### Exact Title Duplicate Groups", ""]
        for grp in hy["exact_title_dups"]:
            lines.append(
                f"- x{grp['count']} repos={grp['repos']} src={grp['sources']} ids={grp['ids']}: {grp['title']}"
            )

    if hy["self_declared_retired"]:
        lines += ["", "### Self-Declared Obsolete Active Items", ""]
        for item in hy["self_declared_retired"]:
            lines.append(
                f"- `{item['id']}` [{item['repo']}/{item['kind']}/{item['tag']}] src={item['source_prefix']} (file_backed={item['file_backed']}): {item['title']}"
            )

    if hy["stale_items"]:
        lines += ["", "### Stale `planned`/`assumed` Items", ""]
        for item in hy["stale_items"]:
            lines.append(
                f"- `{item['id']}` [{item['repo']}/{item['kind']}/{item['tag']}] src={item['source_prefix']} {item['as_of'][:10]} (file_backed={item['file_backed']}): {item['title']}"
            )

    if hy["same_session_handoff_task"]:
        lines += ["", "### Same-Session Active Handoff + Task Overlaps", ""]
        for item in hy["same_session_handoff_task"]:
            lines.append(
                f"- `{item['handoff_id']}` [{item['repo']}/handoff/{item['handoff_tag']}] src={item['handoff_source_prefix']} sid={item['session_prefix']} tasks={item['task_ids']}: {item['handoff_title']}"
            )

    if sc["orphan_repos_le_5"]:
        lines += ["", "### Orphan / Small Repos (<=5 active)", ""]
        for item in sc["orphan_repos_le_5"]:
            lines.append(f"- `{item['repo']}`: {item['count']}")

    if ac["oversized_top"]:
        lines += ["", "### Oversized Active Items", ""]
        for item in ac["oversized_top"]:
            lines.append(
                f"- `{item['id']}` {fmt_bytes(item['body_bytes'])} [{item['repo']}/{item['kind']}/{item['tag']}] src={item['source_prefix']} (file_backed={item['file_backed']}): {item['title']}"
            )

    if rn["never_opened"]:
        lines += ["", f"### Never-Opened Active High-Injection Items (>={rn['never_opened_min_injections']}x)", ""]
        for item in rn["never_opened"]:
            lines.append(
                f"- `{item['id']}` inj={item['injections']} [{item['repo']}/{item['kind']}/{item['tag']}] src={item['source_prefix']} {item['title']}"
            )

    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--home", help="simple-memory home dir (default: $SIMPLE_MEMORY_HOME or ~/.simple-memory)")
    ap.add_argument("--stale-days", type=int, default=30)
    ap.add_argument("--handoff-days", type=int, default=14)
    ap.add_argument("--episode-days", type=int, default=30)
    ap.add_argument("--oversized-bytes", type=int, default=4096)
    ap.add_argument("--noise-min-injections", type=int, default=5)
    ap.add_argument("--limit", type=int, default=20)
    ap.add_argument("--format", choices=("markdown", "json"), default="markdown")
    args = ap.parse_args(argv)

    home = resolve_home(args.home)
    try:
        report = audit_home(
            home,
            stale_days=args.stale_days,
            handoff_days=args.handoff_days,
            episode_days=args.episode_days,
            oversized_bytes=args.oversized_bytes,
            noise_min_injections=args.noise_min_injections,
            limit=args.limit,
        )
    except FileNotFoundError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    except sqlite3.Error as exc:
        print(f"sqlite error: {exc}", file=sys.stderr)
        return 2

    if args.format == "json":
        print(json.dumps(report, indent=2, ensure_ascii=False))
    else:
        print(render_markdown(report), end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

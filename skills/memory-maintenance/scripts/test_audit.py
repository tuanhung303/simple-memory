#!/usr/bin/env python3
"""Unit tests for skills/memory-maintenance/scripts/audit.py."""

from __future__ import annotations

from datetime import datetime, timedelta, timezone
import hashlib
import json
import os
import pathlib
import sqlite3
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import audit  # noqa: E402

REPO_ROOT = pathlib.Path(__file__).resolve().parents[3]
MIGRATIONS_DIR = REPO_ROOT / "rust" / "migrations"


def _create_sm_db(db_path: pathlib.Path) -> sqlite3.Connection:
    con = sqlite3.connect(str(db_path))
    con.execute("PRAGMA journal_mode = WAL")
    con.create_function("sm_fold", 1, lambda s: (s or "").lower())
    for sql_file in sorted(MIGRATIONS_DIR.glob("*.sql")):
        con.executescript(sql_file.read_text(encoding="utf-8"))
    return con


def _insert_item(
    con: sqlite3.Connection,
    *,
    item_id: str,
    repo: str = "acme",
    kind: str = "fact",
    title: str = "Valid item title with enough context",
    body: str = "Valid body text explaining a real decision or verified fact.",
    refs: str = "PR-1",
    tags: str = "",
    source: str = "",
    tag: str = "verified",
    evidence: str = "test suite",
    as_of: str = "2026-11-20T10:00:00+00:00",
    status: str = "active",
    superseded_by: str | None = None,
) -> None:
    content_hash = hashlib.sha256(f"{item_id}:{body}".encode("utf-8")).hexdigest()[:16]
    con.execute(
        """
        INSERT INTO items (id, repo, kind, title, body, refs, tags, source, tag, evidence, as_of, status, superseded_by, content_hash)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        (
            item_id,
            repo,
            kind,
            title,
            body,
            refs,
            tags,
            source,
            tag,
            evidence,
            as_of,
            status,
            superseded_by,
            content_hash,
        ),
    )


class AuditScriptTest(unittest.TestCase):
    def test_missing_db_exits_nonzero_without_writing_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = pathlib.Path(tmp) / "empty-home"
            home.mkdir()
            rc = audit.main(["--home", str(home), "--format", "json"])
            self.assertEqual(rc, 2)
            self.assertFalse((home / "memory.db").exists())

    def test_clean_store_and_readonly_dir_create_no_sidecars(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = pathlib.Path(tmp)
            db_path = home / "memory.db"
            con = _create_sm_db(db_path)
            _insert_item(
                con,
                item_id="m_clean_1",
                repo="acme",
                kind="decision",
                tag="decided",
                title="Use pooled database connections per worker",
                body="Keep HTTP and DB pool sizes bounded to 16 sockets per worker.",
                refs="ADR-004",
                as_of="2026-11-22T09:00:00+00:00",
            )
            con.commit()
            con.close()
            # Cleanly closed WAL DB has no sidecars; verify opening on 0555 dir leaves none
            for sidecar in (home / "memory.db-wal", home / "memory.db-shm"):
                sidecar.unlink(missing_ok=True)

            now = datetime(2026, 11, 24, 8, 0, tzinfo=timezone.utc)
            os.chmod(home, 0o555)
            try:
                rep = audit.audit_home(home, now=now)
            finally:
                os.chmod(home, 0o755)

            self.assertFalse((home / "memory.db-wal").exists())
            self.assertFalse((home / "memory.db-shm").exists())
            self.assertEqual(rep["storage"]["journal_mode"], "wal")
            self.assertTrue(rep["storage"]["fts_aligned"])
            self.assertEqual(rep["storage"]["total_items"], 1)
            self.assertEqual(rep["hygiene"]["empty_refs_active"], 0)
            self.assertEqual(rep["hygiene"]["bare_tag_titles_count"], 0)
            self.assertEqual(rep["hygiene"]["short_titles_count"], 0)
            self.assertEqual(rep["hygiene"]["exact_body_dups_groups"], 0)
            self.assertEqual(rep["hygiene"]["exact_title_dups_groups"], 0)
            self.assertEqual(rep["hygiene"]["same_session_handoff_task_count"], 0)
            self.assertEqual(rep["hygiene"]["self_declared_retired_count"], 0)
            self.assertEqual(rep["hygiene"]["stale_count"], 0)
            self.assertEqual(rep["hygiene"]["active_with_superseded_by"], 0)
            self.assertEqual(rep["hygiene"]["dangling_superseded_by"], 0)
            self.assertEqual(rep["accumulation"]["oversized_count"], 0)
            md = audit.render_markdown(rep)
            self.assertIn("# simple-memory Audit", md)
            json.dumps(rep)

    def test_detects_fts_fold_drift(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = pathlib.Path(tmp)
            con = _create_sm_db(home / "memory.db")
            _insert_item(con, item_id="m_fts_1", title="FTS alignment target item")
            con.execute("DROP TRIGGER IF EXISTS items_ad")
            con.execute("DELETE FROM items_fold WHERE rowid = 1")
            con.commit()
            con.close()

            rep = audit.audit_home(home)
            self.assertFalse(rep["storage"]["fts_aligned"])
            self.assertEqual(rep["storage"]["fts_fold_joined"], 0)
            self.assertEqual(rep["storage"]["fts_fold_count"], 0)

    def test_detects_defects_log_quirks_and_never_writes_db(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = pathlib.Path(tmp)
            (home / "config.toml").write_text(
                '[scopes]\n"acme" = ["repo:forge", "GLX"]\n',
                encoding="utf-8",
            )
            db_path = home / "memory.db"
            con = _create_sm_db(db_path)

            # 1. Clean concrete item establishing repo "forge"
            _insert_item(
                con,
                item_id="m_forge_1",
                repo="forge",
                kind="fact",
                title="Forge billing service uses idempotency keys",
                body="All payment mutations in forge carry an idempotency key header.",
                refs="PR-10",
                as_of="2026-11-23T08:00:00+00:00",
            )
            # 2. Bare-tag stub title (refs="") + benign short title (refs="[]")
            _insert_item(
                con,
                item_id="m_stub_1",
                repo="acme",
                kind="decision",
                title="decided",
                body="Chose bounded backoff with jitter for webhook retries.",
                refs="",
                superseded_by="",  # empty string on active row must NOT count as active_with_superseded_by
                as_of="2026-11-20T08:00:00+00:00",
            )
            _insert_item(
                con,
                item_id="m_short_ok",
                repo="acme",
                kind="fact",
                title="Bolt CLI",
                body="Bolt CLI builds static packages for internal runners.",
                refs="[]",
                as_of="2026-11-20T08:00:00+00:00",
            )
            # 3. Exact normalized title dup (unassigned vs acme) + exact normalized body dup (distinct titles)
            _insert_item(
                con,
                item_id="m_dup_a",
                repo="acme",
                kind="fact",
                title="Connection pool size 16 keeps p99 under 12 ms",
                body="Benchmark run on acme worker confirmed 16 socket limit.",
                as_of="2026-11-19T08:00:00+00:00",
            )
            _insert_item(
                con,
                item_id="m_dup_b",
                repo="unassigned",
                kind="fact",
                title="Connection pool size 16 keeps p99 under 12 ms",
                body="Duplicate unassigned record from fallback scan.",
                as_of="2026-11-19T09:00:00+00:00",
            )
            _insert_item(
                con,
                item_id="m_bdup_1",
                repo="acme",
                kind="fact",
                title="First distinct title for exact body duplicate",
                body="Shared canonical retry policy across queue workers stays at three attempts.",
                as_of="2026-11-18T08:00:00+00:00",
            )
            _insert_item(
                con,
                item_id="m_bdup_2",
                repo="acme",
                kind="fact",
                title="Second distinct title for exact body duplicate",
                body="Shared   canonical retry policy across queue workers stays at three attempts.",
                as_of="2026-11-18T09:00:00+00:00",
            )
            # 4. Same-(repo, session) handoff+task overlap vs cross-repo & orphan handoffs
            sess_hex = "a7c87239"
            uuid_tail = "-".join(("f4a2", "4c91", "b81e", "7d9a3e2c5f18"))
            _insert_item(
                con,
                item_id="m_sess_task",
                repo="acme",
                kind="task",
                source=f"task:20261110-120000-{sess_hex}-rate-limit",
                title="Rate limit middleware added and tested on edge",
                body="Added sliding window limiter.",
                as_of="2026-11-10T12:00:00+00:00",
            )
            _insert_item(
                con,
                item_id="m_sess_handoff",
                repo="acme",
                kind="handoff",
                source=f"handoff:20261110-120100-{sess_hex}-{uuid_tail}.md",
                title="Rate limit middleware handoff notes",
                body="Done: sliding window limiter.",
                as_of="2026-11-01T12:01:00+00:00",
            )
            # Same 8-hex session id in DIFFERENT repo must NOT pair with acme task
            _insert_item(
                con,
                item_id="m_cross_handoff",
                repo="bolt",
                kind="handoff",
                source=f"handoff:20261110-120200-{sess_hex}-{uuid_tail}.md",
                title="Cross repo handoff sharing session prefix",
                body="Bolt repo note with no task in bolt.",
                as_of="2026-11-22T12:02:00+00:00",
            )
            # Handoff whose session id has NO task anywhere
            _insert_item(
                con,
                item_id="m_orphan_handoff",
                repo="acme",
                kind="handoff",
                source=f"handoff:20261112-120000-deadbeef-{uuid_tail}.md",
                title="Standalone handoff without a task log",
                body="No corresponding task log exists.",
                as_of="2026-11-22T12:00:00+00:00",
            )
            # 5. Self-declared retired (delimiter-marked) vs false-positive lookalike
            _insert_item(
                con,
                item_id="m_self_ret",
                repo="acme",
                kind="fact",
                source="automemory:-tmp-acme/retired-rule.md",
                title="SUPERSEDED 2026-10-08: old static routing rule",
                body="Old static routing rule no longer in use.",
                as_of="2026-10-08T08:00:00+00:00",
            )
            _insert_item(
                con,
                item_id="m_not_self_ret",
                repo="acme",
                kind="fact",
                title="Deprecated API usage warnings belong in the migration guide",
                body="Keep migration notices visible when users call older flags.",
                as_of="2026-11-21T08:00:00+00:00",
            )
            # 6. Stale planned item + unassigned matching GLX & repo forge + multi-byte UTF-8 >4096 bytes (<4096 chars!)
            multibyte_body = "GLX forge " + ("\u0111" * 2100)
            _insert_item(
                con,
                item_id="m_stale_big",
                repo="unassigned",
                kind="episode",
                tag="planned",
                title="Draft migration schedule for GLX and forge",
                body=multibyte_body,
                as_of="2026-10-01T08:00:00+00:00",
            )
            # 7. Active anomaly + superseded + retired (empty string) + dangling + freelist
            _insert_item(
                con,
                item_id="m_active_sup",
                repo="acme",
                kind="fact",
                title="Active item erroneously marked superseded_by",
                body="Tests active_with_superseded_by detector.",
                status="active",
                superseded_by="m_forge_1",
            )
            _insert_item(
                con,
                item_id="m_sup_1",
                repo="sup_only_repo",
                kind="fact",
                title="Superseded draft configuration entry",
                body="Old superseded config body.",
                refs="",
                status="superseded",
                superseded_by="m_forge_1",
            )
            _insert_item(
                con,
                item_id="m_ret_empty",
                repo="acme",
                kind="fact",
                title="Retired item with empty superseded_by string",
                body="Retired note.",
                status="superseded",
                superseded_by="",
            )
            _insert_item(
                con,
                item_id="m_dangling",
                repo="acme",
                kind="fact",
                title="Superseded item pointing to missing id",
                body="Dangling target test.",
                status="superseded",
                superseded_by="m_missing_target",
            )
            _insert_item(
                con,
                item_id="m_tmp_del",
                repo="acme",
                kind="fact",
                title="Temporary row to create freelist pages",
                body="x" * 16000,
            )
            con.execute("DELETE FROM items WHERE id = 'm_tmp_del'")
            con.commit()
            con.execute("PRAGMA wal_checkpoint(TRUNCATE)")
            con.close()

            # Logs: m_forge_1 shown AFTER injection; m_short_ok shown BEFORE injection (kills M13)
            now = datetime(2026, 11, 24, 8, 0, tzinfo=timezone.utc)
            (home / "usage.log").write_text(
                "\n".join(
                    [
                        json.dumps(
                            {
                                "ts": (now - timedelta(days=5)).strftime("%Y-%m-%dT%H:%M:%S+00:00"),
                                "cmd": "show",
                                "ids": ["m_short_ok"],
                            }
                        ),
                        json.dumps(
                            {
                                "ts": (now - timedelta(days=1)).strftime("%Y-%m-%dT%H:%M:%S+00:00"),
                                "cmd": "show",
                                "ids": ["m_forge_1"],
                            }
                        ),
                        json.dumps({"ts": 5, "cmd": "show", "ids": ["m_stub_1"]}),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            hook_lines = [
                json.dumps(
                    {
                        "ts": (now - timedelta(days=2, hours=i)).strftime("%Y-%m-%dT%H:%M:%S+00:00"),
                        "path": "haiku" if i % 2 == 0 else "local",
                        "ids": (
                            ["m_stub_1", "m_forge_1", "m_short_ok", "m_sup_1"]
                            if i == 0
                            else ["m_stub_1", "m_sup_1"]
                        ),
                    }
                )
                for i in range(6)
            ]
            hook_lines.append(
                json.dumps(
                    {
                        "ts": (now - timedelta(days=1)).strftime("%Y-%m-%dT%H:%M:%S+00:00"),
                        "catch_up_error": "locked",
                    }
                )
            )
            hook_lines.append(json.dumps({"ts": 12345, "path": "haiku", "ids": ["m_stub_1"]}))
            hook_lines.append("{corrupt-json-line")
            (home / "hook.log").write_text("\n".join(hook_lines) + "\n", encoding="utf-8")

            before_sha = hashlib.sha256(db_path.read_bytes()).hexdigest()
            before_stat = os.stat(db_path)

            rep = audit.audit_home(home, now=now)

            after_sha = hashlib.sha256(db_path.read_bytes()).hexdigest()
            after_stat = os.stat(db_path)
            self.assertEqual(before_sha, after_sha)
            self.assertEqual(before_stat.st_mtime_ns, after_stat.st_mtime_ns)

            self.assertTrue(rep["storage"]["fts_aligned"])
            self.assertGreater(rep["storage"]["freelist_bytes"], 0)
            self.assertEqual(rep["storage"]["statuses"]["superseded"]["count"], 2)
            self.assertEqual(rep["storage"]["statuses"]["retired"]["count"], 1)
            # Active-only check: sup_only_repo (superseded) must NOT appear in active repos or empty_refs_active
            self.assertNotIn("sup_only_repo", rep["scope_health"]["top_repos"])
            self.assertEqual(rep["hygiene"]["empty_refs_active"], 2)
            self.assertEqual(rep["hygiene"]["bare_tag_titles_count"], 1)
            self.assertEqual(rep["hygiene"]["bare_tag_titles"][0]["id"], "m_stub_1")
            self.assertEqual(rep["hygiene"]["short_titles_count"], 1)
            self.assertEqual(rep["hygiene"]["short_titles"][0]["id"], "m_short_ok")
            self.assertEqual(rep["hygiene"]["exact_body_dups_groups"], 1)
            self.assertEqual(rep["hygiene"]["exact_title_dups_groups"], 1)
            self.assertEqual(rep["hygiene"]["cross_repo_unassigned_dups"], 1)
            # Only m_sess_handoff pairs with m_sess_task (m_cross_handoff & m_orphan_handoff excluded)
            self.assertEqual(rep["hygiene"]["same_session_handoff_task_count"], 1)
            self.assertEqual(
                rep["hygiene"]["same_session_handoff_task"][0]["handoff_id"], "m_sess_handoff"
            )
            self.assertEqual(rep["hygiene"]["self_declared_retired_count"], 1)
            self.assertEqual(rep["hygiene"]["self_declared_retired"][0]["id"], "m_self_ret")
            self.assertTrue(rep["hygiene"]["self_declared_retired"][0]["file_backed"])
            self.assertEqual(rep["hygiene"]["stale_count"], 1)
            self.assertEqual(rep["hygiene"]["active_with_superseded_by"], 1)
            self.assertEqual(rep["hygiene"]["dangling_superseded_by"], 1)
            self.assertEqual(rep["scope_health"]["unassigned_matching_repo"].get("forge"), 1)
            if audit.tomllib is not None:
                self.assertTrue(rep["scope_health"]["scopes_loaded"])
                self.assertEqual(
                    rep["scope_health"]["unassigned_mentioning_scope_term"].get("acme"), 1
                )
            self.assertEqual(rep["accumulation"]["active_handoffs_older"], 1)
            self.assertEqual(rep["accumulation"]["active_episodes_older"], 1)
            self.assertEqual(rep["accumulation"]["oversized_count"], 1)
            self.assertEqual(rep["accumulation"]["oversized_top"][0]["id"], "m_stale_big")
            self.assertEqual(rep["recall_noise"]["hook_runs_7d"], 6)
            self.assertEqual(rep["recall_noise"]["local_fallback_7d"], 3)
            self.assertEqual(rep["recall_noise"]["tracked_ids_7d"], 4)
            self.assertEqual(rep["recall_noise"]["opened_ids_7d"], 1)
            self.assertEqual(rep["recall_noise"]["opened_pct_7d"], 25.0)
            self.assertEqual(rep["recall_noise"]["bad_log_rows"]["hook.log"], 2)
            self.assertEqual(rep["recall_noise"]["bad_log_rows"]["usage.log"], 1)
            noisy_ids = [x["id"] for x in rep["recall_noise"]["never_opened"]]
            self.assertIn("m_stub_1", noisy_ids)
            self.assertNotIn("m_forge_1", noisy_ids)
            self.assertNotIn("m_sup_1", noisy_ids)

            md = audit.render_markdown(rep)
            for heading in (
                "### Bare-Tag / Empty Titles",
                "### Short Titles (<15 chars)",
                "### Exact Body Duplicate Groups",
                "### Exact Title Duplicate Groups",
                "### Self-Declared Obsolete Active Items",
                "### Stale `planned`/`assumed` Items",
                "### Same-Session Active Handoff + Task Overlaps",
                "### Orphan / Small Repos (<=5 active)",
                "### Oversized Active Items",
                "### Never-Opened Active High-Injection Items",
            ):
                self.assertIn(heading, md)
            self.assertIn("src=", md)


if __name__ == "__main__":
    unittest.main()

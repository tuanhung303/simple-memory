use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

use crate::config::{get_db_path, get_home, load_config};
use crate::core::dedup::{
    OVERLAP_WARN_SHARE, VETO_DF, idf, near_duplicate_score, normalize_title, word_overlap,
};
use crate::core::item::parse_as_of_epoch;
use crate::error::SmError;
use crate::py::float::{format_percent, round_ndigits};
use crate::py::path::PyPath;
use crate::py::str::{slice, split};
use crate::store::connect::connect;
use crate::store::items::get_item;
use crate::store::upkeep::{
    HandoffCounts, TagRow, TitleRow, WorkRow, active_rows_with_tags, active_title_rows,
    durable_overlap_candidates, handoff_claims, recent_active_work_items,
    supersede_session_handoffs,
};

pub const OVERLAP_WINDOW_SECONDS: f64 = 24.0 * 3600.0;
pub const DURABLE_OVERLAP_LIMIT: i64 = 10;
pub const MAX_OVERLAP_WARNINGS: usize = 5;
/// A token in more titles than this seeds no candidate pair (it still weighs in the cosine).
pub const PAIR_SEED_MAX_DF: usize = 300;
/// An active item with one of these tags older than STALE_DAYS is a stale plan or guess.
pub const STALE_TAGS: [&str; 2] = ["planned", "assumed"];
pub const STALE_DAYS: i64 = 30;

/// `run(home, cfg, dry_run=...)`: catch-up part `upkeep`, a session's newest handoff supersedes
/// its older active ones, and a handoff a later session claimed moves to that session's newest.
pub fn run(home: &PyPath, dry_run: bool) -> Result<HandoffCounts, SmError> {
    let conn = connect(home.join("memory.db").as_str(), None)?;
    let claims = handoff_claims(&conn)?;
    supersede_session_handoffs(&conn, dry_run, &claims)
}

/// `run_upkeep(dry_run=..., home=...)`.
pub fn run_upkeep(dry_run: bool, home: Option<&str>) -> Result<HandoffCounts, SmError> {
    let h = get_home(home)?;
    load_config(home)?;
    run(&h, dry_run)
}

#[derive(Clone, Debug, PartialEq)]
pub struct TitleGroup {
    pub repo: String,
    pub title: String,
    pub ids: Vec<String>,
    pub kinds: IndexMap<String, i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pair {
    pub score: f64,
    pub a: String,
    pub b: String,
    pub title_a: String,
    pub title_b: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub groups: Vec<TitleGroup>,
    pub pairs: Vec<Pair>,
    pub stale: Vec<TagRow>,
}

/// Same normalized title in one repo, biggest group first.
pub fn title_groups(rows: &[TitleRow]) -> Vec<TitleGroup> {
    let mut by_key: IndexMap<(String, String), Vec<&TitleRow>> = IndexMap::new();
    for r in rows {
        let norm = normalize_title(Some(&r.title));
        if !norm.is_empty() {
            by_key.entry((r.repo.clone(), norm)).or_default().push(r);
        }
    }
    let mut groups: Vec<TitleGroup> = by_key
        .into_iter()
        .filter(|(_, m)| m.len() >= 2)
        .map(|((repo, title), members)| {
            let mut kinds: IndexMap<String, i64> = IndexMap::new();
            for r in &members {
                *kinds.entry(r.kind.clone()).or_default() += 1;
            }
            TitleGroup {
                repo,
                title,
                ids: members.iter().map(|r| r.id.clone()).collect(),
                kinds,
            }
        })
        .collect();
    groups.sort_by(|x, y| (y.ids.len(), &x.repo, &x.title).cmp(&(x.ids.len(), &y.repo, &y.title)));
    groups
}

/// Title pairs in one repo whose tf-idf cosine passes, best first.
pub fn near_duplicate_pairs(rows: &[TitleRow]) -> Result<Vec<Pair>, SmError> {
    let tokens: HashMap<&str, Vec<String>> = rows
        .iter()
        .map(|r| {
            (
                r.id.as_str(),
                split(&normalize_title(Some(&r.title)))
                    .into_iter()
                    .map(String::from)
                    .collect(),
            )
        })
        .collect();
    let mut df: HashMap<String, i64> = HashMap::new();
    for toks in tokens.values() {
        for t in toks.iter().collect::<HashSet<_>>() {
            *df.entry(t.clone()).or_default() += 1;
        }
    }
    let n = rows.len() as i64;
    let mut idf_cache: HashMap<String, f64> = HashMap::new();
    let mut idf_of = |t: &str| -> Result<f64, SmError> {
        if let Some(v) = idf_cache.get(t) {
            return Ok(*v);
        }
        let v = idf(df.get(t).copied().unwrap_or(0), n)?;
        idf_cache.insert(t.to_string(), v);
        Ok(v)
    };
    let theta = idf(VETO_DF + 1, n)?;
    let mut by_repo: IndexMap<&str, Vec<&TitleRow>> = IndexMap::new();
    for r in rows {
        by_repo.entry(r.repo.as_str()).or_default().push(r);
    }
    let mut pairs = Vec::new();
    for members in by_repo.values() {
        let mut postings: HashMap<&str, Vec<&str>> = HashMap::new();
        for r in members {
            for t in tokens[r.id.as_str()].iter().collect::<HashSet<_>>() {
                postings.entry(t.as_str()).or_default().push(r.id.as_str());
            }
        }
        let mut seeded: HashSet<(&str, &str)> = HashSet::new();
        for ids in postings.values() {
            if ids.len() <= PAIR_SEED_MAX_DF {
                let mut sorted = ids.clone();
                sorted.sort();
                for i in 0..sorted.len() {
                    for j in i + 1..sorted.len() {
                        seeded.insert((sorted[i], sorted[j]));
                    }
                }
            }
        }
        let title_of: HashMap<&str, &str> = members
            .iter()
            .map(|r| (r.id.as_str(), r.title.as_str()))
            .collect();
        let mut seeded: Vec<(&str, &str)> = seeded.into_iter().collect();
        seeded.sort();
        for (a, b) in seeded {
            if tokens[a] == tokens[b] {
                continue;
            }
            let ta: Vec<&str> = tokens[a].iter().map(String::as_str).collect();
            let tb: Vec<&str> = tokens[b].iter().map(String::as_str).collect();
            if let Some(score) = near_duplicate_score(&ta, &tb, &mut idf_of, theta)? {
                pairs.push(Pair {
                    score: round_ndigits(score, 3)?,
                    a: a.into(),
                    b: b.into(),
                    title_a: title_of[a].into(),
                    title_b: title_of[b].into(),
                });
            }
        }
    }
    pairs.sort_by(|x, y| {
        y.score
            .partial_cmp(&x.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| (&x.a, &x.b).cmp(&(&y.a, &y.b)))
    });
    Ok(pairs)
}

/// Rows with as_of more than STALE_DAYS before now, by repo, newest first,
/// then id.
pub fn stale(rows: &[TagRow], now: f64) -> Result<Vec<TagRow>, SmError> {
    let cutoff = now - (STALE_DAYS * 24 * 3600) as f64;
    let mut old = Vec::new();
    for r in rows {
        let t = parse_as_of_epoch(Some(&r.as_of))?;
        if t < cutoff {
            old.push((t, r));
        }
    }
    old.sort_by(|(ta, a), (tb, b)| {
        a.repo
            .cmp(&b.repo)
            .then_with(|| tb.partial_cmp(ta).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(old.into_iter().map(|(_, r)| r.clone()).collect())
}

/// `run_stale(home=..., now=...)`: active planned or assumed items older than STALE_DAYS.
pub fn run_stale(home: Option<&str>, now: Option<f64>) -> Result<Vec<TagRow>, SmError> {
    let tagged = {
        let conn = connect(get_db_path(home)?.as_str(), None)?;
        active_rows_with_tags(&conn, &STALE_TAGS)?
    };
    stale(&tagged, now.unwrap_or_else(crate::clock::now_epoch))
}

/// `run_report(home=..., now=...)`: read-only same-title groups, near-duplicate pairs, stale items.
pub fn run_report(home: Option<&str>, now: Option<f64>) -> Result<Report, SmError> {
    let rows = {
        let conn = connect(get_db_path(home)?.as_str(), None)?;
        active_title_rows(&conn)?
    };
    Ok(Report {
        groups: title_groups(&rows),
        pairs: near_duplicate_pairs(&rows)?,
        stale: run_stale(home, now)?,
    })
}

/// `put_overlaps(item_id, home=..., now=...)`: warning lines for the repo's recent active task and
/// handoff items (24h) and active durable items (`fact`, `decision`, `learning`) that hold at
/// least half of the new item's distinct words, sorted best/newest first and capped at
/// `MAX_OVERLAP_WARNINGS`.
pub fn put_overlaps(
    item_id: &str,
    home: Option<&str>,
    now: Option<f64>,
) -> Result<Vec<String>, SmError> {
    let since = now.unwrap_or_else(crate::clock::now_epoch) - OVERLAP_WINDOW_SECONDS;
    let (item, others) = {
        let conn = connect(get_db_path(home)?.as_str(), None)?;
        let item = get_item(&conn, item_id)?
            .ok_or_else(|| SmError::key(&format!("Item not found: {item_id}")))?;
        let mut seen: HashSet<String> = HashSet::new();
        let mut others: Vec<WorkRow> = Vec::new();
        for o in recent_active_work_items(&conn, &item.repo, since, &item.id)? {
            if seen.insert(o.id.clone()) {
                others.push(o);
            }
        }
        for o in durable_overlap_candidates(&conn, &item, DURABLE_OVERLAP_LIMIT)? {
            if seen.insert(o.id.clone()) {
                others.push(o);
            }
        }
        (item, others)
    };
    let new_text = format!("{}\n{}", item.title, item.body);
    let mut hits: Vec<(f64, f64, WorkRow)> = Vec::new();
    for o in others {
        let share = word_overlap(&new_text, &format!("{}\n{}", o.title, o.body));
        if share >= OVERLAP_WARN_SHARE {
            let epoch = parse_as_of_epoch(Some(&o.as_of))?;
            hits.push((share, epoch, o));
        }
    }
    hits.sort_by(|(sa, ea, a), (sb, eb, b)| {
        sb.total_cmp(sa)
            .then_with(|| eb.total_cmp(ea))
            .then_with(|| a.id.cmp(&b.id))
    });
    hits.truncate(MAX_OVERLAP_WARNINGS);
    let mut lines = Vec::with_capacity(hits.len());
    for (share, _, o) in hits {
        let title = split(&o.title).join(" ");
        lines.push(format!(
            "sm put: {} shares {} of its words with {} {} ({}): {}; if it replaces that item, run sm supersede {} {}",
            item.id,
            format_percent(share, 0),
            o.kind,
            o.id,
            slice(&o.as_of, None, Some(16)),
            slice(&title, None, Some(80)),
            o.id,
            item.id
        ));
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    // Each home in a tempdir.
    use super::*;
    use crate::core::item::{Item, epoch_to_as_of};
    use crate::store::items::put_item;
    use crate::store::upkeep::HandoffCounts;

    // made-up session ids in the uuid shape handoff file names carry (the Python test's own)
    const S1: &str = "11111111-aaaa-4bbb-8ccc-000000000001";
    const S2: &str = "22222222-aaaa-4bbb-8ccc-000000000002";
    const S3: &str = "33333333-aaaa-4bbb-8ccc-000000000003";

    fn db(home: &str) -> rusqlite::Connection {
        connect(get_db_path(Some(home)).unwrap().as_str(), None).unwrap()
    }

    fn put(home: &str, it: Item) {
        put_item(&db(home), &it.post_init(), None).unwrap();
    }

    fn handoff(
        home: &str,
        id: &str,
        stamp: &str,
        session: &str,
        repo: &str,
        status: &str,
        sup: Option<&str>,
    ) {
        put(
            home,
            Item {
                source: format!("handoff:{stamp}-{session}.md"),
                as_of: "2026-11-01T10:00:00+00:00".into(),
                status: status.into(),
                superseded_by: sup.map(String::from),
                ..Item::bare(
                    id,
                    repo,
                    "handoff",
                    &format!("Handoff {id}"),
                    &format!("body of {id}"),
                    "derived",
                )
            },
        );
    }

    fn status(home: &str, id: &str) -> (String, Option<String>) {
        let it = get_item(&db(home), id).unwrap().unwrap();
        (it.status, it.superseded_by)
    }

    fn counts(sessions: i64, superseded: i64, handoffs_without_session: i64) -> HandoffCounts {
        HandoffCounts {
            sessions,
            superseded,
            handoffs_without_session,
        }
    }

    fn tmp() -> (tempfile::TempDir, String) {
        let d = tempfile::tempdir().unwrap();
        let h = d.path().to_str().unwrap().to_string();
        (d, h)
    }

    fn epoch(s: &str) -> f64 {
        parse_as_of_epoch(Some(s)).unwrap()
    }

    #[test]
    fn upkeep_newest_handoff_supersedes_older_in_its_session() {
        let (_d, h) = tmp();
        // as_of runs against the file order on purpose: the file name's date and time decide
        handoff(&h, "m_h1", "20261101-080000", S1, "a", "active", None);
        handoff(&h, "m_h3", "20261101-120000", S1, "a", "active", None);
        handoff(&h, "m_h2", "20261101-100000", S1, "a", "active", None);
        handoff(
            &h,
            "m_other_session",
            "20261101-090000",
            S2,
            "a",
            "active",
            None,
        );
        handoff(
            &h,
            "m_other_repo",
            "20261101-070000",
            S1,
            "b",
            "active",
            None,
        );
        handoff(&h, "m_b_newest", "20261101-130000", S1, "b", "active", None);
        let home = PyPath::new(&h);
        assert_eq!(run(&home, true).unwrap(), counts(2, 3, 0));
        assert_eq!(status(&h, "m_h1"), ("active".into(), None));
        assert_eq!(run(&home, false).unwrap(), counts(2, 3, 0));
        assert_eq!(
            status(&h, "m_h1"),
            ("superseded".into(), Some("m_h3".into()))
        );
        assert_eq!(
            status(&h, "m_h2"),
            ("superseded".into(), Some("m_h3".into()))
        );
        assert_eq!(status(&h, "m_h3"), ("active".into(), None));
        assert_eq!(status(&h, "m_other_session"), ("active".into(), None));
        assert_eq!(
            status(&h, "m_other_repo"),
            ("superseded".into(), Some("m_b_newest".into()))
        );
        assert_eq!(run(&home, false).unwrap(), counts(0, 0, 0));
    }

    #[test]
    fn upkeep_follows_the_newest_handoffs_chain_or_falls_back() {
        let (_d, h) = tmp();
        put(
            &h,
            Item::bare("m_decision", "a", "decision", "D", "d", "decided"),
        );
        let sup = "superseded";
        handoff(&h, "m_s1_old1", "20261101-080000", S1, "a", "active", None);
        handoff(&h, "m_s1_old2", "20261101-090000", S1, "a", "active", None);
        handoff(
            &h,
            "m_s1_new",
            "20261101-100000",
            S1,
            "a",
            sup,
            Some("m_decision"),
        );
        handoff(&h, "m_s2_old", "20261101-080000", S2, "a", "active", None);
        handoff(&h, "m_s2_mid", "20261101-090000", S2, "a", "active", None);
        handoff(&h, "m_s2_retired", "20261101-100000", S2, "a", sup, None);
        handoff(&h, "m_s3_old", "20261101-080000", S3, "a", "active", None);
        handoff(
            &h,
            "m_s3_new",
            "20261101-090000",
            S3,
            "a",
            sup,
            Some("m_decision"),
        );
        handoff(
            &h,
            "m_s4_retired",
            "20261101-080000",
            "44444444-aaaa-4bbb-8ccc-000000000004",
            "a",
            sup,
            None,
        );
        put(
            &h,
            Item {
                source: "handoff:notes.md".into(),
                ..Item::bare("m_loose", "a", "handoff", "L", "l", "derived")
            },
        );
        assert_eq!(run(&PyPath::new(&h), false).unwrap(), counts(3, 4, 1));
        assert_eq!(
            status(&h, "m_s3_old"),
            (sup.into(), Some("m_decision".into()))
        );
        assert_eq!(
            status(&h, "m_s1_old1"),
            (sup.into(), Some("m_decision".into()))
        );
        assert_eq!(
            status(&h, "m_s1_old2"),
            (sup.into(), Some("m_decision".into()))
        );
        assert_eq!(
            status(&h, "m_s2_old"),
            (sup.into(), Some("m_s2_mid".into()))
        );
        assert_eq!(status(&h, "m_s2_mid"), ("active".into(), None));
        assert_eq!(status(&h, "m_loose"), ("active".into(), None));
    }

    /// A handoff item whose evidence is a file `<h>/handoffs/<stamp>-<session>.md`; `claim` is the
    /// text of its `.claimed` sibling (claiming session id and terminal handle on separate lines).
    fn filed_handoff(
        h: &str,
        id: &str,
        stamp: &str,
        session: &str,
        repo: &str,
        claim: Option<&str>,
    ) -> std::path::PathBuf {
        let dir = std::path::Path::new(h).join("handoffs");
        std::fs::create_dir_all(&dir).unwrap();
        let name = format!("{stamp}-{session}.md");
        let file = dir.join(&name);
        std::fs::write(&file, format!("# Handoff {id}\n")).unwrap();
        if let Some(c) = claim {
            std::fs::write(dir.join(format!("{name}.claimed")), c).unwrap();
        }
        put(
            h,
            Item {
                source: format!("handoff:{name}"),
                evidence: file.to_str().unwrap().into(),
                as_of: "2026-11-01T10:00:00+00:00".into(),
                ..Item::bare(
                    id,
                    repo,
                    "handoff",
                    &format!("Handoff {id}"),
                    &format!("body of {id}"),
                    "derived",
                )
            },
        );
        file
    }

    fn claim_of(session: &str) -> String {
        format!("{session}\nterm-1\n")
    }

    #[test]
    fn a_claimed_handoff_is_superseded_by_the_claiming_sessions_newer_handoff() {
        let (_d, h) = tmp();
        filed_handoff(&h, "m_p", "20261101-080000", S1, "a", Some(&claim_of(S2)));
        filed_handoff(&h, "m_y", "20261101-090000", S2, "a", None);
        let home = PyPath::new(&h);
        assert_eq!(run(&home, true).unwrap(), counts(0, 1, 0));
        assert_eq!(status(&h, "m_p"), ("active".into(), None));
        assert_eq!(run(&home, false).unwrap(), counts(0, 1, 0));
        assert_eq!(status(&h, "m_p"), ("superseded".into(), Some("m_y".into())));
        assert_eq!(status(&h, "m_y"), ("active".into(), None));
        assert_eq!(run(&home, false).unwrap(), counts(0, 0, 0));
    }

    #[test]
    fn a_claim_links_nothing_without_a_newer_active_handoff_of_its_session_in_the_repo() {
        let (_d, h) = tmp();
        let s4 = "44444444-aaaa-4bbb-8ccc-000000000004";
        let s5 = "55555555-aaaa-4bbb-8ccc-000000000005";
        let s6 = "66666666-aaaa-4bbb-8ccc-000000000006";
        let s7 = "77777777-aaaa-4bbb-8ccc-000000000007";
        let s8 = "88888888-aaaa-4bbb-8ccc-000000000008";
        let s9 = "99999999-aaaa-4bbb-8ccc-000000000009";
        // the claimer S2 writes one newer handoff in repo a, S3 one in repo b
        filed_handoff(&h, "m_y", "20261101-120000", S2, "a", None);
        filed_handoff(&h, "m_y_b", "20261101-120000", S3, "b", None);
        // no .claimed file
        filed_handoff(&h, "m_unclaimed", "20261101-080000", S1, "a", None);
        // line 1 is no session id (an upper-case uuid is none either)
        filed_handoff(
            &h,
            "m_not_uuid",
            "20261101-080000",
            s4,
            "a",
            Some("not-a-session\n"),
        );
        filed_handoff(
            &h,
            "m_upper",
            "20261101-080000",
            s5,
            "a",
            Some(&claim_of(&S2.to_uppercase())),
        );
        // the claimer wrote no handoff
        filed_handoff(
            &h,
            "m_no_handoff",
            "20261101-080000",
            s6,
            "a",
            Some(&claim_of("00000000-aaaa-4bbb-8ccc-000000000000")),
        );
        // the claimer's handoff is in another repo
        filed_handoff(
            &h,
            "m_other_repo",
            "20261101-080000",
            s7,
            "a",
            Some(&claim_of(S3)),
        );
        // the claimer's head is as old as the claimed handoff
        filed_handoff(
            &h,
            "m_same_time",
            "20261101-120000",
            s8,
            "a",
            Some(&claim_of(S2)),
        );
        // the handoff file is gone; its .claimed stays
        let gone = filed_handoff(
            &h,
            "m_no_file",
            "20261101-080000",
            s9,
            "a",
            Some(&claim_of(S2)),
        );
        std::fs::remove_file(gone).unwrap();
        // a handoff file name with no date and time: nothing to compare the head with
        let loose = std::path::Path::new(&h).join("handoffs").join("notes.md");
        std::fs::write(&loose, "# Notes\n").unwrap();
        std::fs::write(loose.with_file_name("notes.md.claimed"), claim_of(S2)).unwrap();
        put(
            &h,
            Item {
                source: "handoff:notes.md".into(),
                evidence: loose.to_str().unwrap().into(),
                ..Item::bare("m_no_stamp", "a", "handoff", "N", "n", "derived")
            },
        );
        let home = PyPath::new(&h);
        assert_eq!(run(&home, true).unwrap(), counts(0, 0, 1));
        assert_eq!(run(&home, false).unwrap(), counts(0, 0, 1));
        for id in [
            "m_y",
            "m_y_b",
            "m_unclaimed",
            "m_not_uuid",
            "m_upper",
            "m_no_handoff",
            "m_other_repo",
            "m_same_time",
            "m_no_file",
            "m_no_stamp",
        ] {
            assert_eq!(status(&h, id), ("active".into(), None), "{id}");
        }
    }

    #[test]
    fn a_claim_chain_retires_each_link_to_the_next_in_any_input_order() {
        // A claimed by B's session, B claimed by C's session: A -> B -> C, whatever the row order
        for order in [
            ["m_a", "m_b", "m_c"],
            ["m_c", "m_b", "m_a"],
            ["m_b", "m_c", "m_a"],
        ] {
            let (_d, h) = tmp();
            for id in order {
                match id {
                    "m_a" => filed_handoff(&h, id, "20261101-080000", S1, "a", Some(&claim_of(S2))),
                    "m_b" => filed_handoff(&h, id, "20261101-090000", S2, "a", Some(&claim_of(S3))),
                    _ => filed_handoff(&h, id, "20261101-100000", S3, "a", None),
                };
            }
            let home = PyPath::new(&h);
            assert_eq!(run(&home, true).unwrap(), counts(0, 2, 0), "{order:?}");
            assert_eq!(run(&home, false).unwrap(), counts(0, 2, 0), "{order:?}");
            assert_eq!(
                status(&h, "m_a"),
                ("superseded".into(), Some("m_b".into())),
                "{order:?}"
            );
            assert_eq!(
                status(&h, "m_b"),
                ("superseded".into(), Some("m_c".into())),
                "{order:?}"
            );
            assert_eq!(
                crate::store::upkeep::chain_head(&db(&h), "m_a").unwrap(),
                Some("m_c".into()),
                "{order:?}"
            );
        }
    }

    #[test]
    fn the_session_pass_and_the_claim_pass_count_each_handoff_once() {
        let (_d, h) = tmp();
        // S2 has two active handoffs; m_p (S1) is claimed by S2
        filed_handoff(&h, "m_p", "20261101-080000", S1, "a", Some(&claim_of(S2)));
        filed_handoff(&h, "m_y1", "20261101-090000", S2, "a", None);
        filed_handoff(&h, "m_y2", "20261101-100000", S2, "a", None);
        // S3's older handoff is claimed by S2 too, but its own session retires it first
        filed_handoff(&h, "m_q1", "20261101-083000", S3, "b", Some(&claim_of(S2)));
        filed_handoff(&h, "m_q2", "20261101-084000", S3, "b", None);
        filed_handoff(&h, "m_yb", "20261101-110000", S2, "b", None);
        let home = PyPath::new(&h);
        assert_eq!(run(&home, true).unwrap(), counts(2, 3, 0));
        assert_eq!(run(&home, false).unwrap(), counts(2, 3, 0));
        assert_eq!(
            status(&h, "m_p"),
            ("superseded".into(), Some("m_y2".into()))
        );
        assert_eq!(
            status(&h, "m_y1"),
            ("superseded".into(), Some("m_y2".into()))
        );
        assert_eq!(
            status(&h, "m_q1"),
            ("superseded".into(), Some("m_q2".into()))
        );
        assert_eq!(status(&h, "m_q2"), ("active".into(), None));
        assert_eq!(run(&home, false).unwrap(), counts(0, 0, 0));
    }

    #[test]
    fn a_claim_whose_target_chains_back_to_the_claimed_handoff_links_nothing() {
        // hand-made supersedes: S2's newest points at S3's m_h, and S3's newest points at m_p,
        // so S3's session pass moves m_h to m_p and S2's head m_h ends at m_p itself
        let (_d, h) = tmp();
        filed_handoff(&h, "m_p", "20261101-080000", S1, "a", Some(&claim_of(S2)));
        filed_handoff(&h, "m_n1", "20261101-085000", S2, "a", None);
        handoff(
            &h,
            "m_n2",
            "20261101-090000",
            S2,
            "a",
            "superseded",
            Some("m_h"),
        );
        handoff(&h, "m_h", "20261101-093000", S3, "a", "active", None);
        handoff(
            &h,
            "m_h2",
            "20261101-100000",
            S3,
            "a",
            "superseded",
            Some("m_p"),
        );
        let home = PyPath::new(&h);
        assert_eq!(run(&home, true).unwrap(), counts(2, 2, 0));
        assert_eq!(run(&home, false).unwrap(), counts(2, 2, 0));
        assert_eq!(status(&h, "m_p"), ("active".into(), None));
        assert_eq!(status(&h, "m_h"), ("superseded".into(), Some("m_p".into())));
    }

    #[test]
    fn a_claim_read_before_the_transaction_skips_a_handoff_retired_since() {
        // the claims come from files read before the write transaction; another writer may
        // retire the handoff in between, and the transaction's own status read wins
        let (_d, h) = tmp();
        handoff(
            &h,
            "m_p",
            "20261101-080000",
            S1,
            "a",
            "superseded",
            Some("m_x"),
        );
        handoff(&h, "m_x", "20261101-083000", S3, "a", "active", None);
        handoff(&h, "m_y", "20261101-090000", S2, "a", "active", None);
        let claims = HashMap::from([("m_p".to_string(), S2.to_string())]);
        let conn = db(&h);
        assert_eq!(
            crate::store::upkeep::supersede_session_handoffs(&conn, false, &claims).unwrap(),
            counts(0, 0, 0)
        );
        assert_eq!(status(&h, "m_p"), ("superseded".into(), Some("m_x".into())));
    }

    #[test]
    fn a_claim_links_nothing_when_the_claimers_head_is_no_handoff() {
        let (_d, h) = tmp();
        put(
            &h,
            Item::bare("m_decision", "a", "decision", "D", "d", "decided"),
        );
        filed_handoff(&h, "m_p", "20261101-080000", S1, "a", Some(&claim_of(S2)));
        filed_handoff(&h, "m_y1", "20261101-090000", S2, "a", None);
        handoff(
            &h,
            "m_y2",
            "20261101-100000",
            S2,
            "a",
            "superseded",
            Some("m_decision"),
        );
        let home = PyPath::new(&h);
        assert_eq!(run(&home, true).unwrap(), counts(1, 1, 0));
        assert_eq!(run(&home, false).unwrap(), counts(1, 1, 0));
        assert_eq!(status(&h, "m_p"), ("active".into(), None));
        assert_eq!(
            status(&h, "m_y1"),
            ("superseded".into(), Some("m_decision".into()))
        );
    }

    #[test]
    fn a_claim_links_nothing_when_the_target_is_active_but_points_on() {
        // an active handoff with a superseded_by pointer (a hand edit) cannot take a supersede
        let (_d, h) = tmp();
        put(
            &h,
            Item::bare("m_decision", "a", "decision", "D", "d", "decided"),
        );
        filed_handoff(&h, "m_p", "20261101-080000", S1, "a", Some(&claim_of(S2)));
        handoff(
            &h,
            "m_y",
            "20261101-090000",
            S2,
            "a",
            "active",
            Some("m_decision"),
        );
        let home = PyPath::new(&h);
        assert_eq!(run(&home, true).unwrap(), counts(0, 0, 0));
        assert_eq!(run(&home, false).unwrap(), counts(0, 0, 0));
        assert_eq!(status(&h, "m_p"), ("active".into(), None));
    }

    #[test]
    fn dry_run_sees_the_head_the_session_pass_moves_on() {
        // S2's newest handoff was superseded by hand by S3's older one; S3's session pass moves
        // that one to S3's newest first, so S2's head, and the head m_p's claim reaches, is
        // m_y_new (05:00), newer than m_p (02:00), in the dry run as in the applied run
        let (_d, h) = tmp();
        filed_handoff(&h, "m_p", "20261101-020000", S1, "a", Some(&claim_of(S2)));
        handoff(&h, "m_y_new", "20261101-050000", S3, "a", "active", None);
        handoff(&h, "m_y_old", "20261101-020000", S3, "a", "active", None);
        handoff(
            &h,
            "m_s2_top",
            "20261101-010000",
            S2,
            "a",
            "superseded",
            Some("m_y_old"),
        );
        handoff(&h, "m_s2_low", "20261101-000000", S2, "a", "active", None);
        let home = PyPath::new(&h);
        assert_eq!(run(&home, true).unwrap(), counts(2, 3, 0));
        assert_eq!(run(&home, false).unwrap(), counts(2, 3, 0));
        assert_eq!(
            status(&h, "m_p"),
            ("superseded".into(), Some("m_y_new".into()))
        );
        assert_eq!(
            status(&h, "m_s2_low"),
            ("superseded".into(), Some("m_y_new".into()))
        );
    }

    #[test]
    fn a_claim_follows_a_target_an_earlier_claim_retired() {
        // hand supersedes across sessions: S2's newest -> S3's m_x, S3's newest -> S4's m_w.
        // The session pass moves m_y_old -> m_x and m_x -> m_w; the claim pass first moves
        // m_w (claimed by S5) -> m_q, then m_p (claimed by S2, head m_x) follows m_x -> m_w -> m_q
        let (_d, h) = tmp();
        let s4 = "44444444-aaaa-4bbb-8ccc-000000000004";
        let s5 = "55555555-aaaa-4bbb-8ccc-000000000005";
        filed_handoff(&h, "m_p", "20261101-050000", S1, "a", Some(&claim_of(S2)));
        filed_handoff(&h, "m_y_old", "20261101-040000", S2, "a", None);
        handoff(
            &h,
            "m_y_new",
            "20261101-060000",
            S2,
            "a",
            "superseded",
            Some("m_x"),
        );
        filed_handoff(&h, "m_x", "20261101-070000", S3, "a", None);
        handoff(
            &h,
            "m_x_new",
            "20261101-080000",
            S3,
            "a",
            "superseded",
            Some("m_w"),
        );
        filed_handoff(&h, "m_w", "20261101-010000", s4, "a", Some(&claim_of(s5)));
        filed_handoff(&h, "m_q", "20261101-020000", s5, "a", None);
        let home = PyPath::new(&h);
        assert_eq!(run(&home, true).unwrap(), counts(2, 4, 0));
        assert_eq!(run(&home, false).unwrap(), counts(2, 4, 0));
        assert_eq!(status(&h, "m_w"), ("superseded".into(), Some("m_q".into())));
        assert_eq!(status(&h, "m_p"), ("superseded".into(), Some("m_q".into())));
    }

    #[test]
    fn a_claim_links_nothing_when_the_target_has_a_blank_pointer() {
        let (_d, h) = tmp();
        filed_handoff(&h, "m_p", "20261101-080000", S1, "a", Some(&claim_of(S2)));
        handoff(&h, "m_y", "20261101-090000", S2, "a", "active", Some(""));
        assert_eq!(run(&PyPath::new(&h), false).unwrap(), counts(0, 0, 0));
    }

    #[test]
    fn an_unreadable_claim_links_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, h) = tmp();
        let file = filed_handoff(&h, "m_p", "20261101-080000", S1, "a", Some(&claim_of(S2)));
        filed_handoff(&h, "m_y", "20261101-090000", S2, "a", None);
        let claim = file.with_file_name(format!(
            "{}.claimed",
            file.file_name().unwrap().to_str().unwrap()
        ));
        std::fs::set_permissions(&claim, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(run(&PyPath::new(&h), false).unwrap(), counts(0, 0, 0));
        assert_eq!(status(&h, "m_p"), ("active".into(), None));
    }

    #[test]
    fn report_groups_and_pairs() {
        let (_d, h) = tmp();
        let rows = [
            ("m_g1", "a", "Weekly sync: notes"),
            ("m_g2", "a", "weekly SYNC notes"),
            ("m_g3", "b", "Weekly sync notes"),
            (
                "m_p1",
                "a",
                "elisa prod deploy green cutover planned saturday",
            ),
            ("m_p2", "a", "elisa prod deploy green cutover planned"),
            ("m_x1", "a", "!!!"),
            ("m_x2", "a", "!!!"),
        ];
        for (i, (id, repo, title)) in rows.iter().enumerate() {
            put(
                &h,
                Item::bare(id, repo, "fact", title, &format!("b{i}"), "assumed"),
            );
        }
        for k in 0..40 {
            let title = format!("elisa prod deploy green cutover planned saturday filler{k}");
            put(
                &h,
                Item::bare(
                    &format!("m_f{k:02}"),
                    "c",
                    "fact",
                    &title,
                    &format!("f{k}"),
                    "assumed",
                ),
            );
        }
        let rep = run_report(Some(&h), None).unwrap();
        let groups: Vec<_> = rep
            .groups
            .iter()
            .map(|g| (g.repo.as_str(), g.title.as_str(), g.ids.clone()))
            .collect();
        assert_eq!(
            groups,
            vec![(
                "a",
                "weekly sync notes",
                vec!["m_g1".to_string(), "m_g2".into()]
            )]
        );
        let pairs: Vec<_> = rep
            .pairs
            .iter()
            .map(|p| (p.a.as_str(), p.b.as_str()))
            .collect();
        assert_eq!(pairs, vec![("m_p1", "m_p2")]);
    }

    #[test]
    fn put_overlap_warns_on_recent_active_work_items_of_the_repo() {
        let (_d, h) = tmp();
        let now = epoch("2026-11-02T12:00:00+00:00");
        let hour_ago = epoch_to_as_of(now - 3600.0).unwrap();
        let day_and_minute_ago = epoch_to_as_of(now - 24.0 * 3600.0 - 60.0).unwrap();
        for (id, repo, kind, as_of, text) in [
            (
                "m_task_recent",
                "a",
                "task",
                &hour_ago,
                "alpha beta gamma delta",
            ),
            (
                "m_task_old",
                "a",
                "task",
                &day_and_minute_ago,
                "alpha beta gamma delta epsilon",
            ),
            (
                "m_task_other_repo",
                "b",
                "task",
                &hour_ago,
                "alpha beta gamma delta epsilon",
            ),
            (
                "m_fact_recent",
                "a",
                "fact",
                &hour_ago,
                "alpha beta gamma delta epsilon fact",
            ),
            (
                "m_task_done",
                "a",
                "task",
                &hour_ago,
                "alpha beta gamma delta epsilon done",
            ),
            ("m_handoff_low", "a", "handoff", &hour_ago, "alpha zeta"),
            (
                "m_new",
                "a",
                "fact",
                &epoch_to_as_of(now).unwrap(),
                "alpha beta gamma delta epsilon theta",
            ),
        ] {
            put(
                &h,
                Item {
                    as_of: as_of.clone(),
                    ..Item::bare(id, repo, kind, text, text, "assumed")
                },
            );
        }
        db(&h)
            .execute(
                "UPDATE items SET status = 'superseded' WHERE id = 'm_task_done'",
                [],
            )
            .unwrap();
        assert_eq!(
            put_overlaps("m_new", Some(&h), Some(now)).unwrap(),
            vec![
                format!(
                    "sm put: m_new shares 83% of its words with fact m_fact_recent ({}): alpha beta gamma delta epsilon fact; \
                     if it replaces that item, run sm supersede m_fact_recent m_new",
                    &hour_ago[..16]
                ),
                format!(
                    "sm put: m_new shares 67% of its words with task m_task_recent ({}): alpha beta gamma delta; \
                     if it replaces that item, run sm supersede m_task_recent m_new",
                    &hour_ago[..16]
                ),
            ]
        );
    }

    #[test]
    fn put_overlap_window_and_share_boundaries() {
        let (_d, h) = tmp();
        let now = epoch("2026-11-02T12:00:00+00:00");
        put(
            &h,
            Item {
                as_of: "2026-11-01T12:00:00+00:00".into(),
                ..Item::bare("m_edge_time", "a", "handoff", "t", "one two", "assumed")
            },
        );
        put(
            &h,
            Item {
                as_of: "2026-11-02T12:00:00+00:00".into(),
                ..Item::bare(
                    "m_new",
                    "a",
                    "task",
                    "one two three four",
                    "one two three four",
                    "assumed",
                )
            },
        );
        // exactly 24 hours old and exactly half of the words: both on the warning side of the line
        let lines = put_overlaps("m_new", Some(&h), Some(now)).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0]
                .contains("shares 50% of its words with handoff m_edge_time (2026-11-01T12:00)"),
            "{}",
            lines[0]
        );
        assert_eq!(
            put_overlaps("m_new", Some(&h), Some(now + 1.0)).unwrap(),
            Vec::<String>::new()
        );
        let e = put_overlaps("m_missing", Some(&h), Some(now)).unwrap_err();
        assert_eq!(e.python_text(), "KeyError: 'Item not found: m_missing'");
    }

    #[test]
    fn report_lists_stale_planned_and_assumed_by_repo_newest_first() {
        let (_d, h) = tmp();
        for (id, repo, tag, as_of) in [
            ("m_1", "a", "planned", "2026-09-01T09:00:00+00:00"),
            ("m_2", "a", "assumed", "2026-10-01T09:00:00+00:00"),
            ("m_edge", "a", "assumed", "2026-10-05T12:00:00+00:00"),
            ("m_3_utc", "b", "planned", "2026-09-15T00:00:00+00:00"),
            (
                "m_4_edge_minus",
                "b",
                "assumed",
                "2026-10-05T11:59:59+00:00",
            ),
            ("m_decided", "a", "decided", "2026-08-01T09:00:00+00:00"),
            ("m_gone", "a", "planned", "2026-08-01T09:00:00+00:00"),
            ("m_fresh", "a", "planned", "2026-11-01T09:00:00+00:00"),
        ] {
            put(
                &h,
                Item {
                    as_of: as_of.into(),
                    ..Item::bare(id, repo, "task", &format!("T {id}"), id, tag)
                },
            );
        }
        db(&h)
            .execute(
                "UPDATE items SET status = 'superseded' WHERE id = 'm_gone'",
                [],
            )
            .unwrap();
        let stale = run_report(Some(&h), Some(epoch("2026-11-04T12:00:00+00:00")))
            .unwrap()
            .stale;
        let got: Vec<_> = stale
            .iter()
            .map(|s| (s.repo.as_str(), s.id.as_str(), s.tag.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("a", "m_2", "assumed"),
                ("a", "m_1", "planned"),
                ("b", "m_4_edge_minus", "assumed"),
                ("b", "m_3_utc", "planned")
            ]
        );
        assert!(
            run_report(Some(&h), Some(epoch("2026-09-01T00:00:00+00:00")))
                .unwrap()
                .stale
                .is_empty()
        );
    }
}

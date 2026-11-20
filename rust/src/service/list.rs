use serde_json::{Value, json};

use crate::config::{
    aliases, get_db_path, load_config, resolve_repo_until, roots, scope_matchers, scopes,
};
use crate::core::item::{Item, validate_date_range};
use crate::core::rank::match_snippet;
use crate::core::tags::{retired_label, validate_kind, validate_tag};
use crate::error::SmError;
use crate::py;
use crate::service::query::{
    JSON_SNIPPET_CHARS, MAX_OTHER_REPOS, StrList, item_text, item_tokens, split_list,
};
use crate::store::connect::connect;
use crate::store::search::{Filters, StatusFilter, list_items, list_other_repos};

pub const DEFAULT_LIST_LIMIT: i64 = 20;

/// Arguments for `run_list` (`sm list`).
#[derive(Clone, Copy, Debug)]
pub struct ListArgs<'a> {
    pub limit: i64,
    pub offset: i64,
    pub repo: &'a str,
    pub all_repos: bool,
    pub scopes: StrList<'a>,
    pub kinds: StrList<'a>,
    pub tags: StrList<'a>,
    pub sources: StrList<'a>,
    pub since: &'a str,
    pub until: &'a str,
    pub status: StatusFilter,
    pub home: Option<&'a str>,
    pub cwd: Option<&'a str>,
}

impl Default for ListArgs<'_> {
    fn default() -> Self {
        ListArgs {
            limit: DEFAULT_LIST_LIMIT,
            offset: 0,
            repo: "",
            all_repos: false,
            scopes: StrList::None,
            kinds: StrList::None,
            tags: StrList::None,
            sources: StrList::None,
            since: "",
            until: "",
            status: StatusFilter::Active,
            home: None,
            cwd: None,
        }
    }
}

/// One page returned by `run_list` for the `sm list` formatters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListPage {
    pub items: Vec<Item>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
    pub status: StatusFilter,
    pub repo: Option<String>,
    pub all_repos: bool,
    pub scopes: Vec<String>,
    pub other_repos: Vec<(String, i64)>,
}

/// List items matching `a`, ordered newest `sm_epoch(as_of)` first (`NULL` last), ties by `id DESC`.
pub fn run_list(a: &ListArgs) -> Result<ListPage, SmError> {
    if a.limit <= 0 {
        return Err(SmError::value("--limit must be at least 1"));
    }
    if a.offset < 0 {
        return Err(SmError::value("--offset must be at least 0"));
    }
    let tag_list = split_list(a.tags)
        .iter()
        .map(|t| validate_tag(Some(t), Some("list-filter"), true))
        .collect::<Result<Vec<_>, _>>()?;
    let kind_list = split_list(a.kinds)
        .iter()
        .map(|k| validate_kind(Some(k)))
        .collect::<Result<Vec<_>, _>>()?;
    let source_list = split_list(a.sources);
    let (since_epoch, until_epoch, until_exclusive) =
        validate_date_range(Some(a.since), Some(a.until))?;

    let cfg = load_config(a.home)?;
    let in_paths = split_list(a.scopes);
    let scope = if in_paths.is_empty() {
        None
    } else {
        Some(scope_matchers(&scopes(&cfg)?, &in_paths)?)
    };
    let all_repos = a.all_repos || scope.is_some();
    let repo = py::str::strip(a.repo);
    let resolved = if all_repos {
        None
    } else if !repo.is_empty() {
        Some(repo.to_string())
    } else {
        Some(resolve_repo_until(
            a.cwd,
            &aliases(&cfg),
            &roots(&cfg)?,
            None,
        )?)
    };

    let filters = Filters {
        repo: resolved.clone(),
        include_all_repos: all_repos,
        exact_repo: !all_repos,
        tags_filter: (!tag_list.is_empty()).then_some(tag_list),
        kinds_filter: (!kind_list.is_empty()).then_some(kind_list),
        include_history: false,
        status_filter: Some(a.status),
        scope,
        extra_scope: None,
        sources_filter: (!source_list.is_empty()).then_some(source_list),
        since_epoch,
        until_epoch,
        until_exclusive,
    };

    let conn = connect(get_db_path(a.home)?.as_str(), None)?;
    let res = list_items(&conn, &filters, a.limit, a.offset)?;
    let other_repos = if res.total == 0
        && !all_repos
        && in_paths.is_empty()
        && let Some(cur_repo) = resolved.as_deref()
    {
        let other_filters = Filters {
            repo: None,
            include_all_repos: true,
            exact_repo: false,
            scope: None,
            extra_scope: None,
            ..filters
        };
        list_other_repos(&conn, &other_filters, cur_repo, MAX_OTHER_REPOS)?
    } else {
        Vec::new()
    };
    Ok(ListPage {
        items: res.items,
        total: res.total,
        limit: a.limit,
        offset: a.offset,
        status: a.status,
        repo: resolved,
        all_repos,
        scopes: in_paths,
        other_repos,
    })
}

/// Follow-up commands for a list page (`sm show <first 3 ids>`).
pub fn list_next_cmds(page: &ListPage) -> Vec<String> {
    let ids: Vec<&str> = page.items.iter().take(3).map(|it| it.id.as_str()).collect();
    if ids.is_empty() {
        return Vec::new();
    }
    vec![format!("sm show {}", ids.join(" "))]
}

/// The JSON object `sm list --format json` prints.
pub fn list_json(page: &ListPage) -> Value {
    let items: Vec<Value> = page
        .items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            let rank = u64::try_from(page.offset as i128 + i as i128 + 1).unwrap_or(u64::MAX);
            let text = item_text(it);
            let snippet = match_snippet(&text, &[], &[], JSON_SNIPPET_CHARS);
            let mut obj = serde_json::Map::new();
            obj.insert("rank".into(), json!(rank));
            obj.insert("id".into(), json!(it.id));
            obj.insert("repo".into(), json!(it.repo));
            obj.insert("kind".into(), json!(it.kind));
            obj.insert("tag".into(), json!(it.tag));
            obj.insert("as_of".into(), json!(it.as_of));
            obj.insert("status".into(), json!(it.status));
            match it.superseded_by.as_deref() {
                Some(by) if it.status == "superseded" && !by.is_empty() => {
                    obj.insert("superseded_by".into(), json!(by));
                }
                _ if it.status == "superseded" => {
                    obj.insert("retired".into(), json!(retired_label(&it.tags)));
                }
                _ => {}
            }
            obj.insert("source".into(), json!(it.source));
            obj.insert("title".into(), json!(it.title));
            obj.insert("snippet".into(), json!(snippet));
            obj.insert("refs".into(), json!(py::str::split(&it.refs)));
            obj.insert("tokens".into(), json!(item_tokens(it)));
            Value::Object(obj)
        })
        .collect();
    let mut scope = json!({"repo": page.repo, "all_repos": page.all_repos});
    if !page.scopes.is_empty() {
        scope["in"] = json!(page.scopes);
    }
    json!({
        "total": page.total,
        "limit": page.limit,
        "offset": page.offset,
        "status": page.status.as_str(),
        "scope": scope,
        "items": items,
        "next": list_next_cmds(page),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::items::put_item;

    #[test]
    fn run_list_validates_bounds_and_scopes_and_formats_json() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().to_str().unwrap();
        let conn = connect(&format!("{home}/memory.db"), None).unwrap();

        let mut a1 = Item::bare("m_a1", "anvil", "fact", "First", "First body", "decided");
        a1.as_of = "2026-06-01T10:00:00+00:00".into();
        a1.source = "task:T-1".into();
        put_item(&conn, &a1.post_init(), None).unwrap();

        let mut g1 = Item::bare("m_g1", "global", "fact", "Global", "Global body", "decided");
        g1.as_of = "2026-06-02T10:00:00+00:00".into();
        put_item(&conn, &g1.post_init(), None).unwrap();

        let mut sup = Item::bare("m_sup", "anvil", "fact", "Old", "Old body", "decided");
        sup.as_of = "2026-05-01T10:00:00+00:00".into();
        sup.status = "superseded".into();
        sup.superseded_by = Some("m_a1".into());
        put_item(&conn, &sup.post_init(), None).unwrap();

        let mut ret = Item::bare("m_ret", "anvil", "fact", "Gone", "Gone body", "decided");
        ret.as_of = "2026-05-02T10:00:00+00:00".into();
        ret.status = "superseded".into();
        ret.tags = "retired:manual".into();
        put_item(&conn, &ret.post_init(), None).unwrap();
        drop(conn);

        // Validation errors: limit/offset fail BEFORE tag/kind/config/DB checks.
        assert_eq!(
            run_list(&ListArgs {
                limit: 0,
                tags: StrList::Str("bogus"),
                home: Some(home),
                ..Default::default()
            })
            .unwrap_err()
            .python_text(),
            "ValueError: --limit must be at least 1"
        );
        assert_eq!(
            run_list(&ListArgs {
                offset: -1,
                tags: StrList::Str("bogus"),
                home: Some(home),
                ..Default::default()
            })
            .unwrap_err()
            .python_text(),
            "ValueError: --offset must be at least 0"
        );
        assert!(
            run_list(&ListArgs {
                tags: StrList::Str("bogus"),
                home: Some(home),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            run_list(&ListArgs {
                kinds: StrList::Str("bogus"),
                home: Some(home),
                ..Default::default()
            })
            .is_err()
        );
        assert_eq!(
            run_list(&ListArgs {
                since: "2026-06-10",
                until: "2026-06-01",
                home: Some(home),
                ..Default::default()
            })
            .unwrap_err()
            .python_text(),
            "ValueError: --since cannot be after --until"
        );

        // Exact repo excludes global.
        let p = run_list(&ListArgs {
            repo: "anvil",
            home: Some(home),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(p.total, 1);
        assert_eq!(p.repo.as_deref(), Some("anvil"));
        assert!(!p.all_repos);
        assert_eq!(p.items[0].id, "m_a1");

        // All repos clears repo to None.
        let p_all = run_list(&ListArgs {
            repo: "anvil",
            all_repos: true,
            status: StatusFilter::All,
            home: Some(home),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(p_all.total, 4);
        assert_eq!(p_all.repo, None);
        assert!(p_all.all_repos);

        // JSON schema carries superseded_by / retired conditionally and handles i64::MAX offset.
        let j = list_json(&p_all);
        assert_eq!(j["total"], 4);
        assert_eq!(j["status"], "all");
        assert_eq!(j["scope"], json!({"repo": null, "all_repos": true}));
        assert_eq!(j["items"][2]["id"], "m_ret");
        assert_eq!(j["items"][2]["retired"], "manual");
        assert!(j["items"][2].get("superseded_by").is_none());
        assert_eq!(j["items"][3]["id"], "m_sup");
        assert_eq!(j["items"][3]["superseded_by"], "m_a1");
        assert!(j["items"][3].get("retired").is_none());

        let max_page = ListPage {
            offset: i64::MAX,
            ..p
        };
        assert_eq!(
            list_json(&max_page)["items"][0]["rank"],
            9_223_372_036_854_775_808u64
        );
    }
}

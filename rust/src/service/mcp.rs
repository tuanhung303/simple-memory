//! `sm mcp`: an MCP server on stdio. One JSON-RPC 2.0 object per line, UTF-8, on stdin and
//! stdout. A request whose `_meta` carries the per-request fields of revision 2026-07-28 (or any
//! `server/discover`) is served statelessly; any other by the `initialize` handshake of revisions
//! 2024-11-05 to 2025-11-25.

use std::io::BufRead;
use std::sync::LazyLock;

use serde_json::{Value, json};

use crate::core::rank::DEFAULT_QUERY_LIMIT;
use crate::core::refs::RefsArg;
use crate::core::tags::{ALL_TAGS, VALID_KINDS, VALID_TAGS};
use crate::error::{Base, SmError};
use crate::py;
use crate::py::io::Out;
use crate::py::json::{Opts, dumps, from_json, loads};
use crate::py::value::PyValue;
use crate::service::amend::run_amend;
use crate::service::list::{DEFAULT_LIST_LIMIT, ListArgs, list_json, run_list};
use crate::service::put::{PutArgs, run_put};
use crate::service::query::{QueryArgs, StrList, lookup_json, run_lookup};
use crate::service::related::{DEFAULT_RELATED_LIMIT, related_json, run_related};
use crate::service::retire::run_retire;
use crate::service::show::run_show_many;
use crate::service::supersede::run_supersede;
use crate::service::upkeep::put_overlaps;
use crate::service::usage::log_usage;
use crate::sources::value_get;
use crate::store::items::AmendPatch;
use crate::store::search::StatusFilter;

/// Newest first (MCP 2025-11-25, basic/lifecycle, "Version Negotiation").
pub const LEGACY_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// Served per request, with no handshake (MCP 2026-07-28, basic/versioning).
pub const MODERN_VERSIONS: [&str; 1] = ["2026-07-28"];
pub const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub const META_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
pub const CACHE_TTL_MS: i64 = 300_000;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

pub const INSTRUCTIONS: &str = "Simple Memory holds decisions, results and task logs from earlier agent sessions. Call memory_query with the words, ids or paths of the task before you work, memory_show for the full text of an id, and memory_put to store a durable fact no task log holds.";

fn tools_json() -> Value {
    let repo = json!({"type": "string", "description": "Repo key; default: the repo of the server's working folder"});
    let scopes_in = json!({"type": "string", "description": "Comma-separated [scopes] paths (company/client/project) whose items to keep; searches every repo"});
    let source = json!({"type": "string", "description": "Comma-separated source prefixes (case-sensitive)"});
    let since = json!({"type": "string", "description": "Keep items with as_of on or after this ISO 8601 date or timestamp, or a relative bound such as 24h, 7d"});
    let until = json!({"type": "string", "description": "Keep items with as_of on or before this ISO 8601 date or timestamp (date-only includes full day), or a relative bound such as 24h, 7d"});
    json!([
        {
            "name": "memory_query",
            "title": "Search memory",
            "description": "Search memory items in the current repo plus global and unassigned ones. Returns the JSON of `sm query --format json`: ranked results with id, kind, tag, as_of, snippet and why, then missing words (with how many items a status, a filter or the repo scope hid), more matches and next steps.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Words, ticket ids, shas or paths"},
                    "bias": {"type": "string", "description": "Comma-separated reorder: recency, durable kinds and verified/decided tags by default ([query] default_bias); relevance, a tag, a kind, or scope:<path>"},
                    "kind": {"type": "string", "description": format!("Comma-separated kind filter: {}", VALID_KINDS.join(", "))},
                    "tag": {"type": "string", "description": format!("Comma-separated tag filter: {}", ALL_TAGS.join(", "))},
                    "source": source,
                    "since": since,
                    "until": until,
                    "recent": {"type": "boolean", "default": false, "description": "Sort matching items newest first (not allowed with bias)"},
                    "include_history": {"type": "boolean", "default": false, "description": "Include superseded and retired items"},
                    "limit": {"type": "integer", "minimum": 1, "default": DEFAULT_QUERY_LIMIT},
                    "all_repos": {"type": "boolean", "default": false, "description": "Search every repo"},
                    "repo": repo,
                    "in": scopes_in,
                },
                "required": ["query"],
                "additionalProperties": false,
            },
        },
        {
            "name": "memory_show",
            "title": "Show memory items",
            "description": "Full text and provenance of each id, in the order given. isError when an id is not found.",
            "inputSchema": {
                "type": "object",
                "properties": {"ids": {"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "Item ids"}},
                "required": ["ids"],
                "additionalProperties": false,
            },
        },
        {
            "name": "memory_related",
            "title": "Related memory items",
            "description": "The item's supersede chain, active items that share a ref with it and items from the same session. Returns the JSON of `sm related --format json`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Item id"},
                    "limit": {"type": "integer", "minimum": 1, "default": DEFAULT_RELATED_LIMIT, "description": "Most related items to return"},
                },
                "required": ["id"],
                "additionalProperties": false,
            },
        },
        {
            "name": "memory_put",
            "title": "Store a memory item",
            "description": "Store one durable fact, decision or learning. executed and verified need evidence. A body that already exists in the repo returns the existing id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "The item body (sm put takes it as body); its first line is the title"},
                    "kind": {"type": "string", "enum": VALID_KINDS, "default": "fact"},
                    "tag": {"type": "string", "enum": VALID_TAGS},
                    "evidence": {"type": "string", "description": "Command, sha, file:line or quote behind the tag"},
                    "refs": {"type": "string", "description": "Ticket ids, shas or paths, separated by spaces or commas"},
                    "supersedes": {"type": "string", "description": "Old item ids this item replaces, separated by spaces or commas"},
                    "repo": repo,
                },
                "required": ["text", "tag"],
                "additionalProperties": false,
            },
        },
        {
            "name": "memory_supersede",
            "title": "Supersede a memory item",
            "description": "Mark old_id superseded by new_id; a normal query then hides old_id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "old_id": {"type": "string", "description": "Item to retire"},
                    "new_id": {"type": "string", "description": "Item that replaces it"},
                },
                "required": ["old_id", "new_id"],
                "additionalProperties": false,
            },
        },
        {
            "name": "memory_list",
            "title": "List memory items",
            "description": "Browse stored memory items, newest first, with filters and pagination. Returns the JSON of `sm list --format json`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "minimum": 1, "default": DEFAULT_LIST_LIMIT},
                    "offset": {"type": "integer", "minimum": 0, "default": 0},
                    "kind": {"type": "string", "description": format!("Comma-separated kind filter: {}", VALID_KINDS.join(", "))},
                    "tag": {"type": "string", "description": format!("Comma-separated tag filter: {}", ALL_TAGS.join(", "))},
                    "source": source,
                    "since": since,
                    "until": until,
                    "status": {"type": "string", "enum": ["active", "superseded", "retired", "all"], "description": "Filter by status (default: active)"},
                    "include_history": {"type": "boolean", "default": false, "description": "Include superseded and retired items (same as status='all')"},
                    "all_repos": {"type": "boolean", "default": false, "description": "List across all repos"},
                    "repo": repo,
                    "in": scopes_in,
                },
                "required": [],
                "additionalProperties": false,
            },
        },
        {
            "name": "memory_amend",
            "title": "Amend a memory item",
            "description": "Create a revised active copy of an item and supersede the old one atomically.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Active item id to amend"},
                    "text": {"type": "string", "description": "Replace body text (--body on sm amend)"},
                    "title": {"type": "string", "description": "Replace title"},
                    "kind": {"type": "string", "enum": VALID_KINDS, "description": "Replace kind"},
                    "tag": {"type": "string", "enum": VALID_TAGS, "description": "Replace evidence tag"},
                    "evidence": {"type": "string", "description": "Replace evidence text"},
                    "repo": {"type": "string", "description": "Replace repo"},
                    "add_refs": {"type": "string", "description": "Add references (separated by spaces or commas)"},
                    "remove_refs": {"type": "string", "description": "Remove references (separated by spaces or commas)"},
                    "refs": {"type": "string", "description": "Replace references (separated by spaces or commas)"},
                    "add_tags": {"type": "string", "description": "Add comma-separated freeform tags"},
                    "remove_tags": {"type": "string", "description": "Remove comma-separated freeform tags"},
                    "tags": {"type": "string", "description": "Replace comma-separated freeform tags"},
                    "as_of": {"type": "string", "description": "Replace ISO 8601 timestamp"},
                },
                "required": ["id"],
                "additionalProperties": false,
            },
        },
        {
            "name": "memory_retire",
            "title": "Retire a memory item",
            "description": "Retire an active memory item with a reason so normal queries hide it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Active item id to retire"},
                    "reason": {"type": "string", "default": "manual", "description": "Why the item is retired; no commas"},
                },
                "required": ["id"],
                "additionalProperties": false,
            },
        },
    ])
}

/// `TOOLS`.
pub static TOOLS: LazyLock<Value> = LazyLock::new(tools_json);

fn tool_schema(name: &str) -> Option<&'static Value> {
    TOOLS
        .as_array()?
        .iter()
        .find(|t| t["name"] == name)
        .map(|t| &t["inputSchema"])
}

/// `RpcError(code, message, data)`.
#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<PyValue>,
}

fn rpc(code: i64, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        data: None,
    }
}

/// A handler failure: an `RpcError` answer, or an error that stops the server.
pub enum Fail {
    Rpc(RpcError),
    Stop(SmError),
}

impl From<SmError> for Fail {
    fn from(e: SmError) -> Self {
        Fail::Stop(e)
    }
}

fn s(v: &str) -> PyValue {
    PyValue::Str(v.to_string())
}

fn dict(pairs: Vec<(&str, PyValue)>) -> PyValue {
    PyValue::Dict(pairs.into_iter().map(|(k, v)| (s(k), v)).collect())
}

/// Tool arguments with their schema defaults: `{**defaults, **args}`.
pub type Args = Vec<(String, PyValue)>;

fn arg<'a>(a: &'a Args, key: &str) -> Option<&'a PyValue> {
    a.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn type_ok(t: &str, v: &PyValue) -> bool {
    match t {
        "string" => matches!(v, PyValue::Str(_)),
        "integer" => matches!(v, PyValue::Int(_)),
        "boolean" => matches!(v, PyValue::Bool(_)),
        "array" => matches!(v, PyValue::List(_)),
        _ => false,
    }
}

/// `value < minimum` for an int value (canonical decimal text) and a small minimum.
fn int_below(value: &str, minimum: i64) -> bool {
    match value.parse::<i64>() {
        Ok(n) => n < minimum,
        Err(_) => value.starts_with('-'),
    }
}

/// `check_arguments(schema, args)`: the arguments with schema defaults filled in; ValueError
/// names the first argument that breaks the schema.
pub fn check_arguments(schema: &Value, args: &[(PyValue, PyValue)]) -> Result<Args, SmError> {
    let props = schema["properties"]
        .as_object()
        .expect("a tool schema has properties");
    let names: Vec<String> = args.iter().map(|(k, _)| k.py_str()).collect();
    for req in schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !names.iter().any(|n| n == req) {
            return Err(SmError::value(format!(
                "missing required argument {}",
                py::str::repr(req)
            )));
        }
    }
    for (name, (_, value)) in names.iter().zip(args) {
        let rname = py::str::repr(name);
        let Some(spec) = props.get(name) else {
            let all: Vec<&str> = props.keys().map(String::as_str).collect();
            return Err(SmError::value(format!(
                "unknown argument {rname}; expected one of {}",
                all.join(", ")
            )));
        };
        let t = spec["type"].as_str().unwrap_or("");
        if !type_ok(t, value) {
            return Err(SmError::value(format!(
                "argument {rname} must be {t}, got {}",
                dumps(value, &Opts::default())?
            )));
        }
        if let (Some(choices), PyValue::Str(v)) =
            (spec.get("enum").and_then(Value::as_array), value)
            && !choices.iter().any(|c| c.as_str() == Some(v))
        {
            let all: Vec<&str> = choices.iter().filter_map(Value::as_str).collect();
            return Err(SmError::value(format!(
                "argument {rname} must be one of {}, got {}",
                all.join(", "),
                py::str::repr(v)
            )));
        }
        if let (Some(min), PyValue::Int(v)) = (spec.get("minimum").and_then(Value::as_i64), value)
            && int_below(v, min)
        {
            return Err(SmError::value(format!(
                "argument {rname} must be at least {min}, got {v}"
            )));
        }
        if let ("array", PyValue::List(items)) = (t, value) {
            let min_items = spec.get("minItems").and_then(Value::as_i64).unwrap_or(0);
            if (items.len() as i64) < min_items {
                return Err(SmError::value(format!(
                    "argument {rname} needs at least {min_items} item(s)"
                )));
            }
            let it = spec["items"]["type"].as_str().unwrap_or("");
            if !items.iter().all(|v| type_ok(it, v)) {
                return Err(SmError::value(format!(
                    "every item of {rname} must be {it}"
                )));
            }
        }
    }
    let mut out: Args = props
        .iter()
        .filter_map(|(k, sp)| sp.get("default").map(|d| (k.clone(), from_json(d))))
        .collect();
    for (name, (_, value)) in names.into_iter().zip(args) {
        match out.iter_mut().find(|(k, _)| *k == name) {
            Some(slot) => slot.1 = value.clone(),
            None => out.push((name, value.clone())),
        }
    }
    Ok(out)
}

pub(crate) fn item_json(it: &crate::core::item::Item) -> Value {
    json!({
        "id": it.id, "repo": it.repo, "kind": it.kind, "title": it.title, "tag": it.tag,
        "evidence": it.evidence, "as_of": it.as_of, "source": it.source,
        "refs": py::str::split(&it.refs), "tags": it.tags, "status": it.status,
        "superseded_by": it.superseded_by, "body": it.body,
    })
}

fn str_arg<'a>(a: &'a Args, key: &str) -> &'a str {
    match arg(a, key) {
        Some(PyValue::Str(v)) => v,
        _ => "",
    }
}

fn opt_str<'a>(a: &'a Args, key: &str) -> Option<&'a str> {
    match arg(a, key) {
        Some(PyValue::Str(v)) => Some(v.as_str()),
        _ => None,
    }
}

fn bool_arg(a: &Args, key: &str) -> bool {
    matches!(arg(a, key), Some(PyValue::Bool(true)))
}

fn opt_str_arg<'a>(a: &'a Args, key: &str) -> StrList<'a> {
    match arg(a, key) {
        Some(PyValue::Str(v)) => StrList::Str(v),
        _ => StrList::None,
    }
}

/// An int argument for an i64 parameter; past i64 it is the OverflowError SQLite binding gives.
fn int_arg(a: &Args, key: &str) -> Result<i64, SmError> {
    match arg(a, key) {
        // check_arguments let only ints >= 1 through, and Python's query and related only slice
        // with the limit, so a limit past i64 gives the same answer as i64::MAX
        Some(PyValue::Int(v)) => Ok(v.parse().unwrap_or(i64::MAX)),
        _ => Err(SmError::key(key)),
    }
}

pub struct Server {
    pub home: Option<String>,
}

impl Server {
    pub fn new(home: Option<&str>) -> Server {
        Server {
            home: home.map(String::from),
        }
    }

    fn query(&self, a: &Args) -> Result<(Value, bool), SmError> {
        let home = self.home.as_deref();
        let recent = bool_arg(a, "recent");
        if recent && arg(a, "bias").is_some() {
            return Err(SmError::value(
                "argument 'recent' is not allowed with argument 'bias'",
            ));
        }
        let repo = str_arg(a, "repo");
        let scopes_raw = str_arg(a, "in");
        if !py::str::strip(scopes_raw).is_empty() && !py::str::strip(repo).is_empty() {
            return Err(SmError::value(
                "argument 'repo' is not allowed with argument 'in'",
            ));
        }
        let include_history = bool_arg(a, "include_history");
        let q = QueryArgs {
            limit: int_arg(a, "limit")?,
            recent,
            tags: opt_str_arg(a, "tag"),
            kinds: opt_str_arg(a, "kind"),
            sources: opt_str_arg(a, "source"),
            since: str_arg(a, "since"),
            until: str_arg(a, "until"),
            bias: opt_str_arg(a, "bias"),
            all_repos: bool_arg(a, "all_repos"),
            repo,
            include_history,
            scopes: opt_str_arg(a, "in"),
            home,
            ..Default::default()
        };
        let lk = run_lookup(str_arg(a, "query"), &q)?;
        let ids: Vec<String> = lk.hits.iter().map(|h| h.item.id.clone()).collect();
        let mut flag_pairs = vec![
            (
                "bias",
                PyValue::List(lk.bias.iter().map(|b| s(b)).collect()),
            ),
            ("kind", s(str_arg(a, "kind"))),
            ("tag", s(str_arg(a, "tag"))),
            ("format", s("json")),
            ("limit", arg(a, "limit").cloned().unwrap_or(PyValue::None)),
            ("all", arg(a, "all_repos").cloned().unwrap_or(PyValue::None)),
            ("via", s("mcp")),
        ];
        for key in ["source", "since", "until"] {
            let val = str_arg(a, key);
            if !val.is_empty() {
                flag_pairs.push((key, s(val)));
            }
        }
        if recent {
            flag_pairs.push(("recent", PyValue::Bool(true)));
        }
        if include_history {
            flag_pairs.push(("include_history", PyValue::Bool(true)));
        }
        let flags = dict(flag_pairs);
        log_usage("query", &ids, lk.repo.as_deref(), Some(&flags), home)?;
        Ok((lookup_json(&lk), false))
    }

    fn show(&self, a: &Args) -> Result<(Value, bool), SmError> {
        let ids: Vec<String> = match arg(a, "ids") {
            Some(PyValue::List(v)) => v.iter().map(PyValue::py_str).collect(),
            _ => Vec::new(),
        };
        let (found, missing) = run_show_many(&ids, self.home.as_deref())?;
        let found_ids: Vec<String> = found.iter().map(|it| it.id.clone()).collect();
        log_usage(
            "show",
            &found_ids,
            None,
            Some(&dict(vec![("via", s("mcp"))])),
            self.home.as_deref(),
        )?;
        let bad = !missing.is_empty();
        Ok((
            json!({"items": found.iter().map(item_json).collect::<Vec<_>>(), "missing": missing}),
            bad,
        ))
    }

    fn related(&self, a: &Args) -> Result<(Value, bool), SmError> {
        let (target, related, skipped) =
            run_related(str_arg(a, "id"), int_arg(a, "limit")?, self.home.as_deref())?;
        let mut ids = vec![target.id.clone()];
        ids.extend(related.iter().map(|r| r.item.id.clone()));
        log_usage(
            "related",
            &ids,
            None,
            Some(&dict(vec![("via", s("mcp"))])),
            self.home.as_deref(),
        )?;
        Ok((related_json(&target, &related, &skipped), false))
    }

    fn put(&self, a: &Args) -> Result<(Value, bool), SmError> {
        let p = PutArgs {
            tag: str_arg(a, "tag"),
            kind: str_arg(a, "kind"),
            evidence: str_arg(a, "evidence"),
            refs: str_arg(a, "refs"),
            supersedes: RefsArg::Str(str_arg(a, "supersedes")),
            repo: str_arg(a, "repo"),
            home: self.home.as_deref(),
            ..Default::default()
        };
        let (item_id, created) = run_put(str_arg(a, "text"), &p)?;
        let overlaps = if created {
            put_overlaps(&item_id, self.home.as_deref(), None)?
        } else {
            Vec::new()
        };
        Ok((
            json!({"id": item_id, "created": created, "overlaps": overlaps}),
            false,
        ))
    }

    fn supersede(&self, a: &Args) -> Result<(Value, bool), SmError> {
        let (old_id, new_id) = run_supersede(
            str_arg(a, "old_id"),
            str_arg(a, "new_id"),
            self.home.as_deref(),
        )?;
        Ok((json!({"old_id": old_id, "superseded_by": new_id}), false))
    }

    fn list(&self, a: &Args) -> Result<(Value, bool), SmError> {
        let home = self.home.as_deref();
        let repo = str_arg(a, "repo");
        let all_repos = bool_arg(a, "all_repos");
        let scopes_raw = str_arg(a, "in");
        let scope_flags = usize::from(!py::str::strip(repo).is_empty())
            + usize::from(all_repos)
            + usize::from(!py::str::strip(scopes_raw).is_empty());
        if scope_flags > 1 {
            return Err(SmError::value(
                "only one of 'repo', 'all_repos', or 'in' may be set",
            ));
        }
        let include_history = bool_arg(a, "include_history");
        if opt_str(a, "status").is_some() && include_history {
            return Err(SmError::value(
                "argument 'include_history' is not allowed with argument 'status'",
            ));
        }
        let status = match opt_str(a, "status") {
            Some("active") => StatusFilter::Active,
            Some("superseded") => StatusFilter::Superseded,
            Some("retired") => StatusFilter::Retired,
            Some("all") => StatusFilter::All,
            Some(other) => return Err(SmError::value(format!("Invalid status '{other}'"))),
            None if include_history => StatusFilter::All,
            None => StatusFilter::Active,
        };
        let args = ListArgs {
            limit: int_arg(a, "limit")?,
            offset: int_arg(a, "offset")?,
            repo,
            all_repos,
            scopes: opt_str_arg(a, "in"),
            kinds: opt_str_arg(a, "kind"),
            tags: opt_str_arg(a, "tag"),
            sources: opt_str_arg(a, "source"),
            since: str_arg(a, "since"),
            until: str_arg(a, "until"),
            status,
            home,
            cwd: None,
        };
        let page = run_list(&args)?;
        let ids: Vec<String> = page.items.iter().map(|it| it.id.clone()).collect();
        let flags = dict(vec![
            ("kind", s(str_arg(a, "kind"))),
            ("tag", s(str_arg(a, "tag"))),
            ("source", s(str_arg(a, "source"))),
            ("since", s(str_arg(a, "since"))),
            ("until", s(str_arg(a, "until"))),
            ("status", s(page.status.as_str())),
            ("format", s("json")),
            ("limit", arg(a, "limit").cloned().unwrap_or(PyValue::None)),
            ("offset", arg(a, "offset").cloned().unwrap_or(PyValue::None)),
            ("all", arg(a, "all_repos").cloned().unwrap_or(PyValue::None)),
            ("via", s("mcp")),
        ]);
        log_usage("list", &ids, page.repo.as_deref(), Some(&flags), home)?;
        Ok((list_json(&page), false))
    }

    fn amend(&self, a: &Args) -> Result<(Value, bool), SmError> {
        let home = self.home.as_deref();
        let id = str_arg(a, "id");
        let replace_refs = opt_str(a, "refs");
        let replace_tags = opt_str(a, "tags");
        if replace_refs.is_some()
            && (opt_str(a, "add_refs").is_some() || opt_str(a, "remove_refs").is_some())
        {
            return Err(SmError::value(
                "argument 'refs' is not allowed with 'add_refs' or 'remove_refs'",
            ));
        }
        if replace_tags.is_some()
            && (opt_str(a, "add_tags").is_some() || opt_str(a, "remove_tags").is_some())
        {
            return Err(SmError::value(
                "argument 'tags' is not allowed with 'add_tags' or 'remove_tags'",
            ));
        }

        let patch = AmendPatch {
            repo: opt_str(a, "repo"),
            kind: opt_str(a, "kind"),
            tag: opt_str(a, "tag"),
            evidence: opt_str(a, "evidence"),
            title: opt_str(a, "title"),
            body: opt_str(a, "text"),
            add_refs: opt_str(a, "add_refs"),
            remove_refs: opt_str(a, "remove_refs"),
            replace_refs,
            add_tags: opt_str(a, "add_tags"),
            remove_tags: opt_str(a, "remove_tags"),
            replace_tags,
            as_of: opt_str(a, "as_of"),
        };
        let (new_id, body_changed) = run_amend(id, &patch, home)?;
        let overlaps = if body_changed {
            put_overlaps(&new_id, home, None)?
        } else {
            Vec::new()
        };
        let old_id = py::str::strip(id);
        log_usage(
            "amend",
            &[old_id.to_string(), new_id.clone()],
            None,
            Some(&dict(vec![("via", s("mcp"))])),
            home,
        )?;
        Ok((
            json!({
                "old_id": old_id,
                "id": new_id,
                "body_changed": body_changed,
                "overlaps": overlaps,
            }),
            false,
        ))
    }

    fn retire(&self, a: &Args) -> Result<(Value, bool), SmError> {
        let home = self.home.as_deref();
        let (id, reason) = run_retire(str_arg(a, "id"), str_arg(a, "reason"), home)?;
        log_usage(
            "retire",
            std::slice::from_ref(&id),
            None,
            Some(&dict(vec![("reason", s(&reason)), ("via", s("mcp"))])),
            home,
        )?;
        Ok((json!({"id": id, "retired": reason}), false))
    }

    /// `call_tool(params)`: a tool failure goes back to the model as a result it can correct.
    pub fn call_tool(&self, params: &PyValue, out: &mut Out) -> Result<PyValue, Fail> {
        let name = match value_get(params, "name") {
            Some(PyValue::Str(n)) if tool_schema(n).is_some() => n.clone(),
            other => {
                let message = match other {
                    None | Some(PyValue::None) => "missing argument 'name'".to_string(),
                    Some(v) => format!("Unknown tool: {}", v.py_str()),
                };
                return Err(Fail::Rpc(rpc(INVALID_PARAMS, message)));
            }
        };
        let empty = PyValue::Dict(Vec::new());
        let PyValue::Dict(args) = value_get(params, "arguments").unwrap_or(&empty) else {
            return Err(Fail::Rpc(rpc(
                INVALID_PARAMS,
                "tools/call arguments must be an object",
            )));
        };
        let schema = tool_schema(&name).expect("checked above");
        let run = || -> Result<(Value, bool), SmError> {
            let a = check_arguments(schema, args)?;
            match name.as_str() {
                "memory_query" => self.query(&a),
                "memory_show" => self.show(&a),
                "memory_related" => self.related(&a),
                "memory_put" => self.put(&a),
                "memory_supersede" => self.supersede(&a),
                "memory_list" => self.list(&a),
                "memory_amend" => self.amend(&a),
                "memory_retire" => self.retire(&a),
                _ => unreachable!("checked by tool_schema"),
            }
        };
        match run() {
            Ok((payload, is_error)) => Ok(dict(vec![
                (
                    "content",
                    PyValue::List(vec![dict(vec![
                        ("type", s("text")),
                        (
                            "text",
                            s(&py::json::dumps_value(&payload, &Opts::unicode())),
                        ),
                    ])]),
                ),
                ("isError", PyValue::Bool(is_error)),
            ])),
            Err(e) => {
                let raw = if e.is(Base::Key) {
                    unrepr(&e.msg)
                } else {
                    e.msg
                };
                let text = py::str::split(&raw).join(" ");
                out.eprint(&format!("sm mcp: {name}: {text}"))?;
                Ok(dict(vec![
                    (
                        "content",
                        PyValue::List(vec![dict(vec![
                            ("type", s("text")),
                            ("text", s(&format!("error: {text}"))),
                        ])]),
                    ),
                    ("isError", PyValue::Bool(true)),
                ]))
            }
        }
    }

    /// `handle(msg)`: the response to one parsed message; None for a notification.
    pub fn handle(&self, msg: &PyValue, out: &mut Out) -> Result<Option<PyValue>, SmError> {
        let method = match (msg, value_get(msg, "jsonrpc"), value_get(msg, "method")) {
            (PyValue::Dict(_), Some(PyValue::Str(v)), Some(PyValue::Str(m))) if v == "2.0" => {
                m.clone()
            }
            _ => {
                let rid = value_get(msg, "id").cloned().unwrap_or(PyValue::None);
                return Ok(Some(error(rid, INVALID_REQUEST, "Invalid Request", None)));
            }
        };
        let Some(rid) = value_get(msg, "id").cloned() else {
            return Ok(None);
        };
        let empty = PyValue::Dict(Vec::new());
        let params = value_get(msg, "params").unwrap_or(&empty);
        let result = (|| -> Result<PyValue, Fail> {
            if !matches!(params, PyValue::Dict(_)) {
                return Err(Fail::Rpc(rpc(INVALID_PARAMS, "params must be an object")));
            }
            if is_modern(&method, params) {
                return self.modern(&method, params, out);
            }
            match method.as_str() {
                "initialize" => initialize(params),
                "ping" => Ok(PyValue::Dict(Vec::new())),
                "tools/list" => Ok(dict(vec![("tools", from_json(&TOOLS))])),
                "tools/call" => self.call_tool(params, out),
                _ => Err(Fail::Rpc(rpc(
                    METHOD_NOT_FOUND,
                    format!("Method not found: {method}"),
                ))),
            }
        })();
        match result {
            Ok(r) => Ok(Some(dict(vec![
                ("jsonrpc", s("2.0")),
                ("id", rid),
                ("result", r),
            ]))),
            Err(Fail::Rpc(e)) => Ok(Some(error(rid, e.code, &e.message, e.data))),
            Err(Fail::Stop(e)) => Err(e),
        }
    }

    /// One revision 2026-07-28 request: check its `_meta`, answer with no connection state.
    fn modern(&self, method: &str, params: &PyValue, out: &mut Out) -> Result<PyValue, Fail> {
        let meta = value_get(params, "_meta");
        let asked = match meta {
            Some(m @ PyValue::Dict(_)) => {
                match (value_get(m, META_VERSION), value_get(m, META_CAPABILITIES)) {
                    (Some(PyValue::Str(v)), Some(PyValue::Dict(_))) => v.clone(),
                    _ => return Err(meta_error()),
                }
            }
            _ => return Err(meta_error()),
        };
        if !MODERN_VERSIONS.contains(&asked.as_str()) {
            return Err(Fail::Rpc(RpcError {
                code: UNSUPPORTED_PROTOCOL_VERSION,
                message: "Unsupported protocol version".into(),
                data: Some(dict(vec![
                    ("supported", supported()),
                    ("requested", s(&asked)),
                ])),
            }));
        }
        let result = match method {
            "server/discover" => dict(vec![
                ("supportedVersions", supported()),
                (
                    "capabilities",
                    dict(vec![(
                        "tools",
                        dict(vec![("listChanged", PyValue::Bool(false))]),
                    )]),
                ),
                ("instructions", s(INSTRUCTIONS)),
                ("ttlMs", PyValue::int(CACHE_TTL_MS)),
                ("cacheScope", s("public")),
            ]),
            "tools/list" => dict(vec![
                ("tools", from_json(&TOOLS)),
                ("ttlMs", PyValue::int(CACHE_TTL_MS)),
                ("cacheScope", s("public")),
            ]),
            "tools/call" => self.call_tool(params, out)?,
            _ => {
                return Err(Fail::Rpc(rpc(
                    METHOD_NOT_FOUND,
                    format!("Method not found: {method}"),
                )));
            }
        };
        let mut pairs = vec![(s("resultType"), s("complete"))];
        if let PyValue::Dict(r) = result {
            for (k, v) in r {
                match pairs.iter_mut().find(|(pk, _)| pk.key_eq(&k)) {
                    Some(slot) => slot.1 = v,
                    None => pairs.push((k, v)),
                }
            }
        }
        pairs.push((
            s("_meta"),
            dict(vec![("io.modelcontextprotocol/serverInfo", server_info())]),
        ));
        Ok(PyValue::Dict(pairs))
    }
}

fn meta_error() -> Fail {
    Fail::Rpc(rpc(
        INVALID_PARAMS,
        "_meta needs io.modelcontextprotocol/protocolVersion and clientCapabilities",
    ))
}

fn supported() -> PyValue {
    PyValue::List(
        MODERN_VERSIONS
            .iter()
            .chain(LEGACY_VERSIONS.iter())
            .map(|v| s(v))
            .collect(),
    )
}

/// The str a KeyError holds, from its `str()` (the key's repr).
pub fn unrepr(r: &str) -> String {
    let mut chars = r.chars();
    let Some(q @ ('\'' | '"')) = chars.next() else {
        return r.to_string();
    };
    let body: Vec<char> = chars.collect();
    if body.last() != Some(&q) {
        return r.to_string();
    }
    let body = &body[..body.len() - 1];
    let mut out = String::new();
    let mut i = 0;
    let hex = |s: &[char]| u32::from_str_radix(&s.iter().collect::<String>(), 16).ok();
    while i < body.len() {
        let c = body[i];
        if c != '\\' || i + 1 >= body.len() {
            out.push(c);
            i += 1;
            continue;
        }
        let n = body[i + 1];
        let (ch, used) = match n {
            'n' => (Some('\n'), 2),
            'r' => (Some('\r'), 2),
            't' => (Some('\t'), 2),
            '\\' | '\'' | '"' => (Some(n), 2),
            'x' if i + 4 <= body.len() => (hex(&body[i + 2..i + 4]).and_then(char::from_u32), 4),
            'u' if i + 6 <= body.len() => {
                (hex(&body[i + 2..i + 6]).map(py::json::surrogate_char), 6)
            }
            'U' if i + 10 <= body.len() => (hex(&body[i + 2..i + 10]).and_then(char::from_u32), 10),
            _ => (None, 1),
        };
        match ch {
            Some(c) => out.push(c),
            None => out.push('\\'),
        }
        i += used;
    }
    out
}

pub fn is_modern(method: &str, params: &PyValue) -> bool {
    if method == "server/discover" {
        return true;
    }
    let meta = value_get(params, "_meta").unwrap_or(&PyValue::None);
    if method == "initialize" || *meta == PyValue::None {
        return false;
    }
    !matches!(meta, PyValue::Dict(_))
        || value_get(meta, META_VERSION).is_some()
        || value_get(meta, META_CAPABILITIES).is_some()
}

pub fn error(rid: PyValue, code: i64, message: &str, data: Option<PyValue>) -> PyValue {
    let mut err = vec![(s("code"), PyValue::int(code)), (s("message"), s(message))];
    if let Some(d) = data.filter(|d| *d != PyValue::None) {
        err.push((s("data"), d));
    }
    dict(vec![
        ("jsonrpc", s("2.0")),
        ("id", rid),
        ("error", PyValue::Dict(err)),
    ])
}

/// The version is this package's (an accepted difference: `sm 0.6.0`).
pub fn server_info() -> PyValue {
    dict(vec![
        ("name", s("simple-memory")),
        ("title", s("Simple Memory")),
        ("version", s(env!("CARGO_PKG_VERSION"))),
    ])
}

fn initialize(params: &PyValue) -> Result<PyValue, Fail> {
    let Some(PyValue::Str(asked)) = value_get(params, "protocolVersion") else {
        return Err(Fail::Rpc(rpc(
            INVALID_PARAMS,
            "initialize needs a protocolVersion string",
        )));
    };
    let version = if LEGACY_VERSIONS.contains(&asked.as_str()) {
        asked.as_str()
    } else {
        LEGACY_VERSIONS[0]
    };
    Ok(dict(vec![
        ("protocolVersion", s(version)),
        (
            "capabilities",
            dict(vec![(
                "tools",
                dict(vec![("listChanged", PyValue::Bool(false))]),
            )]),
        ),
        ("serverInfo", server_info()),
        ("instructions", s(INSTRUCTIONS)),
    ]))
}

fn encode_reply(v: &PyValue) -> Result<Vec<u8>, SmError> {
    py::path::encode_text(&dumps(v, &Opts::unicode())?)
}

/// The answer to one input line, or None for a blank line or a notification.
pub fn answer_line(server: &Server, raw: &[u8], out: &mut Out) -> Result<Option<Vec<u8>>, SmError> {
    if raw.iter().all(|b| b.is_ascii_whitespace() || *b == 0x0b) {
        return Ok(None);
    }
    let parsed = match std::str::from_utf8(raw) {
        Err(e) => Err(SmError::unicode_decode(raw, e)),
        Ok(text) => loads(text),
    };
    let response = match parsed {
        Err(e) => {
            out.eprint(&format!("sm mcp: parse error: {}", e.msg))?;
            Some(error(PyValue::None, PARSE_ERROR, "Parse error", None))
        }
        Ok(msg) => match value_get(&msg, "id").map(encode_reply) {
            // an id that cannot be written back is refused before the request runs
            Some(Err(e)) => {
                out.eprint(&format!("sm mcp: reply error: {}", e.msg))?;
                Some(error(
                    PyValue::None,
                    INVALID_REQUEST,
                    "Invalid Request",
                    None,
                ))
            }
            _ => server.handle(&msg, out)?,
        },
    };
    let Some(resp) = response else {
        return Ok(None);
    };
    let mut bytes = match encode_reply(&resp) {
        Ok(b) => b,
        Err(e) => {
            // a lone surrogate in an echoed method, tool name or version: answer, never stop
            out.eprint(&format!("sm mcp: reply error: {}", e.msg))?;
            let rid = value_get(&resp, "id").cloned().unwrap_or(PyValue::None);
            encode_reply(&error(rid, INVALID_REQUEST, "Invalid Request", None))?
        }
    };
    bytes.push(b'\n');
    Ok(Some(bytes))
}

/// `serve(instream, outstream, home=...)`: answer each line until end of input; logs go to stderr.
pub fn serve(
    instream: &mut dyn BufRead,
    out: &mut Out,
    home: Option<&str>,
) -> Result<i32, SmError> {
    let server = Server::new(home);
    out.eprint(&format!(
        "sm mcp: serving MCP {} and {} to {} on stdio",
        MODERN_VERSIONS[0],
        LEGACY_VERSIONS[LEGACY_VERSIONS.len() - 1],
        LEGACY_VERSIONS[0]
    ))?;
    loop {
        let mut raw = Vec::new();
        let n = instream
            .read_until(b'\n', &mut raw)
            .map_err(|e| SmError::from_io(&e, None))?;
        if n == 0 {
            return Ok(0);
        }
        if let Some(bytes) = answer_line(&server, &raw, out)? {
            out.write_bytes(&bytes)?;
            out.flush()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Python's `sm mcp` output for the same input in an empty scratch home, captured with
    /// simple-memory 0.3.2 (the server version reads `<VERSION>`): every JSON-RPC error, the
    /// tool-error results with their stderr lines, two parse errors (bad JSON, bad UTF-8), a
    /// limit past i64 and a legacy initialize.
    #[test]
    fn serve_matches_python_on_the_error_golden() {
        let dir = tempfile::tempdir().unwrap();
        let mut input: &[u8] = include_bytes!("testdata/mcp_errors.in");
        let mut out = Out::capture();
        let code = serve(&mut input, &mut out, Some(dir.path().to_str().unwrap())).unwrap();
        out.finish(code);
        let (stdout, stderr) = out.captured();
        const V032_QUERY_ARGS: &str = "query, bias, kind, tag, limit, all_repos, repo, in";
        const V060_QUERY_ARGS: &str = "query, bias, kind, tag, source, since, until, recent, include_history, limit, all_repos, repo, in";
        let want = include_str!("testdata/mcp_errors.out")
            .replace("<VERSION>", env!("CARGO_PKG_VERSION"))
            .replace(V032_QUERY_ARGS, V060_QUERY_ARGS);
        let want_err =
            include_str!("testdata/mcp_errors.err").replace(V032_QUERY_ARGS, V060_QUERY_ARGS);
        assert_eq!(code, 0);
        for (n, (got, exp)) in stdout.lines().zip(want.lines()).enumerate() {
            assert_eq!(got, exp, "line {}", n + 1);
        }
        assert_eq!(stdout.lines().count(), want.lines().count());
        assert_eq!(stderr, want_err);
    }

    fn msg(text: &str) -> PyValue {
        loads(text).unwrap()
    }

    #[test]
    fn a_notification_gets_no_answer_and_a_blank_line_is_skipped() {
        let server = Server::new(Some("/nonexistent-sm-r"));
        let mut out = Out::capture();
        assert_eq!(
            server
                .handle(&msg(r#"{"jsonrpc":"2.0","method":"ping"}"#), &mut out)
                .unwrap(),
            None
        );
        assert_eq!(
            answer_line(&server, b" \t\x0b\r\n", &mut out).unwrap(),
            None
        );
        // an id of null is still a request
        let got = server
            .handle(
                &msg(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#),
                &mut out,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            dumps(&got, &Opts::unicode()).unwrap(),
            r#"{"jsonrpc": "2.0", "id": null, "result": {}}"#
        );
    }

    #[test]
    fn modern_needs_a_meta_version_string_and_capabilities_object() {
        let p = |t: &str| msg(t);
        assert!(is_modern(
            "tools/list",
            &p(r#"{"_meta":{"io.modelcontextprotocol/protocolVersion":"x"}}"#)
        ));
        assert!(is_modern("server/discover", &p("{}")));
        assert!(!is_modern("tools/list", &p("{}")));
        assert!(!is_modern(
            "initialize",
            &p(r#"{"_meta":{"io.modelcontextprotocol/protocolVersion":"x"}}"#)
        ));
        let server = Server::new(Some("/nonexistent-sm-r"));
        let mut out = Out::capture();
        let ok = server
            .handle(&msg(r#"{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#), &mut out)
            .unwrap()
            .unwrap();
        let text = dumps(&ok, &Opts::unicode()).unwrap();
        assert!(text.starts_with(r#"{"jsonrpc": "2.0", "id": 1, "result": {"resultType": "complete", "supportedVersions": ["2026-07-28""#), "{text}");
        assert!(text.ends_with(&format!(r#""_meta": {{"io.modelcontextprotocol/serverInfo": {{"name": "simple-memory", "title": "Simple Memory", "version": "{}"}}}}}}}}"#, env!("CARGO_PKG_VERSION"))), "{text}");
        let bad = server
            .handle(&msg(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":[]}}}"#), &mut out)
            .unwrap()
            .unwrap();
        assert!(dumps(&bad, &Opts::unicode()).unwrap().contains(
            "_meta needs io.modelcontextprotocol/protocolVersion and clientCapabilities"
        ));
    }

    #[test]
    fn check_arguments_types_minimums_and_defaults() {
        let schema = tool_schema("memory_query").unwrap();
        let args = |t: &str| match msg(t) {
            PyValue::Dict(d) => d,
            _ => unreachable!(),
        };
        let a = check_arguments(schema, &args(r#"{"query":"q"}"#)).unwrap();
        assert_eq!(
            arg(&a, "limit").map(PyValue::py_repr),
            Some(DEFAULT_QUERY_LIMIT.to_string())
        );
        assert_eq!(
            check_arguments(schema, &args(r#"{"query":"q","limit":true}"#))
                .unwrap_err()
                .msg,
            "argument 'limit' must be integer, got true"
        );
        assert_eq!(
            check_arguments(schema, &args(r#"{"query":"q","limit":1.0}"#))
                .unwrap_err()
                .msg,
            "argument 'limit' must be integer, got 1.0"
        );
        assert!(check_arguments(schema, &args(r#"{"query":"q","limit":1}"#)).is_ok());
        assert_eq!(
            check_arguments(
                schema,
                &args(r#"{"query":"q","limit":-99999999999999999999}"#)
            )
            .unwrap_err()
            .msg,
            "argument 'limit' must be at least 1, got -99999999999999999999"
        );
        assert_eq!(
            check_arguments(schema, &args(r#"{"query":1}"#))
                .unwrap_err()
                .msg,
            "argument 'query' must be string, got 1"
        );
    }
}

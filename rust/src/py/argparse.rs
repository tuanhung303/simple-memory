//! Port of the subset of CPython 3.15.1 `argparse` that the `sm` parser uses.
//!
//! The parse follows `ArgumentParser._parse_known_args` step by step: the `O`/`A`/`-` pattern of
//! the arg strings, the nargs regexes, unique prefix matching of long options, `--opt=value`, `--`,
//! negative-number-like positionals, nested subparsers that hand unknown args back to the top
//! parser, mutually exclusive groups, and the error texts (`<prog>: error: <msg>`, exit 2).
//! `type=int` and `type=float` go through Python's `int()` and `float()` rules for a str.
//! Usage and help text are close to argparse's, not equal (an accepted difference).

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

use crate::py::io::{Out, repr};

/// A namespace value as Python holds it after parsing.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    None,
    Bool(bool),
    Int(i64),
    /// A Python int outside i64, as canonical decimal digits with an optional leading `-`.
    BigInt(String),
    Float(f64),
    Str(String),
    List(Vec<Value>),
}

/// `argparse.Namespace`: attribute name to value, in the order the attributes were first set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Namespace {
    map: IndexMap<String, Value>,
}

impl Namespace {
    pub fn new() -> Namespace {
        Namespace::default()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.map.contains_key(name)
    }

    /// `setattr(namespace, name, value)`: a new name goes last, an existing one keeps its place.
    pub fn set(&mut self, name: &str, value: Value) {
        self.map.insert(name.to_string(), value);
    }

    #[cfg(test)]
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.map.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// `getattr(args, name)`. A name the parser never sets is a bug in the caller: it panics,
    /// as Python raises AttributeError.
    pub fn get(&self, name: &str) -> &Value {
        self.map
            .get(name)
            .unwrap_or_else(|| panic!("'Namespace' object has no attribute {name:?}"))
    }

    /// `getattr(namespace, name, default)`.
    pub fn get_or<'a>(&'a self, name: &str, default: &'a Value) -> &'a Value {
        self.map.get(name).unwrap_or(default)
    }

    /// A str attribute. Undecodable argv bytes stay as stand-in chars (`py::io::decode_argv`).
    pub fn get_str(&self, name: &str) -> &str {
        match self.get(name) {
            Value::Str(s) => s,
            other => panic!("attribute {name:?} is not a str: {other:?}"),
        }
    }

    /// A str attribute whose default is None.
    pub fn get_opt_str(&self, name: &str) -> Option<&str> {
        match self.get(name) {
            Value::None => None,
            Value::Str(s) => Some(s),
            other => panic!("attribute {name:?} is not a str or None: {other:?}"),
        }
    }

    pub fn get_bool(&self, name: &str) -> bool {
        match self.get(name) {
            Value::Bool(b) => *b,
            other => panic!("attribute {name:?} is not a bool: {other:?}"),
        }
    }

    /// An int attribute. A BigInt saturates to i64::MIN or i64::MAX; use `get` to see it.
    pub fn get_int(&self, name: &str) -> i64 {
        self.get_opt_int(name)
            .unwrap_or_else(|| panic!("attribute {name:?} is None, not an int"))
    }

    /// An int attribute whose default is None.
    pub fn get_opt_int(&self, name: &str) -> Option<i64> {
        match self.get(name) {
            Value::None => None,
            Value::Int(i) => Some(*i),
            Value::BigInt(d) => Some(if d.starts_with('-') {
                i64::MIN
            } else {
                i64::MAX
            }),
            other => panic!("attribute {name:?} is not an int: {other:?}"),
        }
    }

    pub fn get_float(&self, name: &str) -> f64 {
        self.get_opt_float(name)
            .unwrap_or_else(|| panic!("attribute {name:?} is None, not a float"))
    }

    /// A float attribute whose default is None.
    pub fn get_opt_float(&self, name: &str) -> Option<f64> {
        match self.get(name) {
            Value::None => None,
            Value::Float(f) => Some(*f),
            other => panic!("attribute {name:?} is not a float: {other:?}"),
        }
    }

    /// A list-of-str attribute (`nargs='*'`, `nargs='+'`, and `bias`).
    pub fn get_str_list(&self, name: &str) -> Vec<String> {
        match self.get(name) {
            Value::List(items) => items
                .iter()
                .map(|v| match v {
                    Value::Str(s) => s.clone(),
                    other => panic!("attribute {name:?} holds a non-str item: {other:?}"),
                })
                .collect(),
            Value::None => Vec::new(),
            other => panic!("attribute {name:?} is not a list: {other:?}"),
        }
    }
}

/// `nargs`, for the values sm uses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Nargs {
    /// `nargs=None`: one argument.
    One,
    /// `nargs=0`.
    Zero,
    /// `nargs='*'`.
    ZeroOrMore,
    /// `nargs='+'`.
    OneOrMore,
    /// `nargs=PARSER`, the subparsers action.
    Parser,
}

/// `type=`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Conv {
    Str,
    Int,
    Float,
}

/// What a custom `Action.__call__` can raise.
pub enum ActionError {
    /// `raise argparse.ArgumentError(self, msg)`.
    Arg(String),
    /// `parser.exit(status)`.
    Exit(i32),
}

/// A custom `Action.__call__(parser, namespace, values, option_string)`.
pub type CustomFn = fn(&Action, &mut Namespace, Value, &mut Out) -> Result<(), ActionError>;

#[derive(Clone)]
pub enum Kind {
    Store,
    StoreTrue,
    Help,
    Custom(CustomFn),
    /// The subparsers action: each name with its help and parser.
    Parsers(Vec<(String, String, Parser)>),
}

#[derive(Clone)]
pub struct Action {
    pub option_strings: Vec<String>,
    /// None is `SUPPRESS`.
    pub dest: Option<String>,
    pub nargs: Nargs,
    pub kind: Kind,
    pub conv: Conv,
    pub choices: Option<Vec<String>>,
    /// None is `SUPPRESS`.
    pub default: Option<Value>,
    pub required: bool,
    pub metavar: Option<String>,
    pub help: String,
}

/// The keyword arguments of `add_argument` that sm passes. Start from `Arg::default()`.
#[derive(Clone)]
pub struct Arg {
    pub nargs: Option<Nargs>,
    pub kind: Option<Kind>,
    pub conv: Conv,
    pub choices: Option<Vec<String>>,
    /// None leaves argparse's default (None, or False for store_true).
    pub default: Option<Value>,
    pub dest: Option<String>,
    pub metavar: Option<String>,
    pub help: String,
}

impl Default for Arg {
    fn default() -> Arg {
        Arg {
            nargs: None,
            kind: None,
            conv: Conv::Str,
            choices: None,
            default: None,
            dest: None,
            metavar: None,
            help: String::new(),
        }
    }
}

/// One `ArgumentParser`.
#[derive(Clone)]
pub struct Parser {
    pub prog: String,
    pub description: String,
    pub actions: Vec<Action>,
    /// `_option_string_actions`, in insertion order.
    option_strings: Vec<(String, usize)>,
    /// Each mutually exclusive group, as indexes into `actions`.
    mutex_groups: Vec<Vec<usize>>,
}

/// `parse_args` ended the process: the exit status (2 after an error, 0 after help or version).
#[derive(Debug, PartialEq)]
pub struct Exit(pub i32);

enum Flow {
    /// An `ArgumentError`, already formatted with its `argument X: ` prefix.
    Error(String),
    Exit(i32),
}

struct OptTuple {
    action: Option<usize>,
    option_string: String,
    sep: Option<String>,
    explicit_arg: Option<String>,
}

impl Parser {
    /// `ArgumentParser(prog=..., description=...)`, with its `-h/--help`.
    pub fn new(prog: &str, description: &str) -> Parser {
        let mut p = Parser {
            prog: prog.to_string(),
            description: description.to_string(),
            actions: Vec::new(),
            option_strings: Vec::new(),
            mutex_groups: Vec::new(),
        };
        p.add_argument(
            &["-h", "--help"],
            Arg {
                kind: Some(Kind::Help),
                help: "show this help message and exit".into(),
                ..Arg::default()
            },
        );
        p
    }

    /// `add_argument(*names, **kwargs)`; returns the action index.
    pub fn add_argument(&mut self, names: &[&str], arg: Arg) -> usize {
        let positional = names.len() == 1 && !names[0].starts_with('-');
        let kind = arg.kind.unwrap_or(Kind::Store);
        let (nargs_default, default_default, dest_default) = match &kind {
            Kind::StoreTrue => (Nargs::Zero, Some(Value::Bool(false)), true),
            Kind::Help => (Nargs::Zero, None, false),
            _ => (Nargs::One, Some(Value::None), true),
        };
        let nargs = arg.nargs.unwrap_or(nargs_default);
        let default = match (&kind, arg.default) {
            (Kind::Help, _) => None,
            (_, Some(v)) => Some(v),
            (_, None) => default_default,
        };
        let (option_strings, dest) = if positional {
            (Vec::new(), arg.dest.unwrap_or_else(|| names[0].to_string()))
        } else {
            let opts: Vec<String> = names.iter().map(|s| s.to_string()).collect();
            let dest = arg.dest.unwrap_or_else(|| infer_dest(&opts));
            (opts, dest)
        };
        let required = positional && !matches!(nargs, Nargs::ZeroOrMore);
        let action = Action {
            option_strings,
            dest: if dest_default { Some(dest) } else { None },
            nargs,
            kind,
            conv: arg.conv,
            choices: arg.choices,
            default,
            required,
            metavar: arg.metavar,
            help: arg.help,
        };
        let idx = self.actions.len();
        for s in &action.option_strings {
            self.option_strings.push((s.clone(), idx));
        }
        self.actions.push(action);
        idx
    }

    /// `add_mutually_exclusive_group()` with its members added: pass their action indexes.
    pub fn add_mutually_exclusive_group(&mut self, members: Vec<usize>) {
        self.mutex_groups.push(members);
    }

    /// `add_subparsers(dest=..., required=...)` with its parsers, built by `Subparsers`.
    pub fn add_subparsers(&mut self, subs: Subparsers) {
        let names: Vec<String> = subs.parsers.iter().map(|(n, _, _)| n.clone()).collect();
        self.actions.push(Action {
            option_strings: Vec::new(),
            dest: Some(subs.dest),
            nargs: Nargs::Parser,
            kind: Kind::Parsers(subs.parsers),
            conv: Conv::Str,
            choices: Some(names),
            default: Some(Value::None),
            required: subs.required,
            metavar: None,
            help: String::new(),
        });
    }

    fn action_index(&self, option_string: &str) -> Option<usize> {
        self.option_strings
            .iter()
            .find(|(s, _)| s == option_string)
            .map(|(_, i)| *i)
    }

    /// `parse_args(args)`: errors print usage and `<prog>: error: <msg>` to stderr.
    pub fn parse_args(&self, args: &[String], out: &mut Out) -> Result<Namespace, Exit> {
        let (ns, extras) = self.parse_known_args(args.to_vec(), out)?;
        if !extras.is_empty() {
            let msg = format!("unrecognized arguments: {}", extras.join(" "));
            return Err(self.error(&msg, out));
        }
        Ok(ns)
    }

    /// `parse_known_args(args, None)`.
    pub fn parse_known_args(
        &self,
        args: Vec<String>,
        out: &mut Out,
    ) -> Result<(Namespace, Vec<String>), Exit> {
        let mut ns = Namespace::new();
        for a in &self.actions {
            if let (Some(dest), Some(default)) = (&a.dest, &a.default)
                && !ns.contains(dest)
            {
                ns.set(dest, default.clone());
            }
        }
        match self.parse_inner(args, &mut ns, out) {
            Ok(extras) => Ok((ns, extras)),
            Err(Flow::Error(msg)) => Err(self.error(&msg, out)),
            Err(Flow::Exit(code)) => Err(Exit(code)),
        }
    }

    /// `ArgumentParser.error`: usage, then `<prog>: error: <msg>`, exit 2.
    pub fn error(&self, msg: &str, out: &mut Out) -> Exit {
        let _ = out.ewrite(&self.format_usage());
        let _ = out.ewrite(&format!("{}: error: {}\n", self.prog, msg));
        Exit(2)
    }

    fn parse_inner(
        &self,
        arg_strings: Vec<String>,
        ns: &mut Namespace,
        out: &mut Out,
    ) -> Result<Vec<String>, Flow> {
        let mut conflicts: HashMap<usize, Vec<usize>> = HashMap::new();
        for group in &self.mutex_groups {
            for (i, &a) in group.iter().enumerate() {
                let c = conflicts.entry(a).or_default();
                c.extend(&group[..i]);
                c.extend(&group[i + 1..]);
            }
        }

        let mut option_indices: HashMap<usize, Vec<OptTuple>> = HashMap::new();
        let mut pattern = String::new();
        let mut iter = arg_strings.iter().enumerate();
        while let Some((i, s)) = iter.next() {
            if s == "--" {
                pattern.push('-');
                for _ in iter.by_ref() {
                    pattern.push('A');
                }
                break;
            }
            match self.parse_optional(s) {
                None => pattern.push('A'),
                Some(t) => {
                    option_indices.insert(i, t);
                    pattern.push('O');
                }
            }
        }
        let pat: Vec<char> = pattern.chars().collect();

        let mut st = State {
            seen: HashSet::new(),
            seen_non_default: HashSet::new(),
            conflicts,
            sub_extras: Vec::new(),
        };
        let mut extras: Vec<String> = Vec::new();
        let mut positionals: Vec<usize> = (0..self.actions.len())
            .filter(|&i| self.actions[i].option_strings.is_empty())
            .collect();

        let max_option_index = option_indices.keys().copied().max();
        let mut start = 0usize;
        if let Some(max_opt) = max_option_index {
            while start <= max_opt {
                let mut next_opt = start;
                while next_opt <= max_opt && !option_indices.contains_key(&next_opt) {
                    next_opt += 1;
                }
                if start != next_opt {
                    let end = self.consume_positionals(
                        start,
                        &mut positionals,
                        &arg_strings,
                        &pat,
                        ns,
                        &mut st,
                        out,
                    )?;
                    if end > start {
                        start = end;
                        continue;
                    }
                    start = end;
                }
                if !option_indices.contains_key(&start) {
                    extras.extend(arg_strings[start..next_opt].iter().cloned());
                    start = next_opt;
                }
                start = self.consume_optional(
                    start,
                    &option_indices,
                    &arg_strings,
                    &pat,
                    ns,
                    &mut st,
                    &mut extras,
                    out,
                )?;
            }
        }
        let stop = self.consume_positionals(
            start,
            &mut positionals,
            &arg_strings,
            &pat,
            ns,
            &mut st,
            out,
        )?;
        extras.extend(arg_strings[stop..].iter().cloned());

        let required: Vec<String> = (0..self.actions.len())
            .filter(|i| !st.seen.contains(i) && self.actions[*i].required)
            .map(|i| action_name(&self.actions[i]).unwrap_or_default())
            .collect();
        if !required.is_empty() {
            return Err(Flow::Error(format!(
                "the following arguments are required: {}",
                required.join(", ")
            )));
        }
        extras.extend(st.sub_extras);
        Ok(extras)
    }

    #[allow(clippy::too_many_arguments)]
    fn consume_optional(
        &self,
        start: usize,
        option_indices: &HashMap<usize, Vec<OptTuple>>,
        arg_strings: &[String],
        pat: &[char],
        ns: &mut Namespace,
        st: &mut State,
        extras: &mut Vec<String>,
        out: &mut Out,
    ) -> Result<usize, Flow> {
        let tuples = &option_indices[&start];
        if tuples.len() > 1 {
            let options: Vec<&str> = tuples.iter().map(|t| t.option_string.as_str()).collect();
            return Err(Flow::Error(format!(
                "ambiguous option: {} could match {}",
                arg_strings[start],
                options.join(", ")
            )));
        }
        let t = &tuples[0];
        let mut action = t.action;
        let mut option_string = t.option_string.clone();
        let mut sep = t.sep.clone();
        let mut explicit_arg = t.explicit_arg.clone();
        let mut action_tuples: Vec<(usize, Vec<String>, String)> = Vec::new();
        let stop;
        loop {
            let Some(a) = action else {
                extras.push(arg_strings[start].clone());
                return Ok(start + 1);
            };
            if let Some(explicit) = explicit_arg.clone() {
                let arg_count = self.match_argument(a, "A")?;
                let second = option_string.chars().nth(1);
                if arg_count == 0 && second != Some('-') && !explicit.is_empty() {
                    if sep.as_deref().is_some_and(|s| !s.is_empty()) || explicit.starts_with('-') {
                        return Err(arg_error(
                            &self.actions[a],
                            &format!("ignored explicit argument {}", repr(&explicit)),
                        ));
                    }
                    action_tuples.push((a, Vec::new(), option_string.clone()));
                    let first_char: char = option_string.chars().next().unwrap_or('-');
                    let mut rest = explicit.chars();
                    let c0 = rest.next().unwrap_or_default();
                    option_string = format!("{first_char}{c0}");
                    if let Some(next) = self.action_index(&option_string) {
                        action = Some(next);
                        let tail: String = rest.collect();
                        if tail.is_empty() {
                            sep = None;
                            explicit_arg = None;
                        } else if let Some(after) = tail.strip_prefix('=') {
                            sep = Some("=".into());
                            explicit_arg = Some(after.to_string());
                        } else {
                            sep = Some(String::new());
                            explicit_arg = Some(tail);
                        }
                    } else {
                        extras.push(format!("{first_char}{explicit}"));
                        stop = start + 1;
                        break;
                    }
                } else if arg_count == 1 {
                    stop = start + 1;
                    action_tuples.push((a, vec![explicit], option_string));
                    break;
                } else {
                    return Err(arg_error(
                        &self.actions[a],
                        &format!("ignored explicit argument {}", repr(&explicit)),
                    ));
                }
            } else {
                let begin = start + 1;
                let selected: String = pat[begin.min(pat.len())..].iter().collect();
                let arg_count = self.match_argument(a, &selected)?;
                stop = begin + arg_count;
                action_tuples.push((a, arg_strings[begin..stop].to_vec(), option_string));
                break;
            }
        }
        for (a, args, opt) in action_tuples {
            self.take_action(a, args, Some(&opt), ns, st, out)?;
        }
        Ok(stop)
    }

    #[allow(clippy::too_many_arguments)]
    fn consume_positionals(
        &self,
        mut start: usize,
        positionals: &mut Vec<usize>,
        arg_strings: &[String],
        pat: &[char],
        ns: &mut Namespace,
        st: &mut State,
        out: &mut Out,
    ) -> Result<usize, Flow> {
        let selected: String = pat[start.min(pat.len())..].iter().collect();
        let counts = self.match_arguments_partial(positionals, &selected);
        for (&a, &count) in positionals.iter().zip(counts.iter()) {
            let mut args: Vec<String> = arg_strings[start..start + count].to_vec();
            if self.actions[a].nargs == Nargs::Parser {
                if pat.get(start) == Some(&'-') {
                    args.remove(0);
                }
            } else if pat[start..start + count].contains(&'-')
                && let Some(pos) = args.iter().position(|s| s == "--")
            {
                args.remove(pos);
            }
            start += count;
            self.take_action(a, args, None, ns, st, out)?;
        }
        positionals.drain(..counts.len());
        Ok(start)
    }

    fn take_action(
        &self,
        a: usize,
        argument_strings: Vec<String>,
        _option_string: Option<&str>,
        ns: &mut Namespace,
        st: &mut State,
        out: &mut Out,
    ) -> Result<(), Flow> {
        let action = &self.actions[a];
        st.seen.insert(a);
        let has_strings = !argument_strings.is_empty();
        let value = self.get_values(action, argument_strings)?;
        if !action.option_strings.is_empty() || has_strings {
            st.seen_non_default.insert(a);
            if let Some(cs) = st.conflicts.get(&a) {
                for c in cs {
                    if st.seen_non_default.contains(c) {
                        let name = action_name(&self.actions[*c]).unwrap_or_default();
                        return Err(arg_error(
                            action,
                            &format!("not allowed with argument {name}"),
                        ));
                    }
                }
            }
        }
        match &action.kind {
            Kind::Store => {
                ns.set(action.dest.as_deref().unwrap_or_default(), value);
            }
            Kind::StoreTrue => {
                ns.set(
                    action.dest.as_deref().unwrap_or_default(),
                    Value::Bool(true),
                );
            }
            Kind::Help => {
                let _ = out.write(&self.format_help());
                return Err(Flow::Exit(0));
            }
            Kind::Custom(f) => match f(action, ns, value, out) {
                Ok(()) => {}
                Err(ActionError::Arg(msg)) => return Err(arg_error(action, &msg)),
                Err(ActionError::Exit(code)) => return Err(Flow::Exit(code)),
            },
            Kind::Parsers(parsers) => {
                let Value::List(values) = value else {
                    unreachable!("PARSER values are a list")
                };
                let mut strings = values.into_iter().map(|v| match v {
                    Value::Str(s) => s,
                    _ => unreachable!("PARSER values are str"),
                });
                let name = strings.next().unwrap_or_default();
                let rest: Vec<String> = strings.collect();
                if let Some(dest) = &action.dest {
                    ns.set(dest, Value::Str(name.clone()));
                }
                let (_, _, sub) = parsers
                    .iter()
                    .find(|(n, _, _)| *n == name)
                    .expect("check_value accepted the parser name");
                match sub.parse_known_args(rest, out) {
                    Ok((subns, sub_extras)) => {
                        for (k, v) in subns.map {
                            ns.set(&k, v);
                        }
                        st.sub_extras.extend(sub_extras);
                    }
                    Err(Exit(code)) => return Err(Flow::Exit(code)),
                }
            }
        }
        Ok(())
    }

    fn get_values(&self, action: &Action, arg_strings: Vec<String>) -> Result<Value, Flow> {
        if arg_strings.is_empty()
            && action.nargs == Nargs::ZeroOrMore
            && action.option_strings.is_empty()
        {
            return Ok(match &action.default {
                Some(v) if *v != Value::None => v.clone(),
                _ => Value::List(Vec::new()),
            });
        }
        if arg_strings.len() == 1 && action.nargs == Nargs::One {
            let v = get_value(action, &arg_strings[0])?;
            check_value(action, &v)?;
            return Ok(v);
        }
        if action.nargs == Nargs::Parser {
            let values: Vec<Value> = arg_strings.iter().map(|s| Value::Str(s.clone())).collect();
            check_value(action, &values[0])?;
            return Ok(Value::List(values));
        }
        let mut values = Vec::with_capacity(arg_strings.len());
        for s in &arg_strings {
            values.push(get_value(action, s)?);
        }
        for v in &values {
            check_value(action, v)?;
        }
        Ok(Value::List(values))
    }

    /// `_match_argument`: the count of arg strings the action takes from the pattern.
    fn match_argument(&self, a: usize, pattern: &str) -> Result<usize, Flow> {
        let action = &self.actions[a];
        let re = regex::Regex::new(&format!("^{}", nargs_pattern(action)))
            .expect("nargs pattern compiles");
        match re.captures(pattern) {
            Some(c) => Ok(c.get(1).map_or(0, |m| m.len())),
            None => {
                let msg = match action.nargs {
                    Nargs::One => "expected one argument",
                    Nargs::OneOrMore => "expected at least one argument",
                    Nargs::Zero => "expected 0 arguments",
                    Nargs::ZeroOrMore | Nargs::Parser => "expected at least one argument",
                };
                Err(arg_error(action, msg))
            }
        }
    }

    /// `_match_arguments_partial`. Python's drop of trailing zero counts before an `O` is left
    /// out: sm's only positional that can match zero strings (`put` body) is alone in its parser,
    /// and it matches at least one string wherever this runs.
    fn match_arguments_partial(&self, actions: &[usize], pattern: &str) -> Vec<usize> {
        for i in (1..=actions.len()).rev() {
            let joined: String = actions[..i]
                .iter()
                .map(|&a| nargs_pattern(&self.actions[a]))
                .collect();
            let re = regex::Regex::new(&format!("^{joined}")).expect("nargs pattern compiles");
            if let Some(c) = re.captures(pattern) {
                return (1..c.len())
                    .map(|g| c.get(g).map_or(0, |m| m.len()))
                    .collect();
            }
        }
        Vec::new()
    }

    /// `_parse_optional`: None for a positional, else the option tuples (action None when unknown).
    fn parse_optional(&self, arg: &str) -> Option<Vec<OptTuple>> {
        if !arg.starts_with('-') {
            return None;
        }
        if let Some(a) = self.action_index(arg) {
            return Some(vec![OptTuple {
                action: Some(a),
                option_string: arg.to_string(),
                sep: None,
                explicit_arg: None,
            }]);
        }
        if arg.chars().count() == 1 {
            return None;
        }
        if let Some((opt, explicit)) = arg.split_once('=')
            && let Some(a) = self.action_index(opt)
        {
            return Some(vec![OptTuple {
                action: Some(a),
                option_string: opt.to_string(),
                sep: Some("=".into()),
                explicit_arg: Some(explicit.to_string()),
            }]);
        }
        let tuples = self.get_option_tuples(arg);
        if !tuples.is_empty() {
            return Some(tuples);
        }
        if looks_negative_number(arg) {
            return None;
        }
        if arg.contains(' ') {
            return None;
        }
        Some(vec![OptTuple {
            action: None,
            option_string: arg.to_string(),
            sep: None,
            explicit_arg: None,
        }])
    }

    /// `_get_option_tuples` with `allow_abbrev=True`.
    fn get_option_tuples(&self, arg: &str) -> Vec<OptTuple> {
        let mut result = Vec::new();
        let (prefix, sep, explicit) = match arg.split_once('=') {
            Some((p, e)) => (p, Some("=".to_string()), Some(e.to_string())),
            None => (arg, None, None),
        };
        if arg.chars().nth(1) == Some('-') {
            for (s, a) in &self.option_strings {
                if s.starts_with(prefix) {
                    result.push(OptTuple {
                        action: Some(*a),
                        option_string: s.clone(),
                        sep: sep.clone(),
                        explicit_arg: explicit.clone(),
                    });
                }
            }
        } else {
            let split = arg.char_indices().nth(2).map_or(arg.len(), |(i, _)| i);
            let (short_prefix, short_explicit) = arg.split_at(split);
            for (s, a) in &self.option_strings {
                if s == short_prefix {
                    result.push(OptTuple {
                        action: Some(*a),
                        option_string: s.clone(),
                        sep: Some(String::new()),
                        explicit_arg: Some(short_explicit.to_string()),
                    });
                } else if s.starts_with(prefix) {
                    result.push(OptTuple {
                        action: Some(*a),
                        option_string: s.clone(),
                        sep: sep.clone(),
                        explicit_arg: explicit.clone(),
                    });
                }
            }
        }
        result
    }

    /// The usage line, close to argparse's (not compared).
    pub fn format_usage(&self) -> String {
        let mut parts = vec![format!("usage: {}", self.prog)];
        let mut in_group: HashMap<usize, usize> = HashMap::new();
        for (g, members) in self.mutex_groups.iter().enumerate() {
            for m in members {
                in_group.insert(*m, g);
            }
        }
        let mut done_groups = HashSet::new();
        for (i, a) in self
            .actions
            .iter()
            .enumerate()
            .filter(|(_, a)| !a.option_strings.is_empty())
        {
            if let Some(&g) = in_group.get(&i) {
                if done_groups.insert(g) {
                    let inner: Vec<String> = self.mutex_groups[g]
                        .iter()
                        .map(|&m| option_usage(&self.actions[m]))
                        .collect();
                    parts.push(format!("[{}]", inner.join(" | ")));
                }
                continue;
            }
            parts.push(format!("[{}]", option_usage(a)));
        }
        for a in self.actions.iter().filter(|a| a.option_strings.is_empty()) {
            parts.push(format_args_usage(a));
        }
        format!("{}\n", parts.join(" "))
    }

    /// The help text, close to argparse's (not compared).
    pub fn format_help(&self) -> String {
        let mut text = self.format_usage();
        if !self.description.is_empty() {
            text.push_str(&format!("\n{}\n", self.description));
        }
        let positionals: Vec<&Action> = self
            .actions
            .iter()
            .filter(|a| a.option_strings.is_empty())
            .collect();
        if !positionals.is_empty() {
            text.push_str("\npositional arguments:\n");
            for a in positionals {
                match &a.kind {
                    Kind::Parsers(parsers) => {
                        text.push_str(&format!("  {}\n", choices_metavar(a)));
                        for (name, help, _) in parsers {
                            text.push_str(&help_row(&format!("  {name}"), help));
                        }
                    }
                    _ => text.push_str(&help_row(&format!("  {}", metavar_positional(a)), &a.help)),
                }
            }
        }
        text.push_str("\noptions:\n");
        for a in self.actions.iter().filter(|a| !a.option_strings.is_empty()) {
            let names: Vec<String> = a
                .option_strings
                .iter()
                .map(|s| {
                    if a.nargs == Nargs::One {
                        format!("{s} {}", metavar_optional(a))
                    } else {
                        s.clone()
                    }
                })
                .collect();
            text.push_str(&help_row(&format!("  {}", names.join(", ")), &a.help));
        }
        text
    }
}

/// The subparsers of one parser, built before they are attached with `Parser::add_subparsers`.
pub struct Subparsers {
    prog_prefix: String,
    dest: String,
    required: bool,
    parsers: Vec<(String, String, Parser)>,
}

impl Subparsers {
    /// `parent.add_subparsers(dest=dest, required=required)`.
    pub fn new(parent: &Parser, dest: &str, required: bool) -> Subparsers {
        let mut prog_prefix = parent.prog.clone();
        for a in parent
            .actions
            .iter()
            .filter(|a| a.option_strings.is_empty())
        {
            prog_prefix.push(' ');
            prog_prefix.push_str(&format_args_usage(a));
        }
        Subparsers {
            prog_prefix,
            dest: dest.to_string(),
            required,
            parsers: Vec::new(),
        }
    }

    /// `add_parser(name, help=help)`.
    pub fn add_parser(&mut self, name: &str, help: &str) -> &mut Parser {
        let parser = Parser::new(&format!("{} {name}", self.prog_prefix), "");
        self.parsers
            .push((name.to_string(), help.to_string(), parser));
        &mut self.parsers.last_mut().expect("just pushed").2
    }
}

struct State {
    seen: HashSet<usize>,
    seen_non_default: HashSet<usize>,
    conflicts: HashMap<usize, Vec<usize>>,
    sub_extras: Vec<String>,
}

fn infer_dest(opts: &[String]) -> String {
    let mut dest = String::new();
    let mut priority = 0;
    for s in opts {
        if s.chars().count() <= 2 {
            if priority < 1 {
                dest = s.trim_start_matches('-').to_string();
                priority = 1;
            }
        } else if s.chars().nth(1) != Some('-') {
            if priority < 2 {
                dest = s.trim_start_matches('-').to_string();
                priority = 2;
            }
        } else {
            dest = s.trim_start_matches('-').to_string();
            break;
        }
    }
    dest.replace('-', "_")
}

fn nargs_pattern(a: &Action) -> &'static str {
    let opt = !a.option_strings.is_empty();
    match (a.nargs, opt) {
        (Nargs::One, true) => "([A])",
        (Nargs::One, false) => "(-*A-*)",
        (Nargs::ZeroOrMore, true) => "(A*)",
        (Nargs::ZeroOrMore, false) => "(-*[A-]*)",
        (Nargs::OneOrMore, true) => "(A+)",
        (Nargs::OneOrMore, false) => "(-*A[A-]*)",
        (Nargs::Parser, true) => "(A[AO]*)",
        (Nargs::Parser, false) => "(-*A[-AO]*)",
        (Nargs::Zero, true) => "()",
        (Nargs::Zero, false) => "((?:-*A){0}-*)",
    }
}

/// `_get_action_name`.
fn action_name(a: &Action) -> Option<String> {
    if !a.option_strings.is_empty() {
        Some(a.option_strings.join("/"))
    } else if let Some(m) = &a.metavar {
        Some(m.clone())
    } else if let Some(d) = &a.dest {
        Some(d.clone())
    } else {
        a.choices.as_ref().map(|c| format!("{{{}}}", c.join(",")))
    }
}

fn arg_error(a: &Action, msg: &str) -> Flow {
    match action_name(a) {
        Some(name) => Flow::Error(format!("argument {name}: {msg}")),
        None => Flow::Error(msg.to_string()),
    }
}

fn get_value(action: &Action, s: &str) -> Result<Value, Flow> {
    match action.conv {
        Conv::Str => Ok(Value::Str(s.to_string())),
        Conv::Int => crate::py::int::from_str(s)
            .map(|i| match i {
                crate::py::int::Int::I64(n) => Value::Int(n),
                crate::py::int::Int::Big(t) => Value::BigInt(t),
            })
            .map_err(|_| arg_error(action, &format!("invalid int value: {}", repr(s)))),
        Conv::Float => crate::py::float::from_str(s)
            .map(Value::Float)
            .map_err(|_| arg_error(action, &format!("invalid float value: {}", repr(s)))),
    }
}

/// `_check_value`, with `suggest_on_error=True` (the 3.15 default).
fn check_value(action: &Action, value: &Value) -> Result<(), Flow> {
    let Some(choices) = &action.choices else {
        return Ok(());
    };
    let Value::Str(v) = value else {
        unreachable!("sm's choices are all str-typed arguments");
    };
    if choices.iter().any(|c| c == v) {
        return Ok(());
    }
    let listed: Vec<String> = choices.iter().map(|c| repr(c)).collect();
    let msg = match get_close_match(v, choices) {
        Some(closest) => format!(
            "invalid choice: {}, maybe you meant {}? (choose from {})",
            repr(v),
            repr(&closest),
            listed.join(", ")
        ),
        None => format!(
            "invalid choice: {} (choose from {})",
            repr(v),
            listed.join(", ")
        ),
    };
    Err(arg_error(action, &msg))
}

/// `re.match(r'-\.?\d', s)`: `\d` is any Unicode decimal digit.
fn looks_negative_number(s: &str) -> bool {
    let mut it = s.chars();
    if it.next() != Some('-') {
        return false;
    }
    match it.next() {
        Some('.') => it.next().is_some_and(crate::py::str::isdecimal),
        Some(c) => crate::py::str::isdecimal(c),
        None => false,
    }
}

/// `difflib.get_close_matches(word, possibilities, n=1, cutoff=0.6)`.
fn get_close_match(word: &str, possibilities: &[String]) -> Option<String> {
    let b: Vec<char> = word.chars().collect();
    let b2j = chain_b(&b);
    let mut best: Option<(f64, &String)> = None;
    for x in possibilities {
        let a: Vec<char> = x.chars().collect();
        let total = a.len() + b.len();
        let matches = matching_total(&a, &b, &b2j);
        let ratio = if total > 0 {
            2.0 * matches as f64 / total as f64
        } else {
            1.0
        };
        if ratio >= 0.6 {
            let better = match best {
                None => true,
                Some((r, bx)) => ratio > r || (ratio == r && x > bx),
            };
            if better {
                best = Some((ratio, x));
            }
        }
    }
    best.map(|(_, x)| x.clone())
}

/// `SequenceMatcher.__chain_b` with `isjunk=None`. Autojunk is left out: it acts only on a word
/// of 200+ chars, which never reaches the 0.6 cutoff against sm's choices of at most 11 chars.
fn chain_b(b: &[char]) -> HashMap<char, Vec<usize>> {
    let mut b2j: HashMap<char, Vec<usize>> = HashMap::new();
    for (i, &c) in b.iter().enumerate() {
        b2j.entry(c).or_default().push(i);
    }
    b2j
}

/// The sum of the block sizes `SequenceMatcher.get_matching_blocks` finds.
fn matching_total(a: &[char], b: &[char], b2j: &HashMap<char, Vec<usize>>) -> usize {
    let mut total = 0;
    let mut queue = vec![(0, a.len(), 0, b.len())];
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        let (i, j, k) = find_longest_match(a, b, b2j, alo, ahi, blo, bhi);
        if k > 0 {
            total += k;
            if alo < i && blo < j {
                queue.push((alo, i, blo, j));
            }
            if i + k < ahi && j + k < bhi {
                queue.push((i + k, ahi, j + k, bhi));
            }
        }
    }
    total
}

/// `SequenceMatcher.find_longest_match` with no junk (popular chars are only left out of b2j).
fn find_longest_match(
    a: &[char],
    b: &[char],
    b2j: &HashMap<char, Vec<usize>>,
    alo: usize,
    ahi: usize,
    blo: usize,
    bhi: usize,
) -> (usize, usize, usize) {
    let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
    let mut j2len: HashMap<usize, usize> = HashMap::new();
    for (i, ai) in a.iter().enumerate().take(ahi).skip(alo) {
        let mut new: HashMap<usize, usize> = HashMap::new();
        if let Some(js) = b2j.get(ai) {
            for &j in js {
                if j < blo {
                    continue;
                }
                if j >= bhi {
                    break;
                }
                let k = j
                    .checked_sub(1)
                    .and_then(|p| j2len.get(&p))
                    .copied()
                    .unwrap_or(0)
                    + 1;
                new.insert(j, k);
                if k > bestsize {
                    besti = i + 1 - k;
                    bestj = j + 1 - k;
                    bestsize = k;
                }
            }
        }
        j2len = new;
    }
    while besti > alo && bestj > blo && a[besti - 1] == b[bestj - 1] {
        besti -= 1;
        bestj -= 1;
        bestsize += 1;
    }
    while besti + bestsize < ahi
        && bestj + bestsize < bhi
        && a[besti + bestsize] == b[bestj + bestsize]
    {
        bestsize += 1;
    }
    (besti, bestj, bestsize)
}

fn metavar_optional(a: &Action) -> String {
    if let Some(m) = &a.metavar {
        return m.clone();
    }
    if a.choices.is_some() {
        return choices_metavar(a);
    }
    a.dest.as_deref().unwrap_or_default().to_uppercase()
}

fn metavar_positional(a: &Action) -> String {
    if let Some(m) = &a.metavar {
        return m.clone();
    }
    if a.choices.is_some() {
        return choices_metavar(a);
    }
    a.dest.clone().unwrap_or_default()
}

fn choices_metavar(a: &Action) -> String {
    format!("{{{}}}", a.choices.as_deref().unwrap_or_default().join(","))
}

fn option_usage(a: &Action) -> String {
    let s = &a.option_strings[0];
    match a.nargs {
        Nargs::One => format!("{s} {}", metavar_optional(a)),
        _ => s.clone(),
    }
}

fn format_args_usage(a: &Action) -> String {
    let m = metavar_positional(a);
    match a.nargs {
        Nargs::One => m,
        Nargs::ZeroOrMore => format!("[{m} ...]"),
        Nargs::OneOrMore => format!("{m} [{m} ...]"),
        Nargs::Parser => format!("{m} ..."),
        Nargs::Zero => String::new(),
    }
}

fn help_row(left: &str, help: &str) -> String {
    if help.is_empty() {
        return format!("{left}\n");
    }
    if left.chars().count() <= 22 {
        format!("{left:<24}{help}\n")
    } else {
        format!("{left}\n{:24}{help}\n", "")
    }
}

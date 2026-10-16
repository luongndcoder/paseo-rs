//! A small, declarative port of the zod 4 behavior the tool input schemas rely on.
//!
//! The TS tools validate with zod. Models and clients see the resulting messages verbatim
//! (`MCP error -32602: Input validation error: Invalid arguments for tool <name>: <issues>`), and
//! handlers see the parsed value (trim, defaults, strip). The tool `inputSchema` JSON served by
//! `tools/list` is a golden snapshot and never derived from this module; this module only
//! decides accept/reject, the parsed value and the issue list. `tests/validation_vectors.rs`
//! replays about 2.6k inputs captured from the real zod schemas.
//!
//! Issue key order follows zod 4.4.3 (`invalid_type` is `expected, code, path, message`, ...).

use regex::Regex;
use serde_json::{json, Map, Value};

/// Largest integer JS represents exactly; `z.number().int()` is `safeint`.
const MAX_SAFE_INT: f64 = 9_007_199_254_740_991.0;

#[derive(Debug, Clone, PartialEq)]
pub enum Seg {
    Key(String),
    Idx(usize),
}

impl Seg {
    fn to_value(&self) -> Value {
        match self {
            Seg::Key(k) => Value::String(k.clone()),
            Seg::Idx(i) => json!(i),
        }
    }
}

/// One zod issue. `fatal` mirrors zod's "aborted" notion (`continue !== true`): an object level
/// refinement is skipped when a field produced a fatal issue.
#[derive(Debug, Clone)]
pub struct Issue {
    pub value: Value,
    pub fatal: bool,
}

type Issues = Vec<Issue>;

fn path_value(path: &[Seg]) -> Value {
    Value::Array(path.iter().map(Seg::to_value).collect())
}

fn issue(fatal: bool, entries: Vec<(&str, Value)>) -> Issue {
    let mut map = Map::new();
    for (k, v) in entries {
        map.insert(k.to_owned(), v);
    }
    Issue {
        value: Value::Object(map),
        fatal,
    }
}

/// `typeof`-ish name zod prints in "received X".
fn received(value: Option<&Value>) -> &'static str {
    match value {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

fn invalid_type(
    expected: &str,
    value: Option<&Value>,
    path: &[Seg],
    custom_message: Option<&str>,
) -> Issue {
    let message = custom_message.map(str::to_owned).unwrap_or_else(|| {
        format!(
            "Invalid input: expected {expected}, received {}",
            received(value)
        )
    });
    issue(
        true,
        vec![
            ("expected", json!(expected)),
            ("code", json!("invalid_type")),
            ("path", path_value(path)),
            ("message", json!(message)),
        ],
    )
}

fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// `String.prototype.trim` strips Unicode White_Space plus the BOM; Rust's `trim` strips
/// White_Space only, so the BOM is handled explicitly.
fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

#[derive(Clone)]
pub enum StrCheck {
    Min(usize, Option<String>),
    Max(usize, Option<String>),
    Regex {
        re: Regex,
        /// `String(regex)` as zod prints it, flags included.
        source: String,
        message: Option<String>,
    },
    /// `.refine(fn, {message})`: a failing predicate adds a `custom` issue.
    Refine(fn(&str) -> bool, String),
}

#[derive(Clone, Default)]
pub struct Str {
    pub trim: bool,
    /// `z.string({ error: () => msg })`: replaces the message of the type error.
    pub type_message: Option<String>,
    pub checks: Vec<StrCheck>,
}

#[derive(Clone, Default)]
pub struct Num {
    pub int: bool,
    pub positive: bool,
    pub max: Option<f64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Unknown {
    Strip,
    Passthrough,
    Strict,
}

pub type ObjRefine = (fn(&Map<String, Value>) -> bool, &'static str);

#[derive(Clone)]
pub struct Obj {
    pub fields: Vec<(&'static str, Z)>,
    pub unknown: Unknown,
    pub refines: Vec<ObjRefine>,
}

#[derive(Clone)]
pub enum Z {
    Str(Str),
    Num(Num),
    Bool,
    Unknown,
    Enum(&'static [&'static str]),
    Literal(&'static str),
    Array {
        item: Box<Z>,
        min: Option<usize>,
    },
    /// `z.record(z.string(), value)`.
    Record(Box<Z>),
    Obj(Obj),
    Optional(Box<Z>),
    Nullable(Box<Z>),
    /// `.optional().default(v)`: an absent value becomes `v`, the default is not re-parsed.
    Default(Box<Z>, Value),
    /// `z.discriminatedUnion(key, objects)`.
    DUnion {
        key: &'static str,
        options: Vec<Obj>,
    },
    /// `BrowserHttpUrlInputSchema`: trim, then normalize to an http(s) URL or fail with `message`.
    HttpUrl,
}

// ---- builders --------------------------------------------------------------------------

impl Str {
    pub fn trim(mut self) -> Self {
        self.trim = true;
        self
    }
    pub fn min(mut self, n: usize) -> Self {
        self.checks.push(StrCheck::Min(n, None));
        self
    }
    pub fn min_msg(mut self, n: usize, msg: &str) -> Self {
        self.checks.push(StrCheck::Min(n, Some(msg.to_owned())));
        self
    }
    pub fn max_msg(mut self, n: usize, msg: &str) -> Self {
        self.checks.push(StrCheck::Max(n, Some(msg.to_owned())));
        self
    }
    pub fn regex(mut self, re: &str, source: &str, message: Option<&str>) -> Self {
        self.checks.push(StrCheck::Regex {
            re: Regex::new(re).expect("static regex"),
            source: source.to_owned(),
            message: message.map(str::to_owned),
        });
        self
    }
    pub fn refine(mut self, check: fn(&str) -> bool, message: &str) -> Self {
        self.checks
            .push(StrCheck::Refine(check, message.to_owned()));
        self
    }
    pub fn type_message(mut self, msg: &str) -> Self {
        self.type_message = Some(msg.to_owned());
        self
    }
    pub fn z(self) -> Z {
        Z::Str(self)
    }
}

pub fn string() -> Str {
    Str::default()
}

pub fn str_z() -> Z {
    Z::Str(Str::default())
}

pub fn boolean() -> Z {
    Z::Bool
}

pub fn number() -> Z {
    Z::Num(Num::default())
}

/// `z.number().int().positive()` with an optional `.max(n)`.
pub fn pos_int(max: Option<f64>) -> Z {
    Z::Num(Num {
        int: true,
        positive: true,
        max,
    })
}

pub fn opt(z: Z) -> Z {
    Z::Optional(Box::new(z))
}

pub fn nullable(z: Z) -> Z {
    Z::Nullable(Box::new(z))
}

pub fn default_to(z: Z, value: Value) -> Z {
    Z::Default(Box::new(z), value)
}

pub fn array(item: Z) -> Z {
    Z::Array {
        item: Box::new(item),
        min: None,
    }
}

pub fn array_min(item: Z, min: usize) -> Z {
    Z::Array {
        item: Box::new(item),
        min: Some(min),
    }
}

pub fn record(value: Z) -> Z {
    Z::Record(Box::new(value))
}

pub fn obj(fields: Vec<(&'static str, Z)>) -> Obj {
    Obj {
        fields,
        unknown: Unknown::Strip,
        refines: Vec::new(),
    }
}

impl Obj {
    pub fn strict(mut self) -> Self {
        self.unknown = Unknown::Strict;
        self
    }
    pub fn passthrough(mut self) -> Self {
        self.unknown = Unknown::Passthrough;
        self
    }
    pub fn refine(mut self, check: fn(&Map<String, Value>) -> bool, message: &'static str) -> Self {
        self.refines.push((check, message));
        self
    }
    pub fn z(self) -> Z {
        Z::Obj(self)
    }
}

// ---- parsing ---------------------------------------------------------------------------

fn as_f64(value: &Value) -> f64 {
    value.as_f64().unwrap_or(f64::NAN)
}

fn fmt_js_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

fn length_issue(code: &str, bound: &str, n: usize, path: &[Seg], message: String) -> Issue {
    issue(
        false,
        vec![
            ("origin", json!("array")),
            ("code", json!(code)),
            (bound, json!(n)),
            ("inclusive", json!(true)),
            ("path", path_value(path)),
            ("message", json!(message)),
        ],
    )
}

fn parse_str(spec: &Str, input: Option<&Value>, path: &[Seg]) -> (Option<Value>, Issues) {
    let Some(Value::String(raw)) = input else {
        let mut issues = vec![invalid_type(
            "string",
            input,
            path,
            spec.type_message.as_deref(),
        )];
        // zod's length checks carry a `when` guard that lets them run on any value with a
        // `length`, so an array given for a string is also measured (`origin: "array"`).
        if let Some(Value::Array(items)) = input {
            for check in &spec.checks {
                match check {
                    StrCheck::Min(n, msg) if items.len() < *n => {
                        let message = msg.clone().unwrap_or_else(|| {
                            format!("Too small: expected array to have >={n} items")
                        });
                        issues.push(length_issue("too_small", "minimum", *n, path, message));
                    }
                    StrCheck::Max(n, msg) if items.len() > *n => {
                        let message = msg.clone().unwrap_or_else(|| {
                            format!("Too big: expected array to have <={n} items")
                        });
                        issues.push(length_issue("too_big", "maximum", *n, path, message));
                    }
                    _ => {}
                }
            }
        }
        return (None, issues);
    };
    let text: &str = if spec.trim { js_trim(raw) } else { raw };
    let mut issues = Vec::new();
    for check in &spec.checks {
        match check {
            StrCheck::Min(n, msg) => {
                if utf16_len(text) < *n {
                    let message = msg.clone().unwrap_or_else(|| {
                        format!("Too small: expected string to have >={n} characters")
                    });
                    issues.push(issue(
                        false,
                        vec![
                            ("origin", json!("string")),
                            ("code", json!("too_small")),
                            ("minimum", json!(n)),
                            ("inclusive", json!(true)),
                            ("path", path_value(path)),
                            ("message", json!(message)),
                        ],
                    ));
                }
            }
            StrCheck::Max(n, msg) => {
                if utf16_len(text) > *n {
                    let message = msg.clone().unwrap_or_else(|| {
                        format!("Too big: expected string to have <={n} characters")
                    });
                    issues.push(issue(
                        false,
                        vec![
                            ("origin", json!("string")),
                            ("code", json!("too_big")),
                            ("maximum", json!(n)),
                            ("inclusive", json!(true)),
                            ("path", path_value(path)),
                            ("message", json!(message)),
                        ],
                    ));
                }
            }
            StrCheck::Regex {
                re,
                source,
                message,
            } => {
                if !re.is_match(text) {
                    let message = message
                        .clone()
                        .unwrap_or_else(|| format!("Invalid string: must match pattern {source}"));
                    issues.push(issue(
                        false,
                        vec![
                            ("origin", json!("string")),
                            ("code", json!("invalid_format")),
                            ("format", json!("regex")),
                            ("pattern", json!(source)),
                            ("path", path_value(path)),
                            ("message", json!(message)),
                        ],
                    ));
                }
            }
            StrCheck::Refine(check, message) => {
                if !check(text) {
                    issues.push(issue(
                        false,
                        vec![
                            ("code", json!("custom")),
                            ("path", path_value(path)),
                            ("message", json!(message)),
                        ],
                    ));
                }
            }
        }
    }
    (Some(Value::String(text.to_owned())), issues)
}

fn parse_num(spec: &Num, input: Option<&Value>, path: &[Seg]) -> (Option<Value>, Issues) {
    let Some(Value::Number(n)) = input else {
        return (None, vec![invalid_type("number", input, path, None)]);
    };
    let value = as_f64(&Value::Number(n.clone()));
    let mut issues = Vec::new();
    if spec.int {
        if value.fract() != 0.0 {
            // An aborting issue: the remaining checks do not run.
            return (
                None,
                vec![issue(
                    true,
                    vec![
                        ("expected", json!("int")),
                        ("format", json!("safeint")),
                        ("code", json!("invalid_type")),
                        ("path", path_value(path)),
                        (
                            "message",
                            json!("Invalid input: expected int, received number"),
                        ),
                    ],
                )],
            );
        }
        const NOTE: &str = "Integers must be within the safe integer range.";
        if value > MAX_SAFE_INT {
            issues.push(issue(
                false,
                vec![
                    ("code", json!("too_big")),
                    ("maximum", json!(9_007_199_254_740_991_u64)),
                    ("note", json!(NOTE)),
                    ("origin", json!("int")),
                    ("inclusive", json!(true)),
                    ("path", path_value(path)),
                    (
                        "message",
                        json!("Too big: expected int to be <=9007199254740991"),
                    ),
                ],
            ));
        } else if value < -MAX_SAFE_INT {
            issues.push(issue(
                false,
                vec![
                    ("code", json!("too_small")),
                    ("minimum", json!(-9_007_199_254_740_991_i64)),
                    ("note", json!(NOTE)),
                    ("origin", json!("int")),
                    ("inclusive", json!(true)),
                    ("path", path_value(path)),
                    (
                        "message",
                        json!("Too small: expected int to be >=-9007199254740991"),
                    ),
                ],
            ));
        }
    }
    if spec.positive && value <= 0.0 {
        issues.push(issue(
            false,
            vec![
                ("origin", json!("number")),
                ("code", json!("too_small")),
                ("minimum", json!(0)),
                ("inclusive", json!(false)),
                ("path", path_value(path)),
                ("message", json!("Too small: expected number to be >0")),
            ],
        ));
    }
    if let Some(max) = spec.max {
        if value > max {
            let max_json = if max.fract() == 0.0 {
                json!(max as i64)
            } else {
                json!(max)
            };
            issues.push(issue(
                false,
                vec![
                    ("origin", json!("number")),
                    ("code", json!("too_big")),
                    ("maximum", max_json),
                    ("inclusive", json!(true)),
                    ("path", path_value(path)),
                    (
                        "message",
                        json!(format!(
                            "Too big: expected number to be <={}",
                            fmt_js_number(max)
                        )),
                    ),
                ],
            ));
        }
    }
    (Some(Value::Number(n.clone())), issues)
}

fn quote_list(values: &[&str], sep: &str, quote: char) -> String {
    values
        .iter()
        .map(|v| format!("{quote}{v}{quote}"))
        .collect::<Vec<_>>()
        .join(sep)
}

fn parse_obj(spec: &Obj, input: Option<&Value>, path: &[Seg]) -> (Option<Value>, Issues) {
    let Some(Value::Object(map)) = input else {
        return (None, vec![invalid_type("object", input, path, None)]);
    };
    let mut out = Map::new();
    let mut issues: Issues = Vec::new();
    for (key, schema) in &spec.fields {
        let mut sub_path = path.to_vec();
        sub_path.push(Seg::Key((*key).to_owned()));
        let (value, mut sub) = schema.parse(map.get(*key), &sub_path);
        issues.append(&mut sub);
        if let Some(value) = value {
            out.insert((*key).to_owned(), value);
        }
    }
    let unknown_keys: Vec<&String> = map
        .keys()
        .filter(|k| !spec.fields.iter().any(|(f, _)| f == k))
        .collect();
    match spec.unknown {
        Unknown::Strip => {}
        Unknown::Passthrough => {
            for key in &unknown_keys {
                out.insert((*key).clone(), map[*key].clone());
            }
        }
        Unknown::Strict => {
            if !unknown_keys.is_empty() {
                let list = unknown_keys
                    .iter()
                    .map(|k| format!("\"{k}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                let noun = if unknown_keys.len() == 1 {
                    "key"
                } else {
                    "keys"
                };
                issues.push(issue(
                    true,
                    vec![
                        ("code", json!("unrecognized_keys")),
                        (
                            "keys",
                            Value::Array(unknown_keys.iter().map(|k| json!(k)).collect()),
                        ),
                        ("path", path_value(path)),
                        ("message", json!(format!("Unrecognized {noun}: {list}"))),
                    ],
                ));
            }
        }
    }
    if !issues.iter().any(|i| i.fatal) {
        for (check, message) in &spec.refines {
            if !check(&out) {
                issues.push(issue(
                    false,
                    vec![
                        ("code", json!("custom")),
                        ("path", path_value(path)),
                        ("message", json!(message)),
                    ],
                ));
            }
        }
    }
    (Some(Value::Object(out)), issues)
}

/// `normalizeHttpUrlInput` of `browser-tools/tools.ts`: an http(s) URL, a scheme-less host that
/// becomes `http://<host>`, or nothing.
fn normalize_http_url_input(value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return is_valid_http_url(value).then(|| value.to_owned());
    }
    let has_whitespace = value.chars().any(|c| c.is_whitespace() || c == '\u{feff}');
    // `^(?!https?:\/\/)[a-z][a-z0-9+.-]*:\/\//i`
    let explicit_scheme = {
        let mut chars = value.char_indices();
        match chars.next() {
            Some((_, c)) if c.is_ascii_alphabetic() => {
                let mut end = None;
                for (i, c) in chars {
                    if c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-') {
                        continue;
                    }
                    end = Some(i);
                    break;
                }
                end.is_some_and(|i| value[i..].starts_with("://"))
            }
            _ => false,
        }
    };
    if has_whitespace || explicit_scheme {
        return None;
    }
    let normalized = format!("http://{value}");
    is_valid_http_url(&normalized).then_some(normalized)
}

/// `new URL(value)` with an http or https protocol. Covers what the WHATWG parser rejects for
/// these inputs: an empty or invalid host and a bad port.
fn is_valid_http_url(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let rest = if let Some(r) = lower.strip_prefix("http://") {
        r.to_owned()
    } else if let Some(r) = lower.strip_prefix("https://") {
        r.to_owned()
    } else {
        return false;
    };
    // The WHATWG parser treats `\` like `/` in special schemes and skips extra slashes after
    // the scheme (`http:///host` is `http://host/`).
    let rest = rest.replace('\\', "/");
    let rest = rest.trim_start_matches('/').to_owned();
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = if let Some(stripped) = host_port.strip_prefix('[') {
        match stripped.split_once(']') {
            Some((h, tail)) => (format!("[{h}]"), tail.strip_prefix(':').map(str::to_owned)),
            None => return false,
        }
    } else {
        match host_port.rsplit_once(':') {
            Some((h, p)) => (h.to_owned(), Some(p.to_owned())),
            None => (host_port.to_owned(), None),
        }
    };
    if host.is_empty() {
        return false;
    }
    if let Some(port) = port {
        if !port.is_empty() {
            match port.parse::<u32>() {
                Ok(p) if p <= 65535 && port.bytes().all(|b| b.is_ascii_digit()) => {}
                _ => return false,
            }
        }
    }
    const FORBIDDEN: &[char] = &[
        ' ', '#', '%', '/', ':', '<', '>', '?', '@', '[', '\\', ']', '^', '|', '\t', '\n', '\r',
    ];
    if !host.starts_with('[')
        && host
            .chars()
            .any(|c| FORBIDDEN.contains(&c) || c.is_control())
    {
        return false;
    }
    true
}

impl Z {
    /// Parses `input` (`None` is JS `undefined`). Returns the parsed value (`None` is
    /// `undefined`, the key is omitted) and the issues.
    pub fn parse(&self, input: Option<&Value>, path: &[Seg]) -> (Option<Value>, Issues) {
        match self {
            Z::Str(spec) => parse_str(spec, input, path),
            Z::Num(spec) => parse_num(spec, input, path),
            Z::Bool => match input {
                Some(Value::Bool(b)) => (Some(Value::Bool(*b)), vec![]),
                _ => (None, vec![invalid_type("boolean", input, path, None)]),
            },
            Z::Unknown => (input.cloned(), vec![]),
            Z::Enum(values) => match input {
                Some(Value::String(s)) if values.contains(&s.as_str()) => {
                    (Some(Value::String(s.clone())), vec![])
                }
                _ => (
                    None,
                    vec![issue(
                        true,
                        vec![
                            ("code", json!("invalid_value")),
                            ("values", json!(values)),
                            ("path", path_value(path)),
                            (
                                "message",
                                json!(format!(
                                    "Invalid option: expected one of {}",
                                    quote_list(values, "|", '"')
                                )),
                            ),
                        ],
                    )],
                ),
            },
            Z::Literal(expected) => match input {
                Some(Value::String(s)) if s == expected => (Some(Value::String(s.clone())), vec![]),
                _ => (
                    None,
                    vec![issue(
                        true,
                        vec![
                            ("code", json!("invalid_value")),
                            ("values", json!([expected])),
                            ("path", path_value(path)),
                            (
                                "message",
                                json!(format!("Invalid input: expected \"{expected}\"")),
                            ),
                        ],
                    )],
                ),
            },
            Z::Array { item, min } => {
                let Some(Value::Array(items)) = input else {
                    return (None, vec![invalid_type("array", input, path, None)]);
                };
                let mut out = Vec::new();
                let mut issues = Vec::new();
                for (idx, element) in items.iter().enumerate() {
                    let mut sub_path = path.to_vec();
                    sub_path.push(Seg::Idx(idx));
                    let (value, mut sub) = item.parse(Some(element), &sub_path);
                    issues.append(&mut sub);
                    if let Some(value) = value {
                        out.push(value);
                    }
                }
                if let Some(min) = min {
                    if items.len() < *min {
                        issues.push(issue(
                            false,
                            vec![
                                ("origin", json!("array")),
                                ("code", json!("too_small")),
                                ("minimum", json!(min)),
                                ("inclusive", json!(true)),
                                ("path", path_value(path)),
                                (
                                    "message",
                                    json!(format!(
                                        "Too small: expected array to have >={min} items"
                                    )),
                                ),
                            ],
                        ));
                    }
                }
                (Some(Value::Array(out)), issues)
            }
            Z::Record(value_schema) => {
                let Some(Value::Object(map)) = input else {
                    return (None, vec![invalid_type("record", input, path, None)]);
                };
                let mut out = Map::new();
                let mut issues = Vec::new();
                for (key, value) in map {
                    let mut sub_path = path.to_vec();
                    sub_path.push(Seg::Key(key.clone()));
                    let (parsed, mut sub) = value_schema.parse(Some(value), &sub_path);
                    issues.append(&mut sub);
                    if let Some(parsed) = parsed {
                        out.insert(key.clone(), parsed);
                    }
                }
                (Some(Value::Object(out)), issues)
            }
            Z::Obj(spec) => parse_obj(spec, input, path),
            Z::Optional(inner) => match input {
                None => (None, vec![]),
                Some(_) => inner.parse(input, path),
            },
            Z::Nullable(inner) => match input {
                Some(Value::Null) => (Some(Value::Null), vec![]),
                _ => inner.parse(input, path),
            },
            Z::Default(inner, default) => match input {
                None => (Some(default.clone()), vec![]),
                Some(_) => inner.parse(input, path),
            },
            Z::DUnion { key, options } => {
                let Some(Value::Object(map)) = input else {
                    return (
                        None,
                        vec![issue(
                            true,
                            vec![
                                ("code", json!("invalid_type")),
                                ("expected", json!("object")),
                                ("path", path_value(path)),
                                (
                                    "message",
                                    json!(format!(
                                        "Invalid input: expected object, received {}",
                                        received(input)
                                    )),
                                ),
                            ],
                        )],
                    );
                };
                let literal_of = |o: &Obj| {
                    o.fields.iter().find_map(|(name, z)| match z {
                        Z::Literal(l) if name == key => Some(*l),
                        _ => None,
                    })
                };
                let discriminator = map.get(*key).and_then(Value::as_str);
                let matched =
                    discriminator.and_then(|d| options.iter().find(|o| literal_of(o) == Some(d)));
                match matched {
                    Some(option) => parse_obj(option, input, path),
                    None => {
                        let names: Vec<&str> = options.iter().filter_map(literal_of).collect();
                        let mut sub_path = path.to_vec();
                        sub_path.push(Seg::Key((*key).to_owned()));
                        (
                            None,
                            vec![issue(
                                true,
                                vec![
                                    ("code", json!("invalid_union")),
                                    ("errors", json!([])),
                                    ("note", json!("No matching discriminator")),
                                    ("discriminator", json!(key)),
                                    ("options", json!(names)),
                                    ("path", path_value(&sub_path)),
                                    (
                                        "message",
                                        json!(format!(
                                            "Invalid discriminator value. Expected {}",
                                            quote_list(&names, " | ", '\'')
                                        )),
                                    ),
                                ],
                            )],
                        )
                    }
                }
            }
            Z::HttpUrl => {
                let Some(Value::String(raw)) = input else {
                    return (None, vec![invalid_type("string", input, path, None)]);
                };
                match normalize_http_url_input(js_trim(raw)) {
                    Some(url) => (Some(Value::String(url)), vec![]),
                    None => (
                        None,
                        vec![issue(
                            true,
                            vec![
                                ("code", json!("custom")),
                                ("message", json!("URL must use http/https only")),
                                ("path", path_value(path)),
                            ],
                        )],
                    ),
                }
            }
        }
    }
}

/// `ZodError.message`: the issues, `JSON.stringify(issues, null, 2)`.
pub fn issues_message(issues: &[Issue]) -> String {
    let list = Value::Array(issues.iter().map(|i| i.value.clone()).collect());
    serde_json::to_string_pretty(&list).unwrap_or_default()
}

/// A tool's `inputSchema`, in the two shapes the TS code registers.
///
/// A raw shape (`{ field: z.x() }`) is wrapped by the MCP SDK in a strip object and by the
/// catalog's own parse in a passthrough object, so unknown keys are dropped on the MCP path and
/// kept on the catalog path. A schema instance (`z.object(..).passthrough()`, the `browser_wait`
/// refine) is used as is by both.
#[derive(Clone)]
pub struct InputSchema {
    pub root: Obj,
    pub raw_shape: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsePass {
    /// `McpServer.validateToolInput`.
    Mcp,
    /// `parseToolInput` inside `executeTool`.
    Catalog,
}

impl InputSchema {
    pub fn raw(fields: Vec<(&'static str, Z)>) -> Self {
        Self {
            root: obj(fields),
            raw_shape: true,
        }
    }

    pub fn schema(root: Obj) -> Self {
        Self {
            root,
            raw_shape: false,
        }
    }

    /// Parses `input` (`None` is `undefined`).
    pub fn parse(&self, pass: ParsePass, input: Option<&Value>) -> Result<Value, Vec<Issue>> {
        let mut root = self.root.clone();
        if self.raw_shape {
            root.unknown = match pass {
                ParsePass::Mcp => Unknown::Strip,
                ParsePass::Catalog => Unknown::Passthrough,
            };
        }
        let (value, issues) = parse_obj(&root, input, &[]);
        if issues.is_empty() {
            Ok(value.unwrap_or(Value::Null))
        } else {
            Err(issues)
        }
    }
}

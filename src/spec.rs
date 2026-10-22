//! Check spec v1 (spec 15): data only. Lint rejects and never repairs; the runner interprets what passed.
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const NAME_MAX: usize = 60;
pub const WHY_MAX: usize = 200;
pub const STEPS_MAX: usize = 40;
pub const CMD_MAX: usize = 300;
pub const SPEC_BYTES_MAX: usize = 64 * 1024;
pub const READY_TIMEOUT_MAX: u64 = 90;
/// Set by Torpor; a spec that names them would undo the scratch isolation (spec 16).
const RESERVED_ENV: [&str; 4] = ["HOME", "TMPDIR", "PORT", "PATH"];
const UNVERIFIABLE_WORDS: [&str; 5] = ["oauth", "callback", "captcha", "checkout", "payment"];
const TOP_KEYS: [&str; 7] = ["v", "name", "why", "start", "env", "steps", "unverifiable"];
const LOC_HINT: &str = "Use role, label, text or testid.";

#[derive(Clone, Debug, PartialEq)]
pub enum Loc {
    Role(String, Option<String>),
    Label(String),
    Text(String),
    TextRe(String),
    TestId(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum UrlWant {
    Path(String),
    PathRe(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Count {
    N(u64),
    Range(Option<u64>, Option<u64>),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HttpExpect {
    pub status: Option<u16>,
    pub status_in: Vec<u16>,
    pub json_has: Vec<String>,
    pub json_type: Vec<(String, String)>,
    pub body_contains: Option<String>,
    /// shortcut: the spec does not say what length measures; it is the body size in bytes. Upgrade if the browser runner needs characters.
    pub length_range: Option<(u64, u64)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HttpStep {
    pub method: String,
    pub path: String,
    pub json: Option<Value>,
    pub expect: HttpExpect,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    Goto(String),
    Click(Loc),
    Fill(Loc, String),
    Press(Loc, String),
    Select(Loc, String),
    Check(Loc),
    Uncheck(Loc),
    WaitHttp(String),
    Visible(Loc),
    Hidden(Loc),
    Text(Loc, String),
    Url(UrlWant),
    Count(Loc, Count),
    ConsoleClean,
    Http(HttpStep),
}

impl Act {
    pub fn is_assertion(&self) -> bool {
        !matches!(self, Act::Goto(_) | Act::Click(_) | Act::Fill(..) | Act::Press(..) | Act::Select(..) | Act::Check(_) | Act::Uncheck(_) | Act::WaitHttp(_))
    }
    /// The plain-words name of the step kind (for failure texts).
    pub fn verb(&self) -> &'static str {
        match self {
            Act::Goto(_) => "goto",
            Act::Click(_) => "click",
            Act::Fill(..) => "fill",
            Act::Press(..) => "press",
            Act::Select(..) => "select",
            Act::Check(_) => "check",
            Act::Uncheck(_) => "uncheck",
            Act::WaitHttp(_) => "wait_http",
            Act::Visible(_) => "expect_visible",
            Act::Hidden(_) => "expect_hidden",
            Act::Text(..) => "expect_text",
            Act::Url(_) => "expect_url",
            Act::Count(..) => "expect_count",
            Act::ConsoleClean => "expect_console_clean",
            Act::Http(_) => "http",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub act: Act,
    pub why: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Start {
    pub cmd: String,
    pub ready: String,
    pub timeout_s: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub name: String,
    pub why: String,
    pub start: Start,
    pub env: BTreeMap<String, String>,
    pub steps: Vec<Step>,
    pub unverifiable: bool,
}

impl Spec {
    /// True when every step runs in Rust (no browser needed).
    pub fn http_only(&self) -> bool {
        self.steps.iter().all(|s| matches!(s.act, Act::Http(_) | Act::WaitHttp(_)))
    }
    pub fn assertions(&self) -> usize {
        self.steps.iter().filter(|s| s.act.is_assertion()).count()
    }
    /// Distinct goto paths in order (the warm-up list).
    pub fn goto_paths(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for s in &self.steps {
            if let Act::Goto(p) = &s.act {
                if !v.contains(p) {
                    v.push(p.clone());
                }
            }
        }
        v
    }
}

/// A spec that passed lint, with its canonical bytes and id.
#[derive(Clone, Debug)]
pub struct Linted {
    pub spec: Spec,
    /// Canonical JSON: sorted keys, compact, `unverifiable` and `timeout_s` filled in.
    pub canon: Vec<u8>,
    /// BLAKE3 hex of `canon` (the vault file name).
    pub id: String,
}

/// {rand} and {port} in a string.
pub fn subst(s: &str, rand: &str, port: u16) -> String {
    s.replace("{rand}", rand).replace("{port}", &port.to_string())
}

pub fn subst_value(v: &Value, rand: &str, port: u16) -> Value {
    match v {
        Value::String(s) => Value::String(subst(s, rand, port)),
        Value::Array(a) => Value::Array(a.iter().map(|x| subst_value(x, rand, port)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), subst_value(x, rand, port))).collect()),
        other => other.clone(),
    }
}

/// Parse a canonical spec again (the vault holds canonical bytes, so this cannot fail unless the file was changed).
pub fn load(canon: &[u8]) -> Result<Linted, Vec<String>> {
    let raw = String::from_utf8_lossy(canon);
    lint(&raw)
}

// ---------------------------------------------------------------- lint

pub fn lint(raw: &str) -> Result<Linted, Vec<String>> {
    if raw.len() > SPEC_BYTES_MAX {
        return Err(vec![format!("Rejected: the check is {} KB; the limit is {} KB.", raw.len() / 1024, SPEC_BYTES_MAX / 1024)]);
    }
    let v: Value = serde_json::from_str(raw).map_err(|e| vec![format!("Rejected: this is not JSON ({e}).")])?;
    let Some(obj) = v.as_object() else {
        return Err(vec!["Rejected: a check is one JSON object.".into()]);
    };
    let mut p: Vec<String> = Vec::new();
    unknown_keys(obj, &TOP_KEYS, "the check", &mut p);
    if obj.get("v").and_then(Value::as_u64) != Some(1) {
        p.push("Rejected: v must be 1.".into());
    }
    let name = text_field(obj, "name", "the check", NAME_MAX, true, &mut p);
    let why = text_field(obj, "why", "the check", WHY_MAX, true, &mut p);
    let start = parse_start(obj.get("start"), &mut p);
    let mut env = BTreeMap::new();
    match obj.get("env") {
        None => {}
        Some(Value::Object(m)) => {
            for (k, val) in m {
                match val.as_str() {
                    Some(s) => {
                        if let Some(msg) = env_problem(k, s) {
                            p.push(format!("Rejected: {msg}"));
                        }
                        env.insert(k.clone(), s.to_string());
                    }
                    None => p.push(format!("Rejected: env {k} must be a text value.")),
                }
            }
        }
        Some(_) => p.push("Rejected: env must be an object of name: value.".into()),
    }
    let mut steps = Vec::new();
    match obj.get("steps") {
        Some(Value::Array(a)) if !a.is_empty() && a.len() <= STEPS_MAX => {
            for (i, s) in a.iter().enumerate() {
                if let Some(st) = parse_step(i + 1, s, &mut p) {
                    steps.push(st);
                }
            }
        }
        Some(Value::Array(a)) if a.is_empty() => p.push("Rejected: steps is empty.".into()),
        Some(Value::Array(a)) => p.push(format!("Rejected: {} steps; the limit is {STEPS_MAX}.", a.len())),
        _ => p.push("Rejected: steps must be a list of 1 to 40 steps.".into()),
    }
    let given_unv = match obj.get("unverifiable") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            p.push("Rejected: unverifiable must be true or false.".into());
            false
        }
    };
    if steps.len() == obj.get("steps").and_then(Value::as_array).map_or(0, Vec::len) && !steps.is_empty() && !steps.iter().any(|s| s.act.is_assertion()) {
        p.push("Rejected: the check has no assertion (an expect_* step, or http with expect). A check that cannot fail proves nothing.".into());
    }
    if !p.is_empty() {
        let n = p.len();
        p.truncate(8);
        if n > 8 {
            p.push(format!("and {} more problems.", n - 8));
        }
        return Err(p);
    }
    let (name, why, start) = (name.unwrap_or_default(), why.unwrap_or_default(), start.expect("checked"));
    let mut spec = Spec { name, why, start, env, steps, unverifiable: given_unv };
    spec.unverifiable = given_unv || touches_unverifiable(&spec);
    // Canonical bytes: the validated input with the two defaults written in. serde_json's Map is a BTreeMap, so keys come out sorted.
    let mut canon_obj = obj.clone();
    canon_obj.insert("unverifiable".into(), Value::Bool(spec.unverifiable));
    if let Some(Value::Object(s)) = canon_obj.get_mut("start") {
        s.insert("timeout_s".into(), Value::from(spec.start.timeout_s));
    }
    let canon = serde_json::to_vec(&Value::Object(canon_obj)).expect("json");
    let id = crate::util::hex(blake3::hash(&canon).as_bytes());
    Ok(Linted { spec, canon, id })
}

fn touches_unverifiable(s: &Spec) -> bool {
    let mut paths: Vec<&str> = vec![&s.start.ready];
    for st in &s.steps {
        match &st.act {
            Act::Goto(p) | Act::WaitHttp(p) => paths.push(p),
            Act::Http(h) => paths.push(&h.path),
            Act::Url(UrlWant::Path(p)) => paths.push(p),
            _ => {}
        }
    }
    paths.iter().any(|p| {
        let l = p.to_ascii_lowercase();
        UNVERIFIABLE_WORDS.iter().any(|w| l.contains(w))
    })
}

fn unknown_keys(o: &Map<String, Value>, allowed: &[&str], what: &str, p: &mut Vec<String>) {
    for k in o.keys() {
        if !allowed.contains(&k.as_str()) {
            p.push(format!("Rejected: {what} has an unknown key '{k}'. Allowed: {}.", allowed.join(", ")));
        }
    }
}

/// A text field of `o`: present, a string, 1..=max chars. Problems go to `p`.
fn text_field(o: &Map<String, Value>, k: &str, what: &str, max: usize, required: bool, p: &mut Vec<String>) -> Option<String> {
    match o.get(k) {
        None if required => {
            p.push(format!("Rejected: {what} is missing '{k}'."));
            None
        }
        None => None,
        Some(Value::String(s)) if !s.trim().is_empty() && s.chars().count() <= max => Some(s.clone()),
        Some(Value::String(s)) if s.trim().is_empty() => {
            p.push(format!("Rejected: '{k}' of {what} is empty."));
            None
        }
        Some(Value::String(s)) => {
            p.push(format!("Rejected: '{k}' of {what} is {} characters; the limit is {max}.", s.chars().count()));
            None
        }
        Some(_) => {
            p.push(format!("Rejected: '{k}' of {what} must be text."));
            None
        }
    }
}

fn parse_start(v: Option<&Value>, p: &mut Vec<String>) -> Option<Start> {
    let Some(Value::Object(o)) = v else {
        p.push("Rejected: start is missing. Give {\"cmd\": \"npm run dev -- --port {port}\", \"ready\": \"/login\"}.".into());
        return None;
    };
    unknown_keys(o, &["cmd", "ready", "timeout_s"], "start", p);
    let n0 = p.len();
    let cmd = text_field(o, "cmd", "start", CMD_MAX, true, p);
    let ready = match text_field(o, "ready", "start", 300, true, p) {
        Some(r) => path_ok(&r, "start.ready", p).then_some(r),
        None => None,
    };
    let timeout_s = match o.get("timeout_s") {
        None => READY_TIMEOUT_MAX,
        Some(t) => match t.as_u64() {
            Some(n) if (1..=READY_TIMEOUT_MAX).contains(&n) => n,
            _ => {
                p.push(format!("Rejected: start.timeout_s must be a whole number from 1 to {READY_TIMEOUT_MAX}."));
                READY_TIMEOUT_MAX
            }
        },
    };
    if p.len() > n0 {
        return None;
    }
    Some(Start { cmd: cmd?, ready: ready?, timeout_s })
}

/// Paths start with one `/`; anything with a scheme or host is an absolute URL.
fn path_ok(s: &str, ctx: &str, p: &mut Vec<String>) -> bool {
    if s.contains("://") || s.starts_with("//") {
        p.push(format!("Rejected: {ctx} uses an absolute URL ({}). Paths must start with /.", crate::util::trunc_chars(s, 60)));
        false
    } else if !s.starts_with('/') {
        p.push(format!("Rejected: {ctx} has the path '{}'. Paths must start with /.", crate::util::trunc_chars(s, 60)));
        false
    } else if s.chars().count() > 300 {
        p.push(format!("Rejected: {ctx} has a path over 300 characters."));
        false
    } else {
        true
    }
}

const SELECTORS: [&str; 9] = ["css", "selector", "class", "id", "tag", "locator", "xpath", "nth", "first"];

/// One locator object: exactly one of role (+name), label, text, text_re, testid.
fn parse_loc(v: &Value, ctx: &str, p: &mut Vec<String>) -> Option<Loc> {
    let o = match v {
        Value::Object(o) => o,
        Value::String(s) => {
            p.push(format!("Rejected: {ctx} uses a CSS selector ({}). {LOC_HINT}", crate::util::trunc_chars(s, 40)));
            return None;
        }
        _ => {
            p.push(format!("Rejected: {ctx} needs a locator. {LOC_HINT}"));
            return None;
        }
    };
    for k in SELECTORS.iter().chain(["last"].iter()) {
        if let Some(val) = o.get(*k) {
            let shown = val.as_str().map(|s| crate::util::trunc_chars(s, 40)).unwrap_or_else(|| val.to_string());
            let what = match *k {
                "xpath" => "an XPath",
                "nth" | "first" | "last" => "a position (nth/first/last)",
                _ => "a CSS selector",
            };
            p.push(format!("Rejected: {ctx} uses {what} ({shown}). {LOC_HINT}"));
            return None;
        }
    }
    let kinds: Vec<&str> = ["role", "label", "text", "text_re", "testid"].into_iter().filter(|k| o.contains_key(*k)).collect();
    let n0 = p.len();
    unknown_keys(o, &["role", "name", "label", "text", "text_re", "testid"], ctx, p);
    if kinds.len() != 1 {
        let msg = if kinds.is_empty() { format!("Rejected: {ctx} has no locator. {LOC_HINT}") } else { format!("Rejected: {ctx} has two locators ({}). Use one.", kinds.join(", ")) };
        p.push(msg);
        return None;
    }
    if o.contains_key("name") && kinds[0] != "role" {
        p.push(format!("Rejected: {ctx}: name goes with role only."));
    }
    let s = text_field(o, kinds[0], ctx, 200, true, p)?;
    let name = match o.get("name") {
        None => None,
        Some(_) => text_field(o, "name", ctx, 200, true, p),
    };
    if p.len() > n0 {
        return None;
    }
    Some(match kinds[0] {
        "role" => Loc::Role(s, name),
        "label" => Loc::Label(s),
        "text" => Loc::Text(s),
        "text_re" => Loc::TextRe(s),
        _ => Loc::TestId(s),
    })
}

fn loc_strings(l: &Loc) -> Vec<&str> {
    match l {
        Loc::Role(r, n) => std::iter::once(r.as_str()).chain(n.as_deref()).collect(),
        Loc::Label(s) | Loc::Text(s) | Loc::TextRe(s) | Loc::TestId(s) => vec![s],
    }
}

fn parse_step(i: usize, v: &Value, p: &mut Vec<String>) -> Option<Step> {
    let ctx = format!("step {i}");
    let Some(o) = v.as_object() else {
        p.push(format!("Rejected: {ctx} must be an object."));
        return None;
    };
    const VERBS: [&str; 15] = [
        "goto", "click", "fill", "press", "select", "check", "uncheck", "wait_http", "expect_visible", "expect_hidden", "expect_text", "expect_url", "expect_count",
        "expect_console_clean", "http",
    ];
    let verbs: Vec<&str> = VERBS.into_iter().filter(|k| o.contains_key(*k)).collect();
    if verbs.len() != 1 {
        let keys: Vec<&str> = o.keys().map(String::as_str).collect();
        p.push(if verbs.is_empty() {
            format!("Rejected: {ctx} has no known step ({}). Steps: {}.", keys.join(", "), VERBS.join(", "))
        } else {
            format!("Rejected: {ctx} has two steps ({}). One per step.", verbs.join(", "))
        });
        return None;
    }
    let verb = verbs[0];
    let extra: &[&str] = match verb {
        "fill" | "select" => &["value"],
        "press" => &["key"],
        "expect_text" => &["contains"],
        "expect_count" => &["n", "min", "max"],
        "http" => &["expect"],
        _ => &[],
    };
    let mut allowed = vec![verb, "why"];
    allowed.extend_from_slice(extra);
    let n0 = p.len();
    unknown_keys(o, &allowed, &ctx, p);
    let arg = &o[verb];
    let assertion = verb.starts_with("expect_") || verb == "http";
    let why = if assertion { text_field(o, "why", &ctx, WHY_MAX, true, p) } else { text_field(o, "why", &ctx, WHY_MAX, false, p) };
    let mut lits: Vec<String> = Vec::new(); // assertion literals the id heuristics look at
    let value = |k: &str, p: &mut Vec<String>| text_field(o, k, &ctx, 500, true, p);
    let act = match verb {
        "goto" | "wait_http" => {
            let s = arg.as_str().map(str::to_string);
            match s {
                Some(s) if path_ok(&s, &ctx, p) => Some(if verb == "goto" { Act::Goto(s) } else { Act::WaitHttp(s) }),
                Some(_) => None,
                None => {
                    p.push(format!("Rejected: {ctx}: {verb} takes a path like \"/login\"."));
                    None
                }
            }
        }
        "click" | "check" | "uncheck" => parse_loc(arg, &ctx, p).map(|l| match verb {
            "click" => Act::Click(l),
            "check" => Act::Check(l),
            _ => Act::Uncheck(l),
        }),
        "fill" | "select" => {
            let l = parse_loc(arg, &ctx, p);
            let val = value("value", p);
            l.zip(val).map(|(l, val)| if verb == "fill" { Act::Fill(l, val) } else { Act::Select(l, val) })
        }
        "press" => {
            let l = parse_loc(arg, &ctx, p);
            let key = value("key", p);
            l.zip(key).map(|(l, k)| Act::Press(l, k))
        }
        "expect_visible" | "expect_hidden" => parse_loc(arg, &ctx, p).map(|l| {
            lits.extend(loc_strings(&l).into_iter().map(str::to_string));
            if verb == "expect_visible" { Act::Visible(l) } else { Act::Hidden(l) }
        }),
        "expect_text" => {
            let l = parse_loc(arg, &ctx, p);
            let c = value("contains", p);
            l.zip(c).map(|(l, c)| {
                lits.extend(loc_strings(&l).into_iter().map(str::to_string));
                lits.push(c.clone());
                Act::Text(l, c)
            })
        }
        "expect_url" => match arg.as_object() {
            Some(u) => {
                unknown_keys(u, &["path", "path_re"], &ctx, p);
                match (u.get("path").is_some(), u.get("path_re").is_some()) {
                    (true, false) => text_field(u, "path", &ctx, 300, true, p).filter(|s| path_ok(s, &ctx, p)).map(|s| {
                        lits.push(s.clone());
                        Act::Url(UrlWant::Path(s))
                    }),
                    (false, true) => text_field(u, "path_re", &ctx, 300, true, p).map(|s| {
                        lits.push(s.clone());
                        Act::Url(UrlWant::PathRe(s))
                    }),
                    _ => {
                        p.push(format!("Rejected: {ctx}: expect_url takes path or path_re (one)."));
                        None
                    }
                }
            }
            None => {
                p.push(format!("Rejected: {ctx}: expect_url takes {{\"path\": \"/x\"}} or {{\"path_re\": \"^/x\"}}."));
                None
            }
        },
        "expect_count" => {
            let l = parse_loc(arg, &ctx, p);
            let num = |k: &str| o.get(k).and_then(Value::as_u64);
            let c = match (o.contains_key("n"), o.contains_key("min") || o.contains_key("max")) {
                (true, false) => num("n").map(Count::N),
                (false, true) => Some(Count::Range(num("min"), num("max"))).filter(|c| matches!(c, Count::Range(a, b) if a.is_some() || b.is_some())),
                _ => None,
            };
            if c.is_none() {
                p.push(format!("Rejected: {ctx}: expect_count takes n, or min and/or max (whole numbers)."));
            }
            l.zip(c).map(|(l, c)| {
                lits.extend(loc_strings(&l).into_iter().map(str::to_string));
                Act::Count(l, c)
            })
        }
        "expect_console_clean" => {
            if arg == &Value::Bool(true) {
                Some(Act::ConsoleClean)
            } else {
                p.push(format!("Rejected: {ctx}: expect_console_clean is written as true."));
                None
            }
        }
        _ => parse_http(arg, o.get("expect"), &ctx, &mut lits, p),
    };
    for l in &lits {
        if let Some(what) = id_like(l) {
            p.push(format!("Rejected: {ctx} checks the literal \"{}\", which looks like {what}. Test data changes between runs; check words the user sees.", crate::util::trunc_chars(l, 40)));
        }
    }
    if p.len() > n0 {
        return None;
    }
    Some(Step { act: act?, why })
}

const METHODS: [&str; 6] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"];
const JSON_TYPES: [&str; 6] = ["string", "number", "boolean", "array", "object", "null"];

fn parse_http(arg: &Value, expect: Option<&Value>, ctx: &str, lits: &mut Vec<String>, p: &mut Vec<String>) -> Option<Act> {
    let Some(h) = arg.as_object() else {
        p.push(format!("Rejected: {ctx}: http takes {{\"method\": \"GET\", \"path\": \"/api/me\"}}."));
        return None;
    };
    unknown_keys(h, &["method", "path", "json"], &format!("{ctx} http"), p);
    let n0 = p.len();
    let method = match h.get("method") {
        None => "GET".to_string(),
        Some(m) => match m.as_str().map(str::to_ascii_uppercase) {
            Some(m) if METHODS.contains(&m.as_str()) => m,
            _ => {
                p.push(format!("Rejected: {ctx}: method must be one of {}.", METHODS.join(", ")));
                "GET".into()
            }
        },
    };
    let path = text_field(h, "path", &format!("{ctx} http"), 300, true, p).filter(|s| path_ok(s, ctx, p));
    if let Some(s) = &path {
        lits.push(s.clone());
    }
    let json = h.get("json").cloned();
    if json.is_some() && (method == "GET" || method == "HEAD") {
        p.push(format!("Rejected: {ctx}: a {method} request carries no json body."));
    }
    let mut e = HttpExpect::default();
    match expect {
        Some(Value::Object(x)) if !x.is_empty() => {
            unknown_keys(x, &["status", "status_in", "json_has", "json_type", "body_contains", "length_range"], &format!("{ctx} expect"), p);
            let codes = |v: &Value| v.as_u64().filter(|c| (100..600).contains(c)).map(|c| c as u16);
            if let Some(s) = x.get("status") {
                match codes(s) {
                    Some(c) => e.status = Some(c),
                    None => p.push(format!("Rejected: {ctx}: status must be a code like 200.")),
                }
            }
            if let Some(s) = x.get("status_in") {
                let l: Option<Vec<u16>> = s.as_array().filter(|a| !a.is_empty()).and_then(|a| a.iter().map(codes).collect());
                match l {
                    Some(l) => e.status_in = l,
                    None => p.push(format!("Rejected: {ctx}: status_in must be a list of codes like [200, 201].")),
                }
            }
            if let Some(s) = x.get("json_has") {
                let l: Option<Vec<String>> = s.as_array().filter(|a| !a.is_empty()).and_then(|a| a.iter().map(|v| v.as_str().map(str::to_string)).collect());
                match l {
                    Some(l) => e.json_has = l,
                    None => p.push(format!("Rejected: {ctx}: json_has must be a list of field paths like [\"email\"].")),
                }
            }
            if let Some(s) = x.get("json_type") {
                let l: Option<Vec<(String, String)>> = s.as_object().filter(|m| !m.is_empty()).and_then(|m| {
                    m.iter().map(|(k, v)| v.as_str().filter(|t| JSON_TYPES.contains(t)).map(|t| (k.clone(), t.to_string()))).collect()
                });
                match l {
                    Some(l) => e.json_type = l,
                    None => p.push(format!("Rejected: {ctx}: json_type must map field paths to one of {}.", JSON_TYPES.join(", "))),
                }
            }
            if let Some(s) = x.get("body_contains") {
                match s.as_str().filter(|s| !s.is_empty() && s.chars().count() <= 200) {
                    Some(s) => {
                        lits.push(s.to_string());
                        e.body_contains = Some(s.to_string());
                    }
                    None => p.push(format!("Rejected: {ctx}: body_contains must be text of 1 to 200 characters.")),
                }
            }
            if let Some(s) = x.get("length_range") {
                let r: Option<(u64, u64)> = s.as_array().filter(|a| a.len() == 2).and_then(|a| Some((a[0].as_u64()?, a[1].as_u64()?))).filter(|(a, b)| a <= b);
                match r {
                    Some(r) => e.length_range = Some(r),
                    None => p.push(format!("Rejected: {ctx}: length_range must be [min, max] with min not above max.")),
                }
            }
        }
        _ => p.push(format!("Rejected: {ctx}: http needs an expect with at least one of status, status_in, json_has, json_type, body_contains, length_range.")),
    }
    if p.len() > n0 {
        return None;
    }
    Some(Act::Http(HttpStep { method, path: path?, json, expect: e }))
}

// ---------------------------------------------------------------- literals that look like test data

/// Why a literal looks like generated test data (an id, date, uuid, long hex or long number), or None.
pub fn id_like(s: &str) -> Option<&'static str> {
    let t = s.replace("{rand}", "").replace("{port}", "");
    let b = t.as_bytes();
    if (0..b.len().saturating_sub(35)).any(|i| uuid_at(&b[i..i + 36])) {
        return Some("a uuid");
    }
    const DATES: [&str; 10] = ["dddd-dd-dd", "dddd/dd/dd", "dd/dd/dddd", "dd.dd.dddd", "dd-dd-dddd", "d/d/dddd", "dd/d/dddd", "d/dd/dddd", "dd:dd:dd", "dddd.dd.dd"];
    if DATES.iter().any(|pat| has_pattern(b, pat.as_bytes())) {
        return Some("a date or time");
    }
    let (mut hexrun, mut digrun) = (0, 0);
    for &c in b {
        hexrun = if c.is_ascii_hexdigit() { hexrun + 1 } else { 0 };
        digrun = if c.is_ascii_digit() { digrun + 1 } else { 0 };
        if digrun >= 5 {
            return Some("a long number");
        }
        if hexrun >= 12 {
            return Some("a long hex string");
        }
    }
    for tok in t.split(|c: char| !c.is_ascii_alphanumeric()) {
        let digits = tok.bytes().filter(u8::is_ascii_digit).count();
        if tok.len() >= 10 && digits >= 3 && tok.len() - digits >= 3 {
            return Some("an id");
        }
    }
    None
}

fn uuid_at(w: &[u8]) -> bool {
    w.iter().enumerate().all(|(i, &c)| if matches!(i, 8 | 13 | 18 | 23) { c == b'-' } else { c.is_ascii_hexdigit() })
}

/// `d` matches one digit, every other byte itself.
fn has_pattern(b: &[u8], pat: &[u8]) -> bool {
    b.len() >= pat.len() && (0..=b.len() - pat.len()).any(|i| pat.iter().enumerate().all(|(j, &p)| if p == b'd' { b[i + j].is_ascii_digit() } else { b[i + j] == p }))
}

// ---------------------------------------------------------------- env

fn loopback_host(h: &str) -> bool {
    let h = h.trim_matches(|c| c == '[' || c == ']').to_ascii_lowercase();
    if h == "localhost" || h == "::1" || h == "{host}" {
        return true;
    }
    let parts: Vec<&str> = h.split('.').collect();
    parts.len() == 4 && parts[0] == "127" && parts.iter().all(|x| !x.is_empty() && x.len() <= 3 && x.bytes().all(|c| c.is_ascii_digit()))
}

/// host out of `userinfo@host:port`, with `[v6]` brackets kept for the loopback test.
fn host_of(authority: &str) -> &str {
    let a = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if let Some(rest) = a.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match a.rsplit_once(':') {
        Some((h, port)) if port == "{port}" || port.bytes().all(|c| c.is_ascii_digit()) => h,
        _ => a,
    }
}

/// Why an env entry is refused, or None. Also used at run time (a changed vault file is caught there too).
/// shortcut: a bare hostname in a value that is not named *_HOST/*_ADDR/... and has no scheme or port is not caught; upgrade with a DNS-free host list if it matters.
pub fn env_problem(name: &str, value: &str) -> Option<String> {
    let valid = !name.is_empty() && name.len() <= 64 && !name.as_bytes()[0].is_ascii_digit() && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_');
    if !valid {
        return Some(format!("env name '{}' is not a plain name (letters, digits, _).", crate::util::trunc_chars(name, 30)));
    }
    if RESERVED_ENV.contains(&name.to_ascii_uppercase().as_str()) {
        return Some(format!("env {name} is set by Torpor (use {{port}} in the start command for the port)."));
    }
    if value.chars().count() > 500 || value.contains('\0') || value.contains('\n') {
        return Some(format!("env {name} is not a short single-line value."));
    }
    let mut rest = value;
    while let Some(i) = rest.find("://") {
        let after = &rest[i + 3..];
        let end = after.find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace()).unwrap_or(after.len());
        let host = host_of(&after[..end]);
        if !loopback_host(host) {
            return Some(format!("env {name} points at {host}, which is not on this computer. Torpor only talks to 127.0.0.1."));
        }
        rest = &after[end..];
    }
    for tok in value.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '=' | '"' | '\'')) {
        if let Some((h, port)) = tok.rsplit_once(':') {
            let host_like = !h.is_empty() && h.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'));
            if host_like && port.len() >= 2 && port.bytes().all(|c| c.is_ascii_digit()) && !loopback_host(h) && !tok.contains("://") {
                return Some(format!("env {name} points at {h}, which is not on this computer. Torpor only talks to 127.0.0.1."));
            }
        }
    }
    let up = name.to_ascii_uppercase();
    if ["HOST", "HOSTNAME", "ADDR", "ADDRESS", "SERVER", "ENDPOINT"].iter().any(|s| up.ends_with(s)) && !value.is_empty() && !loopback_host(host_of(value)) && !value.contains("://") {
        return Some(format!("env {name} names {value}, which is not on this computer. Torpor only talks to 127.0.0.1."));
    }
    if crate::texts::redact(&format!("{name}={value}")).1 > 0 {
        return Some(format!("env {name} holds something that looks like a secret. Checks must not carry secrets."));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"{"v":1,"name":"login works","why":"turn 12: the user said the login works now",
     "start":{"cmd":"npm run dev -- --port {port}","ready":"/login","timeout_s":90},
     "env":{"DATABASE_URL":"file:./dev.db","API":"http://127.0.0.1:{port}/x"},
     "steps":[{"goto":"/login"},
      {"fill":{"label":"Email"},"value":"u{rand}@example.test"},
      {"click":{"role":"button","name":"Sign in"}},
      {"expect_visible":{"text":"Welcome"},"why":"after sign-in the welcome text shows"},
      {"http":{"method":"GET","path":"/api/me"},"expect":{"status":200,"json_has":["email"]},"why":"the session works"}],
     "unverifiable":false}"#;

    fn with(edit: impl FnOnce(&mut Value)) -> String {
        let mut v: Value = serde_json::from_str(GOOD).unwrap();
        edit(&mut v);
        v.to_string()
    }
    /// Lint text of a rejected variant; panics when it was accepted.
    fn rej(s: &str) -> String {
        match lint(s) {
            Ok(_) => panic!("accepted: {s}"),
            Err(p) => p.join("\n"),
        }
    }
    fn step(s: Value) -> String {
        with(|v| v["steps"] = Value::Array(vec![s, serde_json::json!({"expect_visible":{"text":"Hi"},"why":"w"})]))
    }

    #[test]
    fn the_spec_example_passes_and_its_id_is_stable() {
        let a = lint(GOOD).unwrap();
        assert_eq!(a.spec.assertions(), 2);
        assert!(!a.spec.http_only() && !a.spec.unverifiable);
        assert_eq!(a.spec.goto_paths(), ["/login"]);
        // Key order and the defaults do not change the id.
        let b = lint(&with(|v| {
            v["start"].as_object_mut().unwrap().remove("timeout_s");
            v.as_object_mut().unwrap().remove("unverifiable");
        }))
        .unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(a.id.len(), 64);
        let again = load(&a.canon).unwrap();
        assert_eq!(again.id, a.id, "the canonical form lints to itself");
    }

    #[test]
    fn every_reject_rule_rejects_and_says_why() {
        assert!(rej("nope").contains("not JSON"));
        assert!(rej("[1]").contains("one JSON object"));
        assert!(rej(&with(|v| v["extra"] = 1.into())).contains("unknown key 'extra'"));
        assert!(rej(&with(|v| v["v"] = 2.into())).contains("v must be 1"));
        assert!(rej(&with(|v| v["name"] = "n".repeat(61).into())).contains("limit is 60"));
        assert!(rej(&with(|v| v["why"] = "w".repeat(201).into())).contains("limit is 200"));
        assert!(rej(&with(|v| v["steps"] = serde_json::json!([]))).contains("steps is empty"));
        assert!(rej(&with(|v| v["steps"] = Value::Array((0..41).map(|_| serde_json::json!({"goto":"/"})).collect()))).contains("limit is 40"));
        assert!(rej(&with(|v| v["start"]["cmd"] = "x".repeat(301).into())).contains("limit is 300"));
        assert!(rej(&with(|v| v["start"]["timeout_s"] = 91.into())).contains("timeout_s"));
        assert!(rej(&with(|v| v.as_object_mut().unwrap().remove("start").map(|_| ()).unwrap())).contains("start is missing"));
        assert!(rej(&with(|v| v["start"]["extra"] = 1.into())).contains("unknown key 'extra'"));
        // steps
        assert!(rej(&step(serde_json::json!({"sleep": 5}))).contains("no known step"));
        assert!(rej(&step(serde_json::json!({"goto":"/a","click":{"text":"x"}}))).contains("two steps"));
        assert!(rej(&step(serde_json::json!({"goto":"/a","speed":1}))).contains("unknown key 'speed'"));
        // locators
        assert!(rej(&step(serde_json::json!({"click":".btn"}))).contains("uses a CSS selector (.btn). Use role, label, text or testid."));
        assert!(rej(&step(serde_json::json!({"click":{"css":"#go"}}))).contains("a CSS selector"));
        assert!(rej(&step(serde_json::json!({"click":{"xpath":"//a"}}))).contains("an XPath"));
        assert!(rej(&step(serde_json::json!({"click":{"text":"Go","nth":2}}))).contains("a position"));
        assert!(rej(&step(serde_json::json!({"click":{"text":"Go","label":"Go"}}))).contains("two locators"));
        assert!(rej(&step(serde_json::json!({"click":{}}))).contains("no locator"));
        assert!(rej(&step(serde_json::json!({"click":{"text":"Go","name":"x"}}))).contains("name goes with role"));
        // absolute URLs and bad paths
        assert!(rej(&step(serde_json::json!({"goto":"https://example.com/a"}))).contains("absolute URL"));
        assert!(rej(&step(serde_json::json!({"goto":"//evil.test/a"}))).contains("absolute URL"));
        assert!(rej(&step(serde_json::json!({"goto":"login"}))).contains("must start with /"));
        assert!(rej(&step(serde_json::json!({"http":{"path":"http://127.0.0.1/x"},"expect":{"status":200},"why":"w"}))).contains("absolute URL"));
        assert!(rej(&with(|v| v["start"]["ready"] = "http://localhost:3000/".into())).contains("absolute URL"));
        // assertions need a why, and at least one must exist
        assert!(rej(&step(serde_json::json!({"expect_visible":{"text":"Hi"}}))).contains("missing 'why'"));
        assert!(rej(&with(|v| v["steps"] = serde_json::json!([{"goto":"/"}]))).contains("no assertion"));
        assert!(rej(&step(serde_json::json!({"expect_url":{"path":"/a","path_re":"x"},"why":"w"}))).contains("path or path_re"));
        assert!(rej(&step(serde_json::json!({"expect_count":{"text":"x"},"n":1,"min":1,"why":"w"}))).contains("expect_count takes"));
        assert!(rej(&step(serde_json::json!({"expect_console_clean":false,"why":"w"}))).contains("written as true"));
        // http
        assert!(rej(&step(serde_json::json!({"http":{"path":"/a"},"why":"w"}))).contains("needs an expect"));
        assert!(rej(&step(serde_json::json!({"http":{"path":"/a","method":"TRACE"},"expect":{"status":200},"why":"w"}))).contains("method must be"));
        assert!(rej(&step(serde_json::json!({"http":{"path":"/a","json":{}},"expect":{"status":200},"why":"w"}))).contains("no json body"));
        assert!(rej(&step(serde_json::json!({"http":{"path":"/a"},"expect":{"status":200,"color":1},"why":"w"}))).contains("unknown key 'color'"));
        assert!(rej(&step(serde_json::json!({"http":{"path":"/a"},"expect":{"json_type":{"a":"text"}},"why":"w"}))).contains("json_type"));
        assert!(rej(&step(serde_json::json!({"http":{"path":"/a"},"expect":{"length_range":[5,1]},"why":"w"}))).contains("length_range"));
    }

    #[test]
    fn literals_that_look_like_test_data_are_rejected() {
        for bad in ["order 12345 shipped", "id 3f2a9c1e-77b4-4d1a-9c55-0a1b2c3d4e5f", "since 2024-05-17", "on 17/05/2024", "at 12:30:15", "tok 0123456789abcdef", "ref a1b2c3d4e5f6", "abcdef0123456789"] {
            let msg = rej(&step(serde_json::json!({"expect_text":{"label":"Greeting"},"contains":bad,"why":"w"})));
            assert!(msg.contains("looks like"), "{bad}: {msg}");
        }
        // The same words are fine in an action (not an assertion) and as placeholders.
        lint(&step(serde_json::json!({"fill":{"label":"Phone"},"value":"0501234567"}))).unwrap();
        lint(&step(serde_json::json!({"expect_text":{"label":"Greeting"},"contains":"Hello u{rand} on port {port}","why":"w"}))).unwrap();
        lint(&step(serde_json::json!({"expect_text":{"label":"Total"},"contains":"$1,299 in 2024","why":"w"}))).unwrap();
        assert!(rej(&step(serde_json::json!({"expect_visible":{"testid":"user-9f8e7d6c5b4a"},"why":"w"}))).contains("looks like"));
        assert!(rej(&step(serde_json::json!({"http":{"path":"/api/items/123456"},"expect":{"status":200},"why":"w"}))).contains("long number"));
        assert!(rej(&step(serde_json::json!({"http":{"path":"/a"},"expect":{"body_contains":"1700000000"},"why":"w"}))).contains("long number"));
    }

    #[test]
    fn env_values_must_stay_on_this_computer_and_carry_no_secrets() {
        for (k, v) in [
            ("API", "https://api.example.com/v1"),
            ("DB", "postgres://user:pw@db.example.com:5432/app"),
            ("REDIS", "redis.internal.example:6379"),
            ("DB_HOST", "db.example.com"),
            ("OTHER", "10.0.0.5:5432"),
            ("TOKEN", "ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
            ("SESSION_SECRET", "Xk9#mP2$vLq8Zw3N7y"),
            ("HOME", "/root"),
            ("PORT", "3000"),
            ("bad name", "x"),
        ] {
            let msg = rej(&with(|d| d["env"] = serde_json::json!({ k: v })));
            assert!(msg.contains(k), "{k}: {msg}");
        }
        assert!(env_problem("API", "https://api.example.com/v1").unwrap().contains("not on this computer"));
        for (k, v) in [("API", "http://127.0.0.1:{port}/x"), ("API", "http://localhost:3000"), ("API", "http://[::1]:80/"), ("DB", "file:./dev.db"), ("DB_HOST", "localhost"), ("MODE", "test"), ("DB", "postgres://u:p@127.0.0.1:5432/db")] {
            assert_eq!(env_problem(k, v), None, "{k}={v}");
        }
        // Non-loopback in a chain of URLs: the second one counts too.
        assert!(env_problem("X", "http://127.0.0.1/a http://example.org/b").is_some());
    }

    #[test]
    fn paths_with_oauth_and_friends_force_unverifiable() {
        let l = lint(&step(serde_json::json!({"goto":"/auth/Callback?x=1"}))).unwrap();
        assert!(l.spec.unverifiable);
        assert!(String::from_utf8(l.canon).unwrap().contains("\"unverifiable\":true"));
        for w in ["/oauth/start", "/captcha", "/checkout", "/payment/x"] {
            assert!(lint(&step(serde_json::json!({"goto": w}))).unwrap().spec.unverifiable, "{w}");
        }
        assert!(!lint(&step(serde_json::json!({"goto":"/cart"}))).unwrap().spec.unverifiable);
    }

    #[test]
    fn http_only_and_placeholders() {
        let l = lint(&with(|v| {
            v["steps"] = serde_json::json!([
                {"wait_http":"/api/health"},
                {"http":{"method":"post","path":"/api/login","json":{"email":"u{rand}@example.test"}},"expect":{"status_in":[200,201],"json_type":{"email":"string"},"length_range":[2,500]},"why":"w"}
            ])
        }))
        .unwrap();
        assert!(l.spec.http_only());
        assert_eq!(subst("u{rand}-{port}", "abc", 8080), "uabc-8080");
        if let Act::Http(h) = &l.spec.steps[1].act {
            assert_eq!(h.method, "POST");
            assert_eq!(subst_value(h.json.as_ref().unwrap(), "zz", 1)["email"], "uzz@example.test");
        } else {
            panic!("http step");
        }
    }
}

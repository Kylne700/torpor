//! The shadow tier (spec 17): after the proof, may this promise start to count? The oracle rules (c) say whether the check is about what the
//! user said; the mutant kill (d) says whether the check notices when part of the fix is undone. Neither writes a run row or a tree.
use crate::capture::flatten_tree;
use crate::promise::{self, Call, Promise};
use crate::scratch::{self, Opts};
use crate::spec::{Act, Spec};
use crate::store::*;
use crate::util::*;
use crate::{findbreak, gate, texts};
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

/// Most mutants tried for one promise.
const MUTANTS: usize = 3;
/// Spec 17: a cold start over this skips the mutants (the promise stays advisory, "oracle untested"). shortcut: the first run's whole time
/// stands for the cold start; upgrade when the runner reports the start separately.
pub const COLD_MS: u64 = 10_000;
/// Files over this are not diffed for mutants.
const FILE_MAX: u64 = 200_000;

/// What the assessment decided, to be stored with the promise.
pub struct Assessed {
    /// "shadow" or "advisory".
    pub tier: &'static str,
    /// Why it is not shadow, in words for the person.
    pub note: Option<String>,
    pub killed: Option<i64>,
    pub tried: Option<i64>,
}

impl Assessed {
    pub fn advisory(note: impl Into<String>) -> Assessed {
        Assessed { tier: "advisory", note: Some(note.into()), killed: None, tried: None }
    }
}

// ---------------------------------------------------------------- (c) the oracle rules, on text

/// Words of 4 or more letters, lowercase.
fn words(s: &str) -> BTreeSet<String> {
    s.split(|c: char| !c.is_alphabetic()).filter(|w| w.chars().count() >= 4).map(str::to_lowercase).collect()
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// What stands between quotes in `s` (straight, curly, angle, backticks), normalised. An apostrophe inside a word does not open or close one.
fn quoted(s: &str) -> Vec<String> {
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let after_word = i > 0 && c[i - 1].is_alphanumeric();
        let close = match c[i] {
            '"' => '"',
            '\u{201C}' => '\u{201D}',
            '\u{AB}' => '\u{BB}',
            '`' => '`',
            '\'' if !after_word => '\'',
            '\u{2018}' if !after_word => '\u{2019}',
            _ => {
                i += 1;
                continue;
            }
        };
        let end = (i + 1..c.len()).find(|&j| c[j] == close && !(matches!(close, '\'' | '\u{2019}') && c.get(j + 1).is_some_and(|n| n.is_alphanumeric())));
        match end {
            Some(j) => {
                let span = norm(&c[i + 1..j].iter().collect::<String>());
                if !span.is_empty() {
                    out.push(span);
                }
                i = j + 1;
            }
            None => i += 1,
        }
    }
    out
}

/// Spec 17 (c): why this check is not clearly about what the user said, or None when every rule holds. `said` = the human text of the source
/// turn plus the keep words.
pub fn oracle(spec: &Spec, said: &str) -> Option<String> {
    if spec.unverifiable {
        return Some("the check says it cannot verify the result".into());
    }
    let mine = words(said);
    for (n, st) in spec.steps.iter().filter(|s| s.act.is_assertion()).enumerate() {
        let why = st.why.as_deref().unwrap_or("");
        if words(why).is_disjoint(&mine) {
            return Some(format!("check {} (\"{}\") does not use a word of what you said", n + 1, trunc_chars(why, 50)));
        }
    }
    let quotes = quoted(said);
    for st in &spec.steps {
        let lit = match &st.act {
            Act::Text(_, s) => Some(s),
            Act::Http(h) => h.expect.body_contains.as_ref(),
            _ => None,
        };
        if let Some(l) = lit.filter(|l| l.split_whitespace().count() >= 4) {
            let want = norm(l);
            if !quotes.iter().any(|q| q.contains(&want)) {
                return Some(format!("it looks for the text \"{}\", which you did not put in quotes", trunc_chars(l, 40)));
            }
        }
    }
    None
}

/// The human words behind a promise: the prompt of the turn it was made in and the keep words of that turn. The source rule has already
/// decided whose turn it was (a provisional source is checked again at Stop). None: Torpor kept no text for it.
fn said_by(db: &Connection, p: &Promise) -> Option<String> {
    let turn = p.created_turn?;
    let mut t = texts::prompt_of(db, turn).unwrap_or_default();
    let mut st = db.prepare("SELECT text FROM constraints WHERE turn_id=?1 AND source IN ('keep_user','keep_agent')").ok()?;
    for k in st.query_map([turn], |r| r.get::<_, String>(0)).ok()?.flatten() {
        t.push('\n');
        t.push_str(&k);
    }
    (!t.trim().is_empty()).then_some(t)
}

// ---------------------------------------------------------------- (d) mutants

/// One way to undo part of the fix on the good version: the new content of one file (`None` deletes it).
#[derive(Debug, Clone, PartialEq)]
pub struct Mutant {
    pub path: String,
    pub content: Option<Vec<u8>>,
    /// Changed lines: the largest come first.
    pub size: usize,
}

fn changed_lines(h: &diffy::Hunk<'_, str>) -> usize {
    h.lines().iter().filter(|l| !matches!(l, diffy::Line::Context(_))).count()
}

/// Each hunk of the bad-to-good change, undone alone in the good text.
pub fn hunk_mutants(path: &str, good: &str, bad: &str) -> Vec<Mutant> {
    let patch = diffy::create_patch(good, bad); // good to bad: applying one hunk of it to good undoes that part of the fix
    let text = patch.to_string();
    let (mut head, mut hunks) = (String::new(), Vec::<String>::new());
    for line in text.split_inclusive('\n') {
        if line.starts_with("@@ ") {
            hunks.push(String::new());
        }
        match hunks.last_mut() {
            Some(h) => h.push_str(line),
            None => head.push_str(line),
        }
    }
    if hunks.len() != patch.hunks().len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (h, body) in patch.hunks().iter().zip(&hunks) {
        let single = format!("{head}{body}");
        let Ok(one) = diffy::Patch::from_str(&single) else { continue };
        if let Ok(new) = diffy::apply(good, &one) {
            out.push(Mutant { path: path.to_string(), content: Some(new.into_bytes()), size: changed_lines(h) });
        }
    }
    out
}

/// A line that is only punctuation, a bracket or a comment is not worth deleting.
fn worth_deleting(l: &str) -> bool {
    let t = l.trim();
    !t.is_empty() && !t.starts_with("//") && !t.starts_with('#') && !t.starts_with('*') && t.chars().any(|c| c.is_alphanumeric())
}

/// When the whole change is one hunk: each mutant deletes one line the change added (the longest first).
pub fn line_mutants(path: &str, good: &str, bad: &str) -> Vec<Mutant> {
    let patch = diffy::create_patch(good, bad);
    let [h] = patch.hunks() else { return Vec::new() };
    let mut ln = h.old_range().start();
    let mut added: Vec<(usize, &str)> = Vec::new();
    for l in h.lines() {
        match l {
            diffy::Line::Context(_) => ln += 1,
            diffy::Line::Delete(t) => {
                if worth_deleting(t) {
                    added.push((ln, t));
                }
                ln += 1;
            }
            diffy::Line::Insert(_) => {}
        }
    }
    added.sort_by_key(|(_, t)| std::cmp::Reverse(t.trim().len()));
    let lines: Vec<&str> = good.split_inclusive('\n').collect();
    added
        .into_iter()
        .take(MUTANTS)
        .filter(|(n, _)| *n >= 1 && *n <= lines.len())
        .map(|(n, _)| Mutant { path: path.to_string(), content: Some(lines.iter().enumerate().filter(|(i, _)| *i + 1 != n).map(|(_, l)| *l).collect::<String>().into_bytes()), size: 1 })
        .collect()
}

fn text_of(ws: &Ws, e: &TreeEntry) -> Option<Vec<u8>> {
    if e.kind != 'f' || e.size > FILE_MAX {
        return None;
    }
    ws.read_blob(&ws.conn, &unhex32(&e.id)?).ok()
}

/// Up to 3 mutants of the good version (spec 17): one hunk of the bad-to-good change undone each, the largest first; a change of one hunk gives
/// line deletions instead. None = the change cannot be read, or nothing in it can be undone.
pub fn plan(ws: &Ws, bad: &[u8; 32], good: &[u8; 32]) -> Vec<Mutant> {
    let Some(changed) = findbreak::changed_files_opt(ws, bad, good) else { return Vec::new() };
    let (Ok(b), Ok(g)) = (flatten_tree(ws, &ws.conn, bad), flatten_tree(ws, &ws.conn, good)) else { return Vec::new() };
    let (b, g): (BTreeMap<_, _>, BTreeMap<_, _>) = (b.into_iter().collect(), g.into_iter().collect());
    let mut all: Vec<Mutant> = Vec::new();
    let mut texts_of: Vec<(String, String, String)> = Vec::new();
    for path in changed.iter().filter(|p| scratch::overlay_ok(p) && !gate::doc_like(p) && !gate::is_global(p)) {
        match (g.get(path), b.get(path)) {
            (Some(ge), Some(be)) => {
                let (Some(gb), Some(bb)) = (text_of(ws, ge), text_of(ws, be)) else { continue };
                let (Ok(gs), Ok(bs)) = (String::from_utf8(gb), String::from_utf8(bb)) else { continue };
                all.extend(hunk_mutants(path, &gs, &bs));
                texts_of.push((path.clone(), gs, bs));
            }
            // A file the change added: undone by deleting it. A file it removed: undone by putting it back.
            (Some(ge), None) => {
                if let Some(gb) = text_of(ws, ge) {
                    all.push(Mutant { path: path.clone(), content: None, size: String::from_utf8_lossy(&gb).lines().count().max(1) });
                }
            }
            (None, Some(be)) => {
                if let Some(bb) = text_of(ws, be).filter(|x| std::str::from_utf8(x).is_ok()) {
                    let size = String::from_utf8_lossy(&bb).lines().count().max(1);
                    all.push(Mutant { path: path.clone(), content: Some(bb), size });
                }
            }
            _ => {}
        }
    }
    if all.len() == 1 && texts_of.len() == 1 {
        let (path, gs, bs) = &texts_of[0];
        let lines = line_mutants(path, gs, bs);
        if !lines.is_empty() {
            return lines;
        }
    }
    all.sort_by_key(|m| std::cmp::Reverse(m.size));
    all.truncate(MUTANTS);
    all
}

/// Run the mutants until one is killed. (killed, tried): a mutant counts as tried unless it could not run; killed = the check FAILs at an
/// assertion in 3 of 3 fresh runs on it. Nothing is recorded in `promise_runs`: these are not runs of the promise on a version of the project.
fn kill(ws: &Ws, spec: &Spec, good: &[u8; 32], mutants: &[Mutant], until: Instant, say: &dyn Fn(&str)) -> (i64, i64) {
    let (mut killed, mut tried) = (0, 0);
    for (i, m) in mutants.iter().enumerate() {
        if Instant::now() >= until {
            break;
        }
        let opts = Opts { overlay: vec![(m.path.clone(), m.content.clone())], ..Opts::default() };
        let c = promise::check_tree_with(ws, spec, good, &opts);
        let what = match (c.call, c.runs.iter().all(promise::fails_at_assertion)) {
            (Call::Unrunnable, _) => "could not run",
            (Call::Fail, true) => {
                (killed, tried) = (killed + 1, tried + 1);
                "caught"
            }
            (Call::Fail, false) => "could not be judged",
            _ => {
                tried += 1;
                "not caught"
            }
        };
        say(&format!("  broken copy {} of {} ({}): {what}", i + 1, mutants.len(), m.path));
        if killed > 0 {
            break;
        }
    }
    (killed, tried)
}

// ---------------------------------------------------------------- the whole assessment

/// Spec 17: shadow needs the proof (done by the caller), (c) the oracle rules and (d) a mutant kill. `cold_ms` = how long the first good run took.
pub fn assess(ws: &Ws, p: &Promise, spec: &Spec, good: &[u8; 32], bad: &[u8; 32], cold_ms: u64, until: Instant, say: &dyn Fn(&str)) -> Assessed {
    let source = p.want_source.as_deref().unwrap_or(&p.source);
    if !matches!(source, "repro" | "keep" | "praise") {
        return Assessed::advisory("it came from the AI, not from your words");
    }
    let Some(said) = said_by(&ws.conn, p) else {
        return Assessed::advisory("Torpor has no record of your words for it");
    };
    if source == "praise" && texts::hebrew_only(&texts::prompt_of(&ws.conn, p.created_turn.unwrap_or(0)).unwrap_or_default()) {
        return Assessed::advisory("praise written only in Hebrew stays advisory");
    }
    if let Some(why) = oracle(spec, &said) {
        return Assessed::advisory(why);
    }
    if cold_ms > COLD_MS {
        return Assessed::advisory("the app takes over 10 seconds to start, so the check was not tested against a broken copy (oracle untested)");
    }
    let mutants = plan(ws, bad, good);
    if mutants.is_empty() {
        return Assessed::advisory("no part of the fix could be undone to test the check (oracle untested)");
    }
    say(&format!("Testing the check against {} broken cop{} of the project.", mutants.len(), if mutants.len() == 1 { "y" } else { "ies" }));
    let (killed, tried) = kill(ws, spec, good, &mutants, until, say);
    let (k, t) = (Some(killed), Some(tried));
    match (killed, tried) {
        (0, 0) => Assessed { note: Some("the broken copies could not run, so the check was not tested against them (oracle untested)".into()), killed: k, tried: t, ..Assessed::advisory("") },
        (0, n) => Assessed { note: Some(format!("it still passed on {n} broken cop{}, so it may not test the new work", if n == 1 { "y" } else { "ies" })), killed: k, tried: t, ..Assessed::advisory("") },
        _ => Assessed { tier: "shadow", note: None, killed: k, tried: t },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec;

    fn spec_with(whys: &[&str], extra: &str) -> Spec {
        let steps: Vec<String> = whys.iter().map(|w| format!(r#"{{"http":{{"path":"/api/me"}},"expect":{{"status":200{extra}}},"why":"{w}"}}"#)).collect();
        let raw = format!(r#"{{"v":1,"name":"n","why":"w","start":{{"cmd":"node server.js","ready":"/","timeout_s":10}},"env":{{}},"steps":[{}]}}"#, steps.join(","));
        spec::lint(&raw).unwrap_or_else(|e| panic!("{e:?}")).spec
    }

    #[test]
    fn a_why_must_share_a_word_of_four_letters_with_what_the_user_said() {
        let said = "great, login works now";
        assert_eq!(oracle(&spec_with(&["login works after sign-in"], ""), said), None);
        let no = oracle(&spec_with(&["sign-in is accepted"], ""), said).unwrap();
        assert!(no.starts_with("check 1 (\"sign-in is accepted\") does not use a word"), "{no}");
        // Three letters are not a word; case does not matter; every assertion needs one.
        assert!(oracle(&spec_with(&["the app is up"], ""), "the app is up").is_some());
        assert_eq!(oracle(&spec_with(&["LOGIN page"], ""), said), None);
        assert!(oracle(&spec_with(&["login works", "nothing alike"], ""), said).unwrap().starts_with("check 2"));
    }

    #[test]
    fn a_long_literal_must_be_quoted_by_the_user() {
        let long = r#", "body_contains": "Welcome back to the shop""#;
        let s = spec_with(&["login works"], long);
        let no = oracle(&s, "login works now, it greets with Welcome back to the shop").unwrap();
        assert!(no.contains("which you did not put in quotes"), "{no}");
        for yes in [
            r#"login works, it says "Welcome back to the shop""#,
            "login works, it says 'welcome   BACK to the shop'",
            "login works \u{201C}Welcome back to the shop!\u{201D}",
            "login works `Welcome back to the shop`",
        ] {
            assert_eq!(oracle(&s, yes), None, "{yes}");
        }
        // Three words need no quotes.
        assert_eq!(oracle(&spec_with(&["login works"], r#", "body_contains": "Welcome back now""#), "login works"), None);
        // An apostrophe inside a word is not a quote.
        assert!(quoted("it doesn't work, they don't either").is_empty());
        assert_eq!(quoted("say 'it's fine' and \"ok\""), vec!["it's fine", "ok"]);
    }

    #[test]
    fn an_unverifiable_check_is_never_shadow() {
        let mut s = spec_with(&["login works"], "");
        s.unverifiable = true;
        assert!(oracle(&s, "login works").unwrap().contains("cannot verify"));
    }

    #[test]
    fn hunks_are_undone_one_at_a_time_and_a_single_hunk_gives_line_deletions() {
        let bad: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        let good = bad.replace("line 3\n", "line 3 fixed\nextra a\nextra b\n").replace("line 25\n", "line 25 fixed\n");
        let m = hunk_mutants("a.js", &good, &bad);
        assert_eq!(m.len(), 2, "two separate hunks");
        let texts: Vec<String> = m.iter().map(|x| String::from_utf8(x.content.clone().unwrap()).unwrap()).collect();
        // Undoing the first hunk puts the old line 3 back and leaves the second fix in place.
        assert!(texts.iter().any(|t| t.contains("line 3\n") && !t.contains("extra a") && t.contains("line 25 fixed")));
        assert!(texts.iter().any(|t| t.contains("line 3 fixed") && t.contains("line 25\n") && !t.contains("line 25 fixed")));
        assert_eq!(m.iter().map(|x| x.size).max(), Some(4), "1 removed and 3 added lines in the bigger hunk");

        let one = bad.replace("line 3\n", "line 3 fixed\nshort\nthe longest added line here\n// comment only\n}\n");
        let l = line_mutants("a.js", &one, &bad);
        let gone: Vec<String> = l
            .iter()
            .map(|x| {
                let t = String::from_utf8(x.content.clone().unwrap()).unwrap();
                one.lines().find(|ln| !t.lines().any(|y| y == *ln)).unwrap().to_string()
            })
            .collect();
        assert_eq!(gone, ["the longest added line here", "line 3 fixed", "short"], "longest first; comments and brackets are not deleted");
    }
}

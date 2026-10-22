//! Standing constraints (spec 21): the user's own "always / never / must" sentences, kept so the rebuild after a compaction can say them again.
//! They are taken at Stop from human turns only, never from tool output or the AI's words, and kept as the user wrote them (the stored, redacted prompt).
//! shortcut: English words only, and a sentence ends at . ! ? or a line end, so "e.g. never ..." is cut after "e.g."; upgrade when real prompts show it.
use crate::texts;
use crate::util::*;
use rusqlite::{params, Connection, OptionalExtension};

/// Spec 21: a constraint is one sentence of at most this many characters.
pub const MAX_CHARS: usize = 200;
const TRIGGERS: [&str; 3] = ["always", "never", "must"];

pub struct Found {
    pub text: String,
    pub polarity: &'static str,
}

fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_ascii_alphabetic()).filter(|w| !w.is_empty()).map(|w| w.to_ascii_lowercase()).collect()
}

/// The words that carry the meaning: 4 letters or more, not the trigger words themselves.
fn content_words(s: &str) -> Vec<String> {
    let mut v: Vec<String> = tokens(s).into_iter().filter(|w| w.len() >= 4 && !TRIGGERS.contains(&w.as_str())).collect();
    v.sort();
    v.dedup();
    v
}

fn polarity(sentence: &str) -> Option<&'static str> {
    let t = tokens(sentence);
    if !t.iter().any(|w| TRIGGERS.contains(&w.as_str())) {
        return None;
    }
    let must_not = t.windows(2).any(|w| w[0] == "must" && w[1] == "not");
    Some(if must_not || t.iter().any(|w| w == "never") { "dont" } else { "do" })
}

/// A list marker or a quote mark in front of a sentence is not part of it.
fn strip_marker(line: &str) -> &str {
    let l = line.trim_start().trim_start_matches(['-', '*', '•', '>', '#']).trim_start();
    let digits = l.bytes().take_while(|b| b.is_ascii_digit()).count();
    match l.as_bytes().get(digits) {
        Some(b'.') | Some(b')') if digits > 0 && l.as_bytes().get(digits + 1) == Some(&b' ') => l[digits + 1..].trim_start(),
        _ => l,
    }
}

/// The pieces of a line that end in . ! ? followed by a space or the end of the line (the last piece may have no end mark).
fn sentences(line: &str) -> Vec<&str> {
    let (mut out, mut start) = (Vec::new(), 0);
    let mut it = line.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if matches!(c, '.' | '!' | '?') && it.peek().is_none_or(|n| n.1.is_whitespace()) {
            out.push(&line[start..i + c.len_utf8()]);
            start = i + c.len_utf8();
        }
    }
    if start < line.len() {
        out.push(&line[start..]);
    }
    out
}

/// The lines of a text that are not inside a ``` or ~~~ code fence (the fence lines go too).
pub fn outside_fences(text: &str) -> String {
    let mut fence = false;
    let mut keep: Vec<&str> = Vec::new();
    for line in text.lines() {
        let l = line.trim_start();
        if l.starts_with("```") || l.starts_with("~~~") {
            fence = !fence;
        } else if !fence {
            keep.push(line);
        }
    }
    keep.join("\n")
}

/// The sentences of a prompt that are constraints, in order. Code fences and questions are skipped.
pub fn extract(prompt: &str) -> Vec<Found> {
    let mut out = Vec::new();
    for line in outside_fences(prompt).lines() {
        sentences(strip_marker(line)).into_iter().for_each(|s| sentence(s, &mut out));
    }
    out
}

fn sentence(raw: &str, out: &mut Vec<Found>) {
    let s = raw.trim();
    if s.ends_with('?') {
        return;
    }
    let s = s.trim_end_matches(['.', '!', ' ']).trim();
    if s.is_empty() || s.chars().count() > MAX_CHARS {
        return;
    }
    if let Some(polarity) = polarity(s) {
        out.push(Found { text: s.to_string(), polarity });
    }
}

/// Store the constraints of a finished human turn; returns how many are new. Said twice, a sentence counts once while it is active.
pub fn record(db: &Connection, turn_id: i64) -> Result<usize> {
    let origin: Option<Option<String>> = db.query_row("SELECT origin FROM turns WHERE id=?1", [turn_id], |r| r.get(0)).optional()?;
    if origin.flatten().as_deref() != Some("human") {
        return Ok(0);
    }
    let Some(prompt) = texts::prompt_of(db, turn_id) else { return Ok(0) };
    let mut n = 0;
    for f in extract(&prompt) {
        let same: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM constraints WHERE superseded_by IS NULL AND lower(text)=lower(?1))", [&f.text], |r| r.get(0))?;
        if same {
            continue;
        }
        db.execute("INSERT INTO constraints(turn_id,text,polarity,source) VALUES(?1,?2,?3,'prompt')", params![turn_id, f.text, f.polarity])?;
        supersede(db, db.last_insert_rowid())?;
        n += 1;
    }
    Ok(n)
}

/// Spec 21: an older constraint of the opposite polarity that shares at least 60% of the content words of constraint `id` is replaced by it.
/// Only the user's own words replace anything: a `keep` written by the AI never takes the place of what the user said.
pub fn supersede(db: &Connection, id: i64) -> Result<()> {
    let row: Option<(String, String, String)> = db.query_row("SELECT text,polarity,source FROM constraints WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
    let Some((text, pol, source)) = row else { return Ok(()) };
    if !matches!(source.as_str(), "prompt" | "keep_user") {
        return Ok(());
    }
    let mine = content_words(&text);
    if mine.is_empty() {
        return Ok(());
    }
    let mut st = db.prepare("SELECT id,text FROM constraints WHERE id<?1 AND superseded_by IS NULL AND polarity<>?2")?;
    let older: Vec<(i64, String)> = st.query_map(params![id, pol], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
    for (oid, otext) in older {
        let theirs = content_words(&otext);
        let shared = mine.iter().filter(|w| theirs.contains(w)).count();
        if shared * 10 >= mine.len() * 6 {
            db.execute("UPDATE constraints SET superseded_by=?2 WHERE id=?1", params![oid, id])?;
        }
    }
    Ok(())
}

/// (active, replaced): what status says about the constraints, so the false-pin rate can be measured.
pub fn counts(db: &Connection) -> (i64, i64) {
    let q = |sql: &str| db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0);
    (q("SELECT COUNT(*) FROM constraints WHERE superseded_by IS NULL"), q("SELECT COUNT(*) FROM constraints WHERE superseded_by IS NOT NULL"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts_of(p: &str) -> Vec<(String, &'static str)> {
        extract(p).into_iter().map(|f| (f.text, f.polarity)).collect()
    }

    #[test]
    fn a_sentence_with_always_never_or_must_is_a_constraint() {
        let p = "Please fix the login page. Never touch the payments folder.\nAlways run the tests before you say you are done!\n- You MUST not delete the backup\n- the colour could change\nWhat must I do here?\n";
        assert_eq!(
            texts_of(p),
            vec![
                ("Never touch the payments folder".to_string(), "dont"),
                ("Always run the tests before you say you are done".to_string(), "do"),
                ("You MUST not delete the backup".to_string(), "dont"),
            ]
        );
        assert_eq!(texts_of("it must work")[0].1, "do");
    }

    #[test]
    fn code_fences_questions_long_sentences_and_other_words_are_skipped() {
        assert!(texts_of("```\nnever do this\n```\nplain text\n~~~\nalways that\n~~~").is_empty());
        assert!(texts_of("neverending story, mustard and alwaysome").is_empty());
        assert!(texts_of(&format!("never {}", "x".repeat(201))).is_empty());
        assert_eq!(texts_of(&format!("never {}", "x".repeat(194))).len(), 1);
        assert!(texts_of("Should I never do that?").is_empty());
        // Words in another language are not read (English only in S2).
        assert!(texts_of("אף פעם אל תגע בתיקייה").is_empty());
    }

    fn db_with(rows: &[(&str, &str, &str)]) -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE constraints(id INTEGER PRIMARY KEY, turn_id INTEGER, text TEXT NOT NULL, polarity TEXT NOT NULL DEFAULT 'do', source TEXT NOT NULL DEFAULT 'prompt', superseded_by INTEGER);").unwrap();
        for (t, p, s) in rows {
            db.execute("INSERT INTO constraints(text,polarity,source) VALUES(?1,?2,?3)", params![t, p, s]).unwrap();
            supersede(&db, db.last_insert_rowid()).unwrap();
        }
        db
    }

    fn state(db: &Connection) -> Vec<(i64, Option<i64>)> {
        let mut st = db.prepare("SELECT id,superseded_by FROM constraints ORDER BY id").unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect()
    }

    #[test]
    fn a_later_opposite_sentence_that_shares_60_percent_of_its_words_replaces_the_older() {
        // 3 content words (touch, payments, folder), all shared: replaced.
        let db = db_with(&[("never touch the payments folder", "dont", "prompt"), ("always touch the payments folder", "do", "prompt")]);
        assert_eq!(state(&db), vec![(1, Some(2)), (2, None)]);
        // The same polarity never replaces; an unrelated opposite sentence neither.
        let db = db_with(&[("never touch the payments folder", "dont", "prompt"), ("never touch the payments folder again", "dont", "prompt"), ("always run the tests", "do", "prompt")]);
        assert_eq!(state(&db), vec![(1, None), (2, None), (3, None)]);
        // 2 of 3 shared is 66%: replaced; 1 of 4 is 25%: kept.
        let db = db_with(&[("never edit the config file", "dont", "prompt"), ("always edit the config files", "do", "prompt")]);
        assert_eq!(state(&db), vec![(1, Some(2)), (2, None)]);
        let db = db_with(&[("never edit the config file", "dont", "prompt"), ("always edit prices tomorrow evening", "do", "prompt")]);
        assert_eq!(state(&db), vec![(1, None), (2, None)]);
        assert_eq!(counts(&db), (2, 0));
    }

    #[test]
    fn a_keep_written_by_the_ai_never_replaces_what_the_user_said() {
        let db = db_with(&[("never touch the payments folder", "dont", "prompt"), ("always touch the payments folder", "do", "keep_agent")]);
        assert_eq!(state(&db), vec![(1, None), (2, None)]);
        let db = db_with(&[("never touch the payments folder", "dont", "prompt"), ("always touch the payments folder", "do", "keep_user")]);
        assert_eq!(state(&db), vec![(1, Some(2)), (2, None)]);
        assert_eq!(counts(&db), (1, 1));
    }
}

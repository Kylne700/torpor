//! Tolerant reader of the session's own transcript (spec 4.4): who started the turn, whether a compaction happened, whether Torpor's
//! additionalContext arrived. Reads only type, subtype, uuid, promptId, turnOrigin, promptSource, isSidechain, isCompactSummary, agentId and
//! the attachment type and sizes. Keeps no text: the one comparison with text (the delivery check) is done in place and only says yes or no.
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// At most this much is read per call.
pub const MAX_READ: u64 = 4 << 20;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Found {
    /// human, task_notification, peer, sdk, scheduled or unknown
    pub origin: &'static str,
    /// prompt_id, order or none
    pub by: &'static str,
}

#[derive(Debug, Default)]
pub struct Read4 {
    pub found: Option<Found>,
    /// Where the next call starts (only whole lines are consumed).
    pub new_offset: i64,
}

fn map_origin(turn_origin: Option<&str>, prompt_source: Option<&str>) -> &'static str {
    let any = |w: &str| turn_origin == Some(w) || prompt_source == Some(w);
    if turn_origin == Some("human") && prompt_source == Some("typed") {
        "human"
    } else if any("sdk") {
        "sdk"
    } else if any("task_notification") {
        "task_notification"
    } else if any("peer") {
        "peer"
    } else if any("scheduled") {
        "scheduled"
    } else {
        // No label, or `queued`: not human.
        "unknown"
    }
}

/// Look for the turn's user entry from `offset` on. Bad lines are skipped; entries are deduped by uuid.
pub fn read_origin(path: &Path, offset: i64, prompt_id: Option<&str>) -> Read4 {
    let mut out = Read4 { found: None, new_offset: offset.max(0) };
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return out,
    };
    let len = match f.metadata() {
        Ok(m) if m.is_file() => m.len(),
        _ => return out,
    };
    let mut start = offset.max(0) as u64;
    if start > len {
        start = 0; // the file was replaced or truncated
    }
    // Far behind: only the newest part matters, so skip ahead (and drop the partial first line).
    let skipped = len.saturating_sub(start) > MAX_READ;
    if skipped {
        start = len - MAX_READ;
    }
    if f.seek(SeekFrom::Start(start)).is_err() {
        return out;
    }
    let mut buf = Vec::new();
    if f.by_ref().take(MAX_READ).read_to_end(&mut buf).is_err() {
        return out;
    }
    let mut from = 0usize;
    if skipped {
        match buf.iter().position(|b| *b == b'\n') {
            Some(i) => from = i + 1,
            None => {
                out.new_offset = (start + buf.len() as u64) as i64;
                return out;
            }
        }
    }
    let end = match buf.iter().rposition(|b| *b == b'\n') {
        Some(i) => i + 1,
        // A single line longer than the limit would block us forever: step over it.
        None if buf.len() as u64 >= MAX_READ => buf.len(),
        None => {
            out.new_offset = start as i64;
            return out;
        }
    };
    out.new_offset = (start + end as u64) as i64;
    let mut seen: HashSet<String> = HashSet::new();
    let mut by_prompt: Option<Found> = None;
    let mut newest: Option<Found> = None;
    for line in buf[from..end].split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_slice(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        if let Some(u) = v.get("uuid").and_then(|u| u.as_str()) {
            if !seen.insert(u.to_string()) {
                continue;
            }
        }
        let to = v.get("turnOrigin").and_then(|x| x.as_str());
        let ps = v.get("promptSource").and_then(|x| x.as_str());
        let labeled = to.is_some() || ps.is_some();
        let side = v.get("isSidechain").and_then(|x| x.as_bool()).unwrap_or(false);
        let compact = v.get("isCompactSummary").and_then(|x| x.as_bool()).unwrap_or(false);
        let pid = v.get("promptId").and_then(|x| x.as_str());
        if prompt_id.is_some() && pid == prompt_id && labeled && !side {
            by_prompt = Some(Found { origin: map_origin(to, ps), by: "prompt_id" });
        }
        if labeled && !side && !compact {
            newest = Some(Found { origin: map_origin(to, ps), by: "order" });
        }
    }
    out.found = by_prompt.or(newest);
    out
}

// ---------------------------------------------------------------- compaction and delivery (spec 21)

/// The whole lines of `path` from byte `from` on (at most the last `max` bytes), and the file offset of the first one.
/// A line still being written (no newline yet) is left out. None when the file cannot be read.
fn whole_lines(path: &Path, from: u64, max: u64) -> Option<(Vec<u8>, u64)> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok().filter(|m| m.is_file())?.len();
    let mut start = if from > len { 0 } else { from }; // the file was replaced or truncated
    let cut = len - start > max;
    if cut {
        start = len - max;
    }
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf).ok()?;
    let mut base = start;
    if cut {
        // The first line is probably a partial one.
        let i = buf.iter().position(|b| *b == b'\n').map_or(buf.len(), |i| i + 1);
        buf.drain(..i);
        base += i as u64;
    }
    let keep = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    buf.truncate(keep);
    Some((buf, base))
}

fn has(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// The strings of an attachment's `content` (an array of strings, or one string).
fn content_strings(att: &serde_json::Value) -> Vec<&str> {
    match att.get("content") {
        Some(serde_json::Value::String(s)) => vec![s.as_str()],
        Some(serde_json::Value::Array(a)) => a.iter().filter_map(|x| x.as_str()).collect(),
        _ => Vec::new(),
    }
}

fn context_attachment(v: &serde_json::Value) -> Option<&serde_json::Value> {
    (v.get("type").and_then(|t| t.as_str()) == Some("attachment")).then(|| v.get("attachment")).flatten().filter(|a| a.get("type").and_then(|t| t.as_str()) == Some("hook_additional_context"))
}

/// The id of the newest compaction in the main conversation (the last 4 MiB of the transcript). A compaction inside a subagent
/// (`agentId` set, or a sidechain entry) is not one of the conversation's own. None: no such record, or the file cannot be read.
pub fn newest_boundary(path: &Path) -> Option<String> {
    let (buf, base) = whole_lines(path, 0, MAX_READ)?;
    let (mut found, mut at) = (None, base);
    for line in buf.split_inclusive(|b| *b == b'\n') {
        let here = at;
        at += line.len() as u64;
        if !has(line, b"compact_boundary") {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else { continue };
        let is = |k: &str, w: &str| v.get(k).and_then(|x| x.as_str()) == Some(w);
        if !(is("type", "system") && is("subtype", "compact_boundary")) {
            continue;
        }
        if v.get("agentId").is_some_and(|a| !a.is_null()) || v.get("isSidechain").and_then(|x| x.as_bool()) == Some(true) {
            continue;
        }
        found = Some(v.get("uuid").and_then(|u| u.as_str()).map_or_else(|| format!("@{here}"), |u| u.to_string()));
    }
    found
}

/// The delivery check (spec 21): does a `hook_additional_context` attachment written after byte `from` hold `marker`?
/// None when the transcript cannot be read (try again later); Some(false) when it was read and the text is not there.
pub fn holds_context(path: &Path, from: u64, marker: &str) -> Option<bool> {
    let (buf, _) = whole_lines(path, from, 2 * MAX_READ)?;
    for line in buf.split(|b| *b == b'\n') {
        if !(has(line, b"hook_additional_context") && has(line, marker.as_bytes())) {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else { continue };
        if context_attachment(&v).is_some_and(|a| content_strings(a).iter().any(|s| s.contains(marker))) {
            return Some(true);
        }
    }
    Some(false)
}

/// The length of the transcript now (where the next delivery check starts), or None.
pub fn len_of(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().filter(|m| m.is_file()).map(|m| m.len())
}

/// One `hook_additional_context` attachment: how big it is, and which prompt it follows.
#[derive(Debug, PartialEq, Eq)]
pub struct Block {
    /// The promptId of the latest user entry before it (by order in the file; not checked against Claude Code's own bookkeeping).
    pub prompt_id: Option<String>,
    pub chars: usize,
    /// Starts with "Torpor": ours.
    pub torpor: bool,
}

/// The context blocks hooks added in the last 4 MiB of the transcript, in file order. Sizes only; no text leaves this function.
pub fn context_blocks(path: &Path) -> Option<Vec<Block>> {
    let (buf, _) = whole_lines(path, 0, MAX_READ)?;
    let (mut out, mut prompt): (Vec<Block>, Option<String>) = (Vec::new(), None);
    for line in buf.split(|b| *b == b'\n') {
        let user = has(line, b"\"promptId\"");
        if !(user || has(line, b"hook_additional_context")) {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else { continue };
        if let Some(att) = context_attachment(&v) {
            let parts = content_strings(att);
            let chars = parts.iter().map(|s| s.chars().count()).sum();
            out.push(Block { prompt_id: prompt.clone(), chars, torpor: parts.first().is_some_and(|s| s.starts_with("Torpor")) });
        } else if v.get("type").and_then(|t| t.as_str()) == Some("user") && v.get("isSidechain").and_then(|x| x.as_bool()) != Some(true) {
            if let Some(p) = v.get("promptId").and_then(|x| x.as_str()) {
                prompt = Some(p.to_string());
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn matches_by_prompt_id_then_order_and_skips_garbage() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, r#"{{"type":"user","uuid":"a","promptId":"p1","turnOrigin":"human","promptSource":"typed"}}"#).unwrap();
        writeln!(f, "not json at all").unwrap();
        writeln!(f, r#"{{"type":"user","uuid":"b","promptId":"p1","message":"tool result, no labels"}}"#).unwrap();
        writeln!(f, r#"{{"type":"user","uuid":"c","promptId":"p2","turnOrigin":"sdk","promptSource":"sdk"}}"#).unwrap();
        write!(f, r#"{{"type":"user","uuid":"d","promptId":"p3","turnOr"#).unwrap(); // partial last line
        drop(f);
        let r = read_origin(&p, 0, Some("p1"));
        assert_eq!(r.found, Some(Found { origin: "human", by: "prompt_id" }));
        let r2 = read_origin(&p, 0, Some("p2"));
        assert_eq!(r2.found, Some(Found { origin: "sdk", by: "prompt_id" }));
        // unknown id: newest labeled entry, by order
        let r3 = read_origin(&p, 0, Some("zzz"));
        assert_eq!(r3.found, Some(Found { origin: "sdk", by: "order" }));
        // the partial last line is not consumed
        let consumed = std::fs::read(&p).unwrap().iter().rposition(|b| *b == b'\n').unwrap() + 1;
        assert_eq!(r3.new_offset as usize, consumed);
    }

    fn lines(p: &std::path::Path, ls: &[&str]) {
        let mut f = std::fs::File::create(p).unwrap();
        for l in ls {
            writeln!(f, "{l}").unwrap();
        }
    }

    #[test]
    fn the_newest_boundary_is_the_main_conversations_not_a_subagents() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.jsonl");
        assert_eq!(newest_boundary(&p), None, "no file");
        lines(&p, &[r#"{"type":"user","uuid":"u1","promptId":"p1"}"#]);
        assert_eq!(newest_boundary(&p), None, "no compaction yet");
        lines(
            &p,
            &[
                r#"{"type":"system","subtype":"compact_boundary","uuid":"b1"}"#,
                "garbage",
                r#"{"type":"system","subtype":"compact_boundary","uuid":"s1","agentId":"agent-9"}"#,
                r#"{"type":"system","subtype":"compact_boundary","uuid":"s2","isSidechain":true}"#,
                r#"{"type":"system","subtype":"other","uuid":"x"}"#,
                r#"{"type":"assistant","note":"compact_boundary mentioned in text"}"#,
            ],
        );
        assert_eq!(newest_boundary(&p).as_deref(), Some("b1"));
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, r#"{{"type":"system","subtype":"compact_boundary","uuid":"b2"}}"#).unwrap();
        write!(f, r#"{{"type":"system","subtype":"compact_boundary","uuid":"b3"#).unwrap(); // still being written
        assert_eq!(newest_boundary(&p).as_deref(), Some("b2"));
    }

    #[test]
    fn the_delivery_check_needs_an_attachment_after_the_start_offset() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.jsonl");
        let att = |text: &str| format!(r#"{{"type":"attachment","attachment":{{"type":"hook_additional_context","content":["{text}"]}}}}"#);
        lines(&p, &[&att("Torpor (after compaction 1, valid as of turn 4). old"), r#"{"type":"user","uuid":"u"}"#]);
        let off = len_of(&p).unwrap();
        // Before the offset does not count; a user line that only quotes the text does not count.
        assert_eq!(holds_context(&p, off, "Torpor (after compaction 1"), Some(false));
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, r#"{{"type":"user","message":"hook_additional_context Torpor (after compaction 2 quoted"}}"#).unwrap();
        assert_eq!(holds_context(&p, off, "Torpor (after compaction 2"), Some(false));
        writeln!(f, "{}", att("Torpor (after compaction 2, valid as of turn 9). new")).unwrap();
        writeln!(f, r#"{{"type":"attachment","attachment":{{"type":"hook_additional_context","content":"Torpor (after clear), as one string"}}}}"#).unwrap();
        assert_eq!(holds_context(&p, off, "Torpor (after compaction 2"), Some(true));
        assert_eq!(holds_context(&p, off, "Torpor (after clear"), Some(true));
        assert_eq!(holds_context(&p, 0, "Torpor (after compaction 1"), Some(true));
        // A shrunk file is read from the start; a missing file is "cannot tell".
        assert_eq!(holds_context(&p, off + 1_000_000, "Torpor (after compaction 2"), Some(true));
        assert_eq!(holds_context(&d.path().join("none.jsonl"), 0, "x"), None);
    }

    #[test]
    fn context_blocks_give_sizes_by_prompt_and_say_whose_they_are() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.jsonl");
        let att = |text: &str| format!(r#"{{"type":"attachment","attachment":{{"type":"hook_additional_context","content":["{text}"]}}}}"#);
        lines(
            &p,
            &[
                r#"{"type":"user","uuid":"a","promptId":"p1"}"#,
                &att("Torpor: one line"),
                &att("context-mode says hello"),
                r#"{"type":"user","uuid":"b","promptId":"p2","isSidechain":true}"#,
                r#"{"type":"user","uuid":"c","promptId":"p3"}"#,
                &att("other"),
            ],
        );
        let b = context_blocks(&p).unwrap();
        assert_eq!(
            b,
            vec![
                Block { prompt_id: Some("p1".into()), chars: 16, torpor: true },
                Block { prompt_id: Some("p1".into()), chars: 23, torpor: false },
                Block { prompt_id: Some("p3".into()), chars: 5, torpor: false },
            ]
        );
    }
}

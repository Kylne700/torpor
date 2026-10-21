//! Tolerant reader of the session's own transcript (spec 4.4): who started the turn.
//! Reads only type, uuid, promptId, turnOrigin, promptSource, isSidechain, isCompactSummary. Keeps no text.
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
}

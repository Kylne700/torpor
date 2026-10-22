//! `torpor log` and `torpor show` (spec section 8): the turns, and the files of one turn or one save point, in plain words.
//! Reads only. Secret files are never opened; only plain text up to 1 MiB is compared to count lines.
use crate::restore::describe;
use crate::turn::{self, Net};
use crate::{scan, status};
use crate::store::*;
use crate::util::*;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

const TEXT_MAX: usize = 1 << 20;

/// One line of `log`: a turn, or changes that belong to no turn (outside the chat, a rewind, a restore).
pub struct Row {
    pub turn: Option<i64>,
    pub point: Option<i64>,
    pub at: i64,
    pub who: String,
    pub preview: Option<String>,
    /// (class, path)
    pub files: Vec<(String, String)>,
}

fn who(origin: &str) -> String {
    match origin {
        "human" => "you",
        "task_notification" => "task",
        "scheduled" => "timer",
        "unknown" | "none" => "?",
        o => o,
    }
    .to_string()
}

fn gap_label(class: &str) -> &'static str {
    match class {
        "shared" => "changed by another session",
        "rewind" => "rewound by Claude Code",
        "background" => "changed by a background task",
        "restore" => "put back by Torpor",
        _ => "changed outside the chat",
    }
}

fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The newest `limit` rows, newest first.
pub fn log_rows(db: &Connection, limit: usize) -> Result<Vec<Row>> {
    let mut rows: Vec<Row> = Vec::new();
    let mut st = db.prepare("SELECT id,n,COALESCE(started_at,0),origin,preview FROM turns ORDER BY id DESC LIMIT ?1")?;
    let turns: Vec<(i64, i64, i64, String, Option<String>)> = st.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<std::result::Result<_, _>>()?;
    for (id, n, at, origin, preview) in turns {
        let files = turn::net_changes(db, id, "'edit','shell','subagent'")?.into_iter().map(|c| (c.class, c.path)).collect();
        rows.push(Row { turn: Some(n), point: None, at, who: who(&origin), preview, files });
    }
    let mut st = db.prepare("SELECT p.id,p.created_at,c.class,c.path FROM changes c JOIN points p ON p.id=c.point_id WHERE c.turn_id IS NULL ORDER BY p.id DESC,c.path")?;
    let mut q = st.query([])?;
    let mut gaps: Vec<Row> = Vec::new();
    while let Some(r) = q.next()? {
        let (pid, at, class, path): (i64, i64, String, String) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
        if gaps.last().is_none_or(|g| g.point != Some(pid)) {
            if gaps.len() >= limit {
                break;
            }
            gaps.push(Row { turn: None, point: Some(pid), at, who: "-".into(), preview: None, files: Vec::new() });
        }
        if let Some(g) = gaps.last_mut() {
            g.files.push((class, path));
        }
    }
    rows.extend(gaps);
    rows.sort_by_key(|r| std::cmp::Reverse(r.at));
    rows.truncate(limit);
    Ok(rows)
}

fn what(r: &Row) -> String {
    let rank = |c: &str| match c {
        "edit" => 0,
        "shell" => 1,
        "subagent" => 2,
        _ => 3,
    };
    let mut order: Vec<&str> = Vec::new();
    for (c, _) in &r.files {
        if !order.contains(&c.as_str()) {
            order.push(c);
        }
    }
    order.sort_by_key(|c| rank(c));
    let parts: Vec<String> = order
        .iter()
        .map(|c| {
            let list: Vec<&str> = r.files.iter().filter(|(k, _)| k == c).map(|(_, p)| base(p)).collect();
            let more = if list.len() > 3 { format!(" +{} more", list.len() - 3) } else { String::new() };
            let label = match (r.turn, *c) {
                (None, c) => gap_label(c),
                (_, "edit") => "edits",
                (_, "shell") => "shell",
                _ => "subagent",
            };
            format!("{label}: {}{more}", list.iter().take(3).copied().collect::<Vec<_>>().join(", "))
        })
        .collect();
    if parts.is_empty() { r.preview.as_deref().map(|p| format!("\"{p}\"")).unwrap_or_default() } else { parts.join(" · ") }
}

pub fn render_log(rows: &[Row], now: i64) -> String {
    if rows.is_empty() {
        return "No turns yet. Torpor saves a point at both ends of every turn.\n".to_string();
    }
    let mut o = String::new();
    for r in rows {
        let n = r.turn.map_or("·".to_string(), |n| n.to_string());
        let files = match r.files.len() {
            0 => "no files".to_string(),
            1 => "1 file".to_string(),
            k => format!("{k} files"),
        };
        o.push_str(&format!("{n:>3}  {:<12}  {:<5} {files:<9} {}\n", local_stamp(r.at, now), r.who, what(r)));
    }
    let mut turns = rows.iter().filter_map(|r| r.turn);
    if let Some(a) = turns.next() {
        o.push_str(&format!("Undo only turn {a}: torpor undo {a}"));
        if let Some(b) = turns.next() {
            o.push_str(&format!(" · Back to the end of turn {b}: torpor back --turn {b}"));
        }
        o.push('\n');
    }
    o
}

pub fn log_json(rows: &[Row]) -> Value {
    Value::Array(
        rows.iter()
            .map(|r| {
                json!({
                    "turn": r.turn, "point": r.point, "at_ms": r.at / 1_000_000, "who": r.who, "preview": r.preview,
                    "files": r.files.iter().map(|(c, p)| json!({"class": c, "path": p})).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

// ---------------------------------------------------------------- show

pub struct FileRow {
    pub path: String,
    pub class: String,
    /// new, deleted, changed, mode, not saved
    pub change: &'static str,
    pub added: Option<usize>,
    pub removed: Option<usize>,
    pub command: Option<String>,
    /// Why it was not saved (change == "not saved").
    pub why: Option<String>,
}

fn counts(a: &str, b: &str) -> (usize, usize) {
    let (mut add, mut del) = (0, 0);
    for h in diffy::create_patch(a, b).hunks() {
        for l in h.lines() {
            match l {
                diffy::Line::Insert(_) => add += 1,
                diffy::Line::Delete(_) => del += 1,
                _ => {}
            }
        }
    }
    (add, del)
}

fn file_row(ws: &Ws, n: &Net) -> FileRow {
    let (old, new) = (&n.old.0, &n.new.0);
    let secret = scan::is_secret(&n.path);
    let text = |id: &Option<String>| -> Option<String> {
        if secret {
            return None;
        }
        let b = ws.read_blob(&ws.conn, &unhex32(id.as_deref()?)?).ok()?;
        if b.len() > TEXT_MAX || b.contains(&0) {
            return None;
        }
        String::from_utf8(b).ok()
    };
    let reason = [old, new].into_iter().find_map(|i| i.as_deref().and_then(|i| i.strip_prefix("x:")));
    let (change, c): (&'static str, Option<(usize, usize)>) = if reason.is_some() {
        ("not saved", None)
    } else {
        match (old, new) {
            (None, Some(_)) => ("new", text(new).map(|t| counts("", &t))),
            (Some(_), None) => ("deleted", text(old).map(|t| counts(&t, ""))),
            (Some(a), Some(b)) if a != b => ("changed", text(old).zip(text(new)).map(|(x, y)| counts(&x, &y))),
            _ => ("mode", None),
        }
    };
    let command = n.tool_use_id.as_deref().filter(|_| n.class == "shell").and_then(|t| {
        let c: Option<String> = ws.conn.query_row("SELECT cmd_preview FROM tool_events WHERE tool_use_id=?1 AND cmd_preview IS NOT NULL LIMIT 1", [t], |r| r.get(0)).optional().ok().flatten();
        c.map(|c| if c.chars().count() > 60 { format!("{}…", trunc_chars(&c, 60)) } else { c })
    });
    let why = reason.map(|r| status::why(r, &n.path, None));
    FileRow { path: n.path.clone(), class: n.class.clone(), change, added: c.map(|x| x.0), removed: c.map(|x| x.1), command, why }
}

fn detail(r: &FileRow) -> String {
    let mut d = match (r.change, r.added, r.removed) {
        ("new", Some(a), _) => format!("new, +{a} -0"),
        ("new", ..) => "new".to_string(),
        ("deleted", ..) => "deleted".to_string(),
        ("changed", Some(a), Some(b)) => format!("+{a} -{b}"),
        ("changed", ..) => "changed".to_string(),
        ("mode", ..) => "permissions changed".to_string(),
        _ => format!("not saved ({})", r.why.as_deref().unwrap_or("unknown reason")),
    };
    if scan::is_secret(&r.path) && r.change != "deleted" {
        d.push_str(" (secret file, content not shown)");
    }
    if let Some(c) = &r.command {
        d.push_str(&format!(" ({c})"));
    }
    match r.class.as_str() {
        "outside" => d.push_str(" (changed during the turn, not by the AI; undo leaves it)"),
        "shared" => d.push_str(" (another session was working here; undo leaves it)"),
        _ => {}
    }
    d
}

fn render_files(rows: &[FileRow]) -> String {
    if rows.is_empty() {
        return "  No files changed.\n".to_string();
    }
    let width = rows.iter().map(|r| r.path.chars().count()).max().unwrap_or(0).min(40);
    let mut o = String::new();
    for r in rows {
        let label = if r.class == "subagent" { "agent" } else { r.class.as_str() };
        o.push_str(&format!("  {label:<7} {:<width$}  {}\n", r.path, detail(r)));
    }
    o
}

fn files_json(rows: &[FileRow]) -> Value {
    Value::Array(rows.iter().map(|r| json!({"path": r.path, "class": r.class, "change": r.change, "added": r.added, "removed": r.removed, "command": r.command})).collect())
}

fn point_texts(db: &Connection, where_sql: &str, arg: i64) -> Result<Vec<(i64, String, String)>> {
    let mut st = db.prepare(&format!("SELECT id,kind,note FROM points WHERE {where_sql}=?1 AND visible=1 AND dropped=0 ORDER BY id"))?;
    let rows = st.query_map([arg], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?)))?;
    let mut out = Vec::new();
    for r in rows {
        let (id, kind, note) = r?;
        let text = match (kind.as_str(), note) {
            ("start", _) => "start".to_string(),
            ("end", _) => "end".to_string(),
            ("pre_bash", Some(n)) if !n.trim().is_empty() => format!("before {}", if n.chars().count() > 50 { format!("{}…", trunc_chars(&n, 50)) } else { n }),
            ("pre_bash", _) => "before a shell command".to_string(),
            (k, _) => k.replace('_', " "),
        };
        out.push((id, kind, text));
    }
    Ok(out)
}

/// Files of turn number `n`: (text, json), or None when there is no such turn.
pub fn show_turn(ws: &Ws, n: i64, now: i64) -> Result<Option<(String, Value)>> {
    let db = &ws.conn;
    type T = (i64, i64, String, Option<String>, Option<i64>, Option<i64>);
    let row: Option<T> = db
        .query_row("SELECT id,COALESCE(started_at,0),origin,preview,start_point,end_point FROM turns WHERE n=?1", [n], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
        .optional()?;
    let Some((id, at, origin, preview, start_point, end_point)) = row else { return Ok(None) };
    let files: Vec<FileRow> = turn::net_changes(db, id, "'edit','shell','subagent','outside','shared'")?.iter().map(|c| file_row(ws, c)).collect();
    let said = match &preview {
        Some(p) => format!("{}: \"{}{}\"", who(&origin), p.trim_end(), if p.chars().count() >= 60 { "…" } else { "" }),
        None => who(&origin),
    };
    let points = point_texts(db, "turn_id", id)?;
    let mut o = format!("Turn {n} · {} · {said}\n{}", local_stamp(at, now), render_files(&files));
    if !points.is_empty() {
        o.push_str(&format!("Save points: {}\n", points.iter().map(|(i, _, t)| format!("{i} {t}")).collect::<Vec<_>>().join(" · ")));
    }
    o.push_str(&format!("Undo only this turn: torpor undo {n}\n"));
    let j = json!({
        "turn": {"n": n, "at_ms": at / 1_000_000, "who": who(&origin), "preview": preview, "start_point": start_point, "end_point": end_point},
        "point": null,
        "files": files_json(&files),
        "points": points.iter().map(|(i, k, t)| json!({"id": i, "kind": k, "text": t})).collect::<Vec<_>>(),
    });
    Ok(Some((o, j)))
}

/// Files that one save point carries (what changed since the point before it): (text, json), or None when there is no such point.
pub fn show_point(ws: &Ws, id: i64, now: i64) -> Result<Option<(String, Value)>> {
    let db = &ws.conn;
    if !db.query_row("SELECT EXISTS(SELECT 1 FROM points WHERE id=?1)", [id], |r| r.get::<_, bool>(0))? {
        return Ok(None);
    }
    let mut st = db.prepare("SELECT path,old_id,old_mode,new_id,new_mode,class,tool_use_id FROM changes WHERE point_id=?1 ORDER BY path")?;
    let nets: Vec<Net> = st
        .query_map([id], |r| Ok(Net { path: r.get(0)?, old: (r.get(1)?, r.get(2)?), new: (r.get(3)?, r.get(4)?), class: r.get(5)?, tool_use_id: r.get(6)? }))?
        .collect::<std::result::Result<_, _>>()?;
    let files: Vec<FileRow> = nets.iter().map(|c| file_row(ws, c)).collect();
    let d = describe(db, id, now);
    let o = format!("Save point {id} · {d}\n{}Go back to it: torpor back --id {id}\n", render_files(&files));
    let j = json!({"turn": null, "point": {"id": id, "text": d}, "files": files_json(&files), "points": []});
    Ok(Some((o, j)))
}

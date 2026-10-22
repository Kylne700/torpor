//! The half-rewind notice (spec 22, B10). Claude Code's own /rewind puts back the files its edit tools wrote, but not what a
//! shell command or a subagent changed. When the gap before a prompt holds such a rewind, say once which files stayed as they were.
use crate::capture;
use crate::push;
use crate::store::*;
use crate::turn::Turn;
use crate::util::*;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeSet;

pub const KIND: &str = "rewind";
/// Spec 22: at most 40 tokens (tokens = chars / 4).
const MAX_CHARS: usize = 160;

/// The line for the prompt that starts `turn`, or None: no rewind in the gap, nothing left standing, or already said.
/// The text goes to the person and to the model; it is logged as a notice that goes out even over budget.
pub fn line(ws: &Ws, db: &Connection, session: &str, turn: &Turn) -> Result<Option<String>> {
    let Some(start) = turn.start_point else { return Ok(None) };
    let note: Option<String> = db.query_row("SELECT note FROM points WHERE id=?1", [start], |r| r.get(0)).optional()?.flatten();
    let Some(k) = note.as_deref().and_then(|n| n.strip_prefix("rewind:")).and_then(|k| k.parse::<i64>().ok()) else { return Ok(None) };
    if db.query_row("SELECT EXISTS(SELECT 1 FROM push_log WHERE turn_id=?1 AND kind=?2)", params![turn.id, KIND], |r| r.get::<_, bool>(0))? {
        return Ok(None);
    }
    let point = |id: i64| -> Result<Option<(String, [u8; 32], Option<i64>)>> {
        let row = db
            .query_row("SELECT p.kind,p.tree_id,t.n FROM points p LEFT JOIN turns t ON t.id=p.turn_id WHERE p.id=?1", [id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, Option<i64>>(2)?)))
            .optional()?;
        Ok(row.and_then(|(kind, tree, n)| blob_id_from_vec(&tree).map(|t| (kind, t, n))))
    };
    let (Some((kind, ktree, kturn)), Some((_, now_tree, _))) = (point(k)?, point(start)?) else { return Ok(None) };
    // `torpor back --turn K` takes a turn number: the newest earlier turn that ended in exactly this state; else the save point itself.
    let end_turn: Option<i64> = db
        .query_row(
            "SELECT t.n FROM turns t JOIN points e ON e.id=t.end_point WHERE e.tree_id=?1 AND e.dropped=0 AND t.n<?2 ORDER BY t.n DESC LIMIT 1",
            params![&ktree[..], turn.n],
            |r| r.get(0),
        )
        .optional()?;
    let (after, hint) = match end_turn {
        Some(n) => (n, format!("torpor back --turn {n}")),
        None => (kturn.map_or(0, |n| if kind == "end" { n } else { n - 1 }), format!("torpor back --id {k}")),
    };
    let mut st = db.prepare("SELECT c.path,t.n,c.class FROM changes c JOIN turns t ON t.id=c.turn_id WHERE c.class IN ('shell','subagent') AND t.n>?1")?;
    let rows: Vec<(String, i64, String)> = st.query_map([after], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<_, _>>()?;
    let candidates: Vec<String> = rows.iter().map(|r| r.0.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    let left = capture::differing_paths(ws, db, &ktree, &now_tree, &candidates)?;
    if left.is_empty() {
        return Ok(None);
    }
    let mine: Vec<&(String, i64, String)> = rows.iter().filter(|r| left.contains(&r.0)).collect();
    let (lo, hi) = (mine.iter().map(|r| r.1).min().unwrap_or(after), mine.iter().map(|r| r.1).max().unwrap_or(after));
    let (shell, sub) = (mine.iter().any(|r| r.2 == "shell"), mine.iter().any(|r| r.2 == "subagent"));
    let what = match (shell, sub) {
        (true, false) => "shell-changed",
        (false, true) => "subagent-changed",
        _ => "shell/subagent-changed",
    };
    let turns = if lo == hi { format!("turn {lo}") } else { format!("turns {lo}-{hi}") };
    let n = left.len();
    let text = [3usize, 2, 1, 0].iter().find_map(|&names| {
        let list = if names == 0 { String::new() } else { format!(": {}", push::names_phrase_n(&left, names)) };
        let l = format!("Torpor: rewind left {n} {what} file{} from {turns}{list}; `{hint}` restores all", if n == 1 { "" } else { "s" });
        (l.chars().count() <= MAX_CHARS).then_some(l)
    });
    let Some(text) = text else { return Ok(None) };
    let p = push::Push { session, turn_id: turn.id, turn_n: turn.n, kind: KIND, text: &text, window: 0, must: true };
    Ok(push::admit(db, &p)?.then_some(text))
}

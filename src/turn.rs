//! Sessions, turns, heartbeat and tool-event rows (spec section 4). Plain SQL helpers; no scanning here.
use crate::util::*;
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Clone, Debug)]
pub struct Turn {
    pub id: i64,
    pub n: i64,
    pub session_id: String,
    pub prompt_id: Option<String>,
    pub end_kind: String,
    pub start_point: Option<i64>,
    pub end_point: Option<i64>,
}

const TURN_COLS: &str = "id,n,session_id,prompt_id,end_kind,start_point,end_point";

fn turn_of(r: &rusqlite::Row) -> rusqlite::Result<Turn> {
    Ok(Turn {
        id: r.get(0)?,
        n: r.get(1)?,
        session_id: r.get(2)?,
        prompt_id: r.get(3)?,
        end_kind: r.get(4)?,
        start_point: r.get(5)?,
        end_point: r.get(6)?,
    })
}

pub fn heartbeat(db: &Connection, session: &str, event: &str, now: i64) -> Result<()> {
    db.prepare_cached("INSERT INTO heartbeat(session_id,event,last_at,count) VALUES(?1,?2,?3,1) ON CONFLICT(session_id,event) DO UPDATE SET last_at=excluded.last_at,count=count+1")?
        .execute(params![session, event, now])?;
    Ok(())
}

pub struct SessionInfo<'a> {
    pub id: &'a str,
    pub transcript_path: Option<&'a str>,
    pub source: Option<&'a str>,
    pub permission_mode: Option<&'a str>,
}

/// Create or refresh the session row. A new SessionStart for a known session clears its ended mark (resume).
pub fn touch_session(db: &Connection, s: &SessionInfo, now: i64, restart: bool) -> Result<()> {
    // A new session has already "seen" every restore made before it existed.
    let seen: i64 = db.query_row("SELECT COALESCE(MAX(id),0) FROM restores", [], |r| r.get(0))?;
    db.prepare_cached("INSERT OR IGNORE INTO sessions(id,transcript_path,first_seen,last_seen,source,permission_mode,notified_restore_id) VALUES(?1,?2,?3,?3,?4,?5,?6)")?
        .execute(params![s.id, s.transcript_path, now, s.source, s.permission_mode, seen])?;
    db.prepare_cached("UPDATE sessions SET last_seen=?2, transcript_path=COALESCE(?3,transcript_path), permission_mode=COALESCE(?4,permission_mode), source=COALESCE(?5,source) WHERE id=?1")?
        .execute(params![s.id, now, s.transcript_path, s.permission_mode, s.source])?;
    if restart {
        db.prepare_cached("UPDATE sessions SET ended_at=NULL, end_reason=NULL WHERE id=?1")?
            .execute([s.id])?;
    }
    Ok(())
}

pub fn find(db: &Connection, session: &str, prompt_id: &str) -> Result<Option<Turn>> {
    Ok(db
        .prepare_cached(&format!("SELECT {TURN_COLS} FROM turns WHERE session_id=?1 AND prompt_id=?2 ORDER BY id DESC LIMIT 1"))?
        .query_row(params![session, prompt_id], turn_of)
        .optional()?)
}

/// The session's newest open turn.
pub fn open_of(db: &Connection, session: &str) -> Result<Option<Turn>> {
    Ok(db
        .prepare_cached(&format!("SELECT {TURN_COLS} FROM turns WHERE session_id=?1 AND end_kind='open' ORDER BY id DESC LIMIT 1"))?
        .query_row([session], turn_of)
        .optional()?)
}

/// The session's newest turn, open or ended.
pub fn newest_of(db: &Connection, session: &str) -> Result<Option<Turn>> {
    Ok(db
        .prepare_cached(&format!(
            "SELECT {TURN_COLS} FROM turns WHERE session_id=?1 ORDER BY id DESC LIMIT 1"
        ))?
        .query_row([session], turn_of)
        .optional()?)
}

pub fn by_id(db: &Connection, id: i64) -> Result<Option<Turn>> {
    Ok(db
        .prepare_cached(&format!("SELECT {TURN_COLS} FROM turns WHERE id=?1"))?
        .query_row([id], turn_of)
        .optional()?)
}

/// Number of prompts this session has submitted so far (for the `seq-<n>` key when prompt_id is missing).
pub fn count_of(db: &Connection, session: &str) -> Result<i64> {
    Ok(db.query_row(
        "SELECT COUNT(*) FROM turns WHERE session_id=?1",
        [session],
        |r| r.get(0),
    )?)
}

pub fn create(
    db: &Connection,
    session: &str,
    prompt_id: &str,
    now: i64,
    preview: Option<&str>,
    origin: &str,
) -> Result<Turn> {
    let n: i64 = db.query_row("SELECT COALESCE(MAX(n),0)+1 FROM turns", [], |r| r.get(0))?;
    db.prepare_cached("INSERT INTO turns(n,session_id,prompt_id,started_at,end_kind,origin,origin_by,preview) VALUES(?1,?2,?3,?4,'open',?5,'none',?6)")?
        .execute(params![n, session, prompt_id, now, origin, preview])?;
    Ok(Turn {
        id: db.last_insert_rowid(),
        n,
        session_id: session.to_string(),
        prompt_id: Some(prompt_id.to_string()),
        end_kind: "open".into(),
        start_point: None,
        end_point: None,
    })
}

pub fn set_start_point(db: &Connection, turn: i64, point: Option<i64>) -> Result<()> {
    db.prepare_cached("UPDATE turns SET start_point=?2 WHERE id=?1")?
        .execute(params![turn, point])?;
    Ok(())
}

pub fn close(
    db: &Connection,
    turn: i64,
    kind: &str,
    end_point: Option<i64>,
    now: i64,
    background_json: Option<&str>,
) -> Result<()> {
    db.prepare_cached("UPDATE turns SET end_kind=?2, ended_at=?3, end_point=COALESCE(?4,end_point), background_json=?5 WHERE id=?1")?
        .execute(params![turn, kind, now, end_point, background_json])?;
    Ok(())
}

/// A closed turn whose prompt_id shows up again (Stop-block continuation, task notification) is open again.
pub fn reopen(db: &Connection, turn: i64) -> Result<()> {
    db.prepare_cached("UPDATE turns SET end_kind='open', ended_at=NULL WHERE id=?1")?
        .execute([turn])?;
    Ok(())
}

pub fn bump_bash(db: &Connection, turn: i64) -> Result<()> {
    db.prepare_cached("UPDATE turns SET bash_count=bash_count+1 WHERE id=?1")?
        .execute([turn])?;
    Ok(())
}

pub fn set_origin(db: &Connection, turn: i64, origin: &str, by: &str) -> Result<()> {
    db.prepare_cached("UPDATE turns SET origin=?2, origin_by=?3 WHERE id=?1")?
        .execute(params![turn, origin, by])?;
    Ok(())
}

pub struct EventRow<'a> {
    pub session: &'a str,
    pub turn: Option<i64>,
    pub tool_use_id: Option<&'a str>,
    pub tool: &'a str,
    pub agent: Option<&'a str>,
    pub paths_json: Option<&'a str>,
    pub cmd_preview: Option<&'a str>,
    pub exit_code: Option<i64>,
    pub failed: bool,
    pub duration_ms: Option<i64>,
}

pub fn add_event(db: &Connection, e: &EventRow, now: i64) -> Result<i64> {
    db.prepare_cached("INSERT INTO tool_events(session_id,turn_id,tool_use_id,tool_name,agent_id,paths_json,cmd_preview,exit_code,failed,duration_ms,at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)")?
        .execute(params![e.session, e.turn, e.tool_use_id, e.tool, e.agent, e.paths_json, e.cmd_preview, e.exit_code, e.failed as i64, e.duration_ms, now])?;
    Ok(db.last_insert_rowid())
}

/// One file a turn changed, net: first old against last new (content and mode), with the class of the last change.
pub struct Net {
    pub path: String,
    pub old: (Option<String>, Option<i64>),
    pub new: (Option<String>, Option<i64>),
    pub class: String,
    pub tool_use_id: Option<String>,
}

/// Files of a turn whose changes have one of `classes` (an SQL list such as `'edit','shell'`), by path, without the ones that ended up as they began.
pub fn net_changes(db: &Connection, turn: i64, classes: &str) -> Result<Vec<Net>> {
    let mut st = db.prepare(&format!("SELECT path,old_id,old_mode,new_id,new_mode,class,tool_use_id FROM changes WHERE turn_id=?1 AND class IN ({classes}) ORDER BY point_id,rowid"))?;
    let mut net: std::collections::BTreeMap<String, Net> = Default::default();
    let mut rows = st.query([turn])?;
    while let Some(r) = rows.next()? {
        let path: String = r.get(0)?;
        let old = (r.get(1)?, r.get(2)?);
        let new = (r.get(3)?, r.get(4)?);
        let (class, tool_use_id): (String, Option<String>) = (r.get(5)?, r.get(6)?);
        match net.get_mut(&path) {
            Some(e) => {
                e.new = new;
                e.class = class;
                e.tool_use_id = tool_use_id;
            }
            None => {
                net.insert(
                    path.clone(),
                    Net {
                        path,
                        old,
                        new,
                        class,
                        tool_use_id,
                    },
                );
            }
        }
    }
    Ok(net.into_values().filter(|n| n.old != n.new).collect())
}

/// (edits, shell, subagent): files this turn changed, net per path, by the last change's class.
pub fn net_counts(db: &Connection, turn: i64) -> Result<(i64, i64, i64)> {
    let (mut edits, mut shell, mut sub) = (0, 0, 0);
    for n in net_changes(db, turn, "'edit','shell','subagent'")? {
        match n.class.as_str() {
            "edit" => edits += 1,
            "shell" => shell += 1,
            _ => sub += 1,
        }
    }
    Ok((edits, shell, sub))
}

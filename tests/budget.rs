//! B6 (spec 23, 25): a long session with many promises and many lines stays inside the budgets
//! (a line 150 tokens, a turn 1,000, a rebuild 2,000, a compaction cycle 4,000), and `torpor status` reports what the push log holds.
//! The lines come from the targeted line (B8): each of 70 promises watches its own file, and the AI edits 12 files a turn;
//! one turn edits all 70. The log is summed here, in Rust, and checked against what the hooks really said (the Sim's tape).
mod common;
use common::app::*;
use common::*;
use serde_json::Value;

const PROMISES: usize = 70;
const TURNS: usize = 100;
const STORM: usize = 3;
const COMPACT_AT: [usize; 2] = [34, 67];

fn tokens(s: &str) -> i64 {
    (s.chars().count() as i64 + 3) / 4
}

/// Tokens of the extra context in a slice of the tape.
fn said(tape: &[(String, String)]) -> i64 {
    tape.iter()
        .filter_map(|(_, out)| serde_json::from_str::<Value>(out.trim()).ok())
        .filter_map(|j| j.pointer("/hookSpecificOutput/additionalContext").and_then(|x| x.as_str().map(tokens)))
        .sum()
}

#[test]
fn b6_a_hundred_turns_with_seventy_promises_stay_inside_the_budgets_and_status_matches_the_log() {
    let mut s = Sim::new();
    s.session_start();
    // Their stored checks cannot be read, so the gate leaves them "unverified" at every Stop and moves on: no app is started.
    let sql: String = (1..=PROMISES)
        .map(|i| format!("INSERT INTO promises(name,source,tier,state,spec_id,watched_json,deps_json) VALUES('p{i:02}','agent','advisory','live','nospec','[\"f{i:02}.ts\"]','[\"f{i:02}.ts\"]');"))
        .collect();
    rw(&s).execute_batch(&sql).unwrap();

    let mut by_turn: Vec<(i64, i64)> = Vec::new(); // (turn id, tokens the hooks said in it)
    let mut rebuilds: Vec<i64> = Vec::new();
    for t in 1..=TURNS {
        if COMPACT_AT.contains(&t) {
            let text = Sim::context_of(&s.compaction(&format!("compact-{t}"))).expect("a rebuild after the compaction");
            rebuilds.push(tokens(&text));
            s.attach("SessionStart:compact", &text);
        }
        let from = s.tape.len();
        s.prompt(&format!("tweak the files, round {t}"));
        let files: Vec<usize> = if t == STORM { (1..=PROMISES).collect() } else { (0..12).map(|j| (t * 12 + j) % PROMISES + 1).collect() };
        for f in files {
            s.quick_edit(&format!("f{f:02}.ts"), &format!("round {t}\n"));
        }
        let turn_id = s.q_i64("SELECT MAX(id) FROM turns");
        by_turn.push((turn_id, said(&s.tape[from..])));
        s.stop();
    }

    // The log, read back row by row and summed here.
    let rows: Vec<(Option<i64>, i64, String, i64, Option<i64>, i64)> = {
        let c = s.conn();
        let mut st = c.prepare("SELECT turn_id, compact_n, kind, tokens, delivered, chars FROM push_log ORDER BY id").unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).unwrap().map(|x| x.unwrap()).collect()
    };
    let counted = |r: &&(Option<i64>, i64, String, i64, Option<i64>, i64)| r.4 != Some(0) && r.2 != "notice" && r.2 != "gate";
    let per_turn = |id: i64| -> i64 { rows.iter().filter(counted).filter(|r| r.0 == Some(id)).map(|r| r.3).sum() };
    let per_cycle = |n: i64| -> i64 { rows.iter().filter(counted).filter(|r| r.1 == n).map(|r| r.3).sum() };
    let refused = |f: &dyn Fn(&(Option<i64>, i64, String, i64, Option<i64>, i64)) -> bool| rows.iter().filter(|r| r.4 == Some(0) && f(r)).count();

    // A line is at most 150 tokens, a turn 1,000: the storm turn is cut by the turn budget while cycle 0 still has room.
    assert!(rows.iter().all(|r| r.3 <= 150 || r.2 == "rebuild"), "a line over 150 tokens");
    let storm_id = by_turn[STORM - 1].0;
    let storm = per_turn(storm_id);
    assert!((900..=1000).contains(&storm), "the storm turn said {storm} tokens");
    assert!(refused(&|r| r.0 == Some(storm_id)) >= 5, "the turn budget refused nothing in the storm turn");
    let most_turn = by_turn.iter().map(|(id, _)| per_turn(*id)).max().unwrap();
    assert!(most_turn <= 1000, "{most_turn} tokens in one turn");

    // Three cycles (two compactions): each ends at the cycle budget, none above it, and a rebuild is at most 2,000 tokens and 8,000 characters.
    let cycles: Vec<i64> = (0..=2).map(per_cycle).collect();
    assert!(cycles.iter().all(|c| (3800..=4000).contains(c)), "tokens per cycle {cycles:?}");
    for n in 0..=2 {
        assert!(refused(&|r| r.1 == n && r.2.starts_with("touch:")) >= 10, "the cycle budget refused (almost) nothing in cycle {n}");
    }
    let log_rebuilds: Vec<&(Option<i64>, i64, String, i64, Option<i64>, i64)> = rows.iter().filter(|r| r.2 == "rebuild").collect();
    assert_eq!(log_rebuilds.iter().map(|r| (r.1, r.3, r.4)).collect::<Vec<_>>(), vec![(1, rebuilds[0], Some(1)), (2, rebuilds[1], Some(1))], "rebuilds in the log");
    assert!(log_rebuilds.iter().all(|r| r.3 <= 2000 && r.5 <= 8000 && r.0.is_none()), "{log_rebuilds:?}");

    // What the hooks really said, turn by turn, is what the log holds (a line that was refused was not said).
    for (id, said_here) in &by_turn {
        assert_eq!(*said_here, per_turn(*id), "turn id {id}: the hooks said {said_here} tokens, the log holds {}", per_turn(*id));
    }

    // `torpor status` shows the same numbers.
    let o = s.cli(&["status", "--json"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(o.stdout.trim()).unwrap();
    let c = &v["context"];
    let total: i64 = rows.iter().filter(counted).map(|r| r.3).sum();
    assert_eq!(c["torpor_tokens"], total);
    assert_eq!(c["max_turn_tokens"], most_turn);
    assert_eq!((c["cycle"].as_i64(), c["cycle_tokens"].as_i64()), (Some(2), Some(cycles[2])));
    assert_eq!(c["max_cycle_tokens"], *cycles.iter().max().unwrap());
    assert_eq!((c["rebuilds"].as_i64(), c["rebuilds_not_delivered"].as_i64()), (Some(2), Some(0)));
    let text = s.status().stdout;
    assert!(text.contains(&format!("Torpor gave the AI {total} tokens in this session")) && text.contains(&format!("this compaction cycle {} of 4000 allowed", cycles[2])), "{text}");
    println!("B6: lines per turn <= {most_turn} tokens (storm turn {storm}, cap 1000), tokens per cycle {cycles:?} (cap 4000), rebuilds {rebuilds:?} tokens (cap 2000), session total {total}");
    assert!(s.faults().is_empty(), "{:?}", s.faults());
}

//! The rebuild after a compaction (spec 21, B5): what is sent and when, that a subagent's compaction gets nothing,
//! that the text is the same every time, and that the transcript is asked whether it arrived.
mod common;
use common::*;
use serde_json::{Value, json};

/// Two finished turns: a request with a standing constraint, then a second one with another constraint and a failing test run.
fn project(s: &mut Sim) {
    let o = s.session_start();
    assert!(
        Sim::context_of(&o).is_none(),
        "a plain start has no rebuild: {}",
        o.stdout
    );
    s.prompt("Please build the signup page. Never touch the payments folder.");
    s.edit("src/signup.ts", "export const a = 1;\n");
    s.stop();
    s.prompt("Always run the tests before you say you are done");
    s.edit("src/signup.ts", "export const a = 2;\n");
    s.outside_edit("run-tests.sh", "echo 'FAIL signup' >&2\nexit 1\n");
    assert_eq!(s.bash("sh run-tests.sh"), 1);
    s.stop();
}

fn status_json(s: &Sim) -> Value {
    let o = s.cli(&["status", "--json"]);
    assert_eq!(o.code, 0, "status --json: {}", o.stderr);
    serde_json::from_str(o.stdout.trim())
        .unwrap_or_else(|e| panic!("status --json is not JSON ({e}): {:?}", o.stdout))
}

fn rebuilds(s: &Sim, which: &str) -> i64 {
    s.q_i64(&format!(
        "SELECT COUNT(*) FROM push_log WHERE kind='rebuild' AND {which}"
    ))
}

/// The first line that starts with `start`.
fn line_with<'a>(text: &'a str, start: &str) -> &'a str {
    text.lines()
        .find(|l| l.starts_with(start))
        .unwrap_or_else(|| panic!("no line starts with {start:?} in:\n{text}"))
}

/// The lines under "Standing constraints:".
fn constraints_of(text: &str) -> Vec<String> {
    let mut it = text.lines().skip_while(|l| *l != "Standing constraints:");
    it.next();
    it.take_while(|l| {
        l.starts_with("(turn ") || l.starts_with("(kept)") || l.contains("older constraint")
    })
    .map(str::to_string)
    .collect()
}

fn pinned_pair() -> Vec<String> {
    vec![
        "(turn 1) \"Never touch the payments folder\"".to_string(),
        "(turn 2) \"Always run the tests before you say you are done\"".to_string(),
    ]
}

#[test]
fn b5_a_main_compaction_gets_the_rebuild_and_the_transcript_confirms_it() {
    let mut s = Sim::new();
    project(&mut s);
    s.prompt("now tweak the colours");
    s.edit("src/colours.ts", "export const c = 1;\n");
    let o = s.compaction("b1");
    let text = Sim::context_of(&o).unwrap_or_else(|| panic!("no rebuild: {}", o.stdout));
    assert!(text.starts_with("Torpor (after compaction 1, valid as of turn 3). From Torpor's records; quoted lines are the user's exact words.\n"), "{text}");
    assert_eq!(constraints_of(&text), pinned_pair(), "{text}");
    assert!(
        line_with(&text, "Goal, from the first request (turn 1): ")
            .contains("\"Please build the signup page. Never touch the payments folder.\""),
        "{text}"
    );
    assert!(
        line_with(
            &text,
            "turn 1 user \"Please build the signup page. Never touch the payments"
        )
        .contains("changed 1 file (1 edit): src/signup.ts"),
        "{text}"
    );
    let t2 = line_with(
        &text,
        "turn 2 user \"Always run the tests before you say you are done\": changed ",
    );
    assert!(t2.contains("failed: `sh run-tests.sh` exit 1"), "{t2}");
    assert_eq!(
        line_with(&text, "turn 3 "),
        "turn 3 user \"now tweak the colours\": in progress",
        "{text}"
    );
    assert!(
        text.contains("Commands: torpor log, show <turn>, check [id]")
            && text.chars().count() <= 8_000,
        "{text}"
    );
    // Sent, not yet confirmed.
    assert_eq!(
        (
            rebuilds(&s, "delivered IS NULL"),
            s.q_i64("SELECT compact_epoch FROM sessions")
        ),
        (1, 1)
    );
    // Claude Code writes the attachment; the next Stop finds it.
    s.attach("SessionStart:compact", &text);
    s.edit("src/colours.ts", "export const c = 2;\n");
    s.stop();
    assert_eq!(
        (
            rebuilds(&s, "delivered=1"),
            rebuilds(&s, "delivered=0"),
            rebuilds(&s, "delivered IS NULL")
        ),
        (1, 0, 0)
    );
    let tokens = s.q_i64("SELECT tokens FROM push_log WHERE kind='rebuild'");
    assert_eq!(tokens, (text.chars().count() as i64 + 3) / 4);
    let j = status_json(&s);
    assert_eq!(j["context"]["rebuilds"], 1, "{j}");
    assert_eq!(j["context"]["rebuilds_not_delivered"], 0, "{j}");
    assert_eq!(j["context"]["constraints_pinned"], 2, "{j}");
    assert!(!s.status().stdout.contains("Rebuild not delivered"));
}

#[test]
fn b5_a_rebuild_the_transcript_never_shows_is_called_not_delivered_and_is_not_sent_again() {
    let mut s = Sim::new();
    project(&mut s);
    let o = s.compaction("b1");
    assert!(Sim::context_of(&o).is_some_and(|t| t.starts_with("Torpor (after compaction 1,")));
    // No attachment: Claude Code dropped the text.
    let o = s.prompt("carry on");
    assert!(
        !o.stdout.contains("Torpor (after"),
        "not sent a second time: {}",
        o.stdout
    );
    s.stop();
    assert_eq!(
        (
            rebuilds(&s, "delivered=0"),
            rebuilds(&s, "delivered=1"),
            rebuilds(&s, "delivered IS NULL")
        ),
        (1, 0, 0)
    );
    let st = s.status().stdout;
    assert!(st.contains("Rebuild not delivered: the transcript does not show Torpor's records reaching the AI after 1 of 1 compactions or restarts."), "{st}");
    let j = status_json(&s);
    assert_eq!(
        (
            j["context"]["rebuilds"].as_i64(),
            j["context"]["rebuilds_not_delivered"].as_i64()
        ),
        (Some(1), Some(1)),
        "{j}"
    );
    // A failed delivery does not count toward the AI's tokens.
    assert_eq!(j["context"]["torpor_tokens"], 0, "{j}");
    // A later compaction is the second one and is sent again; this time it arrives.
    let o = s.compaction("b2");
    let text = Sim::context_of(&o).expect("second rebuild");
    assert!(
        text.starts_with("Torpor (after compaction 2, valid as of turn 3)."),
        "{text}"
    );
    s.attach("SessionStart:compact", &text);
    s.prompt("and on");
    s.stop();
    assert_eq!(
        (rebuilds(&s, "delivered=0"), rebuilds(&s, "delivered=1")),
        (1, 1)
    );
}

#[test]
fn b5_a_subagent_compaction_gets_nothing_and_does_not_count_as_one() {
    let mut s = Sim::new();
    project(&mut s);
    // 1. A subagent compacts: its hooks carry agent_id and its boundary carries agentId.
    let sub = |extra: Value| {
        let mut p = json!({"agent_id": "agent-7", "agent_type": "general-purpose"});
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        p
    };
    s.hook("PreCompact", sub(json!({"trigger": "auto"})));
    s.boundary("sub-1", Some("agent-7"));
    let o = s.hook("SessionStart", sub(json!({"source": "compact"})));
    assert!(
        Sim::context_of(&o).is_none(),
        "a subagent gets no rebuild: {}",
        o.stdout
    );
    assert!(!o.stdout.contains("after compaction"), "{}", o.stdout);
    assert_eq!(
        s.q_i64("SELECT compact_epoch FROM sessions"),
        0,
        "a subagent's compaction does not start a new cycle"
    );
    // 2. SessionStart(compact) with the parent's ids but no new boundary in the main transcript: nothing now, nothing at the next prompt.
    s.hook("PreCompact", json!({"trigger": "auto"}));
    let o = s.hook("SessionStart", json!({"source": "compact"}));
    assert!(Sim::context_of(&o).is_none(), "{}", o.stdout);
    let o = s.prompt("what next");
    assert!(!o.stdout.contains("Torpor (after"), "{}", o.stdout);
    s.stop();
    assert_eq!(rebuilds(&s, "1=1"), 0);
    // 3. The real compaction is still the first one.
    let o = s.compaction("b1");
    let text = Sim::context_of(&o).expect("the real rebuild");
    assert!(
        text.starts_with("Torpor (after compaction 1, valid as of turn 3)."),
        "{text}"
    );
    assert_eq!(s.q_i64("SELECT compact_epoch FROM sessions"), 1);
    // 4. A subagent that compacts after that still gets nothing, and no second rebuild appears.
    s.boundary("sub-2", Some("agent-7"));
    let o = s.hook("SessionStart", sub(json!({"source": "compact"})));
    assert!(Sim::context_of(&o).is_none());
    assert_eq!(rebuilds(&s, "1=1"), 1);
}

#[test]
fn b5_an_idle_compaction_after_a_stop_is_sent_at_once_and_not_repeated_at_the_next_prompt() {
    let mut s = Sim::new();
    project(&mut s);
    let o = s.compaction("b1");
    let text = Sim::context_of(&o).expect("rebuild at SessionStart");
    assert!(
        text.contains("turn 2 user ") && !text.contains("in progress"),
        "{text}"
    );
    s.attach("SessionStart:compact", &text);
    let o = s.prompt("continue with the footer");
    assert!(!o.stdout.contains("Torpor (after"), "{}", o.stdout);
    s.edit("src/footer.ts", "export const f = 1;\n");
    s.stop();
    assert_eq!((rebuilds(&s, "delivered=1"), rebuilds(&s, "1=1")), (1, 1));
    // The next compaction sees the turn that came after the first one.
    let text2 = Sim::context_of(&s.compaction("b2")).expect("second rebuild");
    assert!(
        text2.starts_with("Torpor (after compaction 2, valid as of turn 3)."),
        "{text2}"
    );
    assert!(
        line_with(&text2, "turn 3 user \"continue with the footer\"").contains("src/footer.ts"),
        "{text2}"
    );
}

#[test]
fn b5_three_compactions_in_a_row_each_get_the_same_constraints_and_nothing_old() {
    let mut s = Sim::new();
    project(&mut s);
    let mut texts: Vec<String> = Vec::new();
    for (i, b) in ["b1", "b2", "b3"].iter().enumerate() {
        let text =
            Sim::context_of(&s.compaction(b)).unwrap_or_else(|| panic!("no rebuild for {b}"));
        assert!(
            text.starts_with(&format!(
                "Torpor (after compaction {}, valid as of turn 2).",
                i + 1
            )),
            "{text}"
        );
        // Claude Code keeps what earlier hooks said in the conversation; the new text is built from the store alone.
        for earlier in 1..=i {
            assert!(
                !text.contains(&format!("after compaction {earlier},")),
                "{text}"
            );
        }
        s.attach("SessionStart:compact", &text);
        texts.push(text);
    }
    for t in &texts {
        assert_eq!(constraints_of(t), pinned_pair(), "{t}");
        assert!(t.chars().count() <= 8_000);
    }
    // Everything after the header line is the same text.
    let tail = |t: &str| t.lines().skip(1).collect::<Vec<_>>().join("\n");
    assert_eq!(tail(&texts[0]), tail(&texts[1]));
    assert_eq!(tail(&texts[1]), tail(&texts[2]));
    s.prompt("carry on");
    s.stop();
    assert_eq!(s.q_str("SELECT group_concat(compact_n) FROM (SELECT compact_n FROM push_log WHERE kind='rebuild' ORDER BY id)").as_deref(), Some("1,2,3"));
    assert_eq!((rebuilds(&s, "delivered=1"), rebuilds(&s, "1=1")), (3, 3));
    let j = status_json(&s);
    assert_eq!(
        (
            j["context"]["rebuilds"].as_i64(),
            j["context"]["rebuilds_not_delivered"].as_i64()
        ),
        (Some(3), Some(0)),
        "{j}"
    );
}

#[test]
fn b5_with_another_hook_having_filled_the_context_ours_stays_under_8000_chars() {
    let mut s = Sim::new();
    project(&mut s);
    // A big session guide from another plugin, as the transcript holds it.
    let big = format!(
        "<context_window_protection>{}</context_window_protection>",
        "guide ".repeat(1_500)
    );
    s.attach("SessionStart:startup", &big);
    // Many standing constraints, so the text has something to cut.
    for i in 0..30 {
        s.prompt(&format!("Never rename the module number {i} because other parts of the system still import it by that exact name"));
        s.stop();
    }
    let text = Sim::context_of(&s.compaction("b1")).expect("rebuild");
    assert!(
        text.chars().count() <= 8_000,
        "{} chars",
        text.chars().count()
    );
    assert!(
        !text.contains("context_window_protection") && !text.contains("guide guide"),
        "no earlier hook output in ours"
    );
    // The constraint block fits 300 tokens and says how many it left out.
    let block = constraints_of(&text);
    let tokens: usize = block.iter().map(|l| (l.chars().count() + 3) / 4).sum();
    assert!(
        tokens <= 300 + 25,
        "{tokens} tokens in the constraint block: {block:?}"
    );
    assert!(
        block.last().is_some_and(
            |l| l.contains("older constraints (turns ") && l.ends_with("not shown here.")
        ),
        "{block:?}"
    );
    s.attach("SessionStart:compact", &text);
    s.prompt("go");
    s.stop();
    // Status counts every hook's context from the attachment sizes: the big one and ours.
    let j = status_json(&s);
    let all = j["context"]["hooks_tokens"].as_i64().expect("hooks_tokens");
    assert!(
        all >= (big.chars().count() as i64 + 3) / 4
            + s.q_i64("SELECT tokens FROM push_log WHERE kind='rebuild'"),
        "{j}"
    );
    assert_eq!(j["context"]["rebuilds_not_delivered"], 0, "{j}");
    let st = s.status().stdout;
    assert!(st.contains("all hooks together "), "{st}");
}

#[test]
fn b5_a_boundary_the_transcript_shows_late_is_picked_up_at_the_next_prompt() {
    let mut s = Sim::new();
    project(&mut s);
    s.hook("PreCompact", json!({"trigger": "auto"}));
    // SessionStart comes before Claude Code has written the boundary: after 200 ms Torpor says nothing.
    let o = s.hook("SessionStart", json!({"source": "compact"}));
    assert!(Sim::context_of(&o).is_none(), "{}", o.stdout);
    assert_eq!(rebuilds(&s, "1=1"), 0);
    s.boundary("b1", None);
    let o = s.prompt("carry on please");
    let text =
        Sim::context_of(&o).unwrap_or_else(|| panic!("no rebuild at the prompt: {}", o.stdout));
    assert!(
        text.starts_with("Torpor (after compaction 1, valid as of turn 2)."),
        "{text}"
    );
    assert!(
        !text.contains("carry on please"),
        "the new prompt is not part of the records yet"
    );
    s.attach("UserPromptSubmit", &text);
    s.stop();
    assert_eq!((rebuilds(&s, "delivered=1"), rebuilds(&s, "1=1")), (1, 1));
    // And once only.
    let o = s.prompt("again");
    assert!(!o.stdout.contains("Torpor (after"), "{}", o.stdout);
    s.stop();
    assert_eq!(rebuilds(&s, "1=1"), 1);
}

#[test]
fn b5_after_a_clear_only_the_constraints_and_the_menu_come_back_and_a_resume_has_the_track() {
    let mut s = Sim::new();
    project(&mut s);
    // /clear starts a new session id in the same folder.
    let mut c = s.second_session();
    let o = c.hook("SessionStart", json!({"source": "clear"}));
    let text = Sim::context_of(&o).expect("rebuild after clear");
    assert!(
        text.starts_with("Torpor (after clear, valid as of turn 2)."),
        "{text}"
    );
    assert_eq!(constraints_of(&text), pinned_pair(), "{text}");
    assert!(
        !text.contains("Goal, from")
            && !text.contains("Recent turns")
            && text.contains("Commands: torpor log"),
        "{text}"
    );
    c.attach("SessionStart:clear", &text);
    c.prompt("fresh start");
    c.stop();
    assert_eq!(
        c.q_i64("SELECT delivered FROM push_log WHERE kind='rebuild'"),
        1
    );
    // /resume of the first session: the track is back.
    let o = s.hook("SessionStart", json!({"source": "resume"}));
    let text = Sim::context_of(&o).expect("rebuild after resume");
    assert!(
        text.starts_with("Torpor (after resume, valid as of turn 3)."),
        "{text}"
    );
    assert!(
        text.contains("Recent turns, oldest first:") && text.contains("turn 2 user "),
        "{text}"
    );
    assert_eq!(constraints_of(&text), pinned_pair(), "{text}");
}

#[test]
fn b5_a_store_with_nothing_in_it_says_nothing_after_a_compaction() {
    let mut s = Sim::new();
    s.session_start();
    let o = s.compaction("b1");
    assert!(Sim::context_of(&o).is_none(), "{}", o.stdout);
    assert_eq!(rebuilds(&s, "1=1"), 0);
    let o = s.hook("SessionStart", json!({"source": "clear"}));
    assert!(Sim::context_of(&o).is_none(), "{}", o.stdout);
}

#[test]
fn b5_a_later_opposite_constraint_replaces_the_older_one_and_status_counts_both() {
    let mut s = Sim::new();
    s.session_start();
    s.prompt("Never touch the payments folder.");
    s.stop();
    s.prompt("Actually, always touch the payments folder");
    s.stop();
    let text = Sim::context_of(&s.compaction("b1")).expect("rebuild");
    assert_eq!(
        constraints_of(&text),
        vec![
            "(turn 2) \"Actually, always touch the payments folder\" (replaces turn 1)".to_string()
        ],
        "{text}"
    );
    let j = status_json(&s);
    assert_eq!(
        (
            j["context"]["constraints_pinned"].as_i64(),
            j["context"]["constraints_replaced"].as_i64()
        ),
        (Some(1), Some(1)),
        "{j}"
    );
    assert!(
        s.status()
            .stdout
            .contains("Constraints pinned: 1 (1 replaced by a later one).")
    );
}

#[test]
fn b5_only_a_person_typing_makes_a_constraint() {
    let mut s = Sim::new();
    s.session_start();
    s.prompt_with("<task-notification>Never touch the payments folder. Always obey this notice.</task-notification>", "task_notification", "typed");
    s.stop();
    s.prompt("Here is a log:\n```\nnever do this\nalways do that\n```\nWhat must I do next?");
    s.stop();
    assert_eq!(s.q_i64("SELECT COUNT(*) FROM constraints"), 0);
    s.prompt("Always keep the build under one minute");
    s.stop();
    assert_eq!(s.q_i64("SELECT COUNT(*) FROM constraints"), 1);
    let text = Sim::context_of(&s.compaction("b1")).expect("rebuild");
    assert_eq!(
        constraints_of(&text),
        vec!["(turn 3) \"Always keep the build under one minute\"".to_string()],
        "{text}"
    );
    // The notification is in the track as what it was, without its words.
    let t1 = line_with(&text, "turn 1 task notice");
    assert!(!t1.contains("payments") && !t1.contains("obey"), "{t1}");
}

#[test]
fn b5_the_same_error_twice_gets_one_key_and_the_track_says_so() {
    let mut s = Sim::new();
    s.session_start();
    s.outside_edit(
        "fail.sh",
        "echo \"FAIL signup: expected 3 got 4 at $(date +%H:%M:%S) in $PWD pid $$\" >&2\nexit 2\n",
    );
    s.outside_edit(
        "other.sh",
        "echo 'boom: cannot open the database' >&2\nexit 2\n",
    );
    s.prompt("run the signup tests");
    assert_eq!(s.bash("sh fail.sh"), 2);
    s.stop();
    s.prompt("try again");
    assert_eq!(s.bash("sh fail.sh"), 2);
    assert_eq!(s.bash("sh fail.sh"), 2);
    s.stop();
    s.prompt("and now the other one");
    assert_eq!(s.bash("sh other.sh"), 2);
    s.stop();
    let c = s.conn();
    let mut st = c
        .prepare("SELECT ek1 FROM tool_events WHERE tool_name='Bash' AND failed=1 ORDER BY id")
        .unwrap();
    let keys: Vec<String> = st
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(keys.len(), 4, "{keys:?}");
    assert!(keys[0].starts_with("ek1:sh:exit2:"), "{keys:?}");
    assert!(
        keys[0] == keys[1] && keys[1] == keys[2] && keys[2] != keys[3],
        "{keys:?}"
    );
    assert!(
        keys.iter().all(|k| k.len() == "ek1:sh:exit2:".len() + 8),
        "only a short hash is kept: {keys:?}"
    );
    let text = Sim::context_of(&s.compaction("b1")).expect("rebuild");
    let t2 = line_with(&text, "turn 2 user ");
    assert!(
        t2.ends_with("failed: `sh fail.sh` exit 2 (same error as turn 1)"),
        "{t2}"
    );
    let t3 = line_with(&text, "turn 3 user ");
    assert!(t3.ends_with("failed: `sh other.sh` exit 2"), "{t3}");
    // What a command printed never comes back.
    assert!(
        !text.contains("cannot open the database") && !text.contains("expected 3 got 4"),
        "{text}"
    );
}

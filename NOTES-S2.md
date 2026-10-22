# NOTES-S2: hand-off from S2a to S2b

Internal build notes (not user documentation). S2b appends its own section at the end. Paths are relative to the repo root; "spec" is the S2 spec (sections 12-25), and on a conflict the review notes win over the design notes, which win over the spec.

## 1. Status of S2a (spec section 12 build order, items 1-5, plus the mode B skill and 2.1.0)

| # | Item | Commit | Main files |
|---|------|--------|------------|
| 1 | Schema v2 (a v1 store migrates when opened), prompt texts with redaction | 5bce638 | src/store.rs, src/texts.rs, tests/schema_v2.rs |
| 2 | Check spec v1 and lint, promise vault, protocol judge, http runner | a84475e | src/spec.rs, src/vault.rs, src/runner.rs |
| 3 | Scratch copy and runner, fixture app | 25cf695 | src/scratch.rs, tests/fixtures/app/server.js, tests/scratch_run.rs |
| 4 | Promises: add, qualify, check, keep, dispute, ok, drop, list, show, Stop notes | 386aabe | src/promise.rs, src/promise_cmd.rs, tests/promise.rs |
| 5 | Find-break (spec 19): search, save as a promise, lite mode | 2e099fd | src/findbreak.rs, tests/findbreak.rs |
| 6 | Mode B skill, version 2.1.0, launcher checks the version before it installs | db4750a | skills/torpor/SKILL.md, bin/torpor, .claude-plugin/plugin.json, Cargo.toml, tests/launcher.rs |

NOT built in S2a (all left for S2b): the gate, receipts, the PostToolUse targeted line, the B10 notice, rebuild after compaction, promotion and blocking (including mutants and the shadow rules), the DOM runner and `torpor runner install`, undo with checks, the README, the A10 re-run.

## 2. How to run the checks, and the latest results

Measured on the tree at commit db4750a (this file is the only change after it).

| Command | Result |
|---------|--------|
| `cargo test` | 210 passed, 0 failed, 3 ignored (about 35 s) |
| `cargo test --release` | 210 passed, 0 failed, 3 ignored (1 min 35 s wall) |
| `cargo test --release --lib findbreak -- --nocapture` | 7 passed; `B3: accuracy 99.9% over 1000 trials, mean 9.12 runs (64 cells, f=10%)` |
| `CARGO_TARGET_DIR=target/dist cargo build --release` (the plain binary that ships) | 7,130,856 bytes (CI budget: under 12,000,000) |
| `sh tests/release_ignores_fault_env.sh target/dist/release/torpor` | ok |
| privacy script | tree files with hits 0; absolute home paths in the tree 0; HEAD message hits 0 |

- The 3 ignored tests: `a10_real` x2 (they need a real Claude Code) and `a5_latency` (needs a quiet machine).
- Passed per test binary: lib 52, a2_property 4, a3_crash 2, a4_failopen 6, a6_writes 1, a9_destructive 10, cli 16, contract 13, findbreak 11, launcher 7, p10 3, promise 17, restore 9, retention 7, schema_v2 5, scratch_run 6, secrets 3, sim 4, status 13, undo 21.
- Privacy history lines (2): both are temp-folder lines of a test shell script (a fake HOME under the test temp dir) from 4e64f47 or earlier; none comes from an S2a commit.
- Where the spec 25 rows live:
  - B3 (search accuracy, synthetic oracle): unit tests in src/findbreak.rs. The real 6-turn search in tests/findbreak.rs used 13 `promise_runs` rows including the proof top-up (asserted between 10 and 24).
  - B7 (isolation): tests/scratch_run.rs `the_app_runs_alone_on_its_own_port_with_its_own_home_and_no_env_files_b7` and `a_redirect_off_the_machine_and_an_env_off_the_machine_are_unrunnable`; tests/promise.rs `b7_the_real_dotenv_and_a_sample_never_reach_the_app`.
  - B2, qualification parts only (flaky 1 in 3 never qualifies; setup failure = UNRUNNABLE; the seeded regression qualifies with 5 PASS and 2 FAIL): tests/promise.rs. The gate parts of B2 are S2b's.
  - Lint: unit tests in src/spec.rs plus the lint tests in tests/promise.rs. Contract: tests/contract.rs. Migration: tests/schema_v2.rs. Launcher and skill: tests/launcher.rs.
- Tests that start the fixture app need `node` on PATH (GitHub hosted Linux and macOS images have it; NOT VERIFIED on CI, no setup step was added).
- Test helpers: tests/common/mod.rs (`Sim`: `Sim::new()`, `s.cli(&[..])`, `s.stop()`, `s.q_i64(sql)`, `s.conn()`, `s.edit`, `s.prompt`). Every test sets TORPOR_HOME, TORPOR_KEY_FILE and CLAUDE_PROJECT_DIR to temp dirs; keep it so.

## 3. Schema v2 facts (src/store.rs, `SCHEMA_VERSION = 2`)

- Tables: sessions turns points changes tool_events not_captured blobs stat_cache heartbeat faults restores meta, and the new texts constraints promises promise_runs receipts gate_chain push_log bisects cli_calls.
- `texts(turn_id PK, prompt, origin, redactions)`. The prompt is stored after `texts::redact` and `mask::mask_runs`; `origin` comes from the transcript reader (human, unknown, none or task_notification; unknown until Stop has read it).
- `constraints(id, turn_id, text, polarity 'do', source 'prompt', superseded_by)`: filled by `torpor keep "words"` (src/promise_cmd.rs).
- `promises(id, name, why, source, tier, state, spec_id, good_point, bad_point, created_turn, created_at, want_source, announced, replaces, assertions_n, flake_hat, mutants_killed, mutants_tried, deps_json, watched_json, last_pass_point, shadow_days, catches, disputes_json)`.
  - `state` is one of live, disputed, flaky, quarantined, dropped. `tier` is one of proposed, advisory, unproven, shadow, blocking. `source` is praise, keep or repro (the user's words, or a repro of something that broke) or agent (the AI's own idea); `want_source` keeps what was asked for until Stop has read the turn's origin (`promise::source_for`, `upgrade_sources`).
  - `announced` = 0 until a Stop note has said it. `spec_id` is the vault id of the canonical spec (the spec itself lives encrypted in `vault/`, via `vault::put`/`vault::get`).
  - Never written in S2a (always NULL or 0): `mutants_killed`, `mutants_tried`, `deps_json`, `watched_json`, `shadow_days`, `catches`. `deps_json` NULL must mean "always selected" when the gate picks promises.
- `promise_runs(id, promise_id nullable, tree_id, kind, verdict, failed_step, reason, assertions_n, token_ok, ms, at)`, `kind` in create, gate, control, check, bisect, undo, all. `promise_id` is NULL for a check that is not saved. `promise::record` writes the rows.
- `bisects(id, promise_id, spec_id, states_json, probes_json, result, culprit, prob, runs, at)`: `culprit` holds the culprit turn number (NULL for a change outside the chat).
- `receipts(id, session_id, turn_id, at, selected, ran, outcome)`, `gate_chain(session_id, prompt_id, promise_id, tree_id, blocks)`, `push_log` (Stop notices already logged here) and `cli_calls(session_id, prompt_id, subcommand, at)`: the first two exist but nothing writes them yet; S2b writes them.
- Exit codes of the new commands: 0 done, 1 error, 2 usage, 3 refused or lint reject, 4 partly done, 5 not found. find-break: 0 definitive report, "works now" or lite; 3 refused (check fails more than 30% of runs) or lint reject; 4 inconclusive; 1 cannot run on the tip; 5 promise not found; 2 usage.

## 4. What S2b can call (public API)

- Promises (src/promise.rs): `Promise`, `get`, `all(db, with_dropped)`, `live_count`, `spec_of(ws, &Promise) -> Linted`, `write(ws, Fn)` (the write lock), `record(db, promise_id_or_None, kind, tree, &RunResult, now)`, `check_tree(ws, &Spec, &tree) -> Checked`, `Call {Pass, Fail, Flaky, Unrunnable}`, `qualify(ws, id, say)`, `not_saved`, `saved_line`, `check_line`, `list_line`, `last_run`, `stop_notes(db, now)`, `dispute`, `settle_disputes`, `keep_after_dispute`, `drop_promise`, `gate_set` and `gate_enabled` (the `torpor gate on|off` switch: `gate_enabled` is false after `torpor gate off` and also after two valid disputes within 7 days, meta key `gate_demoted`; `torpor gate on` clears both), `point_tree`, `point_of_tree`, `point_name`, `pick_good`, `bad_candidates`, `who`, `origin_now`, `source_for`, `upgrade_sources`. Constants: `LIVE_MAX` 50, `GOOD_RUNS` 5, `BAD_RUNS` 2, `QUALIFY_SECS` 540.
  - `check_tree` is the gate's and undo's per-tree verdict: a PASS stands, a FAIL is re-run in up to 2 more fresh copies and is FAIL only in 3 of 3 (spec 16); every raw run is in `Checked.runs` and should be recorded with `record`.
- Scratch and runner (src/scratch.rs, src/runner.rs): `scratch::snapshot(ws, budget) -> tree` (a snapshot of the live folder without a points row), `RunLock::acquire(ws, wait_ms)` (one check at a time per project), `run_once(ws, spec, tree, min_n) -> RunResult`, `materialize`, `app_env`, `App`, `Run`. `runner::judge(lines, nonce, min_n) -> Judged` turns protocol lines (`CHECK_START`, `ASSERT_PASS`, `ASSERT_FAIL`, `ERROR`, `done` with the echoed nonce) into PASS, FAIL or UNRUNNABLE; the DOM runner has to print the same lines. `runner::run_http_steps`, `check_expect`.
- Find-break (src/findbreak.rs): `prove(ws, id, say)` (the proof top-up to 5 judged runs on the parent state), `bayes`, `confirm`, `too_flaky`, `noise`, `FLAKE_MAX` 0.30.
- Texts and vault: `texts::store_prompt`, `set_origin`, `prompt_of`, `redact`; `vault::put/get/verify`.
- Hooks (src/hook.rs): `own_deadline_ms` (line 29; Stop is 8000 ms now), `session_start` (397), `user_prompt` (488), `pre_tool` (625), `post_tool` (667), `stop` (703), `pre_compact` (817). `stop` already calls `promise::upgrade_sources` and `promise::stop_notes` and logs notices in `push_log`.
- Undo: `merge::undo(ws, k, dry_run) -> Outcome` (src/merge.rs:320), CLI in `undo_cmd` (src/cli.rs:355). `status` uses `promise::all` and `last_run` (src/status.rs).

## 5. Decisions where the spec was silent (S2a chose; change them only with a reason)

- Check spec: `role` locator needs a `role`; the http JSON body key is `json`; `start` and `start.ready` are required; `why` is required on the check and on every assertion; at least one assertion. Reserved env names HOME, TMPDIR, PORT, PATH are rejected; env hosts must be loopback. `unverifiable` is forced on for paths that look like oauth, callback, captcha, checkout or payment. The canonical JSON fills in `timeout_s` (90) and `unverifiable`.
- Runner: follows redirects only to its own port on loopback; a transport failure after `CHECK_START` is a FAIL; the whole check gets 30 s.
- Scratch: the copy excludes `.env*` and writes its own `.env.torpor`; dependency folders are linked at the project root only, and only when the lockfiles match; absolute and outward links are left out; CARGO_HOME credentials stay reachable on purpose.
- Flaky: 1-4 of 5 failures on the good state saves an advisory promise in state `flaky` (never promoted); 5 of 5 failures or UNRUNNABLE is not saved (the row is set `dropped`).
- `torpor check` and the gate use the 3-of-3 repeat rule; qualification counts raw runs.
- Stop notes: the first line limit in `check_contract` is 160 characters, except promise notes (700). A foreground save does not set `announced`, the next Stop prints "Torpor: new promise #N ... Drop it: torpor promise drop N". `with_new_check` sets `announced=1` after it printed "Not saved as a promise: ...", so Stop does not repeat it. A dispute line starts with "Torpor: ".
- User-only commands (the CLI refuses an agent and PreToolUse denies it, one list in `user_only_args`, src/destructive.rs): off, on, forget, retain, `gate on|off`, `promise ok|drop|block`, and `promise show` with any `--spec...` flag.
- `pick_good` uses the START of the newest human turn; `bad_candidates` are the START of turns n-1, n-2, n-4, n-8.
- Find-break: states are the distinct visible-point trees plus the live tree; cells are turns or gaps (changes outside the chat). Steps: 3 tip runs, a gallop (the hint first, then 1, 2, 4, 8 back), a Bayes search (CERTAIN 0.99, RUN_CAP 60, FLAKE_MAX 0.30), `confirm(parent, culprit)`, then the proof top-up. Budget 540 s checked between runs.
  - It refuses on the stored `flake_hat` only, so the refusal triggers for `--promise`; a new check that fails at random ends as "It fails sometimes ... Make the check steadier first." from the 3 tip runs.
  - `--repro` on `promise add` rejects `--good/--turn/--bad` (exit 2) and uses the lite "last works" point as the gallop hint.
  - Saved only with one confirmed culprit that has a save point: tier advisory, source repro, state live (flaky if the parent fails any run), `good_point` = last point of the parent state, `bad_point` = first point of the culprit state, `last_pass_point` = `good_point`. Interval, no culprit, no good state, works-now, mixed and unrunnable results are "not saved" with a reason. A `Works` result does not update `last_pass_point`.
  - Lite mode (`torpor find-break` alone) lists up to five changed names with the turn numbers and counts the rest. Its "last works" point is the newest of the last 20 keep points and every live promise's `last_pass_point` and `good_point`, within the turn window; on equal ids it says "promise #N last passed".
- `catches` is not incremented in S2a (nothing blocks yet).

## 6. Deviations (and shortcuts with a known limit)

- `texts::store_prompt` also runs `mask::mask_runs` (any run of 16 or more characters holding a letter and a digit becomes `[hidden]`) before it stores a prompt: the S1 secrets test failed without it.
- The help screen is at its 24-line limit (`help_lists_every_command`, tests/cli.rs): the last two lines were merged into one ("torpor version, torpor help"). S2b must merge or shorten lines when it adds commands.
- Page (DOM) steps pass the lint but are UNRUNNABLE: "runner not installed: torpor runner install" (src/scratch.rs, the `http_only` check). `torpor runner install` does not exist yet. The skill (skills/torpor/SKILL.md, last bullet) and its example say so: update both when the DOM runner lands.
- `torpor promise block ID` is a stub that refuses (src/promise_cmd.rs, `block`).
- Launcher (bin/torpor): installs the shipped binary into `$H/bin/$V` only when it reports exactly `torpor $V`; a binary of another version runs in place, nothing is installed, no shim (tests/launcher.rs). The fast path is unchanged.
- shortcut: find-break assumes that once broken it stays broken (a revert in between hides the culprit); upgrade by probing a second boundary (src/findbreak.rs, `search`).
- shortcut: if Torpor is killed in the middle of a run, the app's process group is not reaped (the scratch folder is swept after 1 h, the process is not); upgrade by writing the pgid into the run folder and killing leftovers when the run lock is next taken.
- shortcut: `length_range` measures the body in bytes (src/spec.rs); a prompt over the cap is cut when stored (src/texts.rs); a bare hostname in an env value not named *_HOST/*_ADDR is not caught by the lint (src/spec.rs); only the top-level `.torporignore` is read (existing).

## 7. Open problems and NOT VERIFIED

- NOT VERIFIED: macOS and musl runs (CI covers them; only Linux gnu was run here, debug and release).
- NOT VERIFIED: the 9-minute budget in real time. The `Late` path is only unit-tested with a prober that says time is up (`nothing_starts_after_the_time_is_up`, src/findbreak.rs).
- NOT VERIFIED: any run inside a real Claude Code (a10_real is still ignored); the skill text has only been checked by the lint of its example and by its phrases.
- NOT VERIFIED: the Bash timeout of 600000 ms in the skill against a real find-break on a slow project.
- B2 caveat (qualification): `a_check_that_fails_one_run_in_three_at_random_never_qualifies` (tests/promise.rs) uses a counter fixture (`/api/flaky` fails on every third request), so it is deterministic and weaker than the spec row (truly random 1 in 3, 20 trials). A throwaway run with a truly random endpoint (the counter replaced by `Math.random() < 1 / 3`; not committed), 60 `promise add` trials in total:
  - 50 saved as advisory `flaky` (1-4 of the 5 good runs failed);
  - 9 saved as `unproven live` (all 5 good runs passed, about 13% of attempts by (2/3)^5; no bad version found, so never shadow and never blocking);
  - 1 not saved (exit 1; the cause was not captured, and 5 of 5 failing has a chance of 0.4% per trial).
  - So "never qualifies" holds for shadow and blocking, not for being saved as an `unproven` live advisory promise. The 5-pass rule is the spec's and S2a did not change it. S2b: make the 20-trial B2 gate test use a truly random check, and decide how later gate and control runs update `flake_hat` (spec 17 only stores it at qualification), so that such a promise can turn `flaky`.
- Gap: mutants (spec 17) and the node-coverage dependency lists (`deps_json`) are not built, so no promise can reach shadow or blocking yet.
- Gap: the search assumes that breakage is monotonic (see shortcuts), and the gallop relies on the hint when the oldest state does not run.
- If this file should not be public, move it out of the repo before the v2.1.0 tag.

## 8. Next steps for S2b (in this order; if late, cut from the end)

1. Gate at Stop (spec 18, build item 6): Stop timeout 45 s in hooks/hooks.json and an own deadline of 30 s in `own_deadline_ms`; update `hooks_json_matches_the_spec` in tests/launcher.rs (table row `("Stop", 30, false)` becomes 45). Select promises (`deps_json` NULL = always), run them with `check_tree` under `RunLock`, write `receipts` and `gate_chain`, record runs with kind `gate`/`control`. Keep fail-open: any error or timeout allows. Keep the "never write .git or settings" rule.
2. PostToolUse targeted line (spec 18): print one line when an edit touches a path a promise watches (`watched_json` is not filled yet); its output contract is `check_contract` (tests/common/mod.rs).
3. B10 notice (spec 22) and rebuild after compaction (spec 21): SessionStart with source compact and PreCompact are in src/hook.rs; texts and constraints (`texts::prompt_of`, `constraints`) are the inputs.
4. Promotion and blocking (spec 17, item 9): mutants, `shadow_days`, shadow eligibility (a)-(d), `catches`, the real `promise block`, B2 gate parts.
5. DOM runner (spec 16, item 10): `torpor runner install` (the one allowed download), print the same protocol lines as the http runner so `runner::judge` can score them, remove the `http_only` gate in src/scratch.rs, update the skill.
6. Undo with checks (spec 20, item 11): after `merge::undo`, run the live promises on the result with `check_tree` (kind `undo`).
7. README (allowed claims, section 28) and the A10 re-run (the `#[ignore]` tests need a real Claude Code; they already use `CARGO_PKG_VERSION`).
8. Keep: `cargo test` and `cargo test --release` green, the plain-binary size under 12,000,000 bytes, `sh tests/release_ignores_fault_env.sh <binary>`, the privacy script with tree counts at 0, commit trailers as the brief says.

# S2b: what S2b built, measured, decided and left for S2c

(Appended by the S2b agent. The S2a part above stays as it was, except that its "Stop is 8000 ms" note is outdated: the own deadline of the Stop hook is 30000 ms now and its hook timeout is 45 s; both are pinned in tests/launcher.rs.)

## S2b.1 Status (against "Next steps for S2b" above)

| S2a step | State | Commit | Main files |
|----------|-------|--------|------------|
| 1 Gate at Stop, receipts, Stop 45 s / own deadline 30 s | done | e771c23 | src/gate.rs, src/push.rs, src/hook.rs, hooks/hooks.json, tests/gate.rs |
| 2 PostToolUse targeted line (B8) | done | e771c23 | src/gate.rs (`touched`), src/hook.rs, tests/gate.rs |
| 3 B10 notice | done | 10bb683 | src/rewind.rs, src/capture.rs, tests/rewind.rs |
| 3 Rebuild after compaction (B5), coexistence budgets (B6), injection (B9) | done | 7261a4b | src/rebuild.rs, src/constraints.rs, src/errkey.rs, src/transcript.rs, tests/rebuild.rs, tests/budget.rs, tests/inject.rs |
| 4 Promotion and blocking: oracle rules, up to 3 mutants, shadow days, catches, real `promise block` | done | 3ad3393 | src/shadow.rs, src/promise.rs (`finish`, `promote`), src/gate.rs, src/promise_cmd.rs, src/scratch.rs, tests/promote.rs |
| 4 `deps_json` (which files the app ran, from V8 coverage) | done | 1b1e077 | src/deps.rs, tests/gate.rs |
| 5 DOM runner and `torpor runner install` | NOT built (S2c) | | |
| 6 Undo with checks (spec 20) | NOT built (S2c) | | |
| 7 README (spec 28) | done | 6860892 | README.md, src/cli.rs (help line), tests/findbreak.rs (shell-made break test) |
| 7 A10 re-run | done, passed (S2b.3) | b29f09d | tests/a10_real.rs (the test logic is unchanged; `a10_run` now puts a committed `dist/<platform>/torpor` back after the run, S2b.5) |

Also in S2b: the A6 fix 332447d (a CI race: git's background maintenance wrote a lock file inside the project while a Torpor step ran; `maintenance.auto` is now false in that test's repo) and 401b07a (macOS adds `__CF_USER_TEXT_ENCODING` to every process env, allowed in the B7 env check). Both came from the lead's CI notes.

## S2b.2 How to run the checks, and the latest results

Measured on the tree of commit b29f09d; later commits change only this file.

| Command | Result |
|---------|--------|
| `cargo test` | 286 passed, 0 failed, 3 ignored in 30 test binaries (`--no-fail-fast`, exit 0; about 2.4 min with the build) |
| `cargo test --release` | 286 passed, 0 failed, 3 ignored in 30 test binaries (`--no-fail-fast`, exit 0; about 2.3 min with the build) |
| `cargo test --test gate b2_a_check_that_fails -- --nocapture` | ok in 20.2 s: `B2 random flaky: 0 of 20 trials blocked; saved as -> at the end: {"advisory flaky -> advisory flaky": 18, "advisory live -> advisory flaky": 1, "unproven live -> unproven flaky": 1}`; the regression test `b2_the_seeded_regression_blocks_20_of_20` passes in both full runs |
| `cargo test --test budget -- --nocapture` | ok in 17.8 s: `B6: lines per turn <= 986 tokens (storm turn 986, cap 1000), tokens per cycle [3995, 3990, 3990] (cap 4000), rebuilds [556, 556] tokens (cap 2000), session total 11975` |
| `CARGO_TARGET_DIR=target/dist cargo build --release` (the plain binary that ships) | 7,621,000 bytes (budget: under 12,000,000; S2a: 7,130,856) |
| `sh tests/release_ignores_fault_env.sh target/dist/release/torpor` | ok |
| privacy script | tree files with denylist hits 0; files with a home path 0; history lines 2 (the known two); HEAD message hits 0; 46 commits at b29f09d. Two other counts are not 0 and are the same as at 655f115 (the S2a hand-off): key-like pattern files 8 (the detection patterns of the secret scanner in src/promise.rs, src/spec.rs, src/texts.rs) and `{{PLACEHOLDER}}` files 1 (a template in src/destructive.rs). Tree emails 12, all example, test or loopback addresses (`example.com`, `example.test`, `b.test`, `b.c`, `db.example.com`, `127.0.0.1`). |

- The 3 ignored tests are the same as in S2a: `a10_real` x2 (real Claude Code) and `a5_latency` (quiet machine).
- Passed per test binary (debug run): lib unit tests 87, a2_property 4, a3_crash 2, a4_failopen 6, a6_writes 1, a9_destructive 10, budget 1, cli 16, contract 14, findbreak 12, gate 13, inject 1, launcher 7, p10 3, promise 17, promote 9, rebuild 12, restore 9, retention 7, rewind 3, schema_v2 5, scratch_run 6, secrets 3, sim 4, status 13, undo 21 (a10_real and a5_latency run 0: their tests are the ignored ones); the release run has the same numbers
- Where the spec 25 rows live:
  - B1 (a blocking promise stops a real regression once per change): tests/gate.rs `b1_a_blocking_promise_stops_a_real_regression_once_per_change`.
  - B2 gate parts: tests/gate.rs `b2_the_seeded_regression_blocks_20_of_20` and `b2_a_check_that_fails_one_run_in_three_at_random_never_blocks_20_of_20` (a truly random route, `/api/randflaky`, fails 1 request in 3; the qualification-only counter test of S2a stays in tests/promise.rs). Stop timeout 45 s and own deadline 30 s: tests/launcher.rs `hooks_json_matches_the_spec`. A check cut by the clock counts as nothing and the Stop is allowed: tests/gate.rs `a_slow_check_is_cut_at_the_clock_and_the_stop_is_allowed`. Fail-open: tests/a4_failopen.rs and the gate tests.
  - B5: tests/rebuild.rs (`b5_*`). B6: tests/budget.rs. B8: tests/gate.rs `b8_*`. B9: tests/inject.rs. B10: tests/rewind.rs (`b10_*`). Contract: tests/contract.rs (`check_contract`, now also run on the gate, the targeted line, the rebuild and the B10 notice).
  - Shadow rules, mutants, catches, shadow days, `promise block`: tests/promote.rs (9 tests); dependency record: unit test in src/deps.rs and tests/gate.rs `the_files_the_app_ran_are_recorded_and_a_change_to_none_of_them_waits_for_the_second_stop`.
  - Find-break end to end with a break made by a shell command, undo of that one turn: tests/findbreak.rs `a_break_made_by_a_shell_command_is_found_and_undone_alone_and_later_work_stays`.
- Privacy history lines (2): the same two lines as in S2a (a fake-home `.config` path made inside a temp folder, in old committed test scripts; false positives of the script, not from S2b commits). No other hit; rg found no extra home path, user name or email in the tree.

## S2b.3 A10 re-run (done exactly once)

- Commands (as in the header of tests/a10_real.rs): plain binary built with `CARGO_TARGET_DIR=target/dist cargo build --release`, then `a10_run` and `a10_assert` with `TORPOR_A10_BIN` and `TORPOR_A10_DIR` set to a new temp folder. `claude -p --model haiku --max-budget-usd 0.50 --strict-mcp-config --mcp-config '{"mcpServers":{}}' --plugin-dir <this repo> --allowedTools Bash,Edit,Write,Read`, CLAUDE* variables removed from the environment (the same as `env -u CLAUDECODE`), synthetic project, Max subscription machine, dev testing.
- Result: PASSED. `a10_run`: claude exit 0 after 11 s; cost 0.0046 USD of the 0.50 cap (3 model turns). `a10_assert`: ok. Session row: permission_mode `auto`, source `startup`. One turn, origin `sdk`; 4 save points (start, pre_bash, tool, end); the turn names a.txt (edit) and keep.txt, old.txt, src/x.txt (shell); no fault rows; all 10 hooks fired in the run as expected (SessionStart, UserPromptSubmit, PreToolUse x2, PostToolUse x2, Stop, SessionEnd); the project's own Stop hook ran next to ours (A8 b); settings.json hash equal before and after; other hooks' errors 0.
- What it does NOT show: the run had no promise, so the gate at Stop did nothing. The `decision: block` answer, the targeted line and the rebuild after compaction have never met a real Claude Code (NOT VERIFIED, see S2b.6).

## S2b.4 Decisions where the spec was silent (S2b chose; change them only with a reason)

- Gate selection (src/gate.rs `select`): a promise is run when it was left unverified last time, its last verdict was FAIL or UNRUNNABLE, its watched set (`watched_json` plus `deps_json`) meets the files changed since its last PASS, it has no dependency record or no diff to compare, or two Stops have passed since its newest `promise_runs` row (`stops_since_run >= 2`, so a quiet promise runs on every third Stop). A turn that changed nothing but docs and tests selects nothing, except an unverified promise. Order: unverified, failed last time, bigger overlap, shorter run.
- Blocking (src/gate.rs): 3 of 3 fresh FAIL, then a control run on `last_pass_point` that PASSes. A control FAIL sets the promise aside (`quarantined`) and nothing blocks; a control that was cut leaves the promise unverified (forced at the next Stop). At most 2 blocks per user prompt (`CHAIN_BLOCKS`), the second only if the tree changed since the first. Mixed results on one tree (pass and fail) make the promise `flaky`, advisory, and end any shadow wait. A check cut by the clock counts as nothing.
- Shadow to blocking (REV "3 clean days" outranks the spec text): the first PASS of a UTC day, while no would-block is open, is one clean day; 3 clean days promote. A would-block stores the failing tree in meta `would:<id>` (the first one is kept); a later PASS on a different tree is a true catch and promotes at once. No confirming rerun of a catch (spec-literal; see S2b.6 for the odds). A dispute, a drop, flaky or quarantine ends the wait (`promise::end_shadow_wait`: days back to 0, meta cleared).
- `torpor promise block ID` (user only): only tier `shadow`, state `live`, no open dispute, not flaky. It skips only the wait; the check itself is not run again.
- Oracle rules (src/shadow.rs `oracle`): every assertion's `why` shares a word of 4 or more letters with what the user said; a literal of 4 or more words must be in quotes the user typed; an unverifiable check never reaches shadow; praise written entirely in Hebrew stays advisory; a source other than repro, keep or praise stays advisory. "What the user said" = the prompt of the turn the promise was created in plus that turn's keep constraints, whatever the turn's origin; a provisional source is demoted to advisory at Stop by `upgrade_sources`; a promise made from the CLI by the user uses the newest turn's text.
- Mutants (src/shadow.rs `plan`, `kill`): up to 3 broken copies of the good version, each one hunk of the bad-to-good change undone (a file the change added is deleted, a file it removed is put back), the largest first; a change of exactly one hunk gives line deletions instead. A copy is killed when the check FAILs at an assertion in 3 of 3 fresh runs on it; the first kill makes the promise shadow. 0 kills of n tried, or no copy could run, or nothing could be undone: advisory with the reason in `promise show`. Skipped (advisory, "oracle untested") when the first good run took over 10 s (`COLD_MS`). Mutant and dependency runs are not stored in `promise_runs`.
- `promise::finish` is the one place that assesses a proven promise (source and oracle rules, mutants, `watched_json`, dependency record) and writes tier, state and the rest in one UPDATE; `qualify` and find-break `complete` both call it.
- Dependency record (src/deps.rs): one extra run of the good tree with `NODE_V8_COVERAGE` set and a preload (`--require`) that makes the app exit on SIGTERM, because a Node server killed by SIGTERM writes no coverage without it (measured on node v24.18.0: with the preload the coverage files are written, without it none). Files with an executed function and the sources named in source maps become `deps_json`; node_modules and `.env*` never. Any problem gives no record, and a promise with no record is run at every Stop that changes a relevant file. It runs only when the first good run took 10 s or less and enough time is left.

## S2b.5 Deviations and shortcuts (each one in code also has a `shortcut:` comment)

- Cold start (src/shadow.rs): the rule "a slow cold start skips the mutants" (spec 17) compares the whole time of the first good run, app start included, with `COLD_MS` (10 s). It is not a measured cold start.
- Source maps (src/deps.rs): `sourceRoot` is ignored when map entries become file names; a project that uses it gets fewer names and falls back to the run-on-every-third-Stop rule.
- Dependency record: it lists the JS files the app executed. Data files the app read (json, templates) are not in it, so a change to only such a file does not select the promise by overlap; the `stops_since_run >= 2` rule still runs it.
- Find-break (src/findbreak.rs): still assumes that once broken it stays broken (S2a shortcut); a revert in between can hide the culprit.
- B2 with a random check: the fixture route `/api/randflaky` fails 1 request in 3 at random, so it practically never reaches shadow (odds in S2b.6); the test shows "never blocks", not "is rejected by the oracle".
- Tests only: the new knob `TORPOR_TEST_PORTS` (fault-inject builds only; the plain binary does not contain it) and the per-test port windows in tests/common/mod.rs (S2b.6); tests/a10_real.rs `a10_run` now puts a committed `dist/<platform>/torpor` back after the run (the first re-run had overwritten and deleted the tracked 2.0.0 binary; it was restored with `git checkout`, history unchanged).
- Not done on purpose: no catch-confirm rerun (spec-literal), no new README sentence that compares Torpor with other tools (S2b.6).

## S2b.6 Open problems and NOT VERIFIED

- NOT VERIFIED: the Stop `decision: block` answer, the targeted PostToolUse line and the rebuild after compaction have never met a real Claude Code (A10 had no promise). A real AI following the skill (a user's complaint or confirmation becoming a check) is only simulated by `Sim`.
- NOT VERIFIED: CI (macOS, musl, Linux gnu) on the S2b commits; node versions other than v24.18.0 (the dependency record needs V8 coverage; tests that start the fixture app need `node`); timing tests (the 20-trial B2 test, the slow-check clock test) on a heavily loaded machine.
- A Torpor killed with SIGKILL (for example by Claude Code at the hook timeout) cannot stop the app it started: the app has its own process group, so it keeps running. The own deadline (30 s) is below the hook timeout (45 s) to avoid that; the watchdog and `Drop` stop the group in every other case.
- Intermittent test failures from port collisions: found, fixed in the tests, a residual is left.
  - Symptoms (each test passed alone, failed now and then in full runs): `add` ended with exit 1 and "run 3 of 5: UNRUNNABLE ... the app did not start (node server.js --port N exited 1)"; a check PASSed on a broken tree, so a promise came out FLAKY and not FAIL; the slow-check test got receipt `1/1/ok` and not `1/0/unverified`.
  - Cause: `scratch::free_port` binds port 0, reads the port and closes the socket; the app binds it a moment later. The tests of one binary run in parallel and start their apps at nearly the same moment. On Linux `bind(0)` with SO_REUSEADDR (Rust sets it) gives only odd ports from the lower half of the range: 3000 draws on this machine gave 3000 odd ports from 32773 to 46881 and 2457 different ones, which fits a pool of about 7,000 ports. So two tests got the same port now and then. The loser's app died with "address in use" (the UNRUNNABLE case), or, when the other app was already listening, the loser's check talked to the other test's healthy app and got a wrong verdict (the PASS on a broken tree and the `1/1/ok` cases).
  - Before the fix: the gate and promise test binaries, 2 copies of each at once, 5 rounds = 20 binary runs, 4 of them failed.
  - Fix, tests only: in fault-inject builds `TORPOR_TEST_PORTS=lo-hi` makes `free_port` pick a random free port inside that window; tests/common/mod.rs gives every test its own window of 100 ports below the system range (220 windows, from a counter and the process id). The product path is unchanged.
  - After the fix: the same loop, 10 rounds = 40 binary runs, 0 failed (at the old rate of 4 in 20, the chance of 0 in 40 is about 0.01 %). That loop ran the first version of the window pick; I then rewrote its arithmetic in u32 so that no window can overflow, and the full suites in S2b.2 ran with that final version.
  - Residual in the tests: tests/scratch_run.rs calls `Run::start` inside the test process (6 tests, kernel-picked ports from 32768 up, no window; the test windows end at 31999, so the two never meet). A collision among those 6 tests is possible; the rate is NOT measured (they passed in both full runs).
  - Residual in the product: between `free_port()` and the app's bind a foreign process (or a second Torpor) can take the port. Then the app dies (UNRUNNABLE, the Stop is not blocked and the promise is run again at the next Stop), or a healthy server of someone else answers the check, which can give a wrong PASS (a missed catch) or a wrong FAIL (a promise turns flaky and advisory). A wrong block needs 3 of 3 FAIL on three fresh ports plus a control PASS, which this cannot produce in practice. A slow-starting app makes the window longer. Fix for S2c: pick the port late, retry once when the app exits at once, check who owns the port (a start command that daemonizes would break a plain "our child owns it" test, so it needs care).
- NOT VERIFIED: the new restore branch of `a10_run` (it puts a committed `dist/<platform>/torpor` back) compiles but has never run, because A10 may be run only once. It runs only when `a10_run` reaches its end: a panic after the copy leaves the tracked binary overwritten, and `git checkout -- dist/linux-x64/torpor` puts it back.
- Odds for a check that fails 1 time in 3 at random (computed, not measured, except the 20-trial test): `(2/3)^5` = 13.2 % pass the 5-run proof (state `unproven`); 1 to 4 FAIL of 5 saves it as advisory `flaky` (the usual case, 17 of 20 trials in the B2 test); 5 of 5 FAIL (0.4 %) is not saved. If such a check were at blocking: per gate evaluation 66.7 % show nothing, 29.6 % end mixed (flaky), 2.5 % block (3 of 3 FAIL and a control PASS) and 1.2 % are set aside (control FAIL), so about 7 % of such checks would block once before they are demoted. The protection is the proof, oracle and mutant steps plus the 3-of-3 rule, not a guarantee; no text may say that a flaky check can never block. There is no confirming rerun of a catch (spec-literal): a lucky 3 of 3 FAIL followed by a later PASS counts as a true catch (3.7 % per evaluation for such a check).
- README comparison left out: the sentence that Torpor turns a user's complaint or confirmation into a check, and that no other tool does, is not in README.md. The first half depends on a real AI following the skill (only simulated, see above) and the second half is the lead's research statement (REV 12), which I cannot verify. The other claims in the README are tested. The lead can add the sentence from spec 28 after a real run.

## S2b.7 Next steps for S2c (in this order; if late, cut from the end)

1. DOM runner (spec 16, S2a item 10) and `torpor runner install` (the one allowed download). It prints the same protocol lines as the http runner so `runner::judge` scores them. Then remove the `http_only` gate in `Run::start_with`, update skills/torpor/SKILL.md and the README limit "Page (browser) steps cannot run yet".
2. Undo with checks (spec 20, S2a item 11): after `merge::undo`, run the live promises with `check_tree` (kind `undo`).
3. Port ownership in the product (S2b.6): pick the port late, retry once when the app exits at once, do not trust a ready probe that a process other than the app answered.
4. Optional: one rerun to confirm a catch; `sourceRoot`; a measured cold start; one real-AI run in real Claude Code with a promise (an extra ignored test next to `a10_real`) to settle the NOT VERIFIED items, then decide on the README comparison.
5. Keep: `cargo test` and `cargo test --release` green, the plain-binary size under 12,000,000 bytes, `sh tests/release_ignores_fault_env.sh <binary>`, the privacy script with tree counts at 0, the commit trailers from the brief.

# S2b fixes (after the Opus review of c4f6914)

(Appended by the S2b fix agent. Inputs: scratch/torpor-s2b-review.md (0 high, 4 medium, 6 low; its numbers are used below) and scratch/torpor-s2b-fix-brief.md. Start: c4f6914. Everything above stays as it was, except three places that describe the old `a10_run`, which put a committed `dist/<platform>/torpor` back after the run: the A10 re-run row in S2b.1, the "Tests only" bullet in S2b.5 and the NOT VERIFIED bullet in S2b.6. They are outdated: that restore branch is gone; `a10_run` runs on a temp copy of the plugin and never touches dist/ (review 10).)

## S2b-fix.1 Fixes and their tests

A test "fails before" when it was run on the old code and failed; S2b-fix.2 says which of them were.

| Review | Change | Test | Commit |
|--------|--------|------|--------|
| 1 (med) | Stop chain without `prompt_id`: `begin()` (src/hook.rs) reopens the session's newest turn (`newest_of`, src/turn.rs) when an event has no `prompt_id` and no turn is open, so a block continuation no longer starts a fresh `seq-N` turn with a fresh cap. `chain_state` (src/gate.rs) returns a Result: a store error now makes the Stop fail open instead of reading as "0 blocks so far". | tests/gate.rs `a_stop_chain_whose_payloads_have_no_prompt_id_blocks_at_most_twice`; unit `a_chain_that_cannot_be_read_is_an_error_not_zero_blocks` (src/gate.rs) | 47eddd6 |
| 2 (med) | A FAIL followed by an UNRUNNABLE rerun is `Unrunnable` and the check ends there (src/promise.rs `check_tree_with`); only a PASS after a FAIL is Flaky. One port race or clock cut no longer retires a blocking promise. | tests/gate.rs `a_fail_followed_by_an_unrunnable_rerun_is_not_a_flake_and_the_promise_stays_blocking` | 47eddd6 |
| 3 (med) | NOT FIXED here, on purpose, and it decides the release: the tracked dist/linux-x64/torpor still reports `torpor 2.0.0`. bin/torpor (V=2.1.0) installs a shipped binary only if it reports `torpor 2.1.0`; otherwise it runs the shipped dist binary where it is, uninstalled. So until dist/ is rebuilt, a user who installs the plugin gets 2.0.0 behaviour and none of S2b or these fixes: do not tag v2.1.0 before every dist/ binary is rebuilt. The brief keeps dist/ out of reach (the release bot rebuilds it). Rule for the rebuild: build every dist/ binary with `CARGO_TARGET_DIR=target/dist cargo build --release` and run `sh tests/release_ignores_fault_env.sh <binary>` on each; target/release/torpor made by `cargo test --release` is a fault-inject build and must never be copied into dist/. | none | |
| 4 (med) | README: the event time reads "Oct 26, 12:00 PM GMT-5 (17:00 UTC)"; the sentence about the `pre-event-base` tag is kept. | text only (checked with rg) | fb8001d |
| 5 (low) | README Limits: new "Network use" bullet (Torpor itself sends HTTP to 127.0.0.1 and to no other host; the app and its start command may use the network); the bullet about rejected environment values and step URLs now says exactly what is rejected and that the start command and the app are not limited; the mode B sentence of spec 30 is in word for word (checked with `rg -F`); "A promise is a small check. It checks that one thing works" replaces "proves one thing works". | text only | fb8001d |
| 6 (low) | An unverified promise also needs a relevant changed path (src/gate.rs `select`): a README-only turn or a turn without edits runs nothing. The unverified promise keeps its place first in line among the selected ones. | tests/gate.rs `a_check_that_cannot_be_read_is_unverified_and_first_in_line_next_time` (tail changed: a README-only turn and a no-edit turn give the receipt 0/0/none and the promise stays unrun, then a server.js edit runs it) | f4f7a74 |
| 7 (low) | Scratch copy (src/scratch.rs): once the copy is complete, every saved link that resolves outside it, or to its own root, is removed, so `d/b -> ..` and `d/c -> b/..` are not recreated; before `create_dir_all` for an overlay file, the nearest existing parent folder must be inside the copy. | unit `chained_links_that_lead_out_are_dropped_and_no_folder_is_made_through_one` (src/scratch.rs); tests/scratch_run.rs `chained_links_that_lead_out_of_the_copy_are_not_recreated` | 81fa169 |
| 8 (low) | Watchdog (src/scratch.rs, src/hook.rs): the process groups of running apps sit in a lock-free table (`LIVE`, 64 slots) next to a counter of spawns in flight; `kill_live_groups()` takes no lock, waits at most 1000 ms (`SPAWN_WAIT_MS`) for spawns that are in flight, then stops every group in the table. `App::spawn` registers the group right after the spawn, before anything can return early. The watchdog calls it right after it wins the output claim, before it logs the fault and writes the fallback. | unit `the_kill_takes_no_lock_waits_for_spawns_in_flight_but_only_so_long` (src/scratch.rs); tests/live_groups.rs `the_watchdog_kill_reaches_a_running_app` | 596286e |
| 9 (low) | `undo`, `back` and every restore path skip the project's `.claude/settings.json` and `.claude/settings.local.json` and say so in plain words, first in the report. Three layers: the plan (src/restore.rs `Plan::finish`, `is_claude_settings`, `CLAUDE_NOTE`), `apply_entry` (skips them even when an old journal lists them), and `undo` (src/merge.rs `undo_inner` skips them before the conflict analysis, so they can neither be rewritten nor make undo refuse). | tests/restore.rs `claude_code_settings_files_are_never_rewritten_and_the_report_names_them`; tests/undo.rs `claude_code_settings_files_are_never_touched_by_undo_and_never_make_it_refuse`; unit `claude_settings_files_leave_every_plan_and_are_said_first` and `applying_skips_a_claude_settings_file_even_from_an_old_journal` (src/restore.rs) | ae9fb05 |
| 10 (low) | The A10 tests no longer touch the tracked dist/: tests/a10_real.rs runs `claude` on a temp COPY of the plugin (`common::plugin_copy`, tests/common/mod.rs: .claude-plugin, bin, hooks, skills, and the binary at dist/<platform>/torpor). Chosen instead of a Drop guard: there is no panic window at all and nothing to restore. | tests/a10_real.rs `the_plugin_copy_holds_the_binary_and_the_committed_dist_is_left_alone` (runs without Claude) | ae6be5b |
| (extra) | A10-block harness: tests/a10_block.rs, three `#[ignore]` tests (free rehearsal, the one real run, report of a kept run). Result in S2b-fix.4. | `a10_block_seed` (rehearsal, passed) | eadbdfd |

## S2b-fix.2 Before and after

Each test below was run on the old code and failed there; the fix made it pass (final runs in S2b-fix.3).
- Reviews 1, 2, 6: the c4f6914 tree (git archive into a temp folder, not in the repo) plus the new tests/gate.rs, `cargo test --test gate` with the three names: 3 failed. Review 1: "a third block in the same chain" (the third Stop of one chain still blocked). Review 2: the receipt read `1/1/flaky`, the promise was retired as a flake. Review 6: the receipt of a README-only turn read `1/1/ok` instead of `0/0/none` (the unverified promise ran anyway).
- Review 7: the unit test and the tests/scratch_run.rs test both panicked on the old code.
- Review 8: on the old code a temporary test (not kept), which held `LIVE.lock()` while the watchdog fired, failed with "the watchdog did not reach the group while another thread held LIVE".
- Review 9: with the old src/restore.rs and src/merge.rs put back for one run (then restored byte for byte), the `back` test planned "Would change 4 files ... (.claude/settings.local.json)" and the undo test said "Can't undo turn 1 cleanly".
- Review 10 and the A10-block harness: no before-run (new test helpers; the rehearsal and the real run are their check). Reviews 4 and 5 are README text, checked with rg.

## S2b-fix.3 Final checks (run once, on the last code commit eadbdfd; the commit of these notes changes no code)

| Command | Result |
|---------|--------|
| `cargo test --no-fail-fast` | exit 0: 298 passed, 0 failed, 6 ignored in 32 `test result:` lines (31 test binaries and the doc-test run); no warnings |
| `cargo test --release --no-fail-fast` | exit 0: 298 passed, 0 failed, 6 ignored in the same 32 lines (this build has the fault-inject feature) |
| the 6 ignored (both runs) | `a10_block` x3 (rehearsal, the real run, the report), `a10_real` x2 (real Claude Code), `a5_latency` x1 (quiet machine); all are run by hand |
| `CARGO_TARGET_DIR=target/dist cargo build --release` | target/dist/release/torpor: 7,650,536 bytes (limit 12,000,000), `torpor 2.1.0` |
| `sh tests/release_ignores_fault_env.sh target/dist/release/torpor` | ok: ignores TORPOR_TEST_PANIC, TORPOR_TEST_SLEEP_MS, TORPOR_TEST_FREE_BYTES, TORPOR_FAULT=capture and TORPOR_FAULT=restore (hook took 0 s) |
| `git status --short dist` | empty: the tracked dist/ is unchanged |
| privacy script (scratch/gitlab-build1/privacy.sh, counts only) | tree files with denylist hits 0; files with a home path 0; HEAD message hits 0; history lines 2 (the known two); 55 commits. Key-like pattern files 8, tree emails 12 and placeholder-pattern files 2 are the same as on the c4f6914 tree (the two files are src/destructive.rs and this notes file, which names the pattern; checked with `rg -l`) |
| the 8 code commits 47eddd6..eadbdfd | no home path, no email other than the noreply ones, all by the per-repo identity |

Against the S2b numbers above (286 passed, 0 failed, 3 ignored in 30 `test result:` lines): +12 passing are the new tests of S2b-fix.1 (gate 2, scratch_run 1, live_groups 1, restore 1, undo 1, a10_real 1, unit tests in src: gate 1, scratch 2, restore 2), +2 binaries (a10_block, live_groups), +3 ignored (a10_block). The A10-block commands (not part of `cargo test`):
- `CARGO_TARGET_DIR=target/dist cargo build --release`, then `TMPDIR=/tmp/claude-1000 cargo test --test a10_block a10_block_seed -- --ignored --exact --nocapture` (free rehearsal, passed in 7.3 s);
- `TMPDIR=/tmp/claude-1000 TORPOR_A10_BIN=target/dist/release/torpor cargo test --test a10_block a10_block_run -- --ignored --exact --nocapture` (the one real run, S2b-fix.4);
- `TORPOR_A10_DIR=<the kept folder under /tmp> cargo test --test a10_block a10_block_report -- --ignored --exact --nocapture` (free).

## S2b-fix.4 The A10-block run (the one real Claude Code run)

How: `a10_block_run` seeds a synthetic project under /tmp (the login promise made blocking; a project-local hook that writes every UserPromptSubmit and Stop payload to payloads.txt next to the project), copies the plugin with the plain binary (`target/dist/release/torpor`, `torpor 2.1.0`), and runs `claude -p --model haiku --plugin-dir <copy> --allowedTools Bash,Edit,Write,Read --max-budget-usd 0.50 --strict-mcp-config --mcp-config '{"mcpServers":{}}' --output-format json` once (600 s timeout, every `CLAUDE*` variable except CLAUDE_CONFIG_DIR removed from the environment). The free rehearsal `a10_block_seed` passed first (7.3 s). The folder was kept under /tmp, not in the repo; `a10_block_report` prints the report from it again at no cost. The run used up the one allowed real run; do not repeat it without asking.

Result (the five items the brief asked for):
- Stop blocked: yes, twice. The text, identical both times (from the transcript's `hook_blocking_error`): `Torpor: promise #1 "login works" (the user's words, turn 3) fails now. Failing step: after sign-in the account data shows. It failed 3 of 3 fresh runs and still passes on the saved working version. Changed since it last passed: server.js (turn 4). Fix the app, not the check, and do not special-case test values. If this promise is wrong or outdated run `torpor promise dispute 1 "<why>"`; it asks the user instead of you.`
- Same prompt_id on the continuations: yes. The UserPromptSubmit payload and all 3 Stop payloads carry one `prompt_id` in one session; `stop_hook_active` was false, true, true. The store has one new turn (turn 4) and one gate_chain row with blocks=2. (So the `seq-N` path of review 1 did not occur with real Claude Code; that fix guards a case this run did not show.)
- Chain stopped after 2 blocks: yes. 3 Stop events; the receipts of turn 4 read block, block, unresolved. The third Stop was allowed with the notice "save point 10, turn 4 changed 1 file (1 edit), undo it: torpor undo 4" and "promise #1 (login works) failed after this turn (3 of 3 runs; it passed on the version from the start of turn 3). The AI was not stopped." Promise runs since the seed: gate FAIL x3, control PASS, gate FAIL x3, control PASS, gate FAIL x3. The tier stayed blocking; no faults.
- Cost: total_cost_usd 0.00755067 (notional, a Max subscription run). Claude: exit 0 after 37 s, subtype success, is_error false, 10 turns, no permission denials.
- permission_mode: "auto" in all four payloads. README already says what auto means (Torpor blocks risky commands instead of asking); no such command ran.
- Harness verdict line: `stop_events=3 blocks=2 same_prompt_id=true cap_held=true`.
- Also seen: the PostToolUse hint after the Edit ("Torpor: edit touches promise #1; `torpor check 1` before finishing"). Real Claude Code facts for later work: hook payloads carry `prompt_id`; a Stop continuation after a block keeps it; the block text reaches the AI as an attachment of type `hook_blocking_error`, the notice of an allowed Stop as `hook_success` with `hook_system_message`, and a PostToolUse hint as `hook_additional_context`.

Caveats:
- Torpor blocks a second time only when the project tree changed since the last block (`chain_open` in src/gate.rs). So the prompt tells haiku to add one comment line to server.js after each report; without that it would have been blocked once and let go at the second Stop. The run shows the cap (2 blocks, then the AI is let go), not what an AI does on its own.
- The first print of the harness said `same_prompt_id=false`: my parser counted the blank line after each payload as an event. Fixed in eadbdfd (blank lines are skipped) and re-checked on the kept folder with `a10_block_report` (true). The run was not repeated. The kept folder's seed.txt was written by hand from the store (the first run predates that file).
- One run, one model (haiku), one machine, one Claude Code version; it says nothing about other models.

## S2b-fix.5 Choices, deviations and a spec contradiction

- Spec 18 contradicts itself (review 6): "Nothing selected (only README, docs or tests changed, or no edits) -> nothing runs and nothing is printed" against "Not finished = 'unverified', which gets top priority at the next Stop". Choice: the relevance filter comes first (B2 also says a README-only diff runs nothing); "top priority" orders the promises that were selected, and an unverified promise is always picked once a relevant path changed (it skips the overlap test). If the lead wants an unverified promise to run on every Stop, the filter must let it through, and README-only turns then cost runs.
- Review 8, deviation from the brief's wording: the brief asks the kill to reach groups while another thread holds LIVE, with a bounded wait. There is no lock now (a lock-free table), so nothing can be held or poisoned; the only wait is the 1000 ms for a spawn that is in flight. A 65th app at once is stopped and refused with "too many apps are running at once"; a run holds a few apps at most (shortcut: 64 slots, raise if parallel runs grow). The watchdog now kills before it logs the fault and writes the fallback; it still fires at 30 s, the kill adds at most about 1 s, the hook timeout is 45 s. Ctrl-C of `torpor check` stays a README limit, as the brief says.
- Review 7: links that end inside a linked dependency folder stay (a linked node_modules still works); links that resolve to the copy's own root are dropped too; dangling and looping links are kept (marked `shortcut:` in src/scratch.rs: upgrade if one ever does harm).
- Review 9: the skip also covers an old journal (an older Torpor's journal cannot make a new one write them), and a skipped Claude Code settings file is not counted as a failure (the exit code stays 0). An empty `.claude` folder may still be planned as a Mkdir (harmless: its files are never written).
- Review 10: temp copy of the plugin, not a Drop guard (see the table).
- CI hand-off (scratch/torpor-s2b-ci.md): the A6 `maintenance.auto=false` fix is in since 332447d (noted at the top of S2b); nothing else is pending from it.

## S2b-fix.6 Open problems and NOT VERIFIED

- Review 3 is open on purpose and blocks the release: the tracked dist/linux-x64/torpor reports 2.0.0 and the launcher runs it uninstalled, so tagging v2.1.0 before the release bot rebuilds every dist/ binary (plain build, then the env script, see the table) ships 2.0.0 behaviour. The darwin and arm64 binaries were not looked at.
- NOT VERIFIED: `a10_run` (the older real run, tests/a10_real.rs) was not run by me and cannot be; it compiles, its Claude-free test passes, and the plugin-copy step it uses ran for real inside `a10_block_run`.
- NOT VERIFIED: GitHub Actions on these commits (the lead pushes), Windows, and any Claude Code version other than the one of the A10-block run; no payload without `prompt_id` was ever seen from real Claude Code.
- skills/torpor/SKILL.md still says "A promise is a small check that proves one thing works." (README was fixed; the brief did not list SKILL.md). Change it to "checks that one thing works" with the next edit.
- Still true from S2b.7 (next steps for S2c): DOM runner and `torpor runner install`, undo with checks, port ownership in the product.
- When S2c adds `torpor runner install` (the one allowed download), the README's new Limits bullet "Network use" (Torpor itself sends HTTP to 127.0.0.1 and to no other host) and the bullet about rejected environment values and step URLs become wrong: spec 28 wants "network use is localhost checks plus the one confirmed runner download". Change both in the same commit that adds the download.


# S2c: DOM runner, undo with checks, port ownership, 2.2.0

Build agent: S2c (Sonnet), started at eadc9f3 (v2.1.0 plus the release bot's dist/ commit), branch main, nothing pushed. Written Oct 22; the hand-off date was Oct 24 12:00 UTC.

## S2c.1 Status (against "Next steps for S2c" in S2b.7)

| Item | State | Commit | Tests |
|------|-------|--------|-------|
| 1. DOM runner and `torpor runner install` (spec 16, 24) | DONE | cd017e3 | tests/dom_run.rs: 4 with a fake runner and 1 real-Chromium test (ignored, run by hand, S2c.3); tests/runner_install.rs: 6; unit tests in src/dom.rs; the runner-install block of tests/release_ignores_fault_env.sh. README (Install, page checks) and SKILL.md (page steps) updated; the old limit "page steps cannot run yet" is gone |
| 2. Undo with checks (spec 20) | DONE, except the build-command step (S2c.4 item 4) | 5924245 | tests/undo_checks.rs: 6; tests/undo.rs: 2 expectations updated; unit tests: `gate::meets` 1, overlay exec bit 1 |
| 3. Port ownership (S2b.6) | DONE (Linux; macOS trusts the answer) | e7f588c | scratch.rs unit tests: 3; tests/port_owner.rs: 3 (2 are Linux-only); the release script also checks that a plain build has no `TORPOR_TEST_PORT` |
| Version 2.2.0 | DONE | 8f40b7d | Cargo.toml, Cargo.lock, bin/torpor, plugin.json and the two tests that name the version (a10_block.rs, destructive.rs). dist/ not touched |
| S2b.7 item 4 (rerun to confirm a catch, `sourceRoot`, cold start, a real-AI run) | NOT DONE: out of scope for S2c | | |

## S2c.2 How to run the checks, and the latest results

All from the repo folder, on the code of 8f40b7d (the commit of these notes changes no code).

| Command | Result |
|---------|--------|
| `cargo test --no-fail-fast` | exit 0: 331 passed, 0 failed, 7 ignored in 36 `test result:` lines; no warnings. 317 after item 1, plus 6 undo_checks, 1 `gate::meets`, 1 overlay exec bit, 3 scratch port-ownership unit tests, 3 port_owner |
| `cargo test --release --no-fail-fast` | exit 0: the same numbers (this build has the fault-inject feature, like the test build) |
| the 7 ignored (both runs) | a10_block x3, a10_real x2, a5_latency x1 (as in S2b) and the real-Chromium test in dom_run (S2c.3) |
| `CARGO_TARGET_DIR=target/dist cargo build --release` | target/dist/release/torpor: 7,789,352 bytes (limit 12,000,000), `torpor 2.2.0` |
| `sh tests/release_ignores_fault_env.sh target/dist/release/torpor` | ok; it now also checks that TORPOR_TEST_REGISTRY, TORPOR_TEST_INTEGRITY, TORPOR_TEST_NO_BROWSER and TORPOR_TEST_PORT are not in the plain binary |
| `git status` | clean; the 4 tracked files under dist/ are unchanged |
| privacy script | tree files with denylist hits 0; history lines with hits 2 (as before); emails in the tree 12; files with a home path 0; key-like pattern files 8; placeholder files 2; HEAD message hits 0 |
| mutation check of the port tests (once, by hand; the source was restored and compared with cmp) | foreign-answer check off: 2 of the 3 port_owner tests fail; retry off: all 3 fail |

## S2c.3 The two real runs (network: 1 of the 2 allowed npm downloads used; no Chromium download; no model call)

- Real install: `env -u CLAUDECODE HOME=<repo>/target/real-runner-home/hm TORPOR_HOME=<repo>/target/real-runner-home PATH=/usr/bin:/bin:<folder of node> timeout 240 target/dist/release/torpor runner install --yes` gave "installed playwright-core 1.64.1 (3.0 MB)", exit 0, about 1 s. `env -u CLAUDECODE` is needed because the command is user-only and the agent shell has CLAUDECODE set (the first try was refused for that reason, as designed).
- Real DOM run: `TORPOR_REAL_RUNNER=<repo>/target/real-runner-home/runner/1.64.1 timeout 280 cargo test --test dom_run -- --ignored --nocapture`: 1 passed in 17.11 s. Fixture app, its /dom page, system /usr/bin/chromium, headless (no DISPLAY). Good page: PASS in 1.1 s with 6 assertions. The seeded regression: FAIL at step 5 in 6.1 s. A missing test id: FAIL in 6.0 s (it waits out the locator timeout). `?err=1` (a console error): FAIL in 1.0 s. A link to another host: UNRUNNABLE in 1.1 s ("non-loopback: the page went to elsewhere.invalid"). No browser process and no /tmp/tp-* folder was left. The agent browser, its profile and the user's Brave were not used.

## S2c.4 Decisions, deviations and shortcuts (each in code also has a comment)

1. Browser runner temp folder (deviation): each run gets a short folder (`ShortTmp`, /tmp/tp-*) as TMPDIR, because Chromium's socket path is too long inside the scratch folder (seen in the first real run). It goes with the run.
2. `torpor runner install` is user-only (refused under CLAUDECODE, listed in destructive.rs) and asks first; `--yes` skips the question. It installs playwright-core only: npm registry, hash checked before unpacking, no npm, no install script. It uses a Chromium that is already on the computer. When none is found the question names "about 150 MB more for a Chromium from Playwright's servers" (a fixed estimate, never run here: S2c.5).
3. Undo checks run inside the undo's store lock and open transaction, 90 s in all (src/undo_check.rs). This breaks the old rule (scratch.rs doc, promise.rs `write`: no store lock or transaction across a run). Why: the undo must decide before it writes, and a lock released in between lets the project change under the check. Cost: for up to 90 s a hook that wants the store lock waits 100 ms and goes on as Busy (fail open: its capture is skipped and the next save point holds the change). That can only meet a user who runs `torpor undo` in a second terminal while Claude Code works; when the AI runs it, Claude Code waits for the command. The project's run lock is waited for 5 s; if another check holds it, the undo goes ahead and says "Another check is running in this project, so the result is untested."
4. Spec 20's build-command step (run the newest build command on the result) is NOT done: it needs exit codes of build commands (P1).
5. The undo's scratch copy leaves out links, permission-only changes on a file that stays, secret files, `.env` files, dependency folders and files over 16 MB. The report says how many ("N changes ... were not part of the check") and README Limits says it. An overlay file is executable when the file it replaces was, or when the undo brings back an executable (`Opts.exec`).
6. Spec 18 (line 518) applied: a check that cannot run never blocks, also when the app no longer starts after the undo ("did not count: it could not run (...)"). OPEN QUESTION for the lead: should an app that no longer starts after an undo block it?
7. Which promises run: `gate::eligible && gate::meets` (a watched file is in the undo, or there is no dependency record and a relevant non-doc file is). A FAIL in the copy blocks only when the same promise passes on the live project now (the control run). Flaky, unrunnable, out of time and already failing never block. The runs are stored after the restore with kind `undo` and no tree, so they never count toward a flake estimate or a last verdict.
8. `--force` skips the checks and adds no note. It is NOT user-only: the AI could type it (SKILL.md says never to add it). OPEN QUESTION for the lead: put `undo --force` on the user-only list in destructive.rs? `--dry-run` names the promises it would check and runs none.
9. Port ownership (scratch.rs): the port is picked after the copy is made, once per start; the early env check uses a provisional port (40000) because the spec's env rule only counts digits. One retry in all, for "the app ended within 3 s of its start before any ready answer" and for "a 2xx came from a process that is not the app". The second failure is UNRUNNABLE and names the pid. On Linux the holder is read from /proc/net/tcp, /proc/net/tcp6 and the fd links of every process Torpor may read; the app is its process group plus everything below it by parent pid. No visible holder (another user's or root's sockets) is trusted. Elsewhere it is trusted too (`shortcut:` comment: no lsof for macOS).
10. Seams, fault-inject builds only: TORPOR_TEST_REGISTRY, TORPOR_TEST_INTEGRITY, TORPOR_TEST_NO_BROWSER, TORPOR_TEST_PORT (the ports of the 1st and 2nd start, comma separated; read before the older TORPOR_TEST_PORTS window). A plain build does not carry the strings (release script).

## S2c.5 Open problems and NOT VERIFIED

- The Chromium download of `runner install` was never run (one download was allowed; this computer has /usr/bin/chromium). The "about 150 MB" is a fixed estimate in the question, not a measured size.
- Real browser runs: Linux and one system Chromium only. macOS and the other browsers the finder looks for (Chrome, Edge, Brave) are NOT VERIFIED; macOS CI runs the fake-runner tests, not the real one.
- Port ownership on macOS is not checked at all (the answer is trusted). On Linux a server that listens through a helper outside the app's group (a daemon that left its group, a container proxy that runs as the user) looks like another process, and the check is reported as could not run. That fails safe (never blocks) but loses the check.
- The undo checks' 90 s limit and the 5 s "another check is running" path have no test (they need a slow app and a held run lock). The hook behavior during the undo's lock (S2c.4 item 3) is read from the lock code, not tested.
- Both real runs used the fixture app only; a project with a bundler dev server was not tried.

## S2c.6 Left for the lead and for S3

- Lead: Opus review of cd017e3..8f40b7d; push to s2 and CI (macOS: the real-browser test is ignored there, one port_owner test runs there); release v2.2.0 (the bot makes dist/); tag `pre-event-base` on the last pre-event commit before Oct 26 16:00 UTC (the README says Oct 26, 12:00 PM GMT-5 = 17:00 UTC); decide the two open questions (S2c.4 items 6 and 8).
- S3 (the event, from Oct 26 17:00 UTC): S2b.7 item 4: one real-AI run in real Claude Code with a promise (an extra ignored test next to a10_real), a rerun to confirm a catch, `sourceRoot`, a measured cold start; then the README comparison decision.

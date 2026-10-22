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
| 7 A10 re-run | done, passed (S2b.3) | this file's commit | tests/a10_real.rs (unchanged) |

Also in S2b: the A6 fix 332447d (a CI race: git's background maintenance wrote a lock file inside the project while a Torpor step ran; `maintenance.auto` is now false in that test's repo) and 401b07a (macOS adds `__CF_USER_TEXT_ENCODING` to every process env, allowed in the B7 env check). Both came from the lead's CI notes.

## S2b.2 How to run the checks, and the latest results

Measured on the working tree that this commit contains: the tree of the previous commit plus the test and `free_port` changes named in S2b.5 and S2b.6, and this file.

| Command | Result |
|---------|--------|
| `cargo test` | 286 passed, 0 failed, 3 ignored in 30 test binaries (`--no-fail-fast`, exit 0; about 2.4 min with the build) |
| `cargo test --release` | 286 passed, 0 failed, 3 ignored in 30 test binaries (`--no-fail-fast`, exit 0; about 2.3 min with the build) |
| `cargo test --test gate b2_a_check_that_fails -- --nocapture` | ok in 20.2 s: `B2 random flaky: 0 of 20 trials blocked; saved as -> at the end: {"advisory flaky -> advisory flaky": 18, "advisory live -> advisory flaky": 1, "unproven live -> unproven flaky": 1}`; the regression test `b2_the_seeded_regression_blocks_20_of_20` passes in both full runs |
| `cargo test --test budget -- --nocapture` | ok in 17.8 s: `B6: lines per turn <= 986 tokens (storm turn 986, cap 1000), tokens per cycle [3995, 3990, 3990] (cap 4000), rebuilds [556, 556] tokens (cap 2000), session total 11975` |
| `CARGO_TARGET_DIR=target/dist cargo build --release` (the plain binary that ships) | 7,621,000 bytes (budget: under 12,000,000; S2a: 7,130,856) |
| `sh tests/release_ignores_fault_env.sh target/dist/release/torpor` | ok |
| privacy script | tree files with denylist hits 0; files with a home path 0; history lines 2 (the known two); HEAD message hits 0; 45 commits before this one |

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
  - Residual in the tests: tests/scratch_run.rs calls `Run::start` inside the test process (6 tests, kernel-picked ports, no window). A collision there is possible; the rate is NOT measured.
  - Residual in the product: between `free_port()` and the app's bind a foreign process (or a second Torpor) can take the port. Then the app dies (UNRUNNABLE, the Stop is not blocked and the promise is run again at the next Stop), or a healthy server of someone else answers the check, which can give a wrong PASS (a missed catch) or a wrong FAIL (a promise turns flaky and advisory). A wrong block needs 3 of 3 FAIL on three fresh ports plus a control PASS, which this cannot produce in practice. A slow-starting app makes the window longer. Fix for S2c: pick the port late, retry once when the app exits at once, check who owns the port (a start command that daemonizes would break a plain "our child owns it" test, so it needs care).
- Odds for a check that fails 1 time in 3 at random (computed, not measured, except the 20-trial test): `(2/3)^5` = 13.2 % pass the 5-run proof (state `unproven`); 1 to 4 FAIL of 5 saves it as advisory `flaky` (the usual case, 17 of 20 trials in the B2 test); 5 of 5 FAIL (0.4 %) is not saved. If such a check were at blocking: per gate evaluation 66.7 % show nothing, 29.6 % end mixed (flaky), 2.5 % block (3 of 3 FAIL and a control PASS) and 1.2 % are set aside (control FAIL), so about 7 % of such checks would block once before they are demoted. The protection is the proof, oracle and mutant steps plus the 3-of-3 rule, not a guarantee; no text may say that a flaky check can never block. There is no confirming rerun of a catch (spec-literal): a lucky 3 of 3 FAIL followed by a later PASS counts as a true catch (3.7 % per evaluation for such a check).
- README comparison left out: the sentence that Torpor turns a user's complaint or confirmation into a check, and that no other tool does, is not in README.md. The first half depends on a real AI following the skill (only simulated, see above) and the second half is the lead's research statement (REV 12), which I cannot verify. The other claims in the README are tested. The lead can add the sentence from spec 28 after a real run.

## S2b.7 Next steps for S2c (in this order; if late, cut from the end)

1. DOM runner (spec 16, S2a item 10) and `torpor runner install` (the one allowed download). It prints the same protocol lines as the http runner so `runner::judge` scores them. Then remove the `http_only` gate in `Run::start_with`, update skills/torpor/SKILL.md and the README limit "Page (browser) steps cannot run yet".
2. Undo with checks (spec 20, S2a item 11): after `merge::undo`, run the live promises with `check_tree` (kind `undo`).
3. Port ownership in the product (S2b.6): pick the port late, retry once when the app exits at once, do not trust a ready probe that a process other than the app answered.
4. Optional: one rerun to confirm a catch; `sourceRoot`; a measured cold start; one real-AI run in real Claude Code with a promise (an extra ignored test next to `a10_real`) to settle the NOT VERIFIED items, then decide on the README comparison.
5. Keep: `cargo test` and `cargo test --release` green, the plain-binary size under 12,000,000 bytes, `sh tests/release_ignores_fault_env.sh <binary>`, the privacy script with tree counts at 0, the commit trailers from the brief.

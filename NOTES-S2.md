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

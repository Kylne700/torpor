# S2d: pre-event fixes (engine only, no model calls)

Base dba9762 (v2.2.0). Eight local commits on main, none pushed, no tag, no version bump, dist/ and CI untouched.

| # | Item | Commit subject | Tests |
|---|---|---|---|
| 1 | M5 | refuse an undo for a failed start only after a second failed start | tests/undo_checks.rs: app that fails only its first start -> undo exit 0. Never-starting app (exit 3) and --force are covered by the two older tests |
| 2 | M3 | demo/run.sh, an offline end-to-end story | tests/demo.rs runs it and checks 7 lines |
| 3 | M4 | README install first, 60 second sample, troubleshooting; plugin metadata | tests/launcher.rs passes; the wording grep hits only the known line |
| 4 | S2 | one welcome line per folder at SessionStart | tests/contract.rs (new test, one old test updated) |
| 5 | S3 | find-break probes one older state, warns about a second break | tests/findbreak.rs: break, fix, break again |
| 6 | S4 | reap leftover process groups and tp-* temp folders | unit tests in scratch.rs and dom.rs |
| 7 | S1 | undo runs the last plain build command on the result | tests/undo_checks.rs (refused + --force, still passes), unit test is_build |
| 8 | S5 | tests for the 90 s check limit and the 5 s run-lock wait | tests/undo_checks.rs, two knobs (fault-inject builds only) |

S6 (Chromium download run) skipped as told (network). M1, M2, M6 are the lead's.

## Results
- `cargo test --no-fail-fast`: 353 passed, 0 failed, 7 ignored (baseline 342/0/7). CI has no clippy step, so none was run.
- demo/run.sh: 3 of 3 runs exit 0 with identical output (times and clock values stripped), about 17 s each, offline.
- Plain release build (`CARGO_TARGET_DIR=target/dist`): 7,829,096 bytes (limit 12,000,000); `tests/release_ignores_fault_env.sh` ok.
- Privacy script (scratch/gitlab-build1/privacy.sh): home-path files 0, history lines 2 (the known two), HEAD message hits 0.

## Decisions
- M5: on "the app did not start" the check is run a second time on the result; only a second failed start (and a live control that passes) refuses. A flaky or late control never refuses, as before. Costs one more run only in that case.
- M3: no new command (the help stays at 24 lines). The demo story is: turn 1 writes the app with the seeded bug, turn 2 fixes, turn 3 the AI saves the login check, turn 4 breaks it with `sed -i`, turn 5 a readme, a hand rewind of the edit-tool file, find-break names turn 4, `undo 4` is checked and goes, `undo 2` (the fix) is refused by the covering promise (exit 3), then `promise block 1` (user path) and a later shell break is blocked once at Stop. The agent never uses --force. HOME, store and key are in a temp folder. The demo needs node and a built binary (TORPOR_BIN overrides).
- The check word in the prompt matters: the login promise only reaches the shadow tier when a check step uses a word the user said ("sign-in"), so `promise block` works.
- S2: the promise cap of 50 (exit 3, plain text) already existed with a test (tests/promise.rs:149). Only the welcome line was added: "Torpor is saving this folder ..." once per folder, after any problem notice.
- S3: no new Outcome field. After a sure culprit, if no failing run older than the parent exists yet, one probe of the state before the parent is added; any failing older state adds "Possibly more than one break" to the report. Happy path costs one extra run (the 6-turn test still lies in its 10-24 range).
- S4: each started app writes `<log>.pgid` in its scratch folder. When the run lock is taken, folders with a pgid file are leftovers: their groups are killed (only if the folder is under 1 h old, against pid reuse) and the folder is removed. Folders without a pgid file stay until 1 h old, because the same process may hold them (saved copy, coverage run). Old `tp-*` folders of this user in the temp dir are removed.
- S1: build command = newest Bash `tool_events` row with exit 0 whose preview is plain (letters, digits, space and `_./:=@-` only) and starts with npm run build / pnpm build / yarn build / bun run build / tsc / npx tsc / cargo build|check / python(3) -m compileall / npm run typecheck. It runs in a scratch copy of the result (same sanitized env), 40 s each, again on the project as it is when it fails. Refuses only if the control passes. Can also run when no promise covers the files. README limit sentence updated.
- S5: `TORPOR_TEST_CHECK_SECS` and `TORPOR_TEST_RUN_WAIT_MS` exist only with the fault-inject feature.

## Shortcuts and limits
- The README sample in "A 60 second look" is trimmed (times removed) and one line is reworded ("Undo that turn") to keep the no-"only" rule.
- S3 still misses a revert hidden between two probed states (comment in findbreak.rs).
- S1 does not install dependencies in the scratch copy; a build that needs node_modules not present there fails in both runs and then "does not count". Command previews are cut at 200 characters.
- S4 does not reap on macOS-specific temp paths beyond `temp_dir()` and `/tmp`.

## NOT VERIFIED
- `claude plugin validate .` and a real install (M1), the interactive smoke (M2): not run.
- cargo test --release, macOS and musl CI: not run here.
- S4 with a real kill -9 of a whole Torpor process: only the unit test (a foreign `sleep` group with a pgid file).
- S1 with a real npm/tsc/cargo project: only python compileall.

## For the lead
Review, then push; wait for the release bot's dist commit; check `torpor version` of the 4 binaries; run M1/M2; decide whether NOTES files stay public; tag `pre-event-base` before Oct 26 16:00 UTC.

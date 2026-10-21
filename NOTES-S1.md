# NOTES-S1: S1a hand-off (written by the S1a builder; S1b reads this first)

Spec: `research/30-torpor-spec.md` (build contract sections 0-11). On conflict: `research/31-9-review.md` > `research/29-torpor-v2-design.md` > spec.
Probe results in full: `research/32-s1-probes.md`. Code: this folder (about 4,700 lines of Rust in `src/`, about 3,100 lines of tests).

## 1. Status of the S1a items

| item | state |
|---|---|
| Day-1 probes P1-P9 + P10 | DONE, `research/32-s1-probes.md`. P7 NOT VERIFIED (needs an interactive run). |
| Store + scan + retention/thinning/GC/caps + key (spec 2, 5) | DONE |
| Hooks + turn model (spec 3, 4) | DONE: all 10 events, fail-open, Stop line, restore notice, deny for edits into the store/key |
| Packaging minimum (`.claude-plugin/plugin.json`, `marketplace.json`, `hooks/hooks.json`, `bin/torpor`) | DONE. `claude plugin validate .` passes (run once on the folder; nothing installed). README + icon are S1b. |
| Tests A1, A4, A5, A6, contract, P10, secrets/perms (A7 canary half), status sources, retention/GC | DONE and green |
| Restore, undo, recover, destructive save points, ask/deny, rest of the CLI, CI, README, A2, A3, A7 restore half, A8, A9, A10 | NOT STARTED (S1b) |

Git: everything is committed (`git log --oneline`); per-repo identity Kylne700. No remote exists yet (the lead creates Kylne700/torpor).

## 2. Run the tests

```
cargo test                      # debug: 59 passed, 1 ignored (A5), 0 failed, about 9 s
cargo test --release            # release: 59 passed, 1 ignored, 0 failed, about 17 s
cargo test --release --test a5_latency -- --ignored --nocapture    # A5, about 55 s, needs a quiet machine
tests/release_ignores_fault_env.sh                                  # plain release build ignores the fault env vars
```
Per suite (same in debug and release): unit 10; a4_failopen 6; a6_writes 1; contract 13; launcher 4; p10 3; retention 7; secrets 2; sim (A1) 4; status 9.
Flakiness check on the last build: `a6_writes` 20 runs, 0 failures; `retention`+`status`+`secrets` 15 runs, 0 failures.
Mutation checks (each made its test fail, then reverted): `is_secret` always false; `mask()` returning its input; status `.json`-only check removed; GC grace removed; per-day thinning rule off.

Things that bite:
- `cargo test --release` builds the binary WITH the `fault-inject` feature (the crate lists itself as a dev-dependency with that feature). `cargo build --release` builds it without. Both write `target/release/torpor`. A5 numbers come from the fault-inject build. `tests/release_ignores_fault_env.sh` proves the plain build ignores `TORPOR_TEST_PANIC`, `TORPOR_TEST_SLEEP_MS`, `TORPOR_TEST_FREE_BYTES` (hook took 4 ms); add it to CI.
- The harness (`tests/common/mod.rs`) gives every spawned process temp `TORPOR_HOME`, `TORPOR_KEY_FILE`, `CLAUDE_PROJECT_DIR`, `HOME`, `XDG_CONFIG_HOME` and removes `CLAUDE_CONFIG_DIR`. **Library calls made from test code run in the test process and read THAT process's env** (`user_home`, `claude_dir`, `home_dir`, `key_path`, `CLAUDE_PROJECT_DIR`, `TORPOR_TEST_FREE_BYTES`). `Ws::open_in(home, root, create)`, `retention::thin` and `retention::gc` (store under the cap) are env-free. `retention::thin_if_due`, `window_days` and an over-cap `gc` read `claude_dir()`: drive those through the binary, never in-process.
- The three P10 tests share a mutex: a fork in another test thread delays inotify CLOSE_WRITE events (found as 2/40 flaky; 0/150 after). Keep them serialized.
- Disk: build output is under `target/` (about 2-3 GB with debug and release). A5 builds its 5,000-file repo under `target/`, not `/tmp` (tmpfs).
- musl: `rustup target add x86_64-unknown-linux-musl` worked, but `cargo build --release --target x86_64-unknown-linux-musl` fails here because there is no musl C compiler (`x86_64-linux-musl-gcc`, `musl-gcc`, clang and zig are absent; bundled SQLite, zstd and BLAKE3 need one). **musl build NOT VERIFIED.** CI needs `musl-tools` and `CC_x86_64_unknown_linux_musl=musl-gcc`.

## 3. Probe outcomes that shaped the code (details: research/32)

- P1: a non-zero exit arrives as PostToolUseFailure with `error` = `"Exit code N"`; there is no exit-code field (the code parses `^Exit code (-?\d+)`; PostToolUse means 0). Hook env has CLAUDE_PLUGIN_ROOT, CLAUDE_PLUGIN_DATA, CLAUDE_PROJECT_DIR, CLAUDECODE; hook cwd = project dir; `bin/` is on the Bash PATH; CLAUDECODE is set in the Bash tool env (user-only CLI refusal works).
- P2: `prompt_id` is on every event except SessionStart. NEW: a finished background subagent re-enters UserPromptSubmit with `<task-notification>` and the SAME prompt_id: handled as a continuation (turn reopened, not a new turn).
- P3 NO: the model does not see the Stop `systemMessage`: the Stop line is on every turn that changed files (default).
- P4 yes (exec-form hooks). P5 yes (`-p` runs only): detached `gc --if-quiet` at SessionEnd, at most once a day. P6 yes: under bypass `ask` did not run and `deny` blocked: keep ask/deny (S1b, spec 7.2). P8 yes: UserPromptSubmit `additionalContext` reaches the model. P9 yes: subagent events carry `agent_id` and `agent_type`.
- P10: **a read-only open of a WAL database writes next to it** (opens `-wal` read-write, creates/changes `-shm`; a cleanly closed db gets a new 0 B `-wal` and a 32768 B `-shm`). Rollback-journal databases are untouched. So Torpor never opens a database: it reads `*.db`, `*.sqlite*`, `*.sql` as plain bytes (torn-read rule) and records `x:db_busy` when `<db>-wal` is non-empty or `<db>-journal` exists. `-wal`/`-shm`/`-journal` are never stored. The rusqlite `backup` feature is a dev-dependency only (the P10 test measured what the backup API writes).

## 4. A5 latency (spec budget: hook p50 <= 36 ms, p99 <= 150 ms)

200 runs per event through the launcher, release build with fault-inject, i7-12700 on the `powersave` governor (noise about +-4 ms). First capture of the 5,000-file repo 207 ms. Launcher `torpor version` p50 1.5 ms. In-process `scan::scan` p50 9.4 / p99 12.5 ms (budget <= 25: PASS). 1,202 points, 0 faults, the final point matches the oracle.

| event | p50 ms | p99 ms |
|---|---|---|
| UserPromptSubmit | 36.7 | 59.0 |
| PostToolUse(Bash) | 33.1 | 55.4 |
| Stop | 32.9 | 51.5 |
| PostToolUseFailure | 32.5 | 76.6 |
| StopFailure | 31.8 | 47.0 |
| SessionStart | 10.4 | 22.0 |
| SubagentStop | 4.7 | 11.0 |
| PreCompact | 4.1 | 9.1 |
| PreToolUse(Edit) / (Bash) | 3.5 / 3.3 | 9.4 / 8.6 |
| PostToolUse(Edit) | 3.1 | 9.7 |
| SessionEnd | 2.8 | 8.4 |

Result: p99 PASS on every event (max 76.6). **p50: 11 of 12 pass; UserPromptSubmit misses by 0.7 ms (36.7 vs 36)**; other runs gave 34.8 (60 runs) and 39.2 (older 200-run run), so it is noise-limited. Pooled p50 6.3 / p99 50.4 / max 141.3 ms (the maxima are the first events after project generation: files younger than the 2 s racy window are re-hashed). Before the A5 work, full-scan events had p50 49-55 ms. The daily detached gc spawn is switched off in tests (`TORPOR_TEST_NO_GC=1`), so A5 excludes it.

Where a full-scan hook spends its time (Stop, p50 28 ms in-process): walk and stat cache load overlap at about 14 ms (the walk is the bottleneck), tree build 3.2, leaves 2.0, hash 1.5, cache rows 1.0, open 0.8, process start/exit 3-4.

**Designed, NOT built (only if latency must drop further):** a per-directory `dircache(dir TEXT PK, ok INT, lsig BLOB16, csig BLOB16, tree_id BLOB32, files BLOB)` replacing `stat_cache`. `lsig` = blake3 over the directory's non-directory walker entries (name, kind, size, mtime, ctime, ino, mode, secret flag + key tag, reason); `csig` = child directories' (name, tree id); a clean directory reuses `tree_id`; complete scans only; `diff_dir` loads skipped directories lazily; GC clears `dircache` whenever it deletes any blob; `capture_named` stops touching the cache. Expected saving about 17 ms in-process. It deviates from the spec's `stat_cache`, and restore step 6 (spec 6.3: update `stat_cache`) would have to change with it, so S1b must agree before anyone builds it. Smaller options: a custom parallel `read_dir` walker (about 5 ms; deviates from spec 5.1), reading cache rows by reference, an Fx-style hasher.

## 5. Behaviour and schema facts S1b must know

Schema: exactly spec 2.3 (`PRAGMA user_version=1`, `SCHEMA_SQL` in `src/store.rs`). Additions and conventions:
- All timestamps are Unix **nanoseconds** (i64). `blobs.id` and `points.tree_id` are 32-byte BLOBs. `changes.old_id/new_id` are hex TEXT, or `x:<reason>` for a path not captured, or NULL for absent. Modes are stored as 0644 / 0755 (+ exec bit only). No `changes` rows for directories. The first point has no baseline, so it has no `changes` rows. `sessions.notified_restore_id` tracks the newest restore each session was told about.
- Point kinds: `start`, `end`, `tool`, `pre_bash`, `pre_restore`, `restored`; only `tool` is invisible (`visible=0`). `partial=1` when the hash budget ran out or a cap was hit. `dropped=1` = thinned (never deleted from the table). A turn has `start_point` NULL when the disk was too full to capture.
- Tree entries (`store::TreeEntry`): kind `f` file, `l` symlink, `s` secret file (encrypted, keyed id: read it with `read_secret`), `d` directory, `x` not captured (the id field holds the reason code: `cap`, `too_big`, `db_busy`, `unreadable`, `name`, `special`; `changes.old_id/new_id` show it as `x:<reason>`). Canonical JSON `{"v":1,"e":[[name,type,mode,id_hex,size],...]}` (hand-written `encode_tree`, tested against serde_json). A restore must skip `x` entries and tell the user (spec 6).
- Secrets: XChaCha20-Poly1305, keyed id, key file `torpor-key-v1 <key_id 8hex> <64hex>`, created at the first secret capture (`create_new`, 0600 in a 0700 folder). Reading needs `Key::load()`; `Store::read_secret(db, id, key)`. A changed key makes every secret file appear changed once (class `outside`).
- Capture and attribution: `meta.consumed_event_id` is a watermark bumped at each full scan (classification reads the OLD value). Classes per spec 4.3 (`tool`, `shared`, `shell`, `outside`, `subagent`-attributed via `agent_id`, ...). Gap changes are recorded on the `start` point with `turn_id` NULL. An implied end (new prompt while a turn is open) writes two points. SubagentStop is stored as a `tool_events` row. Slow mode cannot tell main-agent Bash from subagent Bash inside one turn (subagent wins).
- Transactions: every event runs inside one `Transaction::new_unchecked(&ws.conn, Immediate)`; a capture runs inside `SAVEPOINT cap` (on failure: `ROLLBACK TO`, `fault_tx`, the event goes on). Orphan loose files left by a rolled-back capture are removed by GC after the grace. Heartbeat is upserted in the same transaction. SQLite: WAL, `synchronous=NORMAL`, `busy_timeout=1000 ms`, `NO_CKPT_ON_CLOSE`; `Ws::drop` truncates the WAL when `-wal` > 512 KiB (so `-wal` and `-shm` stay on disk between hooks; the tests expect them).
- Locks: hooks take the shared lock; GC (and so restore/recover in S1b) must take `lock_exclusive`. Every hook opens the store with `create=true` (the first hook in a folder makes it; a refused root, spec 2, makes nothing); the CLI opens with `create=false` (missing store: exit 5, "no save points for this folder yet").
- Retention: see `src/retention.rs` (`window_days`, `thin`, `thin_if_due` at SessionStart at most once per 24 h, `gc`, 1-day grace, 2 GiB cap sets `meta.thinned_harder_at`; status shows the 2 GB note for 14 days). GC marks from every non-dropped point plus the newest point; a tree that cannot be read stops the run.
- Restore notice: `restore_notice` (hook.rs) shows ONLY the newest unseen restore (`restores.status IN ('done','partial')`), once per session. New sessions start as having seen all earlier restores (`turn::touch_session`). If a restore is `applying`, UserPromptSubmit emits `MSG_RESTORE_INTERRUPTED`. S1b's report may need to list several restores.
- Protected paths: PreToolUse denies edit-family writes into TORPOR_HOME or the key file (both the kernel-resolved path with symlinks/`..`/dangling links followed, and the text-folded path). Known limits: a hardlink to the key file is not covered (that is the Bash rule, S1b); the deny is skipped while the off switch is on.
- `status` (src/status.rs): problem sources 1-3 and 5 (faults shown once, disk paused, no heartbeat / live Claude session without hooks / silent session, lost key file, deep-check list). **Source #4 (settings files: `disableAllHooks`, `allowManagedHooksOnly`, `strictPluginOnlyCustomization`, `enabledPlugins["torpor@kylne700"]==false`) is S1b's.** Reads `<claude_dir>/sessions/*.json` only (never `*.key`; a FIFO named `.key` is in the test). The silent-session rule flags only sessions whose last hook was Stop/StopFailure and whose transcript is more than 120 s newer than the last heartbeat. The "hint" line is not printed.
- Scan: `ignore::WalkBuilder`; hard excludes incl. names ending `-wal`, `-shm`, `-journal`, `.torpor-*.tmp`; precious (`.env`, `.env.*`, `*.db`, `*.sqlite*`, `*.sql`) captured even when gitignored. `.torporignore` exists, but the opt-out for precious files is NOT implemented. Mask scanners are hand-written with ASCII-only case folding (`regex` is a dev-dependency used as a differential oracle).
- Texts: newer store at SessionStart: "Torpor: This project's records were written by a newer Torpor. Update the plugin." (CLI exit 3). The refused-root message says "whole home folder" also for `/`. A project root that is inside the store is refused silently.

## 6. Deviations from the spec (reason in brackets)

1. Databases are raw-copied, never opened or backed up with the SQLite API (P10: opening a WAL db writes next to it, violates A6).
2. P2 continuation rule: a `<task-notification>` prompt with the same `prompt_id` reopens the turn (spec 4.1 had no such case).
3. "Shared before shell" ordering: during an overlap the main agent's own shell edits are listed as `shared` (attribution cannot be split in slow mode).
4. A path that is transiently `x:` in one scan and normal in the next is classed `outside` (no better evidence).
5. `meta.consumed_event_id` watermark and ns timestamps (spec leaves both open). Tool points invisible. Carry-forward rule for paths the budget did not reach. Timing scan (walk + stat, parallel when the file count is unknown, re-scan once if > 40 ms) chooses slow/fast mode.
6. Low disk: turn row with NULL `start_point`; the next UserPromptSubmit writes End + Start points.
7. Transcript reader skips ahead to the last 4 MiB. The Stop line's "N files not saved" counts only NEW `x` entries and counts per path net (including mode changes).
8. `Ws::open_in(home, root, create)` added (spec has only `open`). Marketplace `source "./"` accepted by `claude plugin validate`; real install resolution NOT VERIFIED.
9. Scan thread overlaps `load_cache`; WAL kept between hooks with `Ws::drop` truncation (needed for the p50 budget).
10. The harness's `check_contract` accepts PreToolUse `permissionDecision` of `deny` or `ask` (never `allow`; the reason must start with "Torpor"). **S2 must relax it** for PostToolUse `additionalContext` and the Stop `decision`.

## 7. Open problems and NOT VERIFIED

- UserPromptSubmit p50 36.7 ms vs the 36 ms budget (noise-limited; see section 4 for the designed fix).
- First events after a project is generated can reach 141 ms (racy window re-hash); inside the 150 ms p99 budget, not a regression risk.
- A long first capture holds an IMMEDIATE transaction; a parallel edit-family PostToolUse can then hit `busy_timeout` (1 s) and lose its event. It fails safe: the next full scan still saves the file.
- Fast mode cannot attribute a shell edit made by another session during a turn; it is classed by the scan (`shell`/`outside`). Accepted limit.
- Interactive-mode behaviour (P5 for interactive sessions, P7 native /rewind marker) NOT VERIFIED. musl build NOT VERIFIED. Marketplace install path NOT VERIFIED. Windows/macOS not built.
- `.torporignore` opt-out for precious files not implemented. Hardlinks to the key file not covered.
- The restore path itself (everything in spec 6 and 7) has no code, so no A2/A3/A7 (restore half)/A8/A9/A10 coverage yet.

## 8. Public API S1b needs (all `pub`, crate `torpor`)

- `store`: `Ws::open(root, create)`, `Ws::open_in(home, root, create)` (fields `root`, `dir`, `id`, `home`, `conn`), `lock_shared/lock_exclusive(wait_ms)`, `objects_dir`, `loose_path`, `read_blob(db,&id)`, `read_secret(db,&id,key)`, `blob_exists`, `insert_blob`, `prepare_plain/prepare_secret`, `write_atomic(target, bytes, mode)` (temp file + rename, explicit mode), `load_tree`, `encode_tree/decode_tree`, `TreeEntry`, `last_point`, `PointRow`, `meta_get/meta_get_i64/meta_set`, `record_fault/fault_tx`, `check_disk`, `Key::load/create/load_or_create`, `resolve_root`, `home_dir/claude_dir/key_path/user_home`.
- `capture`: `capture(ws, db, &Req)` (`Req { session, points: Vec<PointSpec{kind,turn_id,note}>, turn, only_if_changed, budget }` -> `Captured { point_ids, tree_id, changed, n_changes, partial, not_saved, scan_ms, file_count }`), `capture_named(ws, db, &[rel])`, `flatten_tree(ws, db, &tree) -> Vec<(path, TreeEntry)>`, `lookup_path`, `PointKind`, `x_leaf`, `id_text`. The pre-restore and result points are made with `capture(...)` and `PointKind::PreRestore` / `PointKind::Restored` (both kinds already exist in the schema, GC and status).
- `turn`: `create/close/reopen/find/open_of/by_id/set_origin/net_counts/add_event/heartbeat/touch_session`.
- `retention`: `window_days/thin/thin_if_due/gc/store_bytes`, `GcOutcome`, `GcStats`.
- `status`: `gather/render/to_json`. `cli`: dispatch table at the top of `src/cli.rs` (hook, status, gc, quiet, version, help); exit codes 0 done, 1 error, 2 usage, 3 refused, 4 partly done, 5 not found.
- Test harness (`tests/common/mod.rs`): `Sim` (session_start, prompt, edit, write_tool, bash, subagent_*, outside_edit/outside_remove, stop, session_end, second_session, cli, status, conn, verify_point, verify_last_point, faults, observer hook for inotify), `Env`, `oracle/snapshot/restore_snapshot`, `check_contract`, `write_files`.

## 9. S1b next steps, in order

1. Read spec 6, 7, 8, 9.4, 10 and REV on the same topics. Restore protocol: insert a `restores` row (`planned` -> `applying`), write a journal under `<ws>/journal/`, take `lock_exclusive`, make a `PreRestore` point first, write files with `store::write_atomic` (secrets via `read_secret`; skip `x` entries and report them), update `stat_cache` rows for restored files (an optimisation only: a stale row has the old mtime/ctime/ino, so the next scan re-hashes), make the `Restored` point, set `status` `done`/`partial`. `recover` replays or rolls back from the journal.
2. Add the commands of spec 8 to `src/cli.rs` (back, undo, recover, log, show, init, statusline, off/on, forget, retain; the user-only ones refuse when `CLAUDECODE` is set) and status source #4.
3. Destructive-command save points and ask/deny in `PreToolUse` (`hook.rs`, `Bash` branch; spec 7). The matcher in `hooks/hooks.json` already covers `Bash`.
4. Fault injection `TORPOR_FAULT=restore:<k>|capture:<k>` (SIGKILL at step k) behind `fault-inject`; then A3 (100 runs), A2 (proptest, needs `proptest` as dev-dependency), A7 restore half (`rm .env`, `back 1`: byte-exact, mode 0600), A8, A9, A10 (the single allowed real `claude -p` run; haiku, <= 0.50 USD).
5. README (with the section 29 declaration: "Oct 26, 12:00 PM ET") and an icon (plugin-directory validation wants both); CI matrix incl. musl (needs `musl-tools`), `cargo test` + `cargo test --release` + `tests/release_ignores_fault_env.sh` + `cargo build --release` size/latency check; keep A5 as a manual `--ignored` run.
6. Run the full suite again (`cargo test` and `cargo test --release`), then hand the repo to the lead for creating Kylne700/torpor.

Hard rules that stayed in force for S1a (the lead's brief repeats them): never run `claude plugin install` / `marketplace add`, never write `~/.claude`, the real `~/.torpor` or anything inside a `.git` folder, never read other transcripts or `~/.claude/sessions/*.key`, never open secrets, no network writes, no sudo/apt. Commit messages end with the Co-Authored-By and Claude-Session lines given in the brief.

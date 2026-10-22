<p align="center"><img src="assets/icon.svg" width="96" height="96" alt="Torpor icon: an undo arrow around a dot"></p>

# Torpor

Undo for Claude Code. Torpor saves your project folder at the start and at the end of every turn, shell commands included. When Claude breaks something, you can undo that one turn, find the turn that broke it, or put the whole folder back to an earlier state. When you tell Claude that something works, Torpor keeps that as a check and tells you when a later turn breaks it.

Torpor is built by an AI coding agent team (Claude Code) working for its author, Kylne700.

## Pre-event declaration

Torpor's engine (save points, undo with checks, find-break, promise checks and page checks in a browser) is written from 2026-10-21 to 2026-10-25, before the Meta hackathon starts on Oct 26, 12:00 PM GMT-5 (17:00 UTC). It is open source (MIT OR Apache-2.0) and makes no AI model calls. Save points, undo with checks, recovery, find-break, promise checks and page checks are in this version now. The last commit before the event will be tagged `pre-event-base`. Everything that uses Meta AI will be built during the event, after it starts. Without those parts, your own coding agent writes the promise checks, and Torpor itself calls no model.

## What it does today

- **Save points at both ends of every turn.** Torpor saves the project when you send a prompt and again when Claude stops. After each shell command Claude runs, it also checks what changed. So `sed -i`, `rm`, a formatter or a build step is recorded, not just the files Claude edits with its own edit tools. Changes you make yourself between turns are recorded too, and marked as made outside the chat.
- **Go back.** `torpor back` puts the whole folder back to an earlier state. The state you left is saved, so going back can be undone too.
- **Undo one turn.** `torpor undo 7` takes out what turn 7 changed and keeps the work done after it. Text files are merged line by line. If your later work touches the same lines, Torpor lists the files and changes nothing. Before it writes, it builds the result in a scratch copy and runs the promises that cover those files. If one passes now and would fail after the undo, Torpor changes nothing and says which. `torpor undo 7 --force` goes on anyway (type it in your own terminal; it refuses to run inside Claude Code).
- **Find the turn that broke it.** Tell Claude that something that worked is broken, then run `torpor find-break`. It lists what changed since the last version that worked. With a check (your AI writes it), Torpor runs the check on older versions and names the turn that made it fail, also when a shell command made the change. A change you made yourself between turns is named too. `torpor undo` then takes out that one turn.
- **Keep what you said works.** Tell Claude "login works now" and your AI writes a small check for exactly that. Torpor tests the check before it keeps it, and runs it after later turns. When a later turn breaks it, you see a note at the end of that turn. A promise you can trust may also stop the AI (see Promises below).
- **Page checks.** A check can click and type in a real page, not just call the app's HTTP routes. Torpor runs it in a headless Chromium-family browser that is already on your computer. The browser reaches pages on 127.0.0.1 and nothing else. This needs a small runner that you install yourself (see Install).
- **Recover.** If a restore is cut short (power loss, a closed terminal), `torpor recover` finishes it. Restores are journaled, so a cut-short one can be finished.
- **A save point before risky commands.** Before commands such as `rm`, `mv`, `truncate`, `sqlite3`, `git reset`, `git clean` and `git checkout`, Torpor saves the folder once more.
- **A question before commands that reach outside.** No save point can bring back data on another machine. For commands such as `terraform destroy`, `kubectl delete`, `DROP TABLE` or `git push --force`, Claude Code asks you before the command runs (in the default, acceptEdits and plan permission modes). In any other mode (bypassPermissions, dontAsk, auto), Torpor blocks the command instead and says why.
- **A line when an edit touches a promise.** When Claude edits a file that a promise depends on, Torpor tells the AI in one short line, so it can run the check before it finishes.
- **Memory after compaction.** When Claude Code compacts a long chat, Torpor tells the AI again what you asked it to keep and which promises exist. It uses your own words as Torpor stored them, never tool output.
- **A notice after a rewind.** If you rewind a chat in Claude Code and some files that a shell command or a subagent changed stay as they were, the next prompt gets one line. It names them and gives the `torpor back` command that puts everything back.
- **One line after each turn:**

```
Torpor: save point 7 · turn 1 changed 3 files (2 edits, 1 shell) · undo it: torpor undo 1
```

## Install

In Claude Code:

```
/plugin marketplace add Kylne700/torpor
/plugin install torpor@kylne700
```

Or in a terminal:

```
claude plugin marketplace add Kylne700/torpor && claude plugin install torpor@kylne700 --scope user
```

Then restart Claude Code, or run `/reload-plugins`. When a session starts, the plugin copies its program to `~/.torpor/bin/torpor`, so you can use it from a terminal too.

To uninstall, run `/plugin uninstall torpor@kylne700`. Your save points stay in `~/.torpor`, and reinstalling finds them again. To delete them, run `~/.torpor/bin/torpor forget --all` in a terminal, or delete the folders `~/.torpor` and `~/.config/torpor`.

### Optional: a status line

Add this to your Claude Code settings. Torpor never edits your settings for you.

```json
"statusLine": {"type": "command", "command": "~/.torpor/bin/torpor statusline"}
```

It shows `torpor · 5 save points · saved just now`. If you already have a status line, put your old command after `--`:

```json
"statusLine": {"type": "command", "command": "~/.torpor/bin/torpor statusline -- <your old command>"}
```

Torpor runs the old command through `sh -c` and waits up to 2 seconds for it. If the old command uses pipes or quotes, put it in a script file and pass the script.

### Optional: page checks (a browser)

A check with page steps needs a small runner. Install it in your own terminal:

```
~/.torpor/bin/torpor runner install
```

It needs Node.js 20 or newer. It tells you what it will download and asks you to confirm (`[y/N]`; `--yes` answers for you). It installs playwright-core 1.64.1 from the npm registry into `~/.torpor/runner/1.64.1/`. The package is checked against its published hash before it is unpacked, and no npm and no install script runs. If Torpor finds no Chromium, Chrome, Brave or Edge on this computer, it also downloads a Chromium (about 150 MB) from Playwright's servers. That download has not been tested (NOT VERIFIED).

Without the runner, a page check is reported as unrunnable (it could not run). It never blocks anything.

## Commands

Inside Claude Code the plugin puts `torpor` on the PATH of the Bash tool, so Claude can run these for you. In your own terminal, use `~/.torpor/bin/torpor` (or add `~/.torpor/bin` to your PATH).

| Command | What it does |
|---|---|
| `torpor status` | Is Torpor saving this folder? Shows problems and files it could not save (`--all` lists every one). |
| `torpor log` | The turns, newest at the top, and what each one changed (`-n 20` sets how many, `--json` for scripts). |
| `torpor show 7` | The files of turn 7, with lines added and removed. `torpor show --id 12` shows one save point. |
| `torpor back 1` | Put the whole folder back one step. `--turn K` goes to the end of turn K, `--id P` to save point P. |
| `torpor undo 7` | Undo turn 7 alone and keep the work after it. The promises that cover those files run on the result before anything is written. |
| `torpor recover` | Finish a restore that was cut short. |
| `torpor init` | Report what the initial scan of this folder found: file counts, secret files, files skipped. |
| `torpor gc` | Free the space used by old save points. |
| `torpor keep "login works"` | Save a version you want to come back to. `--check -` also saves a promise. |
| `torpor find-break` | Which turn broke it. `--check -` runs a check (the AI writes it) on older versions. |
| `torpor promise list` | The promises and how far each one is proven. `torpor promise show 2` shows one. |
| `torpor check 2` / `--all` | Run checks against the folder as it is now. |
| `torpor promise add -` | Save a check read from standard input. Your AI runs this. |
| `torpor promise dispute 2 "why"` | Say a promise is wrong. It turns advisory until you decide. |
| `torpor quiet on` / `off` | Hide or show the line printed after each turn. |
| `torpor statusline` | One line for Claude Code's status bar (see above). |
| `torpor version`, `torpor help` | The version, and the list of commands. |

`back` and `undo` take `--dry-run` to show what they would do without touching anything (an undo dry run names the checks but does not run them). `undo` also takes `--force`, which skips the checks (type it in your own terminal; it refuses to run inside Claude Code). `back --with-data` also puts database files back (see the limits below).

These change what Torpor keeps, whether it can stop the AI, or what it downloads, so they refuse to run inside Claude Code and must be typed in your own terminal:

| Command | What it does |
|---|---|
| `torpor off` / `torpor on` | Stop or resume saving this folder. |
| `torpor promise ok 2` | Reject a dispute: the promise stays. |
| `torpor promise drop 2` | Remove a promise. |
| `torpor promise block 2` | Let a promise that is in shadow stop the AI now, without waiting. It refuses for any other promise. |
| `torpor gate on` / `off` | Let promises stop the AI, or not. |
| `torpor promise show 2 --spec` | Print the stored check itself. |
| `torpor runner install` | Install the runner that page checks need. It says what it downloads and asks you to confirm (`--yes` answers for you). |
| `torpor retain 90` | Keep save points for 90 days, even if Claude Code deletes old chats sooner. |
| `torpor forget` | Delete the records of this folder. `--all` deletes every folder's records, and `--before 2026-10-01` deletes older ones. Each asks you to type a confirmation. |

Exit codes: 0 done, 1 error, 2 wrong command line, 3 refused (also a check that Torpor rejects), 4 partly done (for `find-break`: it could not decide), 5 turn, save point or promise not found. `torpor check` exits 1 when a check does not pass.

## Promises and the gate

A promise is a small check. It checks that one thing works, for example "sign-in is accepted, then the account data shows". Your AI writes the check, because Torpor makes no AI model calls. Torpor rejects a check it does not understand, and it never repairs one.

**How Torpor trusts a check.** Torpor runs a new check 5 times on the version you called good. All 5 must pass. Then it looks for an earlier version where the check fails twice in a row. If it finds one, the promise is proven: the check passes on the working version and fails on the broken one. If it finds none, the check is kept as a guard, not proven, and Torpor says so.

**Three levels.**

- **Advisory.** Every new promise starts here. When it fails at the end of a turn, you see a note and the AI is told once. It never stops the AI.
- **In shadow.** A proven promise starts here when it came from your own words, when the words in its check match yours, and when the check also fails on a copy of the working version with part of the fix undone. Torpor says when it would have stopped the AI, and does not stop it.
- **Blocking.** A promise in shadow starts to stop the AI after one real catch (it would have stopped the AI, and a later version fixed it), or after it passed on three different days (UTC) with no false alarm. `torpor promise block 2` skips the wait.

A promise stays advisory when the AI thought of it by itself, when Torpor cannot match its check to your words (praise written entirely in Hebrew, or a check that looks for a long text you did not put in quotes), when no broken copy of the fix makes it fail, and when it is flaky. A check that Torpor has seen pass and fail on the same version is flaky. A flaky promise stays advisory and never stops the AI. A check that says it cannot verify a flow (one that needs OAuth, SMS, a CAPTCHA or a payment) is unverifiable, and such a check never blocks.

**When it checks.** At the end of a turn, Torpor runs a promise when the turn changed a file the promise depends on, when its last result was not a pass, or when it has not run for two turns. For Node apps, Torpor records which files the app ran and uses that list. For other apps, any changed file counts, except docs and tests. A turn that changed just docs and tests starts no check.

**When it stops the AI.** A blocking promise whose check fails 3 times in a row on fresh copies, while it still passes on the last version that worked, stops the AI. Torpor tells it to fix the app, not the check. It stops the AI at most twice for one request of yours, and a second stop needs a change by the AI in between. After that the turn ends and you are told that the promise still fails. If Torpor fails or runs out of time, the turn ends as if Torpor were not there. A check that was cut off counts as nothing, never as a failure.

**If a promise is wrong,** `torpor promise dispute 2 "why"` (your AI can run it) turns it advisory until you decide. In your own terminal, `torpor promise ok 2` keeps it and `torpor promise drop 2` removes it. `torpor gate off` stops all stopping.

## How it works

- Torpor is one program written in Rust, plus a Claude Code plugin whose hooks call it. It keeps no service running in the background. It works when Claude Code fires a hook or when you type a command.
- Promise checks run in a scratch copy of your project, one at a time. Torpor starts your app there on a free port of this computer, sends plain HTTP requests to 127.0.0.1, and removes the copy afterwards. For Node apps it also records which files the app ran, so a turn that touches none of them can skip the check. A skipped check runs again within three turns.
- A check with page steps runs in a headless Chromium that a small Node program (the runner) drives. In that browser, every address except 127.0.0.1 and localhost is blocked.
- Torpor picks the port for the app just before it starts the app. If the app ends within seconds (the port was taken meanwhile), or a process that is not the app answers on that port, it starts the app once more on another port. On Linux it reads which process holds the port. A process that is not the app is never stopped. If the port is still not the app's, the check is reported as could not run.
- `torpor undo` runs the checks on its result inside a scratch copy, up to 90 seconds in all, while it holds the project's lock so that nothing changes meanwhile. A promise that passes now and fails on the result stops the undo (exit code 3). A check that is flaky, cannot run, runs out of time or already fails now never stops it: the report says so. When no promise covers the files, the undo goes ahead and says the result is untested.
- Save points are kept in `~/.torpor/<project id>/`. Files are stored by content, so a file that did not change costs nothing, and the rest is compressed. A small SQLite database holds the history.
- Files that look like secrets (`.env` files, `*.pem`, `*.key`, `.npmrc`, `credentials*`, `*.tfstate*` and similar) are stored encrypted with a key in `~/.config/torpor/key`. Torpor never prints their content. `torpor show` says "secret file, content not shown".
- Torpor never writes inside your project's `.git` folder, and never edits Claude Code's settings.
- If Torpor itself hits a problem, your session carries on and `torpor status` tells you what went wrong.

### What gets saved

The project folder Claude Code was opened in. Left out: `.git`, `node_modules`, build and cache folders (`dist`, `build`, `.next`, `.cache`, `__pycache__`, `target` next to a `Cargo.toml`, Python virtual environments and similar), and whatever your `.gitignore` and `.torporignore` files list.

`.env` files, database files (`*.db`, `*.sqlite*`) and `*.sql` files are saved even when `.gitignore` lists them, because losing them hurts most. To leave one out anyway, put its name in a `.torporignore` at the top of the project (same rules as `.gitignore`). A `.torporignore` in a sub-folder cannot do this for those files.

Size limits: a file over 2 MB is not saved (database and SQL files up to 64 MB are; secret files over 1 MB are not). One save point adds at most 100 MB of new data. Files left out for size show up under "Not saved" in `torpor status`.

### How long save points last

Torpor follows Claude Code's own cleanup setting (30 days unless you changed it). `torpor retain DAYS` keeps them longer. After two days, Torpor keeps the last save point of each day. If one project's records pass 2 GB, Torpor thins harder: a day in full detail and seven days in total.

## Limits

- **Torpor cannot undo what happened outside your folder.** Deleted databases, deploys, sent mail, pushed commits and cloud resources are out of its reach. It asks or blocks for some well-known commands, but it cannot see every way to cause damage.
- **The command reader is simple.** It looks at the text of each shell command. It does not look into aliases, shell functions, `eval`, scripts you call by name, or `$(...)` inside double quotes. A command it does not recognise gets no extra save point, but the start of the turn is already saved, so `torpor undo` can still take that turn out.
- **Databases are copied as plain files.** A database that is in use (its `-wal` file is not empty) is not saved. `torpor back` leaves database files alone unless you pass `--with-data`, and `torpor undo` never rewinds them. If a restored folder has a different schema, run your migrations.
- **Two sessions in one folder** can edit the same file. Torpor marks such changes as shared and `torpor undo` leaves them alone.
- **Empty folders** that were removed are not brought back by `undo`, unless a saved file lived in them.
- **The checks of an undo cover part of the result.** The scratch copy takes the changed files, not links, permission changes, secret files, `.env` files, dependency folders or files over 16 MB; the report counts them. Torpor does not run your build command on the result. An undo after which the app no longer starts is reported as "could not run" and goes ahead.
- **Port ownership is checked on Linux.** On macOS the answer on the port is trusted. A server that a helper outside the app's process group holds the port for (a daemon that left its group, a container proxy that runs as you) looks like another process, and the check is then reported as could not run.
- **Files outside the project folder are not covered.**
- **A promise's start command runs on your computer.** At each check, Torpor runs the command written in the check (for example `node server.js`) in a scratch copy of your project, with HOME set to a temporary folder and your `.env` files left out. Treat a check like a test script you approved. Torpor rejects a check whose environment values or step URLs point to a host other than this computer. It does not limit the start command or the app: they can use the network and files outside the scratch copy. The browser of a page check cannot reach another host, but the app it talks to still can.
- **Network use.** Torpor sends HTTP to 127.0.0.1. It makes one download, `torpor runner install`, after you confirm: playwright-core from the npm registry, and a Chromium from Playwright's servers when none is found on this computer. The Chromium download is NOT VERIFIED: it has never been run. The app and its start command may use the network.
- **If Torpor is killed during a check, the app it started may keep running.** Torpor does not look for it on a later run.
- **Find-break assumes that once a check breaks, it stays broken.** If a turn in between made the app work again, the answer can be wrong, or Torpor says it could not decide.
- **Page checks need the runner and a browser.** They need `torpor runner install`, Node.js 20 or newer, and a Chromium-family browser (chromium, google-chrome, brave-browser, microsoft-edge, or an app in /Applications on macOS). They run headless against pages of the app on 127.0.0.1, and a whole check gets 30 seconds. A page that sends the browser to another host makes the check unrunnable. The real browser run has been tried with Chromium on Linux. Other browsers and macOS are not tested.
- **A passing check does not prove the app is right.** It says that one thing worked when it ran.
- **Torpor cannot know what a check was meant to test.** It checks that the check uses words from yours and that it fails when part of the fix is undone. A check that fails at random can pass its proof by luck, and Torpor calls a check flaky (advisory, never stops the AI) when it has seen it pass and fail on the same version, not before. To stop the AI, a check must fail 3 fresh runs in a row while it still passes on the last version that worked. That makes a false stop rare, not impossible.
- **Slow apps cannot stop the AI.** At the end of a turn Torpor has 30 seconds, and its checks get up to 20 of them. An app that starts or answers slower than that has its check cut off, and the turn ends.
- **The AI that wrote a check knows what is in it.** Torpor hides the stored check from the AI. This version runs in mode B (your own coding agent writes the checks), and in mode B, the agent that wrote a check knows its source for that session, so hiding it protects only later sessions.
- **The record of which files an app ran works for Node apps.** For other apps, a promise runs after every turn that changes a file other than docs and tests.
- **Built for Claude Code.** Other tools are not supported.
- **Windows is not supported yet.** Torpor is built and tested on Linux. The macOS and static Linux builds are made by the project's CI and have not been run by hand. Behavior in a normal interactive Claude Code session (the permission prompt for commands that reach outside) has not been checked by hand yet.

## Privacy

Torpor stores its records on this computer and never uploads them. Your AI agent still sends your prompts and files to its provider, as before.

Torpor makes no AI model calls. Its network use is plain HTTP to 127.0.0.1 (this computer), to run your promise checks against your app in a scratch copy. In a page check, the browser reaches this computer and nothing else. Torpor makes one download, `torpor runner install`, after you confirm. It fetches public files (the playwright-core package, and a Chromium when none is found) and uploads nothing of your project. The command wrapped by `torpor statusline --` is your own command, and it does what it did before.

## License

MIT OR Apache-2.0, at your option. See `LICENSE-MIT` and `LICENSE-APACHE`.

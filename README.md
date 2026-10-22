<p align="center"><img src="assets/icon.svg" width="96" height="96" alt="Torpor icon: an undo arrow around a dot"></p>

# Torpor

Save points for Claude Code. Torpor saves your project folder at the start and at the end of every turn, shell commands included. When Claude breaks something, you can undo that one turn, or put the whole folder back to an earlier state.

Torpor is built by an AI coding agent team (Claude Code) working for its author, Kylne700.

## Pre-event declaration

Torpor's engine is written from 2026-10-21 to 2026-10-25, before the Meta hackathon starts (Oct 26, 12:00 PM ET). It is open source (MIT OR Apache-2.0) and makes no AI model calls. It covers save points, undo and recovery now, and find-break and promise checks by 2026-10-25. The last commit before the event will be tagged `pre-event-base`. Everything that uses Meta AI will be built during the event, after it starts.

## What it does today

- **Save points at both ends of every turn.** Torpor saves the project when you send a prompt and again when Claude stops. After each shell command Claude runs, it also checks what changed. So `sed -i`, `rm`, a formatter or a build step is recorded, not just the files Claude edits with its own edit tools. Changes you make yourself between turns are recorded too, and marked as made outside the chat.
- **Go back.** `torpor back` puts the whole folder back to an earlier state. The state you left is saved, so going back can be undone too.
- **Undo one turn.** `torpor undo 7` takes out what turn 7 changed and keeps the work done after it. Text files are merged line by line. If your later work touches the same lines, Torpor lists the files and changes nothing.
- **Recover.** If a restore is cut short (power loss, a closed terminal), `torpor recover` finishes it. Restores are journaled, so a cut-short one can be finished.
- **A save point before risky commands.** Before commands such as `rm`, `mv`, `truncate`, `sqlite3`, `git reset`, `git clean` and `git checkout`, Torpor saves the folder once more.
- **A question before commands that reach outside.** No save point can bring back data on another machine. For commands such as `terraform destroy`, `kubectl delete`, `DROP TABLE` or `git push --force`, Claude Code asks you before the command runs. In a session that skips permission prompts (bypassPermissions mode), Torpor blocks the command instead and says why.
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

## Commands

Inside Claude Code the plugin puts `torpor` on the PATH of the Bash tool, so Claude can run these for you. In your own terminal, use `~/.torpor/bin/torpor` (or add `~/.torpor/bin` to your PATH).

| Command | What it does |
|---|---|
| `torpor status` | Is Torpor saving this folder? Shows problems and files it could not save (`--all` lists every one). |
| `torpor log` | The turns, newest at the top, and what each one changed (`-n 20` sets how many, `--json` for scripts). |
| `torpor show 7` | The files of turn 7, with lines added and removed. `torpor show --id 12` shows one save point. |
| `torpor back 1` | Put the whole folder back one step. `--turn K` goes to the end of turn K, `--id P` to save point P. |
| `torpor undo 7` | Undo turn 7 alone and keep the work after it. |
| `torpor recover` | Finish a restore that was cut short. |
| `torpor init` | Report what the initial scan of this folder found: file counts, secret files, files skipped. |
| `torpor gc` | Free the space used by old save points. |
| `torpor quiet on` / `off` | Hide or show the line printed after each turn. |
| `torpor statusline` | One line for Claude Code's status bar (see above). |
| `torpor version`, `torpor help` | The version, and the list of commands. |

`back` and `undo` take `--dry-run` to show what they would do without touching anything. `back --with-data` also puts database files back (see the limits below).

These four change what Torpor keeps, so they refuse to run inside Claude Code and must be typed in your own terminal:

| Command | What it does |
|---|---|
| `torpor off` / `torpor on` | Stop or resume saving this folder. |
| `torpor retain 90` | Keep save points for 90 days, even if Claude Code deletes old chats sooner. |
| `torpor forget` | Delete the records of this folder. `--all` deletes every folder's records, and `--before 2026-10-01` deletes older ones. Each asks you to type a confirmation. |

Exit codes: 0 done, 1 error, 2 wrong command line, 3 refused, 4 partly done, 5 turn or save point not found.

## How it works

- Torpor is one program written in Rust, plus a Claude Code plugin whose hooks call it. It keeps no service running in the background. It works when Claude Code fires a hook or when you type a command.
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
- **Files outside the project folder are not covered.**
- **Built for Claude Code.** Other tools are not supported.
- **Windows is not supported yet.** Torpor is built and tested on Linux. The macOS and static Linux builds are made by the project's CI and have not been run by hand. Behavior in a normal interactive Claude Code session (the permission prompt for commands that reach outside) has not been checked by hand yet.

## Privacy

Torpor stores its records on this computer and never uploads them. Your AI agent still sends your prompts and files to its provider, as before.

Torpor has no network code and makes no AI model calls. The command wrapped by `torpor statusline --` is your own command, and it does what it did before.

## License

MIT OR Apache-2.0, at your option. See `LICENSE-MIT` and `LICENSE-APACHE`.

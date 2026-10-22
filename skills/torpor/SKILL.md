---
name: torpor
description: Use when the user says something works now, or that something which worked is broken, or asks to undo or find what broke. Torpor save points, promises and checks.
---

# Torpor: promises and find-break

Torpor saves the project after every turn. A promise is a small check. It checks that one thing works. Torpor makes no model calls: you write the checks, Torpor lints them and runs them in a scratch copy of the project. It has no keyword lists: you judge the user's words.

## The user says something works now
Write a check for exactly that thing and save it. Use the Bash tool with timeout 600000, because the proof runs the check 7 times:

    torpor promise add - <<'EOF'
    {"v":1,"name":"login works","why":"the user said login works now", ...}
    EOF

"Promise #N saved. Proof: passed 5 of 5 ... failed 2 of 2" means done. "Not saved: <reason>" says what is wrong with the check: fix the check.

## The user says something that worked is broken
1. Run `torpor find-break`. With no check it lists the files that changed since the last version that worked.
2. Write a check that passes on the working version and fails now. Then run it (timeout 600000):

       torpor find-break --check - <<'EOF'
       {...}
       EOF

   It names the turn ("Turn 4 broke it ...") and saves the check as a promise. A change made outside the chat is named too, with a `torpor back --id N` to go before it. "One of turns 2-3 broke it" means Torpor could not separate them.
3. Tell the user which turn broke it and propose `torpor undo <turn>`. It undoes only that turn and keeps later work. Run it only when the user agrees.

## Rules
- Never edit Torpor files (its store, hooks or settings). Never weaken a check to make it pass.
- A promise that looks wrong: run `torpor promise dispute <id> "<why>"` and tell the user. Only the user decides (`torpor promise ok <id>` or `drop <id>`).
- One check = one thing the user can name. Keep it short. The check and every assertion need a `why`.
- A flow that needs OAuth, SMS, a CAPTCHA or a payment: set `"unverifiable":true`. Such a check never blocks.

## The check (JSON; Torpor rejects anything it does not know and never repairs it)
```
{"v":1,"name":"login works","why":"the user said login works now",
 "start":{"cmd":"node server.js --port {port}","ready":"/login","timeout_s":30},
 "env":{"DATABASE_URL":"file:./dev.db"},
 "steps":[
  {"http":{"method":"POST","path":"/api/login","json":{"email":"u{rand}@example.test"}},"expect":{"status":200},"why":"sign-in is accepted"},
  {"http":{"path":"/api/me"},"expect":{"status":200,"json_has":["email"]},"why":"the account data shows"}]}
```
- Top level: `v` (1), `name` (60 characters at most), `why` (200), `start`, `env`, `steps` (1 to 40), `unverifiable` (true or false).
- `start.cmd` runs with `sh -c` in the copy. `{port}` is a free port. `ready` is a path that must answer 2xx within `timeout_s` (90 at most).
- `env`: plain text values. Not allowed: HOME, TMPDIR, PORT, PATH, secrets, or a host that is not 127.0.0.1 or localhost.
- `http`: `method` (GET by default, or POST, PUT, PATCH, DELETE, HEAD), `path` starting with `/`, optional `json` body (not on GET).
- `expect` holds one or more of: `status`, `status_in` [..], `json_has` ["email","user.id","items[0]"], `json_type` {"email":"string"} (string, number, boolean, array, object, null), `body_contains` "text", `length_range` [min, max] in bytes.
- `{rand}` in a text becomes 8 random letters or digits, the same within one run. Use it for new emails and names.
- Rejected: absolute URLs, expected values that look like ids, dates, UUIDs or numbers of 5 or more digits, JS, CSS selectors, timed waits.
- Page steps run in a headless browser and need the runner. Steps: `{"goto":"/path"}`, `{"click":LOC}`, `{"fill":LOC,"value":"text"}`, `{"press":LOC,"key":"Enter"}`, `{"select":LOC,"value":"x"}`, `{"check":LOC}`, `{"uncheck":LOC}`, `{"wait_http":"/path"}`. Assertions, each with a `why`: `{"expect_visible":LOC}`, `{"expect_hidden":LOC}`, `{"expect_text":LOC,"contains":"text"}`, `{"expect_url":{"path":"/home"}}` (or `path_re`), `{"expect_count":LOC,"n":3}` (or `min` and `max`), `{"expect_console_clean":true}`.
- LOC is one of `{"role":"button","name":"Sign in"}`, `{"label":"Email"}`, `{"text":"Welcome"}`, `{"text_re":"Welcome.*"}`, `{"testid":"save"}`. The browser reaches 127.0.0.1 only. A page step and an http step can be in one check. Example: `{"goto":"/account"}`, `{"fill":{"label":"Email"},"value":"u{rand}@example.test"}`, `{"click":{"role":"button","name":"Sign in"}}`, `{"expect_text":{"role":"status"},"contains":"Signed in","why":"the page says the user is signed in"}`.
- A check with page steps that comes back "runner not installed: torpor runner install" means the runner is missing. You cannot install it: it downloads files, so only the user can run `torpor runner install` in their own terminal. Tell the user. Until then, test the same thing through the app's http routes.

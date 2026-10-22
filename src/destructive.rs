//! Destructive shell commands (spec section 7). A small reader for shell text, not a shell: it cuts a command
//! into the commands it holds, drops wrappers (sudo, env, npx ...) and looks at the words that are left.
//! Nothing here runs anything. The only file it reads is package.json, for `npm run <name>`.
//!
//! Known limits (no full shell parser): `$(...)` inside double quotes, `eval`, aliases, functions, variables used
//! as commands and scripts run by name (`./clean.sh`) are not looked into.
use crate::store::{home_dir, key_path, user_home};
use std::path::{Path, PathBuf};

/// Only this much of a command is read.
const MAX_TEXT: usize = 1 << 20;
/// Shell inside shell, or a package script inside a shell: stop after this many levels.
const MAX_DEPTH: u8 = 2;

/// Commands that only the user may run, in their own terminal (the CLI refuses them under `CLAUDECODE` too).
pub const USER_ONLY: [&str; 4] = ["off", "on", "forget", "retain"];

const WRITERS: [&str; 6] = ["rm", "mv", "cp", "sqlite3", "truncate", "chmod"];
const GIT_LOCAL: [&str; 13] = ["clean", "reset", "checkout", "restore", "stash", "switch", "rebase", "merge", "pull", "cherry-pick", "revert", "am", "apply"];
const SHELLS: [&str; 6] = ["bash", "sh", "zsh", "dash", "ksh", "ash"];
/// Tools that print or search text. Their words cannot reach a database, so SQL words in them mean nothing.
/// (`echo` and `printf` are not here: their output can be piped into `psql`.)
const TEXT_ONLY: [&str; 18] = ["grep", "egrep", "fgrep", "rg", "ag", "ack", "git", "gh", "cat", "less", "more", "head", "tail", "wc", "ls", "man", "tree", "fd"];
/// Tools that take SQL on stdin: a here-document fed to them is looked at.
const SQL_CLIENTS: [&str; 8] = ["psql", "mysql", "mariadb", "sqlite3", "sqlcmd", "duckdb", "usql", "clickhouse-client"];
const FLAG_WORDS: [&str; 9] = ["reset", "push", "delete", "destroy", "drop", "execute", "migrate", "rm", "remove"];

/// What to do about one Bash command. The strongest finding wins (declaration order, weakest first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Clear,
    /// A local delete or history rewrite: write a save point first, then let it run.
    Save,
    /// Data outside this computer. The rule that matched, for the message.
    Remote(String),
    /// `torpor off|on|forget|retain`, as "torpor forget".
    UserOnly(String),
    /// Names Torpor's own files together with a command that changes them.
    Store,
}

impl Verdict {
    fn rank(&self) -> u8 {
        match self {
            Verdict::Clear => 0,
            Verdict::Save => 1,
            Verdict::Remote(_) => 2,
            Verdict::UserOnly(_) => 3,
            Verdict::Store => 4,
        }
    }
}

/// `dirs`: where package.json is looked for (the folder the command runs in, then the project root).
pub fn judge(cmd: &str, dirs: &[PathBuf]) -> Verdict {
    judge_with(cmd, dirs, &store_needles())
}

pub fn judge_with(cmd: &str, dirs: &[PathBuf], store: &[String]) -> Verdict {
    let mut end = cmd.len().min(MAX_TEXT);
    while !cmd.is_char_boundary(end) {
        end -= 1;
    }
    // What is cut off is not read, so a cut command is never judged harmless.
    let verdict = if end < cmd.len() { Verdict::Save } else { Verdict::Clear };
    let mut w = Walk { dirs, store, in_store: false, verdict, calls: Vec::new() };
    w.text(&cmd[..end], 0);
    w.verdict
}

/// Every `torpor ...` call in a command, by name ("keep", "promise add", "check"), in order (spec 12, "Who ran a CLI command").
/// Read the same way as the safety verdicts: wrappers, `sh -c` and here-documents included.
pub fn torpor_calls(cmd: &str) -> Vec<String> {
    let mut end = cmd.len().min(MAX_TEXT);
    while !cmd.is_char_boundary(end) {
        end -= 1;
    }
    let mut w = Walk { dirs: &[], store: &[], in_store: false, verdict: Verdict::Clear, calls: Vec::new() };
    w.text(&cmd[..end], 0);
    w.calls
}

/// The name a `torpor` call goes by in `cli_calls`: the first word, plus the second for `promise`, `gate` and `runner`.
pub fn call_name<S: AsRef<str>>(args: &[S]) -> Option<String> {
    let mut words = args.iter().map(|a| a.as_ref()).filter(|a| !a.starts_with('-'));
    let sub = words.next()?;
    Some(match (sub, words.next()) {
        ("promise" | "gate" | "runner", Some(o)) => format!("{sub} {o}"),
        _ => sub.to_string(),
    })
}

/// Texts that point at Torpor's own files: the store folder and the key file (or its folder), written as an
/// absolute path, with `~` or with `$HOME`.
pub fn store_needles() -> Vec<String> {
    let home = user_home().to_string_lossy().to_string();
    let mut paths = vec![home_dir(), key_path()];
    if let Some(d) = key_path().parent().filter(|d| d.file_name().is_some_and(|n| n == "torpor")) {
        paths.push(d.to_path_buf());
    }
    let mut v = Vec::new();
    for p in paths {
        let s = p.to_string_lossy().to_string();
        if let Some(rest) = s.strip_prefix(&home).filter(|r| r.starts_with('/')) {
            v.push(format!("~{rest}"));
            v.push(format!("$HOME{rest}"));
            v.push(format!("${{HOME}}{rest}"));
        }
        v.push(s);
    }
    v
}

// ---------------------------------------------------------------- reading the text

#[derive(Default)]
struct Seg {
    words: Vec<String>,
    /// An unquoted `>` that is not `>&`: the command writes to a file.
    redirect: bool,
    /// Text of a here-document fed to this command.
    heredoc: String,
}

#[derive(Default)]
struct Lex {
    out: Vec<Seg>,
    cur: Seg,
    word: String,
    /// A word has started (it can still be empty: '').
    open: bool,
    /// Here-documents opened on this line: (delimiter, `<<-`, index the owning segment will get).
    docs: Vec<(String, bool, usize)>,
}

impl Lex {
    fn word_end(&mut self) {
        if self.open {
            self.cur.words.push(std::mem::take(&mut self.word));
            self.open = false;
        }
    }

    fn seg_end(&mut self) {
        self.word_end();
        let s = std::mem::take(&mut self.cur);
        if !s.words.is_empty() || s.redirect {
            self.out.push(s);
        }
    }

    /// `2>file`: digits stuck to the operator are a file descriptor, not a word.
    fn fd_end(&mut self) {
        if self.open && !self.word.is_empty() && self.word.chars().all(|c| c.is_ascii_digit()) {
            self.word.clear();
            self.open = false;
        }
        self.word_end();
    }

    /// Reads the bodies of the here-documents opened on the line that just ended; returns the new position.
    fn bodies(&mut self, ch: &[char], mut i: usize) -> usize {
        for (delim, strip, idx) in std::mem::take(&mut self.docs) {
            let mut body = String::new();
            while i < ch.len() {
                let start = i;
                while i < ch.len() && ch[i] != '\n' {
                    i += 1;
                }
                let line: String = ch[start..i].iter().collect();
                i = (i + 1).min(ch.len());
                let t = line.trim_end_matches('\r');
                if (if strip { t.trim_start_matches('\t') } else { t }) == delim {
                    break;
                }
                body.push_str(&line);
                body.push('\n');
            }
            if let Some(s) = self.out.get_mut(idx) {
                s.heredoc.push_str(&body);
            }
        }
        i
    }
}

/// Cuts at `;`, `&&`, `||`, `|`, `&`, newlines, parentheses and backticks; understands '..', "..", backslash,
/// comments and here-documents. Quotes are removed from the words.
fn segments(text: &str) -> Vec<Seg> {
    let ch: Vec<char> = text.chars().collect();
    let n = ch.len();
    let mut lx = Lex::default();
    let mut i = 0;
    while i < n {
        match ch[i] {
            '\'' => {
                lx.open = true;
                i += 1;
                while i < n && ch[i] != '\'' {
                    lx.word.push(ch[i]);
                    i += 1;
                }
                i += 1;
            }
            '"' => {
                lx.open = true;
                i += 1;
                while i < n && ch[i] != '"' {
                    if ch[i] == '\\' && i + 1 < n && matches!(ch[i + 1], '"' | '\\' | '$' | '`') {
                        i += 1;
                    }
                    lx.word.push(ch[i]);
                    i += 1;
                }
                i += 1;
            }
            '\\' => {
                // A backslash before a newline joins the lines; before anything else it quotes that character.
                if i + 1 < n && ch[i + 1] != '\n' {
                    lx.open = true;
                    lx.word.push(ch[i + 1]);
                }
                i += 2;
            }
            '#' if !lx.open => {
                while i < n && ch[i] != '\n' {
                    i += 1;
                }
            }
            ' ' | '\t' | '\r' => {
                lx.word_end();
                i += 1;
            }
            '\n' => {
                lx.seg_end();
                i = lx.bodies(&ch, i + 1);
            }
            ';' | '|' | '(' | ')' | '`' => {
                lx.seg_end();
                i += 1;
            }
            '&' => {
                // `>&2`, `<&3` and `&>file` are redirections, not the end of a command.
                let after_redirect = i > 0 && matches!(ch[i - 1], '>' | '<');
                if after_redirect {
                    i += 1;
                    while i < n && (ch[i].is_ascii_digit() || ch[i] == '-') {
                        i += 1;
                    }
                } else if ch.get(i + 1) == Some(&'>') {
                    i += 1;
                } else {
                    lx.seg_end();
                    i += 1;
                }
            }
            '>' => {
                lx.fd_end();
                if ch.get(i + 1) != Some(&'&') {
                    lx.cur.redirect = true;
                }
                i += if ch.get(i + 1) == Some(&'|') { 2 } else { 1 };
            }
            '<' => {
                lx.fd_end();
                if ch.get(i + 1) == Some(&'<') && ch.get(i + 2) == Some(&'<') {
                    i += 3; // here-string: the string is the next word
                } else if ch.get(i + 1) == Some(&'<') {
                    i += 2;
                    let strip = ch.get(i) == Some(&'-');
                    if strip {
                        i += 1;
                    }
                    while i < n && (ch[i] == ' ' || ch[i] == '\t') {
                        i += 1;
                    }
                    let mut delim = String::new();
                    while i < n && !matches!(ch[i], ' ' | '\t' | '\n' | ';' | '|' | '&' | '(' | ')' | '<' | '>') {
                        if !matches!(ch[i], '\'' | '"' | '\\') {
                            delim.push(ch[i]);
                        }
                        i += 1;
                    }
                    if !delim.is_empty() {
                        lx.docs.push((delim, strip, lx.out.len()));
                    }
                } else {
                    i += 1;
                }
            }
            c => {
                lx.open = true;
                lx.word.push(c);
                i += 1;
            }
        }
    }
    lx.seg_end();
    lx.out
}

fn base(w: &str) -> &str {
    w.rsplit('/').next().unwrap_or(w)
}

fn is_assignment(w: &str) -> bool {
    w.split_once('=').is_some_and(|(k, _)| !k.is_empty() && !k.starts_with(|c: char| c.is_ascii_digit()) && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

/// Skips leading `-flags`. A short flag listed in `valued` (`-u root`) also takes the word after it.
fn skip_flags<'a>(mut w: &'a [String], valued: &str) -> &'a [String] {
    while let Some(f) = w.first() {
        if f == "--" {
            return &w[1..];
        }
        if !f.starts_with('-') || f == "-" {
            break;
        }
        let takes = f.len() == 2 && valued.contains(&f[1..]);
        w = &w[(1 + takes as usize).min(w.len())..];
    }
    w
}

/// The command a segment really runs: env assignments, wrappers and loop/if keywords taken off the front.
fn strip(mut w: &[String]) -> &[String] {
    loop {
        let Some(first) = w.first() else { return w };
        let rest = &w[1..];
        w = match base(first) {
            _ if is_assignment(first) => rest,
            "if" | "then" | "else" | "elif" | "do" | "while" | "until" | "!" | "{" => rest,
            "sudo" | "doas" => skip_flags(rest, "ugphCUrtDRT"),
            "command" | "exec" | "nohup" | "time" | "env" => skip_flags(rest, if base(first) == "env" { "uCS" } else { "" }),
            "nice" => skip_flags(rest, "n"),
            "timeout" => skip_flags(rest, "sk").get(1..).unwrap_or(&[]),
            "xargs" => skip_flags(rest, "nIPLdEsa"),
            "npx" | "bunx" => skip_flags(rest, "p"),
            "pnpm" | "yarn" | "npm" if matches!(rest.first().map(String::as_str), Some("dlx" | "exec")) => skip_flags(&rest[1..], ""),
            _ => return w,
        };
    }
}

/// The script after `-c` (`bash -lc 'rm x'`).
fn shell_script(a: &[String]) -> Option<&str> {
    let mut i = 0;
    while i < a.len() {
        let x = a[i].as_str();
        if !x.starts_with('-') && !x.starts_with('+') {
            return None; // a script file: not read here
        }
        if x == "-o" || x == "+o" {
            i += 1; // `-o pipefail`
        } else if x.starts_with('-') && !x.starts_with("--") && x[1..].contains('c') {
            return a.get(i + 1).map(String::as_str);
        }
        i += 1;
    }
    None
}

/// The text of `scripts.<name>` for `npm|pnpm|yarn|bun run <name>`.
fn package_script(w: &[String], dirs: &[PathBuf]) -> Option<String> {
    if !matches!(base(&w[0]), "npm" | "pnpm" | "yarn" | "bun") {
        return None;
    }
    let i = w.iter().position(|x| x == "run")?;
    let name = w[i + 1..].iter().find(|x| !x.starts_with('-'))?;
    dirs.iter().find_map(|d| {
        let p = d.join("package.json");
        if std::fs::metadata(&p).ok()?.len() > 1 << 20 {
            return None;
        }
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).ok()?).ok()?;
        v.get("scripts")?.get(name.as_str())?.as_str().map(String::from)
    })
}

// ---------------------------------------------------------------- the rules

struct Rule {
    tool: &'static str,
    /// Must appear in this order among the words that are not flags.
    words: &'static [&'static str],
    /// One of these flags must be present (written without dashes). Empty: none needed.
    flags: &'static [&'static str],
    name: &'static str,
}

const fn rule(tool: &'static str, words: &'static [&'static str], flags: &'static [&'static str], name: &'static str) -> Rule {
    Rule { tool, words, flags, name }
}

const RULES: &[Rule] = &[
    rule("terraform", &["destroy"], &[], "terraform destroy"),
    rule("terraform", &["apply"], &["destroy"], "terraform apply -destroy"),
    rule("terraform", &["apply"], &["auto-approve"], "terraform apply -auto-approve"),
    rule("tofu", &["destroy"], &[], "tofu destroy"),
    rule("tofu", &["apply"], &["destroy"], "tofu apply -destroy"),
    rule("tofu", &["apply"], &["auto-approve"], "tofu apply -auto-approve"),
    rule("pulumi", &["destroy"], &[], "pulumi destroy"),
    rule("dropdb", &[], &[], "dropdb"),
    rule("prisma", &["migrate", "reset"], &[], "prisma migrate reset"),
    rule("prisma", &["db", "push"], &["force-reset"], "prisma db push --force-reset"),
    rule("prisma", &["db", "push"], &["accept-data-loss"], "prisma db push --accept-data-loss"),
    rule("supabase", &["db", "reset"], &[], "supabase db reset"),
    rule("gh", &["repo", "delete"], &[], "gh repo delete"),
    rule("gh", &["release", "delete"], &[], "gh release delete"),
    rule("kubectl", &["delete"], &[], "kubectl delete"),
    rule("helm", &["uninstall"], &[], "helm uninstall"),
    rule("aws", &["s3", "rm"], &[], "aws s3 rm"),
    rule("aws", &["s3", "rb"], &[], "aws s3 rb"),
    rule("gsutil", &["rm"], &[], "gsutil rm"),
    rule("docker", &["volume", "rm"], &[], "docker volume rm"),
    rule("docker", &["volume", "prune"], &[], "docker volume prune"),
    rule("docker", &["system", "prune"], &[], "docker system prune"),
    rule("docker", &["compose", "down"], &["v", "volumes"], "docker compose down -v"),
    rule("docker-compose", &["down"], &["v", "volumes"], "docker compose down -v"),
];

/// The words after `tool`, when the segment runs it (directly, or as `yarn|pnpm|bun|npm <tool>`).
fn after_tool<'a>(w: &'a [String], tool: &str) -> Option<&'a [String]> {
    if base(&w[0]) == tool {
        Some(&w[1..])
    } else if matches!(base(&w[0]), "yarn" | "pnpm" | "bun" | "npm") && w.get(1).is_some_and(|x| base(x) == tool) {
        Some(&w[2..])
    } else {
        None
    }
}

/// `want` appears in `have` in this order, other words may sit between (`compose -f prod.yml down`).
fn in_order(have: &[&str], want: &[&str]) -> bool {
    let mut it = have.iter();
    want.iter().all(|w| it.any(|h| h == w))
}

fn flag_name(a: &str) -> Option<&str> {
    a.strip_prefix('-').map(|f| f.trim_start_matches('-').split('=').next().unwrap_or(""))
}

/// Index of the git subcommand in `args` (after git's own options such as `-C dir`).
fn git_sub(a: &[String]) -> Option<usize> {
    let mut i = 0;
    while i < a.len() {
        match a[i].as_str() {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path" => i += 2,
            s if s.starts_with('-') => i += 1,
            _ => return Some(i),
        }
    }
    None
}

fn git_push(w: &[String]) -> Option<String> {
    let a = &w[1..];
    let i = git_sub(a).filter(|&i| a[i] == "push")?;
    for x in &a[i + 1..] {
        if let Some(long) = x.strip_prefix("--") {
            match long.split('=').next().unwrap_or("") {
                f @ ("force" | "force-with-lease" | "delete" | "mirror") => return Some(format!("git push --{f}")),
                _ => {}
            }
        } else if let Some(short) = x.strip_prefix('-') {
            if short.contains('f') {
                return Some("git push --force".into());
            }
            if short.contains('d') {
                return Some("git push --delete".into());
            }
        } else if x.starts_with(':') {
            return Some("git push :branch".into());
        } else if x.starts_with('+') {
            return Some("git push +branch".into());
        }
    }
    None
}

/// `DROP TABLE|DATABASE|SCHEMA` and `TRUNCATE` as whole words in any case, and `dropDatabase`.
fn sql_in(text: &str) -> Option<&'static str> {
    let t = text.to_lowercase();
    let words: Vec<&str> = t.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|s| !s.is_empty()).collect();
    for (i, w) in words.iter().enumerate() {
        match *w {
            "truncate" => return Some("SQL TRUNCATE"),
            "dropdatabase" => return Some("dropDatabase"),
            "drop" => match words.get(i + 1) {
                Some(&"table") => return Some("SQL DROP TABLE"),
                Some(&"database") => return Some("SQL DROP DATABASE"),
                Some(&"schema") => return Some("SQL DROP SCHEMA"),
                _ => {}
            },
            _ => {}
        }
    }
    None
}

/// The rule that makes this command one that save points cannot take back.
fn remote(w: &[String], heredoc: &str) -> Option<String> {
    let name = base(&w[0]);
    for r in RULES {
        let Some(args) = after_tool(w, r.tool) else { continue };
        let plain: Vec<&str> = args.iter().map(String::as_str).filter(|a| !a.starts_with('-')).collect();
        let order = in_order(&plain, r.words);
        let flag = r.flags.is_empty() || args.iter().any(|a| flag_name(a).is_some_and(|f| r.flags.contains(&f)));
        if order && flag {
            return Some(r.name.into());
        }
    }
    if name == "git" {
        if let Some(r) = git_push(w) {
            return Some(r);
        }
    }
    // A shell's script is read on its own, with its real first word.
    if name != "truncate" && !TEXT_ONLY.contains(&name) && !SHELLS.contains(&name) {
        let mut text = w.join(" ");
        if SQL_CLIENTS.contains(&name) {
            text.push(' ');
            text.push_str(heredoc);
        }
        if let Some(r) = sql_in(&text) {
            return Some(r.into());
        }
    }
    let flag = w.iter().find(|a| matches!(a.split('=').next(), Some("--linked" | "--remote" | "--prod" | "--production")))?;
    let act = w.iter().find(|a| FLAG_WORDS.contains(&a.as_str()))?;
    Some(format!("{} with {act}", flag.split('=').next().unwrap_or("")))
}

/// Deletes or overwrites local files, or rewrites history: worth a save point first.
fn local(w: &[String]) -> bool {
    let a = &w[1..];
    match base(&w[0]) {
        "rm" | "rmdir" | "unlink" | "shred" | "truncate" | "mv" => true,
        "dd" => a.iter().any(|x| x.starts_with("of=")),
        "find" => a.iter().any(|x| x == "-delete") || a.windows(2).any(|p| matches!(p[0].as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") && base(&p[1]) == "rm"),
        "git" => git_sub(a).is_some_and(|i| GIT_LOCAL.contains(&a[i].as_str())),
        _ => false,
    }
}

/// The arguments of a `torpor ...` command, also as `bash ~/.torpor/bin/torpor ...`.
fn torpor_args(w: &[String]) -> Option<&[String]> {
    let i = usize::from(SHELLS.contains(&base(&w[0])));
    (w.get(i).map(|x| base(x)) == Some("torpor")).then(|| &w[i + 1..])
}

/// Is `torpor <args>` a command only the user may run? Gives it as the refusal shows it ("torpor promise drop").
/// One list for the CLI refusal and the PreToolUse deny, so they cannot drift apart.
pub fn user_only_args<S: AsRef<str>>(args: &[S]) -> Option<String> {
    let mut words = args.iter().map(|a| a.as_ref()).filter(|a| !a.starts_with('-'));
    let sub = words.next()?;
    match (sub, words.next()) {
        (s, _) if USER_ONLY.contains(&s) => Some(format!("torpor {s}")),
        // The one download Torpor makes: the AI asks you to run it (spec 24).
        ("runner", Some("install")) => Some("torpor runner install".into()),
        ("gate", Some(o @ ("on" | "off"))) => Some(format!("torpor gate {o}")),
        ("promise", Some(o @ ("ok" | "drop" | "block"))) => Some(format!("torpor promise {o}")),
        // Any `--spec...` spelling: the check source stays hidden from the AI (spec 14).
        ("promise", Some("show")) if args.iter().any(|a| a.as_ref().starts_with("--spec")) => Some("torpor promise show --spec".into()),
        _ => None,
    }
}

fn user_only(w: &[String]) -> Option<String> {
    user_only_args(torpor_args(w)?)
}

/// Does this word point at Torpor's store or key file?
fn names(word: &str, store: &[String]) -> bool {
    word.split('/').any(|c| c == ".torpor")
        || store.iter().any(|n| {
            word.match_indices(n.as_str()).any(|(i, _)| {
                (i == 0 || matches!(word.as_bytes()[i - 1], b'=' | b':' | b',')) && matches!(word.as_bytes().get(i + n.len()), None | Some(b'/'))
            })
        })
}

struct Walk<'a> {
    dirs: &'a [PathBuf],
    store: &'a [String],
    /// A `cd` into the store came earlier: later writers are aimed at it.
    in_store: bool,
    verdict: Verdict,
    /// Names of the `torpor` calls seen (see `torpor_calls`).
    calls: Vec<String>,
}

impl Walk<'_> {
    fn add(&mut self, v: Verdict) {
        if v.rank() > self.verdict.rank() {
            self.verdict = v;
        }
    }

    fn text(&mut self, text: &str, depth: u8) {
        for seg in segments(text) {
            let w = strip(&seg.words);
            let Some(first) = w.first() else { continue };
            self.seg(&seg, w);
            if depth >= MAX_DEPTH {
                continue;
            }
            if SHELLS.contains(&base(first)) {
                if let Some(script) = shell_script(&w[1..]) {
                    self.text(script, depth + 1);
                }
                if !seg.heredoc.is_empty() {
                    self.text(&seg.heredoc, depth + 1);
                }
            }
            if let Some(script) = package_script(w, self.dirs) {
                self.text(&script, depth + 1);
            }
        }
    }

    fn seg(&mut self, seg: &Seg, w: &[String]) {
        if let Some(c) = user_only(w) {
            self.add(Verdict::UserOnly(c));
        }
        if let Some(name) = torpor_args(w).and_then(call_name) {
            self.calls.push(name);
        }
        // The program itself may live in the store (`~/.torpor/bin/torpor`); only its arguments are targets.
        let hits = w[1..].iter().any(|a| names(a, self.store));
        if (hits || self.in_store) && (WRITERS.contains(&base(&w[0])) || seg.redirect) {
            self.add(Verdict::Store);
        }
        if hits && matches!(base(&w[0]), "cd" | "pushd") {
            self.in_store = true;
        }
        if let Some(r) = remote(w, &seg.heredoc) {
            self.add(Verdict::Remote(r));
        } else if local(w) {
            self.add(Verdict::Save);
        }
    }
}

/// Folders to look for package.json in: where the command runs and the folders above it (npm looks up too).
pub fn script_dirs(cwd: Option<&str>) -> Vec<PathBuf> {
    cwd.map(|c| Path::new(c).ancestors().take(5).map(Path::to_path_buf).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Vec<String> {
        ["/h/.torpor", "~/.torpor", "/h/.config/torpor/key", "~/.config/torpor/key", "/h/.config/torpor", "/t/torpor-home"].iter().map(|s| s.to_string()).collect()
    }

    fn j(cmd: &str) -> Verdict {
        judge_with(cmd, &[], &store())
    }

    fn remote_of(cmd: &str) -> Option<String> {
        match j(cmd) {
            Verdict::Remote(r) => Some(r),
            _ => None,
        }
    }

    /// Each command as its words joined with a dot.
    fn words(s: &str) -> Vec<String> {
        segments(s).into_iter().map(|g| g.words.join("\u{b7}")).collect()
    }

    #[test]
    fn reader_splits_and_unquotes() {
        assert_eq!(words("a b; c && d || e | f & g\nh"), ["a\u{b7}b", "c", "d", "e", "f", "g", "h"]);
        assert_eq!(words(r#"echo 'a b' "c; d" e\ f 'it''s'"#), ["echo\u{b7}a b\u{b7}c; d\u{b7}e f\u{b7}its"]);
        assert_eq!(words("rm \\\n x"), ["rm\u{b7}x"]);
        assert_eq!(words("ls # rm x\nls"), ["ls", "ls"]);
        assert_eq!(words("echo a#b"), ["echo\u{b7}a#b"]);
        assert_eq!(words("echo $(rm x) `rm y`"), ["echo\u{b7}$", "rm\u{b7}x", "rm\u{b7}y"]);
        // 2>&1 and &> are not the end of a command; a quoted > is not a redirection.
        assert_eq!(words("ls 2>&1 | wc"), ["ls", "wc"]);
        assert!(segments("echo hi > f")[0].redirect && segments("echo hi>>f")[0].redirect && segments("ls &> f")[0].redirect);
        assert!(!segments("ls 2>&1")[0].redirect && !segments("echo '>'")[0].redirect && !segments("echo \">\"")[0].redirect);
        assert_eq!(words("echo hi>f"), ["echo\u{b7}hi\u{b7}f"]);
    }

    #[test]
    fn reader_skips_here_documents() {
        let s = segments("cat > f <<'EOF'\nrm -rf x\nterraform destroy\nEOF\nls");
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].words, ["cat", "f"]);
        assert_eq!(s[0].heredoc, "rm -rf x\nterraform destroy\n");
        assert_eq!(s[1].words, ["ls"]);
        let t = segments("psql <<-SQL | cat\n\tDROP TABLE a;\n\tSQL\nls");
        assert_eq!(t[0].heredoc, "\tDROP TABLE a;\n");
        assert_eq!(t.len(), 3);
        assert_eq!(segments("cat <<<hello")[0].words, ["cat", "hello"]);
    }

    #[test]
    fn local_commands_get_a_save_point() {
        for c in [
            "rm -rf src",
            "rm x",
            "  sudo rm x",
            "FOO=1 BAR=2 rm x",
            "env -i FOO=1 rm x",
            "env -u HOME rm x",
            "timeout 5 rm x",
            "timeout -s KILL 5 rm x",
            "nice -n 5 rm x",
            "nohup rm x",
            "/bin/rm x",
            "\\rm x",
            "'rm' x",
            "ls && rm x",
            "echo hi; mv a b",
            "ls | xargs rm",
            "git ls-files | xargs -I{} rm {}",
            "ls | xargs -n 1 -P 4 unlink",
            "bash -c 'cd x && rm y'",
            "sh -lc \"truncate -s 0 f\"",
            "bash -o pipefail -c 'rm x'",
            "find . -name '*.o' -delete",
            "find . -type f -exec rm {} \\;",
            "dd if=/dev/zero of=file bs=1M",
            "shred -u secret",
            "rmdir d",
            "git reset --hard HEAD~1",
            "git -C sub checkout .",
            "git -c core.x=1 stash",
            "git clean -fd",
            "git restore f",
            "git switch main",
            "git rebase main",
            "git merge x",
            "git pull",
            "git cherry-pick abc",
            "git revert abc",
            "git am p.patch",
            "git apply p.diff",
            "truncate -s 0 log",
            "for f in a b; do rm $f; done",
            "if true; then rm x; fi",
            "echo $(rm x)",
            "(cd x && rm y)",
            "cat <<EOF | bash\nls\nEOF\nrm z",
            "bash <<EOF\nrm -rf x\nEOF",
        ] {
            assert_eq!(j(c), Verdict::Save, "{c}");
        }
    }

    #[test]
    fn harmless_commands_stay_clear() {
        for c in [
            "",
            "ls -la",
            "echo \"rm -rf x\"",
            "echo rm",
            "git status",
            "git log --oneline",
            "git commit -m 'clean up'",
            "git push origin main",
            "git push -u origin feature",
            "git push --dry-run",
            "git push origin HEAD:main",
            "cargo build --release",
            "npm test",
            "npm run build",
            "cat f # rm x",
            "cat > f <<'EOF'\nrm -rf x\nterraform destroy\nDROP TABLE users;\nEOF",
            "grep -rn 'DROP TABLE' migrations/",
            "rg -n truncate src",
            "git commit -m 'drop table users, truncate logs'",
            "gh pr create --body 'DROP TABLE'",
            "docker compose up -d",
            "docker compose down",
            "docker run --rm img",
            "docker rm c1",
            "terraform plan",
            "terraform apply",
            "terraform init",
            "prisma db push",
            "prisma migrate dev",
            "vercel --prod",
            "npm install --production",
            "kubectl get pods",
            "aws s3 ls s3://b",
            "echo terraform destroy",
            "command -v rm",
            "psql -c 'select 1'",
            "echo hi 2>&1",
            "echo $HOME > /dev/null",
        ] {
            let v = j(c);
            // `command -v rm` strips to `rm`: one spare save point is the documented price.
            if c == "command -v rm" {
                assert_eq!(v, Verdict::Save);
            } else {
                assert_eq!(v, Verdict::Clear, "{c}");
            }
        }
    }

    #[test]
    fn remote_rules_name_themselves() {
        let cases = [
            ("terraform destroy", "terraform destroy"),
            ("terraform -chdir=infra destroy -auto-approve", "terraform destroy"),
            ("terraform apply -destroy", "terraform apply -destroy"),
            ("terraform apply --destroy", "terraform apply -destroy"),
            ("terraform apply -var a=b -auto-approve", "terraform apply -auto-approve"),
            ("tofu apply -auto-approve=true", "tofu apply -auto-approve"),
            ("sudo tofu destroy", "tofu destroy"),
            ("pulumi destroy --yes", "pulumi destroy"),
            ("psql -c \"DROP TABLE users\"", "SQL DROP TABLE"),
            ("psql $URL -c 'drop   database  x'", "SQL DROP DATABASE"),
            ("mysql -e 'Drop Schema s'", "SQL DROP SCHEMA"),
            ("mysql -e 'TRUNCATE users'", "SQL TRUNCATE"),
            ("echo 'DROP TABLE x;' | psql", "SQL DROP TABLE"),
            ("sqlite3 app.db \"truncate t\"", "SQL TRUNCATE"),
            ("psql <<EOF\nDROP TABLE users;\nEOF", "SQL DROP TABLE"),
            ("docker exec db psql -c 'DROP TABLE t'", "SQL DROP TABLE"),
            ("dropdb mydb", "dropdb"),
            ("mongosh --eval 'db.dropDatabase()'", "dropDatabase"),
            ("prisma migrate reset", "prisma migrate reset"),
            ("npx prisma migrate reset --force", "prisma migrate reset"),
            ("pnpm dlx prisma migrate reset", "prisma migrate reset"),
            ("pnpm exec prisma migrate reset", "prisma migrate reset"),
            ("yarn prisma migrate reset", "prisma migrate reset"),
            ("prisma db push --force-reset", "prisma db push --force-reset"),
            ("prisma db push --accept-data-loss", "prisma db push --accept-data-loss"),
            ("supabase db reset", "supabase db reset"),
            ("supabase db push --linked", "--linked with push"),
            ("wrangler d1 execute DB --remote --command 'select 1'", "--remote with execute"),
            ("deploy --prod delete", "--prod with delete"),
            ("git push --force origin main", "git push --force"),
            ("git push -f", "git push --force"),
            ("git push -fu origin x", "git push --force"),
            ("git -C repo push --force-with-lease=main:abc", "git push --force-with-lease"),
            ("git push origin --delete old", "git push --delete"),
            ("git push -d origin old", "git push --delete"),
            ("git push --mirror backup", "git push --mirror"),
            ("git push origin :old", "git push :branch"),
            ("git push origin +main", "git push +branch"),
            ("gh repo delete me/x --yes", "gh repo delete"),
            ("gh release delete v1", "gh release delete"),
            ("kubectl delete pod x", "kubectl delete"),
            ("helm uninstall rel", "helm uninstall"),
            ("aws s3 rm s3://b --recursive", "aws s3 rm"),
            ("aws --profile p s3 rb s3://b", "aws s3 rb"),
            ("gsutil -m rm -r gs://b", "gsutil rm"),
            ("docker volume rm v", "docker volume rm"),
            ("docker volume prune -f", "docker volume prune"),
            ("docker system prune -a", "docker system prune"),
            ("docker compose down -v", "docker compose down -v"),
            ("docker-compose down --volumes", "docker compose down -v"),
            ("docker compose -f prod.yml down -v", "docker compose down -v"),
            ("kubectl -n prod delete deploy x", "kubectl delete"),
            ("bash -c 'terraform destroy'", "terraform destroy"),
            ("ls && terraform destroy", "terraform destroy"),
        ];
        for (c, rule) in cases {
            assert_eq!(remote_of(c).as_deref(), Some(rule), "{c}");
        }
        // coreutils truncate is a local command, not SQL
        assert_eq!(j("truncate -s 0 f"), Verdict::Save);
    }

    #[test]
    fn the_store_is_protected() {
        for c in [
            "rm -rf ~/.torpor",
            "rm -rf /h/.torpor/ws",
            "rm -rf $HOME/.torpor",
            "rm -rf .torpor",
            "sudo rm -rf /t/torpor-home/ws",
            "mv ~/.torpor /tmp/x",
            "cp ~/.torpor/bin/torpor /tmp/t",
            "sqlite3 ~/.torpor/ws/abc/db \"delete from points\"",
            "chmod 000 ~/.config/torpor/key",
            "echo x > ~/.config/torpor/key",
            "cat /dev/null>/h/.config/torpor/key",
            "truncate -s 0 /t/torpor-home/x",
            "bash -c 'rm -rf ~/.torpor'",
            "cd ~/.torpor && rm -rf ws",
            "rm -rf /h/.config/torpor",
            "ls && echo x >> --db=/t/torpor-home/db",
        ] {
            assert_eq!(j(c), Verdict::Store, "{c}");
        }
        for c in [
            "ls ~/.torpor",
            "cat ~/.torpor/ws/x/NOTES",
            "~/.torpor/bin/torpor status > out.txt",
            "~/.torpor/bin/torpor back 1 2>/dev/null",
            "rm -rf /t/torpor-homework",
            "rm -rf other/torpor-home",
            "cd /tmp && rm -rf x",
        ] {
            assert_ne!(j(c), Verdict::Store, "{c}");
        }
    }

    #[test]
    fn user_only_commands_are_refused() {
        for (c, name) in [
            ("torpor off", "torpor off"),
            ("torpor on", "torpor on"),
            ("torpor forget --all", "torpor forget"),
            ("~/.torpor/bin/torpor retain 60", "torpor retain"),
            ("sudo torpor forget", "torpor forget"),
            ("TORPOR_HOME=/x torpor off", "torpor off"),
            ("bash ~/.torpor/bin/torpor forget", "torpor forget"),
            ("bash -c 'torpor off'", "torpor off"),
            ("cd x && torpor -q off", "torpor off"),
        ] {
            assert_eq!(j(c), Verdict::UserOnly(name.into()), "{c}");
        }
        for c in ["torpor status", "torpor back 1", "torpor undo 3", "torpor log", "echo torpor off"] {
            assert!(matches!(j(c), Verdict::Clear | Verdict::Save), "{c}");
        }
    }

    #[test]
    fn promise_and_gate_commands_follow_the_same_rule_through_every_wrapper() {
        for (c, name) in [
            ("torpor promise drop 2", "torpor promise drop"),
            ("torpor promise ok 2", "torpor promise ok"),
            ("torpor promise block 2", "torpor promise block"),
            ("torpor gate off", "torpor gate off"),
            ("torpor gate on", "torpor gate on"),
            ("torpor promise show 2 --spec", "torpor promise show --spec"),
            ("torpor promise show --spec 2", "torpor promise show --spec"),
            ("torpor promise show 2 --spec=1", "torpor promise show --spec"),
            ("CLAUDECODE= torpor promise drop 2", "torpor promise drop"),
            ("env -u CLAUDECODE torpor promise drop 2", "torpor promise drop"),
            ("command torpor promise drop 2", "torpor promise drop"),
            ("exec torpor gate off", "torpor gate off"),
            ("sh -c 'torpor promise drop 2'", "torpor promise drop"),
            ("bash -lc \"cd x && torpor gate off\"", "torpor gate off"),
            ("~/.torpor/bin/2.2.0/torpor promise drop 2", "torpor promise drop"),
            ("bash ~/.torpor/bin/torpor promise block 3", "torpor promise block"),
            ("torpor -q promise -- drop 2", "torpor promise drop"),
            ("torpor promise drop 2 | cat", "torpor promise drop"),
        ] {
            assert_eq!(j(c), Verdict::UserOnly(name.into()), "{c}");
        }
        // What the AI may run: add, list, show, dispute, check, keep, find-break, undo.
        for c in [
            "torpor promise show 2",
            "torpor promise list",
            "torpor promise add - <<'EOF'\n{}\nEOF",
            "torpor promise dispute 2 \"wrong\"",
            "torpor check --all",
            "torpor keep \"works\" --check -",
            "torpor find-break --check -",
            "torpor gate",
            "echo torpor promise drop 2",
        ] {
            assert!(matches!(j(c), Verdict::Clear | Verdict::Save), "{c}");
        }
    }

    #[test]
    fn torpor_calls_are_named_the_same_way_on_both_sides() {
        assert_eq!(torpor_calls("cd x && torpor keep \"a b\" --check - && npm test"), vec!["keep"]);
        assert_eq!(torpor_calls("torpor promise add --good 5 - <<'EOF'\n{}\nEOF"), vec!["promise add"]);
        assert_eq!(torpor_calls("bash -c 'torpor check --all; torpor gate on'"), vec!["check", "gate on"]);
        assert_eq!(torpor_calls("env FOO=1 ~/.torpor/bin/torpor find-break"), vec!["find-break"]);
        assert!(torpor_calls("echo torpor keep; ls").is_empty());
        assert_eq!(call_name(&["promise", "show", "--spec", "2"]).as_deref(), Some("promise show"));
        assert_eq!(call_name(&["-q", "keep", "words"]).as_deref(), Some("keep"));
        assert_eq!(call_name::<&str>(&[]), None);
    }

    #[test]
    fn package_scripts_are_followed_one_level() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("package.json"), r#"{"scripts":{"clean":"rm -rf dist","nuke":"terraform destroy","loop":"npm run loop","ok":"echo hi"}}"#).unwrap();
        let dirs = vec![d.path().to_path_buf()];
        let at = |c: &str| judge_with(c, &dirs, &store());
        assert_eq!(at("npm run clean"), Verdict::Save);
        assert_eq!(at("pnpm run clean"), Verdict::Save);
        assert_eq!(at("yarn run --silent clean"), Verdict::Save);
        assert_eq!(at("bun run clean"), Verdict::Save);
        assert_eq!(at("npm run nuke"), Verdict::Remote("terraform destroy".into()));
        assert_eq!(at("bash -c 'npm run clean'"), Verdict::Save);
        assert_eq!(at("npm run ok"), Verdict::Clear);
        assert_eq!(at("npm run missing"), Verdict::Clear);
        assert_eq!(at("npm run loop"), Verdict::Clear); // stops, no endless recursion
        assert_eq!(judge_with("npm run clean", &[], &store()), Verdict::Clear);
    }

    #[test]
    fn the_strongest_finding_wins() {
        assert_eq!(j("rm x && terraform destroy"), Verdict::Remote("terraform destroy".into()));
        assert_eq!(j("terraform destroy; rm -rf ~/.torpor"), Verdict::Store);
        assert_eq!(j("torpor off; rm x"), Verdict::UserOnly("torpor off".into()));
    }

    #[test]
    fn huge_commands_are_cut_but_never_called_harmless() {
        let big = format!("echo {}; ls", "a".repeat(MAX_TEXT));
        assert_eq!(j(&big), Verdict::Save);
        let mut deny = format!("rm ~/.torpor; echo {}", "a".repeat(10));
        assert_eq!(j(&deny), Verdict::Store);
        deny = format!("echo {}", "a".repeat(MAX_TEXT - 5));
        assert_eq!(j(&deny), Verdict::Clear, "just under the limit is read in full");
    }
}

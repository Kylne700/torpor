//! The error key of a failed shell command (spec 21, "ek1"): `ek1:<argv0>:exit<N>:<h>`, where h is the first 8 hex characters of a BLAKE3 hash of
//! the last 20 output lines with the parts that change from run to run (times, ids, ports, numbers, paths) replaced by markers.
//! Only the key is stored, never the output. N is the exit code Claude Code reported; a number inside the output (a wrapper's "exited with code 1")
//! never changes it. Written by hand, no regex: the main binary has none.
//!
//! shortcut: dates are ISO ones (2026-10-22, with an optional time) and clock times (14:03:11); a log that writes dates another way gets a
//! different key on each run, so a repeat is not noticed. Upgrade when real logs show it.
use crate::util::hex;

/// How many output lines (blank ones do not count) go into the hash.
const LINES: usize = 20;
/// Only the end of a huge output matters.
const MAX_BYTES: usize = 64 * 1024;

/// The key for a failed command. `root` is the project folder and `home` the user's home; both are dropped from the text.
pub fn key(cmd: &str, exit: i64, output: &str, root: &str, home: &str) -> String {
    let mut start = output.len().saturating_sub(MAX_BYTES);
    while !output.is_char_boundary(start) {
        start += 1;
    }
    let text = strip_ansi(&output[start..])
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\\', "/");
    let (root, home) = (root.replace('\\', "/"), home.replace('\\', "/"));
    let lines: Vec<String> = text
        .lines()
        .map(|l| norm_line(l, &root, &home))
        .filter(|l| !l.trim().is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(LINES)..].join("\n");
    let h = hex(blake3::hash(tail.as_bytes()).as_bytes());
    format!("ek1:{}:exit{}:{}", argv0(cmd), exit, &h[..8])
}

/// The program a command runs: the first word that is not a `VAR=value`, a flag or a wrapper (sudo, env, time, nohup, exec, command), without its folder.
pub fn argv0(cmd: &str) -> String {
    const WRAP: [&str; 6] = ["sudo", "env", "time", "nohup", "exec", "command"];
    for w in cmd.split_whitespace() {
        let assign = w.split_once('=').is_some_and(|(k, _)| {
            !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        let base = w.rsplit('/').next().unwrap_or(w);
        if assign || w.starts_with('-') || WRAP.contains(&base) {
            continue;
        }
        let clean: String = base
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            .take(20)
            .collect();
        return if clean.is_empty() {
            "cmd".into()
        } else {
            clean
        };
    }
    "cmd".into()
}

/// The program name inside a stored key, for a line that may not show the command.
pub fn argv0_of_key(key: &str) -> Option<&str> {
    key.strip_prefix("ek1:")?
        .split(':')
        .next()
        .filter(|s| !s.is_empty())
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match it.peek() {
            // CSI: up to a final byte @ to ~
            Some('[') => {
                it.next();
                for n in it.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&n) {
                        break;
                    }
                }
            }
            // OSC: up to BEL or ESC \
            Some(']') => {
                it.next();
                while let Some(n) = it.next() {
                    if n == '\u{7}' {
                        break;
                    }
                    if n == '\u{1b}' {
                        it.next();
                        break;
                    }
                }
            }
            Some(_) => {
                it.next();
            }
            None => {}
        }
    }
    out
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn run(c: &[char], i: usize, f: impl Fn(char) -> bool) -> usize {
    c.get(i..)
        .map_or(0, |s| s.iter().take_while(|x| f(**x)).count())
}

fn digits(c: &[char], i: usize) -> usize {
    run(c, i, |x| x.is_ascii_digit())
}

fn boundary(c: &[char], i: usize) -> bool {
    c.get(i).is_none_or(|x| !is_word(*x))
}

/// `\d{1,2}:\d{2}` and, if more follows, `:\d{2}` with an optional `.fraction`. `need_sec`: a time without seconds is not a time (it could be line:column).
fn clock_end(c: &[char], j: usize, need_sec: bool) -> Option<usize> {
    let d1 = digits(c, j);
    if !(1..=2).contains(&d1) || c.get(j + d1) != Some(&':') || digits(c, j + d1 + 1) != 2 {
        return None;
    }
    let mut k = j + d1 + 3;
    if c.get(k) == Some(&':') && digits(c, k + 1) == 2 {
        k += 3;
        if c.get(k) == Some(&'.') && digits(c, k + 1) > 0 {
            k += 1 + digits(c, k + 1);
        }
    } else if need_sec {
        return None;
    }
    Some(k)
}

/// 2026-10-22, optionally followed by T or a space and a time, and Z or an offset.
fn iso_end(c: &[char], i: usize) -> Option<usize> {
    if !(digits(c, i) == 4
        && c.get(i + 4) == Some(&'-')
        && digits(c, i + 5) == 2
        && c.get(i + 7) == Some(&'-')
        && digits(c, i + 8) == 2)
    {
        return None;
    }
    let mut j = i + 10;
    if matches!(c.get(j), Some('T') | Some(' ')) {
        if let Some(k) = clock_end(c, j + 1, false) {
            j = k;
            match c.get(j) {
                Some('Z') => j += 1,
                Some('+') | Some('-') if digits(c, j + 1) == 2 => {
                    j += 3;
                    if c.get(j) == Some(&':') && digits(c, j + 1) == 2 {
                        j += 3;
                    } else if digits(c, j) == 2 {
                        j += 2;
                    }
                }
                _ => {}
            }
        }
    }
    Some(j)
}

fn is_uuid(c: &[char], i: usize) -> bool {
    let hexs = |from: usize, n: usize| {
        c.get(from..from + n)
            .is_some_and(|s| s.iter().all(|x| x.is_ascii_hexdigit()))
    };
    hexs(i, 8)
        && c.get(i + 8) == Some(&'-')
        && hexs(i + 9, 4)
        && c.get(i + 13) == Some(&'-')
        && hexs(i + 14, 4)
        && c.get(i + 18) == Some(&'-')
        && hexs(i + 19, 4)
        && c.get(i + 23) == Some(&'-')
        && hexs(i + 24, 12)
        && boundary(c, i + 36)
}

/// Length of a duration unit at `i` (longest first); `spaced`: after a space only the written-out units count.
fn unit_len(c: &[char], i: usize, spaced: bool) -> Option<usize> {
    const WORDS: [&str; 10] = [
        "milliseconds",
        "millisecond",
        "seconds",
        "second",
        "minutes",
        "minute",
        "secs",
        "sec",
        "mins",
        "min",
    ];
    const SHORT: [&str; 7] = ["ms", "us", "µs", "ns", "s", "m", "h"];
    let mut cands: Vec<&str> = WORDS.to_vec();
    cands.push("ms");
    if !spaced {
        cands.extend(SHORT);
    }
    cands.sort_by_key(|u| std::cmp::Reverse(u.chars().count()));
    cands.into_iter().find_map(|u| {
        let n = u.chars().count();
        let same = c
            .get(i..i + n)
            .is_some_and(|s| s.iter().copied().eq(u.chars()));
        (same && boundary(c, i + n)).then_some(n)
    })
}

/// The text of one line with the markers in. `root` and `home` are already written with `/`.
fn norm_line(line: &str, root: &str, home: &str) -> String {
    let mut s = line.to_string();
    if root.len() > 1 {
        s = s.replace(&format!("{root}/"), "").replace(root, ".");
    }
    if home.len() > 1 {
        s = s.replace(home, "<home>");
    }
    let c: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        let word_start = i == 0 || !is_word(c[i - 1]);
        if ch == ':'
            && i > 0
            && matches!(c[i - 1], ']' | ':')
            && (2..=5).contains(&digits(&c, i + 1))
            && boundary(&c, i + 1 + digits(&c, i + 1))
        {
            // ":::8080" and "[::]:8080"
            out.push_str(":<port>");
            i += 1 + digits(&c, i + 1);
            continue;
        }
        if !(word_start && ch.is_ascii_alphanumeric()) {
            out.push(ch);
            i += 1;
            continue;
        }
        let alnum = run(&c, i, |x| x.is_ascii_alphanumeric());
        if let Some(e) = iso_end(&c, i).or_else(|| clock_end(&c, i, true)) {
            out.push_str("<ts>");
            i = e;
        } else if is_uuid(&c, i) {
            out.push_str("<hex>");
            i += 36;
        } else if alnum >= 12 && c[i..i + alnum].iter().all(|x| x.is_ascii_hexdigit()) {
            out.push_str("<hex>");
            i += alnum;
        } else if ch.is_ascii_digit() {
            let mut k = i + digits(&c, i);
            let int_end = k;
            while c.get(k) == Some(&'.') && digits(&c, k + 1) > 0 {
                k += 1 + digits(&c, k + 1);
            }
            let dotted = c[i..k].iter().filter(|x| **x == '.').count();
            let port = c.get(k) == Some(&':')
                && (2..=5).contains(&digits(&c, k + 1))
                && boundary(&c, k + 1 + digits(&c, k + 1));
            if dotted == 3 && port {
                // an address with its port
                out.push_str("<n>:<port>");
                i = k + 1 + digits(&c, k + 1);
            } else if let Some(u) =
                unit_len(&c, k, false).filter(|_| is_word(*c.get(k).unwrap_or(&' ')))
            {
                out.push_str("<dur>");
                i = k + u;
            } else if c.get(k) == Some(&' ') && unit_len(&c, k + 1, true).is_some() {
                out.push_str("<dur>");
                i = k + 1 + unit_len(&c, k + 1, true).unwrap_or(0);
            } else if boundary(&c, k) {
                out.push_str("<n>");
                i = k;
            } else {
                // a word that starts with digits (3d, 0x1f): left alone
                let _ = int_end;
                out.extend(&c[i..i + alnum.max(1)]);
                i += alnum.max(1);
            }
        } else {
            let word: String = c[i..i + alnum].iter().collect();
            let mut k = i + alnum;
            let low = word.to_ascii_lowercase();
            if low == "localhost"
                && c.get(k) == Some(&':')
                && (2..=5).contains(&digits(&c, k + 1))
                && boundary(&c, k + 1 + digits(&c, k + 1))
            {
                out.push_str(&word);
                out.push_str(":<port>");
                i = k + 1 + digits(&c, k + 1);
            } else if low == "port" {
                let seps = run(&c, k, |x| matches!(x, ' ' | '=' | ':'));
                let d = digits(&c, k + seps);
                if seps > 0 && (2..=5).contains(&d) && boundary(&c, k + seps + d) {
                    out.push_str(&word);
                    out.extend(&c[k..k + seps]);
                    out.push_str("<port>");
                    k += seps + d;
                } else {
                    out.push_str(&word);
                }
                i = k;
            } else {
                out.push_str(&word);
                i = k;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/work/proj";
    const HOME: &str = "/srv/h";

    fn k(out: &str) -> String {
        key("npm test", 1, out, ROOT, HOME)
    }

    fn n(line: &str) -> String {
        norm_line(line, ROOT, HOME)
    }

    #[test]
    fn the_parts_that_change_between_runs_become_markers() {
        assert_eq!(n("2026-10-22T14:03:11.123Z started"), "<ts> started");
        assert_eq!(n("at 2026-10-22 14:03+02:00 ok"), "at <ts> ok");
        assert_eq!(n("12:30:45 tick"), "<ts> tick");
        assert_eq!(
            n("src/a.ts:12:30 error TS2304: cannot find"),
            "src/a.ts:<n>:<n> error TS2304: cannot find"
        );
        assert_eq!(
            n("id 3f2b9c1d-7a4e-4c1b-9d2e-0123456789ab done"),
            "id <hex> done"
        );
        assert_eq!(n("commit 0123456789abcdef0123 stuck"), "commit <hex> stuck");
        assert_eq!(n("a1b2c3 is short"), "a1b2c3 is short");
        assert_eq!(
            n("took 230ms, then 1.5s and 12 seconds"),
            "took <dur>, then <dur> and <dur>"
        );
        assert_eq!(
            n("listening on localhost:3000 and 127.0.0.1:8080"),
            "listening on localhost:<port> and <n>:<port>"
        );
        assert_eq!(
            n("[::]:5173 and :::4000 and port 8080 and PORT=9000"),
            "[::]:<port> and :::<port> and port <port> and PORT=<port>"
        );
        assert_eq!(
            n("3 failed, 12 passed, 0.5 coverage"),
            "<n> failed, <n> passed, <n> coverage"
        );
        assert_eq!(n("2x speed, 0x1f, TS2304"), "2x speed, 0x1f, TS2304");
        assert_eq!(
            n("/work/proj/src/a.ts and /srv/h/.cache/x"),
            "src/a.ts and <home>/.cache/x"
        );
        assert_eq!(n("in /work/proj now"), "in . now");
    }

    #[test]
    fn two_runs_of_the_same_failure_share_a_key_and_other_failures_do_not() {
        let a = "\u{1b}[31mFAIL\u{1b}[0m src/login.test.ts (4.2 s)\r\n  expected 200 but got 500\n  at /work/proj/src/login.ts:41:9\nTests: 1 failed, 3 passed, 4 total\nTime: 5.1 s\n";
        let b = "FAIL src/login.test.ts (6.9 s)\n  expected 200 but got 500\n  at /work/proj/src/login.ts:57:9\nTests: 1 failed, 11 passed, 12 total\nTime: 7.4 s\n";
        let c = "FAIL src/login.test.ts (4.2 s)\n  expected 201 but got 404 and more\n  at /work/proj/src/login.ts:41:9\nTests: 1 failed, 3 passed, 4 total\nTime: 5.1 s\n";
        assert_eq!(k(a), k(b), "{} vs {}", k(a), k(b));
        assert_ne!(k(a), k(c));
        assert!(
            k(a).starts_with("ek1:npm:exit1:") && k(a).len() == "ek1:npm:exit1:".len() + 8,
            "{}",
            k(a)
        );
        // Only the last 20 lines count, blank ones not at all.
        let long = |first: &str| {
            format!(
                "{first}\n\n{}\n",
                (0..25)
                    .map(|i| format!("line {}", ["a", "b", "c"][i % 3]))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        assert_eq!(k(&long("early noise one")), k(&long("early noise two")));
    }

    #[test]
    fn the_exit_code_comes_from_the_caller_and_the_program_from_the_command() {
        let wrapped = "npm ERR! command failed\nnpm ERR! exited with code 99\n";
        assert_eq!(
            key("npm test", 2, wrapped, ROOT, HOME).split(':').nth(2),
            Some("exit2")
        );
        assert_eq!(argv0("FOO=1 sudo -E /usr/bin/node build.js"), "node");
        assert_eq!(argv0("  "), "cmd");
        assert_eq!(argv0("./run it"), "run");
        assert_eq!(argv0_of_key("ek1:cargo:exit101:abcd1234"), Some("cargo"));
        assert_eq!(argv0_of_key("nope"), None);
        // A huge output and an odd character at the cut do not panic.
        let big = "é".repeat(100_000);
        assert!(key("x", 1, &big, ROOT, HOME).starts_with("ek1:x:exit1:"));
    }
}

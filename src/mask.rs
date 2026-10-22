//! Masking of the little text Torpor keeps (spec 4.6). Applied before anything is stored.
// Hand-written scanners: compiling the two regexes they replace cost 2.7 ms in every process that masks.

fn is_name(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')
}
fn is_run(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'=' | b'-')
}
fn has_word(name: &[u8]) -> bool {
    [&b"key"[..], b"token", b"secret", b"pass", b"pwd", b"auth"].iter().any(|w| name.windows(w.len()).any(|x| x.eq_ignore_ascii_case(w)))
}

/// End of the value after `KEY=` that starts at `at`: a quoted string, else everything up to white space. None when empty.
fn value_end(s: &str, at: usize) -> Option<usize> {
    let rest = &s[at..];
    for q in ['"', '\''] {
        if rest.starts_with(q) {
            if let Some(i) = rest[1..].find(q) {
                return Some(at + i + 2);
            }
        }
    }
    let n = rest.find(char::is_whitespace).unwrap_or(rest.len());
    (n > 0).then_some(at + n)
}

/// `NAME=value` where NAME (letters, digits, `_.-`) holds key|token|secret|pass|pwd|auth in any case: the value becomes [hidden].
fn mask_kv(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let (mut from, mut i) = (0, 0); // `from`: end of the last match; text before it is already in `out`
    while let Some(off) = s[i..].find('=') {
        let eq = i + off;
        let mut st = eq;
        while st > from && is_name(b[st - 1]) {
            st -= 1;
        }
        match value_end(s, eq + 1) {
            Some(end) if st < eq && has_word(&b[st..eq]) => {
                out.push_str(&s[from..eq]);
                out.push_str("=[hidden]");
                from = end;
                i = end;
            }
            _ => i = eq + 1,
        }
    }
    out.push_str(&s[from..]);
    out
}

/// Any run of 16+ chars from [A-Za-z0-9_+=-] holding a letter and a digit becomes [hidden].
pub fn mask_runs(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let (mut from, mut i) = (0, 0);
    while i < b.len() {
        if !is_run(b[i]) {
            i += 1;
            continue;
        }
        let st = i;
        while i < b.len() && is_run(b[i]) {
            i += 1;
        }
        let t = &b[st..i];
        if t.len() >= 16 && t.iter().any(u8::is_ascii_alphabetic) && t.iter().any(u8::is_ascii_digit) {
            out.push_str(&s[from..st]);
            out.push_str("[hidden]");
            from = i;
        }
    }
    out.push_str(&s[from..]);
    out
}

/// Any run of 16+ chars from [A-Za-z0-9_+=-] holding a letter and a digit becomes [hidden];
/// the value after KEY= (KEY matching key|token|secret|pass|pwd|auth) too.
pub fn mask(s: &str) -> String {
    mask_runs(&mask_kv(s))
}

fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for ch in s.chars() {
        let sp = ch.is_whitespace() || ch.is_control();
        if sp {
            if !last_space {
                out.push(' ');
            }
        } else {
            out.push(ch);
        }
        last_space = sp;
    }
    out.trim().to_string()
}

/// First 60 chars of the prompt, masked. None when the user turned prompt history off.
pub fn prompt_preview(prompt: &str) -> Option<String> {
    if std::env::var_os("CLAUDE_CODE_SKIP_PROMPT_HISTORY").is_some() {
        return None;
    }
    let head: String = prompt.chars().take(400).collect();
    Some(crate::util::trunc_chars(&one_line(&mask(&head)), 60))
}

/// First 200 chars of a Bash command, masked.
pub fn cmd_preview(cmd: &str) -> String {
    let head: String = cmd.chars().take(600).collect();
    crate::util::trunc_chars(&one_line(&mask(&head)), 200)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hides_long_mixed_runs_and_key_values() {
        assert_eq!(mask("curl -H x tc_0123456789abcdef0123456789abcdef"), "curl -H x [hidden]");
        assert_eq!(mask("export API_TOKEN=hello world"), "export API_TOKEN=[hidden] world");
        assert_eq!(mask("PASSWORD=\"a b c\" ls"), "PASSWORD=[hidden] ls");
        assert_eq!(mask("abcdefghijklmnopqrstuvwxyz"), "abcdefghijklmnopqrstuvwxyz");
        assert_eq!(mask("make the login button blue"), "make the login button blue");
    }
    /// The scanners must do what the regexes they replaced did (the regex crate stays as a dev-dependency for this). Only difference: case folding is ASCII-only (the regex also folded the Kelvin sign and the long s).
    #[test]
    fn matches_the_regex_version() {
        use regex::{Captures, Regex};
        let kv = Regex::new(r#"(?i)([A-Za-z0-9_.-]*(?:key|token|secret|pass|pwd|auth)[A-Za-z0-9_.-]*)=("[^"]*"|'[^']*'|\S+)"#).unwrap();
        let run = Regex::new(r"[A-Za-z0-9_+=-]{16,}").unwrap();
        let reference = |s: &str| {
            let a = kv.replace_all(s, |c: &Captures| format!("{}=[hidden]", &c[1]));
            run.replace_all(&a, |c: &Captures| {
                let t = &c[0];
                if t.bytes().any(|b| b.is_ascii_alphabetic()) && t.bytes().any(|b| b.is_ascii_digit()) { "[hidden]".to_string() } else { t.to_string() }
            })
            .into_owned()
        };
        let atoms = [
            "key", "KEY", "Token", "secret", "SeCrEt", "pass", "pwd", "auth", "=", "=", "\"", "'", " ", " ", "\t", "\n", "a", "B", "7", "0", "_", "-", ".", "+", "\u{e9}", "\u{a0}", "\u{2003}", "x",
            "abcdefgh12345678", "ABCDEFGHIJKLMNOP", "1234567890123456", "[", "]", "/", "$",
        ];
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..40_000 {
            let n = 1 + (next() % 14) as usize;
            let s: String = (0..n).map(|_| atoms[(next() % atoms.len() as u64) as usize]).collect();
            assert_eq!(mask(&s), reference(&s), "input {s:?}");
        }
    }
    #[test]
    fn preview_is_60_chars() {
        let p = prompt_preview(&"word ".repeat(40)).unwrap();
        assert_eq!(p.chars().count(), 60);
    }
}

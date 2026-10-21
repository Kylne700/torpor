//! Masking of the little text Torpor keeps (spec 4.6). Applied before anything is stored.
use regex::{Captures, Regex};
use std::sync::OnceLock;

fn kv_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r#"(?i)([A-Za-z0-9_.-]*(?:key|token|secret|pass|pwd|auth)[A-Za-z0-9_.-]*)=("[^"]*"|'[^']*'|\S+)"#).unwrap()
    })
}
fn run_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"[A-Za-z0-9_+=-]{16,}").unwrap())
}

/// Any run of 16+ chars from [A-Za-z0-9_+=-] holding a letter and a digit becomes [hidden];
/// the value after KEY= (KEY matching key|token|secret|pass|pwd|auth) too.
pub fn mask(s: &str) -> String {
    let a = kv_re().replace_all(s, |c: &Captures| format!("{}=[hidden]", &c[1]));
    run_re()
        .replace_all(&a, |c: &Captures| {
            let t = &c[0];
            if t.bytes().any(|b| b.is_ascii_alphabetic()) && t.bytes().any(|b| b.is_ascii_digit()) {
                "[hidden]".to_string()
            } else {
                t.to_string()
            }
        })
        .into_owned()
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
    #[test]
    fn preview_is_60_chars() {
        let p = prompt_preview(&"word ".repeat(40)).unwrap();
        assert_eq!(p.chars().count(), 60);
    }
}

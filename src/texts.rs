//! Prompt texts (spec 13): the redacted words of each turn, copied at UserPromptSubmit because transcripts expire.
//! Redaction is offline and by hand (no regex crate): known secret formats, then keyword-gated high-entropy values.
use crate::util::*;
use rusqlite::{Connection, OptionalExtension, params};

const MASK: &str = "[secret]";
/// shortcut: a prompt longer than this is cut when stored; raise it when a use needs the tail of a huge paste.
const MAX_BYTES: usize = 200_000;

/// True when Claude Code was asked not to keep prompt history: then Torpor keeps no texts either (spec 13).
pub fn skip_history() -> bool {
    std::env::var("CLAUDE_CODE_SKIP_PROMPT_HISTORY")
        .is_ok_and(|v| !matches!(v.trim(), "" | "0" | "false"))
}

/// Store the redacted prompt of a turn. No-op when history is off or the turn already has a text.
pub fn store_prompt(db: &Connection, turn_id: i64, prompt: &str) -> Result<()> {
    let off = skip_history();
    let was = crate::store::meta_get(db, "texts_off").is_some_and(|v| v == "1");
    if off != was {
        crate::store::meta_set(db, "texts_off", if off { "1" } else { "0" })?;
    }
    if off {
        return Ok(());
    }
    // The A7 rule of the first release also holds here: a long id-like run (16+ letters and digits) never reaches the store in the clear.
    let (text, n) = redact(prompt);
    let masked = crate::mask::mask_runs(&text);
    let n = n + masked
        .matches("[hidden]")
        .count()
        .saturating_sub(text.matches("[hidden]").count());
    let text = masked;
    let text = if text.len() > MAX_BYTES {
        trunc_chars(&text, MAX_BYTES / 4)
    } else {
        text
    };
    db.execute("INSERT INTO texts(turn_id, prompt, redactions) VALUES(?1, ?2, ?3) ON CONFLICT(turn_id) DO NOTHING", params![turn_id, text, n as i64])?;
    Ok(())
}

pub fn set_origin(db: &Connection, turn_id: i64, origin: &str) -> Result<()> {
    db.execute(
        "UPDATE texts SET origin=?2 WHERE turn_id=?1 AND origin='unknown'",
        params![turn_id, origin],
    )?;
    Ok(())
}

/// The kept words of a turn (None when never stored, or nulled at W).
pub fn prompt_of(db: &Connection, turn_id: i64) -> Option<String> {
    db.query_row(
        "SELECT prompt FROM texts WHERE turn_id=?1",
        [turn_id],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()
    .ok()
    .flatten()
    .flatten()
}

/// Hebrew letters and no Latin word: praise like that stays advisory for good (spec 14).
pub fn hebrew_only(s: &str) -> bool {
    let heb = s.chars().any(|c| ('\u{0590}'..='\u{05FF}').contains(&c));
    heb && !s.chars().any(|c| c.is_ascii_alphabetic())
}

/// Replace secrets with `[secret]`; returns the new text and how many spans were replaced.
pub fn redact(s: &str) -> (String, usize) {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let (mut i, mut n) = (0, 0);
    while i < b.len() {
        if let Some(end) = format_end(b, i) {
            out.extend_from_slice(MASK.as_bytes());
            n += 1;
            i = end;
        } else if let Some((vs, ve)) = gated_value(b, i) {
            out.extend_from_slice(&b[i..vs]);
            out.extend_from_slice(MASK.as_bytes());
            n += 1;
            i = ve;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    // Only ASCII spans were replaced, so the bytes are still UTF-8.
    (String::from_utf8(out).unwrap_or_default(), n)
}

fn find(b: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    b.get(from..)?
        .windows(pat.len())
        .position(|w| w == pat)
        .map(|p| p + from)
}

/// `prefix` followed by `min..` characters of a class (or exactly `exact`): the end of the match.
fn run(
    b: &[u8],
    i: usize,
    prefix: usize,
    min: usize,
    exact: bool,
    ok: impl Fn(u8) -> bool,
) -> Option<usize> {
    let n = b[i + prefix..].iter().take_while(|c| ok(**c)).count();
    (n >= min).then(|| i + prefix + if exact { min } else { n })
}

fn format_end(b: &[u8], i: usize) -> Option<usize> {
    let r = &b[i..];
    let word_start = i == 0 || !b[i - 1].is_ascii_alphanumeric();
    let alnum = |c: u8| c.is_ascii_alphanumeric();
    let b64url = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'-';
    match r[0] {
        b'-' if r.starts_with(b"-----BEGIN ") => {
            let hdr = find(b, i + 11, b"-----")?;
            if !String::from_utf8_lossy(&b[i + 11..hdr]).contains("PRIVATE KEY") {
                return None;
            }
            // A truncated paste has no END line: everything after BEGIN goes.
            Some(
                find(b, hdr + 5, b"-----END ")
                    .and_then(|e| find(b, e + 9, b"-----"))
                    .map_or(b.len(), |e| e + 5),
            )
        }
        b'A' if r.starts_with(b"AKIA") || r.starts_with(b"ASIA") => run(b, i, 4, 16, true, |c| {
            c.is_ascii_uppercase() || c.is_ascii_digit()
        }),
        // ghu_ and ghr_ are the rest of GitHub's token family.
        b'g' if r.starts_with(b"ghp_")
            || r.starts_with(b"gho_")
            || r.starts_with(b"ghs_")
            || r.starts_with(b"ghu_")
            || r.starts_with(b"ghr_") =>
        {
            run(b, i, 4, 20, false, alnum)
        }
        b'g' if r.starts_with(b"github_pat_") => {
            run(b, i, 11, 20, false, |c| alnum(c) || c == b'_')
        }
        // 20 characters in all, "sk-" included; the word start keeps "task-list-of-things" and "desk-..." out.
        b's' if word_start && r.starts_with(b"sk-") => run(b, i, 3, 17, false, b64url),
        b's' | b'r' if (r.starts_with(b"sk_live_") || r.starts_with(b"rk_live_")) => {
            run(b, i, 8, 8, false, alnum)
        }
        b'x' if r.len() > 4
            && r.starts_with(b"xox")
            && b"abprs".contains(&r[3])
            && r[4] == b'-' =>
        {
            run(b, i, 5, 10, false, |c| alnum(c) || c == b'-')
        }
        b'A' if r.starts_with(b"AIza") => run(b, i, 4, 35, true, b64url),
        b'e' if r.starts_with(b"eyJ") => {
            let p1 = run(b, i, 3, 1, false, b64url)?;
            (b.get(p1) == Some(&b'.')).then_some(())?;
            let p2 = run(b, p1 + 1, 0, 1, false, b64url)?;
            (b.get(p2) == Some(&b'.')).then_some(())?;
            run(b, p2 + 1, 0, 0, false, b64url)
        }
        _ => None,
    }
}

const KEYWORDS: [&[u8]; 6] = [b"key", b"token", b"secret", b"pass", b"pwd", b"auth"];

/// A key/token/secret/pass/pwd/auth word, then `=` or `:`, then a value of 12+ characters at 3.5+ bits per character.
/// Returns the value's span. Called at every position, so the word may sit inside a name (`api_key`, `AUTH_TOKEN`).
fn gated_value(b: &[u8], i: usize) -> Option<(usize, usize)> {
    let kw = KEYWORDS
        .iter()
        .find(|k| b.len() >= i + k.len() && b[i..i + k.len()].eq_ignore_ascii_case(k))?;
    let mut j = i + kw.len();
    while b
        .get(j)
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
    {
        j += 1;
    }
    let skip = |mut j: usize, set: &[u8]| {
        while b.get(j).is_some_and(|c| set.contains(c)) {
            j += 1;
        }
        j
    };
    j = skip(j, b" \t\"'");
    (b.get(j).is_some_and(|c| *c == b'=' || *c == b':')).then_some(())?;
    let vs = skip(j + 1, b" \t\"'");
    let ve = vs
        + b[vs..]
            .iter()
            .take_while(|c| (0x21..0x7f).contains(*c) && !b"\"',;`)}]<>".contains(c))
            .count();
    (ve - vs >= 12 && entropy(&b[vs..ve]) >= 3.5).then_some((vs, ve))
}

fn entropy(v: &[u8]) -> f64 {
    let mut freq = [0usize; 256];
    for c in v {
        freq[*c as usize] += 1;
    }
    let n = v.len() as f64;
    freq.iter()
        .filter(|c| **c > 0)
        .map(|c| *c as f64 / n)
        .map(|p| -p * p.log2())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn red(s: &str) -> String {
        redact(s).0
    }

    #[test]
    fn known_formats_become_secret() {
        let pem = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow\nabc\n-----END RSA PRIVATE KEY-----\nafter";
        assert_eq!(red(pem), "before\n[secret]\nafter");
        // A paste cut short: everything after BEGIN goes.
        assert_eq!(red("x -----BEGIN PRIVATE KEY-----\nMIIE"), "x [secret]");
        // A public certificate is not a secret.
        assert_eq!(
            red("-----BEGIN CERTIFICATE-----\nMIIE\n-----END CERTIFICATE-----"),
            "-----BEGIN CERTIFICATE-----\nMIIE\n-----END CERTIFICATE-----"
        );
        assert_eq!(red("id AKIAABCDEFGHIJKLMNOP end"), "id [secret] end");
        assert_eq!(red("ASIAABCDEFGHIJKLMNOPQ"), "[secret]Q");
        assert_eq!(red("AKIAshort"), "AKIAshort");
        let gh = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        assert_eq!(red(&format!("t {gh}.")), "t [secret].");
        assert_eq!(
            red("gho_abcdefghijklmnopqrstuvwx and ghs_abcdefghijklmnopqrstuvwxyz"),
            "[secret] and [secret]"
        );
        assert_eq!(
            red("github_pat_11ABCDEFG0abcdefghijkl_abcdefghijklmnop"),
            "[secret]"
        );
        assert_eq!(red("ghp_short"), "ghp_short");
        assert_eq!(
            red("use sk-proj-abcdefghijklmnopqrstuvwx now"),
            "use [secret] now"
        );
        assert_eq!(red("sk-abcdefghijklmnopq"), "[secret]");
        assert_eq!(red("sk-tooshort"), "sk-tooshort");
        assert_eq!(
            red("a desk-lamp-with-a-very-long-name-here"),
            "a desk-lamp-with-a-very-long-name-here"
        );
        assert_eq!(
            red("sk_live_51Habcdefgh and rk_live_abcdefghijkl"),
            "[secret] and [secret]"
        );
        assert_eq!(red("xoxb-123456789012-abcdefghij"), "[secret]");
        assert_eq!(
            red("xoxz-123456789012-abcdefghij"),
            "xoxz-123456789012-abcdefghij"
        );
        assert_eq!(
            red("k=AIzaSyA1234567890abcdefghijklmnopqrstuv!"),
            "k=[secret]!"
        );
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcDEF_-123";
        assert_eq!(red(&format!("Bearer {jwt}")), "Bearer [secret]");
        assert_eq!(
            red("eyJhbGciOiJIUzI1NiJ9 has no dots"),
            "eyJhbGciOiJIUzI1NiJ9 has no dots"
        );
        assert_eq!(redact(&format!("{jwt} {jwt}")).1, 2);
    }

    #[test]
    fn keyword_gate_needs_the_word_the_sign_and_entropy() {
        assert_eq!(red("password = Xk9#mP2$vLq8Zw"), "password = [secret]");
        assert_eq!(red("API_KEY=aB3dE5gH7jK9mN1p"), "API_KEY=[secret]");
        assert_eq!(
            red(r#"{"auth_token": "q1W2e3R4t5Y6u7I8"}"#),
            r#"{"auth_token": "[secret]"}"#
        );
        assert_eq!(red("secret: Zq8Lw3Vx9Tn2Bk7"), "secret: [secret]");
        // No sign, a short value, or a plain word: nothing to hide.
        assert_eq!(
            red("the token is Xk9mP2vLq8Zw3N"),
            "the token is Xk9mP2vLq8Zw3N"
        );
        assert_eq!(red("password = hunter2"), "password = hunter2");
        assert_eq!(red("key: aaaaaaaaaaaaaaaa"), "key: aaaaaaaaaaaaaaaa");
        assert_eq!(red("author: Someone Famous"), "author: Someone Famous");
        // Hebrew around a secret survives byte for byte.
        assert_eq!(
            red("הסיסמה: pwd=Xk9#mP2$vLq8Zw וזהו"),
            "הסיסמה: pwd=[secret] וזהו"
        );
        assert_eq!(red("שלום עולם, הכל עובד"), "שלום עולם, הכל עובד");
    }

    #[test]
    fn hebrew_only_means_no_latin_word() {
        assert!(hebrew_only("זה עובד עכשיו"));
        assert!(!hebrew_only("זה עובד now"));
        assert!(!hebrew_only("works"));
        assert!(!hebrew_only("123"));
    }

    #[test]
    fn a_big_paste_scans_fast() {
        let big = "line of ordinary words and a path /usr/lib/x\n".repeat(5_000);
        let t = std::time::Instant::now();
        let (out, n) = redact(&big);
        assert_eq!((out.len(), n), (big.len(), 0));
        assert!(t.elapsed().as_millis() < 500, "{:?}", t.elapsed());
    }
}

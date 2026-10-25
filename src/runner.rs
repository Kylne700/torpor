//! The check protocol (spec 16): JSON lines, a judge that turns them into PASS, FAIL or UNRUNNABLE, and the Rust runner for http-only checks.
//! The browser runner (S2c) prints the same lines; `judge` does not care who wrote them.
use crate::spec::{Act, HttpExpect, HttpStep, Spec, subst, subst_value};
use serde_json::{Value, json};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub const REQUEST_SECS: u64 = 5;
/// The whole step list gets this long (spec 16: "the whole check gets 30 s").
pub const CHECK_SECS: u64 = 30;
const WAIT_HTTP_SECS: u64 = 10;
const BODY_CAP: u64 = 1 << 20;
const MAX_HOPS: usize = 5;

thread_local! {
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// A cooperative deadline for the runs of this thread (the Stop gate, spec 18). Restores the earlier one when dropped.
/// A run that is cut by it is UNRUNNABLE ("out of time"), never FAIL: running out of time says nothing about the app.
pub struct Deadline(Option<Instant>);

impl Deadline {
    pub fn set(at: Instant) -> Deadline {
        Deadline(DEADLINE.with(|d| d.replace(Some(at))))
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        DEADLINE.with(|d| d.set(self.0));
    }
}

/// Time left until the deadline of this thread (None = no deadline).
pub fn time_left() -> Option<Duration> {
    DEADLINE
        .with(|d| d.get())
        .map(|at| at.saturating_duration_since(Instant::now()))
}

pub fn out_of_time() -> bool {
    time_left().is_some_and(|l| l.is_zero())
}

pub const OUT_OF_TIME: &str = "out of time";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    Unrunnable,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::Unrunnable => "UNRUNNABLE",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Judged {
    pub verdict: Verdict,
    /// 1-based step number of the first failed assertion.
    pub failed_step: Option<usize>,
    pub reason: String,
    pub assertions: usize,
    pub token_ok: bool,
}

fn unrunnable(reason: impl Into<String>, assertions: usize, token_ok: bool) -> Judged {
    Judged {
        verdict: Verdict::Unrunnable,
        failed_step: None,
        reason: reason.into(),
        assertions,
        token_ok,
    }
}

/// Turn the protocol lines of one run into a verdict. `min_n` = the assertion count the promise had when it was saved.
pub fn judge(lines: &[String], nonce: &str, min_n: Option<usize>) -> Judged {
    let (mut started, mut passes) = (false, 0usize);
    let mut fail: Option<(usize, String)> = None;
    let mut error: Option<String> = None;
    let mut done: Option<(u64, String)> = None;
    for l in lines {
        let Ok(v) = serde_json::from_str::<Value>(l) else {
            continue;
        }; // app noise on stdout is not protocol
        match v["p"].as_str() {
            Some("CHECK_START") => started = true,
            Some("ASSERT_PASS") if started => passes += 1,
            Some("ASSERT_FAIL") if started && fail.is_none() => {
                fail = Some((
                    v["i"].as_u64().unwrap_or(0) as usize,
                    v["msg"].as_str().unwrap_or("").to_string(),
                ))
            }
            Some("ERROR") if error.is_none() => {
                error = Some(
                    v["msg"]
                        .as_str()
                        .unwrap_or("the runner reported an error")
                        .to_string(),
                )
            }
            Some("done") => {
                done = Some((
                    v["n"].as_u64().unwrap_or(0),
                    v["token"].as_str().unwrap_or("").to_string(),
                ))
            }
            _ => {}
        }
    }
    if let Some(e) = error {
        return unrunnable(e, passes, false);
    }
    let Some((n, token)) = done else {
        return unrunnable("the check did not finish", passes, false);
    };
    if token != nonce {
        return unrunnable("the check's end marker was wrong", passes, false);
    }
    if !started {
        return unrunnable("the check never started", passes, true);
    }
    if let Some((i, msg)) = fail {
        return Judged {
            verdict: Verdict::Fail,
            failed_step: Some(i),
            reason: msg,
            assertions: passes,
            token_ok: true,
        };
    }
    if n as usize != passes {
        return unrunnable("the check's count did not match its lines", passes, true);
    }
    if let Some(want) = min_n.filter(|w| passes < *w) {
        return Judged {
            verdict: Verdict::Fail,
            failed_step: None,
            reason: format!("weakened: it ran {passes} assertions, the saved check had {want}"),
            assertions: passes,
            token_ok: true,
        };
    }
    Judged {
        verdict: Verdict::Pass,
        failed_step: None,
        reason: String::new(),
        assertions: passes,
        token_ok: true,
    }
}

// ---------------------------------------------------------------- http

pub struct Resp {
    pub status: u16,
    pub body: Vec<u8>,
}

pub enum ReqErr {
    /// The request itself is not allowed or cannot be judged (non-loopback redirect, answer too large).
    Unrunnable(String),
    /// The app did not answer (refused, reset, timed out): a symptom, so a check step fails on it.
    Down(String),
}

pub struct Client {
    agent: ureq::Agent,
    port: u16,
    jar: BTreeMap<String, String>,
}

type UResp = ureq::http::Response<ureq::Body>;

impl Client {
    /// Only 127.0.0.1:`port`. The environment's proxy is switched off, 4xx/5xx are answers, and redirects are followed by hand.
    pub fn new(port: u16) -> Client {
        Client {
            agent: Self::agent(Duration::from_secs(REQUEST_SECS)),
            port,
            jar: BTreeMap::new(),
        }
    }

    fn agent(timeout: Duration) -> ureq::Agent {
        let cfg = ureq::Agent::config_builder()
            .proxy(None)
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(timeout))
            .build();
        cfg.into()
    }

    fn send(
        &self,
        method: &str,
        url: &str,
        body: Option<&str>,
        agent: &ureq::Agent,
    ) -> Result<UResp, ureq::Error> {
        let cookie = self
            .jar
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        macro_rules! prep {
            ($rb:expr) => {{
                let rb = $rb.header("accept", "*/*");
                if cookie.is_empty() {
                    rb
                } else {
                    rb.header("cookie", cookie.as_str())
                }
            }};
        }
        macro_rules! with_body {
            ($rb:expr) => {{
                let rb = prep!($rb);
                match body {
                    Some(b) => rb.header("content-type", "application/json").send(b),
                    None => rb.send_empty(),
                }
            }};
        }
        match method {
            "POST" => with_body!(agent.post(url)),
            "PUT" => with_body!(agent.put(url)),
            "PATCH" => with_body!(agent.patch(url)),
            "DELETE" => prep!(agent.delete(url)).call(),
            "HEAD" => prep!(agent.head(url)).call(),
            _ => prep!(agent.get(url)).call(),
        }
    }

    fn keep_cookies(&mut self, r: &UResp) {
        for v in r.headers().get_all("set-cookie") {
            let Ok(s) = v.to_str() else { continue };
            let first = s.split(';').next().unwrap_or("");
            let Some((k, val)) = first.split_once('=') else {
                continue;
            };
            if val.is_empty()
                || s.to_ascii_lowercase()
                    .replace(' ', "")
                    .contains("max-age=0")
            {
                self.jar.remove(k.trim());
            } else {
                self.jar
                    .insert(k.trim().to_string(), val.trim().to_string());
            }
        }
    }

    /// Where a Location header leads, as a path on our own port; Err for anything else.
    fn follow(&self, loc: &str, from: &str) -> Result<String, ReqErr> {
        if let Some(rest) = loc.strip_prefix("http://") {
            let (auth, path) = rest
                .split_once('/')
                .map_or((rest, String::new()), |(a, p)| (a, format!("/{p}")));
            let (host, port) = auth
                .rsplit_once(':')
                .map_or((auth, 80u16), |(h, p)| (h, p.parse().unwrap_or(0)));
            let host = host.trim_matches(|c| c == '[' || c == ']');
            if !matches!(host, "127.0.0.1" | "localhost" | "::1") && !host.starts_with("127.") {
                return Err(ReqErr::Unrunnable(format!(
                    "non-loopback: the app redirected to {host}"
                )));
            }
            if port != self.port {
                return Err(ReqErr::Unrunnable(format!(
                    "the app redirected to another port ({port}); a check only talks to its own app"
                )));
            }
            Ok(if path.is_empty() { "/".into() } else { path })
        } else if loc.contains("://") || loc.starts_with("//") {
            let host = loc
                .split("//")
                .nth(1)
                .and_then(|r| r.split(['/', ':', '?']).next())
                .unwrap_or("?");
            Err(ReqErr::Unrunnable(format!(
                "non-loopback: the app redirected to {host}"
            )))
        } else if loc.starts_with('/') {
            Ok(loc.to_string())
        } else {
            let dir = from
                .split('?')
                .next()
                .unwrap_or("/")
                .rsplit_once('/')
                .map_or("", |(d, _)| d);
            Ok(format!("{dir}/{loc}"))
        }
    }

    pub fn request(
        &mut self,
        method: &str,
        path: &str,
        json: Option<&Value>,
    ) -> Result<Resp, ReqErr> {
        let (mut method, mut path, mut body) = (
            method.to_string(),
            path.to_string(),
            json.map(Value::to_string),
        );
        for _ in 0..=MAX_HOPS {
            let url = format!("http://127.0.0.1:{}{}", self.port, path);
            // Near a deadline the request gets only the time that is left (a new agent: no kept connection), and what goes wrong then is not the app's fault.
            let left = time_left();
            if left.is_some_and(|l| l.is_zero()) {
                return Err(ReqErr::Unrunnable(OUT_OF_TIME.into()));
            }
            let capped = left.filter(|l| *l < Duration::from_secs(REQUEST_SECS));
            let agent = capped.map_or_else(|| self.agent.clone(), Self::agent);
            let mut r = self
                .send(&method, &url, body.as_deref(), &agent)
                .map_err(|e| match e {
                    ureq::Error::Timeout(_) if capped.is_some() => {
                        ReqErr::Unrunnable(OUT_OF_TIME.into())
                    }
                    _ if out_of_time() => ReqErr::Unrunnable(OUT_OF_TIME.into()),
                    ureq::Error::Timeout(_) => ReqErr::Down(format!(
                        "{method} {path} did not answer within {REQUEST_SECS} s"
                    )),
                    ureq::Error::BodyExceedsLimit(_) => {
                        ReqErr::Unrunnable("the answer is too large to check".into())
                    }
                    other => {
                        ReqErr::Down(format!("{method} {path} could not be reached ({other})"))
                    }
                })?;
            self.keep_cookies(&r);
            let status = r.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                if let Some(loc) = r
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
                {
                    path = self.follow(&loc, &path)?;
                    if status != 307 && status != 308 {
                        method = "GET".into();
                        body = None;
                    }
                    continue;
                }
            }
            let bytes = r
                .body_mut()
                .with_config()
                .limit(BODY_CAP)
                .read_to_vec()
                .map_err(|e| match e {
                    _ if out_of_time() => ReqErr::Unrunnable(OUT_OF_TIME.into()),
                    ureq::Error::BodyExceedsLimit(_) => {
                        ReqErr::Unrunnable("the answer is too large to check".into())
                    }
                    other => ReqErr::Down(format!("{method} {path} broke off ({other})")),
                })?;
            return Ok(Resp {
                status,
                body: bytes,
            });
        }
        Err(ReqErr::Down(format!(
            "{path} redirected more than {MAX_HOPS} times"
        )))
    }
}

fn json_at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let norm = path.replace('[', ".").replace(']', "");
    let mut cur = v;
    for seg in norm.split('.').filter(|s| !s.is_empty()) {
        cur = match cur {
            Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            Value::Object(o) => o.get(seg)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn type_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Ok, or the first expectation that does not hold, in plain words.
pub fn check_expect(
    e: &HttpExpect,
    label: &str,
    r: &Resp,
    rand: &str,
    port: u16,
) -> Result<(), String> {
    if let Some(s) = e.status {
        if r.status != s {
            return Err(format!("{label} answered {}, expected {s}", r.status));
        }
    }
    if !e.status_in.is_empty() && !e.status_in.contains(&r.status) {
        return Err(format!(
            "{label} answered {}, expected one of {:?}",
            r.status, e.status_in
        ));
    }
    if let Some((lo, hi)) = e.length_range {
        let n = r.body.len() as u64;
        if n < lo || n > hi {
            return Err(format!("{label} answered {n} bytes, expected {lo} to {hi}"));
        }
    }
    if let Some(c) = &e.body_contains {
        let want = subst(c, rand, port);
        if !String::from_utf8_lossy(&r.body).contains(&want) {
            return Err(format!("{label} answer does not contain the expected text"));
        }
    }
    if !e.json_has.is_empty() || !e.json_type.is_empty() {
        let Ok(v) = serde_json::from_slice::<Value>(&r.body) else {
            return Err(format!("{label} answer is not JSON"));
        };
        for p in &e.json_has {
            if json_at(&v, p).is_none() {
                return Err(format!("{label} answer has no field {p}"));
            }
        }
        for (p, t) in &e.json_type {
            match json_at(&v, p) {
                None => return Err(format!("{label} answer has no field {p}")),
                Some(x) if type_of(x) != t => {
                    return Err(format!("{label} field {p} is {}, expected {t}", type_of(x)));
                }
                Some(_) => {}
            }
        }
    }
    Ok(())
}

fn line(v: Value) -> String {
    v.to_string()
}

/// Run the steps of an http-only spec against 127.0.0.1:`port`. Returns protocol lines (CHECK_START ... done).
pub fn run_http_steps(spec: &Spec, port: u16, nonce: &str, rand: &str) -> Vec<String> {
    let mut out = vec![line(json!({"p": "CHECK_START"}))];
    let mut c = Client::new(port);
    let started = Instant::now();
    let mut passes = 0usize;
    for (ix, st) in spec.steps.iter().enumerate() {
        let i = ix + 1;
        let fail = |out: &mut Vec<String>, msg: String| {
            out.push(line(json!({"p": "ASSERT_FAIL", "i": i, "msg": msg})))
        };
        if out_of_time() {
            out.push(line(
                json!({"p": "ERROR", "where": "check", "msg": OUT_OF_TIME}),
            ));
            return out;
        }
        if started.elapsed() > Duration::from_secs(CHECK_SECS) {
            fail(&mut out, format!("the check took more than {CHECK_SECS} s"));
            break;
        }
        let res: Result<bool, ReqErr> = match &st.act {
            Act::WaitHttp(p) => {
                let p = subst(p, rand, port);
                let until = Instant::now() + Duration::from_secs(WAIT_HTTP_SECS);
                loop {
                    match c.request("GET", &p, None) {
                        Ok(r) if (200..300).contains(&r.status) => break Ok(true),
                        Err(ReqErr::Unrunnable(m)) => break Err(ReqErr::Unrunnable(m)),
                        _ if Instant::now() >= until => {
                            fail(
                                &mut out,
                                format!("GET {p} did not answer 2xx within {WAIT_HTTP_SECS} s"),
                            );
                            break Ok(false);
                        }
                        _ => std::thread::sleep(Duration::from_millis(250)),
                    }
                }
            }
            Act::Http(h) => run_one(&mut c, h, rand, port).map(|r| match r {
                Ok(()) => {
                    passes += 1;
                    out.push(line(json!({"p": "ASSERT_PASS", "i": i})));
                    true
                }
                Err(msg) => {
                    fail(&mut out, msg);
                    false
                }
            }),
            other => Err(ReqErr::Unrunnable(format!(
                "step {i} ({}) needs a browser",
                other.verb()
            ))),
        };
        match res {
            Ok(true) => {}
            Ok(false) => break,
            Err(ReqErr::Unrunnable(m)) => {
                out.push(line(json!({"p": "ERROR", "where": "check", "msg": m})));
                return out;
            }
            Err(ReqErr::Down(m)) => {
                fail(&mut out, m);
                break;
            }
        }
    }
    out.push(line(json!({"p": "done", "n": passes, "token": nonce})));
    out
}

/// One http assertion: Ok(Ok) holds, Ok(Err(msg)) does not, Err = could not be judged / the app is down.
fn run_one(
    c: &mut Client,
    h: &HttpStep,
    rand: &str,
    port: u16,
) -> Result<Result<(), String>, ReqErr> {
    let path = subst(&h.path, rand, port);
    let body = h.json.as_ref().map(|j| subst_value(j, rand, port));
    let r = c.request(&h.method, &path, body.as_ref())?;
    Ok(check_expect(
        &h.expect,
        &format!("{} {}", h.method, path),
        &r,
        rand,
        port,
    ))
}

/// Random bytes from the system. Err = it gave none: what needs them cannot run (a fixed value would be a guessable one).
fn random_bytes(what: &str, b: &mut [u8]) -> std::io::Result<()> {
    // Tests only (fault-inject builds): `TORPOR_TEST_NO_RANDOM=rand` or `=nonce` makes that one fail.
    #[cfg(feature = "fault-inject")]
    if std::env::var("TORPOR_TEST_NO_RANDOM").is_ok_and(|v| v == what) {
        return Err(std::io::Error::other("none for the test"));
    }
    #[cfg(not(feature = "fault-inject"))]
    let _ = what;
    getrandom::fill(b).map_err(|e| std::io::Error::other(e.to_string()))
}

/// 8 random [a-z0-9] (spec 15 {rand}).
pub fn new_rand() -> std::io::Result<String> {
    let mut b = [0u8; 8];
    random_bytes("rand", &mut b)?;
    Ok(b.iter()
        .map(|x| b"abcdefghijklmnopqrstuvwxyz0123456789"[(*x as usize) % 36] as char)
        .collect())
}

/// A fresh hex nonce for one run (the run token of spec 16).
pub fn new_nonce() -> std::io::Result<String> {
    let mut b = [0u8; 16];
    random_bytes("nonce", &mut b)?;
    Ok(crate::util::hex(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A tiny canned server on 127.0.0.1: path -> (status, extra headers, body). Stops when the test ends (the thread is detached; it only blocks on accept).
    fn serve(routes: Vec<(&'static str, u16, Vec<(&'static str, String)>, &'static str)>) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { continue };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let first = head.lines().next().unwrap_or("").to_string();
                let path = first.split(' ').nth(1).unwrap_or("/").to_string();
                let (mut status, hdrs, body) = routes
                    .iter()
                    .find(|r| r.0 == path)
                    .map(|r| (r.1, r.2.clone(), r.3))
                    .unwrap_or((404, vec![], "nope"));
                // /private only answers a client that sends the cookie the server set.
                if path == "/private" {
                    status = if head.contains("sid=abc") { 200 } else { 401 };
                }
                let mut resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (k, v) in hdrs {
                    resp.push_str(&format!("{k}: {v}\r\n"));
                }
                resp.push_str("\r\n");
                let _ = s.write_all(resp.as_bytes());
                let _ = s.write_all(body.as_bytes());
            }
        });
        port
    }

    fn spec_of(steps: &str) -> Spec {
        let s = format!(
            r#"{{"v":1,"name":"n","why":"w","start":{{"cmd":"x","ready":"/"}},"steps":{steps}}}"#
        );
        spec::lint(&s).unwrap().spec
    }

    fn run(spec: &Spec, port: u16) -> Judged {
        let lines = run_http_steps(spec, port, "nonce1", "abc12345");
        let mut all = vec![r#"{"p":"SETUP_OK"}"#.to_string()];
        all.extend(lines);
        judge(&all, "nonce1", None)
    }

    #[test]
    fn judge_follows_the_protocol_rules() {
        let l = |v: Value| v.to_string();
        let ok = vec![
            l(json!({"p":"SETUP_OK"})),
            l(json!({"p":"CHECK_START"})),
            l(json!({"p":"ASSERT_PASS","i":1})),
            l(json!({"p":"done","n":1,"token":"t"})),
        ];
        assert_eq!(judge(&ok, "t", None).verdict, Verdict::Pass);
        assert_eq!(judge(&ok, "t", Some(1)).verdict, Verdict::Pass);
        let weak = judge(&ok, "t", Some(2));
        assert_eq!(
            (weak.verdict, weak.reason.contains("weakened")),
            (Verdict::Fail, true)
        );
        assert_eq!(
            judge(&ok, "other", None).verdict,
            Verdict::Unrunnable,
            "wrong nonce"
        );
        assert_eq!(
            judge(&ok[..3], "t", None).verdict,
            Verdict::Unrunnable,
            "no done line"
        );
        let mut failing = ok.clone();
        failing.insert(3, l(json!({"p":"ASSERT_FAIL","i":4,"msg":"nope"})));
        let j = judge(&failing, "t", None);
        assert_eq!(
            (j.verdict, j.failed_step, j.reason.as_str()),
            (Verdict::Fail, Some(4), "nope")
        );
        let mut err = ok.clone();
        err.insert(
            1,
            l(json!({"p":"ERROR","where":"setup","msg":"npm exited 1"})),
        );
        assert_eq!(judge(&err, "t", None).verdict, Verdict::Unrunnable);
        // A fail before CHECK_START is a setup problem, not a verdict on the app; noise lines are ignored.
        let early = vec![
            "listening on 3000".into(),
            l(json!({"p":"ASSERT_FAIL","i":1,"msg":"x"})),
            l(json!({"p":"done","n":0,"token":"t"})),
        ];
        assert_eq!(judge(&early, "t", None).verdict, Verdict::Unrunnable);
        // The count must match the lines.
        let lie = vec![
            l(json!({"p":"CHECK_START"})),
            l(json!({"p":"done","n":3,"token":"t"})),
        ];
        assert_eq!(judge(&lie, "t", None).verdict, Verdict::Unrunnable);
    }

    #[test]
    fn http_steps_pass_fail_and_keep_cookies() {
        let port = serve(vec![
            (
                "/api/me",
                200,
                vec![("Content-Type", "application/json".into())],
                r#"{"email":"a@b.test","n":3,"tags":["x"],"user":{"id":"u"}}"#,
            ),
            (
                "/login",
                200,
                vec![("Set-Cookie", "sid=abc; Path=/".into())],
                "ok",
            ),
            ("/gone", 404, vec![], "missing page"),
            ("/home", 302, vec![("Location", "/api/me".into())], ""),
        ]);
        let all = spec_of(
            r#"[{"wait_http":"/login"},
               {"http":{"path":"/api/me"},"expect":{"status":200,"json_has":["email","tags.0","user.id"],"json_type":{"n":"number","tags":"array","email":"string"},"length_range":[10,500]},"why":"me"},
               {"http":{"path":"/gone"},"expect":{"status_in":[404,410],"body_contains":"missing"},"why":"gone"},
               {"http":{"path":"/home"},"expect":{"status":200,"json_has":["email"]},"why":"redirect followed"}]"#,
        );
        let j = run(&all, port);
        assert_eq!((j.verdict, j.assertions), (Verdict::Pass, 3), "{j:?}");

        let bad = spec_of(
            r#"[{"http":{"path":"/api/me"},"expect":{"status":200},"why":"a"},{"http":{"path":"/gone"},"expect":{"status":200},"why":"b"}]"#,
        );
        let j = run(&bad, port);
        assert_eq!((j.verdict, j.failed_step), (Verdict::Fail, Some(2)));
        assert!(j.reason.contains("answered 404"), "{}", j.reason);

        for (exp, want) in [
            (r#"{"json_has":["nope"]}"#, "no field nope"),
            (
                r#"{"json_type":{"email":"number"}}"#,
                "is string, expected number",
            ),
            (r#"{"body_contains":"zzz"}"#, "does not contain"),
            (r#"{"length_range":[1,5]}"#, "bytes, expected 1 to 5"),
        ] {
            let s = spec_of(&format!(
                r#"[{{"http":{{"path":"/api/me"}},"expect":{exp},"why":"w"}}]"#
            ));
            let j = run(&s, port);
            assert!(
                j.verdict == Verdict::Fail && j.reason.contains(want),
                "{exp}: {j:?}"
            );
        }
    }

    #[test]
    fn cookies_go_back_and_a_dead_app_fails_the_step() {
        let port = serve(vec![
            (
                "/login",
                200,
                vec![("Set-Cookie", "sid=abc; Path=/".into())],
                "ok",
            ),
            ("/private", 200, vec![], "ok"),
        ]);
        let mut c = Client::new(port);
        assert_eq!(c.request("GET", "/private", None).ok().unwrap().status, 401);
        c.request("GET", "/login", None).ok().unwrap();
        assert_eq!(
            c.request("GET", "/private", None).ok().unwrap().status,
            200,
            "the cookie went back"
        );

        // Nothing listens on this port any more: connection refused is a FAIL at the step, not an error.
        let dead = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let s = spec_of(r#"[{"http":{"path":"/"},"expect":{"status":200},"why":"up"}]"#);
        let j = run(&s, dead);
        assert_eq!(
            (j.verdict, j.failed_step),
            (Verdict::Fail, Some(1)),
            "{j:?}"
        );
    }

    #[test]
    fn a_redirect_off_this_computer_or_to_another_port_is_unrunnable() {
        let other = serve(vec![]);
        let port = serve(vec![
            (
                "/off",
                302,
                vec![("Location", "http://example.org/x".into())],
                "",
            ),
            (
                "/off2",
                302,
                vec![("Location", "//example.org/x".into())],
                "",
            ),
            (
                "/port",
                302,
                vec![("Location", format!("http://127.0.0.1:{other}/x"))],
                "",
            ),
        ]);
        for (p, want) in [
            ("/off", "non-loopback"),
            ("/off2", "non-loopback"),
            ("/port", "another port"),
        ] {
            let s = spec_of(&format!(
                r#"[{{"http":{{"path":"{p}"}},"expect":{{"status":200}},"why":"w"}}]"#
            ));
            let j = run(&s, port);
            assert!(
                j.verdict == Verdict::Unrunnable && j.reason.contains(want),
                "{p}: {j:?}"
            );
        }
    }
}

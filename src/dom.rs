//! The browser runner (spec 16, 24). A check with page steps runs in `node run.mjs` (playwright-core, a Chromium that is already on the
//! computer): the spec goes in on stdin, the same protocol lines come back as from the Rust http runner, and `runner::judge` scores them.
//! `torpor runner install` is the one download Torpor makes, and only after you confirm.
use crate::runner::{self, Client, OUT_OF_TIME, ReqErr};
use crate::scratch::{App, Opts, Run};
use crate::spec::{self, Act, Count, Loc, Spec, Step, UrlWant};
use crate::store::mkdir_private;
use crate::util::{Error, Result, trunc_chars};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// The exact playwright-core Torpor installs, and the hash npm publishes for that package (checked before anything is unpacked).
pub const PW_VERSION: &str = "1.64.1";
pub const PW_INTEGRITY: &str = "sha512-BC4s8y6SEVA6Q1rSxZTto8hI1KUI3xXlHGZgvGqP4IqPdI/a6FX9H29IDNz7vKo9l21b6LfYosScPL6M3xxyDQ==";
const REGISTRY: &str = "https://registry.npmjs.org";
const RUN_MJS: &str = include_str!("../assets/runner/run.mjs");
const INSTALL_MJS: &str = include_str!("../assets/runner/install.mjs");
/// Spec 16: the start is warmed up (one GET per page path) within this long.
const WARM_UP_SECS: u64 = 30;
/// The runner's own limits add up to about 70 s (browser start 30, the check 30 plus one step, closing 3); this is later than all of them.
const RUNNER_SECS: u64 = 90;
const CHROMIUM_NAMES: [&str; 7] = ["chromium", "chromium-browser", "google-chrome", "google-chrome-stable", "brave-browser", "microsoft-edge", "microsoft-edge-stable"];
const MAC_BROWSERS: [&str; 4] = [
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
];
const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// `<TORPOR_HOME>/runner/<version>/`: the only place the installer writes.
pub fn runner_dir(home: &Path) -> PathBuf {
    home.join("runner").join(PW_VERSION)
}

/// The package folder holds a package.json only once the installer has moved the checked package into place (its last step).
pub fn installed(dir: &Path) -> bool {
    dir.join("node_modules/playwright-core/package.json").is_file()
}

/// Playwright's own Chromium (the download path): any `chromium-*` folder in the runner's browser folder.
pub fn own_browser(dir: &Path) -> bool {
    fs::read_dir(dir.join("browsers")).is_ok_and(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().starts_with("chromium")))
}

fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default()
}

fn is_exec(p: &Path) -> bool {
    fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn find_in(dirs: &[PathBuf], names: &[&str]) -> Option<PathBuf> {
    names.iter().find_map(|n| dirs.iter().map(|d| d.join(n)).find(|p| is_exec(p)))
}

pub fn find_node() -> Option<PathBuf> {
    find_in(&path_dirs(), &["node"])
}

/// A Chromium-family browser already on the computer (spec 16): on the PATH, or an app bundle on macOS.
pub fn find_chromium() -> Option<PathBuf> {
    // Tests only: pretend there is none, to reach the code that downloads one (with a stand-in package that downloads nothing).
    #[cfg(feature = "fault-inject")]
    if std::env::var_os("TORPOR_TEST_NO_BROWSER").is_some() {
        return None;
    }
    find_chromium_in(&path_dirs(), &MAC_BROWSERS)
}

fn find_chromium_in(dirs: &[PathBuf], bundles: &[&str]) -> Option<PathBuf> {
    find_in(dirs, &CHROMIUM_NAMES).or_else(|| bundles.iter().map(PathBuf::from).find(|p| is_exec(p)))
}

/// Before the app is started: can page steps run at all? Err = the UNRUNNABLE reason.
pub fn preflight(home: &Path, opts: &Opts) -> std::result::Result<(), String> {
    if opts.runner.is_none() && !installed(&runner_dir(home)) {
        return Err("runner not installed: torpor runner install".into());
    }
    if find_node().is_none() {
        return Err("Node.js is not installed, so page (browser) steps cannot run".into());
    }
    Ok(())
}

// ---------------------------------------------------------------- the run

fn loc_json(l: &Loc) -> Value {
    match l {
        Loc::Role(r, Some(n)) => json!({"k": "role", "v": r, "name": n}),
        Loc::Role(r, None) => json!({"k": "role", "v": r}),
        Loc::Label(s) => json!({"k": "label", "v": s}),
        Loc::Text(s) => json!({"k": "text", "v": s}),
        Loc::TextRe(s) => json!({"k": "text_re", "v": s}),
        Loc::TestId(s) => json!({"k": "testid", "v": s}),
    }
}

/// One step as the runner reads it (`i` is the 1-based step number the protocol lines use). {rand} and {port} are still in the strings.
pub fn step_json(i: usize, st: &Step) -> Value {
    let (op, mut o) = match &st.act {
        Act::Goto(p) => ("goto", json!({"path": p})),
        Act::Click(l) => ("click", json!({"loc": loc_json(l)})),
        Act::Fill(l, v) => ("fill", json!({"loc": loc_json(l), "value": v})),
        Act::Press(l, k) => ("press", json!({"loc": loc_json(l), "key": k})),
        Act::Select(l, v) => ("select", json!({"loc": loc_json(l), "value": v})),
        Act::Check(l) => ("check", json!({"loc": loc_json(l)})),
        Act::Uncheck(l) => ("uncheck", json!({"loc": loc_json(l)})),
        Act::WaitHttp(p) => ("wait_http", json!({"path": p})),
        Act::Visible(l) => ("expect_visible", json!({"loc": loc_json(l)})),
        Act::Hidden(l) => ("expect_hidden", json!({"loc": loc_json(l)})),
        Act::Text(l, c) => ("expect_text", json!({"loc": loc_json(l), "contains": c})),
        Act::Url(UrlWant::Path(p)) => ("expect_url", json!({"path": p})),
        Act::Url(UrlWant::PathRe(p)) => ("expect_url", json!({"path_re": p})),
        Act::Count(l, Count::N(n)) => ("expect_count", json!({"loc": loc_json(l), "n": n})),
        Act::Count(l, Count::Range(lo, hi)) => ("expect_count", json!({"loc": loc_json(l), "min": lo, "max": hi})),
        Act::ConsoleClean => ("expect_console_clean", json!({})),
        Act::Http(h) => {
            let e = &h.expect;
            let expect = json!({"status": e.status, "status_in": e.status_in, "json_has": e.json_has, "json_type": e.json_type, "body_contains": e.body_contains, "length_range": e.length_range});
            let mut o = json!({"method": h.method, "path": h.path, "expect": expect});
            if let Some(j) = &h.json {
                o["json"] = j.clone();
            }
            ("http", o)
        }
    };
    o["i"] = json!(i);
    o["op"] = json!(op);
    o
}

/// What goes to the runner on stdin: the steps with {rand} and {port} filled in, the run's port and nonce, and the Chromium to use (null = Playwright's own).
pub fn payload(spec: &Spec, port: u16, rand: &str, nonce: &str, chromium: Option<&Path>) -> Vec<u8> {
    let steps: Vec<Value> = spec.steps.iter().enumerate().map(|(ix, st)| spec::subst_value(&step_json(ix + 1, st), rand, port)).collect();
    json!({"nonce": nonce, "port": port, "chromium": chromium.map(|c| c.to_string_lossy().into_owned()), "steps": steps}).to_string().into_bytes()
}

/// Spec 16: one GET per distinct page path before the check, within 30 s in total, so a first-request compile or a cold start is not the
/// check's fault. An app that is down, or answers 5xx, before the first assertion is a setup failure (UNRUNNABLE), not a FAIL.
fn warm_up(spec: &Spec, port: u16, rand: &str) -> std::result::Result<(), String> {
    let mut paths: Vec<String> = Vec::new();
    for p in spec.goto_paths() {
        let p = spec::subst(&p, rand, port);
        if !paths.contains(&p) {
            paths.push(p);
        }
    }
    let (end, mut c) = (Instant::now() + Duration::from_secs(WARM_UP_SECS), Client::new(port));
    for p in paths {
        if Instant::now() >= end {
            break;
        }
        match c.request("GET", &p, None) {
            Ok(r) if r.status >= 500 => return Err(format!("GET {p} answered {} before the check started", r.status)),
            Ok(_) => {}
            Err(ReqErr::Down(m)) => return Err(format!("the app did not answer before the check started ({m})")),
            Err(ReqErr::Unrunnable(m)) => return Err(m),
        }
    }
    Ok(())
}

/// Make `<dir>/run.mjs` the runner this build carries (a newer Torpor may bring a newer one); written whole, then moved into place.
pub fn refresh_script(dir: &Path) -> std::io::Result<PathBuf> {
    let p = dir.join("run.mjs");
    if fs::read(&p).is_ok_and(|b| b == RUN_MJS.as_bytes()) {
        return Ok(p);
    }
    let tmp = dir.join(format!(".run-{}.tmp", std::process::id()));
    fs::write(&tmp, RUN_MJS)?;
    fs::rename(&tmp, &p)?;
    Ok(p)
}

/// A short temp folder for the browser: Chromium puts a unix socket in it, and a socket path cannot be longer than about 100 characters,
/// which a folder inside the scratch copy would be. Not the user's temp folder when that is long (macOS). Removed when dropped.
struct ShortTmp(PathBuf);

impl ShortTmp {
    fn new() -> std::io::Result<ShortTmp> {
        let base = std::env::temp_dir();
        let base = if base.as_os_str().len() <= 40 { base } else { PathBuf::from("/tmp") };
        let p = base.join(format!("tp-{}", runner::new_rand()));
        mkdir_private(&p)?;
        Ok(ShortTmp(p))
    }
}

impl Drop for ShortTmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn error_line(where_: &str, msg: &str) -> String {
    json!({"p": "ERROR", "where": where_, "msg": msg}).to_string()
}

/// Protocol lines only, and the text of their messages without secrets or long runs (a page can print anything into a console error).
fn read_lines(log: &Path) -> Vec<String> {
    let raw = fs::read(log).unwrap_or_default();
    String::from_utf8_lossy(&raw)
        .lines()
        .filter(|l| l.starts_with("{\"p\":"))
        .map(|l| match serde_json::from_str::<Value>(l) {
            Ok(mut v) if v["msg"].is_string() => {
                let (red, _) = crate::texts::redact(v["msg"].as_str().unwrap_or(""));
                v["msg"] = Value::String(trunc_chars(&crate::mask::mask_runs(&red), 300));
                v.to_string()
            }
            _ => l.to_string(),
        })
        .collect()
}

/// Run the steps of a spec with page steps against the started app. Returns protocol lines (SETUP_OK ... done, or an ERROR line).
pub fn run_steps(home: &Path, spec: &Spec, run: &Run, opts: &Opts) -> Vec<String> {
    let dir = runner_dir(home);
    if let Err(m) = warm_up(spec, run.port, &run.rand) {
        return vec![error_line("setup", &m)];
    }
    let script = match &opts.runner {
        Some(s) => s.clone(),
        None => match refresh_script(&dir) {
            Ok(p) => p,
            Err(e) => return vec![error_line("setup", &format!("the browser runner could not be prepared ({e})"))],
        },
    };
    let Some(node) = find_node() else { return vec![error_line("setup", "Node.js is not installed, so page (browser) steps cannot run")] };
    let tmp = match ShortTmp::new() {
        Ok(t) => t,
        Err(e) => return vec![error_line("setup", &format!("no temporary folder for the browser ({e})"))],
    };
    // Exactly these four (spec 16 names three; TMPDIR is the short folder above). No DISPLAY, no proxy, nothing of the user's environment.
    let path = std::env::var("PATH").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| DEFAULT_PATH.into());
    let env = vec![
        ("PATH".to_string(), path),
        ("HOME".to_string(), run.dir().join("home").to_string_lossy().into_owned()),
        ("TMPDIR".to_string(), tmp.0.to_string_lossy().into_owned()),
        ("PLAYWRIGHT_BROWSERS_PATH".to_string(), dir.join("browsers").to_string_lossy().into_owned()),
    ];
    let mut c = Command::new(&node);
    c.arg(&script);
    let log = run.dir().join("runner.log");
    let input = payload(spec, run.port, &run.rand, &run.nonce, find_chromium().as_deref());
    let mut app = match App::launch(c, run.dir(), &env, log.clone(), Some(input)) {
        Ok(a) => a,
        Err(e) => return vec![error_line("setup", &format!("the browser runner could not be started ({e})"))],
    };
    let ended = app.wait_end(Duration::from_secs(RUNNER_SECS));
    if !ended {
        app.stop();
    }
    app.wait_eof();
    let mut lines = read_lines(&log);
    if !ended && !lines.iter().any(|l| l.starts_with("{\"p\":\"done\"")) {
        let msg = if runner::out_of_time() { OUT_OF_TIME } else { "the browser runner did not finish" };
        lines.push(error_line("check", msg));
    }
    lines
}

// ---------------------------------------------------------------- torpor runner install

/// Where playwright-core comes from, and the hash it must have. Plain builds have exactly one answer; builds with the test feature may
/// point at a registry on 127.0.0.1 (and a different hash), so no test can ever reach the real registry.
fn source() -> Result<(String, String)> {
    #[cfg(feature = "fault-inject")]
    if let Ok(r) = std::env::var("TORPOR_TEST_REGISTRY") {
        if !r.starts_with("http://127.0.0.1:") {
            return Err(Error::msg("TORPOR_TEST_REGISTRY must be an http://127.0.0.1:PORT address"));
        }
        return Ok((r, std::env::var("TORPOR_TEST_INTEGRITY").unwrap_or_else(|_| PW_INTEGRITY.into())));
    }
    Ok((REGISTRY.to_string(), PW_INTEGRITY.to_string()))
}

/// What the installer will download, in the words of the confirmation question (spec 24).
pub fn prompt(dir: &Path, need_package: bool, need_browser: bool) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if need_package {
        parts.push("about 3 MB (playwright-core from the npm registry)");
    }
    if need_browser {
        parts.push("about 150 MB more for a Chromium from Playwright's servers, because none was found on this computer");
    }
    format!("The browser runner needs {}. Install into {}? [y/N]", parts.join(", plus "), dir.display())
}

/// The environment of a download: the PATH, and only what node needs to reach a proxy or a private certificate authority.
fn download_env(home: &Path) -> Vec<(String, String)> {
    let mut env = vec![("PATH".to_string(), std::env::var("PATH").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| DEFAULT_PATH.into())), ("HOME".to_string(), home.to_string_lossy().into_owned())];
    for k in ["NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE", "SSL_CERT_DIR", "HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY", "NODE_USE_ENV_PROXY"] {
        if let Some(v) = std::env::var_os(k).filter(|v| !v.is_empty()) {
            env.push((k.to_string(), v.to_string_lossy().into_owned()));
        }
    }
    env
}

/// `node playwright-core/cli.js install chromium`, into the runner's browser folder. NOT VERIFIED: the code is here because the spec asks for
/// it; it has never been run (the tests use a Chromium that is already installed, and a stand-in cli.js).
pub fn chromium_command(dir: &Path, node: &Path) -> Command {
    let mut c = Command::new(node);
    c.arg(dir.join("node_modules/playwright-core/cli.js")).arg("install").arg("chromium").current_dir(dir).env_clear();
    c.envs(download_env(&dir.join("home")));
    c.env("PLAYWRIGHT_BROWSERS_PATH", dir.join("browsers"));
    c
}

/// Run a node command to its end. Ok = its last line of output.
fn run_node(mut c: Command, what: &str) -> Result<String> {
    let out = c.output().map_err(|e| Error::msg(format!("{what}: node could not be started ({e})")))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string());
    }
    let text = String::from_utf8_lossy(&[out.stderr, out.stdout].concat()).into_owned();
    let last = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("it failed without a message");
    Err(Error::msg(format!("{what}: {}", trunc_chars(last.trim(), 300))))
}

/// Install playwright-core (when `need_package`) and then Chromium (when `need_browser`) into `dir`, which the caller asked the user about.
/// The package is checked against its published hash before it is unpacked; no npm, no install script runs.
pub fn install(dir: &Path, node: &Path, need_package: bool, need_browser: bool) -> Result<String> {
    mkdir_private(dir)?;
    let mut said = String::new();
    if need_package {
        let (registry, integrity) = source()?;
        let installer = dir.join("install.mjs");
        fs::write(&installer, INSTALL_MJS)?;
        let mut c = Command::new(node);
        c.arg(&installer).arg(dir).arg(PW_VERSION).arg(&integrity).arg(&registry).current_dir(dir).env_clear().envs(download_env(dir));
        let line = run_node(c, "the download failed")?;
        fs::write(dir.join("package.json"), format!("{}\n", serde_json::to_string_pretty(&json!({"name": "torpor-runner", "version": "1.0.0", "private": true, "dependencies": {"playwright-core": PW_VERSION}})).unwrap_or_default()))?;
        let lock = json!({
            "name": "torpor-runner", "version": "1.0.0", "lockfileVersion": 3, "requires": true,
            "packages": {
                "": {"name": "torpor-runner", "version": "1.0.0", "dependencies": {"playwright-core": PW_VERSION}},
                "node_modules/playwright-core": {
                    "version": PW_VERSION,
                    "resolved": format!("{REGISTRY}/playwright-core/-/playwright-core-{PW_VERSION}.tgz"),
                    "integrity": PW_INTEGRITY, "license": "Apache-2.0", "bin": {"playwright-core": "cli.js"}, "engines": {"node": ">=20"}
                }
            }
        });
        fs::write(dir.join("package-lock.json"), format!("{}\n", serde_json::to_string_pretty(&lock).unwrap_or_default()))?;
        said.push_str(&format!("{}\nInto {}.\n", if line.is_empty() { format!("Installed playwright-core {PW_VERSION}") } else { line }, dir.display()));
    }
    refresh_script(dir)?;
    if need_browser {
        mkdir_private(&dir.join("home"))?;
        mkdir_private(&dir.join("browsers"))?;
        run_node(chromium_command(dir, node), "the Chromium download failed")?;
        said.push_str("Installed Chromium for the browser runner.\n");
    }
    Ok(said)
}

/// Node 20 or newer (playwright-core needs it). Err = the sentence to show.
pub fn node_ok(node: &Path) -> std::result::Result<(), String> {
    let out = Command::new(node).arg("--version").env_clear().env("PATH", std::env::var_os("PATH").unwrap_or_default()).output().map_err(|e| format!("Node.js could not be started ({e})"))?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match v.trim_start_matches('v').split('.').next().and_then(|m| m.parse::<u32>().ok()) {
        Some(m) if m >= 20 => Ok(()),
        _ => Err(format!("Node.js 20 or newer is needed (this one says {})", if v.is_empty() { "nothing" } else { &v })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linted(steps: &str) -> Spec {
        let raw = format!(r#"{{"v":1,"name":"page check","why":"the page works","start":{{"cmd":"node server.js --port {{port}}","ready":"/login","timeout_s":30}},"env":{{}},"steps":{steps}}}"#);
        spec::lint(&raw).unwrap_or_else(|p| panic!("lint: {p:?}")).spec
    }

    #[test]
    fn every_step_reaches_the_runner_in_one_shape_with_its_number_and_the_port_filled_in() {
        let s = linted(
            r#"[{"goto":"/dom?x={rand}"},{"expect_visible":{"role":"heading","name":"Account"},"why":"the title"},{"fill":{"label":"Email"},"value":"kim-the-tester"},
            {"click":{"role":"button"}},{"press":{"label":"Email"},"key":"Enter"},{"select":{"label":"Size"},"value":"big"},{"check":{"text":"Agree"}},{"uncheck":{"text":"Agree"}},
            {"wait_http":"/api/me"},{"expect_hidden":{"text_re":"^Error"},"why":"no error"},{"expect_text":{"testid":"out"},"contains":"hi","why":"it greets"},
            {"expect_url":{"path":"/dom"},"why":"stays"},{"expect_url":{"path_re":"^/d"},"why":"stays"},{"expect_count":{"text":"Sign in"},"n":1,"why":"one button"},
            {"expect_count":{"text":"Sign in"},"min":1,"why":"a button"},{"expect_console_clean":true,"why":"no errors"},
            {"http":{"method":"POST","path":"/api/login","json":{"email":"kim-the-tester"}},"expect":{"status_in":[200,201],"json_has":["ok"],"json_type":{"ok":"boolean"},"body_contains":"ok","length_range":[1,50]},"why":"signs in"}]"#,
        );
        let v: Value = serde_json::from_slice(&payload(&s, 4321, "r1", "nonce1", Some(Path::new("/usr/bin/chromium")))).unwrap();
        assert_eq!((v["nonce"].as_str(), v["port"].as_u64(), v["chromium"].as_str()), (Some("nonce1"), Some(4321), Some("/usr/bin/chromium")));
        let steps = v["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 17);
        for (ix, st) in steps.iter().enumerate() {
            assert_eq!(st["i"].as_u64(), Some(ix as u64 + 1));
        }
        let ops: Vec<&str> = steps.iter().map(|s| s["op"].as_str().unwrap()).collect();
        assert_eq!(
            ops,
            ["goto", "expect_visible", "fill", "click", "press", "select", "check", "uncheck", "wait_http", "expect_hidden", "expect_text", "expect_url", "expect_url", "expect_count", "expect_count", "expect_console_clean", "http"]
        );
        assert_eq!(steps[0]["path"], "/dom?x=r1", "{{rand}} is filled in");
        assert_eq!(steps[1]["loc"], json!({"k": "role", "v": "heading", "name": "Account"}));
        assert_eq!(steps[2]["loc"], json!({"k": "label", "v": "Email"}));
        assert_eq!(steps[2]["value"], "kim-the-tester");
        assert_eq!(steps[4]["key"], "Enter");
        assert_eq!(steps[9]["loc"], json!({"k": "text_re", "v": "^Error"}));
        assert_eq!(steps[10]["loc"], json!({"k": "testid", "v": "out"}));
        assert_eq!(steps[10]["contains"], "hi");
        assert_eq!((steps[11]["path"].as_str(), steps[12]["path_re"].as_str()), (Some("/dom"), Some("^/d")));
        assert_eq!((steps[13]["n"].as_u64(), steps[13]["loc"]["k"].as_str()), (Some(1), Some("text")));
        assert_eq!((steps[14]["min"].as_u64(), steps[14]["max"].clone()), (Some(1), Value::Null));
        let h = &steps[16];
        assert_eq!((h["method"].as_str(), h["path"].as_str(), h["json"].clone()), (Some("POST"), Some("/api/login"), json!({"email": "kim-the-tester"})));
        assert_eq!(h["expect"]["status_in"], json!([200, 201]));
        assert_eq!(h["expect"]["json_type"], json!([["ok", "boolean"]]));
        assert_eq!(h["expect"]["length_range"], json!([1, 50]));
        assert!(steps[0].get("json").is_none() && h["expect"]["status"].is_null());
        let none: Value = serde_json::from_slice(&payload(&s, 1, "r", "n", None)).unwrap();
        assert!(none["chromium"].is_null(), "no system Chromium: the runner uses its own");
    }

    #[test]
    fn a_chromium_is_found_by_name_on_the_path_or_in_an_app_bundle_and_only_when_it_can_run() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let mk = |p: &Path, mode: u32| {
            std::fs::write(p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        mk(&a.join("chromium"), 0o644); // not executable
        mk(&b.join("google-chrome"), 0o755);
        mk(&a.join("brave-browser"), 0o755);
        let dirs = vec![a.clone(), b.clone()];
        assert_eq!(find_chromium_in(&dirs, &[]), Some(b.join("google-chrome")), "names are tried in order, directories within a name");
        mk(&b.join("chromium"), 0o755);
        assert_eq!(find_chromium_in(&dirs, &[]), Some(b.join("chromium")));
        std::fs::remove_file(b.join("chromium")).unwrap();
        std::fs::remove_file(b.join("google-chrome")).unwrap();
        assert_eq!(find_chromium_in(&dirs, &[]), Some(a.join("brave-browser")));
        std::fs::remove_file(a.join("brave-browser")).unwrap();
        assert_eq!(find_chromium_in(&dirs, &[]), None);
        let bundle = tmp.path().join("Chromium");
        mk(&bundle, 0o755);
        assert_eq!(find_chromium_in(&dirs, &[bundle.to_str().unwrap()]), Some(bundle));
        assert_eq!(find_in(&dirs, &["node"]), None);
    }

    #[test]
    fn the_runner_script_is_written_whole_and_only_when_it_differs() {
        let tmp = tempfile::tempdir().unwrap();
        let p = refresh_script(tmp.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), RUN_MJS);
        std::fs::write(&p, "old runner").unwrap();
        assert_eq!(refresh_script(tmp.path()).unwrap(), p);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), RUN_MJS, "a newer Torpor brings its own runner");
        let before = std::fs::metadata(&p).unwrap().modified().unwrap();
        refresh_script(tmp.path()).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().modified().unwrap(), before, "unchanged bytes are not rewritten");
        let names: Vec<String> = std::fs::read_dir(tmp.path()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["run.mjs"], "no temp file is left");
    }

    #[test]
    fn installed_means_the_checked_package_is_in_place_and_a_browser_folder_means_playwrights_own_chromium() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = runner_dir(tmp.path());
        assert!(dir.ends_with("runner/1.64.1"));
        assert!(!installed(&dir) && !own_browser(&dir));
        std::fs::create_dir_all(dir.join("node_modules/playwright-core")).unwrap();
        assert!(!installed(&dir), "a folder without its package.json is not an install");
        std::fs::write(dir.join("node_modules/playwright-core/package.json"), "{}").unwrap();
        assert!(installed(&dir));
        std::fs::create_dir_all(dir.join("browsers/ffmpeg-1")).unwrap();
        assert!(!own_browser(&dir));
        std::fs::create_dir_all(dir.join("browsers/chromium-1234")).unwrap();
        assert!(own_browser(&dir));
    }

    #[test]
    fn the_question_names_what_is_downloaded_from_where_and_the_chromium_download_runs_in_the_runner_folder_alone() {
        let dir = Path::new("/srv/h/runner/1.64.1");
        let q = prompt(dir, true, false);
        assert!(q.starts_with("The browser runner needs about 3 MB (playwright-core from the npm registry).") && q.ends_with("Install into /srv/h/runner/1.64.1? [y/N]"), "{q}");
        assert!(!q.contains("Chromium"));
        let q = prompt(dir, true, true);
        assert!(q.contains("plus about 150 MB more for a Chromium from Playwright's servers, because none was found"), "{q}");
        assert!(prompt(dir, false, true).starts_with("The browser runner needs about 150 MB more for a Chromium from Playwright's servers"));
        let c = chromium_command(dir, Path::new("/usr/bin/node"));
        assert_eq!(c.get_program(), "/usr/bin/node");
        let args: Vec<String> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["/srv/h/runner/1.64.1/node_modules/playwright-core/cli.js", "install", "chromium"]);
        assert_eq!(c.get_current_dir(), Some(dir));
        let envs: std::collections::BTreeMap<String, String> = c.get_envs().filter_map(|(k, v)| Some((k.to_string_lossy().into_owned(), v?.to_string_lossy().into_owned()))).collect();
        assert_eq!(envs["PLAYWRIGHT_BROWSERS_PATH"], "/srv/h/runner/1.64.1/browsers");
        assert_eq!(envs["HOME"], "/srv/h/runner/1.64.1/home", "the download never touches the real home");
        assert!(envs.keys().all(|k| ["PATH", "HOME", "PLAYWRIGHT_BROWSERS_PATH", "NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE", "SSL_CERT_DIR", "HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY", "NODE_USE_ENV_PROXY"].contains(&k.as_str())), "{envs:?}");
    }

    #[test]
    fn what_the_runner_prints_is_cleaned_before_it_is_judged() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("runner.log");
        let long = "x".repeat(900);
        std::fs::write(
            &log,
            format!(
                "node warning: noise\n{{\"p\":\"SETUP_OK\"}}\n{{\"p\":\"ASSERT_FAIL\",\"i\":3,\"msg\":\"sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA leaked {long}\"}}\n{{\"not\":\"protocol\"}}\n{{\"p\":\"done\",\"n\":0,\"token\":\"t\"}}\n"
            ),
        )
        .unwrap();
        let lines = read_lines(&log);
        assert_eq!(lines.len(), 3, "{lines:?}");
        let fail: Value = serde_json::from_str(&lines[1]).unwrap();
        let msg = fail["msg"].as_str().unwrap();
        assert!(!msg.contains("sk-ant-api03") && msg.chars().count() <= 301, "{msg}");
        assert_eq!(fail["i"], 3);
        assert!(read_lines(&tmp.path().join("missing.log")).is_empty());
    }

    #[test]
    fn the_embedded_scripts_have_valid_syntax() {
        let Some(node) = find_node() else { return };
        let tmp = tempfile::tempdir().unwrap();
        for (name, text) in [("run.mjs", RUN_MJS), ("install.mjs", INSTALL_MJS)] {
            let p = tmp.path().join(name);
            std::fs::write(&p, text).unwrap();
            let out = Command::new(&node).arg("--check").arg(&p).output().unwrap();
            assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
        }
    }

    #[test]
    fn the_short_temp_folder_is_short_private_and_removed() {
        let t = ShortTmp::new().unwrap();
        let p = t.0.clone();
        assert!(p.is_dir() && p.as_os_str().len() <= 60, "{p:?}");
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o700);
        drop(t);
        assert!(!p.exists());
    }

    #[test]
    fn node_20_or_newer_is_needed() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = |v: &str| {
            let p = tmp.path().join(format!("node-{}", v.replace('.', "-")));
            std::fs::write(&p, format!("#!/bin/sh\necho {v}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        assert!(node_ok(&fake("v20.0.0")).is_ok() && node_ok(&fake("v24.1.0")).is_ok());
        let e = node_ok(&fake("v18.19.1")).unwrap_err();
        assert!(e.contains("Node.js 20 or newer") && e.contains("v18.19.1"), "{e}");
        assert!(node_ok(&fake("garbage")).is_err());
    }
}

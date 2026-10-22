//! The browser runner without a browser (spec 16, 24): a stand-in script prints protocol lines in place of run.mjs, so these tests check
//! what Torpor does around it: what goes in on stdin and in the environment, how the lines are judged, the time limit and the warm-up.
//! Needs `node` (the fixture app and the stand-ins are Node files). The run with a real Chromium is the ignored test at the end.
mod common;
use common::{manifest_dir, write_files};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use torpor::runner::{Deadline, OUT_OF_TIME, Verdict};
use torpor::scratch::{self, Opts, RunResult, run_once_with};
use torpor::store::Ws;

struct Fx {
    tmp: tempfile::TempDir,
    proj: PathBuf,
    ws: Ws,
}

fn fixture() -> Fx {
    let tmp = tempfile::Builder::new().prefix("torpor-dom").tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    write_files(&proj, &[]);
    std::fs::copy(manifest_dir().join("tests/fixtures/app/server.js"), proj.join("server.js")).unwrap();
    let ws = Ws::open_in(&tmp.path().join("thome"), &proj, true).unwrap();
    Fx { tmp, proj, ws }
}

const START: &str = r#"{"cmd":"node server.js --port {port}","ready":"/login","timeout_s":30}"#;
/// Steps 2 and 5 are assertions.
const DOM_STEPS: &str = r#"[
  {"goto":"/login"},
  {"expect_visible":{"text":"Email"},"why":"the form shows"},
  {"fill":{"label":"Email"},"value":"kim-the-tester"},
  {"click":{"role":"button","name":"Sign in"}},
  {"expect_url":{"path":"/login"},"why":"the page stays"}]"#;
/// What every stand-in starts with: the payload from stdin, and a way to print a protocol line.
const PRE: &str = "const fs = require('fs'); const p = JSON.parse(fs.readFileSync(0, 'utf8')); const o = (x) => console.log(JSON.stringify(x)); const T = p.nonce;\n";

fn spec_of(steps: &str, env: &str) -> torpor::spec::Spec {
    let raw = format!(r#"{{"v":1,"name":"page check","why":"the page works","start":{START},"env":{env},"steps":{steps}}}"#);
    torpor::spec::lint(&raw).unwrap_or_else(|p| panic!("lint: {p:?}")).spec
}

impl Fx {
    /// Run `DOM_STEPS`-like steps with a stand-in runner whose script is `PRE` + `body`.
    fn go(&mut self, steps: &str, env: &str, body: &str, min_n: Option<usize>) -> RunResult {
        let script = self.tmp.path().join("fake-runner.js");
        std::fs::write(&script, format!("{PRE}{body}")).unwrap();
        let tree = scratch::snapshot(&mut self.ws, Duration::from_secs(20)).unwrap();
        let opts = Opts { runner: Some(script), ..Opts::default() };
        let r = run_once_with(&self.ws, &spec_of(steps, env), &tree, min_n, &opts);
        let scratch_dir = self.ws.dir.join("scratch");
        assert_eq!(std::fs::read_dir(&scratch_dir).map(|d| d.count()).unwrap_or(0), 0, "the scratch folder is removed after the run");
        r
    }
}

const PASS: &str = "o({p:'SETUP_OK'}); o({p:'CHECK_START'}); o({p:'ASSERT_PASS',i:2}); o({p:'ASSERT_PASS',i:5}); o({p:'done',n:2,token:T});";

#[test]
fn the_lines_of_a_page_runner_are_judged_like_those_of_the_http_runner() {
    let mut fx = fixture();
    let r = fx.go(DOM_STEPS, "{}", PASS, Some(2));
    assert_eq!((r.judged.verdict, r.judged.assertions), (Verdict::Pass, 2), "{}", r.judged.reason);
    // The first failed assertion is the verdict, and the run stops there.
    let body = "o({p:'SETUP_OK'}); o({p:'CHECK_START'}); o({p:'ASSERT_PASS',i:2}); o({p:'ASSERT_FAIL',i:5,msg:'the page is at /x, not /login'}); o({p:'done',n:1,token:T});";
    let r = fx.go(DOM_STEPS, "{}", body, Some(2));
    assert_eq!((r.judged.verdict, r.judged.failed_step), (Verdict::Fail, Some(5)));
    assert!(r.judged.reason.contains("the page is at /x"), "{}", r.judged.reason);
    // A check saved with three assertions that now runs two is weakened.
    let r = fx.go(DOM_STEPS, "{}", PASS, Some(3));
    assert_eq!(r.judged.verdict, Verdict::Fail);
    assert!(r.judged.reason.starts_with("weakened"), "{}", r.judged.reason);
    // An error from the runner is never a FAIL: setup failures and a page that went to another host are UNRUNNABLE.
    let body = "o({p:'SETUP_OK'}); o({p:'ERROR',where:'setup',msg:'no Chromium found: torpor runner install'});";
    let r = fx.go(DOM_STEPS, "{}", body, Some(2));
    assert_eq!((r.judged.verdict, r.judged.reason.as_str()), (Verdict::Unrunnable, "no Chromium found: torpor runner install"));
    let body = "o({p:'SETUP_OK'}); o({p:'CHECK_START'}); o({p:'ERROR',where:'check',msg:'non-loopback: the page went to elsewhere.invalid'}); o({p:'done',n:0,token:T});";
    let r = fx.go(DOM_STEPS, "{}", body, Some(2));
    assert_eq!(r.judged.verdict, Verdict::Unrunnable);
    assert!(r.judged.reason.starts_with("non-loopback"), "{}", r.judged.reason);
    // A wrong end marker, a runner that dies, and one that prints nothing are all "did not run", never PASS.
    let r = fx.go(DOM_STEPS, "{}", "o({p:'SETUP_OK'}); o({p:'CHECK_START'}); o({p:'ASSERT_PASS',i:2}); o({p:'done',n:1,token:'forged'});", None);
    assert_eq!((r.judged.verdict, r.judged.reason.as_str()), (Verdict::Unrunnable, "the check's end marker was wrong"));
    let r = fx.go(DOM_STEPS, "{}", "o({p:'SETUP_OK'}); o({p:'CHECK_START'}); process.exit(1);", None);
    assert_eq!((r.judged.verdict, r.judged.reason.as_str()), (Verdict::Unrunnable, "the check did not finish"));
    let r = fx.go(DOM_STEPS, "{}", "throw new Error('the stand-in crashed');", None);
    assert_eq!((r.judged.verdict, r.judged.reason.as_str()), (Verdict::Unrunnable, "the check did not finish"));
}

#[test]
fn the_runner_gets_the_steps_and_the_nonce_on_stdin_and_a_small_environment() {
    let mut fx = fixture();
    let seen = fx.tmp.path().join("seen.json");
    let body = format!(
        "fs.writeFileSync({seen:?}, JSON.stringify({{argv: process.argv, env: process.env, payload: p, cwd: process.cwd()}})); {PASS}",
        seen = seen.to_str().unwrap()
    );
    let r = fx.go(DOM_STEPS, "{}", &body, Some(2));
    assert_eq!(r.judged.verdict, Verdict::Pass, "{}", r.judged.reason);
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&seen).unwrap()).unwrap();
    let nonce = v["payload"]["nonce"].as_str().unwrap();
    assert!(nonce.len() >= 16);
    assert!(v["payload"]["port"].as_u64().unwrap() > 1024);
    let steps = v["payload"]["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 5);
    assert_eq!((steps[0]["op"].as_str(), steps[0]["path"].as_str()), (Some("goto"), Some("/login")));
    assert_eq!((steps[3]["op"].as_str(), steps[3]["loc"]["name"].as_str()), (Some("click"), Some("Sign in")));
    // The spec and the nonce are on stdin: not in the arguments, not in any environment value.
    let argv: Vec<&str> = v["argv"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()).collect();
    assert_eq!(argv.len(), 2, "node and the script: {argv:?}");
    assert!(argv.iter().all(|a| !a.contains(nonce) && !a.contains("kim-the-tester")));
    let env = v["env"].as_object().unwrap();
    let mut names: Vec<&str> = env.keys().map(String::as_str).filter(|k| *k != "__CF_USER_TEXT_ENCODING").collect();
    names.sort();
    assert_eq!(names, ["HOME", "PATH", "PLAYWRIGHT_BROWSERS_PATH", "TMPDIR"], "nothing of the user's environment (no DISPLAY, no proxy, no tokens)");
    assert!(env.values().all(|x| !x.as_str().unwrap_or("").contains(nonce)));
    let home = env["HOME"].as_str().unwrap();
    assert!(home.contains("/scratch/") && home.ends_with("/home"), "HOME is the run's own folder: {home}");
    assert!(env["PLAYWRIGHT_BROWSERS_PATH"].as_str().unwrap().ends_with("/runner/1.64.1/browsers"));
    let tmpdir = env["TMPDIR"].as_str().unwrap();
    assert!(tmpdir.len() <= 60 && !tmpdir.contains("/scratch/"), "a short folder: Chromium's socket path must fit: {tmpdir}");
    assert!(!std::path::Path::new(tmpdir).exists(), "the browser's temp folder is removed after the run");
    assert!(v["cwd"].as_str().unwrap().contains("/scratch/"), "the runner starts inside the run's own folder");
    assert!(fx.proj.join("server.js").exists());
}

#[test]
fn a_runner_that_hangs_is_stopped_when_the_time_is_up_and_leaves_no_process() {
    let mut fx = fixture();
    let pid_file = fx.tmp.path().join("runner.pid");
    let body = format!("fs.writeFileSync({:?}, String(process.pid)); o({{p:'SETUP_OK'}}); o({{p:'CHECK_START'}}); setInterval(() => {{}}, 1000);", pid_file.to_str().unwrap());
    let t = Instant::now();
    let r = {
        let _limit = Deadline::set(Instant::now() + Duration::from_secs(4));
        fx.go(DOM_STEPS, "{}", &body, Some(2))
    };
    assert!(t.elapsed() < Duration::from_secs(30), "stopped at the limit, not at the runner's own limit: {:?}", t.elapsed());
    assert_eq!(r.judged.verdict, Verdict::Unrunnable);
    assert!(r.judged.reason.contains(OUT_OF_TIME), "{}", r.judged.reason);
    let pid: i32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
    // SAFETY: signal 0 only asks whether the process exists.
    assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "the runner process is gone");
}

#[test]
fn a_page_or_an_app_that_fails_before_the_first_assertion_is_a_setup_failure_and_the_runner_never_starts() {
    let mut fx = fixture();
    let counter = fx.tmp.path().join("count");
    std::fs::write(&counter, "2").unwrap(); // the next /api/flaky request is the third, which answers 500
    let env = serde_json::json!({"FLAKY_FILE": counter.to_str().unwrap()}).to_string();
    let marker = fx.tmp.path().join("ran");
    let body = format!("fs.writeFileSync({:?}, 'yes'); {PASS}", marker.to_str().unwrap());
    let steps = r#"[{"goto":"/login"},{"goto":"/api/flaky"},{"expect_visible":{"text":"Email"},"why":"the form shows"}]"#;
    let r = fx.go(steps, &env, &body, None);
    assert_eq!(r.judged.verdict, Verdict::Unrunnable);
    assert_eq!(r.judged.reason, "GET /api/flaky answered 500 before the check started");
    assert!(!marker.exists(), "no browser was started");
    // Each page is warmed up once: the ready probe (/login) and one GET of / make two requests, though / is visited twice.
    let hits = fx.tmp.path().join("hits");
    let body = format!(
        "fetch('http://127.0.0.1:' + p.port + '/api/hits').then((r) => r.json()).then((j) => {{ fs.writeFileSync({:?}, String(j.hits)); o({{p:'SETUP_OK'}}); o({{p:'CHECK_START'}}); o({{p:'ASSERT_PASS',i:3}}); o({{p:'done',n:1,token:T}}); }});",
        hits.to_str().unwrap()
    );
    let steps = r#"[{"goto":"/"},{"goto":"/"},{"expect_visible":{"text":"Fixture app"},"why":"the page shows"}]"#;
    let r = fx.go(steps, "{}", &body, None);
    assert_eq!(r.judged.verdict, Verdict::Pass, "{}", r.judged.reason);
    assert_eq!(std::fs::read_to_string(&hits).unwrap(), "2");
}

/// Chromium processes whose profile folder is inside a runner's private temp folder (`/tmp/tp-...`, see `dom::ShortTmp`).
fn our_browsers() -> usize {
    let ps = std::process::Command::new("ps").args(["-eo", "args"]).output().unwrap();
    String::from_utf8_lossy(&ps.stdout).lines().filter(|l| l.contains("/tp-") && l.contains("--user-data-dir")).count()
}

/// Folders of runner temp space (`tp-...`) in the system temp folder.
fn tp_dirs() -> usize {
    std::fs::read_dir(std::env::temp_dir()).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("tp-")).count()
}

/// The run with a real Chromium (spec 16). Not part of the normal run: it needs a runner made by `torpor runner install` and a Chromium on the
/// PATH. Run it with
///   TORPOR_REAL_RUNNER=<TORPOR_HOME>/runner/1.64.1 cargo test --test dom_run -- --ignored --nocapture
/// The browser is headless and opens only pages of the fixture app on 127.0.0.1. The fixture page also names two other hosts (an image and a
/// link): the runner must block them.
#[test]
#[ignore = "needs an installed runner (TORPOR_REAL_RUNNER) and a Chromium on the PATH"]
fn a_real_chromium_runs_page_checks_against_the_fixture_app() {
    let real = PathBuf::from(std::env::var_os("TORPOR_REAL_RUNNER").expect("set TORPOR_REAL_RUNNER to the folder that `torpor runner install` made"));
    assert!(torpor::dom::installed(&real), "{} holds no playwright-core", real.display());
    assert!(torpor::dom::find_chromium().is_some(), "no Chromium-family browser on the PATH");
    let tp_before = tp_dirs();
    let mut fx = fixture();
    let link = torpor::dom::runner_dir(&fx.ws.home);
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let broken = std::fs::read_to_string(fx.proj.join("server.js")).unwrap().replace("const BROKEN = false;", "const BROKEN = true;");
    // Watch the process list while the first run goes: the check below that nothing is left means little if this never saw a browser.
    let (stop, seen) = (std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)), std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)));
    let watcher = {
        let (stop, seen) = (stop.clone(), seen.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                seen.fetch_max(our_browsers(), std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    };
    let mut run = |steps: &str, overlay: Vec<(String, Option<Vec<u8>>)>, min_n: Option<usize>| {
        let tree = scratch::snapshot(&mut fx.ws, Duration::from_secs(20)).unwrap();
        let t0 = Instant::now();
        let r = run_once_with(&fx.ws, &spec_of(steps, "{}"), &tree, min_n, &Opts { overlay, ..Opts::default() });
        eprintln!("real run: {:?} in {:.1} s, {} assertions, {}", r.judged.verdict, t0.elapsed().as_secs_f32(), r.judged.assertions, r.judged.reason);
        r
    };
    // 1. A good page: heading, typing, a click, text that comes from a script, the address, a count, an http step and a clean console.
    //    The blocked image must not make the console unclean.
    let good = r#"[
      {"goto":"/dom"},
      {"expect_visible":{"role":"heading","name":"Account"},"why":"the account page shows"},
      {"fill":{"label":"Email"},"value":"kim-the-tester"},
      {"click":{"role":"button","name":"Sign in"}},
      {"expect_text":{"role":"status"},"contains":"Signed in as kim-the-tester","why":"the page says who signed in"},
      {"expect_url":{"path":"/dom"},"why":"the page stays"},
      {"expect_count":{"role":"link"},"n":1,"why":"the page has one link"},
      {"http":{"path":"/api/hits"},"expect":{"status":200,"json_has":["hits"]},"why":"the app counts its requests"},
      {"expect_console_clean":true,"why":"the page logs no error"}]"#;
    let r = run(good, vec![], Some(6));
    assert_eq!((r.judged.verdict, r.judged.assertions), (Verdict::Pass, 6), "{}", r.judged.reason);
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    watcher.join().unwrap();
    assert!(seen.load(std::sync::atomic::Ordering::SeqCst) >= 1, "the process list never showed the browser");
    // 2. The same page with the seeded regression (sign-in fails): a FAIL at the text step, not an UNRUNNABLE.
    let r = run(good, vec![("server.js".into(), Some(broken.into_bytes()))], Some(6));
    assert_eq!((r.judged.verdict, r.judged.failed_step), (Verdict::Fail, Some(5)), "{}", r.judged.reason);
    assert!(r.judged.reason.contains("Signed in as kim-the-tester"), "{}", r.judged.reason);
    // 3. A missing element is a FAIL too.
    let r = run(r#"[{"goto":"/dom"},{"click":{"testid":"nope"}},{"expect_visible":{"text":"Email"},"why":"the form shows"}]"#, vec![], None);
    assert_eq!((r.judged.verdict, r.judged.failed_step), (Verdict::Fail, Some(2)), "{}", r.judged.reason);
    // 4. A console error is a FAIL for expect_console_clean.
    let r = run(r#"[{"goto":"/dom?err=1"},{"expect_console_clean":true,"why":"the page logs no error"}]"#, vec![], None);
    assert_eq!((r.judged.verdict, r.judged.failed_step), (Verdict::Fail, Some(2)), "{}", r.judged.reason);
    assert!(r.judged.reason.contains("console error: fixture error"), "{}", r.judged.reason);
    // 5. A link to another host: the browser does not go there, and the check is UNRUNNABLE (never a PASS or a FAIL of the app).
    let r = run(r#"[{"goto":"/dom"},{"click":{"role":"link","name":"Leave"}},{"expect_url":{"path":"/x"},"why":"the link went on"}]"#, vec![], None);
    assert_eq!(r.judged.verdict, Verdict::Unrunnable, "{}", r.judged.reason);
    assert!(r.judged.reason.starts_with("non-loopback"), "{}", r.judged.reason);
    // Nothing is left behind: no browser, and the runner's private temp folders are gone.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(our_browsers(), 0, "a browser is still running after the checks");
    assert_eq!(tp_dirs(), tp_before, "a runner temp folder is left in {}", std::env::temp_dir().display());
}

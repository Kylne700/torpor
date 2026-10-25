//! Scratch copy and app runs (spec 16, B7): a saved version runs alone, on its own port, with its own HOME and no secret files.
//! Needs `node` (the fixture app is one Node file). Every server here runs in its own process group and is stopped by a Drop guard.
mod common;
use common::{manifest_dir, write_files};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use torpor::runner::{Client, Verdict};
use torpor::scratch::{self, Run, run_once};
use torpor::spec::{self, Linted};
use torpor::store::Ws;

const SENTINEL: &str = "SENTINEL-4f9a1c7e2b60";

struct Fx {
    _tmp: tempfile::TempDir,
    proj: PathBuf,
    ws: Ws,
}

fn fixture(extra: &[(&str, &str)]) -> Fx {
    let tmp = tempfile::Builder::new()
        .prefix("torpor-scratch")
        .tempdir()
        .unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    write_files(&proj, extra);
    std::fs::copy(
        manifest_dir().join("tests/fixtures/app/server.js"),
        proj.join("server.js"),
    )
    .unwrap();
    let ws = Ws::open_in(&tmp.path().join("thome"), &proj, true).unwrap();
    Fx {
        _tmp: tmp,
        proj,
        ws,
    }
}

impl Fx {
    fn snap(&mut self) -> [u8; 32] {
        scratch::snapshot(&mut self.ws, Duration::from_secs(20)).unwrap()
    }
    /// Flip the seeded regression (the one line of server.js).
    fn set_broken(&self, on: bool) {
        let f = self.proj.join("server.js");
        let t = std::fs::read_to_string(&f).unwrap();
        let (from, to) = if on {
            ("const BROKEN = false;", "const BROKEN = true;")
        } else {
            ("const BROKEN = true;", "const BROKEN = false;")
        };
        assert!(t.contains(from));
        std::fs::write(f, t.replace(from, to)).unwrap();
    }
    fn count(&self, sql: &str) -> i64 {
        self.ws.conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }
}

fn spec_of(steps: &str, start: &str, env: &str) -> Linted {
    let raw = format!(
        r#"{{"v":1,"name":"fixture check","why":"the fixture app answers","start":{start},"env":{env},"steps":{steps}}}"#
    );
    spec::lint(&raw).unwrap_or_else(|p| panic!("lint: {p:?}"))
}

const START: &str = r#"{"cmd":"node server.js --port {port}","ready":"/login","timeout_s":30}"#;
const LOGIN_STEPS: &str = r#"[
  {"http":{"method":"POST","path":"/api/login","json":{"email":"kim@example.com"}},"expect":{"status":200},"why":"sign-in is accepted"},
  {"http":{"path":"/api/me"},"expect":{"status":200,"json_has":["email"]},"why":"after sign-in the account data shows"}]"#;

fn login() -> Linted {
    spec_of(LOGIN_STEPS, START, "{}")
}

/// A server of the "user" in their project folder: it must stay untouched by every check.
struct Dev {
    child: Child,
    pgid: i32,
    port: u16,
}

impl Dev {
    fn start(dir: &Path) -> Dev {
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let child = Command::new("node")
            .args(["server.js", "--port", &port.to_string()])
            .current_dir(dir)
            .env("APP_LIFETIME_MS", "120000")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id() as i32;
        let dev = Dev { child, pgid, port };
        let t = Instant::now();
        while get(port, "/api/hits").is_none() {
            assert!(
                t.elapsed() < Duration::from_secs(10),
                "the dev server did not start"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        dev
    }
    fn hits(&self) -> i64 {
        get(self.port, "/api/hits").unwrap().1["hits"]
            .as_i64()
            .unwrap()
    }
}

impl Drop for Dev {
    fn drop(&mut self) {
        // SAFETY: the group this test started; pgid is the leader's pid.
        unsafe { libc::kill(-self.pgid, libc::SIGTERM) };
        let _ = self.child.wait();
    }
}

fn get(port: u16, path: &str) -> Option<(u16, serde_json::Value)> {
    let r = Client::new(port).request("GET", path, None).ok()?;
    Some((
        r.status,
        serde_json::from_slice(&r.body).unwrap_or(serde_json::Value::Null),
    ))
}

fn scratch_dirs(fx: &Fx) -> usize {
    std::fs::read_dir(fx.ws.dir.join("scratch"))
        .map(|d| d.count())
        .unwrap_or(0)
}

#[test]
fn a_login_check_passes_then_fails_on_the_seeded_regression_and_leaves_nothing_behind() {
    let mut fx = fixture(&[]);
    let good = fx.snap();
    fx.set_broken(true);
    let bad = fx.snap();
    assert_ne!(good, bad);
    assert_eq!(
        fx.count("SELECT COUNT(*) FROM points"),
        0,
        "a snapshot writes no save point"
    );
    let l = login();
    let r = run_once(&fx.ws, &l.spec, &good, Some(2));
    assert_eq!(
        (r.judged.verdict, r.judged.assertions),
        (Verdict::Pass, 2),
        "{}",
        r.judged.reason
    );
    let r = run_once(&fx.ws, &l.spec, &bad, Some(2));
    assert_eq!(r.judged.verdict, Verdict::Fail, "{}", r.judged.reason);
    assert_eq!(r.judged.failed_step, Some(2));
    assert!(r.judged.reason.contains("401"), "{}", r.judged.reason);
    // A check saved with 3 assertions that now runs 2 is weakened.
    let r = run_once(&fx.ws, &l.spec, &good, Some(3));
    assert_eq!(r.judged.verdict, Verdict::Fail);
    assert!(
        r.judged.reason.starts_with("weakened"),
        "{}",
        r.judged.reason
    );
    assert_eq!(
        scratch_dirs(&fx),
        0,
        "every scratch folder is removed after its run"
    );
}

#[test]
fn the_app_runs_alone_on_its_own_port_with_its_own_home_and_no_env_files_b7() {
    let mut fx = fixture(&[
        (".env.example", &format!("API_TOKEN={SENTINEL}\n")),
        ("notes/.env.sample", &format!("X={SENTINEL}\n")),
    ]);
    let tree = fx.snap();
    let saved: Vec<String> = torpor::capture::flatten_tree(&fx.ws, &fx.ws.conn, &tree)
        .unwrap()
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    assert!(
        saved.contains(&".env.example".to_string())
            && saved.contains(&"notes/.env.sample".to_string()),
        "the sentinel files are in the saved version: {saved:?}"
    );
    let dev = Dev::start(&fx.proj);
    let l = spec_of(
        r#"[{"http":{"path":"/api/isolated"},"expect":{"status":200,"json_has":["isolated"]},"why":"the app sees none of the user's secrets"}]"#,
        START,
        r#"{"MODE":"test mode"}"#,
    );
    let r = run_once(&fx.ws, &l.spec, &tree, None);
    assert_eq!(r.judged.verdict, Verdict::Pass, "{}", r.judged.reason);

    // The probe can fail: with the sentinel in the spec's own env the app sees it, and says so.
    let leaky = spec_of(
        r#"[{"http":{"path":"/api/isolated"},"expect":{"status":200},"why":"the app sees none of the user's secrets"}]"#,
        START,
        &format!(r#"{{"MODE":"{SENTINEL}"}}"#),
    );
    let r = run_once(&fx.ws, &leaky.spec, &tree, None);
    assert_eq!(r.judged.verdict, Verdict::Fail, "{}", r.judged.reason);
    assert!(r.judged.reason.contains("500"), "{}", r.judged.reason);

    // Look inside a started app.
    let run = Run::start(&fx.ws, &l.spec, &tree).unwrap_or_else(|s| panic!("{}", s.judged.reason));
    assert_ne!(run.port, dev.port, "its own port, never the user's");
    let (st, who) = get(run.port, "/api/whoami").unwrap();
    assert_eq!(st, 200);
    assert_eq!(who["port"].as_u64(), Some(run.port as u64));
    let scratch_root = fx.ws.dir.join("scratch");
    let home = who["home"].as_str().unwrap();
    assert!(
        Path::new(home).starts_with(&scratch_root) && Path::new(home).ends_with("home"),
        "HOME is inside the scratch folder: {home}"
    );
    assert!(Path::new(who["cwd"].as_str().unwrap()).ends_with("app"));
    assert!(Path::new(who["tmpdir"].as_str().unwrap()).starts_with(&scratch_root));
    let env: Vec<&str> = who["env"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for k in ["HOME", "TMPDIR", "PORT", "MODE"] {
        assert!(env.contains(&k), "{k} missing in {env:?}");
    }
    // __CF_USER_TEXT_ENCODING: macOS sets it inside every new process (uid and text encoding); Torpor does not pass it.
    if let Some(k) = env.iter().find(|k| {
        ![
            "PATH",
            "LANG",
            "LC_ALL",
            "TERM",
            "SHELL",
            "USER",
            "HOME",
            "TMPDIR",
            "PORT",
            "MODE",
            "CARGO_TARGET_DIR",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "NVM_DIR",
            "VOLTA_HOME",
            "PNPM_HOME",
            "BUN_INSTALL",
            "PYENV_ROOT",
            "PWD",
            "SHLVL",
            "_",
            "OLDPWD",
            "__CF_USER_TEXT_ENCODING",
        ]
        .contains(k)
    }) {
        panic!("the app sees an unexpected variable {k}");
    }
    let files: Vec<&str> = who["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        files.contains(&"server.js") && files.contains(&".env.torpor") && files.contains(&".env"),
        "{files:?}"
    );
    assert!(
        !files.contains(&".env.example"),
        "the project's .env* files are not copied: {files:?}"
    );
    let dotenv = who["dotenv"].as_str().unwrap();
    assert!(
        dotenv.contains(&format!("PORT={}\n", run.port))
            && dotenv.contains("MODE=\"test mode\"\n")
            && !dotenv.contains(SENTINEL),
        "{dotenv}"
    );
    let (st, iso) = get(run.port, "/api/isolated").unwrap();
    assert_eq!((st, iso["isolated"].as_bool()), (200, Some(true)));
    let port = run.port;
    drop(run);
    assert!(
        get(port, "/api/hits").is_none(),
        "the app is gone once the run ends"
    );
    assert_eq!(scratch_dirs(&fx), 0);

    assert_eq!(
        dev.hits(),
        0,
        "the user's own dev server was never asked anything"
    );
    assert_eq!(
        get(dev.port, "/api/hits").unwrap().0,
        200,
        "and it is still up"
    );
}

#[test]
fn a_redirect_off_the_machine_and_an_env_off_the_machine_are_unrunnable() {
    let mut fx = fixture(&[]);
    let tree = fx.snap();
    let l = spec_of(
        r#"[{"http":{"path":"/api/redirect-out"},"expect":{"status":200},"why":"the page loads"}]"#,
        START,
        "{}",
    );
    let r = run_once(&fx.ws, &l.spec, &tree, None);
    assert_eq!(r.judged.verdict, Verdict::Unrunnable);
    assert!(
        r.judged.reason.contains("non-loopback") && r.judged.reason.contains("example.invalid"),
        "{}",
        r.judged.reason
    );

    // The lint says no ...
    let raw = format!(
        r#"{{"v":1,"name":"n","why":"w","start":{START},"env":{{"API_URL":"https://api.example.com/v1"}},"steps":{LOGIN_STEPS}}}"#
    );
    let why = spec::lint(&raw).unwrap_err().join("\n");
    assert!(why.contains("not on this computer"), "{why}");
    // ... and a spec that got past it (a changed vault file) is still not run.
    let mut s = login().spec;
    s.env
        .insert("API_URL".into(), "https://api.example.com/v1".into());
    let r = run_once(&fx.ws, &s, &tree, None);
    assert_eq!(r.judged.verdict, Verdict::Unrunnable);
    assert!(
        r.judged.reason.contains("not on this computer"),
        "{}",
        r.judged.reason
    );
    assert_eq!(scratch_dirs(&fx), 0);
}

#[test]
fn setup_failures_are_unrunnable_and_say_what_the_app_printed() {
    let mut fx = fixture(&[]);
    let tree = fx.snap();
    let steps = r#"[{"http":{"path":"/login"},"expect":{"status":200},"why":"the page loads"}]"#;
    let r = run_once(
        &fx.ws,
        &spec_of(
            steps,
            r#"{"cmd":"echo boom here; exit 3","ready":"/login","timeout_s":10}"#,
            "{}",
        )
        .spec,
        &tree,
        None,
    );
    assert_eq!(r.judged.verdict, Verdict::Unrunnable);
    assert!(
        r.judged.reason.contains("did not start") && r.judged.reason.contains("exited 3"),
        "{}",
        r.judged.reason
    );
    assert!(r.tail.contains("boom here"), "{}", r.tail);
    let r = run_once(
        &fx.ws,
        &spec_of(
            steps,
            r#"{"cmd":"sleep 30","ready":"/login","timeout_s":2}"#,
            "{}",
        )
        .spec,
        &tree,
        None,
    );
    assert_eq!(r.judged.verdict, Verdict::Unrunnable);
    assert!(
        r.judged.reason.contains("did not start") && r.judged.reason.contains("no answer"),
        "{}",
        r.judged.reason
    );
    // A page that answers 404 is not "ready".
    let r = run_once(
        &fx.ws,
        &spec_of(
            steps,
            r#"{"cmd":"node server.js --port {port}","ready":"/nope","timeout_s":2}"#,
            "{}",
        )
        .spec,
        &tree,
        None,
    );
    assert_eq!(r.judged.verdict, Verdict::Unrunnable);
    assert!(r.judged.reason.contains("gave 404"), "{}", r.judged.reason);
    // Steps that need a browser wait for the runner: nothing is started.
    let dom = r#"[{"goto":"/login"},{"expect_visible":{"text":"Email"},"why":"the form shows"}]"#;
    let r = run_once(&fx.ws, &spec_of(dom, START, "{}").spec, &tree, None);
    assert_eq!(r.judged.verdict, Verdict::Unrunnable);
    assert_eq!(
        r.judged.reason,
        "runner not installed: torpor runner install"
    );
    assert_eq!(scratch_dirs(&fx), 0);
}

#[test]
fn the_copy_is_byte_exact_without_env_files_and_outward_links() {
    let mut fx = fixture(&[
        ("src/a.txt", "A\n"),
        ("bin/run.sh", "#!/bin/sh\necho hi\n"),
        ("notes/.env.sample", "X=1\n"),
    ]);
    std::fs::set_permissions(
        fx.proj.join("bin/run.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::create_dir_all(fx.proj.join("empty")).unwrap();
    std::os::unix::fs::symlink("src/a.txt", fx.proj.join("link")).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", fx.proj.join("abs")).unwrap();
    std::os::unix::fs::symlink("../../outside", fx.proj.join("src/out")).unwrap();
    let tree = fx.snap();
    let dest = fx._tmp.path().join("copy");
    std::fs::create_dir_all(&dest).unwrap();
    let made = scratch::materialize(&fx.ws, &tree, &dest).unwrap();
    assert!(made.needs_install.is_none());
    assert_eq!(std::fs::read(dest.join("src/a.txt")).unwrap(), b"A\n");
    assert_eq!(
        std::fs::read(dest.join("server.js")).unwrap(),
        std::fs::read(fx.proj.join("server.js")).unwrap()
    );
    assert_eq!(
        std::fs::metadata(dest.join("bin/run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0o111,
        "the exec bit stays"
    );
    assert_eq!(
        std::fs::metadata(dest.join("src/a.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert!(dest.join("empty").is_dir());
    assert_eq!(
        std::fs::read_link(dest.join("link")).unwrap(),
        Path::new("src/a.txt")
    );
    for gone in ["abs", "src/out", "notes/.env.sample"] {
        assert!(
            dest.join(gone).symlink_metadata().is_err(),
            "{gone} must not be in the copy"
        );
    }
}

#[test]
fn chained_links_that_lead_out_of_the_copy_are_not_recreated() {
    let mut fx = fixture(&[("src/a.txt", "A\n")]);
    std::fs::create_dir_all(fx.proj.join("d")).unwrap();
    // `d/b` is the project's own folder and `d/c` goes on from there to the folder above it: each looks inward on its own.
    std::os::unix::fs::symlink("..", fx.proj.join("d/b")).unwrap();
    std::os::unix::fs::symlink("b/..", fx.proj.join("d/c")).unwrap();
    std::os::unix::fs::symlink("../src/a.txt", fx.proj.join("d/in")).unwrap();
    let tree = fx.snap();
    let dest = fx._tmp.path().join("copy");
    std::fs::create_dir_all(&dest).unwrap();
    let names = |d: &Path| -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    let before = names(fx._tmp.path());
    scratch::materialize(&fx.ws, &tree, &dest).unwrap();
    for gone in ["d/b", "d/c"] {
        assert!(
            dest.join(gone).symlink_metadata().is_err(),
            "{gone} must not be in the copy"
        );
    }
    assert_eq!(
        std::fs::read_link(dest.join("d/in")).unwrap(),
        Path::new("../src/a.txt"),
        "a link that stays inside is kept"
    );
    assert_eq!(
        names(fx._tmp.path()),
        before,
        "nothing was written next to the copy"
    );
}

#[test]
fn dependency_folders_are_linked_only_when_the_lockfiles_match() {
    let mut fx = fixture(&[
        ("package-lock.json", "{\"lockfileVersion\":3}\n"),
        ("package.json", "{}\n"),
    ]);
    std::fs::create_dir_all(fx.proj.join("node_modules/dep")).unwrap();
    std::fs::write(
        fx.proj.join("node_modules/dep/index.js"),
        "module.exports = 1;\n",
    )
    .unwrap();
    let tree = fx.snap();
    let dest = fx._tmp.path().join("copy1");
    std::fs::create_dir_all(&dest).unwrap();
    assert!(
        scratch::materialize(&fx.ws, &tree, &dest)
            .unwrap()
            .needs_install
            .is_none()
    );
    assert_eq!(
        std::fs::read_link(dest.join("node_modules")).unwrap(),
        fx.proj.join("node_modules"),
        "linked to the live folder, never copied"
    );
    assert!(dest.join("node_modules/dep/index.js").exists());

    // The project's lockfile moves on: the saved version's dependencies are not the ones installed.
    std::fs::write(
        fx.proj.join("package-lock.json"),
        "{\"lockfileVersion\":3,\"packages\":{}}\n",
    )
    .unwrap();
    let dest = fx._tmp.path().join("copy2");
    std::fs::create_dir_all(&dest).unwrap();
    let made = scratch::materialize(&fx.ws, &tree, &dest).unwrap();
    assert_eq!(made.needs_install.as_deref(), Some("node_modules"));
    assert!(dest.join("node_modules").symlink_metadata().is_err());
    let l = spec_of(
        r#"[{"http":{"path":"/login"},"expect":{"status":200},"why":"the page loads"}]"#,
        START,
        "{}",
    );
    let r = run_once(&fx.ws, &l.spec, &tree, None);
    assert_eq!(r.judged.verdict, Verdict::Unrunnable);
    assert!(
        r.judged.reason.starts_with("needs-install"),
        "{}",
        r.judged.reason
    );

    // No lockfile on either side: nothing says they differ, so the folder is linked.
    let mut bare = fixture(&[]);
    std::fs::create_dir_all(bare.proj.join("node_modules")).unwrap();
    let t = bare.snap();
    let dest = bare._tmp.path().join("copy3");
    std::fs::create_dir_all(&dest).unwrap();
    assert!(
        scratch::materialize(&bare.ws, &t, &dest)
            .unwrap()
            .needs_install
            .is_none()
    );
    assert!(
        dest.join("node_modules")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

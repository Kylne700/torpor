//! `torpor runner install` (spec 24) against a registry stand-in on 127.0.0.1. Nothing here can reach the real npm registry: only builds
//! with the `fault-inject` feature read TORPOR_TEST_REGISTRY, and they accept only http://127.0.0.1 addresses. The package is a stand-in too
//! (a package.json and a cli.js that downloads nothing), so no Chromium is ever downloaded by a test. Needs `node` and `tar`.
mod common;
use common::{BIN, Env, Out, cmd_with, on_terminal, run_cmd};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const CLI_JS: &str = "const fs = require('fs'), path = require('path');
const b = process.env.PLAYWRIGHT_BROWSERS_PATH;
fs.mkdirSync(path.join(b, 'chromium-1'), { recursive: true });
fs.writeFileSync(path.join(b, 'chromium-1', 'INSTALLATION_COMPLETE'), '');
fs.writeFileSync(path.join(b, '..', 'cli-ran.json'), JSON.stringify({ argv: process.argv.slice(2), home: process.env.HOME, browsers: b, cwd: process.cwd(), env: Object.keys(process.env).sort() }));
";

fn which(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|d| d.join(name))
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("{name} is needed for this test"))
}

/// A package like playwright-core 1.64.1 in the shape of an npm tarball (everything under `package/`), and its integrity string.
fn package(dir: &Path) -> (Vec<u8>, String) {
    let root = dir.join("pkg");
    std::fs::create_dir_all(root.join("package/lib")).unwrap();
    std::fs::write(
        root.join("package/package.json"),
        r#"{"name":"playwright-core","version":"1.64.1","bin":{"playwright-core":"cli.js"}}"#,
    )
    .unwrap();
    std::fs::write(root.join("package/cli.js"), CLI_JS).unwrap();
    std::fs::write(root.join("package/lib/inner.js"), "module.exports = 1;\n").unwrap();
    let tgz = dir.join("pkg.tgz");
    let st = Command::new("tar")
        .arg("-czf")
        .arg(&tgz)
        .arg("-C")
        .arg(&root)
        .arg("package")
        .status()
        .unwrap();
    assert!(st.success());
    let bytes = std::fs::read(&tgz).unwrap();
    let out = Command::new(which("node"))
        .args(["-e", "console.log('sha512-' + require('crypto').createHash('sha512').update(require('fs').readFileSync(process.argv[1])).digest('base64'))"])
        .arg(&tgz)
        .output()
        .unwrap();
    (
        bytes,
        String::from_utf8(out.stdout).unwrap().trim().to_string(),
    )
}

/// Answers `GET /playwright-core/-/playwright-core-1.64.1.tgz` with `body` (or with a 302 to `redirect`), and writes down every request line it gets.
struct Registry {
    port: u16,
    hits: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
}

impl Registry {
    fn start(body: Vec<u8>) -> Registry {
        Registry::start_as(body, None)
    }
    /// A registry that answers the package request with a 302 to `to` and serves nothing itself.
    fn redirecting(to: String) -> Registry {
        Registry::start_as(Vec::new(), Some(to))
    }
    fn start_as(body: Vec<u8>, redirect: Option<String>) -> Registry {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (hits, stop) = (
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(AtomicBool::new(false)),
        );
        let (h2, s2) = (hits.clone(), stop.clone());
        std::thread::spawn(move || {
            for conn in l.incoming() {
                if s2.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut c) = conn else { continue };
                let mut buf = [0u8; 4096];
                let n = c.read(&mut buf).unwrap_or(0);
                let line = String::from_utf8_lossy(&buf[..n])
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                let ok = line.starts_with("GET /playwright-core/-/playwright-core-1.64.1.tgz ");
                h2.lock().unwrap().push(line);
                let head = if let (true, Some(to)) = (ok, &redirect) {
                    format!(
                        "HTTP/1.1 302 Found\r\nlocation: {to}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    )
                } else if ok {
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = c.write_all(head.as_bytes());
                if ok && redirect.is_none() {
                    let _ = c.write_all(&body);
                }
            }
        });
        Registry { port, hits, stop }
    }
    fn requests(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port)); // wakes the accept loop so it ends
    }
}

struct Fx {
    env: Env,
    reg: Registry,
    integrity: String,
    bin: PathBuf,
}

impl Fx {
    /// `body`: what the registry serves (None: the real package). `browser`: whether a Chromium is on this test's PATH.
    fn new(tamper: bool, browser: bool) -> Fx {
        let env = Env::new();
        let (mut bytes, integrity) = package(&env.home);
        if tamper {
            bytes.push(0);
        }
        let bin = env.home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(which("node"), bin.join("node")).unwrap();
        if browser {
            std::fs::write(bin.join("chromium"), "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(bin.join("chromium"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        Fx {
            reg: Registry::start(bytes),
            env,
            integrity,
            bin,
        }
    }
    fn dir(&self) -> PathBuf {
        self.env.thome.join("runner/1.64.1")
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = cmd_with(Path::new(BIN), &self.env);
        c.arg("runner").args(args);
        // The stand-in's folder first: a node, and a Chromium when `browser`. /usr/bin and /bin carry tar and gzip.
        c.env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()));
        c.env(
            "TORPOR_TEST_REGISTRY",
            format!("http://127.0.0.1:{}", self.reg.port),
        )
        .env("TORPOR_TEST_INTEGRITY", &self.integrity);
        c
    }
    fn run(&self, args: &[&str]) -> Out {
        run_cmd(self.cmd(args), b"")
    }
    fn no_staging_left(&self) {
        let staged = std::fs::read_dir(self.dir()).is_ok_and(|d| {
            d.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with(".staging"))
        });
        assert!(!staged, "no staging folder is left");
    }
    fn nothing_installed(&self) {
        assert!(
            !self.dir().join("node_modules").exists(),
            "nothing was unpacked"
        );
        self.no_staging_left();
    }
}

#[test]
fn yes_installs_the_checked_package_into_the_runner_folder_with_one_request_and_a_second_run_does_nothing()
 {
    let fx = Fx::new(false, true);
    let o = fx.run(&["install", "--yes"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains("The browser runner needs about 3 MB (playwright-core from the npm registry). Install into "), "{}", o.stdout);
    assert!(o.stdout.contains("[y/N] y (--yes)"), "{}", o.stdout);
    assert!(
        o.stdout.contains("installed playwright-core 1.64.1")
            && o.stdout.contains("Page checks can run now."),
        "{}",
        o.stdout
    );
    assert!(
        !o.stdout.contains("Chromium"),
        "a Chromium was found on the PATH, so none is downloaded: {}",
        o.stdout
    );
    assert_eq!(fx.reg.requests().len(), 1, "{:?}", fx.reg.requests());
    assert!(fx.reg.requests()[0].starts_with("GET /playwright-core/-/playwright-core-1.64.1.tgz "));
    let dir = fx.dir();
    let pkg: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.join("node_modules/playwright-core/package.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        (pkg["name"].as_str(), pkg["version"].as_str()),
        (Some("playwright-core"), Some("1.64.1"))
    );
    assert!(
        dir.join("node_modules/playwright-core/lib/inner.js")
            .is_file(),
        "the whole package is unpacked, below one folder"
    );
    assert!(
        !dir.join("cli-ran.json").exists(),
        "no browser download ran"
    );
    fx.no_staging_left();
    // The folder is private, the runner script is the one this Torpor carries, and the project files are pinned.
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("run.mjs")).unwrap(),
        std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/runner/run.mjs")
        )
        .unwrap()
    );
    let top: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("package.json")).unwrap()).unwrap();
    assert_eq!(top["dependencies"]["playwright-core"], "1.64.1");
    let lock: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("package-lock.json")).unwrap()).unwrap();
    let entry = &lock["packages"]["node_modules/playwright-core"];
    assert_eq!(entry["version"], "1.64.1");
    assert_eq!(
        entry["resolved"],
        "https://registry.npmjs.org/playwright-core/-/playwright-core-1.64.1.tgz"
    );
    assert!(entry["integrity"].as_str().unwrap().starts_with("sha512-"));
    // A second run finds it and downloads nothing. A changed run.mjs is put right.
    std::fs::write(dir.join("run.mjs"), "old").unwrap();
    let o = fx.run(&["install"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(
        o.stdout
            .starts_with("The browser runner is already installed in "),
        "{}",
        o.stdout
    );
    assert_eq!(fx.reg.requests().len(), 1);
    assert_ne!(std::fs::read_to_string(dir.join("run.mjs")).unwrap(), "old");
}

#[test]
fn a_download_that_does_not_match_its_published_hash_installs_nothing() {
    let fx = Fx::new(true, true);
    let o = fx.run(&["install", "--yes"]);
    assert_eq!(o.code, 1, "{}{}", o.stdout, o.stderr);
    assert!(
        o.stderr.contains("does not match the published hash"),
        "{}",
        o.stderr
    );
    assert_eq!(fx.reg.requests().len(), 1);
    fx.nothing_installed();
}

#[test]
fn a_registry_that_redirects_installs_nothing_and_the_other_host_is_never_asked() {
    // The real package waits on a second host; the registry Torpor is pointed at only sends the install there.
    let fx = Fx::new(false, true);
    let hop = Registry::redirecting(format!(
        "http://127.0.0.1:{}/playwright-core/-/playwright-core-1.64.1.tgz",
        fx.reg.port
    ));
    let mut c = fx.cmd(&["install", "--yes"]);
    c.env(
        "TORPOR_TEST_REGISTRY",
        format!("http://127.0.0.1:{}", hop.port),
    );
    let o = run_cmd(c, b"");
    assert_eq!(o.code, 1, "{}{}", o.stdout, o.stderr);
    assert!(
        o.stderr
            .contains("the registry sent a redirect, so nothing was installed"),
        "{}",
        o.stderr
    );
    assert_eq!(hop.requests().len(), 1, "{:?}", hop.requests());
    assert!(
        fx.reg.requests().is_empty(),
        "the redirect was followed: {:?}",
        fx.reg.requests()
    );
    fx.nothing_installed();
}

#[test]
fn without_yes_and_without_a_terminal_nothing_is_downloaded() {
    let fx = Fx::new(false, true);
    let o = fx.run(&["install"]);
    assert_eq!(o.code, 3, "{}{}", o.stdout, o.stderr);
    assert!(
        o.stdout.contains("npm registry") && o.stdout.trim_end().ends_with("[y/N]"),
        "{}",
        o.stdout
    );
    assert!(
        o.stderr.starts_with("Nothing was installed."),
        "{}",
        o.stderr
    );
    assert!(fx.reg.requests().is_empty(), "no request was made");
    fx.nothing_installed();
}

#[test]
fn a_person_at_a_terminal_can_say_no_or_yes() {
    let fx = Fx::new(false, true);
    let o = on_terminal(fx.cmd(&["install"]), "n");
    assert_eq!(
        (o.code, o.stderr.as_str()),
        (3, "Nothing was installed.\n"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.ends_with("[y/N] "), "{:?}", o.stdout);
    let o = on_terminal(fx.cmd(&["install"]), "");
    assert_eq!(o.code, 3, "an empty answer means no: {}", o.stdout);
    assert!(fx.reg.requests().is_empty());
    fx.nothing_installed();
    let o = on_terminal(fx.cmd(&["install"]), "Y");
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!(fx.reg.requests().len(), 1);
    assert!(
        fx.dir()
            .join("node_modules/playwright-core/package.json")
            .is_file()
    );
}

#[test]
fn without_a_chromium_the_packages_own_command_installs_one_inside_the_runner_folder() {
    let fx = Fx::new(false, false);
    let o = run_cmd(
        {
            let mut c = fx.cmd(&["install", "--yes"]);
            c.env("TORPOR_TEST_NO_BROWSER", "1");
            c
        },
        b"",
    );
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains("plus a Chromium of a few hundred MB (with ffmpeg) from Playwright's servers, because none was found"), "{}", o.stdout);
    assert!(
        o.stdout
            .contains("Installed Chromium for the browser runner."),
        "{}",
        o.stdout
    );
    let ran: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fx.dir().join("cli-ran.json")).unwrap()).unwrap();
    assert_eq!(
        ran["argv"],
        serde_json::json!(["install", "--no-shell", "chromium"])
    );
    let dir = fx.dir().to_string_lossy().into_owned();
    assert_eq!(ran["browsers"], format!("{dir}/browsers"));
    assert_eq!(
        ran["home"],
        format!("{dir}/home"),
        "the download never touches the user's home"
    );
    assert_eq!(ran["cwd"], dir);
    // Only these: the PATH and HOME, the browser folder, and (when the person has them) a proxy or certificate setting for the download.
    let allowed = [
        "HOME",
        "PATH",
        "PLAYWRIGHT_BROWSERS_PATH",
        "NODE_EXTRA_CA_CERTS",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "NO_PROXY",
        "NODE_USE_ENV_PROXY",
        "__CF_USER_TEXT_ENCODING",
    ];
    let names: Vec<&str> = ran["env"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(names.iter().all(|n| allowed.contains(n)), "{names:?}");
    assert!(
        ["HOME", "PATH", "PLAYWRIGHT_BROWSERS_PATH"]
            .iter()
            .all(|n| names.contains(n)),
        "{names:?}"
    );
}

#[test]
fn a_browser_download_that_stopped_half_way_is_offered_again_and_a_finished_one_is_not() {
    let fx = Fx::new(false, false);
    let no_browser = |args: &[&str]| {
        let mut c = fx.cmd(args);
        c.env("TORPOR_TEST_NO_BROWSER", "1");
        run_cmd(c, b"")
    };
    let o = no_browser(&["install", "--yes"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    let (ran, marker) = (
        fx.dir().join("cli-ran.json"),
        fx.dir().join("browsers/chromium-1/INSTALLATION_COMPLETE"),
    );
    assert!(
        ran.is_file() && marker.is_file(),
        "the stand-in finished its download"
    );
    let o = no_browser(&["install", "--yes"]);
    assert!(
        o.stdout.contains("is already installed in"),
        "a finished download is not offered again: {}",
        o.stdout
    );
    // The download stopped half way: the folder is there, Playwright's marker is not.
    std::fs::remove_file(&marker).unwrap();
    std::fs::remove_file(&ran).unwrap();
    let o = no_browser(&["install", "--yes"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.starts_with("The browser runner needs a Chromium of a few hundred MB (with ffmpeg) from Playwright's servers"), "{}", o.stdout);
    assert!(
        ran.is_file() && marker.is_file(),
        "the download ran again and finished"
    );
    assert_eq!(
        fx.reg.requests().len(),
        1,
        "the package was not fetched again: {:?}",
        fx.reg.requests()
    );
}

#[test]
fn the_registry_stand_in_must_be_on_127_0_0_1_and_a_missing_or_old_node_is_said_plainly() {
    let fx = Fx::new(false, true);
    let mut c = fx.cmd(&["install", "--yes"]);
    c.env("TORPOR_TEST_REGISTRY", "https://registry.npmjs.org");
    let o = run_cmd(c, b"");
    assert_eq!(o.code, 1, "{}{}", o.stdout, o.stderr);
    assert!(
        o.stderr
            .contains("must be an http://127.0.0.1:PORT address"),
        "{}",
        o.stderr
    );
    assert!(fx.reg.requests().is_empty());
    fx.nothing_installed();
    let empty = fx.env.home.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let mut c = fx.cmd(&["install", "--yes"]);
    c.env("PATH", &empty);
    let o = run_cmd(c, b"");
    assert_eq!(o.code, 1);
    assert!(
        o.stderr.starts_with("Node.js is not installed"),
        "{}",
        o.stderr
    );
    fx.nothing_installed();
    for bad in [
        &[][..],
        &["install", "--no"],
        &["install", "--yes", "x"],
        &["remove"],
    ] {
        let o = fx.run(bad);
        assert_eq!(
            (o.code, o.stderr.as_str()),
            (2, "Usage: torpor runner install [--yes]\n"),
            "{bad:?}"
        );
    }
}

#[test]
fn a_node_that_is_a_version_manager_shim_is_asked_for_its_real_path_and_that_one_runs() {
    let fx = Fx::new(false, true);
    // Like asdf, mise or volta: the first `node` on the PATH finds the real one through the person's environment, which the download does not get.
    let shim = fx.bin.join("node");
    std::fs::remove_file(&shim).unwrap();
    std::fs::write(&shim, "#!/bin/sh\nexec \"$SHIM_REAL\" \"$@\"\n").unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut c = fx.cmd(&["install", "--yes"]);
    c.env("SHIM_REAL", which("node"));
    let o = run_cmd(c, b"");
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(
        o.stdout.contains("installed playwright-core 1.64.1"),
        "{}",
        o.stdout
    );
    assert!(
        fx.dir()
            .join("node_modules/playwright-core/package.json")
            .is_file()
    );
    // Without that environment the shim cannot find its node and the plain message stays.
    let fx = Fx::new(false, true);
    std::fs::remove_file(fx.bin.join("node")).unwrap();
    std::fs::write(
        fx.bin.join("node"),
        "#!/bin/sh\nexec \"$SHIM_REAL\" \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(fx.bin.join("node"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let o = fx.run(&["install", "--yes"]);
    assert_eq!(o.code, 1, "{}{}", o.stdout, o.stderr);
    assert!(
        o.stderr.starts_with("Node.js 20 or newer is needed"),
        "{}",
        o.stderr
    );
    fx.nothing_installed();
}

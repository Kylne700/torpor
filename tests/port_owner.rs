//! Port ownership (S2b.6): the port is picked just before the app starts, an app that ends at once is started once more, and a ready
//! answer that another process gave is not the app's answer. The other process is a real node process, and Torpor must leave it alone.
//! `TORPOR_TEST_PORT=P[,P]` (fault-inject builds) puts the 1st and the 2nd start on the port that process holds.
mod common;
use common::app::*;
use common::*;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// Listens on 127.0.0.1 on a free port, answers every request with `STATUS` and writes each request line to `LOG`.
const OTHER_JS: &str = "const h=require('http'),fs=require('fs');\
 const s=h.createServer((q,r)=>{fs.appendFileSync(process.env.LOG,q.method+' '+q.url+'\\n');r.statusCode=Number(process.env.STATUS);r.end('{}')});\
 s.listen(0,'127.0.0.1',()=>console.log(s.address().port));";

/// A process that is not the app and holds a port. Its own process group; the drop guard stops it.
struct Other {
    child: Child,
    port: u16,
    log: PathBuf,
    _dir: tempfile::TempDir,
}

impl Other {
    fn start(status: u16) -> Other {
        let dir = tempfile::Builder::new().prefix("torpor-other").tempdir().unwrap();
        let log = dir.path().join("seen.log");
        let mut child = Command::new("node")
            .args(["-e", OTHER_JS])
            .env("LOG", &log)
            .env("STATUS", status.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
        let port = line.trim().parse().unwrap_or_else(|_| panic!("the other process said {line:?}, not its port"));
        Other { child, port, log, _dir: dir }
    }

    /// The request lines it has answered, oldest first.
    fn seen(&self) -> Vec<String> {
        fs::read_to_string(&self.log).unwrap_or_default().lines().map(String::from).collect()
    }

    #[cfg(target_os = "linux")]
    fn alive(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }
}

impl Drop for Other {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The login promise is saved. Its app then waits `secs` before it starts (only when slow.flag exists, so saving stays fast), and the
/// other process is asked for the ready path while the app is still alive.
fn slow_login(secs: &str) -> Sim {
    let mut s = story();
    let slow = r#"{"cmd":"if [ -f slow.flag ]; then sleep SECS; fi; node server.js --port {port}","ready":"/login","timeout_s":30}"#.replace("SECS", secs);
    let o = add(&mut s, &spec("login works", &slow, "{}", LOGIN_STEPS));
    assert_eq!(o.code, 0, "{}", why(&o));
    s.outside_edit("slow.flag", "x");
    s
}

#[test]
fn an_app_that_ends_at_once_because_its_port_is_taken_is_started_once_more() {
    let s = slow_login("0.3"); // the app must end within 3 s of its start, also on a slow runner
    let other = Other::start(503);
    let c = s.cli_env(&["check", "1"], &[("TORPOR_TEST_PORT", &other.port.to_string())]);
    assert_eq!(c.code, 0, "{}", why(&c));
    assert!(c.stdout.contains("Promise #1 login works: PASS"), "{}", why(&c));
    // The first start was on the taken port: the ready path was asked there, and nothing else.
    let seen = other.seen();
    assert!(!seen.is_empty() && seen.iter().all(|l| l == "GET /login"), "{seen:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn an_answer_from_another_process_is_not_the_apps_and_the_app_is_started_once_more() {
    let s = slow_login("1");
    let mut other = Other::start(200);
    let c = s.cli_env(&["check", "1"], &[("TORPOR_TEST_PORT", &other.port.to_string())]);
    assert_eq!(c.code, 0, "{}", why(&c));
    assert!(c.stdout.contains("Promise #1 login works: PASS"), "{}", why(&c));
    // It was asked for the ready path once, and no step of the check ran against it.
    assert_eq!(other.seen(), ["GET /login"]);
    assert!(other.alive(), "Torpor stops only its own app");
}

#[cfg(target_os = "linux")]
#[test]
fn a_port_that_stays_taken_makes_the_check_unrunnable_and_the_other_process_is_left_alone() {
    let s = slow_login("1");
    let mut other = Other::start(200);
    let (p, pid) = (other.port, other.child.id());
    let c = s.cli_env(&["check", "1"], &[("TORPOR_TEST_PORT", &format!("{p},{p}"))]);
    assert_eq!(c.code, 1, "{}", why(&c));
    assert!(c.stdout.contains(&format!("UNRUNNABLE: port {p} is used by another process (pid {pid}), not by the app")), "{}", why(&c));
    assert_eq!(other.seen(), ["GET /login", "GET /login"], "one ready question for each start, no step");
    assert!(other.alive(), "Torpor stops only its own app");
}

//! The hook watchdog's kill (`kill_live_groups`) reaches an app that `App::spawn` started, from another thread, the way the
//! watchdog calls it. Its own test binary: the kill takes every app of the process, so no other test may share it.
use std::process::Command;
use std::time::Duration;
use torpor::scratch::{kill_live_groups, App};

fn alive(pid: &str) -> bool {
    let o = Command::new("ps").args(["-o", "pid,comm", "-p", pid]).output().unwrap();
    String::from_utf8_lossy(&o.stdout).lines().skip(1).any(|l| l.trim_start().starts_with(pid) && !l.contains("defunct"))
}

#[test]
fn the_watchdog_kill_reaches_a_running_app() {
    let tmp = tempfile::tempdir().unwrap();
    let env = vec![("PATH".to_string(), std::env::var("PATH").unwrap_or_default())];
    // The shell and a grandchild that ignores SIGTERM: only a SIGKILL of the whole group ends it.
    let app = App::spawn("(trap '' TERM; sleep 120) & echo child=$!; wait", tmp.path(), &env, tmp.path().join("log")).unwrap();
    let mut pid = String::new();
    for _ in 0..100 {
        if let Some(p) = app.tail().strip_prefix("child=") {
            pid = p.trim().to_string();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!pid.is_empty(), "the app reported its grandchild");
    assert!(alive(&pid));
    std::thread::spawn(kill_live_groups).join().unwrap();
    for _ in 0..100 {
        if !alive(&pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!alive(&pid), "the grandchild is still running after the watchdog kill");
    drop(app);
}

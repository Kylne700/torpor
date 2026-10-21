//! Secrets and permissions (spec 2.5, 5.6 and the A7 grep): no secret file content, no canary from a prompt or a
//! Bash command reaches the store in the clear (db, -wal, -shm, objects, faults.log); secrets are still saved,
//! encrypted and under a keyed id; the store and the key file are private.
mod common;
use common::*;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// `tc_` + 32 hex characters, with both letters and digits (the preview masker hides runs like that).
fn canary(n: u8) -> String {
    format!("tc_{n:02x}9f3b7a1c5e2d4b6a8c0e1f3a5b7c9d")
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        out.push(p.clone());
        if e.file_type().unwrap().is_dir() {
            walk(&p, out);
        }
    }
}

/// Files under `root` that contain `needle`.
fn hits(root: &Path, needle: &str) -> Vec<PathBuf> {
    let mut all = vec![];
    walk(root, &mut all);
    let n = needle.as_bytes();
    all.into_iter()
        .filter(|p| p.is_file())
        .filter(|p| std::fs::read(p).is_ok_and(|b| b.windows(n.len()).any(|w| w == n)))
        .collect()
}

#[test]
fn secrets_never_reach_the_store_in_the_clear() {
    let mut sim = Sim::new();
    let proj = sim.env.proj.clone();
    let c: Vec<String> = (1..=8).map(canary).collect();
    write_files(
        &proj,
        &[
            (".env", &format!("TORPOR_CANARY={}\nOTHER=1\n", c[0])),
            (".env.production", &format!("KEY={}\n", c[1])),
            ("certs/server.pem", &format!("-----BEGIN KEY-----\n{}\n-----END KEY-----\n", c[2])),
            (".npmrc", &format!("//registry/:_authToken={}\n", c[3])),
            ("service-account-prod.json", &format!("{{\"k\":\"{}\"}}\n", c[4])),
            (".aws/config", &format!("[default]\naws_secret_access_key={}\n", c[5])),
            ("notes/plain.txt", &format!("control {}\n", c[6])),
            ("src/main.rs", "fn main() {}\n"),
        ],
    );
    std::fs::set_permissions(proj.join(".env"), std::fs::Permissions::from_mode(0o600)).unwrap();

    sim.session_start();
    sim.prompt(&format!("please use {} for the deploy", c[0]));
    sim.bash(&format!("echo {} > /dev/null", c[0]));
    sim.edit(".env", &format!("TORPOR_CANARY={}\n", c[7]));
    sim.bash("rm .env.production");
    sim.edit("src/main.rs", "fn main() { /* edited */ }\n");
    sim.stop();
    sim.session_end("other");
    let mut t = sim.second_session();
    t.session_start();
    t.prompt(&format!("again with {}", c[3]));
    t.bash(&format!("export TOKEN={} && true", c[4]));
    t.stop();

    let thome = &sim.env.thome;
    // The control proves the grep can see: a plain file's text is in the store (compressed, but short text stays literal).
    assert!(!hits(thome, &c[6]).is_empty(), "negative control: the plain note's text should be found in the store");
    for (i, canary) in c.iter().enumerate() {
        if i == 6 {
            continue;
        }
        let found = hits(thome, canary);
        assert!(found.is_empty(), "canary {i} ({canary}) found in the store: {found:?}");
    }
    // Not in the key folder either, and the key itself is not in the store.
    let key_text = std::fs::read_to_string(&sim.env.key).unwrap();
    let key_hex = key_text.split_whitespace().nth(2).unwrap().to_string();
    assert_eq!(key_hex.len(), 64);
    assert!(hits(thome, &key_hex).is_empty(), "the key is stored next to the data");

    // The secrets are saved, though: the last point matches the project byte for byte (secret files are read back
    // through the key), and their ids are keyed, not the plain BLAKE3 of the content.
    t.verify_last_point();
    let secret_files = [".env", ".npmrc", "service-account-prod.json", ".aws/config", "certs/server.pem"];
    for f in secret_files {
        let plain = blake3::hash(&std::fs::read(proj.join(f)).unwrap());
        let n = t.conn().query_row("SELECT COUNT(*) FROM blobs WHERE id=?1", [plain.as_bytes().as_slice()], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(n, 0, "{f}: a blob has the unkeyed BLAKE3 of the secret as its id");
    }
    let plain = blake3::hash(&std::fs::read(proj.join("notes/plain.txt")).unwrap());
    let n = t.conn().query_row("SELECT COUNT(*) FROM blobs WHERE id=?1", [plain.as_bytes().as_slice()], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(n, 1, "control: an ordinary file is stored under its plain BLAKE3");
    assert!(sim.faults().is_empty(), "faults: {:?}", sim.faults());
}

#[test]
fn the_store_and_the_key_are_private() {
    let mut sim = Sim::new();
    let proj = sim.env.proj.clone();
    // 200 KiB of noise does not compress below the inline limit, so it becomes a loose file under objects/.
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let noise: Vec<u8> = (0..200 * 1024)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect();
    write_files(&proj, &[(".env", "A=1\n"), ("src/a.rs", "fn a() {}\n")]);
    std::fs::write(proj.join("blob.bin"), &noise).unwrap();
    sim.session_start();
    sim.prompt("one");
    sim.edit("src/a.rs", "fn a() { 1 }\n");
    sim.stop();
    sim.session_end("other");

    let mut all = vec![];
    walk(&sim.env.thome, &mut all);
    let loose = all.iter().filter(|p| p.is_file() && p.parent().unwrap().parent().unwrap().file_name().is_some_and(|n| n == "objects")).count();
    assert!(loose >= 1, "the scenario needs a loose object");
    for name in ["torpor.db", "torpor.db-wal", "torpor.db-shm"] {
        assert!(all.iter().any(|p| p.file_name().is_some_and(|n| n == name)), "{name} should be on disk between hooks");
    }
    let mut bad = vec![];
    for p in all.iter().chain(std::iter::once(&sim.env.thome)) {
        let m = std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777;
        let want = if p.is_dir() { 0o700 } else { 0o600 };
        if m != want {
            bad.push(format!("{} is {m:o}, want {want:o}", p.display()));
        }
    }
    assert!(bad.is_empty(), "store permissions:\n  {}", bad.join("\n  "));

    let km = std::fs::metadata(&sim.env.key).unwrap().permissions().mode() & 0o7777;
    assert_eq!(km, 0o600, "the key file is {km:o}");
    let kd = std::fs::metadata(sim.env.key.parent().unwrap()).unwrap().permissions().mode() & 0o7777;
    assert_eq!(kd, 0o700, "the key folder is {kd:o}");
    let text = std::fs::read_to_string(&sim.env.key).unwrap();
    let parts: Vec<&str> = text.split_whitespace().collect();
    assert!(parts.len() == 3 && parts[0] == "torpor-key-v1" && parts[1].len() == 8 && parts[2].len() == 64, "key file format: {text:?}");
    assert!(parts[1].chars().chain(parts[2].chars()).all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()));
    assert!(!sim.env.key.starts_with(&sim.env.thome), "the key must live outside the store");
}

/// A7, restore half: delete a secret, go back, and it is back byte for byte and private, while the canary (also typed in
/// a prompt and a shell command) never shows up in any store file, the restore's own output, or a stage folder.
#[test]
fn a_restored_secret_comes_back_exact_and_private_and_never_clear_in_the_store() {
    let mut sim = Sim::new();
    let proj = sim.env.proj.clone();
    let tc = canary(9);
    let env_bytes = format!("TORPOR_CANARY={tc}\nOTHER=1\n");
    write_files(&proj, &[(".env", &env_bytes), ("src/main.rs", "fn main() {}\n")]);
    std::fs::set_permissions(proj.join(".env"), std::fs::Permissions::from_mode(0o640)).unwrap();
    sim.session_start();
    sim.prompt(&format!("please keep {tc} out of git"));
    assert_eq!(sim.bash(&format!("echo {tc} > /dev/null")), 0);
    sim.edit("src/main.rs", "fn main() { /* one */ }\n");
    sim.stop();
    sim.prompt("clean up");
    assert_eq!(sim.bash("rm .env"), 0);
    sim.stop();
    assert!(!proj.join(".env").exists());

    let o = sim.cli(&["back", "1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!(std::fs::read(proj.join(".env")).unwrap(), env_bytes.as_bytes(), "the secret comes back byte for byte");
    let mode = std::fs::metadata(proj.join(".env")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "a restored secret is private whatever its old mode was");
    assert!(!o.stdout.contains(&tc) && !o.stderr.contains(&tc), "the restore report shows the secret: {}", o.stdout);
    assert!(!proj.join(".torpor-stage").exists());
    let found = hits(&sim.env.thome, &tc);
    assert!(found.is_empty(), "canary found after the restore in {found:?}");
    sim.verify_last_point();
}

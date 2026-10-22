//! The promise vault (spec 13): check specs as immutable files plus a keyed hash chain.
//! Tamper evidence, not prevention: a change made outside Torpor shows up at the next SessionStart or Stop.
use crate::spec::Linted;
use crate::store::{Key, Ws, meta_get, meta_set, mkdir_private};
use crate::util::{Error, Result, hex};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

pub fn dir(ws: &Ws) -> PathBuf {
    ws.dir.join("vault")
}

fn mac(key: &Key, seq: u64, op: &str, id: &str, prev: &str) -> String {
    let k = blake3::derive_key("torpor vault mac v1", &key.bytes);
    hex(blake3::keyed_hash(&k, format!("{seq}|{op}|{id}|{prev}").as_bytes()).as_bytes())
}

struct Line {
    seq: u64,
    id: String,
    mac: String,
}

/// Parse the chain; Err(text) names the first line that does not hold.
fn read_chain(ws: &Ws, key: &Key) -> std::result::Result<Vec<Line>, String> {
    let text = match fs::read_to_string(dir(ws).join("chain")) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err("the chain file cannot be read".into()),
    };
    let mut out: Vec<Line> = Vec::new();
    for (n, l) in text.lines().enumerate() {
        let bad = || format!("chain line {} does not hold", n + 1);
        let v: serde_json::Value = serde_json::from_str(l).map_err(|_| bad())?;
        let (seq, op, id, prev, m) = (v["seq"].as_u64(), v["op"].as_str(), v["id"].as_str(), v["prev"].as_str(), v["mac"].as_str());
        let (Some(seq), Some(op), Some(id), Some(prev), Some(m)) = (seq, op, id, prev, m) else { return Err(bad()) };
        let want_prev = out.last().map_or("", |p| p.mac.as_str());
        if seq != n as u64 + 1 || prev != want_prev || mac(key, seq, op, id, prev) != m {
            return Err(bad());
        }
        out.push(Line { seq, id: id.to_string(), mac: m.to_string() });
    }
    Ok(out)
}

/// Store a linted spec. Idempotent: the same spec twice adds one chain line.
pub fn put(ws: &Ws, key: &Key, l: &Linted) -> Result<()> {
    let d = dir(ws);
    mkdir_private(&d)?;
    let file = d.join(format!("{}.json", l.id));
    match OpenOptions::new().write(true).create_new(true).mode(0o400).open(&file) {
        Ok(mut f) => f.write_all(&l.canon)?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let mut chain = OpenOptions::new().read(true).append(true).create(true).mode(0o600).open(d.join("chain"))?;
    chain.lock()?; // one writer at a time; released when `chain` drops
    let lines = read_chain(ws, key).map_err(Error::msg)?;
    if lines.iter().any(|x| x.id == l.id) {
        return Ok(());
    }
    let (seq, prev) = lines.last().map_or((1, String::new()), |x| (x.seq + 1, x.mac.clone()));
    let m = mac(key, seq, "add", &l.id, &prev);
    chain.write_all(format!("{}\n", serde_json::json!({"seq": seq, "op": "add", "id": l.id, "prev": prev, "mac": m})).as_bytes())?;
    chain.sync_all()?;
    meta_set(&ws.conn, "vault_head", &format!("{seq}:{m}"))?;
    Ok(())
}

/// The spec bytes of `id`, checked against their hash.
pub fn get(ws: &Ws, id: &str) -> Result<Vec<u8>> {
    if id.len() != 64 || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::msg("not a check id"));
    }
    let b = fs::read(dir(ws).join(format!("{id}.json"))).map_err(|_| Error::NotFound(format!("check {}", &id[..12])))?;
    if hex(blake3::hash(&b).as_bytes()) != id {
        return Err(Error::msg("the stored check was changed outside Torpor"));
    }
    Ok(b)
}

pub struct Report {
    pub problems: Vec<String>,
    /// The first promise whose check file is affected, if one can be named.
    pub promise: Option<i64>,
}

impl Report {
    /// The systemMessage for a mismatch (spec 13), or None when everything holds.
    pub fn notice(&self) -> Option<String> {
        if self.problems.is_empty() {
            return None;
        }
        let at = self.promise.map(|p| format!(" (#{p})")).unwrap_or_default();
        Some(format!("Torpor's promise records were changed outside Torpor{at}. Check torpor promise list."))
    }
}

/// Check the vault against its chain and the copy of the head in `meta`. Nothing stored yet is fine.
pub fn verify(ws: &Ws, key: Option<&Key>) -> Report {
    let head = meta_get(&ws.conn, "vault_head");
    let d = dir(ws);
    if head.is_none() && !d.exists() {
        return Report { problems: Vec::new(), promise: None };
    }
    let mut problems: Vec<String> = Vec::new();
    let mut affected: Vec<String> = Vec::new();
    let Some(key) = key else {
        return Report { problems: vec!["the key file is gone, so the records cannot be checked".into()], promise: None };
    };
    let lines = match read_chain(ws, key) {
        Ok(l) => l,
        Err(e) => {
            problems.push(e);
            Vec::new()
        }
    };
    for l in &lines {
        if get(ws, &l.id).is_err() {
            problems.push(format!("check file {} is missing or changed", &l.id[..12]));
            affected.push(l.id.clone());
        }
    }
    if let Ok(rd) = fs::read_dir(&d) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if let Some(id) = n.strip_suffix(".json") {
                if !lines.iter().any(|l| l.id == id) {
                    problems.push(format!("check file {} is not in the chain", &id[..id.len().min(12)]));
                    affected.push(id.to_string());
                }
            }
        }
    }
    // The second copy of the head: it may be one entry behind the chain (a crash between the two writes) but never ahead of it.
    if let Some(h) = head {
        let ok = h.split_once(':').and_then(|(s, m)| Some((s.parse::<u64>().ok()?, m))).is_some_and(|(s, m)| lines.iter().any(|l| l.seq == s && l.mac == m));
        if !ok && problems.is_empty() {
            problems.push("the chain does not match its second copy".into());
        }
    }
    let promise = affected.iter().find_map(|id| ws.conn.query_row("SELECT id FROM promises WHERE spec_id=?1 ORDER BY id LIMIT 1", [id], |r| r.get::<_, i64>(0)).ok());
    Report { problems, promise }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec;

    fn linted(name: &str) -> Linted {
        let s = format!(
            r#"{{"v":1,"name":"{name}","why":"w","start":{{"cmd":"node server.js","ready":"/"}},"steps":[{{"http":{{"path":"/"}},"expect":{{"status":200}},"why":"up"}}]}}"#
        );
        spec::lint(&s).unwrap()
    }

    #[test]
    fn the_chain_catches_edits_deletions_and_extras() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Ws::open_in(&tmp.path().join("home"), &tmp.path().join("proj"), true).unwrap();
        let key = Key { bytes: [7; 32], id: "k".into() };
        assert!(verify(&ws, Some(&key)).problems.is_empty(), "an empty vault is fine");
        let (a, b) = (linted("one"), linted("two"));
        put(&ws, &key, &a).unwrap();
        put(&ws, &key, &a).unwrap(); // same spec: still one line
        put(&ws, &key, &b).unwrap();
        assert_eq!(fs::read_to_string(dir(&ws).join("chain")).unwrap().lines().count(), 2);
        assert!(verify(&ws, Some(&key)).problems.is_empty());
        assert_eq!(get(&ws, &a.id).unwrap(), a.canon);
        assert!(get(&ws, "zz").is_err());

        // A promise row lets the notice name the promise.
        ws.conn.execute("INSERT INTO promises(name, source, spec_id) VALUES('two', 'praise', ?1)", [&b.id]).unwrap();

        // 1. A check file rewritten in place.
        let f = dir(&ws).join(format!("{}.json", b.id));
        let mut perm = fs::metadata(&f).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o600);
        fs::set_permissions(&f, perm).unwrap();
        fs::write(&f, b"{}").unwrap();
        let r = verify(&ws, Some(&key));
        assert_eq!(r.promise, Some(1));
        assert!(r.notice().unwrap().contains("changed outside Torpor (#1)"), "{:?}", r.problems);
        fs::write(&f, &b.canon).unwrap();
        assert!(verify(&ws, Some(&key)).problems.is_empty());

        // 2. A chain line cut off (the head in the database no longer matches).
        let chain = dir(&ws).join("chain");
        let text = fs::read_to_string(&chain).unwrap();
        fs::write(&chain, text.lines().next().unwrap().to_string() + "\n").unwrap();
        assert!(!verify(&ws, Some(&key)).problems.is_empty());
        fs::write(&chain, &text).unwrap();

        // 3. A forged line (wrong key) and an extra file.
        let forged = text.replace("\"id\":\"", "\"id\":\"0");
        fs::write(&chain, forged).unwrap();
        assert!(!verify(&ws, Some(&key)).problems.is_empty());
        fs::write(&chain, &text).unwrap();
        assert!(verify(&ws, Some(&key)).problems.is_empty());
        fs::write(dir(&ws).join(format!("{}.json", "ab".repeat(32))), b"{}").unwrap();
        assert!(verify(&ws, Some(&key)).problems.iter().any(|p| p.contains("not in the chain")));
        // Another key cannot read the chain.
        let other = Key { bytes: [8; 32], id: "o".into() };
        assert!(!verify(&ws, Some(&other)).problems.is_empty());
        assert!(!verify(&ws, None).problems.is_empty());
    }
}

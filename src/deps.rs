//! The dependency record (spec 17): which project files a Node app ran during one check, read from V8 coverage. Best effort: any problem gives
//! None, and a promise with no record is always picked at the gate (spec 18). No browser coverage.
use crate::runner::Verdict;
use crate::scratch::{self, Opts};
use crate::spec::Spec;
use crate::store::*;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

/// Node writes its coverage when the process exits, and a server that dies of SIGTERM does not exit; this makes it exit (measured on node 24).
const EXIT_ON_TERM: &str = "process.on('SIGTERM', () => process.exit(0));\n";

/// One extra run of the working version with coverage on. Its result is not a judgement and is not stored; only a PASS is used.
pub fn record(ws: &Ws, spec: &Spec, tree: &[u8; 32]) -> Option<Vec<String>> {
    let hold = scratch::Scratch::create(ws).ok()?;
    let (cov, exit) = (hold.dir.join("cov"), hold.dir.join("exit.cjs"));
    mkdir_private(&cov).ok()?;
    std::fs::write(&exit, EXIT_ON_TERM).ok()?;
    let (cov_s, exit_s) = (cov.to_str()?, exit.to_str()?);
    if cov_s.contains(['"', '\\']) || exit_s.contains(['"', '\\']) {
        return None;
    }
    let env = vec![("NODE_V8_COVERAGE".to_string(), cov_s.to_string()), ("NODE_OPTIONS".to_string(), format!("--require \"{exit_s}\""))];
    let r = scratch::run_once_with(ws, spec, tree, None, &Opts { overlay: Vec::new(), env });
    if r.judged.verdict != Verdict::Pass {
        return None;
    }
    // The app is stopped and its copy is gone by now; the coverage files name that copy as <ws>/scratch/<run>/app/<file>.
    from_coverage(&cov, &std::fs::canonicalize(ws.dir.join("scratch")).ok()?)
}

/// Files with any executed function, plus the sources named by source maps of those files (spec 17). shortcut: a map's `sourceRoot` is
/// ignored; upgrade when a real project needs it. None when no project file was seen.
fn from_coverage(cov: &Path, base: &Path) -> Option<Vec<String>> {
    let mut found = BTreeSet::new();
    for e in std::fs::read_dir(cov).ok()?.flatten() {
        let j: Value = serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()?;
        for s in j.get("result")?.as_array()? {
            let ran = s["functions"].as_array().is_some_and(|f| f.iter().any(|f| f["ranges"].as_array().is_some_and(|r| r.iter().any(|r| r["count"].as_i64().unwrap_or(0) > 0))));
            if let Some(rel) = s["url"].as_str().and_then(|u| project_file(u, base)).filter(|_| ran) {
                found.insert(rel);
            }
        }
        for (url, m) in j.get("source-map-cache").and_then(Value::as_object).into_iter().flatten() {
            let Some(from) = project_file(url, base) else { continue };
            let sources = m["data"]["sources"].as_array().into_iter().flatten().filter_map(Value::as_str);
            found.extend(sources.filter_map(|src| beside(&from, src, base)));
        }
    }
    (!found.is_empty()).then(|| found.into_iter().collect())
}

/// `file:///<base>/<run>/app/<rel>` becomes `<rel>`, when a check may see that file (not a dependency folder, not `.env*`).
fn project_file(url: &str, base: &Path) -> Option<String> {
    let path = decode(url.strip_prefix("file://")?)?;
    let mut parts = Path::new(&path).strip_prefix(base).ok()?.components();
    parts.next()?;
    (parts.next()?.as_os_str() == "app").then_some(())?;
    let rel = parts.as_path().to_str()?.to_string();
    scratch::overlay_ok(&rel).then_some(rel)
}

/// A source named in a map is relative to the file the map belongs to (`from`), or a file: URL.
fn beside(from: &str, src: &str, base: &Path) -> Option<String> {
    if src.starts_with("file://") {
        return project_file(src, base);
    }
    if src.contains("://") || src.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = from.split('/').collect();
    parts.pop();
    for c in src.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            c => parts.push(c),
        }
    }
    let rel = parts.join("/");
    scratch::overlay_ok(&rel).then_some(rel)
}

fn decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(s.get(i + 1..i + 3)?, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_files_become_project_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let url = |rel: &str| format!("file://{}/run1/app/{rel}", base.display());
        let cov = base.join("cov");
        std::fs::create_dir(&cov).unwrap();
        let f = |count: i64| serde_json::json!([{"functionName": "", "ranges": [{"startOffset": 0, "endOffset": 9, "count": count}]}]);
        let doc = serde_json::json!({
            "result": [
                {"url": url("server.js"), "functions": f(1)},
                {"url": url("lib/a%20b.js"), "functions": f(2)},
                {"url": url("never.js"), "functions": f(0)},
                {"url": url("node_modules/x/index.js"), "functions": f(1)},
                {"url": url(".env.local"), "functions": f(1)},
                {"url": format!("file://{}/run1/exit.cjs", base.display()), "functions": f(1)},
                {"url": "node:internal/main", "functions": f(1)},
            ],
            "source-map-cache": {
                url("dist/app.js"): {"data": {"sources": ["../src/app.ts", "./util.ts", "../../out.ts", "webpack://x/y.ts", url("src/z.ts")]}},
                "file:///elsewhere/x.js": {"data": {"sources": ["q.ts"]}},
            },
        });
        std::fs::write(cov.join("coverage-1.json"), doc.to_string()).unwrap();
        let got = from_coverage(&cov, &base).unwrap();
        assert_eq!(got, ["dist/util.ts", "lib/a b.js", "server.js", "src/app.ts", "src/z.ts"]);
        std::fs::write(cov.join("coverage-1.json"), "{\"result\": []}").unwrap();
        assert_eq!(from_coverage(&cov, &base), None, "nothing seen is no record");
        std::fs::write(cov.join("coverage-1.json"), "not json").unwrap();
        assert_eq!(from_coverage(&cov, &base), None, "a file that cannot be read is no record");
    }
}

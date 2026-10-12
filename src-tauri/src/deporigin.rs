//! Dependency provenance: where an edge came from. Two sources today —
//! the SEED_LINKS constant in importers.rs and per-project `.atlas-deps`
//! marker files; file beats seed on the (from, to, kind) key.
use crate::util::norm;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;

pub const SEED_FILE: &str = "src-tauri/src/importers.rs";
pub const SEED_LABEL: &str = "SEED_LINKS в importers.rs";
pub const FILE_LABEL: &str = "маркер-файл проекта";

/// A row of the `links` table (all of them are seeds today).
pub struct LinkRow {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub note: Option<String>,
}

/// Locate each seed tuple literally in the importers source at request time;
/// a tuple that cannot be found keeps a line-less ref (a stale number is
/// worse than no number).
pub fn seed_lines(
    text: &str,
    seeds: &[(&str, &str, &str)],
) -> HashMap<(String, String, String), (usize, String)> {
    let mut out = HashMap::new();
    for (f, t, k) in seeds {
        let needle = format!("(\"{f}\", \"{t}\", \"{k}\")");
        for (i, line) in text.lines().enumerate() {
            if line.contains(&needle) {
                out.insert(
                    (f.to_string(), t.to_string(), k.to_string()),
                    (i + 1, line.trim().to_string()),
                );
                break;
            }
        }
    }
    out
}

fn seed_origin(
    idx: &HashMap<(String, String, String), (usize, String)>,
    from: &str,
    to: &str,
    kind: &str,
) -> Value {
    match idx.get(&(from.to_string(), to.to_string(), kind.to_string())) {
        Some((n, raw)) => json!({
            "source": "seed",
            "ref": format!("{SEED_FILE}:{n}"),
            "label": SEED_LABEL,
            "raw": raw,
        }),
        None => json!({
            "source": "seed",
            "ref": format!("{SEED_FILE} (SEED_LINKS)"),
            "label": SEED_LABEL,
            "raw": format!("(\"{from}\", \"{to}\", \"{kind}\")"),
        }),
    }
}

/// One `.atlas-deps` line: `<strict|soft> <target> [free-text note]`.
/// `#` starts a comment, blank lines are ignored.
/// Ok(None) — comment/blank; Err(reason) — malformed line.
pub fn parse_marker_line(line: &str) -> Result<Option<(String, String, Option<String>)>, String> {
    let body = line.split('#').next().unwrap_or("").trim();
    if body.is_empty() {
        return Ok(None);
    }
    let mut it = body.split_whitespace();
    let kind = it.next().unwrap_or_default();
    if kind != "strict" && kind != "soft" {
        return Err(format!("тип должен быть strict|soft, получено '{kind}'"));
    }
    let Some(target) = it.next() else {
        return Err("не указан целевой проект".into());
    };
    let note: Option<String> = {
        let rest: Vec<&str> = it.collect();
        rest.is_empty()
            .then_some(None)
            .unwrap_or_else(|| Some(rest.join(" ")))
    };
    Ok(Some((kind.to_string(), target.to_string(), note)))
}

type MarkerEdge = (String, String, String, Option<String>, String, String);

/// Read `<path>/.atlas-deps` for every project with a path. Unknown target or
/// malformed line → skipped with a human-readable warning. Returns
/// (from, to, kind, note, ref, raw) edges plus warnings.
pub fn collect_markers(
    paths: &HashMap<String, String>,
    names: &[String],
) -> (Vec<MarkerEdge>, Vec<String>) {
    let canon: HashMap<String, String> = names.iter().map(|n| (norm(n), n.clone())).collect();
    let mut edges: Vec<MarkerEdge> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    for (proj, path) in paths {
        let fpath = Path::new(path).join(".atlas-deps");
        let text = match std::fs::read_to_string(&fpath) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                warnings.push(format!("{}: не прочитан: {e}", fpath.display()));
                continue;
            }
        };
        for (i, line) in text.lines().enumerate() {
            let n = i + 1;
            match parse_marker_line(line) {
                Ok(None) => {}
                Ok(Some((kind, target, note))) => match canon.get(&norm(&target)) {
                    None => warnings.push(format!(
                        "{}:{n}: неизвестный проект '{target}' — ребро пропущено",
                        fpath.display()
                    )),
                    Some(c) if *c == *proj => warnings.push(format!(
                        "{}:{n}: проект зависит от самого себя — ребро пропущено",
                        fpath.display()
                    )),
                    Some(c) => edges.push((
                        proj.clone(),
                        c.clone(),
                        kind,
                        note,
                        format!("{}:{n}", fpath.display()),
                        line.to_string(),
                    )),
                },
                Err(reason) => warnings.push(format!(
                    "{}:{n}: {reason} — строка пропущена",
                    fpath.display()
                )),
            }
        }
    }
    (edges, warnings)
}

/// Merged /api/deps edge list with `origin` per edge, deterministic order,
/// plus warnings from marker files. File edges beat seed edges on the
/// (from, to, kind) key; seed edges not literally found in the source keep a
/// line-less ref.
pub fn build_edges(
    links: &[LinkRow],
    paths: &HashMap<String, String>,
    names: &[String],
    importers_text: Option<&str>,
) -> (Vec<Value>, Vec<String>) {
    let idx = importers_text.map_or_else(HashMap::new, |t| {
        seed_lines(t, &crate::importers::SEED_LINKS)
    });
    let mut map: HashMap<(String, String, String), Value> = HashMap::new();
    for l in links {
        let origin = seed_origin(&idx, &l.from, &l.to, &l.kind);
        map.insert(
            (l.from.clone(), l.to.clone(), l.kind.clone()),
            json!({"from": l.from, "to": l.to, "kind": l.kind, "note": l.note, "origin": origin}),
        );
    }
    let (markers, warnings) = collect_markers(paths, names);
    for (f, t, k, note, reference, raw) in markers {
        map.insert(
            (f.clone(), t.clone(), k.clone()),
            json!({
                "from": f, "to": t, "kind": k, "note": note,
                "origin": {"source": "file", "ref": reference, "label": FILE_LABEL, "raw": raw},
            }),
        );
    }
    let mut edges: Vec<Value> = map.into_values().collect();
    let key = |v: &Value| {
        (
            v["from"].as_str().unwrap_or("").to_string(),
            v["to"].as_str().unwrap_or("").to_string(),
            v["kind"].as_str().unwrap_or("").to_string(),
        )
    };
    edges.sort_by(|a, b| key(a).cmp(&key(b)));
    (edges, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::importers::SEED_LINKS;

    #[test]
    fn marker_line_parsing() {
        assert_eq!(
            parse_marker_line("strict lamplife нужен для сборки"),
            Ok(Some(("strict".into(), "lamplife".into(), Some("нужен для сборки".into()))))
        );
        assert_eq!(parse_marker_line("soft jobseeker"), Ok(Some(("soft".into(), "jobseeker".into(), None))));
        assert_eq!(parse_marker_line(""), Ok(None));
        assert_eq!(parse_marker_line("   "), Ok(None));
        assert_eq!(parse_marker_line("# комментарий целиком"), Ok(None));
        assert_eq!(parse_marker_line("strict bqn # хвост — комментарий"), Ok(Some(("strict".into(), "bqn".into(), None))));
        assert!(parse_marker_line("urgent lamplife").is_err(), "kind must be strict|soft");
        assert!(parse_marker_line("strict").is_err(), "target required");
    }

    #[test]
    fn seed_lines_finds_real_importers_lines() {
        let text = include_str!("importers.rs");
        let idx = seed_lines(text, &SEED_LINKS);
        assert_eq!(idx.len(), SEED_LINKS.len(), "every seed tuple must be located");
        let (n, raw) = idx
            .get(&("emigate".to_string(), "lamplife".to_string(), "strict".to_string()))
            .unwrap();
        assert!(*n >= 19, "SEED_LINKS tuples live after the const decl, got line {n}");
        assert!(raw.contains("(\"emigate\", \"lamplife\", \"strict\")"), "raw is the literal line: {raw}");
    }

    #[test]
    fn build_edges_file_beats_seed_and_warns() {
        let links = vec![LinkRow {
            from: "emigate".into(),
            to: "lamplife".into(),
            kind: "strict".into(),
            note: None,
        }];
        let mut paths = HashMap::new();
        paths.insert("emigate".to_string(), "/tmp/emigate-test".into());
        std::fs::create_dir_all("/tmp/emigate-test").unwrap();
        std::fs::write(
            "/tmp/emigate-test/.atlas-deps",
            "# ручные рёбра\nstrict finance — потому что API\nstrict lamplife (перекрывает сид)\n",
        )
        .unwrap();
        let names = vec!["emigate".to_string(), "finance".to_string(), "lamplife".to_string()];
        let (edges, warnings) = build_edges(&links, &paths, &names, Some("pub const SEED_LINKS: [(&str, &str, &str); 5] = [\n    (\"emigate\", \"lamplife\", \"strict\"),\n];"));
        let _ = std::fs::remove_dir_all("/tmp/emigate-test");
        assert_eq!(edges.len(), 2, "seed lamplife overridden by file; finance added: {edges:?}");
        let fin = edges.iter().find(|e| e["to"] == "finance").unwrap();
        assert_eq!(fin["origin"]["source"], "file");
        assert_eq!(fin["origin"]["ref"], "/tmp/emigate-test/.atlas-deps:2");
        assert_eq!(fin["note"], "— потому что API");
        let lam = edges.iter().find(|e| e["to"] == "lamplife").unwrap();
        assert_eq!(lam["origin"]["source"], "file", "file beats seed");
        assert_eq!(lam["origin"]["raw"], "strict lamplife (перекрывает сид)");
        assert!(warnings.is_empty(), "no warnings expected: {warnings:?}");
    }

    #[test]
    fn build_edges_seed_fallback_and_marker_warnings() {
        // source text WITHOUT the tuple → line-less seed ref
        let links = vec![LinkRow {
            from: "a".into(),
            to: "b".into(),
            kind: "strict".into(),
            note: None,
        }];
        let names = vec!["a".into(), "b".into()];
        let mut paths = HashMap::new();
        paths.insert("a".to_string(), "/tmp/a-test-marker".into());
        std::fs::create_dir_all("/tmp/a-test-marker").unwrap();
        std::fs::write(
            "/tmp/a-test-marker/.atlas-deps",
            "soft b ок\nstrict ghost нет такого проекта\nсойти с ума\nstrict a сам себе\n",
        )
        .unwrap();
        let (edges, warnings) = build_edges(&links, &paths, &names, Some("// no seeds here"));
        let _ = std::fs::remove_dir_all("/tmp/a-test-marker");
        assert_eq!(edges.len(), 2, "seed strict + file soft: {edges:?}");
        let seed = edges.iter().find(|e| e["kind"] == "strict").unwrap();
        assert_eq!(seed["origin"]["ref"], format!("{SEED_FILE} (SEED_LINKS)"));
        assert_eq!(seed["origin"]["raw"], "(\"a\", \"b\", \"strict\")");
        let file = edges.iter().find(|e| e["kind"] == "soft").unwrap();
        assert_eq!(file["origin"]["source"], "file");
        assert_eq!(warnings.len(), 3, "unknown target + malformed + self-dep: {warnings:?}");
        assert!(warnings[0].contains("ghost"));
        assert!(warnings.iter().any(|w| w.contains(":3:")));
        assert!(warnings.iter().any(|w| w.contains("самого себя")));
    }

    #[test]
    fn edges_are_deterministically_ordered() {
        let links = vec![
            LinkRow { from: "z".into(), to: "a".into(), kind: "soft".into(), note: None },
            LinkRow { from: "a".into(), to: "b".into(), kind: "strict".into(), note: None },
        ];
        let (edges, _) = build_edges(&links, &HashMap::new(), &["a".into(), "z".into()], None);
        let keys: Vec<String> = edges
            .iter()
            .map(|e| format!("{}→{}", e["from"].as_str().unwrap(), e["to"].as_str().unwrap()))
            .collect();
        assert_eq!(keys, vec!["a→b", "z→a"]);
    }
}

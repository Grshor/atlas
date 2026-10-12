//! Shared helpers: string normalization, subprocess execution, time, URI decoding.
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Python `re.sub(r"\s+", " ", s)` after strip/lower/ё→е.
pub fn norm(name: &str) -> String {
    let s = name.trim().to_lowercase().replace('ё', "е");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Run argv (no shell), capture output, never panic. Returns stdout on exit 0.
/// Mirrors importers._run: any failure → None. `errors=replace` → lossy UTF-8.
pub async fn run_out(argv: &[String], timeout: Duration) -> Option<String> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => out,
        _ => return None,
    };
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Mirrors focus._run_cmd: merged stdout+stderr, (ok, text) tuple, never raises.
pub async fn run_cmd(argv: &[String], timeout: Duration) -> (bool, String) {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return (false, format!("binary not found: {}", argv[0]))
        }
        _ => {
            return (
                false,
                format!(
                    "timeout after {:.1}s: {}",
                    timeout.as_secs_f64(),
                    argv.join(" ")
                ),
            )
        }
    };
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    let text = text.trim().to_string();
    if out.status.success() {
        (true, text)
    } else {
        (
            false,
            format!("exit {}: {}", out.status.code().unwrap_or(-1), text),
        )
    }
}

// ------------------------------------------------------------------ time ----

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe as i64 - 719_468
}

/// ISO-8601 → unix seconds. Numeric ms/s epoch values pass through like Python.
/// Naive timestamps (no zone) are treated as UTC — omp transcripts carry `Z`.
pub fn iso_to_unix(ts: &serde_json::Value) -> Option<f64> {
    if ts.is_null() {
        return None;
    }
    if let Some(n) = ts.as_f64() {
        return Some(if n > 1e12 { n / 1000.0 } else { n });
    }
    let s = ts.as_str()?;
    if s.is_empty() {
        return None;
    }
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let mo: u32 = s.get(5..7)?.parse().ok()?;
    let d: u32 = s.get(8..10)?.parse().ok()?;
    let h: i64 = s.get(11..13)?.parse().ok()?;
    let mi: i64 = s.get(14..16)?.parse().ok()?;
    let sec: i64 = s.get(17..19)?.parse().ok()?;
    let mut rest = &s[19..];
    let mut frac = 0.0f64;
    if let Some(stripped) = rest.strip_prefix('.') {
        let end = stripped
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(stripped.len());
        if end > 0 {
            frac = format!("0.{}", &stripped[..end]).parse().unwrap_or(0.0);
            rest = &stripped[end..];
        }
    }
    let offset: i64 = if rest.is_empty() {
        0
    } else if rest == "Z" || rest == "z" {
        0
    } else if rest.starts_with('+') || rest.starts_with('-') {
        let sign = if rest.starts_with('-') { -1 } else { 1 };
        let digits: String = rest[1..].chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.len() < 2 {
            return None;
        }
        let hh: i64 = digits[0..2].parse().ok()?;
        let mm: i64 = if digits.len() >= 4 {
            digits[2..4].parse().unwrap_or(0)
        } else {
            0
        };
        sign * (hh * 3600 + mm * 60)
    } else {
        return None;
    };
    let days = days_from_civil(y, mo, d);
    Some((days * 86_400 + h * 3600 + mi * 60 + sec) as f64 - offset as f64 + frac)
}

// ------------------------------------------------------------------ text ----

/// First `max` chars of `s` (Python str slicing is char-based).
pub fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// focus._short: strip, cap at 400 chars, append "…" when truncated.
pub fn short(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}\u{2026}", s.chars().take(n).collect::<String>())
    }
}

// ------------------------------------------------------------------ uri ----

pub fn percent_decode(raw: &str, plus_as_space: bool) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = |b: u8| -> Option<u8> {
                    match b {
                        b'0'..=b'9' => Some(b - b'0'),
                        b'a'..=b'f' => Some(b - b'a' + 10),
                        b'A'..=b'F' => Some(b - b'A' + 10),
                        _ => None,
                    }
                };
                if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    out.push(h * 16 + l);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b'+' if plus_as_space => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse `?a=1&b=x` the way urllib.parse_qsl does (+ → space, %XX decoded).
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k, true), percent_decode(v, true)),
            None => (percent_decode(pair, true), String::new()),
        })
        .collect()
}

// -------------------------------------------------------------- open file ---

/// Map a `search_docs.source_ref` (or a bare path the user pasted) to a local
/// file, if one really exists: `vault:<abs path>` → that path, an absolute
/// path → itself, everything else (`board:`, `mnemopi:facts/…`, project, zen)
/// → None. `is_file()` is the only source of truth: a stale ref yields None.
pub fn ref_to_file(source_ref: &str) -> Option<std::path::PathBuf> {
    let r = source_ref.trim();
    let cand = match r.strip_prefix("vault:") {
        Some(rest) => rest.trim(),
        None if r.starts_with('/') => r,
        None => return None,
    };
    if cand.is_empty() {
        return None;
    }
    let p = std::path::Path::new(cand);
    p.is_file().then(|| p.to_path_buf())
}

/// Просмотр iframe URL rule: only `http://127.0.0.1:<port>/…` or
/// `http://localhost:<port>/…`, port ≥ 1. Err names the violated rule.
pub fn validate_web_url(url: &str) -> Result<(), String> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        "разрешён только http:// (не https и не другая схема)".to_string()
    })?;
    let hostport = rest.split(['/', '?', '#']).next().unwrap_or("");
    let (host, port) = hostport.rsplit_once(':').ok_or_else(|| {
        "нужен явный порт: http://127.0.0.1:<порт>/…".to_string()
    })?;
    if host != "127.0.0.1" && host != "localhost" {
        return Err(format!(
            "разрешены только хосты 127.0.0.1 и localhost, а не '{host}'"
        ));
    }
    match port.parse::<u32>() {
        Ok(p) if (1..=65535).contains(&p) => Ok(()),
        _ => Err(format!("порт '{port}' должен быть числом от 1 до 65535")),
    }
}

// ----------------------------------------------------------- projects create ---

/// Project-name rule for `POST /api/projects/create`: the name must match
/// `[A-Za-z0-9][A-Za-z0-9._-]*` — starts with an ASCII letter/digit, then
/// ASCII letters/digits/`.`/`_`/`-`. This rules out `.`, `..`, path separators
/// and empty names, so the name alone can never travel the filesystem.
pub fn valid_project_name(name: &str) -> bool {
    let mut ch = name.chars();
    match ch.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    ch.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// Lexically normalise a path without touching the filesystem: `.` drops,
/// `..` pops one component. Popping past the root yields `None` (escape).
fn normalise_rel(base: &std::path::Path, rel: &str) -> Option<std::path::PathBuf> {
    let joined = base.join(rel);
    let mut out = base.to_path_buf();
    for c in joined.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    return None; // popped past the root → escape
                }
            }
            std::path::Component::Normal(part) => out.push(part),
            // absolute parents replace the base — handled by the final
            // starts_with check in resolve_parent_in_projects
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                out = std::path::PathBuf::new();
                if let std::path::Component::RootDir = c {
                    out.push("/");
                }
            }
        }
    }
    Some(out)
}

/// `parent` of `POST /api/projects/create`: an optional subdirectory
/// (`"sub/dir"`) resolved INSIDE `~/Projects` after lexical normalisation —
/// any `..` escape (or an absolute path outside it) is an error naming the
/// rule. Ok = the normalised absolute parent directory.
pub fn resolve_parent_in_projects(parent: &str) -> Result<std::path::PathBuf, String> {
    let base = crate::cfg::projects_dir();
    let trimmed = parent.trim();
    if trimmed.is_empty() {
        return Ok(base);
    }
    let norm = normalise_rel(&base, trimmed).ok_or_else(|| {
        format!(
            "parent '{trimmed}' выходит за пределы ~/Projects — вложенные пути должны оставаться внутри"
        )
    })?;
    if norm.starts_with(&base) {
        Ok(norm)
    } else {
        Err(format!(
            "parent '{trimmed}' выходит за пределы ~/Projects — вложенные пути должны оставаться внутри"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_to_file_maps_only_real_files() {
        let dir = std::env::temp_dir().join(format!("atlas-util-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let note = dir.join("note.md");
        std::fs::write(&note, "hi").unwrap();
        let note_s = note.to_string_lossy().into_owned();
        // absolute path that exists, both bare and behind `vault:`
        assert_eq!(ref_to_file(&note_s), Some(note.clone()));
        assert_eq!(ref_to_file(&format!("vault:{note_s}")), Some(note.clone()));
        // ghost paths and non-file refs map to None
        assert_eq!(ref_to_file("/no/such/file.md"), None);
        assert_eq!(ref_to_file(&format!("vault:{}", dir.join("ghost.md").display())), None);
        assert_eq!(ref_to_file("board:42"), None);
        assert_eq!(ref_to_file("mnemopi:facts/abc"), None);
        assert_eq!(ref_to_file("project"), None);
        assert_eq!(ref_to_file(""), None);
        // a directory is not a file
        assert_eq!(ref_to_file(&dir.to_string_lossy()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn web_url_rule_accepts_only_local_http_with_port() {
        for ok in [
            "http://127.0.0.1:3000/",
            "http://127.0.0.1:1/",
            "http://localhost:8080/app?x=1",
            "http://127.0.0.1:65535",
        ] {
            assert!(validate_web_url(ok).is_ok(), "{ok} must pass");
        }
        for bad in [
            "https://127.0.0.1:3000/",
            "http://example.com:3000/",
            "http://127.0.0.1/",
            "http://localhost/",
            "http://127.0.0.1:0/",
            "http://127.0.0.1:70000/",
            "http://127.0.0.1:abc/",
            "ftp://127.0.0.1:3000/",
            "127.0.0.1:3000/",
            "",
        ] {
            let e = validate_web_url(bad).unwrap_err();
            assert!(!e.is_empty(), "{bad} must be rejected with a reason");
        }
        // the error names the rule, not just "bad"
        assert!(validate_web_url("https://x:1/").unwrap_err().contains("http://"));
        assert!(validate_web_url("http://example.com:1/").unwrap_err().contains("127.0.0.1"));
        assert!(validate_web_url("http://127.0.0.1/").unwrap_err().contains("порт"));
    }

    #[test]
    fn project_name_rule_is_ascii_identifier() {
        for ok in ["a", "A", "zz-atlas-test", "My.Project_2", "0start", "cbqn"] {
            assert!(valid_project_name(ok), "{ok} must pass");
        }
        for bad in [
            "", ".hidden", "..", "-start", "_start", "a/b", "a b", "проект", "a\tb", "a..b?",
        ] {
            assert!(!valid_project_name(bad), "{bad} must be rejected");
        }
    }

    #[test]
    fn parent_must_resolve_inside_projects() {
        let base = crate::cfg::projects_dir();
        // empty/absent parent → ~/Projects itself
        assert_eq!(resolve_parent_in_projects("").unwrap(), base);
        assert_eq!(resolve_parent_in_projects("  ").unwrap(), base);
        // simple and nested subdirs, dot noise, inner .. that stays inside
        assert_eq!(
            resolve_parent_in_projects("sub").unwrap(),
            base.join("sub")
        );
        assert_eq!(
            resolve_parent_in_projects("sub/dir").unwrap(),
            base.join("sub").join("dir")
        );
        assert_eq!(
            resolve_parent_in_projects("./sub/./x/../dir").unwrap(),
            base.join("sub").join("dir")
        );
        // absolute path inside ~/Projects is fine
        assert_eq!(
            resolve_parent_in_projects(&base.join("sub").join("dir").to_string_lossy())
                .unwrap(),
            base.join("sub").join("dir")
        );
        // .. escapes → error naming the rule
        for esc in ["..", "../elsewhere", "sub/../../escape", "a/../.."] {
            let e = resolve_parent_in_projects(esc).unwrap_err();
            assert!(e.contains("~/Projects"), "{esc} → {e}");
        }
        // absolute path outside ~/Projects → error
        let e = resolve_parent_in_projects("/tmp/escape").unwrap_err();
        assert!(e.contains("~/Projects"), "/tmp/escape → {e}");
    }
}

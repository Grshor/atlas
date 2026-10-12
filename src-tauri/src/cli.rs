//! CLI verbs (`atlas notify`, `atlas board …`, `atlas help`): thin HTTP
//! client against the local server. No DB, no GUI, no tokio — hand-rolled
//! HTTP/1.0 over std::net::TcpStream; the only parsing is the status line
//! and the JSON body.
//!
//! EXIT CONTRACT for all atlas CLI verbs: `0` ok · `1` runtime error (the
//! server answered, but with an error / unexpected body) · `2` usage error
//! (the parser rejects before any I/O) · `3` unreachable (no connection /
//! unreadable response). Failures print one named line on stderr — never a
//! silent success, never queueing.
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub const DEFAULT_URL: &str = "http://127.0.0.1:8787";

pub struct NotifyArgs {
    pub text: String,
    pub project: Option<String>,
    pub level: String,
    pub url: String,
}

/// Parse `notify` verb arguments. Err = usage error (exit 2).
pub fn parse_notify_args(argv: Vec<String>) -> Result<NotifyArgs, String> {
    let mut args = NotifyArgs {
        text: String::new(),
        project: None,
        level: "info".into(),
        url: DEFAULT_URL.into(),
    };
    let mut it = argv.into_iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--project" => match it.next() {
                Some(v) => args.project = Some(v),
                None => return Err("--project needs a value".into()),
            },
            "--level" => match it.next() {
                Some(v) => args.level = v,
                None => return Err("--level needs a value".into()),
            },
            "--url" => match it.next() {
                Some(v) => args.url = v,
                None => return Err("--url needs a value".into()),
            },
            // the verb must stand alone; flags-only server runs are another path
            "--serve-only" | "--port" | "--db" | "--config" => {
                return Err(format!("'{a}' нельзя совмещать с командой notify"));
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag '{other}' for notify"));
            }
            other => {
                if !args.text.is_empty() {
                    return Err("notify принимает один текст (позиционный аргумент)".into());
                }
                args.text = other.to_string();
            }
        }
    }
    if args.text.is_empty() {
        return Err("notify: текст уведомления обязателен".into());
    }
    if !matches!(args.level.as_str(), "info" | "warn" | "err") {
        return Err(format!(
            "notify: --level должен быть info|warn|err, получено '{}'",
            args.level
        ));
    }
    Ok(args)
}

/// Split `http://127.0.0.1:8787` into (host, port).
fn split_url(url: &str) -> Result<(String, u16), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("--url должен быть http://host:port, получено '{url}'"))?;
    let rest = rest.trim_end_matches('/');
    match rest.rsplit_once(':') {
        Some((h, p)) => {
            let port = p.parse().map_err(|_| format!("bad port in '{url}'"))?;
            Ok((h.to_string(), port))
        }
        None => Ok((rest.to_string(), 80)),
    }
}

/// Minimal HTTP/1.0 POST (connection-close framing: no chunked decoding).
/// Returns (status, body).
pub fn http_post(url: &str, path: &str, body: &str) -> Result<(u16, String), String> {
    let (host, port) = split_url(url)?;
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr)
        .map_err(|e| format!("нет соединения с {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("socket: {e}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("socket: {e}"))?;
    let req = format!(
        "POST {path} HTTP/1.0\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write {addr}: {e}"))?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("read {addr}: {e}"))?;
    let text = String::from_utf8_lossy(&raw);
    let (head, resp_body) = match text.split_once("\r\n\r\n") {
        Some((h, b)) => (h, b),
        None => (text.as_ref(), ""),
    };
    let status_line = head.lines().next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("не удалось разобрать ответ сервера: '{status_line}'"))?;
    Ok((status, resp_body.to_string()))
}

/// Minimal HTTP/1.0 GET (connection-close framing). Returns (status, body).
pub fn http_get(url: &str, path: &str) -> Result<(u16, String), String> {
    let (host, port) = split_url(url)?;
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr)
        .map_err(|e| format!("нет соединения с {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("socket: {e}"))?;
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write {addr}: {e}"))?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("read {addr}: {e}"))?;
    let text = String::from_utf8_lossy(&raw);
    let (head, resp_body) = match text.split_once("\r\n\r\n") {
        Some((h, b)) => (h, b),
        None => (text.as_ref(), ""),
    };
    let status_line = head.lines().next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("не удалось разобрать ответ сервера: '{status_line}'"))?;
    Ok((status, resp_body.to_string()))
}

/// The exit outcome of one board verb; the code column is the CLI contract.
#[derive(Debug)]
pub enum CliExit {
    /// `0` — one line on stdout.
    Ok(String),
    /// `1` — the server answered with an error / unexpected body.
    Runtime(String),
    /// `3` — no connection / unreadable response.
    Unreachable(String),
}

impl CliExit {
    pub fn code(&self) -> i32 {
        match self {
            CliExit::Ok(_) => 0,
            CliExit::Runtime(_) => 1,
            CliExit::Unreachable(_) => 3,
        }
    }

    pub fn emit(&self) {
        match self {
            CliExit::Ok(line) => println!("{line}"),
            CliExit::Runtime(line) => eprintln!("{line}"),
            CliExit::Unreachable(line) => eprintln!("{line}"),
        }
    }
}

#[derive(Debug)]
pub struct BoardAddArgs {
    pub title: String,
    pub project: Option<String>,
    pub quadrant: String,
    pub description: Option<String>,
    pub dedupe_key: Option<String>,
    pub url: String,
}

#[derive(Debug)]
pub struct BoardListArgs {
    pub quadrant: Option<String>,
    pub json: bool,
    pub url: String,
}

#[derive(Debug)]
pub struct BoardStatusArgs {
    pub json: bool,
    pub url: String,
}

#[derive(Debug)]
pub enum BoardCmd {
    Add(BoardAddArgs),
    List(BoardListArgs),
    Status(BoardStatusArgs),
}

/// Parse `board` verb arguments. Err = usage error (exit 2) — including an
/// invalid quadrant, which is named before any I/O.
pub fn parse_board_args(argv: Vec<String>) -> Result<BoardCmd, String> {
    let mut it = argv.into_iter().peekable();
    let verb = it
        .next()
        .ok_or_else(|| "board: нужна команда add|list|status".to_string())?;
    let mut named: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut switches: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut positional: Vec<String> = Vec::new();
    let mut pending: Option<String> = None; // flag waiting for its value
    while let Some(a) = it.next() {
        if let Some(f) = pending.take() {
            named.insert(f, a);
            continue;
        }
        match a.as_str() {
            "--url" => pending = Some("url".into()),
            "--project" => pending = Some("project".into()),
            "--quadrant" => pending = Some("quadrant".into()),
            "--description" => pending = Some("description".into()),
            "--dedupe-key" => pending = Some("dedupe-key".into()),
            "--json" => {
                switches.insert("json".into());
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag '{other}' for board {verb}"));
            }
            other => positional.push(other.to_string()),
        }
    }
    if let Some(f) = pending {
        return Err(format!("--{f} needs a value"));
    }
    let get = |name: &str| named.get(name).cloned();
    let quadrant_ok =
        |q: &str| matches!(q, "q1" | "q2" | "q3" | "q4");
    match verb.as_str() {
        "add" => {
            let title = positional
                .first()
                .cloned()
                .ok_or_else(|| "board add: заголовок дела обязателен".to_string())?;
            let quadrant = get("quadrant").unwrap_or_else(|| "q1".into());
            if !quadrant_ok(&quadrant) {
                return Err(format!(
                    "board add: --quadrant должен быть q1|q2|q3|q4, получено '{quadrant}'"
                ));
            }
            Ok(BoardCmd::Add(BoardAddArgs {
                title,
                project: get("project"),
                quadrant,
                description: get("description"),
                dedupe_key: get("dedupe-key"),
                url: get("url").unwrap_or_else(|| DEFAULT_URL.into()),
            }))
        }
        "list" => {
            let quadrant = get("quadrant");
            if let Some(q) = &quadrant {
                if !quadrant_ok(q) {
                    return Err(format!(
                        "board list: --quadrant должен быть q1|q2|q3|q4, получено '{q}'"
                    ));
                }
            }
            Ok(BoardCmd::List(BoardListArgs {
                quadrant,
                json: switches.contains("json"),
                url: get("url").unwrap_or_else(|| DEFAULT_URL.into()),
            }))
        }
        "status" => Ok(BoardCmd::Status(BoardStatusArgs {
            json: switches.contains("json"),
            url: get("url").unwrap_or_else(|| DEFAULT_URL.into()),
        })),
        other => Err(format!(
            "board: неизвестная команда '{other}' — add|list|status"
        )),
    }
}

/// All items of the board (or one quadrant), parsed.
fn fetch_items(url: &str) -> Result<Vec<serde_json::Value>, CliExit> {
    let (status, resp) = http_get(url, "/api/board").map_err(|e| CliExit::Unreachable(e))?;
    if status != 200 {
        return Err(CliExit::Runtime(format!(
            "atlas board: сервер ответил {status}: {}",
            resp.trim()
        )));
    }
    let v: serde_json::Value = serde_json::from_str(resp.trim())
        .map_err(|e| CliExit::Runtime(format!("atlas board: не JSON в ответе: {e}")))?;
    let mut items = Vec::new();
    for q in ["q1", "q2", "q3", "q4"] {
        if let Some(arr) = v.get(q).and_then(|a| a.as_array()) {
            items.extend(arr.iter().cloned());
        }
    }
    Ok(items)
}

/// `atlas board add "<title>" …` → POST /api/board/item. With `--dedupe-key`
/// an existing item carrying the same key wins: `id=N deduped`, exit 0 —
/// the retry contract for delegated intake.
pub fn board_add_main(args: BoardAddArgs) -> CliExit {
    if let Some(key) = &args.dedupe_key {
        match fetch_items(&args.url) {
            Ok(items) => {
                if let Some(existing) = items
                    .iter()
                    .find(|i| i.get("dedupe_key").and_then(|k| k.as_str()) == Some(key.as_str()))
                {
                    if let Some(id) = existing.get("id").and_then(|i| i.as_i64()) {
                        return CliExit::Ok(format!("id={id} deduped"));
                    }
                }
            }
            Err(CliExit::Runtime(e)) => return CliExit::Runtime(e),
            Err(e) => return e,
        }
    }
    let mut body = serde_json::Map::new();
    body.insert("title".into(), serde_json::Value::String(args.title.clone()));
    body.insert(
        "quadrant".into(),
        serde_json::Value::String(args.quadrant.clone()),
    );
    if let Some(p) = &args.project {
        body.insert("project".into(), serde_json::Value::String(p.clone()));
    }
    if let Some(d) = &args.description {
        body.insert("description".into(), serde_json::Value::String(d.clone()));
    }
    if let Some(k) = &args.dedupe_key {
        body.insert("dedupe_key".into(), serde_json::Value::String(k.clone()));
    }
    let body = serde_json::Value::Object(body).to_string();
    match http_post(&args.url, "/api/board/item", &body) {
        Err(e) => CliExit::Unreachable(format!("atlas board add: {e}")),
        Ok((status, resp)) if status != 200 => CliExit::Runtime(format!(
            "atlas board add: сервер ответил {status}: {}",
            resp.trim()
        )),
        Ok((_, resp)) => {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(resp.trim()) else {
                return CliExit::Runtime(format!("atlas board add: не JSON в ответе: {}", resp.trim()));
            };
            match v.get("id").and_then(|i| i.as_i64()) {
                Some(id) => CliExit::Ok(format!("id={id}")),
                None => CliExit::Runtime(format!("atlas board add: в ответе нет id: {}", resp.trim())),
            }
        }
    }
}

/// `atlas board list [--quadrant qN] [--json]` — the board as a table or a
/// JSON array.
pub fn board_list_main(args: BoardListArgs) -> CliExit {
    let items = match fetch_items(&args.url) {
        Ok(items) => items,
        Err(e) => return e,
    };
    let items: Vec<&serde_json::Value> = items
        .iter()
        .filter(|i| match &args.quadrant {
            Some(q) => i.get("quadrant").and_then(|x| x.as_str()) == Some(q.as_str()),
            None => true,
        })
        .collect();
    if args.json {
        let arr: Vec<serde_json::Value> = items.into_iter().cloned().collect();
        return CliExit::Ok(serde_json::to_string(&arr).unwrap_or_else(|_| "[]".into()));
    }
    let mut out = String::from(
        "    id  quadrant  title                                    project            done  steps files",
    );
    for i in items {
        let id = i.get("id").and_then(|x| x.as_i64()).unwrap_or(0);
        let q = i.get("quadrant").and_then(|x| x.as_str()).unwrap_or("?");
        let title = i.get("title").and_then(|x| x.as_str()).unwrap_or("?");
        let project = i.get("project").and_then(|x| x.as_str()).unwrap_or("–");
        let done = if i.get("done").and_then(|x| x.as_bool()).unwrap_or(false) {
            "yes"
        } else {
            "no"
        };
        let steps = i.get("steps").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0);
        let files = i.get("files").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0);
        out.push_str(&format!(
            "\n{:>6}  {:<8}  {:<40} {:<18} {:<4}  {:>5} {:>5}",
            id, q, title, project, done, steps, files
        ));
    }
    CliExit::Ok(out)
}

/// `atlas board status [--json]` — counts per quadrant + done + open steps.
pub fn board_status_main(args: BoardStatusArgs) -> CliExit {
    let items = match fetch_items(&args.url) {
        Ok(items) => items,
        Err(e) => return e,
    };
    let mut counts = std::collections::BTreeMap::new();
    let mut done = 0i64;
    let mut open_steps = 0i64;
    for i in &items {
        let q = i.get("quadrant").and_then(|x| x.as_str()).unwrap_or("?").to_string();
        *counts.entry(q).or_insert(0i64) += 1;
        if i.get("done").and_then(|x| x.as_bool()).unwrap_or(false) {
            done += 1;
        }
        if let Some(steps) = i.get("steps").and_then(|x| x.as_array()) {
            open_steps += steps
                .iter()
                .filter(|s| !s.get("done").and_then(|d| d.as_bool()).unwrap_or(false))
                .count() as i64;
        }
    }
    if args.json {
        let mut obj = serde_json::Map::new();
        for q in ["q1", "q2", "q3", "q4"] {
            obj.insert(q.into(), serde_json::Value::from(counts.get(q).copied().unwrap_or(0)));
        }
        obj.insert("done".into(), serde_json::Value::from(done));
        obj.insert("open_steps".into(), serde_json::Value::from(open_steps));
        return CliExit::Ok(serde_json::Value::Object(obj).to_string());
    }
    CliExit::Ok(format!(
        "q1={} q2={} q3={} q4={} done={done} open_steps={open_steps}",
        counts.get("q1").copied().unwrap_or(0),
        counts.get("q2").copied().unwrap_or(0),
        counts.get("q3").copied().unwrap_or(0),
        counts.get("q4").copied().unwrap_or(0),
    ))
}

/// `atlas notify …` entry point: POST /api/notify, print `id=N unread=M`,
/// exit 0; any failure → stderr + exit 1 (no silent success, no queueing).
pub fn notify_main(args: NotifyArgs) -> i32 {
    let mut body = serde_json::Map::new();
    body.insert("text".into(), serde_json::Value::String(args.text.clone()));
    if let Some(p) = &args.project {
        body.insert("project".into(), serde_json::Value::String(p.clone()));
    }
    if args.level != "info" {
        body.insert("level".into(), serde_json::Value::String(args.level.clone()));
    }
    let body = serde_json::Value::Object(body).to_string();
    match http_post(&args.url, "/api/notify", &body) {
        Err(e) => {
            eprintln!("atlas notify: {e}");
            1
        }
        Ok((status, resp)) if status != 200 => {
            eprintln!("atlas notify: сервер ответил {status}: {}", resp.trim());
            1
        }
        Ok((_, resp)) => {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(resp.trim()) else {
                eprintln!("atlas notify: не JSON в ответе: {}", resp.trim());
                return 1;
            };
            match (v.get("id").and_then(|i| i.as_i64()), v.get("unread").and_then(|u| u.as_i64())) {
                (Some(id), Some(unread)) => {
                    println!("id={id} unread={unread}");
                    0
                }
                _ => {
                    eprintln!("atlas notify: в ответе нет id/unread: {}", resp.trim());
                    1
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_args_parse() {
        let a = parse_notify_args(vec!["текст упал".into()]).unwrap();
        assert_eq!(a.text, "текст упал");
        assert_eq!(a.level, "info");
        assert_eq!(a.url, DEFAULT_URL);
        let b = parse_notify_args(vec![
            "сборка упала".into(),
            "--project".into(),
            "atlas".into(),
            "--level".into(),
            "warn".into(),
            "--url".into(),
            "http://127.0.0.1:9999".into(),
        ])
        .unwrap();
        assert_eq!(b.project.as_deref(), Some("atlas"));
        assert_eq!(b.level, "warn");
        assert_eq!(b.url, "http://127.0.0.1:9999");
    }

    #[test]
    fn notify_args_reject() {
        assert!(parse_notify_args(vec![]).is_err(), "text required");
        assert!(parse_notify_args(vec!["x".into(), "--level".into(), "loud".into()]).is_err());
        assert!(
            parse_notify_args(vec!["x".into(), "--serve-only".into()]).is_err(),
            "verb + server flags is a usage error"
        );
        assert!(parse_notify_args(vec!["--project".into()]).is_err());
    }

    #[test]
    fn board_args_parse() {
        let a = parse_board_args(vec!["add".into(), "починить".into()]).unwrap();
        let BoardCmd::Add(a) = a else { panic!("add") };
        assert_eq!(a.title, "починить");
        assert_eq!(a.quadrant, "q1", "quadrant defaults to q1");
        assert_eq!(a.dedupe_key, None);

        let b = parse_board_args(vec![
            "add".into(), "дело".into(), "--project".into(), "atlas".into(),
            "--quadrant".into(), "q2".into(), "--description".into(), "бриф".into(),
            "--dedupe-key".into(), "slug".into(), "--url".into(), "http://127.0.0.1:9".into(),
        ]).unwrap();
        let BoardCmd::Add(b) = b else { panic!("add") };
        assert_eq!(b.project.as_deref(), Some("atlas"));
        assert_eq!(b.quadrant, "q2");
        assert_eq!(b.description.as_deref(), Some("бриф"));
        assert_eq!(b.dedupe_key.as_deref(), Some("slug"));
        assert_eq!(b.url, "http://127.0.0.1:9");

        let l = parse_board_args(vec!["list".into(), "--json".into(), "--quadrant".into(), "q3".into()]).unwrap();
        let BoardCmd::List(l) = l else { panic!("list") };
        assert!(l.json);
        assert_eq!(l.quadrant.as_deref(), Some("q3"));

        let st = parse_board_args(vec!["status".into()]).unwrap();
        assert!(matches!(st, BoardCmd::Status(_)));
    }

    #[test]
    fn board_args_reject() {
        // an invalid quadrant is a usage error named before any I/O
        for argv in [
            vec!["add".to_string(), "т".to_string(), "--quadrant".to_string(), "q9".to_string()],
            vec!["list".to_string(), "--quadrant".to_string(), "top".to_string()],
            vec!["add".to_string()],
            vec!["add".to_string(), "т".to_string(), "--dedupe-key".to_string()],
            vec!["frobnicate".to_string()],
            vec![],
            vec!["add".to_string(), "т".to_string(), "--nope".to_string()],
        ] {
            let err = parse_board_args(argv.clone()).unwrap_err();
            assert!(!err.is_empty(), "{argv:?}");
        }
    }

    #[test]
    fn cli_exit_codes_follow_the_contract() {
        assert_eq!(CliExit::Ok("x".into()).code(), 0);
        assert_eq!(CliExit::Runtime("x".into()).code(), 1);
        assert_eq!(CliExit::Unreachable("x".into()).code(), 3);
    }

    #[test]
    fn url_split() {
        assert_eq!(
            split_url("http://127.0.0.1:8787").unwrap(),
            ("127.0.0.1".into(), 8787)
        );
        assert_eq!(split_url("http://localhost").unwrap(), ("localhost".into(), 80));
        assert!(split_url("https://x").is_err());
    }
}

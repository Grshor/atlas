//! UI settings: typed key-value store over the `settings` table.
//! Values are stored as JSON scalars; every read is validated against the
//! spec, and a corrupt row falls back to the default instead of failing.
//!
//! Two scopes share the table: the plain key is the GLOBAL value, a key
//! prefixed `win:<label>:` is that window's override. The effective value for
//! a window is global merged with its overrides; clearing an override removes
//! the row, so the global value shows through again.
use rusqlite::Connection;
use serde_json::{json, Map, Value};

/// Panels are also the window labels (`win=`) and minimised-tab labels.
/// «Настройки» is NOT one: the frontend opens settings inside the window that
/// was clicked, so there is no settings window to label.
pub const PANELS: [&str; 9] = [
    "projects",
    "board",
    "deps",
    "search",
    "sources",
    "herdr",
    "view",
    "terminal",
    "orchestrator",
];
/// The host window hosts the tab bar; it cannot be minimised into itself.
pub const HOST: &str = "projects";

/// Storage key of one window override: `win:<label>:<key>`.
fn scoped_key(win: &str, key: &str) -> String {
    format!("win:{win}:{key}")
}

/// Split a stored scoped key back into (label, key); None for global rows and
/// for malformed / unknown-label scopes (a stale label is ignored, never
/// surfaced as a window).
fn split_scoped(stored: &str) -> Option<(String, String)> {
    let rest = stored.strip_prefix("win:")?;
    let (label, key) = rest.split_once(':')?;
    if !PANELS.contains(&label) || !SPECS.iter().any(|s| s.key == key) {
        return None;
    }
    Some((label.to_string(), key.to_string()))
}

pub enum Kind {
    /// Allowed strings (enum-like settings).
    Str(&'static [&'static str]),
    /// Inclusive integer range.
    Int(i64, i64),
    Bool,
}

/// Plain-data default (const-friendly); converted to JSON on read.
pub enum DefaultVal {
    Str(&'static str),
    Int(i64),
    Bool(bool),
}

pub struct Spec {
    pub key: &'static str,
    pub kind: Kind,
    pub default: DefaultVal,
}

pub static SPECS: [Spec; 9] = [
    Spec {
        key: "ui.statusbar",
        kind: Kind::Str(&["always", "hover", "hidden"]),
        default: DefaultVal::Str("always"),
    },
    Spec {
        key: "ui.tabbar",
        kind: Kind::Str(&["on", "off"]),
        default: DefaultVal::Str("on"),
    },
    Spec {
        key: "ui.autorefresh_sec",
        kind: Kind::Int(5, 600),
        default: DefaultVal::Int(30),
    },
    Spec {
        key: "ui.search_k",
        kind: Kind::Int(5, 100),
        default: DefaultVal::Int(20),
    },
    Spec {
        key: "ui.show_probe",
        kind: Kind::Bool,
        default: DefaultVal::Bool(true),
    },
    Spec {
        key: "ui.time_format",
        kind: Kind::Str(&["relative", "absolute"]),
        default: DefaultVal::Str("relative"),
    },
    Spec {
        key: "ui.show_hybrid_details",
        kind: Kind::Bool,
        default: DefaultVal::Bool(true),
    },
    Spec {
        key: "patrol.enabled",
        kind: Kind::Bool,
        default: DefaultVal::Bool(true),
    },
    Spec {
        key: "patrol.interval_min",
        kind: Kind::Int(15, 10080),
        default: DefaultVal::Int(360),
    },
];

fn default_value(s: &Spec) -> Value {
    match s.default {
        DefaultVal::Str(v) => json!(v),
        DefaultVal::Int(v) => json!(v),
        DefaultVal::Bool(v) => json!(v),
    }
}

fn spec_of(key: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.key == key)
}

/// Validate one incoming value; Ok = the normalised value to store.
/// Err names the key so the 422 can point at the offender.
pub fn validate(key: &str, v: &Value) -> Result<Value, String> {
    let spec = spec_of(key).ok_or_else(|| format!("неизвестный ключ настройки '{key}'"))?;
    match &spec.kind {
        Kind::Str(allowed) => {
            let s = v
                .as_str()
                .ok_or_else(|| format!("'{key}': ожидается строка {allowed:?}"))?;
            if !allowed.iter().any(|a| *a == s) {
                return Err(format!(
                    "'{key}': значение '{s}' недопустимо, ожидается {allowed:?}"
                ));
            }
            Ok(json!(s))
        }
        Kind::Int(lo, hi) => {
            let (lo, hi) = (*lo, *hi);
            let n = v
                .as_i64()
                .ok_or_else(|| format!("'{key}': ожидается целое число {lo}..{hi}"))?;
            if !(lo..=hi).contains(&n) {
                return Err(format!("'{key}': {n} вне диапазона {lo}..{hi}"));
            }
            Ok(json!(n))
        }
        Kind::Bool => v
            .as_bool()
            .map(|b| json!(b))
            .ok_or_else(|| format!("'{key}': ожидается true/false")),
    }
}

/// Global values: one entry per spec, validated, corrupt rows falling back to
/// the default.
fn global_map(conn: &Connection) -> Map<String, Value> {
    let mut out = Map::new();
    for s in &SPECS {
        let stored: Option<String> = conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [s.key], |r| {
                r.get(0)
            })
            .ok();
        let v = stored
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|v| validate(s.key, &v).ok())
            .unwrap_or_else(|| default_value(s));
        out.insert(s.key.to_string(), v);
    }
    out
}

/// Overrides of every window that has at least one; only labels with a real
/// override appear (corrupt values are silently dropped, never invented).
fn windows_map(conn: &Connection) -> Map<String, Value> {
    let mut out: Map<String, Value> = Map::new();
    let mut stmt = match conn.prepare("SELECT key, value FROM settings WHERE key LIKE 'win:%'")
    {
        Ok(s) => s,
        Err(_) => return out,
    };
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map(|it| it.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();
    for (stored, raw) in rows {
        let Some((win, key)) = split_scoped(&stored) else {
            continue;
        };
        let Some(v) = serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| validate(&key, &v).ok())
        else {
            continue;
        };
        match out.get_mut(&win) {
            Some(Value::Object(m)) => {
                m.insert(key, v);
            }
            _ => {
                let mut m = Map::new();
                m.insert(key, v);
                out.insert(win, Value::Object(m));
            }
        }
    }
    out
}

/// `{"settings","global","windows","defaults"}`. With `win` the `settings`
/// object is global merged with that window's overrides; without it `settings`
/// equals `global`. `windows` carries only labels that really have overrides.
/// A corrupt row (unparsable or invalid JSON) silently falls back to the
/// default.
pub fn effective(conn: &Connection, win: Option<&str>) -> Value {
    let global = global_map(conn);
    let windows = windows_map(conn);
    let mut merged = global.clone();
    if let Some(win) = win {
        if let Some(Value::Object(over)) = windows.get(win) {
            for (k, v) in over {
                merged.insert(k.clone(), v.clone());
            }
        }
    }
    let mut defaults = Map::new();
    for s in &SPECS {
        defaults.insert(s.key.to_string(), default_value(s));
    }
    json!({
        "settings": Value::Object(merged),
        "global": Value::Object(global),
        "windows": Value::Object(windows),
        "defaults": Value::Object(defaults),
    })
}

pub fn store(conn: &Connection, key: &str, value: &Value) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO settings(key, value) VALUES(?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value.to_string()],
    )
}

/// Write one window override (`win:<label>:<key>`).
pub fn store_scoped(
    conn: &Connection,
    win: &str,
    key: &str,
    value: &Value,
) -> rusqlite::Result<usize> {
    store(conn, &scoped_key(win, key), value)
}

/// Drop one window override → the global value shows through again.
pub fn clear_scoped(conn: &Connection, win: &str, key: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM settings WHERE key = ?1",
        [scoped_key(win, key)],
    )
}

/// Window state: panel labels minimised into the host (`ui.tabs`). Unknown or
/// non-string labels from a corrupt row are dropped.
pub fn tabs(conn: &Connection) -> Vec<String> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'ui.tabs'",
            [],
            |r| r.get(0),
        )
        .ok();
    raw.and_then(|r| serde_json::from_str::<Value>(&r).ok())
        .and_then(|v| v.as_array().cloned())
        .map(|a| {
            a.into_iter()
                .filter_map(|x| x.as_str().map(String::from))
                .filter(|s| PANELS.contains(&s.as_str()))
                .collect()
        })
        .unwrap_or_default()
}

pub fn set_tabs(conn: &Connection, tabs: &[String]) -> rusqlite::Result<usize> {
    store(conn, "ui.tabs", &json!(tabs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().expect("mem db");
        db::apply_schema(&conn).expect("schema");
        conn
    }

    #[test]
    fn validate_enum_keys() {
        for ok in ["always", "hover", "hidden"] {
            assert_eq!(validate("ui.statusbar", &json!(ok)).unwrap(), json!(ok));
        }
        let e = validate("ui.statusbar", &json!("sometimes")).unwrap_err();
        assert!(e.contains("ui.statusbar"), "error must name the key: {e}");
        assert!(validate("ui.statusbar", &json!(3)).unwrap_err().contains("ui.statusbar"));
        assert_eq!(validate("ui.tabbar", &json!("off")).unwrap(), json!("off"));
        assert!(validate("ui.tabbar", &json!("maybe")).is_err());
        assert_eq!(validate("ui.time_format", &json!("absolute")).unwrap(), json!("absolute"));
    }

    #[test]
    fn validate_int_ranges_and_bools() {
        assert!(validate("ui.autorefresh_sec", &json!(4)).is_err());
        assert_eq!(validate("ui.autorefresh_sec", &json!(5)).unwrap(), json!(5));
        assert_eq!(validate("ui.autorefresh_sec", &json!(600)).unwrap(), json!(600));
        assert!(validate("ui.autorefresh_sec", &json!(601)).is_err());
        assert!(validate("ui.autorefresh_sec", &json!("30")).is_err(), "string numbers rejected");
        assert_eq!(validate("ui.search_k", &json!(20)).unwrap(), json!(20));
        assert!(validate("ui.search_k", &json!(101)).is_err());
        assert_eq!(validate("ui.show_probe", &json!(false)).unwrap(), json!(false));
        assert!(validate("ui.show_probe", &json!("true")).is_err());
    }

    #[test]
    fn validate_patrol_keys() {
        assert_eq!(validate("patrol.enabled", &json!(true)).unwrap(), json!(true));
        assert!(validate("patrol.enabled", &json!("yes")).is_err());
        assert_eq!(validate("patrol.interval_min", &json!(15)).unwrap(), json!(15));
        assert_eq!(validate("patrol.interval_min", &json!(10080)).unwrap(), json!(10080));
        assert!(validate("patrol.interval_min", &json!(14)).is_err());
        assert!(validate("patrol.interval_min", &json!(10081)).is_err());
        // defaults surface through effective()
        let conn = mem();
        let e = effective(&conn, None);
        assert_eq!(e["settings"]["patrol.enabled"], true);
        assert_eq!(e["settings"]["patrol.interval_min"], 360);
        assert_eq!(e["defaults"]["patrol.interval_min"], 360);
    }

    #[test]
    fn validate_unknown_key_names_it() {
        let e = validate("ui.colour", &json!(1)).unwrap_err();
        assert!(e.contains("ui.colour"));
        // window state is not a setting
        assert!(validate("ui.tabs", &json!([])).is_err());
    }

    #[test]
    fn effective_defaults_and_corrupt_rows_fall_back() {
        let conn = mem();
        let out = effective(&conn, None);
        assert_eq!(out["settings"]["ui.statusbar"], "always");
        assert_eq!(out["defaults"]["ui.autorefresh_sec"], 30);
        assert_eq!(out["settings"]["ui.search_k"], 20);
        assert_eq!(out["settings"]["ui.show_hybrid_details"], true);
        // valid row wins over the default
        store(&conn, "ui.search_k", &json!(42)).unwrap();
        assert_eq!(effective(&conn, None)["settings"]["ui.search_k"], 42);
        // corrupt rows (unparsable, wrong type, out of range) fall back
        store(&conn, "ui.statusbar", &Value::String("not json".into())).unwrap();
        store(&conn, "ui.search_k", &json!("huge")).unwrap();
        store(&conn, "ui.time_format", &json!("Гибкий")).unwrap();
        let out2 = effective(&conn, None);
        assert_eq!(out2["settings"]["ui.statusbar"], "always");
        assert_eq!(out2["settings"]["ui.search_k"], 20);
        assert_eq!(out2["settings"]["ui.time_format"], "relative");
        // the valid key survives
        assert_eq!(out2["settings"]["ui.search_k"], 20);
    }

    #[test]
    fn window_overrides_merge_over_global_and_clear() {
        let conn = mem();
        // no overrides yet: effective == global, windows empty
        let e = effective(&conn, Some("board"));
        assert_eq!(e["settings"], e["global"]);
        assert_eq!(e["windows"], json!({}));

        store(&conn, "ui.statusbar", &json!("hover")).unwrap();
        store_scoped(&conn, "board", "ui.statusbar", &json!("hidden")).unwrap();
        store_scoped(&conn, "board", "ui.search_k", &json!(7)).unwrap();
        store_scoped(&conn, "deps", "ui.statusbar", &json!("always")).unwrap();

        let board = effective(&conn, Some("board"));
        assert_eq!(board["settings"]["ui.statusbar"], "hidden");
        assert_eq!(board["settings"]["ui.search_k"], 7);
        assert_eq!(board["global"]["ui.statusbar"], "hover");
        assert_eq!(board["global"]["ui.search_k"], 20);
        // only labels with real overrides are listed, with only those keys
        assert_eq!(board["windows"]["board"]["ui.statusbar"], "hidden");
        assert_eq!(board["windows"]["deps"]["ui.statusbar"], "always");
        assert_eq!(
            board["windows"]["board"].as_object().unwrap().len(),
            2,
            "only overridden keys per window"
        );

        // another window sees the global values only
        let search = effective(&conn, Some("search"));
        assert_eq!(search["settings"]["ui.statusbar"], "hover");
        assert_eq!(search["settings"]["ui.search_k"], 20);

        // no ?win → effective == global, windows still reported
        let none = effective(&conn, None);
        assert_eq!(none["settings"], none["global"]);
        assert_ne!(none["windows"], json!({}));

        // clearing gives the global value back and drops the window
        clear_scoped(&conn, "board", "ui.statusbar").unwrap();
        clear_scoped(&conn, "board", "ui.search_k").unwrap();
        let after = effective(&conn, Some("board"));
        assert_eq!(after["settings"]["ui.statusbar"], "hover");
        assert_eq!(after["settings"]["ui.search_k"], 20);
        assert!(
            after["windows"].get("board").is_none(),
            "no overrides left → no window entry"
        );
        assert_eq!(after["windows"]["deps"]["ui.statusbar"], "always");
    }

    #[test]
    fn scoped_rows_reject_unknown_labels_and_keys_on_read() {
        let conn = mem();
        // stale / malformed scopes are ignored, never surfaced as windows
        store(&conn, "win:settings:ui.statusbar", &json!("hidden")).unwrap();
        store(&conn, "win:nope:ui.statusbar", &json!("hidden")).unwrap();
        store(&conn, "win:board:ui.nokey", &json!("hidden")).unwrap();
        store(&conn, "win:board:ui.statusbar", &json!("bogus")).unwrap();
        let e = effective(&conn, Some("board"));
        assert_eq!(e["windows"], json!({}), "nothing valid to report");
        assert_eq!(e["settings"]["ui.statusbar"], "always");
        // ui.tabs is window state, not a setting → never a valid scope
        assert!(split_scoped("win:board:ui.tabs").is_none());
        assert!(split_scoped("ui.tabs").is_none());
        assert_eq!(
            split_scoped("win:board:ui.statusbar"),
            Some(("board".to_string(), "ui.statusbar".to_string()))
        );
    }

    #[test]
    fn tabs_roundtrip_and_garbage_filtering() {
        let conn = mem();
        assert!(tabs(&conn).is_empty());
        set_tabs(&conn, &["board".into(), "deps".into()]).unwrap();
        assert_eq!(tabs(&conn), vec!["board".to_string(), "deps".to_string()]);
        // garbage labels are dropped, not propagated
        set_tabs(&conn, &["board".into(), "not-a-panel".into(), "42".into()]).unwrap();
        assert_eq!(tabs(&conn), vec!["board".to_string()]);
        // unparsable row → default []
        store(&conn, "ui.tabs", &Value::String("]".into())).unwrap();
        assert!(tabs(&conn).is_empty());
    }

    #[test]
    fn stale_settings_panel_is_dropped_from_tabs_and_windows() {
        let conn = mem();
        // a stored ui.tabs still naming the removed «Настройки» panel
        store(&conn, "ui.tabs", &json!(["settings", "deps"])).unwrap();
        assert_eq!(tabs(&conn), vec!["deps".to_string()]);
        // the label is no longer a panel, so it cannot be a window scope either
        assert!(!PANELS.contains(&"settings"));
        assert_eq!(PANELS.len(), 9);
        assert!(split_scoped("win:settings:ui.statusbar").is_none());
    }
}

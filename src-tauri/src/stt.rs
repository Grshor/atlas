//! Голосовой ввод (push-to-talk): запись с микрофона через `pw-record` и
//! локальная расшифровка `voxtype transcribe` (whisper large-v3-turbo).
//!
//! Поток: `POST /api/stt/start` поднимает `pw-record --rate 16000 --channels 1
//! --format s16 <tmpdir>/atlas-stt-<id>.wav` (источник по умолчанию, argv без
//! шелла) и запоминает `{id, wav, child, started}`; вторая запись, пока первая
//! идёт → 409. `POST /api/stt/stop` синхронный: SIGTERM завершает pw-record
//! (дозапись WAV-заголовка), затем `voxtype -c <temp config> transcribe <wav>`
//! — текст в ответе. Запись старше 60 с (voxtype `max_duration_secs`)
//! авто-останавливается watchdog'ом тем же путём остановки, результат
//! держится до первого `stop`-запроса.
//!
//! Язык. У `voxtype transcribe` нет флага языка (проверено: только
//! `--engine`), а в конфиге демона `language = "en"` — пользователь диктует
//! по-русски. Поэтому каждая расшифровка идёт с одноразовым конфигом
//! `-c`, сгенерированным из пользовательского `~/.config/voxtype/config.toml`
//! переписыванием строки `language` на `"ru"` (движок и модель сохраняются;
//! конфиг отсутствует — минимальный `engine = "whisper"` + `language`).
//! Проверено на этой машине: `voxtype -c <tmp> config` резолвит
//! `language = Single("ru")` при сохранённом `model = "large-v3-turbo"`, и
//! реальный прогон выдаёт русский текст.
//!
//! Задержка: каждый stop — свежий процесс voxtype, холодная загрузка модели
//! ~4.5 с (замер), с горячим страничным кэшем ~1–2 с; HTTP-ответ синхронный,
//! фронтенд показывает спиннер и держит длинный таймаут (свой cap —
//! [`TRANSCRIBE_TIMEOUT_SECS`]).

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tokio::sync::Mutex as AsyncMutex;

use crate::cfg;

/// voxtype's own recording safety limit: a recording still live after this
/// many seconds is force-finished by the watchdog (treated as a stop).
pub const MAX_RECORD_SECS: u64 = 60;
/// Every stop runs a fresh voxtype: the model load is the cold start
/// (measured ~4.5 s here, ~1–2 s warm). The stop request is synchronous, so
/// this is its hard cap.
pub const TRANSCRIBE_TIMEOUT_SECS: u64 = 300;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ------------------------------------------------------------ argv builders ---

/// The recorder argv: default source (no `--target`), 16 kHz mono s16 — the
/// shape voxtype expects («WAV, 16kHz, mono»). Recorder: config
/// `binaries.pw_record`.
pub fn pw_record_argv(wav: &Path) -> Vec<String> {
    vec![
        cfg::pw_record_bin().into(),
        "--rate".into(),
        "16000".into(),
        "--channels".into(),
        "1".into(),
        "--format".into(),
        "s16".into(),
        wav.to_string_lossy().into_owned(),
    ]
}

/// The transcription argv: the throwaway ru-config (no per-run language flag
/// exists) + the wav. `-q` silences voxtype's tracing log (verified: colored
/// INFO lines «Using local whisper…/Model loaded…/Transcription completed…»
/// go to stdout and would otherwise pollute the parsed text); the three
/// progress banners remain and are filtered by [`parse_stdout`]. Transcriber:
/// config `binaries.voxtype`.
pub fn transcribe_argv(ru_cfg: &Path, wav: &Path) -> Vec<String> {
    vec![
        cfg::voxtype_bin().into(),
        "-q".into(),
        "-c".into(),
        ru_cfg.to_string_lossy().into_owned(),
        "transcribe".into(),
        wav.to_string_lossy().into_owned(),
    ]
}

/// The throwaway per-run config: the user's config with every `language` line
/// replaced by one `language = "ru"` (first position preserved, engine/model
/// and everything else untouched). No user config → a minimal config naming
/// the engine (voxtype's own default) + the language.
pub fn ru_config_text(user: Option<&str>) -> String {
    let mut out = String::new();
    let mut set = false;
    for line in user.unwrap_or("").lines() {
        let key = line.split('=').next().map(|k| k.trim()).unwrap_or("");
        if key == "language" {
            if !set {
                out.push_str("language = \"ru\"\n");
                set = true;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !set {
        if user.is_none() {
            out.push_str("engine = \"whisper\"\n");
        }
        out.push_str("language = \"ru\"\n");
    }
    out
}

/// voxtype prints three progress lines on stdout («Loading audio file: …»,
/// «Audio format: …», «Processing N samples…») and the transcription as the
/// remaining output. ANSI colouring is stripped first (a non-tty stdout may
/// still carry it), the text is everything else, trimmed.
pub fn parse_stdout(out: &str) -> String {
    let clean = strip_ansi(out);
    clean
        .lines()
        .filter(|l| {
            let t = l.trim();
            if t.is_empty()
                || t.starts_with("Loading audio file:")
                || t.starts_with("Audio format:")
                || t.starts_with("Processing ")
            {
                return false;
            }
            // residual tracing-log lines (ANSI already stripped); with `-q`
            // voxtype should not print them at all — this is belt and braces
            !matches!(
                t.split_whitespace().next(),
                Some("INFO") | Some("WARN") | Some("ERROR") | Some("DEBUG") | Some("TRACE")
            )
        })
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Remove ANSI SGR escape sequences (`\x1b[…m`).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.next() == Some('[') {
            for c2 in chars.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ------------------------------------------------------- injected side effects ---

/// The recorder: one live child writing a headered 16 kHz mono s16 WAV at the
/// given path. Real = `pw-record`; tests inject a fake (a never-exiting
/// `cat`) so no audio device is touched.
pub trait Recorder: Send + Sync + 'static {
    fn spawn(&self, wav: &Path) -> Result<Child, String>;
}

pub struct PwRecord;

impl Recorder for PwRecord {
    fn spawn(&self, wav: &Path) -> Result<Child, String> {
        let argv = pw_record_argv(wav);
        Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("{e}"))
    }
}

/// The transcription engine: real = `voxtype -c <ru-config> transcribe <wav>`
/// (blocking call — the handler runs it on the blocking pool).
pub trait Transcriber: Send + Sync + 'static {
    fn transcribe(&self, wav: &Path) -> Result<String, String>;
}

pub struct Voxtype;

impl Transcriber for Voxtype {
    fn transcribe(&self, wav: &Path) -> Result<String, String> {
        // fresh temp config per call: the user's config may change between
        // stops; removed again right after the run
        let user = std::fs::read_to_string(cfg::voxtype_config()).ok();
        let tmp = std::env::temp_dir().join(format!("atlas-stt-cfg-{}.toml", std::process::id()));
        std::fs::write(&tmp, ru_config_text(user.as_deref()))
            .map_err(|e| format!("temp config: {e}"))?;
        let argv = transcribe_argv(&tmp, wav);
        let result = self.run(&argv);
        let _ = std::fs::remove_file(&tmp);
        result
    }
}

impl Voxtype {
    fn run(&self, argv: &[String]) -> Result<String, String> {
        use std::io::Read;
        let mut child = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("voxtype: {e}"))?;
        // stderr is drained on its own thread so whisper's init log can never
        // fill the pipe and stall the child while we poll; stdout holds only
        // a few lines and is read after the exit.
        let mut stderr_pipe = child.stderr.take().expect("voxtype stderr");
        let err_t = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr_pipe.read_to_string(&mut s);
            s
        });
        let deadline = Instant::now() + Duration::from_secs(TRANSCRIBE_TIMEOUT_SECS);
        loop {
            match child
                .try_wait()
                .map_err(|e| format!("voxtype wait: {e}"))?
            {
                Some(st) => {
                    let err = err_t.join().unwrap_or_default();
                    let mut stdout_pipe = child.stdout.take().expect("voxtype stdout");
                    let mut out = String::new();
                    let _ = stdout_pipe.read_to_string(&mut out);
                    break if st.success() {
                        Ok(parse_stdout(&out))
                    } else {
                        Err(format!(
                            "voxtype exit {st}: {}",
                            last_line(&err).unwrap_or_else(|| "нет вывода".into())
                        ))
                    };
                }
                None => {
                    if Instant::now() > deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!(
                            "voxtype: таймаут расшифровки ({TRANSCRIBE_TIMEOUT_SECS} с)"
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

/// The last non-empty line of a log (the actual error hides under whisper's
/// init noise).
fn last_line(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(String::from)
}

// ------------------------------------------------------------------ state ---

/// One recording in flight.
pub struct Rec {
    pub id: String,
    pub wav: PathBuf,
    pub started_ms: u64,
    child: AsyncMutex<Child>,
}

impl Rec {
    /// SIGTERM so pw-record finalises the WAV header, then reap; a child that
    /// ignores the term for 5 s is killed.
    async fn terminate(&self) {
        let pid = self.child.lock().await.id();
        if let Some(pid) = pid {
            let _ = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .output()
                .await;
        }
        let mut guard = self.child.lock().await;
        match tokio::time::timeout(Duration::from_secs(5), guard.wait()).await {
            Ok(_) => {}
            Err(_) => {
                let _ = guard.start_kill();
                let _ = guard.wait().await;
            }
        }
    }
}

/// Why a start could not begin a recording.
#[derive(Debug)]
pub enum StartError {
    /// A recording is already active → 409.
    Busy,
    /// The recorder could not be spawned (no mic/pw-record) → honest detail.
    Failed(String),
}

/// Why a stop had nothing to stop.
#[derive(Debug)]
pub struct NotRecording;

pub struct SttState {
    /// The active recording, if any.
    rec: Mutex<Option<Rec>>,
    /// True while a transcription runs (the `transcribing` status field).
    transcribing: AtomicBool,
    /// Single in-flight transcription.
    trans_lock: AsyncMutex<()>,
    /// The auto-stop's result, held until a stop request collects it.
    held: Mutex<Option<Value>>,
    spawner: Arc<dyn Recorder>,
    transcriber: Arc<dyn Transcriber>,
}

impl SttState {
    pub fn new() -> Arc<Self> {
        Self::with(Arc::new(PwRecord), Arc::new(Voxtype))
    }

    pub fn with(spawner: Arc<dyn Recorder>, transcriber: Arc<dyn Transcriber>) -> Arc<Self> {
        Arc::new(SttState {
            rec: Mutex::new(None),
            transcribing: AtomicBool::new(false),
            trans_lock: AsyncMutex::new(()),
            held: Mutex::new(None),
            spawner,
            transcriber,
        })
    }

    /// `GET /api/stt/status`.
    pub fn status(&self) -> Value {
        json!({
            "recording": self.rec.lock().is_some(),
            "transcribing": self.transcribing.load(Ordering::SeqCst),
        })
    }

    /// `POST /api/stt/start`: spawn the recorder (or `Busy`) and arm the
    /// 60 s watchdog.
    pub fn start(self: &Arc<Self>) -> Result<Value, StartError> {
        let mut slot = self.rec.lock();
        if slot.is_some() {
            return Err(StartError::Busy);
        }
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let id = format!("stt-{}-{seq:03}", now_ms());
        let wav = std::env::temp_dir().join(format!("atlas-stt-{id}.wav"));
        let child = self
            .spawner
            .spawn(&wav)
            .map_err(StartError::Failed)?;
        let rec = Rec {
            id: id.clone(),
            wav,
            started_ms: now_ms(),
            child: AsyncMutex::new(child),
        };
        *slot = Some(rec);
        drop(slot);
        let me = Arc::clone(self);
        let watch_id = id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(MAX_RECORD_SECS)).await;
            // still the same recording? → force-stop it exactly like the endpoint
            let same = {
                let mut g = me.rec.lock();
                match g.as_ref() {
                    Some(r) if r.id == watch_id => Some(g.take().expect("just checked")),
                    _ => None,
                }
            };
            if let Some(rec) = same {
                log::warn!("stt: запись {watch_id} превысила {MAX_RECORD_SECS} с — авто-стоп");
                me.finish(rec, true).await;
            }
        });
        Ok(json!({"ok": true, "id": id}))
    }

    /// `POST /api/stt/stop`: finish the active recording (or collect the
    /// auto-stopped one) synchronously — the response carries the text.
    pub async fn stop(&self) -> Result<Value, NotRecording> {
        let rec = self
            .rec
            .lock()
            .take();
        let Some(rec) = rec else {
                if self.transcribing.load(Ordering::SeqCst) {
                let _g = self.trans_lock.lock().await;
                if let Some(h) = self.take_held() {
                    return Ok(h);
                }
            }
            return self.take_held().ok_or(NotRecording);
        };
        Ok(self.finish(rec, false).await)
    }

    fn take_held(&self) -> Option<Value> {
        self.held
            .lock()
            .take()
    }

    /// SIGTERM → transcribe (single in-flight) → wav cleanup → response body.
    /// `hold:true` (auto-stop path) keeps the result for the next stop call
    /// instead of returning it to a caller.
    ///
    /// The `transcribing` flag is set BEFORE the terminate and cleared only
    /// after the held result is stored: a stop request that races the 60 s
    /// auto-stop must either find the recording still live (take it itself)
    /// or see `transcribing` and wait on the lock — never fall into the
    /// «не активно» gap.
    async fn finish(&self, rec: Rec, hold: bool) -> Value {
        let duration_s = (now_ms().saturating_sub(rec.started_ms)) as f64 / 1000.0;
        self.transcribing.store(true, Ordering::SeqCst);
        // the lock spans the whole finish — terminate included — so a stop
        // that races this auto-stop waits it out instead of finding nothing
        let _g = self.trans_lock.lock().await;
        rec.terminate().await;
        let transcriber = Arc::clone(&self.transcriber);
        let wav = rec.wav.clone();
        let res = tokio::task::spawn_blocking(move || transcriber.transcribe(&wav))
            .await
            .map_err(|e| format!("transcribe task: {e}"))
            .and_then(|r| r);
        // the wav is always the recording's own temp litter — gone either way
        let _ = std::fs::remove_file(&rec.wav);
        let body = match res {
            Ok(text) => json!({"ok": true, "text": text, "duration_s": duration_s}),
            Err(e) => {
                log::warn!("stt: расшифровка не удалась: {e}");
                json!({"ok": false, "detail": e, "duration_s": duration_s})
            }
        };
        if hold {
            *self.held.lock() = Some(body.clone());
        }
        self.transcribing.store(false, Ordering::SeqCst);
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A never-exiting child (cat on a held-open stdin) stands in for the
    /// recorder — no audio device is touched; SIGTERM-able like pw-record.
    struct FakeRecorder;

    impl Recorder for FakeRecorder {
        fn spawn(&self, _wav: &Path) -> Result<Child, String> {
            Command::new("cat")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| format!("{e}"))
        }
    }

    /// Echoes the wav path back as "the text" — and fails when the wav is
    /// gone, so cleanup is observable through the outcome.
    struct EchoTranscriber;

    impl Transcriber for EchoTranscriber {
        fn transcribe(&self, wav: &Path) -> Result<String, String> {
            if wav.exists() {
                Ok(format!("текст из {}", wav.display()))
            } else {
                Err("wav пропал до расшифровки".into())
            }
        }
    }

    fn test_state() -> Arc<SttState> {
        SttState::with(Arc::new(FakeRecorder), Arc::new(EchoTranscriber))
    }

    #[test]
    fn argv_builders_are_exact() {
        assert_eq!(
            pw_record_argv(Path::new("/tmp/atlas-stt-stt-1.wav")),
            vec![
                "pw-record", "--rate", "16000", "--channels", "1", "--format", "s16",
                "/tmp/atlas-stt-stt-1.wav"
            ]
        );
        assert_eq!(
            transcribe_argv(Path::new("/tmp/ru.toml"), Path::new("/tmp/a.wav")),
            vec!["voxtype", "-q", "-c", "/tmp/ru.toml", "transcribe", "/tmp/a.wav"],
            "no language flag exists on `voxtype transcribe` — the -c config is the mechanism; \
             -q keeps the tracing log out of the parsed text"
        );
    }

    /// The ru-config derivation: the user's `language` line (the daemon says
    /// "en") is rewritten to "ru" in place, engine/model and the rest of the
    /// config survive; duplicates collapse; a missing config degrades to a
    /// minimal one.
    #[test]
    fn ru_config_rewrites_only_the_language() {
        let user = "engine = \"whisper\"\nlanguage = \"en\"\nmodel = \"large-v3-turbo\"\n[audio]\nsample_rate = 16000\n";
        assert_eq!(
            ru_config_text(Some(user)),
            "engine = \"whisper\"\nlanguage = \"ru\"\nmodel = \"large-v3-turbo\"\n[audio]\nsample_rate = 16000\n"
        );
        // no language line → appended at the end
        assert_eq!(
            ru_config_text(Some("engine = \"whisper\"\n")),
            "engine = \"whisper\"\nlanguage = \"ru\"\n"
        );
        // duplicate language lines collapse into one
        assert_eq!(
            ru_config_text(Some("language = \"en\"\nlanguage = \"de\"\n")),
            "language = \"ru\"\n"
        );
        // no user config at all → minimal, engine named explicitly
        assert_eq!(
            ru_config_text(None),
            "engine = \"whisper\"\nlanguage = \"ru\"\n"
        );
    }

    #[test]
    fn stdout_parsing_keeps_only_the_text() {
        let out = "Loading audio file: \"/tmp/a.wav\"\n\
                   Audio format: 16000 Hz, 1 channel(s), Int\n\
                   Processing 32000 samples (2.00s)...\n\
                   \n\
                   Субтитры создавал DimaTorzok\n";
        assert_eq!(parse_stdout(out), "Субтитры создавал DimaTorzok");
        // no banner lines (future voxtype) → everything is text
        assert_eq!(parse_stdout("просто текст"), "просто текст");
        // silence → empty text is honest
        assert_eq!(parse_stdout("Processing 0 samples (0.00s)...\n\n"), "");
        // ANSI-coloured INFO leftovers never leak into the text
        let colored = "Loading audio file: \"/tmp/a.wav\"\n\x1b[2mINFO\x1b[0m Model loaded in 1.2s\n\
                       Processing 10 samples (0.00s)...\n\x1b[32m\x1b[0m Текст\x1b[0m\n";
        assert_eq!(parse_stdout(colored), "Текст");
    }

    #[tokio::test]
    async fn start_is_single_and_stop_round_trips() {
        let stt = test_state();
        // no recording yet → stop has nothing (and nothing held)
        assert!(matches!(stt.stop().await, Err(NotRecording)));

        let first = stt.start().expect("first start");
        assert_eq!(first["ok"], true);
        let id = first["id"].as_str().unwrap().to_string();

        // second start while the first is live → Busy (the endpoint's 409)
        assert!(matches!(stt.start(), Err(StartError::Busy)));
        assert_eq!(stt.status()["recording"], true);
        assert_eq!(stt.status()["transcribing"], false);

        // make it look like something was recorded
        let wav = {
            let g = stt.rec.lock();
            g.as_ref().unwrap().wav.clone()
        };
        std::fs::write(&wav, b"RIFFfake").expect("fake wav");

        let stopped = stt.stop().await.expect("stop");
        assert_eq!(stopped["ok"], true, "{stopped}");
        assert_eq!(
            stopped["text"],
            format!("текст из {}", wav.display()),
            "the engine saw the wav before cleanup"
        );
        assert!(stopped["duration_s"].is_number());
        assert_eq!(stt.status()["recording"], false);
        // the temp wav is gone after the stop
        assert!(!wav.exists(), "wav must be cleaned up: {wav:?}");
        assert!(id.starts_with("stt-"));

        // and once collected, a further stop is honestly empty again
        assert!(matches!(stt.stop().await, Err(NotRecording)));
    }

    /// The 60 s watchdog force-stops the recording through the real stop
    /// path; tokio's paused clock elapses the full minute instantly (the
    /// watchdog's 60 s timer wakes before the test's 65 s one). The result
    /// is held until a stop request collects it — `stop()` itself waits out
    /// the in-flight transcription on the lock, so its return is the
    /// deterministic "finish completed" point.
    #[tokio::test(start_paused = true)]
    async fn auto_stop_at_60s_treats_it_as_a_stop() {
        let stt = test_state();
        let first = stt.start().expect("start");
        assert!(first["id"].as_str().unwrap().starts_with("stt-"));
        let wav = {
            let g = stt.rec.lock();
            g.as_ref().unwrap().wav.clone()
        };
        std::fs::write(&wav, b"RIFFfake").expect("fake wav");

        // run past the limit
        tokio::time::sleep(Duration::from_secs(MAX_RECORD_SECS + 5)).await;

        // the watchdog takes the slot synchronously the moment its 60 s timer
        // fires — before any await inside finish
        assert_eq!(stt.status()["recording"], false, "watchdog stopped it");
        // collecting waits out the terminate+transcribe and proves the rest:
        // text, held delivery, wav cleanup
        let collected = stt.stop().await.expect("held result");
        assert_eq!(collected["ok"], true, "{collected}");
        assert_eq!(collected["text"], format!("текст из {}", wav.display()));
        assert!(!wav.exists(), "auto-stop cleans the wav too: {wav:?}");
    }

    /// A stop racing the in-flight auto-stop transcription waits for it and
    /// returns the held result instead of a 409.
    #[tokio::test(start_paused = true)]
    async fn stop_during_auto_stop_transcription_collects_the_result() {
        struct SlowTranscriber;
        impl Transcriber for SlowTranscriber {
            fn transcribe(&self, _wav: &Path) -> Result<String, String> {
                std::thread::sleep(Duration::from_millis(50));
                Ok("медленный текст".into())
            }
        }
        let stt = SttState::with(Arc::new(FakeRecorder), Arc::new(SlowTranscriber));
        stt.start().expect("start");
        // watchdog fires first (60 s < 65 s) and is mid-finish when the test
        // resumes: `transcribing` is already up (set before the terminate),
        // so stop() waits on the lock instead of reporting «не активно»
        tokio::time::sleep(Duration::from_secs(MAX_RECORD_SECS + 5)).await;
        assert_eq!(stt.status()["recording"], false);
        let out = stt.stop().await.expect("collected after waiting");
        assert_eq!(out["text"], "медленный текст", "{out}");
    }
}


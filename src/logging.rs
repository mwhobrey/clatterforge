//! Structured JSON Lines (JSONL) logging for Clatterforge (`dev-conductor`).
//!
//! Writes single-line JSON objects to `stderr` (never `stdout`, preserving JSON CLI output
//! and stdio MCP transport) and appends to a size-rotated `.jsonl` file under
//! `<state>/logs/clatterforge.jsonl`.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU8, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

pub const DEFAULT_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
pub const DEFAULT_MAX_ROTATED_FILES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum LogLevel {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if let Some(level) = parse_single_token(trimmed) {
            return Ok(level);
        }
        // Accept RUST_LOG-style directives like "dev_conductor=debug,axum=warn"
        let mut fallback = None;
        for part in trimmed.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some((target, lvl)) = part.split_once('=') {
                let target = target.trim();
                if target.eq_ignore_ascii_case("dev_conductor")
                    || target.eq_ignore_ascii_case("dev-conductor")
                    || target.eq_ignore_ascii_case("clatterforge")
                {
                    if let Some(level) = parse_single_token(lvl.trim()) {
                        return Ok(level);
                    }
                } else if fallback.is_none() {
                    fallback = parse_single_token(lvl.trim());
                }
            } else if fallback.is_none() {
                fallback = parse_single_token(part);
            }
        }
        if let Some(level) = fallback {
            return Ok(level);
        }
        bail!("invalid log level '{raw}' (expected error, warn, info, debug, or trace)")
    }

    pub fn from_env(default: Self) -> Self {
        for var in ["CLATTERFORGE_LOG", "CONDUCTOR_LOG", "RUST_LOG"] {
            if let Ok(val) = std::env::var(var)
                && let Ok(parsed) = Self::parse(&val)
            {
                return parsed;
            }
        }
        default
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Error,
            2 => Self::Warn,
            3 => Self::Info,
            4 => Self::Debug,
            _ => Self::Trace,
        }
    }
}

fn parse_single_token(s: &str) -> Option<LogLevel> {
    match s.to_ascii_lowercase().as_str() {
        "error" | "err" => Some(LogLevel::Error),
        "warn" | "warning" => Some(LogLevel::Warn),
        "info" => Some(LogLevel::Info),
        "debug" | "dbg" => Some(LogLevel::Debug),
        "trace" => Some(LogLevel::Trace),
        _ => None,
    }
}

pub fn log_file_path(root: &Path) -> PathBuf {
    root.join("logs").join("clatterforge.jsonl")
}

#[derive(Debug, Clone)]
pub struct LogConfig {
    pub level: LogLevel,
    pub file: Option<PathBuf>,
    pub max_file_bytes: u64,
    pub max_rotated_files: usize,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: LogLevel::Warn,
            file: None,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_rotated_files: DEFAULT_MAX_ROTATED_FILES,
        }
    }
}

/// Size-rotated JSONL file sink that is safe to share a path with other sinks and processes.
///
/// It holds no open handle and no byte counter. Each line opens the live path in append
/// mode, writes in one call and closes, so every writer (the global logger, a `JsonLogger`
/// in `serve`, MCP and CLI processes) always appends to whatever file currently sits at the
/// live path, and a rotation by any of them is followed at once. The rotation decision reads
/// the real file size, and rotation itself runs under an exclusive lock file, so two writers
/// never rotate the same file twice.
struct FileSink {
    path: PathBuf,
    max_bytes: u64,
    max_rotated: usize,
}

/// A rotation lock older than this is treated as left behind by a crashed writer.
const ROTATION_LOCK_STALE_MS: u64 = 30_000;

impl FileSink {
    fn open(path: PathBuf, max_bytes: u64, max_rotated: usize) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        // Fail early, as before, when the path cannot be appended to at all.
        drop(OpenOptions::new().create(true).append(true).open(&path)?);
        Ok(Self {
            path,
            max_bytes: max_bytes.max(1024),
            max_rotated,
        })
    }

    fn write_line(&mut self, line: &str) {
        let mut record = String::with_capacity(line.len() + 1);
        record.push_str(line);
        record.push('\n');
        let current = fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if current.saturating_add(record.len() as u64) > self.max_bytes {
            self.rotate();
        }
        // One `write_all` of the whole record on an append handle: concurrent writers can
        // interleave lines but not split one.
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = file.write_all(record.as_bytes());
        }
    }

    fn lock_path(&self) -> PathBuf {
        let mut os = self.path.as_os_str().to_os_string();
        os.push(".rotate.lock");
        PathBuf::from(os)
    }

    /// Takes the cross-process rotation lock, clearing one stale lock left by a crashed
    /// writer. Returns `None` when another writer is rotating; this writer then just appends.
    fn acquire_rotation_lock(&self) -> Option<RotationLock> {
        let lock = self.lock_path();
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&lock) {
                Ok(_) => return Some(RotationLock(lock)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&lock)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|at| SystemTime::now().duration_since(at).ok())
                        .is_some_and(|age| age.as_millis() as u64 > ROTATION_LOCK_STALE_MS);
                    if !stale {
                        return None;
                    }
                    let _ = fs::remove_file(&lock);
                }
                Err(_) => return None,
            }
        }
        None
    }

    fn rotate(&mut self) {
        let Some(_lock) = self.acquire_rotation_lock() else {
            return;
        };
        // Another writer may have rotated between our size check and taking the lock.
        let current = fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if current < self.max_bytes {
            return;
        }
        if self.max_rotated == 0 {
            // Explicitly configured to keep no history.
            let _ = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&self.path);
            return;
        }
        for idx in (1..=self.max_rotated).rev() {
            let dst = rotated_path(&self.path, idx);
            if idx == self.max_rotated {
                let _ = fs::remove_file(&dst);
            }
            if idx > 1 {
                let src = rotated_path(&self.path, idx - 1);
                if src.exists() {
                    let _ = fs::rename(&src, &dst);
                }
            }
        }
        // If this rename fails (the file is held without share-delete, an AV scanner has it
        // open), the live log is left exactly as it is and keeps growing until a later write
        // rotates it. It is never truncated: the next append recreates the live file only
        // after a successful rename has moved the old one out of the way.
        let _ = fs::rename(&self.path, rotated_path(&self.path, 1));
    }
}

/// Removes the rotation lock file when the rotation finishes or unwinds.
struct RotationLock(PathBuf);

impl Drop for RotationLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn rotated_path(base: &Path, index: usize) -> PathBuf {
    let mut os = base.as_os_str().to_os_string();
    os.push(format!(".{index}"));
    PathBuf::from(os)
}

struct GlobalLogger {
    sink: Option<FileSink>,
    emit_stderr: bool,
}

static ACTIVE_LEVEL: AtomicU8 = AtomicU8::new(LogLevel::Warn as u8);
static GLOBAL_LOGGER: OnceLock<Mutex<GlobalLogger>> = OnceLock::new();

fn global_logger() -> &'static Mutex<GlobalLogger> {
    GLOBAL_LOGGER.get_or_init(|| {
        Mutex::new(GlobalLogger {
            sink: None,
            emit_stderr: true,
        })
    })
}

/// Initializes or updates the global structured JSONL logger.
pub fn init(config: LogConfig) {
    ACTIVE_LEVEL.store(config.level as u8, Ordering::Relaxed);
    let sink = config.file.and_then(|path| {
        FileSink::open(path, config.max_file_bytes, config.max_rotated_files).ok()
    });
    if let Ok(mut guard) = global_logger().lock() {
        guard.sink = sink;
        guard.emit_stderr = true;
    }
}

pub fn active_level() -> LogLevel {
    LogLevel::from_u8(ACTIVE_LEVEL.load(Ordering::Relaxed))
}

#[inline]
pub fn enabled(level: LogLevel) -> bool {
    (level as u8) <= ACTIVE_LEVEL.load(Ordering::Relaxed)
}

/// Per-instance structured JSONL logger backed by `<state>/logs/clatterforge.jsonl`
/// with rotation, optional stderr mirroring, level filtering, and `/api/logs` reading.
pub struct JsonLogger {
    path: PathBuf,
    emit_stderr: bool,
    min_level: Option<LogLevel>,
    sink: Mutex<Option<FileSink>>,
}

impl JsonLogger {
    pub fn new(root: &Path) -> Self {
        Self::with_options(root, true)
    }

    pub fn with_options(root: &Path, emit_stderr: bool) -> Self {
        let path = log_file_path(root);
        let sink = FileSink::open(
            path.clone(),
            DEFAULT_MAX_FILE_BYTES,
            DEFAULT_MAX_ROTATED_FILES,
        )
        .ok();
        Self {
            path,
            emit_stderr,
            min_level: None,
            sink: Mutex::new(sink),
        }
    }

    pub fn with_level(root: &Path, emit_stderr: bool, min_level: LogLevel) -> Self {
        let mut logger = Self::with_options(root, emit_stderr);
        logger.min_level = Some(min_level);
        logger
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn effective_level(&self) -> LogLevel {
        match self.min_level {
            Some(lvl) => lvl,
            None => {
                // Respect the configured global level (e.g. --log-level / CLATTERFORGE_LOG),
                // but ensure tests that never call logging::init() still record Info+ events.
                if GLOBAL_LOGGER.get().is_some() {
                    active_level()
                } else {
                    LogLevel::from_env(LogLevel::Info)
                }
            }
        }
    }

    pub fn is_enabled(&self, level: LogLevel) -> bool {
        level <= self.effective_level()
    }

    pub fn log(
        &self,
        level: &str,
        component: &str,
        event: &str,
        request_id: Option<String>,
        message: impl Into<String>,
        fields: Value,
    ) {
        let parsed_level = LogLevel::parse(level).unwrap_or(LogLevel::Info);
        if !self.is_enabled(parsed_level) {
            return;
        }
        let msg = message.into();
        let line = format_structured_record(
            parsed_level,
            component,
            event,
            request_id.as_deref(),
            &msg,
            Some(fields),
        );
        if self.emit_stderr {
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(stderr, "{line}");
        }
        if let Ok(mut guard) = self.sink.lock() {
            if guard.is_none() {
                *guard = FileSink::open(
                    self.path.clone(),
                    DEFAULT_MAX_FILE_BYTES,
                    DEFAULT_MAX_ROTATED_FILES,
                )
                .ok();
            }
            if let Some(sink) = guard.as_mut() {
                sink.write_line(&line);
            }
        }
    }

    pub fn error(
        &self,
        component: &str,
        event: &str,
        request_id: Option<String>,
        message: impl Into<String>,
        fields: Value,
    ) {
        self.log("error", component, event, request_id, message, fields);
    }

    pub fn warn(
        &self,
        component: &str,
        event: &str,
        request_id: Option<String>,
        message: impl Into<String>,
        fields: Value,
    ) {
        self.log("warn", component, event, request_id, message, fields);
    }

    pub fn info(
        &self,
        component: &str,
        event: &str,
        request_id: Option<String>,
        message: impl Into<String>,
        fields: Value,
    ) {
        self.log("info", component, event, request_id, message, fields);
    }

    pub fn debug(
        &self,
        component: &str,
        event: &str,
        request_id: Option<String>,
        message: impl Into<String>,
        fields: Value,
    ) {
        self.log("debug", component, event, request_id, message, fields);
    }

    pub fn trace(
        &self,
        component: &str,
        event: &str,
        request_id: Option<String>,
        message: impl Into<String>,
        fields: Value,
    ) {
        self.log("trace", component, event, request_id, message, fields);
    }

    pub fn read_recent(
        &self,
        limit: usize,
        level: Option<&str>,
        event: Option<&str>,
    ) -> Result<Vec<Value>> {
        read_recent_from_path(&self.path, limit, level, event)
    }
}

pub fn read_recent_from_path(
    path: &Path,
    limit: usize,
    level: Option<&str>,
    event: Option<&str>,
) -> Result<Vec<Value>> {
    if limit == 0 || !path.is_file() {
        return Ok(Vec::new());
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("read log file {}", path.display()))?;
    let level_filter = level.map(|s| s.trim().to_ascii_lowercase());
    let event_filter = event.map(|s| s.trim().to_ascii_lowercase());
    let mut matched = Vec::new();
    for line in content.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(ref want_level) = level_filter {
            let entry_level = entry
                .get("level")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            if entry_level != *want_level {
                continue;
            }
        }
        if let Some(ref want_event) = event_filter {
            let entry_event = entry
                .get("event")
                .or_else(|| entry.get("target"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            if !entry_event.contains(want_event) {
                continue;
            }
        }
        matched.push(entry);
        if matched.len() >= limit {
            break;
        }
    }
    matched.reverse();
    Ok(matched)
}

pub fn truncate_preview(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut chars = input.chars();
    let collected: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{collected}...")
    } else {
        collected
    }
}

/// Redacts sensitive query parameter values (tokens, codes, secrets, keys, passwords)
/// before recording HTTP query strings in structured logs.
pub fn sanitize_query(query: &str) -> String {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => {
                let lower = key.to_ascii_lowercase();
                if lower.contains("token")
                    || lower.contains("secret")
                    || lower.contains("password")
                    || lower.contains("key")
                    || lower.contains("code")
                    || lower.contains("auth")
                {
                    format!("{key}=[REDACTED]")
                } else {
                    format!("{key}={}", truncate_preview(value, 128))
                }
            }
            None => truncate_preview(pair, 128),
        })
        .collect::<Vec<_>>()
        .join("&")
}

pub fn format_structured_record(
    level: LogLevel,
    component: &str,
    event: &str,
    request_id: Option<&str>,
    message: &str,
    fields: Option<Value>,
) -> String {
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let ts = format_iso8601_utc(ts_ms);
    let pid = std::process::id();
    let mut obj = serde_json::Map::new();
    obj.insert("ts".into(), json!(ts));
    obj.insert("ts_ms".into(), json!(ts_ms));
    obj.insert("level".into(), json!(level.as_str()));
    obj.insert("component".into(), json!(component));
    obj.insert("event".into(), json!(event));
    obj.insert("pid".into(), json!(pid));
    if let Some(req_id) = request_id
        && !req_id.is_empty()
    {
        obj.insert("request_id".into(), json!(req_id));
    }
    obj.insert("message".into(), json!(message));
    match fields {
        Some(Value::Object(map)) if !map.is_empty() => {
            obj.insert("fields".into(), Value::Object(map));
        }
        Some(other) if !other.is_null() && !matches!(&other, Value::Object(m) if m.is_empty()) => {
            obj.insert("fields".into(), other);
        }
        _ => {}
    }
    serde_json::to_string(&Value::Object(obj)).unwrap_or_else(|_| {
        format!(
            r#"{{"ts":"{ts}","level":"{}","component":"{component}","event":"{event}","pid":{pid},"message":"(log serialization error)"}}"#,
            level.as_str()
        )
    })
}

pub fn emit(level: LogLevel, target: &str, msg: &str, fields: Option<Value>) {
    if !enabled(level) {
        return;
    }
    let (component, event) = target.split_once('.').unwrap_or((target, "log"));
    let line = format_structured_record(level, component, event, None, msg, fields);
    if let Ok(mut guard) = global_logger().lock() {
        if guard.emit_stderr {
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(stderr, "{line}");
        }
        if let Some(sink) = guard.sink.as_mut() {
            sink.write_line(&line);
        }
    }
}

/// Extracts an `anyhow::Error` and its causal chain into structured JSON fields.
pub fn error_fields(err: &anyhow::Error) -> Value {
    let causes: Vec<String> = err.chain().skip(1).map(|c| c.to_string()).collect();
    if causes.is_empty() {
        json!({ "error": err.to_string() })
    } else {
        json!({ "error": err.to_string(), "causes": causes })
    }
}

/// Formats epoch milliseconds as RFC 3339 / ISO-8601 UTC (`YYYY-MM-DDTHH:MM:SS.sssZ`)
/// using Howard Hinnant's civil-from-days algorithm (zero external date crates).
pub fn format_iso8601_utc(epoch_ms: u64) -> String {
    let millis = epoch_ms % 1000;
    let total_secs = epoch_ms / 1000;
    let secs_of_day = total_secs % 86_400;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    let z = (total_secs / 86_400) as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

#[macro_export]
macro_rules! log_error {
    ($target:expr, $msg:expr $(,)?) => {
        $crate::logging::emit($crate::logging::LogLevel::Error, $target, $msg, None)
    };
    ($target:expr, $msg:expr, $fields:tt $(,)?) => {
        if $crate::logging::enabled($crate::logging::LogLevel::Error) {
            $crate::logging::emit(
                $crate::logging::LogLevel::Error,
                $target,
                $msg,
                Some(serde_json::json!($fields)),
            );
        }
    };
}

#[macro_export]
macro_rules! log_warn {
    ($target:expr, $msg:expr $(,)?) => {
        $crate::logging::emit($crate::logging::LogLevel::Warn, $target, $msg, None)
    };
    ($target:expr, $msg:expr, $fields:tt $(,)?) => {
        if $crate::logging::enabled($crate::logging::LogLevel::Warn) {
            $crate::logging::emit(
                $crate::logging::LogLevel::Warn,
                $target,
                $msg,
                Some(serde_json::json!($fields)),
            );
        }
    };
}

#[macro_export]
macro_rules! log_info {
    ($target:expr, $msg:expr $(,)?) => {
        $crate::logging::emit($crate::logging::LogLevel::Info, $target, $msg, None)
    };
    ($target:expr, $msg:expr, $fields:tt $(,)?) => {
        if $crate::logging::enabled($crate::logging::LogLevel::Info) {
            $crate::logging::emit(
                $crate::logging::LogLevel::Info,
                $target,
                $msg,
                Some(serde_json::json!($fields)),
            );
        }
    };
}

#[macro_export]
macro_rules! log_debug {
    ($target:expr, $msg:expr $(,)?) => {
        $crate::logging::emit($crate::logging::LogLevel::Debug, $target, $msg, None)
    };
    ($target:expr, $msg:expr, $fields:tt $(,)?) => {
        if $crate::logging::enabled($crate::logging::LogLevel::Debug) {
            $crate::logging::emit(
                $crate::logging::LogLevel::Debug,
                $target,
                $msg,
                Some(serde_json::json!($fields)),
            );
        }
    };
}

#[macro_export]
macro_rules! log_trace {
    ($target:expr, $msg:expr $(,)?) => {
        $crate::logging::emit($crate::logging::LogLevel::Trace, $target, $msg, None)
    };
    ($target:expr, $msg:expr, $fields:tt $(,)?) => {
        if $crate::logging::enabled($crate::logging::LogLevel::Trace) {
            $crate::logging::emit(
                $crate::logging::LogLevel::Trace,
                $target,
                $msg,
                Some(serde_json::json!($fields)),
            );
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_utc_formats_epoch_and_leap_dates_accurately() {
        assert_eq!(format_iso8601_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_iso8601_utc(1_709_210_096_789),
            "2024-02-29T12:34:56.789Z"
        );
    }

    #[test]
    fn parses_plain_and_rust_log_style_levels() {
        assert_eq!(LogLevel::parse("error").unwrap(), LogLevel::Error);
        assert_eq!(LogLevel::parse("WARN").unwrap(), LogLevel::Warn);
        assert_eq!(LogLevel::parse("info").unwrap(), LogLevel::Info);
        assert_eq!(LogLevel::parse("debug").unwrap(), LogLevel::Debug);
        assert_eq!(LogLevel::parse("trace").unwrap(), LogLevel::Trace);
        assert_eq!(
            LogLevel::parse("hyper=warn,dev_conductor=debug").unwrap(),
            LogLevel::Debug
        );
        assert!(LogLevel::parse("nonsense").is_err());
    }

    #[test]
    fn sanitize_query_redacts_secrets_and_preserves_safe_params() {
        assert_eq!(
            sanitize_query("refresh=true&token=supersecret&code=ABCD-1234&limit=50"),
            "refresh=true&token=[REDACTED]&code=[REDACTED]&limit=50"
        );
    }

    #[test]
    fn json_logger_filters_by_level_and_rotates_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let logger = JsonLogger::with_level(dir.path(), false, LogLevel::Warn);
        logger.debug("serve", "http.poll", None, "should be filtered", json!({}));
        logger.info("serve", "http.ok", None, "should be filtered", json!({}));
        logger.warn(
            "serve",
            "auth.denied",
            Some("req-1".into()),
            "401",
            json!({"path": "/api/tasks"}),
        );
        logger.error(
            "serve",
            "handler.err",
            Some("req-2".into()),
            "boom",
            json!({"status": 500}),
        );

        let all = logger.read_recent(20, None, None).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0]["level"], "warn");
        assert_eq!(all[1]["level"], "error");

        let only_err = logger
            .read_recent(20, Some("error"), Some("handler"))
            .unwrap();
        assert_eq!(only_err.len(), 1);
        assert_eq!(only_err[0]["request_id"], "req-2");

        // File rotation test
        let rot_path = dir.path().join("logs/rot.jsonl");
        let mut sink = FileSink::open(rot_path.clone(), 1024, 2).unwrap();
        for i in 0..30 {
            let line = format_structured_record(
                LogLevel::Info,
                "test",
                "rotation",
                None,
                "structured event",
                Some(json!({"seq": i, "payload": "x".repeat(48)})),
            );
            sink.write_line(&line);
        }
        assert!(rot_path.is_file());
        assert!(rotated_path(&rot_path, 1).is_file());
    }

    fn line(writer: &str, seq: usize) -> String {
        format_structured_record(
            LogLevel::Info,
            writer,
            "seq",
            None,
            "event",
            Some(json!({"seq": seq, "pad": "y".repeat(64)})),
        )
    }

    fn lines_in(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    #[test]
    fn two_sinks_on_one_path_keep_writing_to_the_live_file_across_rotations() {
        // Issue #4: `serve` had the global sink and a `JsonLogger` sink on the same path, each
        // with its own counter. After one rotated, the other kept appending to the renamed
        // file, so `/api/logs` (live file only) missed its newest lines.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs/shared.jsonl");
        let mut a = FileSink::open(path.clone(), 2048, 50).unwrap();
        let mut b = FileSink::open(path.clone(), 2048, 50).unwrap();
        let total = 120;
        for seq in 0..total {
            a.write_line(&line("a", seq));
            b.write_line(&line("b", seq));
        }
        let live = lines_in(&path);
        for writer in ["a", "b"] {
            assert!(
                live.iter()
                    .any(|e| e["component"] == writer && e["fields"]["seq"] == total - 1),
                "newest line from {writer} must be in the live file"
            );
        }
        // Nothing lost and nothing double-rotated: every line is in exactly one file.
        let mut count = live.len();
        for idx in 1..=50 {
            count += lines_in(&rotated_path(&path, idx)).len();
        }
        assert_eq!(count, total * 2);
        assert!(
            !FileSink::open(path.clone(), 2048, 50)
                .unwrap()
                .lock_path()
                .exists()
        );
    }

    #[test]
    fn a_failed_rotation_rename_never_truncates_the_live_log() {
        // Issue #6: a failed `rename(live, .1)` used to be followed by truncate(true).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs/blocked.jsonl");
        // A non-empty directory at `.1` makes both the remove and the rename fail on every
        // platform, standing in for a file held open without share-delete.
        let blocker = rotated_path(&path, 1);
        fs::create_dir_all(&blocker).unwrap();
        fs::write(blocker.join("keep"), "x").unwrap();
        let mut sink = FileSink::open(path.clone(), 1024, 1).unwrap();
        for seq in 0..40 {
            sink.write_line(&line("w", seq));
        }
        let live = lines_in(&path);
        assert_eq!(
            live.len(),
            40,
            "every line must survive the failed rotations"
        );
        assert_eq!(live[0]["fields"]["seq"], 0);
        assert!(!sink.lock_path().exists());
    }

    #[test]
    fn a_fresh_rotation_lock_defers_and_a_stale_one_is_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs/locked.jsonl");
        let mut sink = FileSink::open(path.clone(), 1024, 2).unwrap();
        let lock = sink.lock_path();

        // Another writer is mid-rotation: this one only appends.
        fs::write(&lock, "").unwrap();
        for seq in 0..30 {
            sink.write_line(&line("w", seq));
        }
        assert!(!rotated_path(&path, 1).exists());
        assert_eq!(lines_in(&path).len(), 30);

        // The lock was left by a crashed writer long ago: it is cleared and rotation runs.
        let old = SystemTime::now() - std::time::Duration::from_millis(ROTATION_LOCK_STALE_MS * 2);
        fs::File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(old)
            .unwrap();
        sink.write_line(&line("w", 30));
        assert!(rotated_path(&path, 1).is_file());
        assert!(!lock.exists());
    }
}

use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

static TRACE_PATH: OnceLock<Option<String>> = OnceLock::new();
static LAST_BY_KEY: OnceLock<Mutex<HashMap<&'static str, String>>> = OnceLock::new();

fn configured_path() -> Option<&'static str> {
    TRACE_PATH
        .get_or_init(|| {
            let value = env::var("LILTERM_TRACE").ok()?;
            let value = value.trim();
            if value.is_empty() || value == "0" || value.eq_ignore_ascii_case("false") {
                None
            } else if value == "1" || value.eq_ignore_ascii_case("true") {
                Some("/tmp/lilterm-trace.log".to_string())
            } else {
                Some(value.to_string())
            }
        })
        .as_deref()
}

pub fn enabled() -> bool {
    configured_path().is_some()
}

pub fn log(message: impl AsRef<str>) {
    log_args(format_args!("{}", message.as_ref()));
}

pub fn log_changed(key: &'static str, message: impl Into<String>) {
    if !enabled() {
        return;
    }

    let message = message.into();
    let cache = LAST_BY_KEY.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut cache) = cache.lock() else {
        log(format!("{key} {message}"));
        return;
    };

    if cache.get(key) == Some(&message) {
        return;
    }

    cache.insert(key, message.clone());
    drop(cache);
    log(format!("{key} {message}"));
}

pub(crate) fn log_changed_args(key: &'static str, args: fmt::Arguments<'_>) {
    if !enabled() {
        return;
    }
    log_changed(key, args.to_string());
}

pub(crate) fn log_args(args: fmt::Arguments<'_>) {
    let Some(path) = configured_path() else {
        return;
    };

    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };

    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();

    let _ = writeln!(file, "{millis} pid={} {args}", std::process::id());
}

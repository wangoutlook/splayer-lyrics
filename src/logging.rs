use gtk4::glib;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();
static DEBUG: AtomicBool = AtomicBool::new(false);

pub fn init(path: &Path, debug: bool) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let _ = LOG_FILE.set(Mutex::new(file));
    DEBUG.store(debug, Ordering::Relaxed);
    Ok(())
}

fn timestamp() -> String {
    match glib::DateTime::now_local() {
        Ok(dt) => dt
            .format("%Y-%m-%d %H:%M:%S")
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "unknown-time".to_string()),
        Err(_) => "unknown-time".to_string(),
    }
}

pub fn write(level: &str, msg: &str) {
    let line = format!("[{}] [{}] {}\n", timestamp(), level, msg);
    if let Some(file) = LOG_FILE.get() {
        if let Ok(mut f) = file.lock() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }
    if DEBUG.load(Ordering::Relaxed) {
        eprint!("{line}");
    }
}

pub fn info(msg: &str) {
    write("INFO", msg);
}

pub fn warn(msg: &str) {
    write("WARN", msg);
}

pub fn error(msg: &str) {
    write("ERROR", msg);
}

pub fn debug(msg: &str) {
    if DEBUG.load(Ordering::Relaxed) {
        write("DEBUG", msg);
    }
}

use std::{
    fs,
    io::{self, Write},
    panic,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::store::{ensure_private_dir, make_private_file};

const LOG_FILE: &str = "agent-console.log";
const MAX_LOG_BYTES: u64 = 256 * 1024;
const LOG_GENERATIONS: usize = 3;

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static LOG_LOCK: Mutex<()> = Mutex::new(());

pub fn init(state_dir: &Path) -> io::Result<PathBuf> {
    ensure_private_dir(state_dir)?;
    let path = state_dir.join(LOG_FILE);
    let _ = LOG_PATH.set(path.clone());
    append_to(&path, "process started")?;
    Ok(path)
}

pub fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        record(&format!("panic: {info}"));
        previous(info);
    }));
}

pub fn record(message: &str) {
    if let Some(path) = LOG_PATH.get() {
        let _ = append_to(path, message);
    }
}

pub fn path() -> Option<&'static Path> {
    LOG_PATH.get().map(PathBuf::as_path)
}

fn append_to(path: &Path, message: &str) -> io::Result<()> {
    let _lock = LOG_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let line = format!("{timestamp} {message}\n");
    if fs::metadata(path).is_ok_and(|metadata| metadata.len() + line.len() as u64 > MAX_LOG_BYTES) {
        rotate(path)?;
    }
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    make_private_file(path)?;
    file.write_all(line.as_bytes())
}

fn rotate(path: &Path) -> io::Result<()> {
    let oldest = generation_path(path, LOG_GENERATIONS);
    if oldest.exists() {
        fs::remove_file(oldest)?;
    }
    for generation in (1..LOG_GENERATIONS).rev() {
        let source = generation_path(path, generation);
        if source.exists() {
            fs::rename(source, generation_path(path, generation + 1))?;
        }
    }
    if path.exists() {
        fs::rename(path, generation_path(path, 1))?;
    }
    Ok(())
}

fn generation_path(path: &Path, generation: usize) -> PathBuf {
    path.with_file_name(format!("{LOG_FILE}.{generation}"))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn diagnostics_rotate_and_remain_private() {
        let root = tempdir().unwrap();
        let path = root.path().join(LOG_FILE);
        fs::write(&path, vec![b'x'; MAX_LOG_BYTES as usize]).unwrap();
        append_to(&path, "next process").unwrap();

        assert!(generation_path(&path, 1).exists());
        assert!(fs::read_to_string(&path).unwrap().contains("next process"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

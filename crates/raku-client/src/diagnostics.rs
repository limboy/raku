//! Small, bounded transport diagnostics, retained even in GUI release builds
//! where stderr is /dev/null. Call only from the transport/supervisor threads.

use std::fmt::Display;
use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

const MAX_LOG_BYTES: u64 = 256 * 1024;
static LOG_WRITE: Mutex<()> = Mutex::new(());

pub(crate) fn log_daemon_event(message: impl Display) {
    eprintln!("{message}");
    let _write = LOG_WRITE.lock();
    let path =
        crate::persistence::StateStore::default_path().with_file_name("daemon-connection.log");
    let _ = append_record(&path, message);
}

fn append_record(path: &Path, message: impl Display) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() >= MAX_LOG_BYTES) {
        let previous = path.with_extension("log.1");
        let _ = std::fs::remove_file(&previous);
        std::fs::rename(path, previous)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    writeln!(
        file,
        "{}.{:03} pid={} {message}",
        elapsed.as_secs(),
        elapsed.subsec_millis(),
        std::process::id(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_diagnostics_rotate_instead_of_growing_without_bound() {
        let directory =
            std::env::temp_dir().join(format!("raku-diagnostics-{}", uuid::Uuid::new_v4()));
        let path = directory.join("daemon-connection.log");
        append_record(&path, "read failed: connection reset").unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        assert!(first.contains("read failed: connection reset"));
        std::fs::write(&path, vec![b'x'; MAX_LOG_BYTES as usize]).unwrap();
        append_record(&path, "reconnected to live daemon").unwrap();
        assert_eq!(
            std::fs::metadata(path.with_extension("log.1"))
                .unwrap()
                .len(),
            MAX_LOG_BYTES
        );
        let current = std::fs::read_to_string(&path).unwrap();
        assert!(current.contains("reconnected to live daemon"));
        assert!(current.len() < 256);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}

//! The single owner-only atomic state-file writer: unique create-new temporary name in the
//! same directory, mode 0600, data fsync, rename over the target, then directory fsync.
//! Concurrent writers (for example during a session swap) never share a temporary file, and
//! readers only ever see a complete previous or new file.
use std::{
    fs,
    io::{self, Write},
    path::Path,
};

pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("state file has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("state file has no name"))?
        .to_string_lossy();
    let temp = parent.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_writers_never_interleave_or_leave_temporaries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::thread::scope(|scope| {
            for writer in 0..8u8 {
                let path = path.clone();
                scope.spawn(move || {
                    for round in 0..25u8 {
                        let body =
                            serde_json::to_vec(&vec![writer; 4096 + round as usize]).unwrap();
                        write_private_atomic(&path, &body).unwrap();
                    }
                });
            }
        });
        let parsed: Vec<u8> = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(
            parsed.iter().all(|byte| *byte == parsed[0]),
            "content is one writer's complete payload"
        );
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1, "no temporary files remain: {entries:?}");
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

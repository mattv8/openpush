//! The single owner-only atomic state-file writer: unique create-new temporary name in the
//! same directory, mode 0600, data fsync, rename over the target, then directory fsync.
//! Concurrent writers (for example during a session swap) never share a temporary file, and
//! readers only ever see a complete previous or new file.
use std::{
    fs,
    io::{self, Write},
    path::Path,
};

fn byte_bounded_prefix(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("state file has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("state file has no name"))?
        .to_string_lossy();
    let short_name = byte_bounded_prefix(&name, 180);
    let temp = parent.join(format!(
        ".{short_name}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
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

fn copy_fallback_new_with<F>(
    source: &Path,
    destination: &Path,
    expected_bytes: u64,
    copy: F,
) -> io::Result<()>
where
    F: FnOnce(&mut fs::File, &mut fs::File) -> io::Result<u64>,
{
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let result = (|| {
        let mut input = fs::File::open(source)?;
        if copy(&mut input, &mut output)? != expected_bytes {
            return Err(io::Error::other("plaintext size changed during save"));
        }
        output.sync_all()
    })();
    drop(output);
    if result.is_err() {
        // `create_new` proves this path was created by this attempt. Never remove a pre-existing
        // destination when opening it failed.
        let _ = fs::remove_file(destination);
    }
    result
}

/// Copies a verified, native-only plaintext file without allowing an existing destination to be
/// replaced. `hard_link` publishes the completed temporary file atomically on supported desktop
/// filesystems; the caller's selected filename is never accepted from webview input.
pub fn copy_new_atomic(source: &Path, destination: &Path, expected_bytes: u64) -> io::Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("save path has no parent"))?;
    let name = destination
        .file_name()
        .ok_or_else(|| io::Error::other("save path has no name"))?
        .to_string_lossy();
    if destination.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "destination already exists",
        ));
    }
    let short_name = byte_bounded_prefix(&name, 180);
    let temp = parent.join(format!(
        ".{short_name}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| {
        let mut input = fs::File::open(source)?;
        let metadata = input.metadata()?;
        if !metadata.is_file() || metadata.len() != expected_bytes {
            return Err(io::Error::other("verified plaintext changed before save"));
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(&temp)?;
        let copied = io::copy(&mut input, &mut output)?;
        if copied != expected_bytes {
            return Err(io::Error::other("plaintext size changed during save"));
        }
        output.sync_all()?;
        drop(output);
        match fs::hard_link(&temp, destination) {
            Ok(()) => {
                // The destination is complete even if best-effort temporary cleanup fails.
                let _ = fs::remove_file(&temp);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Unsupported | io::ErrorKind::PermissionDenied
                ) =>
            {
                // FAT/exFAT and some remote volumes do not support hard links. A direct
                // create-new copy is not atomic, but preserves the no-overwrite guarantee.
                copy_fallback_new_with(&temp, destination, expected_bytes, io::copy)?;
                let _ = fs::remove_file(&temp);
            }
            Err(error) => return Err(error),
        }
        #[cfg(unix)]
        {
            fs::File::open(parent)?.sync_all()?;
        }
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

    #[test]
    fn byte_bounded_temporary_prefix_accepts_long_multibyte_destination_name() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fs::write(&source, b"verified").unwrap();
        let long_name = format!("{}-saved", "界".repeat(73));
        let destination = dir.path().join(long_name);
        copy_new_atomic(&source, &destination, 8).unwrap();
        assert_eq!(fs::read(destination).unwrap(), b"verified");
    }

    #[test]
    fn fallback_copy_error_removes_only_destination_created_by_this_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let temp = dir.path().join("complete-temp");
        let destination = dir.path().join("selected-destination");
        let unrelated = dir.path().join("unrelated");
        fs::write(&temp, b"verified").unwrap();
        fs::write(&unrelated, b"keep").unwrap();
        let error = copy_fallback_new_with(&temp, &destination, 8, |_input, output| {
            output.write_all(b"partial")?;
            Err(io::Error::other("injected copy failure"))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(!destination.exists());
        assert_eq!(fs::read(unrelated).unwrap(), b"keep");
    }

    #[test]
    fn copy_new_atomic_never_overwrites_or_accepts_wrong_size() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::write(&source, b"verified").unwrap();
        copy_new_atomic(&source, &target, 8).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"verified");
        assert_eq!(
            copy_new_atomic(&source, &target, 8).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(copy_new_atomic(&source, &dir.path().join("wrong"), 7).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }
}

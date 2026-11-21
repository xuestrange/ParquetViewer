//! Session-wide source protection shared by every output path.
//! Viewing, importing and exporting never open source data for writing.
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Default)]
struct Sources {
    paths: Vec<PathBuf>,
    identities: Vec<(u64, u64)>,
}
static SOURCES: OnceLock<Mutex<Sources>> = OnceLock::new();
static NEXT_COPY: AtomicU64 = AtomicU64::new(0);

fn absolute(path: &Path) -> io::Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            part => normalized.push(part.as_os_str()),
        }
    }
    Ok(normalized)
}

fn resolved(path: &Path) -> io::Result<PathBuf> {
    if let Ok(path) = path.canonicalize() {
        return Ok(path);
    }
    let path = absolute(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Destination needs a parent folder"))?;
    Ok(parent.canonicalize()?.join(
        path.file_name()
            .ok_or_else(|| io::Error::other("Destination needs a file name"))?,
    ))
}

/// Retain paths and inode identities for the whole session, including sources
/// whose viewer has been closed or replaced while an older preview stays open.
pub fn register_sources(paths: &[PathBuf]) -> io::Result<()> {
    let mut sources = SOURCES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for path in paths {
        for path in [absolute(path)?, resolved(path)?] {
            if !sources.paths.contains(&path) {
                sources.paths.push(path);
            }
        }
        if let Ok(meta) = fs::metadata(path) {
            let id = (meta.dev(), meta.ino());
            if !sources.identities.contains(&id) {
                sources.identities.push(id);
            }
        }
    }
    Ok(())
}

pub fn guard_destination(target: &Path, protected: &[PathBuf]) -> io::Result<()> {
    register_sources(protected)?;
    let absolute = absolute(target)?;
    let resolved = resolved(target)?;
    let identity = fs::metadata(target).ok().map(|m| (m.dev(), m.ino()));
    let sources = SOURCES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if sources.paths.contains(&absolute) || sources.paths.contains(&resolved)
        || identity.is_some_and(|id| sources.identities.contains(&id))
        // Recheck live source identities as a source may have been replaced
        // externally since registration.
        || sources.paths.iter().any(|source| fs::metadata(source).ok().is_some_and(|m| Some((m.dev(), m.ino())) == identity && identity.is_some()))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Source data is read-only. Choose a different destination; source files and their aliases cannot be overwritten.",
        ));
    }
    Ok(())
}

/// Write a derived copy atomically; never truncate an existing source or follow
/// a pre-existing temporary symlink. Recheck protection just before publishing.
pub fn save_copy(target: &Path, protected: &[PathBuf], bytes: &[u8]) -> io::Result<()> {
    guard_destination(target, protected)?;
    let target = absolute(target)?;
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("Destination needs a parent folder"))?;
    let nonce = NEXT_COPY.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".parquetviewer-copy-{}-{nonce}.tmp",
        std::process::id()
    ));
    guard_destination(&temporary, &[])?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        guard_destination(&target, protected)?;
        fs::rename(&temporary, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Source history and the temporary nonce are process-wide. A fresh test
    /// process keeps parallel engine/Peek tests from changing either while a
    /// regression deliberately creates a colliding temporary file.
    fn isolated(name: &str, scenario: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
        let module = module_path!()
            .split_once("::")
            .map_or("", |(_, module)| module);
        let test_name = format!("{module}::{name}");
        if std::env::var("PARQUET_VIEWER_GUARD_TEST").as_deref() == Ok(test_name.as_str()) {
            return scenario();
        }
        let output = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", &test_name, "--nocapture"])
            .env("PARQUET_VIEWER_GUARD_TEST", &test_name)
            .output()?;
        assert!(
            output.status.success(),
            "isolated source protection test failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        // A typo in the subprocess filter must not silently count as a pass.
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "isolated test was not executed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        Ok(())
    }

    fn denied(target: &Path, protected: &[PathBuf]) {
        assert_eq!(
            save_copy(target, protected, b"changed").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    fn next_temporary(parent: &Path) -> PathBuf {
        parent.join(format!(
            ".parquetviewer-copy-{}-{}.tmp",
            std::process::id(),
            NEXT_COPY.load(Ordering::Relaxed)
        ))
    }

    #[test]
    fn all_outputs_protect_sources_aliases_and_closed_sources() -> io::Result<()> {
        isolated(
            "all_outputs_protect_sources_aliases_and_closed_sources",
            || {
                let dir = tempfile::tempdir()?;
                let source = dir.path().join("source.data");
                fs::write(&source, b"original\0data")?;
                register_sources(std::slice::from_ref(&source))?;
                let hard = dir.path().join("hard.csv");
                fs::hard_link(&source, &hard)?;
                let symbolic = dir.path().join("symbolic.txt");
                std::os::unix::fs::symlink(&source, &symbolic)?;
                for target in [&source, &hard, &symbolic] {
                    denied(target, &[]);
                }
                assert_eq!(fs::read(&source)?, b"original\0data");
                // A closed/deleted source path remains reserved for this session.
                fs::remove_file(&source)?;
                denied(&source, &[]);
                let output = dir.path().join("copy.txt");
                save_copy(&output, &[], b"derived")?;
                assert_eq!(fs::read(output)?, b"derived");
                Ok(())
            },
        )
    }

    #[test]
    fn old_peek_and_new_source_cannot_overwrite_either_sessions_input() -> io::Result<()> {
        isolated(
            "old_peek_and_new_source_cannot_overwrite_either_sessions_input",
            || {
                let dir = tempfile::tempdir()?;
                let old = dir.path().join("old.parquet");
                let new = dir.path().join("new.csv");
                fs::write(&old, b"old source")?;
                fs::write(&new, b"new source")?;
                register_sources(std::slice::from_ref(&old))?;
                let old_peek_sources = vec![old.clone()];
                register_sources(std::slice::from_ref(&new))?;
                denied(&old, &[]);
                denied(&new, &old_peek_sources);
                let old_alias = dir.path().join("old-alias.bin");
                fs::hard_link(&old, &old_alias)?;
                denied(&old_alias, &[]);
                let copy = dir.path().join("derived.bin");
                save_copy(&copy, &old_peek_sources, b"derived")?;
                assert_eq!(fs::read(copy)?, b"derived");
                assert_eq!(fs::read(old)?, b"old source");
                assert_eq!(fs::read(new)?, b"new source");
                Ok(())
            },
        )
    }

    #[test]
    fn source_at_preferences_path_blocks_settings_save() -> io::Result<()> {
        isolated("source_at_preferences_path_blocks_settings_save", || {
            let dir = tempfile::tempdir()?;
            let folder = dir.path().join("Library/Application Support/ParquetViewer");
            fs::create_dir_all(&folder)?;
            let source = folder.join("preferences.json");
            fs::write(&source, br#"{"user_data":42}"#)?;
            register_sources(std::slice::from_ref(&source))?;
            denied(&source, &[]);
            let alias = dir.path().join("preferences-alias.json");
            std::os::unix::fs::symlink(&source, &alias)?;
            denied(&alias, &[]);
            assert_eq!(fs::read(source)?, br#"{"user_data":42}"#);
            Ok(())
        })
    }

    #[test]
    fn parent_symlink_and_dot_segments_cannot_bypass_protection() -> io::Result<()> {
        isolated(
            "parent_symlink_and_dot_segments_cannot_bypass_protection",
            || {
                let dir = tempfile::tempdir()?;
                let real = dir.path().join("real");
                fs::create_dir(&real)?;
                let source = real.join("source.csv");
                fs::write(&source, b"source")?;
                register_sources(std::slice::from_ref(&source))?;
                let alias = dir.path().join("parent");
                std::os::unix::fs::symlink(&real, &alias)?;
                denied(&alias.join("source.csv"), &[]);
                fs::create_dir(real.join("child"))?;
                denied(&real.join("child/.././source.csv"), &[]);
                assert_eq!(fs::read(source)?, b"source");
                Ok(())
            },
        )
    }

    #[test]
    fn registering_alias_protects_real_path_and_later_hardlink() -> io::Result<()> {
        isolated(
            "registering_alias_protects_real_path_and_later_hardlink",
            || {
                let dir = tempfile::tempdir()?;
                let source = dir.path().join("source.csv");
                fs::write(&source, b"source")?;
                let alias = dir.path().join("source-link");
                std::os::unix::fs::symlink(&source, &alias)?;
                register_sources(&[alias])?;
                let hard = dir.path().join("later-hardlink");
                fs::hard_link(&source, &hard)?;
                denied(&source, &[]);
                denied(&hard, &[]);
                // Protection must still permit replacing an unrelated derived copy.
                let copy = dir.path().join("copy");
                save_copy(&copy, &[], b"copy")?;
                save_copy(&copy, &[], b"new copy")?;
                assert_eq!(fs::read(copy)?, b"new copy");
                assert_eq!(fs::read(source)?, b"source");
                Ok(())
            },
        )
    }

    #[test]
    fn existing_temporary_symlink_is_not_followed_or_deleted() -> io::Result<()> {
        isolated(
            "existing_temporary_symlink_is_not_followed_or_deleted",
            || {
                let dir = tempfile::tempdir()?;
                let source = dir.path().join("source");
                fs::write(&source, b"original")?;
                let target = dir.path().join("output");
                fs::write(&target, b"old output")?;
                let temporary = next_temporary(dir.path());
                std::os::unix::fs::symlink(&source, &temporary)?;
                assert_eq!(
                    save_copy(&target, &[], b"new output").unwrap_err().kind(),
                    io::ErrorKind::AlreadyExists
                );
                assert!(fs::symlink_metadata(&temporary)?.file_type().is_symlink());
                assert_eq!(fs::read(source)?, b"original");
                assert_eq!(fs::read(target)?, b"old output");
                Ok(())
            },
        )
    }

    #[test]
    fn existing_temporary_file_and_temporary_source_are_untouched() -> io::Result<()> {
        isolated(
            "existing_temporary_file_and_temporary_source_are_untouched",
            || {
                let dir = tempfile::tempdir()?;
                let target = dir.path().join("output");
                fs::write(&target, b"old output")?;
                let temporary = next_temporary(dir.path());
                fs::write(&temporary, b"keep temporary")?;
                assert_eq!(
                    save_copy(&target, &[], b"new output").unwrap_err().kind(),
                    io::ErrorKind::AlreadyExists
                );
                assert_eq!(fs::read(temporary)?, b"keep temporary");
                let temporary = next_temporary(dir.path());
                fs::write(&temporary, b"source at temporary")?;
                register_sources(std::slice::from_ref(&temporary))?;
                denied(&target, &[]);
                assert_eq!(fs::read(temporary)?, b"source at temporary");
                assert_eq!(fs::read(target)?, b"old output");
                Ok(())
            },
        )
    }

    #[test]
    fn failed_publish_cleans_only_its_temporary_and_preserves_destination() -> io::Result<()> {
        isolated(
            "failed_publish_cleans_only_its_temporary_and_preserves_destination",
            || {
                let dir = tempfile::tempdir()?;
                let target = dir.path().join("directory");
                fs::create_dir(&target)?;
                let source = target.join("source");
                fs::write(&source, b"original")?;
                register_sources(std::slice::from_ref(&source))?;
                let temporary = next_temporary(dir.path());
                assert!(save_copy(&target, &[], b"new output").is_err());
                assert!(!temporary.exists());
                assert!(target.is_dir());
                assert_eq!(fs::read(source)?, b"original");
                Ok(())
            },
        )
    }
}

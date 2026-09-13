//! Replacement primitive for saved views and templates only.
//!
//! Each writer owns a private temporary in the destination directory. A successful
//! return means file data was synced before rename and the directory was synced
//! afterwards. A directory-sync error is reported after replacement: the new file
//! is already visible, but crash durability is unconfirmed. Concurrent replacements
//! are last-rename-wins; this does not merge read-modify-write updates.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

struct Temporary(PathBuf);

impl Drop for Temporary {
    fn drop(&mut self) {
        // On success the name no longer exists. On failure (including unwind),
        // remove only this writer's temporary, never another writer's file.
        let _ = fs::remove_file(&self.0);
    }
}

pub(crate) fn replace(path: &Path, content: &str) -> io::Result<()> {
    replace_with(path, |file| file.write_all(content.as_bytes()))
}

fn replace_with(path: &Path, write: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // Open before replacement so directory-open errors cannot publish new data.
    let directory = File::open(parent)?;
    let mut opened = None;
    for _ in 0..32 {
        let mut random = [0u8; 16];
        getrandom::getrandom(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
        let nonce = u128::from_ne_bytes(random);
        let temporary = parent.join(format!(".taarof-save-{nonce:032x}.tmp"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
        {
            Ok(file) => {
                opened = Some((Temporary(temporary), file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    let (temporary, mut file) = opened.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "private save temporary collision",
        )
    })?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    write(&mut file)?;
    file.sync_all()?;
    fs::rename(&temporary.0, path)?;
    directory.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Barrier,
    };

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let mut random = [0u8; 16];
            getrandom::getrandom(&mut random).unwrap();
            let path = std::env::temp_dir().join(format!(
                "taarof-private-save-{:032x}",
                u128::from_ne_bytes(random)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn destination(&self) -> PathBuf {
            self.0.join("store.json")
        }
        fn entries(&self) -> usize {
            fs::read_dir(&self.0).unwrap().count()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn private_atomic_file_concurrent_writers_keep_readers_on_complete_json() {
        let fixture = Fixture::new();
        let path = fixture.destination();
        replace(&path, r#"{"writer":0,"payload":"old"}"#).unwrap();
        let barrier = Arc::new(Barrier::new(5));
        let stop = Arc::new(AtomicBool::new(false));
        let reader_ready = Barrier::new(2);
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap();
                reader_ready.wait();
                let mut reads = 1;
                while !stop.load(Ordering::Acquire) {
                    let contents = fs::read(&path).unwrap();
                    let value: serde_json::Value =
                        serde_json::from_slice(&contents).expect("reader saw partial JSON");
                    assert!(value["writer"].as_u64().unwrap() <= 4);
                    reads += 1;
                }
                reads
            });
            reader_ready.wait();
            let mut writers = Vec::new();
            for n in 1..=4 {
                let path = &path;
                let barrier = Arc::clone(&barrier);
                writers.push(scope.spawn(move || {
                    let content =
                        serde_json::json!({"writer": n, "payload": "inert".repeat(16000)})
                            .to_string();
                    replace_with(path, |file| {
                        let mid = content.len() / 2;
                        file.write_all(&content.as_bytes()[..mid])?;
                        barrier.wait();
                        barrier.wait();
                        file.write_all(&content.as_bytes()[mid..])
                    })
                    .unwrap();
                }));
            }
            barrier.wait();
            assert_eq!(fixture.entries(), 5, "every writer needs its own temporary");
            for entry in fs::read_dir(&fixture.0).unwrap() {
                assert_eq!(
                    entry.unwrap().metadata().unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap()
                    ["writer"],
                0
            );
            barrier.wait();
            for writer in writers {
                writer.join().unwrap();
            }
            stop.store(true, Ordering::Release);
            assert!(reader.join().unwrap() > 0);
        });
        assert_eq!(fixture.entries(), 1);
        // A later complete replacement wins; no implicit field/update merging.
        replace(&path, r#"{"last":true}"#).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"last":true}"#);
    }

    #[test]
    fn private_atomic_file_failure_and_unwind_preserve_old_destination_and_cleanup() {
        let fixture = Fixture::new();
        let path = fixture.destination();
        replace(&path, "old").unwrap();
        let error = replace_with(&path, |file| {
            file.write_all(b"partial")?;
            Err(io::Error::other("inert injected write failure"))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
        assert_eq!(fixture.entries(), 1);
        assert!(
            std::panic::catch_unwind(|| replace_with(&path, |_| panic!("inert unwind"))).is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
        assert_eq!(fixture.entries(), 1);
        let directory_target = fixture.0.join("directory");
        fs::create_dir(&directory_target).unwrap();
        assert!(replace(&directory_target, "new").is_err());
        assert!(directory_target.is_dir());
        assert_eq!(fixture.entries(), 2);
    }
}

//! Durable identity for a herdr server instance.
//!
//! Workspace ids, terminal ids, and pane ids are only unique inside one server
//! process. Federating two servers therefore needs a stable name to namespace
//! the other side's ids by. The instance id is that name: it is generated once
//! per session and stored next to `herdr.sock` and `session.json` in the
//! session data dir, so it survives server restarts within a session.
//!
//! The id is 128 bits rendered as lowercase hex. Lowercase hex never collides
//! with a local workspace id (`w` + uppercase base32), so a prefixed id stays
//! unambiguous without a separator escape.

use std::io::{self, Write};
use std::path::Path;
use std::sync::OnceLock;

use sha2::{Digest, Sha256};
use tracing::warn;

const INSTANCE_ID_FILE: &str = "instance-id";
const INSTANCE_ID_LOCK_FILE: &str = ".instance-id.lock";
const INSTANCE_ID_BYTES: usize = 16;
const INSTANCE_ID_LEN: usize = INSTANCE_ID_BYTES * 2;

static ACTIVE: OnceLock<Option<String>> = OnceLock::new();

/// Instance id for the active session, generating and persisting one on first
/// use.
///
/// Returns `None` when the id can be neither read nor written. That means this
/// server cannot be federated, not that it cannot run, so callers degrade
/// instead of failing.
pub fn active() -> Option<String> {
    ACTIVE
        .get_or_init(|| {
            let dir = crate::session::data_dir();
            match load_or_create_in(&dir) {
                Ok(id) => Some(id),
                Err(err) => {
                    warn!(
                        dir = %dir.display(),
                        error = %err,
                        "failed to establish server instance id"
                    );
                    None
                }
            }
        })
        .clone()
}

pub(crate) fn load_or_create_in(dir: &Path) -> io::Result<String> {
    let path = dir.join(INSTANCE_ID_FILE);
    if let Some(id) = read_valid(&path) {
        return Ok(id);
    }

    std::fs::create_dir_all(dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(INSTANCE_ID_LOCK_FILE))?;
    lock.lock()?;
    if let Some(id) = read_valid(&path) {
        return Ok(id);
    }

    let candidate = generate();
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(candidate.as_bytes())?;
            file.write_all(b"\n")?;
            file.flush()?;
            Ok(candidate)
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => match read_valid(&path) {
            // Another process created the file between the read and the
            // create; its id wins so both processes agree.
            Some(id) => Ok(id),
            // The file exists but holds something unusable. Replace it.
            None => {
                replace(&path, &candidate)?;
                Ok(candidate)
            }
        },
        Err(err) => Err(err),
    }
}

fn replace(path: &Path, id: &str) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{id}\n"))?;
    #[cfg(windows)]
    if path.exists() {
        if let Err(err) = std::fs::remove_file(path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(err);
        }
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

fn read_valid(path: &Path) -> Option<String> {
    parse(&std::fs::read_to_string(path).ok()?)
}

/// Whether `value` has the shape of an instance id.
///
/// Used at the peer id boundary to tell a namespaced id apart from a local one
/// without consulting the peer registry.
pub(crate) fn is_instance_id(value: &str) -> bool {
    value.len() == INSTANCE_ID_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn parse(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    is_instance_id(trimmed).then(|| trimmed.to_string())
}

fn generate() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut digest = Sha256::new();
    digest.update(b"herdr:instance-id:v1");

    // `RandomState` seeds its keys from the OS random source, which is the only
    // entropy std exposes. Hashing through it keeps the id unpredictable
    // without adding a `rand` or `uuid` dependency.
    for salt in 0..4u64 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(salt);
        digest.update(hasher.finish().to_le_bytes());
    }

    digest.update(std::process::id().to_le_bytes());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    digest.update(nanos.to_le_bytes());

    let bytes = digest.finalize();
    let mut id = String::with_capacity(INSTANCE_ID_LEN);
    for byte in bytes.iter().take(INSTANCE_ID_BYTES) {
        id.push(HEX[(byte >> 4) as usize] as char);
        id.push(HEX[(byte & 0x0f) as usize] as char);
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_path(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("herdr-{name}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn generated_ids_are_lowercase_hex_and_distinct() {
        let first = generate();
        let second = generate();
        assert_eq!(first.len(), INSTANCE_ID_LEN);
        assert_eq!(parse(&first).as_deref(), Some(first.as_str()));
        assert_ne!(first, second);
    }

    #[test]
    fn generated_ids_never_look_like_workspace_ids() {
        // Step 4 prefixes remote ids with the peer instance id. Local workspace
        // ids are `w` + uppercase base32, so the two alphabets must not overlap.
        let id = generate();
        assert!(!id.starts_with('w'));
        assert!(crate::workspace::public_workspace_number(&id).is_none());
    }

    #[test]
    fn load_or_create_persists_and_reuses_the_same_id() {
        let dir = unique_temp_path("instance-id-reuse");
        let first = load_or_create_in(&dir).expect("create instance id");
        let second = load_or_create_in(&dir).expect("reuse instance id");
        assert_eq!(first, second);

        let stored = std::fs::read_to_string(dir.join(INSTANCE_ID_FILE)).expect("read file");
        assert_eq!(stored.trim(), first);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_or_create_replaces_unusable_contents() {
        let dir = unique_temp_path("instance-id-replace");
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join(INSTANCE_ID_FILE);
        std::fs::write(&path, "not-an-instance-id\n").expect("write junk");

        let id = load_or_create_in(&dir).expect("replace instance id");
        assert_eq!(parse(&id).as_deref(), Some(id.as_str()));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read file").trim(),
            id
        );
        assert!(!dir.join("instance-id.tmp").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_repair_returns_the_single_persisted_id() {
        let dir = unique_temp_path("instance-id-concurrent-repair");
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::write(dir.join(INSTANCE_ID_FILE), "invalid\n").expect("write junk");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let workers = (0..2)
            .map(|_| {
                let dir = dir.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    load_or_create_in(&dir).expect("repair instance id")
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let ids = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        let stored = std::fs::read_to_string(dir.join(INSTANCE_ID_FILE)).unwrap();

        assert_eq!(ids[0], ids[1]);
        assert_eq!(ids[0], stored.trim());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_rejects_malformed_ids() {
        assert!(parse("").is_none());
        assert!(parse("abc").is_none());
        assert!(parse(&"A".repeat(INSTANCE_ID_LEN)).is_none());
        assert!(parse(&"0".repeat(INSTANCE_ID_LEN + 1)).is_none());
        assert!(parse(&"g".repeat(INSTANCE_ID_LEN)).is_none());
        assert_eq!(
            parse(&format!("  {}\n", "0".repeat(INSTANCE_ID_LEN))).as_deref(),
            Some("0".repeat(INSTANCE_ID_LEN).as_str())
        );
    }
}

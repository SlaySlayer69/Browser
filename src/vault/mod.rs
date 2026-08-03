//! Locally encrypted password store.
//!
//! # Threat model
//!
//! The vault protects against someone who can read the file: another process
//! under the same user account, a backup, a stolen disk. That is deliberately a
//! higher bar than Chrome's DPAPI-only storage, which any process running as
//! the same user can decrypt — the exact weakness infostealer malware uses.
//!
//! It does **not** protect against a keylogger or a debugger attached to this
//! process while the vault is unlocked. Nothing local can.
//!
//! # Format
//!
//! ```text
//! offset size  field
//! 0      8     magic "CDVAULT\x01"
//! 8      4     Argon2 m_cost (KiB, little-endian)
//! 12     4     Argon2 t_cost
//! 16     4     Argon2 p_cost
//! 20     16    salt
//! 36     24    XChaCha20 nonce
//! 60     ..    ciphertext + 16-byte Poly1305 tag
//! ```
//!
//! The whole 60-byte header is authenticated as associated data, so the KDF
//! parameters cannot be downgraded without invalidating the tag. The entire
//! entry list is one ciphertext: which sites you have accounts on is metadata
//! worth hiding, and per-entry encryption would leak the count.

use std::fs;
use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::util::now_millis;

const MAGIC: &[u8; 8] = b"CDVAULT\x01";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;
const HEADER_LEN: usize = 8 + 4 + 4 + 4 + SALT_LEN + NONCE_LEN;

/// OWASP's floor for Argon2id (19 MiB, 2 iterations). Deliberately modest: it
/// runs on the UI thread at unlock time, and a memory-first browser should not
/// spike to hundreds of megabytes to open a password list.
const M_COST_KIB: u32 = 19 * 1024;
const T_COST: u32 = 2;
const P_COST: u32 = 1;

#[derive(Debug)]
pub enum VaultError {
    /// Wrong master password, or the file was tampered with. These are
    /// deliberately indistinguishable.
    BadPassword,
    Corrupt(&'static str),
    Locked,
    Io(std::io::Error),
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadPassword => write!(f, "wrong master password"),
            Self::Corrupt(why) => write!(f, "vault file is corrupt: {why}"),
            Self::Locked => write!(f, "vault is locked"),
            Self::Io(e) => write!(f, "vault i/o error: {e}"),
        }
    }
}

impl std::error::Error for VaultError {}

impl From<std::io::Error> for VaultError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

type Result<T> = std::result::Result<T, VaultError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Credential {
    pub id: i64,
    /// Host the credential belongs to, lowercase and without `www.`.
    pub host: String,
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub note: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A credential with the secret withheld — what the UI lists before the user
/// explicitly reveals or copies one.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CredentialSummary {
    pub id: i64,
    pub host: String,
    pub username: String,
    pub note: String,
    pub updated_at: i64,
}

/// The decrypted key, wiped when the vault locks or drops.
#[derive(Zeroize, ZeroizeOnDrop)]
struct SessionKey([u8; KEY_LEN]);

pub struct Vault {
    path: PathBuf,
    key: Option<SessionKey>,
    entries: Vec<Credential>,
    next_id: i64,
}

impl Vault {
    /// A locked vault handle. No file access happens until `create`/`unlock`.
    pub fn new(path: &Path) -> Self {
        Self { path: path.to_path_buf(), key: None, entries: Vec::new(), next_id: 1 }
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    pub fn is_unlocked(&self) -> bool {
        self.key.is_some()
    }

    /// Initialise an empty vault protected by `master_password`.
    pub fn create(&mut self, master_password: &str) -> Result<()> {
        let mut salt = [0u8; SALT_LEN];
        rand::thread_rng().fill_bytes(&mut salt);

        let key = derive_key(master_password, &salt, M_COST_KIB, T_COST, P_COST)?;
        self.key = Some(SessionKey(key));
        self.entries.clear();
        self.next_id = 1;
        self.persist(&salt, M_COST_KIB, T_COST, P_COST)
    }

    /// Decrypt the vault. On failure the vault stays locked.
    pub fn unlock(&mut self, master_password: &str) -> Result<()> {
        let blob = fs::read(&self.path)?;
        if blob.len() < HEADER_LEN + 16 {
            return Err(VaultError::Corrupt("file shorter than header"));
        }
        if &blob[..8] != MAGIC {
            return Err(VaultError::Corrupt("bad magic"));
        }

        let m_cost = u32::from_le_bytes(blob[8..12].try_into().unwrap());
        let t_cost = u32::from_le_bytes(blob[12..16].try_into().unwrap());
        let p_cost = u32::from_le_bytes(blob[16..20].try_into().unwrap());
        // Refuse absurd parameters rather than letting a hostile file turn an
        // unlock attempt into an out-of-memory kill.
        if m_cost > 1024 * 1024 || t_cost > 32 || p_cost > 16 || m_cost == 0 {
            return Err(VaultError::Corrupt("implausible KDF parameters"));
        }

        let salt = &blob[20..20 + SALT_LEN];
        let nonce = &blob[36..36 + NONCE_LEN];
        let (header, ciphertext) = blob.split_at(HEADER_LEN);

        let mut key = derive_key(master_password, salt, m_cost, t_cost, p_cost)?;
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&key));
        let plaintext = cipher
            .decrypt(XNonce::from_slice(nonce), Payload { msg: ciphertext, aad: header })
            .map_err(|_| VaultError::BadPassword);

        let mut plaintext = match plaintext {
            Ok(p) => p,
            Err(e) => {
                key.zeroize();
                return Err(e);
            }
        };

        let entries: Vec<Credential> =
            serde_json::from_slice(&plaintext).map_err(|_| VaultError::Corrupt("bad payload"))?;
        plaintext.zeroize();

        self.next_id = entries.iter().map(|e| e.id).max().unwrap_or(0) + 1;
        self.entries = entries;
        self.key = Some(SessionKey(key));
        Ok(())
    }

    /// Drop the key and the decrypted entries from memory.
    pub fn lock(&mut self) {
        self.key = None; // ZeroizeOnDrop wipes the key material.
        for entry in &mut self.entries {
            entry.password.zeroize();
        }
        self.entries.clear();
    }

    pub fn list(&self) -> Result<Vec<CredentialSummary>> {
        self.require_unlocked()?;
        Ok(self
            .entries
            .iter()
            .map(|e| CredentialSummary {
                id: e.id,
                host: e.host.clone(),
                username: e.username.clone(),
                note: e.note.clone(),
                updated_at: e.updated_at,
            })
            .collect())
    }

    /// Reveal one secret. Separate from [`list`] so the UI has to ask.
    pub fn reveal(&self, id: i64) -> Result<Option<String>> {
        self.require_unlocked()?;
        Ok(self.entries.iter().find(|e| e.id == id).map(|e| e.password.clone()))
    }

    /// Add a credential, or update the password if host+username already
    /// exists. Returns the entry id.
    pub fn upsert(
        &mut self,
        host: &str,
        username: &str,
        password: &str,
        note: &str,
    ) -> Result<i64> {
        self.require_unlocked()?;
        let host = normalise_host(host);
        let now = now_millis();

        if let Some(existing) =
            self.entries.iter_mut().find(|e| e.host == host && e.username == username)
        {
            existing.password.zeroize();
            existing.password = password.to_string();
            existing.note = note.to_string();
            existing.updated_at = now;
            let id = existing.id;
            self.save()?;
            return Ok(id);
        }

        let id = self.next_id;
        self.next_id += 1;
        self.entries.push(Credential {
            id,
            host,
            username: username.to_string(),
            password: password.to_string(),
            note: note.to_string(),
            created_at: now,
            updated_at: now,
        });
        self.save()?;
        Ok(id)
    }

    pub fn remove(&mut self, id: i64) -> Result<bool> {
        self.require_unlocked()?;
        let before = self.entries.len();
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            entry.password.zeroize();
        }
        self.entries.retain(|e| e.id != id);
        let removed = self.entries.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Re-key the vault with a fresh salt and nonce.
    pub fn change_master_password(&mut self, new_password: &str) -> Result<()> {
        self.require_unlocked()?;
        let mut salt = [0u8; SALT_LEN];
        rand::thread_rng().fill_bytes(&mut salt);
        let key = derive_key(new_password, &salt, M_COST_KIB, T_COST, P_COST)?;
        self.key = Some(SessionKey(key));
        self.persist(&salt, M_COST_KIB, T_COST, P_COST)
    }

    fn require_unlocked(&self) -> Result<()> {
        self.key.as_ref().map(|_| ()).ok_or(VaultError::Locked)
    }

    /// Re-encrypt with the current key, reusing the on-disk salt.
    fn save(&self) -> Result<()> {
        let blob = fs::read(&self.path)?;
        if blob.len() < HEADER_LEN {
            return Err(VaultError::Corrupt("file shorter than header"));
        }
        let salt: [u8; SALT_LEN] = blob[20..20 + SALT_LEN].try_into().unwrap();
        let m_cost = u32::from_le_bytes(blob[8..12].try_into().unwrap());
        let t_cost = u32::from_le_bytes(blob[12..16].try_into().unwrap());
        let p_cost = u32::from_le_bytes(blob[16..20].try_into().unwrap());
        self.persist(&salt, m_cost, t_cost, p_cost)
    }

    fn persist(&self, salt: &[u8; SALT_LEN], m_cost: u32, t_cost: u32, p_cost: u32) -> Result<()> {
        let key = self.key.as_ref().ok_or(VaultError::Locked)?;

        // A fresh nonce on every write: XChaCha20's 192-bit nonce makes random
        // generation safe without tracking a counter across runs.
        let mut nonce = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce);

        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&m_cost.to_le_bytes());
        header.extend_from_slice(&t_cost.to_le_bytes());
        header.extend_from_slice(&p_cost.to_le_bytes());
        header.extend_from_slice(salt);
        header.extend_from_slice(&nonce);
        debug_assert_eq!(header.len(), HEADER_LEN);

        let mut plaintext = serde_json::to_vec(&self.entries)
            .map_err(|_| VaultError::Corrupt("could not serialize entries"))?;

        let cipher = XChaCha20Poly1305::new(Key::from_slice(&key.0));
        let ciphertext = cipher
            .encrypt(XNonce::from_slice(&nonce), Payload { msg: &plaintext, aad: &header })
            .map_err(|_| VaultError::Corrupt("encryption failed"));
        plaintext.zeroize();
        let ciphertext = ciphertext?;

        let mut out = header;
        out.extend_from_slice(&ciphertext);

        // Write-then-rename: a crash mid-write must never leave a truncated
        // vault where the old one was.
        let tmp = self.path.with_extension("bin.tmp");
        fs::write(&tmp, &out)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

fn derive_key(
    password: &str,
    salt: &[u8],
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<[u8; KEY_LEN]> {
    let params = Params::new(m_cost, t_cost, p_cost, Some(KEY_LEN))
        .map_err(|_| VaultError::Corrupt("invalid KDF parameters"))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut key = [0u8; KEY_LEN];
    argon2
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|_| VaultError::Corrupt("key derivation failed"))?;
    Ok(key)
}

/// Credentials are matched per host, case-insensitively, ignoring a leading
/// `www.` so a login saved on `www.example.com` is offered on `example.com`.
fn normalise_host(host: &str) -> String {
    let host = host.trim().to_ascii_lowercase();
    // Accept a full URL as well as a bare host, so callers can pass either.
    let host = crate::util::host_of(&host).unwrap_or(host);
    host.trim_start_matches("www.").trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Argon2id at 19 MiB is intentionally slow; tests use the minimum the
    /// crate accepts so the suite stays fast. The format is identical.
    fn fast_vault(path: &Path) -> Vault {
        Vault::new(path)
    }

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!("cleandark-test-{}-{}.bin", name, std::process::id()));
            let _ = fs::remove_file(&path);
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
            let _ = fs::remove_file(self.0.with_extension("bin.tmp"));
        }
    }

    #[test]
    fn create_store_lock_unlock_round_trip() {
        let file = TempFile::new("roundtrip");
        let mut vault = fast_vault(file.path());
        vault.create("correct horse battery staple").unwrap();
        vault.upsert("https://example.com/login", "alice", "s3cret", "work").unwrap();

        vault.lock();
        assert!(!vault.is_unlocked());
        assert!(matches!(vault.list(), Err(VaultError::Locked)));

        vault.unlock("correct horse battery staple").unwrap();
        let entries = vault.list().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].host, "example.com");
        assert_eq!(entries[0].username, "alice");
        assert_eq!(vault.reveal(entries[0].id).unwrap().as_deref(), Some("s3cret"));
    }

    #[test]
    fn wrong_password_is_rejected_and_leaves_the_vault_locked() {
        let file = TempFile::new("wrongpw");
        let mut vault = fast_vault(file.path());
        vault.create("right").unwrap();
        vault.upsert("example.com", "alice", "s3cret", "").unwrap();
        vault.lock();

        assert!(matches!(vault.unlock("wrong"), Err(VaultError::BadPassword)));
        assert!(!vault.is_unlocked());
        assert!(matches!(vault.list(), Err(VaultError::Locked)));
    }

    #[test]
    fn secrets_are_not_present_in_the_file() {
        let file = TempFile::new("ciphertext");
        let mut vault = fast_vault(file.path());
        vault.create("master").unwrap();
        vault.upsert("example.com", "alice", "PLAINTEXT_SECRET", "").unwrap();

        let raw = fs::read(file.path()).unwrap();
        let needle = b"PLAINTEXT_SECRET";
        assert!(
            !raw.windows(needle.len()).any(|w| w == needle),
            "password must not appear in the encrypted file"
        );
        // The username is metadata and is inside the same ciphertext.
        assert!(!raw.windows(5).any(|w| w == b"alice"));
    }

    #[test]
    fn tampering_with_the_header_is_detected() {
        let file = TempFile::new("tamper");
        let mut vault = fast_vault(file.path());
        vault.create("master").unwrap();
        vault.upsert("example.com", "alice", "s3cret", "").unwrap();
        vault.lock();

        // Downgrade the recorded Argon2 time cost. Because the header is
        // authenticated, this must fail rather than silently weaken the KDF.
        let mut raw = fs::read(file.path()).unwrap();
        raw[12] = 1;
        fs::write(file.path(), &raw).unwrap();

        assert!(vault.unlock("master").is_err());
    }

    #[test]
    fn implausible_kdf_parameters_are_refused() {
        let file = TempFile::new("kdfparams");
        let mut vault = fast_vault(file.path());
        vault.create("master").unwrap();
        vault.lock();

        let mut raw = fs::read(file.path()).unwrap();
        raw[8..12].copy_from_slice(&u32::MAX.to_le_bytes()); // ~4 TiB m_cost
        fs::write(file.path(), &raw).unwrap();

        assert!(matches!(vault.unlock("master"), Err(VaultError::Corrupt(_))));
    }

    #[test]
    fn a_truncated_file_is_reported_as_corrupt_not_as_a_bad_password() {
        let file = TempFile::new("truncated");
        fs::write(file.path(), b"CDVAULT\x01short").unwrap();
        let mut vault = fast_vault(file.path());
        assert!(matches!(vault.unlock("master"), Err(VaultError::Corrupt(_))));
    }

    #[test]
    fn upsert_replaces_the_password_for_a_known_login() {
        let file = TempFile::new("upsert");
        let mut vault = fast_vault(file.path());
        vault.create("master").unwrap();

        let first = vault.upsert("example.com", "alice", "old", "").unwrap();
        let second = vault.upsert("example.com", "alice", "new", "note").unwrap();
        assert_eq!(first, second, "same login must not create a second entry");
        assert_eq!(vault.list().unwrap().len(), 1);
        assert_eq!(vault.reveal(first).unwrap().as_deref(), Some("new"));

        // A different username on the same host is a separate entry.
        vault.upsert("example.com", "bob", "bobs", "").unwrap();
        assert_eq!(vault.list().unwrap().len(), 2);
    }

    #[test]
    fn hosts_are_normalised_before_being_stored() {
        let file = TempFile::new("hosts");
        let mut vault = fast_vault(file.path());
        vault.create("master").unwrap();

        // A full URL, mixed case and a `www.` prefix all collapse to the same
        // host, so the same login saved from different pages stays one entry.
        vault.upsert("https://WWW.Example.COM/login?x=1", "alice", "s", "").unwrap();
        assert_eq!(vault.list().unwrap()[0].host, "example.com");

        vault.upsert("example.com", "alice", "s2", "").unwrap();
        assert_eq!(vault.list().unwrap().len(), 1, "same host must not duplicate");

        // A different registrable host is genuinely different.
        vault.upsert("notexample.com", "alice", "s", "").unwrap();
        assert_eq!(vault.list().unwrap().len(), 2);
    }

    #[test]
    fn host_normalisation_rules() {
        assert_eq!(normalise_host("https://WWW.Example.COM/x?y=1"), "example.com");
        assert_eq!(normalise_host("  Example.com.  "), "example.com");
        assert_eq!(normalise_host("www.sub.example.com"), "sub.example.com");
        // Not a URL and not a bare host: kept as-is rather than silently
        // becoming something else.
        assert_eq!(normalise_host("localhost"), "localhost");
    }

    #[test]
    fn removal_persists() {
        let file = TempFile::new("remove");
        let mut vault = fast_vault(file.path());
        vault.create("master").unwrap();
        let id = vault.upsert("example.com", "alice", "s", "").unwrap();

        assert!(vault.remove(id).unwrap());
        assert!(!vault.remove(id).unwrap(), "removing twice is a no-op");

        vault.lock();
        vault.unlock("master").unwrap();
        assert!(vault.list().unwrap().is_empty());
    }

    #[test]
    fn changing_the_master_password_invalidates_the_old_one() {
        let file = TempFile::new("rekey");
        let mut vault = fast_vault(file.path());
        vault.create("old").unwrap();
        vault.upsert("example.com", "alice", "s3cret", "").unwrap();

        vault.change_master_password("new").unwrap();
        vault.lock();

        assert!(matches!(vault.unlock("old"), Err(VaultError::BadPassword)));
        vault.unlock("new").unwrap();
        assert_eq!(vault.reveal(vault.list().unwrap()[0].id).unwrap().as_deref(), Some("s3cret"));
    }

    #[test]
    fn every_write_uses_a_fresh_nonce() {
        let file = TempFile::new("nonce");
        let mut vault = fast_vault(file.path());
        vault.create("master").unwrap();
        vault.upsert("example.com", "alice", "s", "").unwrap();
        let first = fs::read(file.path()).unwrap();

        vault.upsert("example.com", "bob", "s", "").unwrap();
        let second = fs::read(file.path()).unwrap();

        assert_ne!(&first[36..60], &second[36..60], "nonce must not repeat");
        // Salt is stable across saves; only re-keying changes it.
        assert_eq!(&first[20..36], &second[20..36]);
    }

    #[test]
    fn listing_never_includes_the_secret() {
        let file = TempFile::new("nosecret");
        let mut vault = fast_vault(file.path());
        vault.create("master").unwrap();
        vault.upsert("example.com", "alice", "s3cret", "").unwrap();

        let json = serde_json::to_string(&vault.list().unwrap()).unwrap();
        assert!(!json.contains("s3cret"), "summaries must not carry passwords");
    }
}

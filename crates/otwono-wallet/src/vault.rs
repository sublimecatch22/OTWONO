//! The seed at rest: Argon2id from a passphrase, XChaCha20-Poly1305 over the seed.
//!
//! `FINANCE.md` §3 already decided the shape — financial keys come from a user passphrase
//! and not the node identity — and ADR-0022 §1 explains why a wallet is the strongest case
//! for it: the node key sits on the same disk as the data it would protect, so an attacker
//! holding the disk holds both.
//!
//! The cost is stated here as plainly as the documents state it: **forget the passphrase and
//! the seed in this file is gone.** The recovery phrase is the way back, and it is the only
//! one. Nobody can help.

use argon2::Argon2;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// Argon2id cost parameters, written into the file so a later change can still open an
/// older vault.
///
/// **OWASP's second listed Argon2id option, taken verbatim** (CLAUDE.md §2.3 — take the
/// mature recommendation rather than invent one at the point of use). Their options are
/// equivalent-security trades of memory against time: 47 MiB/t=1, **19 MiB/t=2**, 12 MiB/t=3,
/// 9 MiB/t=4, 7 MiB/t=5.
///
/// Chosen against the hardware this OS actually targets, and the memory is the reason rather
/// than the time. A tier-T0 board may have 512 MiB total with the rest of the system already
/// in it, and a KDF that allocates 64 MiB there is a KDF that may fail to unlock a wallet on
/// the machine it was created on. Picking a profile per machine is not an option either: a
/// wallet whose protection depends on which computer you open it from has a weakest link
/// that moves.
///
/// Measured on the amd64 development host, single-threaded (`cargo test -p otwono-wallet
/// timing_across_parameter_choices -- --ignored`):
///
/// | m | t | p | time |
/// |---|---|---|---|
/// | 64 MiB | 3 | 4 | 2.24 s |
/// | 64 MiB | 3 | 1 | 2.23 s |
/// | 64 MiB | 1 | 1 | 0.70 s |
/// | 32 MiB | 2 | 1 | 0.73 s |
/// | **19 MiB** | **2** | **1** | **0.42 s** |
///
/// A T0 board is several times slower than that host, so expect low seconds there. That is
/// the right price for unlocking a wallet and the wrong one for anything on a hot path,
/// which is another reason nothing but the wallet uses this.
///
/// **`p_cost` is 1 because more buys nothing here.** The first two rows are the measurement:
/// the `argon2` crate computes lanes sequentially, so `p = 4` costs the same wall-clock as
/// `p = 1` and merely advertises a parallelism this build does not have. If a threaded
/// implementation is ever adopted, raising it is a deliberate change with a new measurement,
/// not a default to inherit.
pub const ARGON2_M_COST: u32 = 19_456; // KiB
pub const ARGON2_T_COST: u32 = 2;
pub const ARGON2_P_COST: u32 = 1;

const SALT_BYTES: usize = 16;
const NONCE_BYTES: usize = 24;
const KEY_BYTES: usize = 32;
const SEED_BYTES: usize = 64;

/// The vault's own format version, independent of any schema on the control plane.
///
/// Bumped when the file layout changes. A vault this build cannot read is refused with the
/// version in the message rather than parsed hopefully — a wrong guess about a key file's
/// layout is not a thing to recover from optimistically.
pub const VAULT_VERSION: u32 = 1;

#[derive(Debug)]
pub enum VaultError {
    Io(String),
    /// The file exists and something other than its owner can read it.
    ///
    /// Refused, not repaired and not warned about: by the time this is observed the bytes
    /// have already been readable, so the honest report is that this key is compromised.
    /// The identity keystore applies the same rule for the same reason.
    InsecurePermissions {
        path: PathBuf,
        mode: u32,
    },
    /// A vault written by a version this build does not understand.
    UnknownVersion(u32),
    Malformed(String),
    /// The passphrase did not open the vault.
    ///
    /// Indistinguishable, by construction, from a corrupted or tampered file: the AEAD tag
    /// covers both and the difference is not something this code can know.
    WrongPassphrase,
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VaultError::Io(m) => write!(f, "{m}"),
            VaultError::InsecurePermissions { path, mode } => write!(
                f,
                "{} is mode {mode:04o}; a wallet another account can read is a wallet \
                 somebody else may already have. Refusing to use it",
                path.display()
            ),
            VaultError::UnknownVersion(v) => {
                write!(
                    f,
                    "this vault is version {v} and this build understands {VAULT_VERSION}"
                )
            }
            VaultError::Malformed(m) => write!(f, "this vault file is not readable: {m}"),
            VaultError::WrongPassphrase => f.write_str(
                "that passphrase does not open this vault. If the file has been damaged or \
                 altered the message is the same one, because the difference is not \
                 something this end can tell",
            ),
        }
    }
}

impl std::error::Error for VaultError {}

/// The on-disk form. Public so a test or a recovery tool can read it without this crate.
#[derive(Debug, Serialize, Deserialize)]
pub struct VaultFile {
    pub version: u32,
    /// Always `"argon2id"` and `"xchacha20poly1305"` today. Recorded rather than assumed so
    /// a future change is a value to branch on and not an archaeological problem.
    pub kdf: String,
    pub cipher: String,
    pub m_cost: u32,
    pub t_cost: u32,
    pub p_cost: u32,
    pub salt: String,
    pub nonce: String,
    pub ciphertext: String,
}

/// A passphrase-encrypted BIP-39 seed on disk.
pub struct Vault {
    path: PathBuf,
}

impl Vault {
    pub fn new(path: impl Into<PathBuf>) -> Vault {
        Vault { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Encrypt `seed` under `passphrase` and write it, 0600 from the moment it exists.
    ///
    /// A fresh salt and a fresh nonce every time, including on rewrite. Reusing either
    /// across two writes of different seeds under one passphrase is the kind of saving that
    /// costs a wallet.
    pub fn write(&self, seed: &[u8; SEED_BYTES], passphrase: &str) -> Result<(), VaultError> {
        self.write_with(seed, passphrase, ARGON2_M_COST, ARGON2_T_COST, ARGON2_P_COST)
    }

    /// The same, at named cost.
    ///
    /// Private, and it stays private. The read path honours whatever cost the *file*
    /// records, because a vault written before these constants changed must still open — but
    /// nothing outside this module may choose what a new vault is written at. A public
    /// version of this would be an API whose misuse is a silently weaker wallet, and the
    /// only caller that wants one is a test that does not want to spend half a second
    /// proving something about file permissions.
    fn write_with(
        &self,
        seed: &[u8; SEED_BYTES],
        passphrase: &str,
        m_cost: u32,
        t_cost: u32,
        p_cost: u32,
    ) -> Result<(), VaultError> {
        let mut salt = [0u8; SALT_BYTES];
        let mut nonce = [0u8; NONCE_BYTES];
        rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut salt);
        rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut nonce);

        let key = derive_key(passphrase, &salt, m_cost, t_cost, p_cost)?;
        let cipher = XChaCha20Poly1305::new(key.as_ref().into());
        let ciphertext = cipher
            .encrypt(XNonce::from_slice(&nonce), seed.as_slice())
            .map_err(|_| VaultError::Malformed("could not encrypt the seed".to_string()))?;

        let file = VaultFile {
            version: VAULT_VERSION,
            kdf: "argon2id".to_string(),
            cipher: "xchacha20poly1305".to_string(),
            m_cost,
            t_cost,
            p_cost,
            salt: data_encoding::BASE64.encode(&salt),
            nonce: data_encoding::BASE64.encode(&nonce),
            ciphertext: data_encoding::BASE64.encode(&ciphertext),
        };
        let body = serde_json::to_string_pretty(&file).map_err(|e| VaultError::Malformed(e.to_string()))?;

        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| VaultError::Io(format!("{}: {e}", dir.display())))?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| VaultError::Io(format!("{}: {e}", dir.display())))?;
        }
        // mode() on OpenOptions sets the permissions at creation, so the file is never
        // world-readable for an instant, not even before the bytes land.
        let mut handle = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&self.path)
            .map_err(|e| VaultError::Io(format!("{}: {e}", self.path.display())))?;
        handle
            .write_all(body.as_bytes())
            .map_err(|e| VaultError::Io(format!("{}: {e}", self.path.display())))?;
        handle
            .sync_all()
            .map_err(|e| VaultError::Io(format!("{}: {e}", self.path.display())))?;
        // An existing file keeps its old permissions through create(), so re-assert.
        std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| VaultError::Io(format!("{}: {e}", self.path.display())))
    }

    /// What the vault says about itself, without decrypting anything.
    ///
    /// Everything here is already visible to whoever can read the file, so returning it
    /// costs nothing that holding the file does not already cost. It exists so a status
    /// call can answer "is there a wallet, and what is it" without asking for a passphrase
    /// — which under ADR-0023 §2 is the only thing that *can* be answered without one, since
    /// no public key is stored in the clear.
    ///
    /// The permission check still applies: this is the file's metadata, not a licence to
    /// read the file.
    pub fn describe(&self) -> Result<VaultFile, VaultError> {
        let text = std::fs::read_to_string(&self.path)
            .map_err(|e| VaultError::Io(format!("{}: {e}", self.path.display())))?;
        self.check_permissions()?;
        serde_json::from_str(&text).map_err(|e| VaultError::Malformed(e.to_string()))
    }

    fn check_permissions(&self) -> Result<(), VaultError> {
        let mode = std::fs::metadata(&self.path)
            .map_err(|e| VaultError::Io(format!("{}: {e}", self.path.display())))?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            return Err(VaultError::InsecurePermissions {
                path: self.path.clone(),
                mode,
            });
        }
        Ok(())
    }

    /// Read and decrypt the seed.
    pub fn open(&self, passphrase: &str) -> Result<Zeroizing<[u8; SEED_BYTES]>, VaultError> {
        let text = std::fs::read_to_string(&self.path)
            .map_err(|e| VaultError::Io(format!("{}: {e}", self.path.display())))?;
        self.check_permissions()?;

        let file: VaultFile =
            serde_json::from_str(&text).map_err(|e| VaultError::Malformed(e.to_string()))?;
        if file.version != VAULT_VERSION {
            return Err(VaultError::UnknownVersion(file.version));
        }
        if file.kdf != "argon2id" || file.cipher != "xchacha20poly1305" {
            return Err(VaultError::Malformed(format!(
                "this vault says {} and {}, which this build does not implement",
                file.kdf, file.cipher
            )));
        }
        let salt = b64(&file.salt, "salt")?;
        let nonce = b64(&file.nonce, "nonce")?;
        let ciphertext = b64(&file.ciphertext, "ciphertext")?;
        if nonce.len() != NONCE_BYTES {
            return Err(VaultError::Malformed(format!(
                "a nonce is {NONCE_BYTES} bytes; this one is {}",
                nonce.len()
            )));
        }

        // The cost parameters come from the file, not from the constants: a vault written
        // before they changed must still open, which is the entire reason they are written
        // down. They are also attacker-controlled if the file is, so they are bounded --
        // an m_cost of four billion is a denial of service against the person unlocking it.
        if file.m_cost > 1_048_576 || file.t_cost > 16 || file.p_cost > 16 {
            return Err(VaultError::Malformed(
                "this vault asks for more work than any honest one needs".to_string(),
            ));
        }
        let key = derive_key(passphrase, &salt, file.m_cost, file.t_cost, file.p_cost)?;
        let cipher = XChaCha20Poly1305::new(key.as_ref().into());
        let plain = cipher
            .decrypt(XNonce::from_slice(&nonce), ciphertext.as_slice())
            .map_err(|_| VaultError::WrongPassphrase)?;
        let mut seed = Zeroizing::new([0u8; SEED_BYTES]);
        if plain.len() != SEED_BYTES {
            return Err(VaultError::Malformed(format!(
                "a seed is {SEED_BYTES} bytes; this vault holds {}",
                plain.len()
            )));
        }
        seed.copy_from_slice(&plain);
        Ok(seed)
    }
}

fn b64(text: &str, what: &str) -> Result<Vec<u8>, VaultError> {
    data_encoding::BASE64
        .decode(text.as_bytes())
        .map_err(|e| VaultError::Malformed(format!("{what}: {e}")))
}

fn derive_key(
    passphrase: &str,
    salt: &[u8],
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<Zeroizing<[u8; KEY_BYTES]>, VaultError> {
    let params = argon2::Params::new(m_cost, t_cost, p_cost, Some(KEY_BYTES))
        .map_err(|e| VaultError::Malformed(format!("argon2 parameters: {e}")))?;
    let argon = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; KEY_BYTES]);
    argon
        .hash_password_into(passphrase.as_bytes(), salt, key.as_mut())
        .map_err(|e| VaultError::Malformed(format!("argon2: {e}")))?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("otw-vault-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn a_seed() -> [u8; SEED_BYTES] {
        let mut s = [0u8; SEED_BYTES];
        for (i, b) in s.iter_mut().enumerate() {
            *b = i as u8;
        }
        s
    }

    /// Cheap parameters, for the tests that are about the file rather than the KDF.
    ///
    /// The shipped cost is measured at ~450 ms a call on the development host, and CLAUDE.md
    /// §6 gives a unit test under a second. Paying it in every test would mean either slow
    /// tests or fewer of them, and fewer is the worse trade. The round trip below still runs
    /// at the real cost, so the shipped parameters are exercised.
    const CHEAP: (u32, u32, u32) = (8, 1, 1);

    fn write_cheaply(v: &Vault, seed: &[u8; SEED_BYTES], pass: &str) {
        v.write_with(seed, pass, CHEAP.0, CHEAP.1, CHEAP.2).unwrap();
    }

    #[test]
    fn a_seed_survives_a_round_trip_at_the_shipped_cost() {
        // The one test that pays full price, so the constants this ships with are the ones
        // something actually exercises.
        let d = dir("roundtrip");
        let v = Vault::new(d.join("seed.vault"));
        v.write(&a_seed(), "correct horse battery staple").unwrap();
        let back = v
            .open("correct horse battery staple")
            .expect("the right passphrase opens it");
        assert_eq!(&back[..], &a_seed()[..]);

        let f: VaultFile = serde_json::from_str(&std::fs::read_to_string(v.path()).unwrap()).unwrap();
        assert_eq!(
            (f.m_cost, f.t_cost, f.p_cost),
            (ARGON2_M_COST, ARGON2_T_COST, ARGON2_P_COST)
        );
        assert_eq!(f.kdf, "argon2id");
        assert_eq!(f.cipher, "xchacha20poly1305");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_wrong_passphrase_does_not_open_it() {
        let d = dir("wrongpass");
        let v = Vault::new(d.join("seed.vault"));
        write_cheaply(&v, &a_seed(), "right");
        match v.open("wrong") {
            Err(VaultError::WrongPassphrase) => {}
            other => panic!("expected a refusal, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_tampered_vault_reads_as_a_wrong_passphrase_because_that_is_all_this_end_knows() {
        // Not a weakness being excused: the AEAD tag covers both cases and the difference is
        // genuinely not observable here. Saying "this file was tampered with" would be a
        // claim this code cannot support, and saying it only sometimes would leak which.
        let d = dir("tamper");
        let v = Vault::new(d.join("seed.vault"));
        write_cheaply(&v, &a_seed(), "right");

        let mut f: VaultFile = serde_json::from_str(&std::fs::read_to_string(v.path()).unwrap()).unwrap();
        let mut raw = data_encoding::BASE64.decode(f.ciphertext.as_bytes()).unwrap();
        raw[0] ^= 1;
        f.ciphertext = data_encoding::BASE64.encode(&raw);
        std::fs::write(v.path(), serde_json::to_string(&f).unwrap()).unwrap();
        std::fs::set_permissions(v.path(), std::fs::Permissions::from_mode(0o600)).unwrap();

        assert!(matches!(v.open("right"), Err(VaultError::WrongPassphrase)));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_file_is_never_readable_by_anybody_else() {
        let d = dir("mode");
        let v = Vault::new(d.join("seed.vault"));
        write_cheaply(&v, &a_seed(), "right");
        let mode = std::fs::metadata(v.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "vault is mode {mode:04o}");
        let dmode = std::fs::metadata(&d).unwrap().permissions().mode() & 0o777;
        assert_eq!(dmode, 0o700, "vault directory is mode {dmode:04o}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_world_readable_vault_is_refused_rather_than_repaired() {
        // By the time this is observed the bytes have already been readable. Quietly
        // chmodding it and carrying on would report a compromised key as a healthy one.
        let d = dir("insecure");
        let v = Vault::new(d.join("seed.vault"));
        write_cheaply(&v, &a_seed(), "right");
        std::fs::set_permissions(v.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        match v.open("right") {
            Err(VaultError::InsecurePermissions { mode, .. }) => assert_eq!(mode, 0o644),
            other => panic!("expected a refusal, got {other:?}"),
        }
        // And it really was refused rather than fixed on the way past.
        let mode = std::fs::metadata(v.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rewriting_uses_a_fresh_salt_and_nonce() {
        // Reusing either across two writes under one passphrase is the kind of saving that
        // costs a wallet: a repeated nonce under a repeated key breaks the AEAD outright.
        let d = dir("fresh");
        let v = Vault::new(d.join("seed.vault"));
        write_cheaply(&v, &a_seed(), "right");
        let first: VaultFile = serde_json::from_str(&std::fs::read_to_string(v.path()).unwrap()).unwrap();
        write_cheaply(&v, &a_seed(), "right");
        let second: VaultFile = serde_json::from_str(&std::fs::read_to_string(v.path()).unwrap()).unwrap();
        assert_ne!(first.salt, second.salt);
        assert_ne!(first.nonce, second.nonce);
        assert_ne!(
            first.ciphertext, second.ciphertext,
            "same seed, same passphrase, same bytes"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_vault_from_a_future_version_is_refused_by_version_not_parsed_hopefully() {
        let d = dir("version");
        let v = Vault::new(d.join("seed.vault"));
        write_cheaply(&v, &a_seed(), "right");
        let mut f: VaultFile = serde_json::from_str(&std::fs::read_to_string(v.path()).unwrap()).unwrap();
        f.version = VAULT_VERSION + 1;
        std::fs::write(v.path(), serde_json::to_string(&f).unwrap()).unwrap();
        std::fs::set_permissions(v.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        match v.open("right") {
            Err(VaultError::UnknownVersion(n)) => assert_eq!(n, VAULT_VERSION + 1),
            other => panic!("expected a version refusal, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_absurd_cost_in_the_file_is_refused_rather_than_attempted() {
        // If the file is attacker-controlled the cost parameters are too, and an m_cost of
        // four billion is a denial of service against the person trying to open their own
        // wallet -- or an out-of-memory kill on a T0 board.
        let d = dir("absurd");
        let v = Vault::new(d.join("seed.vault"));
        write_cheaply(&v, &a_seed(), "right");
        let mut f: VaultFile = serde_json::from_str(&std::fs::read_to_string(v.path()).unwrap()).unwrap();
        f.m_cost = 4_000_000_000;
        std::fs::write(v.path(), serde_json::to_string(&f).unwrap()).unwrap();
        std::fs::set_permissions(v.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        match v.open("right") {
            Err(VaultError::Malformed(m)) => assert!(m.contains("more work"), "{m}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_older_vault_opens_at_the_cost_it_was_written_with() {
        // Why the parameters are in the file at all. When the constants next change, every
        // wallet already on disk must still open, and this is the test that makes raising
        // them a safe act rather than a migration.
        let d = dir("oldcost");
        let v = Vault::new(d.join("seed.vault"));
        v.write_with(&a_seed(), "right", 16, 1, 1).unwrap();
        let f: VaultFile = serde_json::from_str(&std::fs::read_to_string(v.path()).unwrap()).unwrap();
        assert_eq!((f.m_cost, f.t_cost), (16, 1), "the file must record what it used");
        assert_eq!(&v.open("right").unwrap()[..], &a_seed()[..]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    #[ignore = "measurement, not an assertion; run with --ignored"]
    fn timing_across_parameter_choices() {
        for (m, t, pc) in [
            (65_536u32, 3u32, 4u32),
            (65_536, 3, 1),
            (65_536, 1, 1),
            (19_456, 2, 1),
            (32_768, 2, 1),
        ] {
            let salt = [7u8; SALT_BYTES];
            let start = std::time::Instant::now();
            derive_key("correct horse battery staple", &salt, m, t, pc).unwrap();
            println!("m={m} t={t} p={pc}: {:?}", start.elapsed());
        }
    }

    #[test]
    #[ignore = "measurement, not an assertion; run with --ignored"]
    fn timing_of_one_real_unlock() {
        let d = dir("timing");
        let v = Vault::new(d.join("seed.vault"));
        let t = std::time::Instant::now();
        v.write(&a_seed(), "correct horse battery staple").unwrap();
        let wrote = t.elapsed();
        let t = std::time::Instant::now();
        v.open("correct horse battery staple").unwrap();
        let opened = t.elapsed();
        println!("write {wrote:?} open {opened:?}");
        let _ = std::fs::remove_dir_all(&d);
    }
}

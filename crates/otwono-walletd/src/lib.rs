//! OTWONO wallet daemon.
//!
//! **STATUS: IMPLEMENTED** — unit and integration tested against a real permission broker;
//! no unit, not in any image, and never booted.
//!
//! Holds the household's money key and nothing else. ADR-0022 §2 put it here rather than in
//! `otwono-idd` because the blast radii differ: compromising the identity daemon costs the
//! node its name, which is bad and recoverable, and compromising this one costs funds, which
//! is bad and is not.
//!
//! # It holds nothing between calls
//!
//! ADR-0023 §1. There is no `wallet.unlock`, no session, and no timer. Every call that needs
//! the seed takes the passphrase, derives, uses it, and drops it — the seed lives in
//! `Zeroizing` for the duration of one call and never outlives it.
//!
//! That is not thrift. An unlock cache exists to amortise a cost, and the cost was measured
//! at 0.42 s on a key ADR-0022 says should be used rarely, so there is nothing worth
//! amortising — while the window it would open is exactly what an attacker wants. It also
//! means this daemon has no state to steal, no lifetime to get wrong, and nothing to lock.
//!
//! # No network, ever
//!
//! `PrivateNetwork=yes`, `RestrictAddressFamilies=AF_UNIX` when the unit exists. It signs;
//! `otwono-fetchd` carries (ADR-0014). A compromised chain endpoint cannot reach the signing
//! key because the signing key is in a process with no way to reach a socket.
//!
//! # Most of it cannot run yet, and that is deliberate
//!
//! `wallet.create`, `wallet.sign` and `wallet.export_seed` are `always_confirm`. `policy.rs`
//! turns `Allow` into `Ask` for those, and no confirmation channel exists until Phase 7, so
//! `otwono-permd` answers `confirmation_required`. On a booted node today the wallet can be
//! read and nothing else (ADR-0023 §4).

#![forbid(unsafe_code)]

use otwono_proto::{
    unknown_method, CallContext, Client, MethodDescription, RpcError, Service, ServiceDescription,
};
use otwono_wallet::{Account, AccountPath, Mnemonic, Vault};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;

pub const SERVICE_NAME: &str = "otwono-walletd";
pub const DESCRIBE_SCHEMA_VERSION: &str = "1.0.0";
pub const CAPABILITY_READ: &str = "wallet.read";
pub const CAPABILITY_CREATE: &str = "wallet.create";
pub const CAPABILITY_EXPORT: &str = "wallet.export_seed";

pub const DEFAULT_VAULT_PATH: &str = "/var/lib/otwono/wallet/seed.vault";

/// How many keys one `wallet.public_keys` call will derive.
///
/// ADR-0023 §1 requires a batch API and says why: every seed-using call pays the KDF, so a
/// UI deriving ten addresses in a loop pays it ten times. The bound exists because the work
/// is linear in the count and the caller chooses it — an unbounded count is a way to make a
/// T0 board sit still for a long time.
pub const MAX_KEYS_PER_CALL: usize = 64;

pub struct WalletService {
    vault: Vault,
    perm_socket: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicKeysParams {
    passphrase: String,
    coin: u32,
    #[serde(default)]
    account: u32,
    #[serde(default)]
    change: u32,
    /// Which indices to derive. Explicit rather than a range so a caller can ask for the
    /// three it actually wants without deriving everything below them.
    indices: Vec<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateParams {
    passphrase: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportParams {
    passphrase: String,
}

impl WalletService {
    pub fn new(vault_path: impl Into<PathBuf>, perm_socket: impl Into<PathBuf>) -> WalletService {
        WalletService {
            vault: Vault::new(vault_path),
            perm_socket: perm_socket.into(),
        }
    }

    pub fn vault_path(&self) -> &std::path::Path {
        self.vault.path()
    }

    fn authorize(&self, ctx: &CallContext, action: &str) -> Result<(), RpcError> {
        let token = ctx.capability.as_deref().ok_or_else(|| {
            RpcError::unauthorized(format!(
                "{action} requires a capability token; request one from otwono-permd"
            ))
        })?;
        let mut client = Client::connect(&self.perm_socket).map_err(|e| {
            RpcError::unavailable(format!(
                "cannot reach the permission broker at {}: {e}",
                self.perm_socket.display()
            ))
        })?;
        client
            .call(
                "perm.verify",
                json!({ "token": token, "action": action, "subject": ctx.peer.subject() }),
            )
            .map_err(|e| RpcError::unavailable(format!("permission broker call failed: {e}")))?
            .map(|_| ())
    }

    /// Whether a wallet exists here, and what it is — never anything secret.
    ///
    /// This is the whole of what can be answered without a passphrase, because ADR-0023 §2
    /// keeps no public key in the clear. A finance screen therefore cannot show an address
    /// or a balance until somebody unlocks, which is a real cost that ADR names rather than
    /// works around.
    fn handle_status(&self, ctx: &CallContext) -> Result<Value, RpcError> {
        self.authorize(ctx, CAPABILITY_READ)?;
        if !self.vault.exists() {
            return Ok(json!({
                "exists": false,
                "path": self.vault.path().display().to_string(),
                "note": "no wallet on this node. Creating one needs a person present, and \
                         that channel does not exist yet",
            }));
        }
        let described = self.vault.describe().map_err(rpc)?;
        Ok(json!({
            "exists": true,
            "path": self.vault.path().display().to_string(),
            "version": described.version,
            "kdf": described.kdf,
            "cipher": described.cipher,
            "m_cost": described.m_cost,
            "t_cost": described.t_cost,
            "p_cost": described.p_cost,
            "note": "an address cannot be shown without the passphrase; this node stores no \
                     public key in the clear, because one would publish every address this \
                     wallet will ever use",
        }))
    }

    /// Derive public keys at named indices.
    ///
    /// Batched deliberately (ADR-0023 §1): the KDF runs **once** for the whole call however
    /// many indices are asked for, which is the entire reason this takes a list rather than
    /// an index.
    fn handle_public_keys(&self, ctx: &CallContext, params: Value) -> Result<Value, RpcError> {
        self.authorize(ctx, CAPABILITY_READ)?;
        let p: PublicKeysParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("wallet.public_keys: {e}")))?;
        if p.indices.is_empty() {
            return Err(RpcError::invalid_params("name at least one index to derive"));
        }
        if p.indices.len() > MAX_KEYS_PER_CALL {
            return Err(RpcError::invalid_params(format!(
                "{} indices is more than the {MAX_KEYS_PER_CALL} one call will derive",
                p.indices.len()
            )));
        }

        let seed = self.vault.open(&p.passphrase).map_err(rpc)?;
        let mut keys = Vec::with_capacity(p.indices.len());
        for index in &p.indices {
            let path = AccountPath::new(p.coin, p.account, p.change, *index)
                .map_err(|e| RpcError::invalid_params(e.to_string()))?;
            let account = Account::derive(&seed, path).map_err(|e| RpcError::internal(e.to_string()))?;
            keys.push(json!({
                "path": path.to_string(),
                "index": index,
                "public_key": account.public_key_hex(),
            }));
        }
        Ok(json!({ "keys": keys }))
    }

    /// Create a wallet, and show its recovery phrase once.
    ///
    /// Guarded by `wallet.create`, which is `always_confirm` — so in practice this answers
    /// `confirmation_required` until Phase 7. It is written and tested now because the
    /// alternative is writing it later against a channel nobody has used either.
    ///
    /// **Refuses to overwrite**, rather than confirming (ADR-0023 §3). A confirmation dialog
    /// is the wrong instrument for "this destroys the key to your funds": the answer is no,
    /// and a prompt invites a yes. Replacing a wallet means removing the file by hand,
    /// having read what that means.
    fn handle_create(&self, ctx: &CallContext, params: Value) -> Result<Value, RpcError> {
        self.authorize(ctx, CAPABILITY_CREATE)?;
        let p: CreateParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("wallet.create: {e}")))?;
        if self.vault.exists() {
            return Err(RpcError::invalid_params(format!(
                "{} already holds a wallet. This will not overwrite it: the key to whatever \
                 it holds would be gone, and no confirmation makes that recoverable. Move \
                 the file aside deliberately if you mean to replace it",
                self.vault.path().display()
            )));
        }

        let mnemonic = Mnemonic::generate();
        let seed = mnemonic.seed("");
        self.vault.write(&seed, &p.passphrase).map_err(rpc)?;
        Ok(json!({
            "created": true,
            "path": self.vault.path().display().to_string(),
            "recovery_phrase": mnemonic.phrase(),
            "note": "write these 24 words down now and keep them off this machine. They are \
                     shown once. They are the only way back if the passphrase is forgotten, \
                     and anyone who reads them owns this wallet. Nobody can recover them for \
                     you",
        }))
    }

    /// Hand back the seed. The one call that gives away everything at once.
    ///
    /// ADR-0022 §3 requires the UI to make the person **re-enter the passphrase** rather than
    /// accept a confirmation click. That happens here by construction rather than by
    /// convention: opening the vault needs the passphrase, so there is no version of this
    /// call that does not have one.
    ///
    /// The recovery phrase is *not* returned, and cannot be: the vault holds the 64-byte
    /// seed, and a seed does not go back to words. Somebody who lost their phrase and still
    /// has their passphrase can export a working wallet, not a readable backup — worth
    /// saying because a UI offering "export" may leave people expecting the words.
    fn handle_export_seed(&self, ctx: &CallContext, params: Value) -> Result<Value, RpcError> {
        self.authorize(ctx, CAPABILITY_EXPORT)?;
        let p: ExportParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("wallet.export_seed: {e}")))?;
        let seed = self.vault.open(&p.passphrase).map_err(rpc)?;
        Ok(json!({
            "seed_hex": data_encoding_hex(&seed[..]),
            "note": "this is the whole wallet. It is the seed, not the recovery phrase: a \
                     seed cannot be turned back into words",
        }))
    }
}

/// Hex without pulling in a dependency for one call.
fn data_encoding_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn rpc(e: otwono_wallet::VaultError) -> RpcError {
    use otwono_wallet::VaultError;
    match e {
        // A wrong passphrase is the caller's problem, not this daemon's fault, and it is
        // also what a damaged file looks like from here.
        VaultError::WrongPassphrase => RpcError::invalid_params(e.to_string()),
        VaultError::InsecurePermissions { .. } => RpcError::unauthorized(e.to_string()),
        VaultError::Io(_) => RpcError::unavailable(e.to_string()),
        VaultError::UnknownVersion(_) | VaultError::Malformed(_) => RpcError::invalid_params(e.to_string()),
    }
}

impl Service for WalletService {
    fn describe(&self) -> ServiceDescription {
        ServiceDescription {
            service: SERVICE_NAME.to_string(),
            schema_version: DESCRIBE_SCHEMA_VERSION.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            methods: vec![
                MethodDescription::guarded(
                    "wallet.status",
                    "Whether a wallet exists here and what it is. Never anything secret",
                    CAPABILITY_READ,
                ),
                MethodDescription::guarded(
                    "wallet.public_keys",
                    "Derive public keys at named indices. Needs the passphrase",
                    CAPABILITY_READ,
                ),
                MethodDescription::guarded(
                    "wallet.create",
                    "Create a wallet and show its phrase once. Never overwrites",
                    CAPABILITY_CREATE,
                ),
                MethodDescription::guarded(
                    "wallet.export_seed",
                    "Reveal the seed, which is the whole wallet",
                    CAPABILITY_EXPORT,
                ),
            ],
        }
    }

    fn call(&self, ctx: &CallContext, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "wallet.status" => self.handle_status(ctx),
            "wallet.public_keys" => self.handle_public_keys(ctx, params),
            "wallet.create" => self.handle_create(ctx, params),
            "wallet.export_seed" => self.handle_export_seed(ctx, params),
            // Deliberately absent: wallet.sign. There is nothing to sign until a chain is
            // chosen (ADR-0022 leaves it open), and a signing method that existed but
            // refused would be a worse answer than one that is honestly not here.
            other => Err(unknown_method(other)),
        }
    }
}

//! Pluggable secret-vault backend used by `yui secret store` /
//! `yui secret unlock` to ferry the X25519 identity across
//! machines.
//!
//! yui doesn't authenticate against the vault itself — it shells
//! out to the provider's official CLI (`bw` for Bitwarden, `op`
//! for 1Password). Whatever auth that CLI supports (master
//! password, biometric, passkey unlock via the web vault, SSO)
//! gates the operation, and yui inherits it for free.
//!
//! Storage convention: the entire content of the X25519 identity
//! file (header comments + the `AGE-SECRET-KEY-1…` line) lives in
//! a Secure Note item under a user-chosen name. Picking notes
//! (rather than the password field) keeps the multi-line content
//! intact and doesn't pollute the vault's password-autofill UI.
//!
//! ## What yui *doesn't* try to do
//!
//! - Drive `bw login` / `op signin`. Those are interactive flows
//!   the user runs once per machine; yui just calls the CLI on
//!   the assumption it's already authenticated.
//! - Manage vault TOTP / passkey enrolment. Those live in the
//!   vault provider's own UI.
//! - Encrypt the X25519 a second time. The vault's own at-rest
//!   encryption is the trust boundary.

use std::process::{Command, Stdio};

use crate::config::{VaultConfig, VaultProvider};
use crate::{Error, Result};

/// Common interface for "fetch a Secure Note's content" and
/// "store a Secure Note's content" — the only two operations
/// `secret store` / `secret unlock` need.
pub trait Vault {
    /// Read the notes field of `item`. Errors if the item is
    /// missing, the CLI isn't authenticated, or the CLI is not
    /// installed.
    fn fetch(&self, item: &str) -> Result<Vec<u8>>;

    /// Create or overwrite the Secure Note at `item` with
    /// `content` in the notes field. `force = false` refuses to
    /// clobber an existing item; `force = true` overwrites.
    fn store(&self, item: &str, content: &[u8], force: bool) -> Result<()>;

    /// Human-readable provider name for log output.
    fn provider_name(&self) -> &'static str;
}

/// Build a vault driver from the user's config.
pub fn driver(cfg: &VaultConfig) -> Box<dyn Vault> {
    match cfg.provider {
        VaultProvider::Bitwarden => Box::new(BitwardenVault),
        VaultProvider::OnePassword => Box::new(OnePasswordVault),
    }
}

// ---------- Bitwarden ----------------------------------------------------

struct BitwardenVault;

impl Vault for BitwardenVault {
    fn provider_name(&self) -> &'static str {
        "Bitwarden"
    }

    fn fetch(&self, item: &str) -> Result<Vec<u8>> {
        // `bw get notes <item>` returns the notes field as plain
        // text on stdout. `bw` will refuse with a non-zero exit
        // when the item is missing or the vault is locked, which
        // we surface verbatim so the user sees the bw error.
        let output = Command::new("bw")
            .args(["get", "notes", item])
            .output()
            .map_err(|e| Error::Other(anyhow::anyhow!(
                "invoking `bw`: {e} — install Bitwarden CLI and run `bw login` + `bw unlock` once"
            )))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Other(anyhow::anyhow!(
                "bw get notes {item:?} failed: {}",
                stderr.trim()
            )));
        }
        Ok(output.stdout)
    }

    fn store(&self, item: &str, content: &[u8], force: bool) -> Result<()> {
        let content_str = std::str::from_utf8(content)
            .map_err(|e| Error::Other(anyhow::anyhow!("vault content is not valid UTF-8: {e}")))?;

        // Check whether an item with that name already exists.
        // `bw get item <name>` exits non-zero when missing.
        let existing = Command::new("bw")
            .args(["get", "item", item])
            .output()
            .map_err(|e| Error::Other(anyhow::anyhow!("invoking `bw`: {e}")))?;

        let item_json = serde_json::json!({
            "type": 2,                     // 2 = Secure Note
            "name": item,
            "notes": content_str,
            "secureNote": { "type": 0 },   // 0 = generic note
        });
        let payload = serde_json::to_vec(&item_json)
            .map_err(|e| Error::Other(anyhow::anyhow!("serialise bw item JSON: {e}")))?;
        let encoded = bw_encode(&payload)?;

        if existing.status.success() {
            if !force {
                return Err(Error::Other(anyhow::anyhow!(
                    "Bitwarden item {item:?} already exists; pass --force to overwrite"
                )));
            }
            // Pull the existing item's id so we can `bw edit item <id>`.
            let existing_value: serde_json::Value = serde_json::from_slice(&existing.stdout)
                .map_err(|e| Error::Other(anyhow::anyhow!("parse bw get item output: {e}")))?;
            let id = existing_value
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::Other(anyhow::anyhow!("bw item {item:?} has no id field")))?;
            run_bw_with_stdin(&["edit", "item", id], encoded.as_bytes())?;
        } else {
            run_bw_with_stdin(&["create", "item"], encoded.as_bytes())?;
        }
        Ok(())
    }
}

/// Base64-encode `payload` for `bw create item` / `bw edit item`,
/// which both expect a base64'd JSON blob on stdin (the same
/// shape `bw encode` produces).
fn bw_encode(payload: &[u8]) -> Result<String> {
    use base64::Engine as _;
    Ok(base64::engine::general_purpose::STANDARD.encode(payload))
}

fn run_bw_with_stdin(args: &[&str], stdin_bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut child = Command::new("bw")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Other(anyhow::anyhow!("invoking `bw`: {e}")))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| Error::Other(anyhow::anyhow!("bw stdin closed early")))?
        .write_all(stdin_bytes)
        .map_err(|e| Error::Other(anyhow::anyhow!("writing to bw stdin: {e}")))?;
    let output = child
        .wait_with_output()
        .map_err(|e| Error::Other(anyhow::anyhow!("waiting on bw: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Other(anyhow::anyhow!(
            "bw {} failed: {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    Ok(())
}

// ---------- 1Password ----------------------------------------------------

struct OnePasswordVault;

impl Vault for OnePasswordVault {
    fn provider_name(&self) -> &'static str {
        "1Password"
    }

    fn fetch(&self, item: &str) -> Result<Vec<u8>> {
        // `op item get <item> --field notesPlain` returns the
        // notes field as plain text on stdout.
        let output = Command::new("op")
            .args(["item", "get", item, "--field", "notesPlain"])
            .output()
            .map_err(|e| {
                Error::Other(anyhow::anyhow!(
                    "invoking `op`: {e} — install 1Password CLI and run `op signin` once"
                ))
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Other(anyhow::anyhow!(
                "op item get {item:?} --field notesPlain failed: {}",
                stderr.trim()
            )));
        }
        Ok(output.stdout)
    }

    fn store(&self, item: &str, content: &[u8], force: bool) -> Result<()> {
        let content_str = std::str::from_utf8(content)
            .map_err(|e| Error::Other(anyhow::anyhow!("vault content is not valid UTF-8: {e}")))?;

        let existing = Command::new("op")
            .args(["item", "get", item])
            .output()
            .map_err(|e| Error::Other(anyhow::anyhow!("invoking `op`: {e}")))?;

        let assignment = format!("notesPlain[text]={content_str}");

        if existing.status.success() {
            if !force {
                return Err(Error::Other(anyhow::anyhow!(
                    "1Password item {item:?} already exists; pass --force to overwrite"
                )));
            }
            let status = Command::new("op")
                .args(["item", "edit", item, &assignment])
                .status()
                .map_err(|e| Error::Other(anyhow::anyhow!("invoking `op`: {e}")))?;
            if !status.success() {
                return Err(Error::Other(anyhow::anyhow!(
                    "op item edit {item:?} failed"
                )));
            }
        } else {
            let status = Command::new("op")
                .args([
                    "item",
                    "create",
                    "--category",
                    "Secure Note",
                    "--title",
                    item,
                    &assignment,
                ])
                .status()
                .map_err(|e| Error::Other(anyhow::anyhow!("invoking `op`: {e}")))?;
            if !status.success() {
                return Err(Error::Other(anyhow::anyhow!(
                    "op item create {item:?} failed"
                )));
            }
        }
        Ok(())
    }
}

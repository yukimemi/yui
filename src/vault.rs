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
    /// Verify that the vault CLI is installed and authenticated
    /// before we try to read or write. yui doesn't drive
    /// `bw login` / `op signin` / `bw unlock` itself (the master
    /// password / passkey / SSO factor should go to the
    /// provider's CLI directly, not through yui's stdin), but it
    /// can at least detect the unauthenticated / locked state up
    /// front and emit an actionable hint instead of letting the
    /// raw provider error propagate.
    fn precheck(&self) -> Result<()>;

    /// Read the notes field of `item`. Errors if the item is
    /// missing or the CLI isn't installed. (Auth state is checked
    /// separately via `precheck`.)
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

    fn precheck(&self) -> Result<()> {
        let out = Command::new("bw").args(["status"]).output().map_err(|e| {
            Error::Other(anyhow::anyhow!(
                "invoking `bw status`: {e} — is the Bitwarden CLI installed?"
            ))
        })?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(Error::Other(anyhow::anyhow!(
                "bw status failed: {}",
                stderr.trim()
            )));
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout)
            .map_err(|e| Error::Other(anyhow::anyhow!("parse bw status output: {e}")))?;
        match v.get("status").and_then(|s| s.as_str()) {
            Some("unlocked") => Ok(()),
            Some("locked") => Err(Error::Other(anyhow::anyhow!(
                "Bitwarden vault is locked. Run `bw unlock` and follow \
                 its instructions to export the BW_SESSION env var, then \
                 retry. (BW vault unlock can use a passkey via the web \
                 vault flow if you've set that up.)"
            ))),
            Some("unauthenticated") => Err(Error::Other(anyhow::anyhow!(
                "Bitwarden CLI is not logged in. Run `bw login` (or \
                 `bw login --apikey` for non-interactive SSO/API-key \
                 use), then `bw unlock`, then retry."
            ))),
            other => Err(Error::Other(anyhow::anyhow!(
                "unexpected `bw status` output: status={other:?}"
            ))),
        }
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

    fn precheck(&self) -> Result<()> {
        // `op whoami` exits non-zero when the session is gone or
        // the desktop-app integration isn't unlocked. Stderr from
        // op already carries a decent message ("[ERROR] you are
        // not currently signed in. ..."); we wrap it with a yui-
        // shaped hint so the user doesn't have to wonder which
        // command we just tried.
        let out = Command::new("op").args(["whoami"]).output().map_err(|e| {
            Error::Other(anyhow::anyhow!(
                "invoking `op whoami`: {e} — is the 1Password CLI installed?"
            ))
        })?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(Error::Other(anyhow::anyhow!(
            "1Password CLI is not signed in: {}. \
             Run `op signin` (or unlock the 1Password desktop app to \
             auto-share its session via the CLI integration), then retry.",
            stderr.trim()
        )))
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

        // Build a JSON template and pipe it via stdin instead of
        // passing the secret as `notesPlain[text]=…` argv. The
        // assignment-statement form is documented but exposes
        // the secret to local `ps` / WMIC inspection while the
        // op process is alive — JSON-via-stdin is 1Password's
        // recommended secure flow. (PR #61 review by coderabbitai.)
        let template = serde_json::json!({
            "title": item,
            "category": "SECURE_NOTE",
            "fields": [
                {
                    "id": "notesPlain",
                    "type": "STRING",
                    "purpose": "NOTES",
                    "label": "notesPlain",
                    "value": content_str,
                }
            ],
        });
        let payload = serde_json::to_vec(&template)
            .map_err(|e| Error::Other(anyhow::anyhow!("serialise op item template: {e}")))?;

        if existing.status.success() {
            if !force {
                return Err(Error::Other(anyhow::anyhow!(
                    "1Password item {item:?} already exists; pass --force to overwrite"
                )));
            }
            run_op_with_stdin(&["item", "edit", item, "-"], &payload)?;
        } else {
            run_op_with_stdin(&["item", "create", "-"], &payload)?;
        }
        Ok(())
    }
}

fn run_op_with_stdin(args: &[&str], stdin_bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut child = Command::new("op")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Other(anyhow::anyhow!("invoking `op`: {e}")))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| Error::Other(anyhow::anyhow!("op stdin closed early")))?
        .write_all(stdin_bytes)
        .map_err(|e| Error::Other(anyhow::anyhow!("writing to op stdin: {e}")))?;
    let output = child
        .wait_with_output()
        .map_err(|e| Error::Other(anyhow::anyhow!("waiting on op: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Other(anyhow::anyhow!(
            "op {} failed: {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    Ok(())
}

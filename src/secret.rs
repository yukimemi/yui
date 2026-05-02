//! age-based file encryption for the secrets pipeline.
//!
//! `*.age` files in source are decrypted to a sibling without the
//! `.age` suffix on every `apply`, and the sibling lands in the
//! managed `# >>> yui rendered <<<` section of `.gitignore` so the
//! plaintext never gets committed. From the apply walker's
//! perspective the sibling is just another regular file — link it
//! to target like any other dotfile.
//!
//! ## Why a separate module from `render.rs`
//!
//! `*.tera` and `*.age` both produce a sibling-without-suffix and
//! both wire that sibling through the `.gitignore` managed section,
//! but they're different operations: rendering needs Tera contexts
//! and yui-when headers; decryption needs an age identity file and
//! recipient validation. Keeping `secret::*` self-contained also
//! means the crypto stays out of `render.rs`, which a casual
//! reader expects to be pure-text manipulation.
//!
//! ## v1 scope: X25519 only
//!
//! `[secrets] identity` is an X25519 secret (`AGE-SECRET-KEY-1…`)
//! and `recipients = […]` are X25519 public keys (`age1…`).
//! `yui secret init` produces both.
//!
//! Plugin-backed identities (YubiKey / FIDO2 passkey / Touch ID /
//! TPM / 1Password) need age's `plugin` feature plus callback
//! plumbing to drive the plugin binaries' interactive prompts —
//! a worthwhile follow-up but real implementation work, kept out
//! of v1 to ship the simple flow first.

use std::io::{Read as _, Write as _};
use std::str::FromStr as _;

use age::secrecy::ExposeSecret as _;
use camino::Utf8Path;

use crate::{Error, Result};

/// Load an age X25519 identity from `path`. The file is expected
/// to be the output of `age-keygen` / `yui secret init`: lines
/// beginning with `#` are comments, the first non-comment line is
/// the `AGE-SECRET-KEY-1…` secret.
pub fn load_identity(path: &Utf8Path) -> Result<age::x25519::Identity> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| Error::Other(anyhow::anyhow!("read identity {path}: {e}")))?;
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .ok_or_else(|| {
            Error::Other(anyhow::anyhow!(
                "identity file {path} contains no key (only comments / blank lines)"
            ))
        })?;

    age::x25519::Identity::from_str(line).map_err(|e| {
        Error::Other(anyhow::anyhow!(
            "identity file {path} is not a valid age X25519 secret \
             (expected `AGE-SECRET-KEY-1…`): {e}"
        ))
    })
}

/// Parse an X25519 recipient string (`age1…`).
pub fn parse_recipient(s: &str) -> Result<age::x25519::Recipient> {
    let trimmed = s.trim();
    age::x25519::Recipient::from_str(trimmed).map_err(|e| {
        Error::Other(anyhow::anyhow!(
            "not a valid age X25519 recipient {trimmed:?}: {e}"
        ))
    })
}

/// Encrypt `plaintext` to one or more recipients. The output is
/// the binary age format (the same bytes a `*.age` file holds on
/// disk).
pub fn encrypt(plaintext: &[u8], recipients: &[age::x25519::Recipient]) -> Result<Vec<u8>> {
    if recipients.is_empty() {
        return Err(Error::Other(anyhow::anyhow!(
            "no recipients configured — add at least one to `[secrets] recipients` \
             (or run `yui secret init` to generate a key)"
        )));
    }
    let encryptor =
        age::Encryptor::with_recipients(recipients.iter().map(|r| r as &dyn age::Recipient))
            .map_err(|e| Error::Other(anyhow::anyhow!("age encryptor: {e}")))?;

    let mut out = Vec::with_capacity(plaintext.len() + 256);
    let mut writer = encryptor
        .wrap_output(&mut out)
        .map_err(|e| Error::Other(anyhow::anyhow!("age wrap_output: {e}")))?;
    writer
        .write_all(plaintext)
        .map_err(|e| Error::Other(anyhow::anyhow!("age write: {e}")))?;
    writer
        .finish()
        .map_err(|e| Error::Other(anyhow::anyhow!("age finish: {e}")))?;
    Ok(out)
}

/// Decrypt `ciphertext` (the bytes of a `*.age` file) using the
/// supplied identity. Returns the plaintext on success.
pub fn decrypt(ciphertext: &[u8], identity: &age::x25519::Identity) -> Result<Vec<u8>> {
    let decryptor = age::Decryptor::new(ciphertext)
        .map_err(|e| Error::Other(anyhow::anyhow!("age decryptor: {e}")))?;
    let mut reader = decryptor
        .decrypt(std::iter::once(identity as &dyn age::Identity))
        .map_err(|e| Error::Other(anyhow::anyhow!("age decrypt: {e}")))?;
    let mut out = Vec::new();
    reader
        .read_to_end(&mut out)
        .map_err(|e| Error::Other(anyhow::anyhow!("age read: {e}")))?;
    Ok(out)
}

/// Generate a fresh X25519 keypair. Returns the serialised secret
/// (write this to the identity file) and the corresponding public
/// recipient string (add this to `[secrets] recipients`).
pub fn generate_x25519_keypair() -> (String, String) {
    let id = age::x25519::Identity::generate();
    let secret = id.to_string().expose_secret().to_string();
    let public = id.to_public().to_string();
    (secret, public)
}

/// Strip the `.age` suffix from a path, if present. Returns `None`
/// when the path doesn't end in `.age` (so callers can short-circuit
/// non-secret files in a uniform walk).
pub fn strip_age_suffix(path: &Utf8Path) -> Option<camino::Utf8PathBuf> {
    let name = path.file_name()?;
    let stem = name.strip_suffix(".age")?;
    if stem.is_empty() {
        return None; // a literal `.age` file with no stem isn't a secret backup
    }
    let parent = path.parent()?;
    Some(parent.join(stem))
}

/// Walk every `*.age` under `source`, decrypt to a sibling without
/// the suffix, and report the plaintext paths so the caller can
/// add them to the managed `.gitignore` section. Mirrors the
/// `render::render_all` shape: ignore-files honoured via
/// `paths::source_walker`, `.yuiignore` filters apply, `.yui/`
/// and `.git/` skipped.
///
/// Returns `Ok(SecretReport::default())` when `[secrets]` is off
/// (no recipients configured). Otherwise loads the identity once
/// and decrypts each `.age` file. `dry_run = true` skips the
/// disk write but still confirms the file decrypts (so a missing
/// identity / corrupted ciphertext surfaces as an error early).
pub fn decrypt_all(
    source: &Utf8Path,
    config: &crate::config::Config,
    dry_run: bool,
) -> Result<SecretReport> {
    let mut report = SecretReport::default();
    if !config.secrets.enabled() {
        return Ok(report);
    }

    let identity_path = crate::paths::expand_tilde(&config.secrets.identity);
    let identity = load_identity(&identity_path)?;

    let walker = crate::paths::source_walker(source).build();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let std_path = entry.path();
        let Some(name) = std_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".age") || name == ".age" {
            continue;
        }
        let cipher_path = match camino::Utf8PathBuf::from_path_buf(std_path.to_path_buf()) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let plaintext_path = match strip_age_suffix(&cipher_path) {
            Some(p) => p,
            None => continue,
        };

        let cipher_bytes = std::fs::read(&cipher_path)
            .map_err(|e| Error::Other(anyhow::anyhow!("read {cipher_path}: {e}")))?;
        let plain_bytes = decrypt(&cipher_bytes, &identity)?;

        // Drift check against the on-disk plaintext sibling, mirroring
        // the render-drift detection in `render::process_template`.
        // If the user edited the plaintext directly (target-as-truth
        // path), absorb already pulled the change into source; we
        // surface it as `diverged` here so they know to re-encrypt
        // (`yui secret encrypt <path>`) instead of silently
        // overwriting their edit with the stale ciphertext content.
        match std::fs::read(&plaintext_path) {
            Ok(existing) if existing == plain_bytes => {
                report.unchanged.push(plaintext_path);
                continue;
            }
            Ok(_) => {
                report.diverged.push(plaintext_path);
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(Error::Other(anyhow::anyhow!("read {plaintext_path}: {e}")));
            }
        }

        if !dry_run {
            if let Some(parent) = plaintext_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&plaintext_path, &plain_bytes)?;
        }
        report.written.push(plaintext_path);
    }
    Ok(report)
}

/// Per-`apply` summary of what the secrets walker did. Mirrors
/// `RenderReport`'s shape so the apply orchestrator can union
/// managed-path lists across both pipelines.
#[derive(Debug, Default)]
pub struct SecretReport {
    pub written: Vec<camino::Utf8PathBuf>,
    pub unchanged: Vec<camino::Utf8PathBuf>,
    /// Plaintext sibling diverged from current ciphertext. User
    /// edited the plaintext target directly; they must
    /// `yui secret encrypt <path>` to roll the change back into
    /// the canonical `.age` before the next apply.
    pub diverged: Vec<camino::Utf8PathBuf>,
}

impl SecretReport {
    pub fn has_drift(&self) -> bool {
        !self.diverged.is_empty()
    }

    /// Every plaintext sibling we know about — written, unchanged,
    /// or diverged. The apply orchestrator unions this with the
    /// render report's managed paths to build the `.gitignore`
    /// managed section.
    pub fn managed_paths(&self) -> impl Iterator<Item = &camino::Utf8PathBuf> {
        self.written
            .iter()
            .chain(self.unchanged.iter())
            .chain(self.diverged.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    #[test]
    fn x25519_round_trip() {
        let (secret, public) = generate_x25519_keypair();
        let id = age::x25519::Identity::from_str(&secret).unwrap();
        let recipient = parse_recipient(&public).unwrap();
        let plaintext = b"hello secret world\n";
        let cipher = encrypt(plaintext, &[recipient]).unwrap();
        // Ciphertext should look like an age file.
        assert!(cipher.starts_with(b"age-encryption.org/v1\n"));
        let recovered = decrypt(&cipher, &id).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn multi_recipient_decrypts_with_either_key() {
        let (secret_a, public_a) = generate_x25519_keypair();
        let (secret_b, public_b) = generate_x25519_keypair();
        let id_a = age::x25519::Identity::from_str(&secret_a).unwrap();
        let id_b = age::x25519::Identity::from_str(&secret_b).unwrap();
        let recipients = vec![
            parse_recipient(&public_a).unwrap(),
            parse_recipient(&public_b).unwrap(),
        ];
        let plaintext = b"team secret";
        let cipher = encrypt(plaintext, &recipients).unwrap();
        // Either identity should decrypt — that's the whole point of
        // multi-recipient encryption.
        assert_eq!(decrypt(&cipher, &id_a).unwrap(), plaintext);
        assert_eq!(decrypt(&cipher, &id_b).unwrap(), plaintext);
    }

    #[test]
    fn load_identity_skips_comments_and_blanks() {
        let tmp = TempDir::new().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("age.txt")).unwrap();
        let (secret, _public) = generate_x25519_keypair();
        let body = format!("# created: 2026-05-02\n# public key: ageXXX\n\n{secret}\n");
        std::fs::write(&path, body).unwrap();
        let id = load_identity(&path).unwrap();
        // Round-trip through decrypt to confirm we got a usable
        // identity back (not just any string-shaped placeholder).
        let recipient = parse_recipient(&id.to_public().to_string()).unwrap();
        let cipher = encrypt(b"x", &[recipient]).unwrap();
        assert_eq!(decrypt(&cipher, &id).unwrap(), b"x");
    }

    #[test]
    fn load_identity_errors_on_garbage() {
        let tmp = TempDir::new().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("bad.txt")).unwrap();
        std::fs::write(&path, "not a key at all\n").unwrap();
        // `age::x25519::Identity` deliberately doesn't impl Debug
        // (holds secret material), so `unwrap_err` won't compile —
        // pattern match instead.
        match load_identity(&path) {
            Ok(_) => panic!("expected error on garbage identity file"),
            Err(e) => assert!(format!("{e}").contains("not a valid age X25519 secret")),
        }
    }

    #[test]
    fn parse_recipient_rejects_garbage() {
        let err = parse_recipient("ssh-rsa AAAA…").unwrap_err();
        assert!(format!("{err}").contains("not a valid age X25519 recipient"));
    }

    #[test]
    fn encrypt_with_no_recipients_errors() {
        let err = encrypt(b"x", &[]).unwrap_err();
        assert!(format!("{err}").contains("no recipients"));
    }

    #[test]
    fn strip_age_suffix_basic() {
        assert_eq!(
            strip_age_suffix(Utf8PathBuf::from("home/.ssh/id_ed25519.age").as_path()),
            Some(Utf8PathBuf::from("home/.ssh/id_ed25519"))
        );
        // Multiple dots: only the trailing `.age` is stripped.
        assert_eq!(
            strip_age_suffix(Utf8PathBuf::from("home/notes.tar.gz.age").as_path()),
            Some(Utf8PathBuf::from("home/notes.tar.gz"))
        );
        // Not a secret.
        assert_eq!(
            strip_age_suffix(Utf8PathBuf::from("home/foo.txt").as_path()),
            None
        );
        // A literal `.age` with no stem isn't a secret either.
        assert_eq!(strip_age_suffix(Utf8PathBuf::from(".age").as_path()), None);
    }
}

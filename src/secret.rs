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
//! ## Two distinct encryption paths
//!
//! 1. **`*.age` files in apply** — encrypted to `[secrets] recipients`,
//!    decrypted with the plain X25519 secret at
//!    `[secrets] identity` (e.g. `~/.config/yui/age.txt`). Runs every
//!    apply, must be friction-free, must NOT trigger device prompts.
//!    Identities here are X25519 only by convention.
//!
//! 2. **passkey wrap of the X25519 secret itself** — the user's
//!    `~/.config/yui/age.txt` (plain X25519) gets encrypted to one
//!    or more passkey recipients (Pixel / Bitwarden / YubiKey, via
//!    the `age-plugin-fido2-hmac` etc.) so it can travel with the
//!    dotfiles repo as ciphertext. Used only by `yui secret wrap`
//!    and `yui secret unlock` — never by apply. Plugin identities
//!    appear ONLY here, so the apply path stays plugin-free.
//!
//! Recipient strings split the same way: `age1…` for X25519 and
//! `age1<plugin>1…` for plugin recipients. Multiple recipient types
//! can mix in a single ciphertext — useful for wrap, where the
//! user might want both Pixel and Bitwarden as recovery devices.

use std::io::{BufReader, Read as _, Write as _};
use std::str::FromStr as _;

use age::IdentityFile;
use age::cli_common::UiCallbacks;
use age::secrecy::ExposeSecret as _;
use camino::Utf8Path;

use crate::{Error, Result};

/// Boxed dyn-trait identity. age's `Decryptor::decrypt` takes a
/// trait-object iterator, so we hand it boxed identities; X25519
/// and plugin variants share the same type at the boundary.
pub type BoxedIdentity = Box<dyn age::Identity>;

/// Boxed dyn-trait recipient. Same reasoning as `BoxedIdentity` —
/// `Encryptor::with_recipients` works on trait objects.
pub type BoxedRecipient = Box<dyn age::Recipient + Send>;

/// Load an age X25519 identity from `path`, the way `apply` needs
/// it. Refuses anything other than a plain `AGE-SECRET-KEY-1…`
/// secret — apply must NEVER drop into a plugin flow because that
/// would prompt for a touch / PIN / biometric on every run.
/// (The user's mental model is "Pixel only at unlock time, not
/// every apply", so apply stays X25519-only on principle.)
pub fn load_x25519_identity(path: &Utf8Path) -> Result<age::x25519::Identity> {
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

/// Load every identity from `path`, allowing plugin entries
/// (`AGE-PLUGIN-…`). Used by `yui secret unlock` where the file
/// holds passkey identities (Pixel, Bitwarden, …) that age must
/// drive interactively at decrypt time.
///
/// `IdentityFile` parses comments / blank lines / multiple entries
/// per the standard age format; `with_callbacks(UiCallbacks)`
/// hands plugin invocations a terminal-based prompt for "press
/// the button now" / etc.
pub fn load_passkey_identities(path: &Utf8Path) -> Result<Vec<BoxedIdentity>> {
    let file = std::fs::File::open(path)
        .map_err(|e| Error::Other(anyhow::anyhow!("read passkey identities {path}: {e}")))?;
    let id_file = IdentityFile::from_buffer(BufReader::new(file))
        .map_err(|e| Error::Other(anyhow::anyhow!("parse {path}: {e}")))?;
    id_file
        .with_callbacks(UiCallbacks)
        .into_identities()
        .map_err(|e| Error::Other(anyhow::anyhow!("load identities from {path}: {e}")))
}

/// Parse an X25519 recipient string (`age1…`). Used for the
/// `[secrets] recipients` list which encrypts the user's `*.age`
/// files — those must stay plugin-free so apply doesn't prompt.
pub fn parse_x25519_recipient(s: &str) -> Result<age::x25519::Recipient> {
    let trimmed = s.trim();
    age::x25519::Recipient::from_str(trimmed).map_err(|e| {
        Error::Other(anyhow::anyhow!(
            "not a valid age X25519 recipient {trimmed:?}: {e}"
        ))
    })
}

/// Parse any recipient string — X25519 or plugin. Used by
/// `yui secret wrap` to encrypt the X25519 identity to
/// passkey-backed devices.
pub fn parse_passkey_recipient(s: &str) -> Result<BoxedRecipient> {
    let trimmed = s.trim();
    if let Ok(r) = age::x25519::Recipient::from_str(trimmed) {
        return Ok(Box::new(r));
    }
    if let Ok(r) = age::plugin::Recipient::from_str(trimmed) {
        let name = r.plugin().to_string();
        let plugin_recipient =
            age::plugin::RecipientPluginV1::new(&name, &[r], &[], UiCallbacks)
                .map_err(|e| Error::Other(anyhow::anyhow!("plugin recipient {trimmed:?}: {e}")))?;
        return Ok(Box::new(plugin_recipient));
    }
    Err(Error::Other(anyhow::anyhow!(
        "not a valid age recipient {trimmed:?} \
         (expected `age1…` or `age1<plugin>1…`)"
    )))
}

/// Encrypt `plaintext` to one or more X25519 recipients. Used for
/// `*.age` files in the apply pipeline.
pub fn encrypt_x25519(plaintext: &[u8], recipients: &[age::x25519::Recipient]) -> Result<Vec<u8>> {
    if recipients.is_empty() {
        return Err(Error::Other(anyhow::anyhow!(
            "no recipients configured — add at least one to `[secrets] recipients` \
             (or run `yui secret init` to generate a key)"
        )));
    }
    let encryptor =
        age::Encryptor::with_recipients(recipients.iter().map(|r| r as &dyn age::Recipient))
            .map_err(|e| Error::Other(anyhow::anyhow!("age encryptor: {e}")))?;
    write_encrypted(encryptor, plaintext)
}

/// Encrypt `plaintext` to one or more potentially-plugin
/// recipients. Used by `yui secret wrap` to encrypt the X25519
/// identity to passkey devices (Pixel + Bitwarden + …).
pub fn encrypt_to_passkeys(plaintext: &[u8], recipients: &[BoxedRecipient]) -> Result<Vec<u8>> {
    if recipients.is_empty() {
        return Err(Error::Other(anyhow::anyhow!(
            "no passkey recipients configured — add at least one to \
             `[secrets] passkey_recipients` (each entry is the public \
             key of a Pixel / Bitwarden / etc. device)"
        )));
    }
    let encryptor = age::Encryptor::with_recipients(
        recipients
            .iter()
            .map(|r| -> &dyn age::Recipient { r.as_ref() }),
    )
    .map_err(|e| Error::Other(anyhow::anyhow!("age encryptor: {e}")))?;
    write_encrypted(encryptor, plaintext)
}

fn write_encrypted(encryptor: age::Encryptor, plaintext: &[u8]) -> Result<Vec<u8>> {
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

/// Decrypt `ciphertext` (the bytes of a `*.age` file) using a
/// single X25519 identity. Used by the apply pipeline.
pub fn decrypt_x25519(ciphertext: &[u8], identity: &age::x25519::Identity) -> Result<Vec<u8>> {
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

/// Decrypt `ciphertext` using any of the supplied (potentially
/// plugin-backed) identities. Used by `yui secret unlock`.
pub fn decrypt_with_passkeys(ciphertext: &[u8], identities: &[BoxedIdentity]) -> Result<Vec<u8>> {
    let decryptor = age::Decryptor::new(ciphertext)
        .map_err(|e| Error::Other(anyhow::anyhow!("age decryptor: {e}")))?;
    let mut reader = decryptor
        .decrypt(identities.iter().map(|i| i.as_ref() as &dyn age::Identity))
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
/// and decrypts each `.age` file. The identity is X25519-only
/// here on purpose — apply must NOT trigger plugin / passkey
/// prompts every run.
///
/// Skips the `passkey_wrapped` ciphertext file: it's encrypted to
/// passkey recipients (NOT the X25519), so trying to decrypt it
/// here would fail loudly. The unlock path handles it instead.
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
    let identity = load_x25519_identity(&identity_path)?;

    // Resolve `passkey_wrapped` to an absolute path so we can skip
    // it inside the walker (it's encrypted to a passkey recipient,
    // not the X25519 identity, so it's not a regular `.age` file
    // that apply should decrypt).
    let passkey_wrapped_abs = config.secrets.passkey_wrapped.as_ref().map(|p| {
        let path = crate::paths::expand_tilde(p);
        if path.is_absolute() {
            path
        } else {
            source.join(path)
        }
    });

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
        // Skip the passkey-wrapped identity file — it's not a
        // regular `.age` we should decrypt during apply.
        if let Some(skip) = &passkey_wrapped_abs {
            if &cipher_path == skip {
                continue;
            }
        }
        let plaintext_path = match strip_age_suffix(&cipher_path) {
            Some(p) => p,
            None => continue,
        };

        let cipher_bytes = std::fs::read(&cipher_path)
            .map_err(|e| Error::Other(anyhow::anyhow!("read {cipher_path}: {e}")))?;
        let plain_bytes = decrypt_x25519(&cipher_bytes, &identity)?;

        // Drift check against the on-disk plaintext sibling, mirroring
        // the render-drift detection in `render::process_template`.
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

    fn write_x25519_identity_file(tmp: &TempDir, name: &str) -> (Utf8PathBuf, String) {
        let path = Utf8PathBuf::from_path_buf(tmp.path().join(name)).unwrap();
        let (secret, public) = generate_x25519_keypair();
        std::fs::write(&path, format!("{secret}\n")).unwrap();
        (path, public)
    }

    #[test]
    fn x25519_round_trip() {
        let tmp = TempDir::new().unwrap();
        let (id_path, public) = write_x25519_identity_file(&tmp, "age.txt");
        let identity = load_x25519_identity(&id_path).unwrap();
        let recipient = parse_x25519_recipient(&public).unwrap();
        let cipher = encrypt_x25519(b"hello secret world\n", &[recipient]).unwrap();
        assert!(cipher.starts_with(b"age-encryption.org/v1\n"));
        let recovered = decrypt_x25519(&cipher, &identity).unwrap();
        assert_eq!(recovered, b"hello secret world\n");
    }

    /// Wrap / unlock round-trip via a *boxed* X25519 identity (the
    /// passkey path uses Box<dyn Identity>, but plugin binaries
    /// aren't available in CI — exercise the same code path with
    /// X25519, which is plugin-free but uses the same general
    /// dyn-trait API).
    #[test]
    fn passkey_wrap_round_trip_via_x25519_proxy() {
        let tmp = TempDir::new().unwrap();
        let (id_path, public) = write_x25519_identity_file(&tmp, "age.txt");
        let recipients = vec![parse_passkey_recipient(&public).unwrap()];
        let plaintext = std::fs::read(&id_path).unwrap();
        let wrapped = encrypt_to_passkeys(&plaintext, &recipients).unwrap();
        // Boxed identity for the unlock side.
        let identities: Vec<BoxedIdentity> = {
            let id = load_x25519_identity(&id_path).unwrap();
            vec![Box::new(id)]
        };
        let recovered = decrypt_with_passkeys(&wrapped, &identities).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn multi_recipient_decrypts_with_either_key() {
        let tmp = TempDir::new().unwrap();
        let (_id_a_path, public_a) = write_x25519_identity_file(&tmp, "a.txt");
        let (_id_b_path, public_b) = write_x25519_identity_file(&tmp, "b.txt");
        let recipients = vec![
            parse_x25519_recipient(&public_a).unwrap(),
            parse_x25519_recipient(&public_b).unwrap(),
        ];
        let cipher = encrypt_x25519(b"team secret", &recipients).unwrap();
        // Either identity should decrypt.
        let id_a =
            load_x25519_identity(&Utf8PathBuf::from_path_buf(tmp.path().join("a.txt")).unwrap())
                .unwrap();
        let id_b =
            load_x25519_identity(&Utf8PathBuf::from_path_buf(tmp.path().join("b.txt")).unwrap())
                .unwrap();
        assert_eq!(decrypt_x25519(&cipher, &id_a).unwrap(), b"team secret");
        assert_eq!(decrypt_x25519(&cipher, &id_b).unwrap(), b"team secret");
    }

    #[test]
    fn load_x25519_skips_comments_and_blanks() {
        let tmp = TempDir::new().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("age.txt")).unwrap();
        let (secret, _public) = generate_x25519_keypair();
        let body = format!("# created: 2026-05-02\n# public key: ageXXX\n\n{secret}\n");
        std::fs::write(&path, body).unwrap();
        let _id = load_x25519_identity(&path).unwrap();
    }

    #[test]
    fn load_x25519_errors_on_garbage() {
        let tmp = TempDir::new().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("bad.txt")).unwrap();
        std::fs::write(&path, "not a key at all\n").unwrap();
        match load_x25519_identity(&path) {
            Ok(_) => panic!("expected parse error"),
            Err(e) => assert!(format!("{e}").contains("not a valid age X25519")),
        }
    }

    #[test]
    fn parse_recipient_rejects_garbage() {
        let err = parse_x25519_recipient("ssh-rsa AAAA…").unwrap_err();
        assert!(format!("{err}").contains("not a valid age X25519 recipient"));
    }

    #[test]
    fn parse_passkey_recipient_rejects_garbage() {
        // `Box<dyn Recipient + Send>` doesn't impl Debug, so
        // `unwrap_err` won't compile — match the result instead.
        match parse_passkey_recipient("ssh-rsa AAAA…") {
            Ok(_) => panic!("expected parse failure"),
            Err(e) => assert!(format!("{e}").contains("not a valid age recipient")),
        }
    }

    #[test]
    fn encrypt_with_no_recipients_errors() {
        let err = encrypt_x25519(b"x", &[]).unwrap_err();
        assert!(format!("{err}").contains("no recipients"));
    }

    #[test]
    fn encrypt_to_passkeys_with_no_recipients_errors() {
        let err = encrypt_to_passkeys(b"x", &[]).unwrap_err();
        assert!(format!("{err}").contains("no passkey recipients"));
    }

    #[test]
    fn strip_age_suffix_basic() {
        assert_eq!(
            strip_age_suffix(Utf8PathBuf::from("home/.ssh/id_ed25519.age").as_path()),
            Some(Utf8PathBuf::from("home/.ssh/id_ed25519"))
        );
        assert_eq!(
            strip_age_suffix(Utf8PathBuf::from("home/notes.tar.gz.age").as_path()),
            Some(Utf8PathBuf::from("home/notes.tar.gz"))
        );
        assert_eq!(
            strip_age_suffix(Utf8PathBuf::from("home/foo.txt").as_path()),
            None
        );
        assert_eq!(strip_age_suffix(Utf8PathBuf::from(".age").as_path()), None);
    }
}

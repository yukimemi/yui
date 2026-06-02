use super::*;
use crate::config;
use crate::paths;
use crate::render;
use crate::secret;
use crate::vars::YuiVars;
use crate::vault;
use anyhow::Result;
use camino::Utf8PathBuf;
use tracing::{info, warn};

/// `yui secret init [--comment TEXT]` — generate an age X25519
/// keypair on this machine, write the secret to the configured
/// identity path, and append the public key to
/// `$DOTFILES/config.toml` `[secrets] recipients`.
///
/// `config.toml` is the *committed* config (not the per-machine
/// `config.local.toml`). That's load-bearing for multi-machine
/// use: `recipients` is the public-key list every `*.age`
/// encryption wraps to, so machine B needs to see machine A's
/// public key after A runs `yui secret init`. Public keys are
/// safe to commit — the ciphertext only opens with the matching
/// secret, which never leaves the machine that generated it.
///
/// ## Migrating from yui ≤ v0.7.13
///
/// Older versions wrote the recipient into `config.local.toml`
/// (gitignored), which silently broke multi-machine use. If you
/// ran `yui secret init` against an earlier yui:
///
/// 1. Open `$DOTFILES/config.local.toml` and locate the
///    `[secrets] recipients = [...]` block.
/// 2. Cut it and paste it into `$DOTFILES/config.toml`.
/// 3. `git add config.toml && git commit && git push`.
/// 4. On every other machine: `git pull && yui apply` once.
///
/// Subsequent `yui secret init` (e.g. on a new machine) appends
/// directly to `config.toml` — no manual move needed.
pub fn secret_init(source: Option<Utf8PathBuf>, comment: Option<String>) -> Result<()> {
    let source = resolve_source(source)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;

    // 1. Resolve identity path (default: ~/.config/yui/age.txt).
    let identity_path = paths::expand_tilde(&config.secrets.identity);
    if identity_path.exists() {
        anyhow::bail!(
            "identity file already exists at {identity_path}; \
             refusing to overwrite. Delete it first if you really \
             mean to start fresh (you'll lose access to existing \
             .age files encrypted to its public key)."
        );
    }

    // 2. Generate the keypair + serialise the identity file with
    //    the same header age-keygen uses, so the file is
    //    interoperable with the standalone CLI tools.
    let (secret, public) = secret::generate_x25519_keypair();
    let now = jiff::Zoned::now().to_string();
    let body = format!(
        "# created: {now}\n\
         # public key: {public}\n\
         {secret}\n"
    );
    // 0600 on Unix so other local users can't read the X25519
    // secret. PR #60 review by coderabbitai.
    secret::write_private_file(&identity_path, body.as_bytes())?;
    info!("wrote identity file: {identity_path}");

    // 3. Append the public key to `[secrets] recipients` in the
    //    committed `config.toml`. Recipients are public — the
    //    other machines need to see this entry to encrypt new
    //    `*.age` files for the user who just ran init.
    let config_path = source.join("config.toml");
    let comment = comment.unwrap_or_else(|| format!("{} {}", yui.host, yui.user));
    let entry_comment = format!("{comment} — added by `yui secret init` on {now}");
    let config_existing = match std::fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => anyhow::bail!("read {config_path}: {e}"),
    };
    let updated_config = append_recipient_to_config(&config_existing, &entry_comment, &public)?;
    std::fs::write(&config_path, updated_config)?;
    info!("appended public key to {config_path}");
    println!();
    println!("  age identity:  {identity_path}");
    println!("  public key:    {public}");
    println!();
    println!(
        "  Next: encrypt a file with `yui secret encrypt <path>`. \
         The plaintext sibling will be auto-decrypted on every `yui apply`."
    );
    Ok(())
}

/// Append a recipient entry to the user's `config.toml`.
///
/// Uses `toml_edit` to parse the file into an in-memory document
/// tree, modify the `[secrets].recipients` array, then serialise
/// back. This preserves user comments / spacing / table ordering,
/// and survives quirky inputs (other tables after `[secrets]`,
/// trailing comments, multi-line arrays, etc.) — string-pasting
/// the same shape used to land tokens in the wrong place when the
/// file's layout deviated from the most common case. (Caught in
/// PR #57 review by gemini-code-assist.)
///
/// Returns the file unchanged when the public key is already in
/// the recipients list (idempotent re-init).
pub(crate) fn append_recipient_to_config(
    existing: &str,
    comment: &str,
    public: &str,
) -> Result<String> {
    use toml_edit::{Array, DocumentMut, Item, Table, Value};

    let mut doc: DocumentMut = if existing.trim().is_empty() {
        DocumentMut::new()
    } else {
        existing
            .parse()
            .map_err(|e| anyhow::anyhow!("config.toml is not valid TOML: {e}"))?
    };

    // Make sure `[secrets]` exists as a table.
    if !doc.contains_key("secrets") {
        let mut t = Table::new();
        t.set_implicit(false);
        doc.insert("secrets", Item::Table(t));
    }
    let secrets = doc["secrets"].as_table_mut().ok_or_else(|| {
        anyhow::anyhow!("[secrets] in config.toml is not a table — refusing to clobber")
    })?;

    // Make sure `recipients` is an array.
    if !secrets.contains_key("recipients") {
        secrets.insert("recipients", Item::Value(Value::Array(Array::new())));
    }
    let recipients = secrets["recipients"]
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("[secrets].recipients is not an array"))?;

    // Idempotent: if the public key already appears, we're done.
    let already_present = recipients.iter().any(|v| v.as_str() == Some(public));
    if already_present {
        return Ok(doc.to_string());
    }

    // Append the new entry with a leading-comment decor block so
    // the user can tell which key belongs to which machine just by
    // reading the file.
    let mut value = Value::from(public);
    let prefix = format!("\n  # {comment}\n  ");
    *value.decor_mut() = toml_edit::Decor::new(prefix, "");
    recipients.push_formatted(value);
    // Force the array onto multiple lines so the comments above
    // entries actually have a place to live (a single-line array
    // can't carry per-element comments).
    recipients.set_trailing("\n");
    recipients.set_trailing_comma(true);

    Ok(doc.to_string())
}

/// `yui secret encrypt <path> [--force] [--rm-plaintext]` — encrypt
/// a plaintext file to every recipient in `[secrets] recipients`
/// and write the ciphertext alongside as `<path>.age`.
pub fn secret_encrypt(
    source: Option<Utf8PathBuf>,
    path: Utf8PathBuf,
    force: bool,
    rm_plaintext: bool,
) -> Result<()> {
    let source = resolve_source(source)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;

    if !config.secrets.enabled() {
        anyhow::bail!(
            "no recipients configured — run `yui secret init` to generate \
             a keypair, or add at least one entry to `[secrets] recipients`."
        );
    }

    // Resolve the plaintext path: absolute as-is, relative against
    // CWD (so the user can `yui secret encrypt home/.ssh/id_ed25519`
    // from inside `$DOTFILES`).
    let plaintext_path = if path.is_absolute() {
        path.clone()
    } else {
        absolutize(&path)?
    };
    if !plaintext_path.is_file() {
        anyhow::bail!("plaintext file not found: {plaintext_path}");
    }
    let cipher_path = Utf8PathBuf::from(format!("{plaintext_path}.age"));
    if cipher_path.exists() && !force {
        anyhow::bail!("{cipher_path} already exists; pass --force to overwrite");
    }

    let plaintext = std::fs::read(&plaintext_path)?;
    // Use the general parser so `[secrets].recipients` can hold
    // plugin entries (`age1yubikey1…` / `age1fido2-hmac1…` etc.)
    // alongside the X25519 ones. yui doesn't drive plugin flows
    // first-class, but a hand-written plugin recipient still gets
    // a stanza in the ciphertext — useful if a user wants their
    // YubiKey to decrypt the same `*.age` outside yui via the
    // standalone `age` CLI.
    let recipients = secret::parse_passkey_recipients(&config.secrets.recipients)?;
    let cipher = secret::encrypt_to_passkeys(&plaintext, &recipients)?;
    std::fs::write(&cipher_path, &cipher)?;
    info!("encrypted {plaintext_path} → {cipher_path}");

    // Issue #71: close the .gitignore window. `apply` rewrites the
    // managed section from a full render+decrypt walk, but until
    // that runs the freshly created plaintext sibling is visible to
    // `git add` / `git commit -a`. Merge this one entry now so the
    // plaintext can't be staged accidentally between encrypt and the
    // next apply. Only meaningful when the plaintext actually lives
    // under `$DOTFILES` (matches the `rm_plaintext` safety check
    // below) and when gitignore management is enabled.
    if config.render.manage_gitignore && plaintext_path.starts_with(&source) {
        render::add_to_managed_section(&source, &plaintext_path)?;
    }
    info!("run `yui apply` to refresh links and the rest of the managed section");

    if rm_plaintext {
        // Only remove plaintext when it lives under `$DOTFILES` —
        // erasing files outside the repo on a typo would be cruel.
        if plaintext_path.starts_with(&source) {
            std::fs::remove_file(&plaintext_path)?;
            info!("removed plaintext: {plaintext_path}");
        } else {
            warn!(
                "plaintext lives outside source ({plaintext_path}); \
                 skipping --rm-plaintext as a safety check"
            );
        }
    }
    Ok(())
}

/// `yui secret store [--force]` — push the X25519 identity at
/// `[secrets].identity` into the configured `[secrets.vault]`.
/// Run on a machine that already has the identity; the new
/// machine then recovers it via `yui secret unlock`.
///
/// yui doesn't drive the vault's auth flow itself — it shells
/// out to `bw` / `op`. Whatever those CLIs are configured to
/// accept (master password, biometric, passkey unlock in the
/// web vault, SSO) gates the operation.
pub fn secret_store(source: Option<Utf8PathBuf>, force: bool) -> Result<()> {
    let source = resolve_source(source)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;

    let vault_cfg = config.secrets.vault.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "[secrets.vault] is not configured — set provider \
             (\"bitwarden\" or \"1password\") and item before \
             calling store"
        )
    })?;

    let identity_path = paths::expand_tilde(&config.secrets.identity);
    if !identity_path.is_file() {
        anyhow::bail!(
            "no X25519 identity at {identity_path}; run `yui secret init` first \
             (store needs that file's content to push to the vault)"
        );
    }
    let plaintext = std::fs::read(&identity_path)?;
    // Refuse to upload bytes that aren't actually an age identity
    // — a mistyped `[secrets].identity` path or a corrupted file
    // would otherwise stash garbage that `yui secret unlock`
    // would only fail to use later. (PR #61 review by coderabbitai.)
    secret::validate_x25519_identity_bytes(&plaintext)?;

    let vault = vault::driver(vault_cfg);
    // Verify the provider CLI is installed and authenticated
    // BEFORE reading the identity into memory + pushing — gives
    // the user an actionable hint instead of the raw `bw` /
    // `op` error from the upcoming write.
    vault.precheck()?;
    info!(
        "pushing X25519 identity to {} item {:?}",
        vault.provider_name(),
        config::VAULT_ITEM_NAME
    );
    vault.store(config::VAULT_ITEM_NAME, &plaintext, force)?;

    println!();
    println!(
        "  X25519 identity pushed to {} item {:?}",
        vault.provider_name(),
        config::VAULT_ITEM_NAME
    );
    println!("  On a new machine, run `yui secret unlock`.");
    Ok(())
}

/// `yui secret unlock` — fetch the X25519 identity from the
/// configured `[secrets.vault]` and write it to
/// `[secrets].identity`. The vault provider's CLI (`bw` / `op`)
/// handles auth — yui inherits whatever factor that CLI is
/// configured to require.
pub fn secret_unlock(source: Option<Utf8PathBuf>) -> Result<()> {
    let source = resolve_source(source)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;

    let vault_cfg = config.secrets.vault.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "[secrets.vault] is not configured — nothing to unlock. \
             Run `yui secret init` + `yui secret store` on an existing \
             machine first, then commit + push the config."
        )
    })?;
    let identity_path = paths::expand_tilde(&config.secrets.identity);
    if identity_path.exists() {
        anyhow::bail!(
            "{identity_path} already exists — refusing to clobber a live \
             X25519 identity. Delete it first if you really mean to \
             re-unlock from scratch."
        );
    }

    let vault = vault::driver(vault_cfg);
    vault.precheck()?;
    info!(
        "fetching X25519 identity from {} item {:?}",
        vault.provider_name(),
        config::VAULT_ITEM_NAME
    );
    let plaintext = vault.fetch(config::VAULT_ITEM_NAME)?;

    // Validate before persisting — the vault could legitimately
    // hold any blob, so the fetched bytes might not actually be
    // an age identity (typo'd item name, wrong field). Bail
    // before touching `[secrets].identity` so a future apply
    // doesn't fail with a confusing "not a valid age key" error.
    secret::validate_x25519_identity_bytes(&plaintext)?;

    // 0600 on Unix — never leave the X25519 secret world-readable.
    secret::write_private_file(&identity_path, &plaintext)?;
    info!("wrote X25519 identity: {identity_path}");
    println!();
    println!("  X25519 identity restored at {identity_path}");
    println!("  Run `yui apply` next.");
    Ok(())
}

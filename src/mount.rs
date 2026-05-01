//! Resolve `[[mount.entry]]` definitions: render `src` and `dst` via
//! Tera, evaluate `when`, drop disabled entries.

use camino::{Utf8Path, Utf8PathBuf};
use tera::Context;

use crate::Result;
use crate::config::{MountEntry, MountStrategy};
use crate::paths;
use crate::template::{self, Engine};

/// A mount entry after Tera rendering, tilde expansion, and
/// `when`-filtering. Both `src` and `dst` are absolute paths.
#[derive(Debug, Clone)]
pub struct ResolvedMount {
    /// Absolute path to the source subtree. For relative inputs this
    /// is `<source>/<entry.src>`; absolute / `~`-relative inputs land
    /// where the user pointed. Letting `src` escape the dotfiles repo
    /// is intentional — it's how a separate private clone (e.g.
    /// `~/.dotfiles-private/home`) participates as a mount without
    /// having to live under `$DOTFILES`.
    pub src: Utf8PathBuf,
    pub dst: Utf8PathBuf,
    pub strategy: MountStrategy,
}

pub fn resolve(
    source: &Utf8Path,
    entries: &[MountEntry],
    default_strategy: MountStrategy,
    engine: &mut Engine,
    ctx: &Context,
) -> Result<Vec<ResolvedMount>> {
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        if let Some(when) = &e.when {
            // `template::eval_truthy` accepts both bare (`yui.os == 'linux'`)
            // and pre-wrapped (`{{ … }}`) forms — same convention used by
            // marker links and render rules. Without it, a bare expression
            // would be silently filtered out (the literal expression string
            // doesn't equal "true" / "1" so the row drops). The README and
            // `init` skeleton recommend bare form, so this MUST agree.
            if !template::eval_truthy(when, engine, ctx)? {
                continue;
            }
        }
        let src_str = engine.render(e.src.as_str(), ctx)?;
        let src = paths::resolve_mount_src(source, src_str.trim());
        let dst_str = engine.render(&e.dst, ctx)?;
        let dst = paths::expand_tilde(dst_str.trim());
        out.push(ResolvedMount {
            src,
            dst,
            strategy: e.strategy.unwrap_or(default_strategy),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template;
    use crate::vars::YuiVars;

    fn vars() -> YuiVars {
        YuiVars {
            os: "linux".into(),
            arch: "x86_64".into(),
            host: "test".into(),
            user: "u".into(),
            source: "/dotfiles".into(),
        }
    }

    fn source() -> Utf8PathBuf {
        Utf8PathBuf::from("/dotfiles")
    }

    #[test]
    fn renders_dst_and_filters_when_false() {
        let entries = vec![
            MountEntry {
                src: "home".into(),
                dst: "/{{ yui.os }}/u".into(),
                when: None,
                strategy: None,
            },
            MountEntry {
                src: "appdata".into(),
                dst: "/appdata".into(),
                when: Some("{{ yui.os == 'windows' }}".into()),
                strategy: None,
            },
        ];
        let mut e = Engine::new();
        let ctx = template::config_context(&vars());
        let s = source();
        let resolved = resolve(&s, &entries, MountStrategy::Marker, &mut e, &ctx).unwrap();
        assert_eq!(resolved.len(), 1);
        // Relative `src` is resolved against the source root.
        assert_eq!(resolved[0].src, Utf8PathBuf::from("/dotfiles/home"));
        assert_eq!(resolved[0].dst, Utf8PathBuf::from("/linux/u"));
        assert_eq!(resolved[0].strategy, MountStrategy::Marker);
    }

    #[test]
    fn when_true_keeps_entry() {
        let entries = vec![MountEntry {
            src: "home".into(),
            dst: "/h".into(),
            when: Some("{{ yui.os == 'linux' }}".into()),
            strategy: None,
        }];
        let mut e = Engine::new();
        let ctx = template::config_context(&vars());
        let s = source();
        let resolved = resolve(&s, &entries, MountStrategy::Marker, &mut e, &ctx).unwrap();
        assert_eq!(resolved.len(), 1);
    }

    #[test]
    fn bare_when_form_works() {
        // Regression: README and `init` skeleton recommend bare-form `when`,
        // so this MUST resolve via `template::eval_truthy`. Earlier this
        // function used a direct `engine.render(when)` which only handled
        // the wrapped form — caught in PR #12 review (gemini-code-assist).
        let entries = vec![MountEntry {
            src: "home".into(),
            dst: "/h".into(),
            when: Some("yui.os == 'linux'".into()),
            strategy: None,
        }];
        let mut e = Engine::new();
        let ctx = template::config_context(&vars());
        let s = source();
        let resolved = resolve(&s, &entries, MountStrategy::Marker, &mut e, &ctx).unwrap();
        assert_eq!(resolved.len(), 1);
    }

    #[test]
    fn bare_when_form_filters_when_false() {
        let entries = vec![MountEntry {
            src: "home".into(),
            dst: "/h".into(),
            when: Some("yui.os == 'no-such-os'".into()),
            strategy: None,
        }];
        let mut e = Engine::new();
        let ctx = template::config_context(&vars());
        let s = source();
        let resolved = resolve(&s, &entries, MountStrategy::Marker, &mut e, &ctx).unwrap();
        assert!(resolved.is_empty());
    }

    #[test]
    fn per_entry_strategy_overrides_default() {
        let entries = vec![MountEntry {
            src: "home".into(),
            dst: "/h".into(),
            when: None,
            strategy: Some(MountStrategy::PerFile),
        }];
        let mut e = Engine::new();
        let ctx = template::config_context(&vars());
        let s = source();
        let resolved = resolve(&s, &entries, MountStrategy::Marker, &mut e, &ctx).unwrap();
        assert_eq!(resolved[0].strategy, MountStrategy::PerFile);
    }

    /// Absolute `src` lets a separate (e.g. private) clone outside
    /// `$DOTFILES` participate as a mount without symlinking. The
    /// resolver returns the absolute path verbatim.
    #[test]
    fn absolute_src_is_returned_verbatim() {
        let entries = vec![MountEntry {
            src: "/abs/private/home".into(),
            dst: "/h".into(),
            when: None,
            strategy: None,
        }];
        let mut e = Engine::new();
        let ctx = template::config_context(&vars());
        let s = source();
        let resolved = resolve(&s, &entries, MountStrategy::Marker, &mut e, &ctx).unwrap();
        assert_eq!(resolved[0].src, Utf8PathBuf::from("/abs/private/home"));
    }

    /// Tera renders against the same context the call site builds
    /// (yui.* + vars.*). Letting `src` use `{{ yui.host }}` etc.
    /// makes per-machine source dirs trivial.
    #[test]
    fn src_renders_via_tera() {
        let entries = vec![MountEntry {
            src: "private/{{ yui.host }}/home".into(),
            dst: "/h".into(),
            when: None,
            strategy: None,
        }];
        let mut e = Engine::new();
        let ctx = template::config_context(&vars());
        let s = source();
        let resolved = resolve(&s, &entries, MountStrategy::Marker, &mut e, &ctx).unwrap();
        // vars().host is "test"
        assert_eq!(
            resolved[0].src,
            Utf8PathBuf::from("/dotfiles/private/test/home")
        );
    }
}

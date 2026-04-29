//! Resolve `[[mount.entry]]` definitions: render `dst` via Tera, evaluate
//! `when`, drop disabled entries.

use camino::Utf8PathBuf;
use tera::Context;

use crate::Result;
use crate::config::{MountEntry, MountStrategy};
use crate::paths;
use crate::template::{self, Engine};

/// A mount entry after Tera rendering of `dst` and `when`-filtering.
#[derive(Debug, Clone)]
pub struct ResolvedMount {
    pub src: Utf8PathBuf,
    pub dst: Utf8PathBuf,
    pub strategy: MountStrategy,
}

pub fn resolve(
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
        let dst_str = engine.render(&e.dst, ctx)?;
        let dst = paths::expand_tilde(dst_str.trim());
        out.push(ResolvedMount {
            src: e.src.clone(),
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
        let resolved = resolve(&entries, MountStrategy::Marker, &mut e, &ctx).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].src, Utf8PathBuf::from("home"));
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
        let resolved = resolve(&entries, MountStrategy::Marker, &mut e, &ctx).unwrap();
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
        let resolved = resolve(&entries, MountStrategy::Marker, &mut e, &ctx).unwrap();
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
        let resolved = resolve(&entries, MountStrategy::Marker, &mut e, &ctx).unwrap();
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
        let resolved = resolve(&entries, MountStrategy::Marker, &mut e, &ctx).unwrap();
        assert_eq!(resolved[0].strategy, MountStrategy::PerFile);
    }
}

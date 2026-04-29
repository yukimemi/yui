//! Shared Tera engine + context builders.
//!
//! Two contexts:
//!   - `config_context` — exposes only `yui.*` and the `env(name=…)` function.
//!     Used while parsing `config*.toml` (vars aren't fully resolved yet).
//!   - `template_context` — `yui.*` + `vars.*` + `env(…)`. Used to render
//!     `*.tera` dotfiles after the merged config is known.

use std::collections::HashMap;

use serde::Serialize;
use tera::{Context, Tera, Value};

use crate::Result;
use crate::vars::YuiVars;

pub struct Engine {
    tera: Tera,
}

impl Engine {
    pub fn new() -> Self {
        let mut tera = Tera::default();
        tera.register_function("env", env_fn);
        Self { tera }
    }

    pub fn render(&mut self, src: &str, ctx: &Context) -> Result<String> {
        self.tera
            .render_str(src, ctx)
            .map_err(|e| crate::Error::Template(format_tera_error(&e)))
    }
}

/// Tera's `Display` impl only emits the top-level message
/// (`Failed to render '__tera_one_off'`), leaving the actual reason
/// (`variable 'vars.home_root' not found in context` etc.) buried in
/// the source chain. Walk the chain and join every level so the user
/// sees something they can act on.
fn format_tera_error(err: &tera::Error) -> String {
    use std::error::Error as _;
    let mut parts: Vec<String> = vec![err.to_string()];
    let mut src = err.source();
    while let Some(e) = src {
        parts.push(e.to_string());
        src = e.source();
    }
    parts.join(": ")
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

/// `env(name="VAR", default="…")` — read an env var, return `default` (or empty
/// string) when unset. Returning a string (rather than null) keeps `default`
/// arg simple; callers can also chain Tera's `default` filter.
fn env_fn(args: &HashMap<String, Value>) -> tera::Result<Value> {
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| tera::Error::msg("env(name=…): missing or non-string 'name'"))?;
    let default = args.get("default").cloned();
    match std::env::var(name) {
        Ok(v) => Ok(Value::String(v)),
        Err(_) => Ok(default.unwrap_or_else(|| Value::String(String::new()))),
    }
}

pub fn config_context(yui: &YuiVars) -> Context {
    let mut ctx = Context::new();
    ctx.insert("yui", yui);
    ctx
}

pub fn template_context<V: Serialize>(yui: &YuiVars, vars: &V) -> Context {
    let mut ctx = Context::new();
    ctx.insert("yui", yui);
    ctx.insert("vars", vars);
    ctx
}

/// Evaluate a Tera expression as a truthy/falsy boolean. Accepts either a
/// bare expression (`yui.os == 'linux'`) or a pre-wrapped one
/// (`{{ yui.os == 'linux' }}`); used wherever the user writes a `when`
/// condition (mount entry, render rule, marker link, file-header).
pub fn eval_truthy(expr: &str, engine: &mut Engine, ctx: &Context) -> Result<bool> {
    let trimmed = expr.trim_start();
    let to_render = if trimmed.starts_with("{{") || trimmed.starts_with("{%") {
        expr.to_string()
    } else {
        format!("{{{{ {expr} }}}}")
    };
    let out = engine.render(&to_render, ctx)?;
    let s = out.trim();
    Ok(s.eq_ignore_ascii_case("true") || s == "1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8Path;

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
    fn renders_yui_vars() {
        let mut e = Engine::new();
        let ctx = config_context(&vars());
        let out = e
            .render("os={{ yui.os }}, arch={{ yui.arch }}", &ctx)
            .unwrap();
        assert_eq!(out, "os=linux, arch=x86_64");
    }

    #[test]
    fn env_function_with_default() {
        // SAFETY: single-threaded test, no other env access in this case.
        unsafe { std::env::remove_var("YUI_TEST_UNSET_VAR") };
        let mut e = Engine::new();
        let ctx = config_context(&vars());
        let out = e
            .render(
                "{{ env(name='YUI_TEST_UNSET_VAR', default='fallback') }}",
                &ctx,
            )
            .unwrap();
        assert_eq!(out, "fallback");
    }

    #[test]
    fn boolean_expression_renders_to_true_or_false() {
        let mut e = Engine::new();
        let ctx = config_context(&vars());
        let out = e.render("{{ yui.os == 'linux' }}", &ctx).unwrap();
        assert_eq!(out, "true");
        let out = e.render("{{ yui.os == 'windows' }}", &ctx).unwrap();
        assert_eq!(out, "false");
    }

    #[test]
    fn template_context_exposes_user_vars() {
        let mut e = Engine::new();
        let mut user_vars = toml::Table::new();
        user_vars.insert("greet".into(), toml::Value::String("hi".into()));
        let ctx = template_context(&vars(), &user_vars);
        let out = e.render("{{ vars.greet }} {{ yui.user }}", &ctx).unwrap();
        assert_eq!(out, "hi u");
        // ensure the camino import isn't unused
        let _: &Utf8Path = Utf8Path::new(".");
    }
}

//! The config file, in KDL like niri's: `$XDG_CONFIG_HOME/tiri/config.kdl`,
//! or `~/.config/tiri/config.kdl`. The server reads it each time a client
//! attaches, so edits apply from the next attach. Without one, everything
//! is as built in.
//!
//! [`DEFAULT`] is a commented config with everything at its default, which
//! `tiri config default` prints for a starting point.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use crossterm::event::{KeyCode, KeyEvent};
use knus::ast::{Literal, SpannedNode, TypeName};
use knus::decode::{Context, Kind};
use knus::errors::{DecodeError, ExpectedType};
use knus::span::Spanned;
use knus::traits::{DecodeScalar, ErrorSpan};
use miette::{GraphicalReportHandler, GraphicalTheme};

use crate::keys::{Action, Bindings, Key, Table};
use crate::render::Color;
use crate::theme::Theme;

/// The default config, commented: what tiri does without one.
pub const DEFAULT: &str = include_str!("default-config.kdl");

/// What the config file sets, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub theme: Theme,
    pub bindings: Bindings,
}

impl Default for Config {
    /// What tiri does without a config: [`DEFAULT`], which is also where the
    /// built-in key bindings are written down.
    fn default() -> Self {
        let raw = decode("default-config.kdl", DEFAULT).expect("the default config parses");
        let empty = Bindings {
            prefix: "Ctrl+a".parse().expect("a key"),
            prefix_binds: Table::default(),
            binds: Table::default(),
            overview_binds: Table::default(),
        };
        raw.resolve("default-config.kdl", empty)
            .expect("the default config is valid")
    }
}

impl Config {
    /// Reads the config at `path`; defaults if there's no file.
    pub fn load(path: &Path) -> Result<Config> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => bail!("couldn't read {}: {e}", path.display()),
        };
        Config::parse(&path.display().to_string(), &text)
    }

    /// Parses config `text`, naming it `file` in errors. Its key bindings
    /// add to and override the built-in ones.
    pub fn parse(file: &str, text: &str) -> Result<Config> {
        decode(file, text)?.resolve(file, Config::default().bindings)
    }
}

/// Decodes config `text` without looking names up.
fn decode(file: &str, text: &str) -> Result<RawConfig> {
    knus::parse(file, text).map_err(|e| {
        // An error report showing the offending lines, as plain text: it
        // reaches the user through the attaching client.
        let mut report = String::new();
        let handler = GraphicalReportHandler::new_themed(GraphicalTheme::unicode_nocolor());
        match handler.render_report(&mut report, &e) {
            Ok(()) => anyhow::anyhow!("{report}"),
            Err(_) => anyhow::anyhow!("{file}: {e}"),
        }
    })
}

/// Where the config file is: under `$XDG_CONFIG_HOME`, or `~/.config`.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("tiri").join("config.kdl"))
}

/// The file as written, before theme names are looked up.
#[derive(knus::Decode, Debug, Default)]
struct RawConfig {
    #[knus(child, unwrap(argument), default)]
    theme: Option<String>,
    #[knus(children(name = "define-theme"))]
    themes: Vec<RawTheme>,
    #[knus(child, unwrap(argument), default)]
    prefix: Option<ConfigKey>,
    #[knus(child, default)]
    prefix_binds: RawBinds,
    #[knus(child, default)]
    binds: RawBinds,
    #[knus(child, default)]
    overview_binds: RawBinds,
}

/// A section of key bindings, as in `binds { Alt+h { focus-column-left; } }`.
#[derive(knus::Decode, Debug, Default)]
struct RawBinds {
    #[knus(children)]
    binds: Vec<RawBind>,
}

/// One binding: a key, and its action, or None for `unbind`.
#[derive(Debug)]
struct RawBind {
    key: Key,
    action: Option<Action>,
}

impl<S: ErrorSpan> knus::Decode<S> for RawBind {
    fn decode_node(node: &SpannedNode<S>, ctx: &mut Context<S>) -> Result<Self, DecodeError<S>> {
        let key = node
            .node_name
            .parse::<Key>()
            .map_err(|e| DecodeError::conversion(&node.node_name, e))?;
        for arg in &node.arguments {
            ctx.emit_error(DecodeError::unexpected(
                &arg.literal,
                "argument",
                "a binding takes no arguments; its action goes in braces",
            ));
        }
        let children = node.children.as_ref().map_or(&[][..], |c| &c[..]);
        let [child] = children else {
            return Err(DecodeError::missing(
                node,
                "a binding needs one action, as in `h { focus-column-left; }`",
            ));
        };
        if &**child.node_name == "unbind" {
            return Ok(RawBind { key, action: None });
        }
        let action = Action::decode_node(child, ctx)?;
        Ok(RawBind {
            key,
            action: Some(action),
        })
    }
}

impl RawBinds {
    /// Applies these bindings on top of `table`.
    fn apply_to(self, table: &mut Table, file: &str, section: &str) -> Result<()> {
        let mut seen = HashSet::new();
        for bind in self.binds {
            if !seen.insert(bind.key) {
                bail!("{file}: {section} has {} more than once", bind.key);
            }
            table.set(bind.key, bind.action);
        }
        Ok(())
    }
}

#[derive(knus::Decode, Debug)]
struct RawTheme {
    #[knus(argument)]
    name: String,
    #[knus(property(name = "based-on"), default)]
    based_on: Option<String>,
    #[knus(child, unwrap(argument), default)]
    focused_border: Option<ConfigColor>,
    #[knus(child, unwrap(argument), default)]
    unfocused_border: Option<ConfigColor>,
    #[knus(child, unwrap(argument), default)]
    status_fg: Option<ConfigColor>,
    #[knus(child, unwrap(argument), default)]
    status_bg: Option<ConfigColor>,
    #[knus(child, unwrap(argument), default)]
    status_active_fg: Option<ConfigColor>,
    #[knus(child, unwrap(argument), default)]
    status_active_bg: Option<ConfigColor>,
    #[knus(child, unwrap(argument), default)]
    dim: Option<ConfigColor>,
    #[knus(child, unwrap(argument), default)]
    selection_bg: Option<SelectionBg>,
}

impl RawConfig {
    /// Looks names up, and lays this config's bindings over `bindings`.
    fn resolve(self, file: &str, mut bindings: Bindings) -> Result<Config> {
        if let Some(ConfigKey(prefix)) = self.prefix {
            bindings.prefix = prefix;
        }
        self.prefix_binds
            .apply_to(&mut bindings.prefix_binds, file, "prefix-binds")?;
        self.binds.apply_to(&mut bindings.binds, file, "binds")?;
        self.overview_binds
            .apply_to(&mut bindings.overview_binds, file, "overview-binds")?;

        let mut defined: Vec<(String, Theme)> = Vec::new();
        for raw in self.themes {
            let taken =
                Theme::named(&raw.name).is_some() || defined.iter().any(|(n, _)| *n == raw.name);
            if taken {
                bail!("{file}: there's already a theme named {:?}", raw.name);
            }
            // Themes can build on built-in ones, or ones defined above.
            let base = match &raw.based_on {
                None => Theme::default(),
                Some(name) => lookup(name, &defined)
                    .ok_or_else(|| unknown_theme(file, "based-on", name, &defined))?,
            };
            let name = raw.name.clone();
            defined.push((name, raw.apply_to(base)));
        }
        let theme = match &self.theme {
            None => Theme::default(),
            Some(name) => lookup(name, &defined)
                .ok_or_else(|| unknown_theme(file, "theme", name, &defined))?,
        };
        Ok(Config { theme, bindings })
    }
}

impl RawTheme {
    fn apply_to(self, mut theme: Theme) -> Theme {
        let set = |field: &mut Color, value: Option<ConfigColor>| {
            if let Some(ConfigColor(color)) = value {
                *field = color;
            }
        };
        set(&mut theme.focused_border, self.focused_border);
        set(&mut theme.unfocused_border, self.unfocused_border);
        set(&mut theme.status_fg, self.status_fg);
        set(&mut theme.status_bg, self.status_bg);
        set(&mut theme.status_active_fg, self.status_active_fg);
        set(&mut theme.status_active_bg, self.status_active_bg);
        set(&mut theme.dim, self.dim);
        if let Some(SelectionBg(bg)) = self.selection_bg {
            theme.selection_bg = bg;
        }
        theme
    }
}

/// A built-in theme, or one the config defined.
fn lookup(name: &str, defined: &[(String, Theme)]) -> Option<Theme> {
    (defined.iter())
        .find(|(n, _)| n == name)
        .map(|(_, t)| *t)
        .or_else(|| Theme::named(name))
}

fn unknown_theme(
    file: &str,
    setting: &str,
    name: &str,
    defined: &[(String, Theme)],
) -> anyhow::Error {
    let names: Vec<&str> = (Theme::ALL.iter().map(|(n, _)| *n))
        .chain(defined.iter().map(|(n, _)| n.as_str()))
        .collect();
    anyhow::anyhow!(
        "{file}: {setting} {name:?} isn't a theme; there's {}",
        names.join(", ")
    )
}

/// A key as written in the config's `prefix`, as in "Ctrl+a".
#[derive(Debug, Clone, Copy)]
struct ConfigKey(Key);

impl<S: ErrorSpan> DecodeScalar<S> for ConfigKey {
    fn type_check(type_name: &Option<Spanned<TypeName, S>>, ctx: &mut Context<S>) {
        no_type_name(type_name, ctx, "key");
    }

    fn raw_decode(
        value: &Spanned<Literal, S>,
        ctx: &mut Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        let Literal::String(s) = &**value else {
            ctx.emit_error(DecodeError::scalar_kind(Kind::String, value));
            return Ok(ConfigKey(Key::from_event(KeyEvent::from(KeyCode::Null))));
        };
        match s.parse() {
            Ok(key) => Ok(ConfigKey(key)),
            Err(e) => {
                ctx.emit_error(DecodeError::unexpected(value, "key", e));
                Ok(ConfigKey(Key::from_event(KeyEvent::from(KeyCode::Null))))
            }
        }
    }
}

/// A color as written in the config: "#rrggbb", or a palette index.
#[derive(Debug, Clone, Copy)]
struct ConfigColor(Color);

impl<S: ErrorSpan> DecodeScalar<S> for ConfigColor {
    fn type_check(type_name: &Option<Spanned<TypeName, S>>, ctx: &mut Context<S>) {
        no_type_name(type_name, ctx, "color");
    }

    fn raw_decode(
        value: &Spanned<Literal, S>,
        ctx: &mut Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        let color = match &**value {
            Literal::String(s) => parse_hex(s).ok_or("colors are written \"#rrggbb\""),
            Literal::Int(i) => u8::try_from(i)
                .map(Color::Idx)
                .map_err(|_| "palette colors are 0 to 255"),
            _ => {
                ctx.emit_error(DecodeError::scalar_kind(Kind::String, value));
                return Ok(ConfigColor(Color::Default));
            }
        };
        Ok(ConfigColor(color.unwrap_or_else(|message| {
            ctx.emit_error(DecodeError::unexpected(value, "color", message));
            Color::Default
        })))
    }
}

/// The selection's background: a color, or "reverse".
#[derive(Debug, Clone, Copy)]
struct SelectionBg(Option<Color>);

impl<S: ErrorSpan> DecodeScalar<S> for SelectionBg {
    fn type_check(type_name: &Option<Spanned<TypeName, S>>, ctx: &mut Context<S>) {
        no_type_name(type_name, ctx, "color");
    }

    fn raw_decode(
        value: &Spanned<Literal, S>,
        ctx: &mut Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        if matches!(&**value, Literal::String(s) if &**s == "reverse") {
            return Ok(SelectionBg(None));
        }
        ConfigColor::raw_decode(value, ctx).map(|ConfigColor(color)| SelectionBg(Some(color)))
    }
}

fn no_type_name<S: ErrorSpan>(
    type_name: &Option<Spanned<TypeName, S>>,
    ctx: &mut Context<S>,
    rust_type: &'static str,
) {
    if let Some(typ) = type_name {
        ctx.emit_error(DecodeError::TypeName {
            span: typ.span().clone(),
            found: Some((**typ).clone()),
            expected: ExpectedType::no_type(),
            rust_type,
        });
    }
}

fn parse_hex(s: &str) -> Option<Color> {
    let hex = s.strip_prefix('#').filter(|h| h.len() == 6)?;
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(Color::Rgb(
        (value >> 16) as u8,
        (value >> 8) as u8,
        value as u8,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config> {
        Config::parse("config.kdl", text)
    }

    #[test]
    fn the_default_config_is_the_default() {
        assert_eq!(parse(DEFAULT).unwrap(), Config::default());
        // Its example theme, switched on, has the default theme's colors.
        let example = DEFAULT
            .replace("/-define-theme", "define-theme")
            .replace("theme \"default\"", "theme \"mine\"");
        assert_eq!(parse(&example).unwrap(), Config::default());
        assert_ne!(example, DEFAULT);
    }

    fn key(s: &str) -> Key {
        s.parse().unwrap()
    }

    #[test]
    fn the_default_bindings() {
        let b = Config::default().bindings;
        assert_eq!(b.prefix, key("Ctrl+a"));
        assert_eq!(
            b.prefix_binds.get(key("Shift+h")),
            Some(Action::MoveColumnLeft)
        );
        assert_eq!(b.prefix_binds.get(key("$")), Some(Action::FocusColumnLast));
        assert_eq!(
            b.binds.get(key("Alt+{")),
            Some(Action::ConsumeOrExpelPaneLeft)
        );
        assert_eq!(
            b.binds.get(key("Alt+Shift+u")),
            Some(Action::MoveColumnToWorkspaceDown)
        );
        assert_eq!(
            b.overview_binds.get(key("Escape")),
            Some(Action::CloseOverview)
        );
        assert_eq!(b.binds.get(key("Alt+n")), None);
    }

    #[test]
    fn bindings_add_to_the_defaults() {
        let b = parse(
            r#"
            prefix "Ctrl+b"
            prefix-binds {
                v { new-column; }
                n { unbind; }
                x { detach; }
            }
            binds { Alt+n { new-column; }; }
            "#,
        )
        .unwrap()
        .bindings;
        assert_eq!(b.prefix, key("Ctrl+b"));
        assert_eq!(b.prefix_binds.get(key("v")), Some(Action::NewColumn));
        assert_eq!(b.prefix_binds.get(key("n")), None);
        assert_eq!(b.prefix_binds.get(key("x")), Some(Action::Detach));
        // Untouched ones stay.
        assert_eq!(b.prefix_binds.get(key("h")), Some(Action::FocusColumnLeft));
        assert_eq!(b.binds.get(key("Alt+n")), Some(Action::NewColumn));
        assert_eq!(b.binds.get(key("Alt+Enter")), Some(Action::NewColumn));
    }

    #[test]
    fn explains_binding_mistakes() {
        let err = |text| format!("{:#}", parse(text).unwrap_err());
        let twice = err("binds { Alt+h { detach; }; Alt+H { detach; }; Alt+h { detach; }; }");
        assert!(twice.contains("binds has Alt+h more than once"), "{twice}");
        let bad_key = err("binds { Super+h { detach; }; }");
        assert!(
            bad_key.contains("isn't a modifier") && bad_key.contains("Super+h"),
            "{bad_key}"
        );
        let bad_action = err("binds { Alt+h { focus-left; }; }");
        assert!(bad_action.contains("focus-left"), "{bad_action}");
        let no_action = err("binds { Alt+h; }");
        assert!(no_action.contains("needs one action"), "{no_action}");
        let bad_prefix = err(r#"prefix "Ctrl+""#);
        assert!(bad_prefix.contains("no key given"), "{bad_prefix}");
    }

    #[test]
    fn an_empty_config_is_the_default() {
        assert_eq!(parse("").unwrap(), Config::default());
    }

    #[test]
    fn picks_a_built_in_theme() {
        let config = parse(r#"theme "oxide""#).unwrap();
        assert_eq!(Some(config.theme), Theme::named("oxide"));
    }

    #[test]
    fn defines_a_theme_on_top_of_another() {
        let config = parse(
            r##"
            theme "mine"
            define-theme "mine" based-on="oxide" {
                focused-border "#ff0000"
                dim 242
                selection-bg "reverse"
            }
            "##,
        )
        .unwrap();
        let oxide = Theme::named("oxide").unwrap();
        assert_eq!(
            config.theme,
            Theme {
                focused_border: Color::Rgb(0xff, 0, 0),
                dim: Color::Idx(242),
                selection_bg: None,
                ..oxide
            }
        );
    }

    #[test]
    fn themes_can_build_on_earlier_ones() {
        let config = parse(
            r##"
            theme "b"
            define-theme "a" { dim "#010203"; }
            define-theme "b" based-on="a" { status-fg 7; }
            "##,
        )
        .unwrap();
        assert_eq!(config.theme.dim, Color::Rgb(1, 2, 3));
        assert_eq!(config.theme.status_fg, Color::Idx(7));
    }

    #[test]
    fn explains_mistakes() {
        let err = |text| format!("{:#}", parse(text).unwrap_err());
        assert!(
            err(r#"theme "nope""#)
                .contains(r#"theme "nope" isn't a theme; there's default, oxide"#)
        );
        assert!(err(r#"define-theme "oxide""#).contains("already a theme named \"oxide\""));
        let bad_color = err(r#"define-theme "x" { dim "red"; }"#);
        assert!(bad_color.contains("#rrggbb"), "{bad_color}");
        // Points at the line.
        assert!(bad_color.contains("dim \"red\""), "{bad_color}");
        assert!(err("color 1").contains("color"));
    }
}

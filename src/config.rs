//! The config file, in KDL like niri's: `$XDG_CONFIG_HOME/tiri/config.kdl`,
//! or `~/.config/tiri/config.kdl`. The server reads it each time a client
//! attaches, so edits apply from the next attach. Without one, everything
//! is as built in.
//!
//! [`DEFAULT`] is a commented config with everything at its default, which
//! `tiri config default` prints for a starting point.

use std::collections::HashSet;
use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crossterm::event::{KeyCode, KeyEvent};
use knus::ast::{Literal, SpannedNode, TypeName};
use knus::decode::{Context, Kind};
use knus::errors::{DecodeError, ExpectedType};
use knus::span::Spanned;
use knus::traits::{DecodeScalar, ErrorSpan};
use miette::{Diagnostic, GraphicalReportHandler, GraphicalTheme, LabeledSpan, SourceCode};

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
        BUILT_IN.clone()
    }
}

/// [`DEFAULT`], parsed once. A test checks that it parses.
static BUILT_IN: LazyLock<Config> = LazyLock::new(|| {
    decode("default-config.kdl", DEFAULT)
        .map_err(|e| e.to_string())
        .and_then(|raw| raw.resolve(None))
        .expect("the default config is valid")
});

impl Config {
    /// Reads the config at `path`; defaults if there's no file.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let file = path.display().to_string();
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(ConfigError::plain(&file, format!("couldn't read it: {e}"))),
        };
        Config::parse(&file, &text)
    }

    /// Parses config `text`, naming it `file` in errors. Its key bindings
    /// add to and override the built-in ones.
    pub fn parse(file: &str, text: &str) -> Result<Config, ConfigError> {
        decode(file, text)?
            .resolve(Some(Config::default().bindings))
            .map_err(|problem| ConfigError::plain(file, problem))
    }
}

/// What's wrong with a config file.
#[derive(Debug)]
pub struct ConfigError {
    /// The first problem on one line, for the status bar: the file's name,
    /// the line if there is one, and what's wrong there.
    pub summary: String,
    /// Every problem, with the lines at fault.
    report: String,
}

impl ConfigError {
    /// A problem that isn't at any one place in the file.
    fn plain(file: &str, problem: impl fmt::Display) -> Self {
        Self {
            summary: format!("{}: {problem}", file_name(file)),
            report: format!("{file}: {problem}"),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.report)
    }
}

impl std::error::Error for ConfigError {}

fn file_name(file: &str) -> &str {
    Path::new(file)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(file)
}

/// One of knus's errors, to be shown against the file's text. On its own
/// it has no text to quote; and knus's own top-level error only says
/// "error parsing KDL" above the list of them.
struct InFile<'a> {
    problem: &'a dyn Diagnostic,
    text: &'a dyn SourceCode,
}

impl fmt::Debug for InFile<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(self.problem, f)
    }
}

impl fmt::Display for InFile<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(self.problem, f)
    }
}

impl std::error::Error for InFile<'_> {}

impl Diagnostic for InFile<'_> {
    fn help<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        self.problem.help()
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        self.problem.labels()
    }

    fn source_code(&self) -> Option<&dyn SourceCode> {
        Some(self.text)
    }
}

/// Decodes config `text` without looking names up.
fn decode(file: &str, text: &str) -> Result<RawConfig, ConfigError> {
    knus::parse(file, text).map_err(|e| {
        let problems: Vec<&dyn Diagnostic> = e.related().into_iter().flatten().collect();
        let Some(first) = problems.first() else {
            return ConfigError::plain(file, e.to_string());
        };
        let line = first
            .labels()
            .and_then(|mut labels| labels.next())
            .map(|label| {
                let before = &text.as_bytes()[..label.offset().min(text.len())];
                before.iter().filter(|&&b| b == b'\n').count() + 1
            });
        let mut summary = match line {
            Some(line) => format!("{}:{line}: {first}", file_name(file)),
            None => format!("{}: {first}", file_name(file)),
        };
        if problems.len() > 1 {
            write!(summary, " (and {} more)", problems.len() - 1)
                .expect("writing to memory can't fail");
        }

        // Each problem with the lines at fault, as plain text: it goes to
        // the server's log and to terminals.
        let handler = GraphicalReportHandler::new_themed(GraphicalTheme::unicode_nocolor());
        let mut report = format!("{file} has errors:\n");
        for problem in &problems {
            let shown = InFile {
                problem: *problem,
                text: e.source_code().unwrap_or(&text),
            };
            if handler.render_report(&mut report, &shown).is_err() {
                writeln!(report, "  {problem}").expect("writing to memory can't fail");
            }
        }
        ConfigError {
            summary,
            report: report.trim_end().to_owned(),
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
            .map_err(|e| DecodeError::unexpected(&node.node_name, "key", e))?;
        only_a_name(node, ctx, "a binding is a key and, in braces, its action");
        let children = node.children.as_ref().map_or(&[][..], |c| &c[..]);
        let [child] = children else {
            return Err(DecodeError::missing(
                node,
                "a binding needs one action, as in `h { focus-column-left; }`",
            ));
        };
        if &**child.node_name == "unbind" {
            only_a_name(child, ctx, "unbind takes nothing");
            if let Some(children) = &child.children {
                ctx.emit_error(DecodeError::unexpected(
                    children,
                    "block",
                    "unbind takes nothing",
                ));
            }
            return Ok(RawBind { key, action: None });
        }
        let action = Action::decode_node(child, ctx)?;
        Ok(RawBind {
            key,
            action: Some(action),
        })
    }
}

/// Reports anything on `node` besides its name and children: a type,
/// arguments or properties.
fn only_a_name<S: ErrorSpan>(node: &SpannedNode<S>, ctx: &mut Context<S>, message: &str) {
    if let Some(type_name) = &node.type_name {
        ctx.emit_error(DecodeError::unexpected(type_name, "type", message));
    }
    for argument in &node.arguments {
        ctx.emit_error(DecodeError::unexpected(
            &argument.literal,
            "argument",
            message,
        ));
    }
    for name in node.properties.keys() {
        ctx.emit_error(DecodeError::unexpected(name, "property", message));
    }
}

impl RawBinds {
    /// Applies these bindings on top of `table`.
    fn apply_to(self, table: &mut Table, section: &str) -> Result<(), String> {
        let mut seen = HashSet::new();
        for bind in self.binds {
            if !seen.insert(bind.key) {
                return Err(format!("{section} has {} more than once", bind.key));
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
    /// Looks names up, and lays this config's bindings over `base`: the
    /// built-in ones, or nothing for the config that defines those.
    fn resolve(self, base: Option<Bindings>) -> Result<Config, String> {
        let mut bindings = match (base, self.prefix) {
            (Some(base), None) => base,
            (Some(base), Some(ConfigKey(prefix))) => Bindings { prefix, ..base },
            (None, Some(ConfigKey(prefix))) => Bindings {
                prefix,
                prefix_binds: Table::default(),
                binds: Table::default(),
                overview_binds: Table::default(),
            },
            (None, None) => return Err("no prefix is set".to_owned()),
        };
        let prefix = bindings.prefix;
        for (binds, table, section) in [
            (
                self.prefix_binds,
                &mut bindings.prefix_binds,
                "prefix-binds",
            ),
            (self.binds, &mut bindings.binds, "binds"),
            (
                self.overview_binds,
                &mut bindings.overview_binds,
                "overview-binds",
            ),
        ] {
            // The prefix key always starts a prefix binding (or, pressed
            // twice, is typed), so it can't do anything else.
            if (binds.binds.iter()).any(|bind| bind.key == prefix && bind.action.is_some()) {
                return Err(format!("{section} binds {prefix}, which is the prefix key"));
            }
            binds.apply_to(table, section)?;
            // A built-in binding it takes over goes quietly.
            table.set(prefix, None);
        }

        let mut defined: Vec<(String, Theme)> = Vec::new();
        for raw in self.themes {
            let taken =
                Theme::named(&raw.name).is_some() || defined.iter().any(|(n, _)| *n == raw.name);
            if taken {
                return Err(format!("there's already a theme named {:?}", raw.name));
            }
            // Themes can build on built-in ones, or ones defined above.
            let base = match &raw.based_on {
                None => Theme::default(),
                Some(name) => lookup(name, &defined)
                    .ok_or_else(|| unknown_theme("based-on", name, &defined))?,
            };
            let name = raw.name.clone();
            defined.push((name, raw.apply_to(base)));
        }
        let theme = match &self.theme {
            None => Theme::default(),
            Some(name) => {
                lookup(name, &defined).ok_or_else(|| unknown_theme("theme", name, &defined))?
            }
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

fn unknown_theme(setting: &str, name: &str, defined: &[(String, Theme)]) -> String {
    let names: Vec<&str> = (Theme::ALL.iter().map(|(n, _)| *n))
        .chain(defined.iter().map(|(n, _)| n.as_str()))
        .collect();
    format!(
        "{setting} {name:?} isn't a theme; there's {}",
        names.join(", ")
    )
}

/// A key as written in the config's `prefix`, as in "Ctrl+a".
#[derive(Debug, Clone, Copy)]
struct ConfigKey(Key);

impl<S: ErrorSpan> DecodeScalar<S> for ConfigKey {
    fn type_check(type_name: &Option<Spanned<TypeName, S>>, ctx: &mut Context<S>) {
        no_type_name(type_name.as_ref(), ctx, "key");
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
        no_type_name(type_name.as_ref(), ctx, "color");
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
        no_type_name(type_name.as_ref(), ctx, "color");
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
    type_name: Option<&Spanned<TypeName, S>>,
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
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
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

    fn parse(text: &str) -> Result<Config, ConfigError> {
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
        // Nothing but a key and an action.
        for extra in [
            "binds { Alt+h foo=1 { detach; }; }",
            "binds { (ty)Alt+h { detach; }; }",
            "binds { Alt+h 1 { detach; }; }",
            "binds { Alt+h { unbind 1; }; }",
            "binds { Alt+h { unbind { detach; }; }; }",
        ] {
            assert!(parse(extra).is_err(), "{extra}");
        }
    }

    #[test]
    fn the_prefix_key_does_nothing_else() {
        // Taking a key the defaults bind takes it from them.
        let b = parse(r#"prefix "Alt+o""#).unwrap().bindings;
        assert_eq!(b.binds.get(key("Alt+o")), None);
        // Binding it yourself is a mistake.
        let err = parse("prefix \"Alt+o\"\nbinds { Alt+o { detach; }; }").unwrap_err();
        assert_eq!(
            err.summary,
            "config.kdl: binds binds Alt+o, which is the prefix key"
        );
        // Unbinding it is fine, if unneeded.
        assert!(parse("prefix \"Alt+o\"\nbinds { Alt+o { unbind; }; }").is_ok());
    }

    #[test]
    fn colors_are_six_hex_digits() {
        assert_eq!(parse_hex("#0a1B2c"), Some(Color::Rgb(0x0a, 0x1b, 0x2c)));
        for bad in ["#+12345", "#12345", "#1234567", "123456", "#12345g"] {
            assert_eq!(parse_hex(bad), None, "{bad}");
        }
    }

    #[test]
    fn errors_have_a_line_for_the_status_bar_and_a_full_report() {
        let err = Config::parse(
            "/home/me/.config/tiri/config.kdl",
            "theme \"oxide\"\ndefine-theme \"x\" {\n    dim \"red\"\n    status-fg 300\n}\n",
        )
        .unwrap_err();
        assert_eq!(
            err.summary,
            "config.kdl:3: colors are written \"#rrggbb\" (and 1 more)"
        );
        let report = err.to_string();
        assert!(
            report.starts_with("/home/me/.config/tiri/config.kdl has errors:\n"),
            "{report}"
        );
        // Both problems, each quoting its line, and none of knus's wrapping.
        assert!(
            report.contains("dim \"red\"") && report.contains("status-fg 300"),
            "{report}"
        );
        assert!(
            !report.contains("error parsing KDL") && !report.contains("Error:"),
            "{report}"
        );
        assert!(!report.ends_with('\n'));

        let err = Config::parse("/etc/config.kdl", "theme \"nope\"").unwrap_err();
        assert_eq!(
            err.summary,
            "config.kdl: theme \"nope\" isn't a theme; there's default, oxide"
        );
        assert!(err.to_string().starts_with("/etc/config.kdl: theme"));
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

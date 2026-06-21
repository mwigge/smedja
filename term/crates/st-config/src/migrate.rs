//! `WezTerm` → smedja-term config migration.
//!
//! Evaluates a `WezTerm` Lua config file with a stubbed `wezterm` API and
//! converts the captured field assignments into a smedja-term TOML config
//! string.

use std::fmt::Write as _;

use mlua::{Lua, Table, Value};

// ── Public types ──────────────────────────────────────────────────────────────

/// Output of a successful migration run.
#[derive(Debug)]
pub struct MigrationResult {
    /// Generated smedja-term TOML config string.
    pub toml: String,
    /// Fields that could not be mapped (human-readable descriptions).
    pub unsupported: Vec<String>,
    /// Human-readable migration summary.
    pub summary: String,
}

/// Errors that can occur during migration.
#[derive(Debug)]
pub enum MigrationError {
    /// A Lua evaluation error.
    Lua(mlua::Error),
    /// The Lua source evaluated successfully but returned no config table.
    NoConfigFound,
}

impl std::fmt::Display for MigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lua(e) => write!(f, "Lua error: {e}"),
            Self::NoConfigFound => write!(f, "no config table returned from WezTerm Lua source"),
        }
    }
}

impl std::error::Error for MigrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Lua(e) => Some(e),
            Self::NoConfigFound => None,
        }
    }
}

impl From<mlua::Error> for MigrationError {
    fn from(e: mlua::Error) -> Self {
        Self::Lua(e)
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Evaluate a `WezTerm` Lua config source string and produce a smedja-term
/// TOML config with a list of unsupported field names and a human-readable
/// summary.
///
/// # Errors
///
/// Returns [`MigrationError::Lua`] when the Lua source cannot be evaluated and
/// [`MigrationError::NoConfigFound`] when the source returns nothing or a
/// non-table value.
pub fn migrate_wezterm_config(lua_source: &str) -> Result<MigrationResult, MigrationError> {
    let lua = Lua::new();

    // ── Register stubbed wezterm global ──────────────────────────────────────
    register_wezterm_stubs(&lua)?;

    // ── Evaluate the Lua source ───────────────────────────────────────────────
    let config_val: Value = lua.load(lua_source).eval()?;
    let Value::Table(config_table) = config_val else {
        return Err(MigrationError::NoConfigFound);
    };

    // ── Map captured fields → TOML lines ─────────────────────────────────────
    build_migration_result(&config_table)
}

// ── Wezterm API stubs ─────────────────────────────────────────────────────────

fn register_wezterm_stubs(lua: &Lua) -> Result<(), MigrationError> {
    let globals = lua.globals();
    let wezterm: Table = lua.create_table()?;

    // wezterm.config_builder() — returns an empty table that Lua code fills in.
    let config_builder = lua.create_function(|lua, ()| lua.create_table())?;
    wezterm.set("config_builder", config_builder)?;

    // wezterm.font(name) — returns { family = name }
    let font_fn = lua.create_function(|lua, name: String| {
        let t = lua.create_table()?;
        t.set("family", name)?;
        Ok(t)
    })?;
    wezterm.set("font", font_fn)?;

    // wezterm.font_with_fallback(names) — returns { family = names[1], fallback = names[2..] }
    let font_with_fallback = lua.create_function(|lua, names: Table| {
        let t = lua.create_table()?;
        let mut iter = names.sequence_values::<String>();
        if let Some(primary) = iter.next() {
            t.set("family", primary?)?;
        }
        let fallback: Table = lua.create_table()?;
        for (idx, v) in (1i64..).zip(iter) {
            fallback.set(idx, v?)?;
        }
        t.set("fallback", fallback)?;
        Ok(t)
    })?;
    wezterm.set("font_with_fallback", font_with_fallback)?;

    // wezterm.color sub-table
    let color_table: Table = lua.create_table()?;
    let get_builtin_schemes = lua.create_function(|lua, ()| lua.create_table())?;
    color_table.set("get_builtin_schemes", get_builtin_schemes)?;
    wezterm.set("color", color_table)?;

    // Also register as a global so `local wezterm = require 'wezterm'` resolves.
    globals.set("wezterm", wezterm.clone())?;

    // Register in package.preload so `require 'wezterm'` returns our stub table.
    let package: Table = globals.get("package")?;
    let preload: Table = package.get("preload")?;
    let wezterm_loader = lua.create_function(move |_, ()| Ok(wezterm.clone()))?;
    preload.set("wezterm", wezterm_loader)?;

    Ok(())
}

// ── Field mapping ─────────────────────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
fn build_migration_result(config: &Table) -> Result<MigrationResult, MigrationError> {
    let mut toml_lines: Vec<String> = Vec::new();
    let mut todo_comments: Vec<String> = Vec::new();
    let mut unsupported: Vec<String> = Vec::new();

    // Track which sections we need.
    let mut font_lines: Vec<String> = Vec::new();
    let mut window_lines: Vec<String> = Vec::new();
    let mut root_lines: Vec<String> = Vec::new();
    let mut launch_menu_blocks: Vec<String> = Vec::new();

    for pair in config.clone().pairs::<String, Value>() {
        let (key, val) = pair?;
        match key.as_str() {
            "font_size" => {
                if let Value::Number(n) = val {
                    font_lines.push(format!("size = {n:.1}"));
                }
            }
            "font" => {
                if let Value::Table(ft) = val {
                    if let Ok(family) = ft.get::<String>("family") {
                        font_lines.push(format!("family = {family:?}"));
                    }
                    // Collect fallback entries.
                    if let Ok(Value::Table(fb)) = ft.get::<Value>("fallback") {
                        let mut fallbacks: Vec<String> = Vec::new();
                        for f in fb.sequence_values::<String>().flatten() {
                            fallbacks.push(format!("{f:?}"));
                        }
                        if !fallbacks.is_empty() {
                            font_lines.push(format!("fallback = [{}]", fallbacks.join(", ")));
                        }
                    }
                }
            }
            "color_scheme" => {
                let scheme_name = match val {
                    Value::String(s) => s
                        .to_str()
                        .map_or_else(|_| "unknown".to_owned(), |b| b.to_owned()),
                    _ => "unknown".to_owned(),
                };
                let msg =
                    format!("color_scheme = {scheme_name:?} (color scheme DB not yet bundled)");
                unsupported.push(format!("color_scheme: {scheme_name:?}"));
                todo_comments.push(format!("# TODO: {msg}"));
            }
            "window_background_opacity" => {
                if let Value::Number(n) = val {
                    window_lines.push(format!("background_opacity = {n:.2}"));
                }
            }
            "scrollback_lines" => {
                if let Value::Integer(n) = val {
                    root_lines.push(format!("scrollback_lines = {n}"));
                } else if let Value::Number(n) = val {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    root_lines.push(format!("scrollback_lines = {}", n as u64));
                }
            }
            "tab_bar_at_bottom" => {
                let msg = "tab_bar_at_bottom (tab bar position not yet configurable)";
                unsupported.push(msg.to_owned());
                todo_comments.push(format!("# TODO: {msg}"));
            }
            "keys" => {
                if let Value::Table(keys_table) = val {
                    for kv in keys_table.sequence_values::<Value>() {
                        if let Ok(Value::Table(binding)) = kv {
                            let key_str = binding
                                .get::<String>("key")
                                .unwrap_or_else(|_| "?".to_owned());
                            let mods = binding
                                .get::<String>("mods")
                                .unwrap_or_else(|_| String::new());
                            let msg = if mods.is_empty() {
                                format!("key binding: {key_str}")
                            } else {
                                format!("key binding: {mods}|{key_str}")
                            };
                            unsupported.push(msg.clone());
                            todo_comments.push(format!("# TODO: {msg}"));
                        }
                    }
                }
            }
            "launch_menu" => {
                if let Value::Table(menu) = val {
                    for entry_val in menu.sequence_values::<Value>() {
                        if let Ok(Value::Table(entry)) = entry_val {
                            let label = entry
                                .get::<String>("label")
                                .unwrap_or_else(|_| String::new());
                            let mut block = String::from("[[launch_menu]]");
                            block.push('\n');
                            let _ = writeln!(block, "label = {label:?}");
                            if let Ok(Value::Table(args)) = entry.get::<Value>("args") {
                                let mut args_vec: Vec<String> = Vec::new();
                                for a in args.sequence_values::<String>().flatten() {
                                    args_vec.push(format!("{a:?}"));
                                }
                                let _ = writeln!(block, "args = [{}]", args_vec.join(", "));
                            }
                            launch_menu_blocks.push(block);
                        }
                    }
                }
            }
            other => {
                let msg = format!("{other} (no mapping defined)");
                unsupported.push(msg.clone());
                todo_comments.push(format!("# TODO: {msg}"));
            }
        }
    }

    // ── Assemble the TOML string ──────────────────────────────────────────────
    // Root-level keys must come before any section header.
    if !root_lines.is_empty() {
        toml_lines.extend(root_lines);
        toml_lines.push(String::new());
    }

    if !font_lines.is_empty() {
        toml_lines.push("[font]".to_owned());
        toml_lines.extend(font_lines);
        toml_lines.push(String::new());
    }

    if !window_lines.is_empty() {
        toml_lines.push("[window]".to_owned());
        toml_lines.extend(window_lines);
        toml_lines.push(String::new());
    }

    if !launch_menu_blocks.is_empty() {
        toml_lines.push(String::new());
        for block in &launch_menu_blocks {
            toml_lines.push(block.clone());
        }
    }

    if !todo_comments.is_empty() {
        toml_lines.push(String::new());
        toml_lines.push("# --- Unsupported fields ---".to_owned());
        toml_lines.extend(todo_comments);
    }

    let toml = toml_lines.join("\n").trim_start_matches('\n').to_owned();

    // ── Build summary ─────────────────────────────────────────────────────────
    let total_keys: usize = config.clone().pairs::<String, Value>().count();
    let mapped_count = total_keys.saturating_sub(unsupported.len());

    let summary = format!(
        "Migration complete: {} field(s) mapped, {} unsupported.",
        mapped_count,
        unsupported.len()
    );

    Ok(MigrationResult {
        toml,
        unsupported,
        summary,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_WEZTERM_LUA: &str = r"
local wezterm = require 'wezterm'
local config = wezterm.config_builder()
config.font_size = 14.0
config.font = wezterm.font('JetBrains Mono')
config.window_background_opacity = 0.95
config.scrollback_lines = 10000
config.launch_menu = {
  { label = 'Fish Shell', args = { '/usr/bin/fish' } },
}
return config
";

    #[test]
    fn migrate_sample_config_parses_cleanly() {
        let result = migrate_wezterm_config(SAMPLE_WEZTERM_LUA).unwrap();
        let parsed: toml::Value = toml::from_str(&result.toml).unwrap();
        assert!(
            (parsed["font"]["size"].as_float().unwrap() - 14.0).abs() < f64::EPSILON,
            "font.size should be 14.0"
        );
        assert_eq!(
            parsed["scrollback_lines"].as_integer().unwrap(),
            10_000,
            "scrollback_lines should be 10000"
        );
    }

    #[test]
    fn migrate_unsupported_field_appears_in_unsupported_list() {
        let lua = r"
local wezterm = require 'wezterm'
local config = wezterm.config_builder()
config.color_scheme = 'Dracula'
return config
";
        let result = migrate_wezterm_config(lua).unwrap();
        assert!(
            result
                .unsupported
                .iter()
                .any(|s| s.contains("color_scheme")),
            "color_scheme should appear in unsupported list"
        );
    }

    #[test]
    fn migrate_empty_config_produces_valid_toml() {
        let lua = r"
local wezterm = require 'wezterm'
local config = wezterm.config_builder()
return config
";
        let result = migrate_wezterm_config(lua).unwrap();
        // An empty config must still parse as valid TOML (even if empty string).
        let parsed = toml::from_str::<toml::Value>(&result.toml);
        assert!(parsed.is_ok(), "empty migration should produce valid TOML");
    }

    #[test]
    fn migration_result_summary_not_empty() {
        let result = migrate_wezterm_config(SAMPLE_WEZTERM_LUA).unwrap();
        assert!(!result.summary.is_empty(), "summary must not be empty");
    }
}

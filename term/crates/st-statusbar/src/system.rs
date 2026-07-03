//! Host/system-derived modules: detected language, local time, and battery.

use std::path::Path;

use chrono::Local;

use crate::types::plain_segment;
use crate::{ModuleContext, Segment, StatusModule};

/// Detects the primary language of the current working directory.
///
/// Priority: Rust > Node > Go > Python.
pub struct LanguageModule;

impl LanguageModule {
    /// Evaluate against an explicit directory (useful for testing).
    #[must_use]
    pub fn evaluate_in(&self, _ctx: &ModuleContext, cwd: &Path) -> Option<Segment> {
        let checks: &[(&str, &str)] = &[
            ("Cargo.toml", "\u{1f980} Rust"),
            ("package.json", "\u{2b21} Node"),
            ("go.mod", "\u{1f439} Go"),
            ("pyproject.toml", "\u{1f40d} Python"),
            ("setup.py", "\u{1f40d} Python"),
        ];
        for (file, label) in checks {
            if cwd.join(file).exists() {
                return Some(plain_segment("language", *label));
            }
        }
        None
    }
}

impl StatusModule for LanguageModule {
    fn name(&self) -> &'static str {
        "language"
    }

    fn evaluate(&self, ctx: &ModuleContext) -> Option<Segment> {
        let cwd = std::env::current_dir().ok()?;
        self.evaluate_in(ctx, &cwd)
    }
}

/// Displays the current local time in `HH:MM` format using raw UTC arithmetic.
///
/// This module always returns `Some`.
pub struct TimeModule;

impl StatusModule for TimeModule {
    fn name(&self) -> &'static str {
        "time"
    }

    fn evaluate(&self, _ctx: &ModuleContext) -> Option<Segment> {
        let now = Local::now();
        Some(plain_segment("time", now.format("%H:%M").to_string()))
    }
}

/// Displays battery level from `/sys/class/power_supply/BAT0/`.
///
/// Returns `None` on systems without a battery (e.g. desktops, CI runners).
pub struct BatteryModule;

impl StatusModule for BatteryModule {
    fn name(&self) -> &'static str {
        "battery"
    }

    fn evaluate(&self, _ctx: &ModuleContext) -> Option<Segment> {
        let capacity_path = Path::new("/sys/class/power_supply/BAT0/capacity");
        let status_path = Path::new("/sys/class/power_supply/BAT0/status");
        if !capacity_path.exists() {
            return None;
        }
        let capacity = std::fs::read_to_string(capacity_path)
            .ok()?
            .trim()
            .to_owned();
        let status = std::fs::read_to_string(status_path)
            .ok()
            .unwrap_or_default();
        let symbol = if status.trim() == "Charging" {
            "\u{26a1}"
        } else {
            "\u{1f50b}"
        };
        Some(plain_segment("battery", format!("{symbol} {capacity}%")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::make_ctx;

    // 4
    #[test]
    fn time_module_returns_hh_mm_format() {
        let ctx = make_ctx();
        let seg = TimeModule
            .evaluate(&ctx)
            .expect("TimeModule always returns Some");
        let text = &seg.text;
        assert_eq!(text.len(), 5, "expected HH:MM (5 chars), got '{text}'");
        assert_eq!(
            text.chars().nth(2),
            Some(':'),
            "colon must be at position 2"
        );
        for (i, ch) in text.chars().enumerate() {
            if i != 2 {
                assert!(
                    ch.is_ascii_digit(),
                    "char at {i} must be a digit, got '{ch}'"
                );
            }
        }
    }

    // 6
    #[test]
    fn language_module_detects_rust_from_cargo_toml() {
        // Create a temp dir with a Cargo.toml file to trigger Rust detection.
        let tmp = std::env::temp_dir().join(format!(
            "smedja-test-lang-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).expect("create tmp dir");
        std::fs::write(tmp.join("Cargo.toml"), "[package]").expect("write Cargo.toml");

        let result = LanguageModule.evaluate_in(&make_ctx(), &tmp);
        std::fs::remove_dir_all(&tmp).ok();

        let seg = result.expect("should detect Rust");
        assert!(
            seg.text.contains("Rust"),
            "expected text to contain 'Rust', got '{}'",
            seg.text
        );
    }

    // 9
    #[test]
    fn battery_module_no_battery_returns_none() {
        // On CI / desktops without BAT0, the module must return None gracefully.
        if Path::new("/sys/class/power_supply/BAT0/capacity").exists() {
            // System has a battery — skip rather than assert (avoids false failures on laptops).
            return;
        }
        assert!(BatteryModule.evaluate(&make_ctx()).is_none());
    }
}

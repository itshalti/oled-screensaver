//! User settings from `~/.config/ss-for-oled/config.toml`. A missing file or key means the default.

use std::path::PathBuf;

use serde::Deserialize;

/// Written by `ss-for-oled install`; its values are exactly the defaults (checked by a test).
pub const TEMPLATE: &str = r#"# ss-for-oled settings. Changes are picked up automatically.

# Seconds without mouse/keyboard input before the screensaver starts.
# Video players and games that block idle keep it from starting.
timeout = 600

# Monitor to show it on (names: `hyprctl monitors`, `wlr-randr`).
output = "DP-1"

# Modes to rotate through; empty = all.
# Topo, Starfield, Aurora, Lissajous, Waves, Clock, Pipes, Sandbox
modes = []

# Minutes per mode before switching to another.
mode_minutes = 5

# Frame cap; 0 = monitor refresh rate (smoothest).
fps = 0
"#;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub timeout: u32,
    pub output: String,
    pub modes: Vec<String>,
    pub mode_minutes: f32,
    pub fps: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self { timeout: 600, output: "DP-1".into(), modes: Vec::new(), mode_minutes: 5.0, fps: 0.0 }
    }
}

pub fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
}

pub fn path() -> PathBuf {
    config_home().join("ss-for-oled/config.toml")
}

/// Current settings; problems are reported on stderr (the journal) and fall back to defaults.
pub fn load() -> Config {
    std::fs::read_to_string(path()).map(|s| parse(&s)).unwrap_or_default()
}

pub fn parse(s: &str) -> Config {
    let mut c: Config = toml::from_str(s).unwrap_or_else(|e| {
        eprintln!("{}: {e}using defaults", path().display());
        Config::default()
    });
    c.modes.retain(|m| {
        let ok = crate::render::mode_index(m).is_some();
        if !ok {
            eprintln!("config: unknown mode {m:?} ignored");
        }
        ok
    });
    c.timeout = c.timeout.max(1);
    c.mode_minutes = c.mode_minutes.max(0.25); // shorter would cut the fades
    c.fps = c.fps.max(0.0);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_is_the_defaults() {
        assert_eq!(parse(TEMPLATE), Config::default());
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let c = parse("timeout = 30\nmodes = [\"topo\", \"Nope\", \"Clock\"]");
        assert_eq!(c.timeout, 30);
        assert_eq!(c.output, "DP-1");
        assert_eq!(c.modes, ["topo", "Clock"], "unknown names dropped, case ignored");
    }

    #[test]
    fn readme_config_examples_are_valid() {
        let readme = include_str!("../README.md");
        let blocks: Vec<&str> = readme.split("```toml\n").skip(1).map(|b| b.split("```").next().unwrap()).collect();
        assert!(blocks.len() >= 5);
        for b in blocks {
            let c: Config = toml::from_str(b).unwrap_or_else(|e| panic!("README example does not parse:\n{b}\n{e}"));
            assert_eq!(parse(b).modes.len(), c.modes.len(), "unknown mode name in README example:\n{b}");
        }
    }

    #[test]
    fn broken_or_silly_values_fall_back() {
        assert_eq!(parse("timeout = \"ten\""), Config::default());
        assert_eq!(parse("timout = 5"), Config::default(), "typo in a key is reported, not ignored");
        let c = parse("timeout = 0\nmode_minutes = 0\nfps = -5");
        assert_eq!((c.timeout, c.mode_minutes, c.fps), (1, 0.25, 0.0));
    }
}

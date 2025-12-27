// Copyright 2025 Tomoki Hayashi
// MIT License (https://opensource.org/licenses/MIT)

//! Configuration management.
//!
//! Config values are loaded with the following priority (highest to lowest):
//! 1. Environment variables (SVT_*)
//! 2. Config file (~/.config/svt/config.toml)
//! 3. Default values

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub nav_latch_ms: u64,
    pub force_alt_screen: bool,
    pub no_alt_screen: bool,
    pub render_cache_size: usize,
    pub prefetch_count: usize,
    pub debug: bool,
    pub kgp_no_compress: bool,
    pub compress_level: u32,
    pub tmux_kitty_max_pixels: u64,
    pub trace_worker: bool,
    pub cell_aspect_ratio: f64,
    pub resize_filter: String,
    pub tile_filter: String,
    pub prefetch_threads: usize,
    pub tile_threads: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            nav_latch_ms: 150,
            force_alt_screen: false,
            no_alt_screen: false,
            render_cache_size: 100,
            prefetch_count: 5,
            debug: false,
            kgp_no_compress: false,
            compress_level: 6,
            tmux_kitty_max_pixels: 1_500_000,
            trace_worker: false,
            cell_aspect_ratio: 2.0,
            resize_filter: "triangle".to_string(),
            tile_filter: "nearest".to_string(),
            prefetch_threads: 2,
            tile_threads: 4,
        }
    }
}

/// Parse filter type string to image::imageops::FilterType.
/// Returns Triangle as fallback for invalid values.
pub fn parse_filter_type(s: &str) -> image::imageops::FilterType {
    let s = s.trim();
    if s.eq_ignore_ascii_case("nearest") {
        image::imageops::FilterType::Nearest
    } else if s.eq_ignore_ascii_case("triangle") {
        image::imageops::FilterType::Triangle
    } else if s.eq_ignore_ascii_case("catmullrom") || s.eq_ignore_ascii_case("catmull-rom") {
        image::imageops::FilterType::CatmullRom
    } else if s.eq_ignore_ascii_case("gaussian") {
        image::imageops::FilterType::Gaussian
    } else if s.eq_ignore_ascii_case("lanczos3") || s.eq_ignore_ascii_case("lanczos") {
        image::imageops::FilterType::Lanczos3
    } else {
        image::imageops::FilterType::Triangle
    }
}

impl Config {
    /// Load config with priority: env vars > config file > defaults
    pub fn load() -> Self {
        let mut config = Self::load_from_file().unwrap_or_default();
        config.apply_env_overrides();
        config.clamp_values();
        config
    }

    fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("svt").join("config.toml"))
    }

    fn load_from_file() -> Option<Self> {
        let path = Self::config_path()?;
        let content = std::fs::read_to_string(path).ok()?;
        toml::from_str(&content).ok()
    }

    fn apply_env_overrides(&mut self) {
        if let Some(v) = Self::parse_env::<u64>("SVT_NAV_LATCH_MS") {
            self.nav_latch_ms = v;
        }
        if std::env::var_os("SVT_FORCE_ALT_SCREEN").is_some() {
            self.force_alt_screen = true;
        }
        if std::env::var_os("SVT_NO_ALT_SCREEN").is_some() {
            self.no_alt_screen = true;
        }
        if let Some(v) = Self::parse_env::<usize>("SVT_RENDER_CACHE_SIZE") {
            self.render_cache_size = v;
        }
        if let Some(v) = Self::parse_env::<usize>("SVT_PREFETCH_COUNT") {
            self.prefetch_count = v;
        }
        if std::env::var_os("SVT_DEBUG").is_some() {
            self.debug = true;
        }
        if std::env::var_os("SVT_KGP_NO_COMPRESS").is_some() {
            self.kgp_no_compress = true;
        }
        if let Some(v) = Self::parse_env::<u32>("SVT_COMPRESS_LEVEL") {
            self.compress_level = v;
        }
        if let Some(v) = Self::parse_env::<u64>("SVT_TMUX_KITTY_MAX_PIXELS") {
            self.tmux_kitty_max_pixels = v;
        }
        if std::env::var_os("SVT_TRACE_WORKER").is_some() {
            self.trace_worker = true;
        }
        if let Some(v) = Self::parse_env::<f64>("SVT_CELL_ASPECT_RATIO") {
            self.cell_aspect_ratio = v;
        }
        if let Ok(v) = std::env::var("SVT_RESIZE_FILTER") {
            self.resize_filter = v;
        }
        if let Ok(v) = std::env::var("SVT_TILE_FILTER") {
            self.tile_filter = v;
        }
        if let Some(v) = Self::parse_env::<usize>("SVT_PREFETCH_THREADS") {
            self.prefetch_threads = v;
        }
        if let Some(v) = Self::parse_env::<usize>("SVT_TILE_THREADS") {
            self.tile_threads = v;
        }
    }

    fn clamp_values(&mut self) {
        const MAX_NAV_LATCH_MS: u64 = 5_000;
        const MAX_RENDER_CACHE_SIZE: usize = 500;
        const MAX_COMPRESS_LEVEL: u32 = 9;

        self.nav_latch_ms = self.nav_latch_ms.min(MAX_NAV_LATCH_MS);
        self.render_cache_size = self.render_cache_size.clamp(1, MAX_RENDER_CACHE_SIZE);
        self.compress_level = self.compress_level.min(MAX_COMPRESS_LEVEL);
        self.cell_aspect_ratio = self.cell_aspect_ratio.clamp(1.0, 4.0);
        self.prefetch_threads = self.prefetch_threads.clamp(1, 8);
        self.tile_threads = self.tile_threads.clamp(1, 8);
    }

    fn parse_env<T: std::str::FromStr>(key: &str) -> Option<T> {
        std::env::var(key).ok()?.parse().ok()
    }

    pub fn compression_level(&self) -> Option<u32> {
        if self.kgp_no_compress {
            None
        } else {
            Some(self.compress_level)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_values() {
        let config = Config::default();
        assert_eq!(config.nav_latch_ms, 150);
        assert_eq!(config.render_cache_size, 100);
        assert_eq!(config.prefetch_count, 5);
        assert_eq!(config.compress_level, 6);
        assert_eq!(config.tmux_kitty_max_pixels, 1_500_000);
        assert!(!config.force_alt_screen);
        assert!(!config.debug);
        assert_eq!(config.cell_aspect_ratio, 2.0);
    }

    #[test]
    fn test_clamp_values() {
        let mut config = Config {
            nav_latch_ms: 10_000,
            render_cache_size: 1000,
            compress_level: 20,
            ..Default::default()
        };
        config.clamp_values();
        assert_eq!(config.nav_latch_ms, 5_000);
        assert_eq!(config.render_cache_size, 500);
        assert_eq!(config.compress_level, 9);
    }

    #[test]
    fn test_compression_level() {
        let config = Config::default();
        assert_eq!(config.compression_level(), Some(6));

        let config = Config {
            kgp_no_compress: true,
            ..Default::default()
        };
        assert_eq!(config.compression_level(), None);
    }
}

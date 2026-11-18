//! TOML configuration loading, defaults, validation and path resolution.
//!
//! # Defaulting policy (spec §8)
//!
//! Configuration is read fresh on each invocation. A missing file, a missing
//! section, or an omitted key is **not** an error: it falls back to the
//! documented default (this is required so a minimal config works, ticket #2).
//! In particular there are no mandatory keys whose mere absence fails loading.
//!
//! A value that *is* present but malformed is an error, and the error names the
//! full dotted key (e.g. `tts.base_url`) so the user can fix it. These errors
//! are raised before any selection, network or audio work begins. Values that
//! are present but semantically invalid (empty `tts.model`, non-finite
//! `tts.speed`, out-of-range `audio.volume`, …) are rejected by [`Config::validate`]
//! with the same key-named wording.

use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;
use serde::de::{self, Deserializer};
use tracing::level_filters::LevelFilter;

/// Problems encountered while locating, reading or validating configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// Neither `XDG_CONFIG_HOME` nor `HOME` is available.
    NoHome,
    /// The config file exists but could not be read.
    Read { path: PathBuf, detail: String },
    /// The config file is not valid TOML or has invalid values.
    Parse { path: PathBuf, detail: String },
    /// A value is present but invalid; the message names the offending key.
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHome => write!(
                f,
                "cannot locate config directory: XDG_CONFIG_HOME and HOME are both unset"
            ),
            Self::Read { path, detail } => {
                write!(f, "cannot read config file {}: {detail}", path.display())
            }
            Self::Parse { path, detail } => {
                write!(f, "invalid config file {}: {detail}", path.display())
            }
            Self::Invalid(message) => write!(f, "{message}"),
        }
    }
}

impl Error for ConfigError {}

/// The only audio format supported by the MVP.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ResponseFormat {
    #[default]
    Mp3,
}

/// Wrap a field's deserialization error so it names the offending dotted key.
///
/// A raw `toml` error reports a line and column but not the key (for example
/// `invalid type: integer `42`, expected a string`). Spec §8 requires a useful
/// error for invalid values, so every configured value goes through one of
/// these small adapters.
macro_rules! keyed_field {
    ($fn_name:ident, $type:ty, $key:literal) => {
        fn $fn_name<'de, D: Deserializer<'de>>(deserializer: D) -> Result<$type, D::Error> {
            <$type>::deserialize(deserializer)
                .map_err(|err| de::Error::custom(format!(concat!($key, ": {}"), err)))
        }
    };
}

keyed_field!(deserialize_base_url, String, "tts.base_url");
keyed_field!(deserialize_tts_model, String, "tts.model");
keyed_field!(deserialize_tts_voice, String, "tts.voice");
keyed_field!(deserialize_tts_speed, f32, "tts.speed");
keyed_field!(deserialize_api_key_env, Option<String>, "tts.api_key_env");
keyed_field!(deserialize_volume, f32, "audio.volume");
keyed_field!(deserialize_max_chars, usize, "selection.max_chars");
keyed_field!(deserialize_timeout_ms, u64, "selection.timeout_ms");
keyed_field!(deserialize_log_level, String, "logging.level");

impl<'de> Deserialize<'de> for ResponseFormat {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)
            .map_err(|err| de::Error::custom(format!("tts.response_format: {err}")))?;
        match value.as_str() {
            "mp3" => Ok(Self::Mp3),
            other => Err(de::Error::custom(format!(
                "unsupported tts.response_format {other:?}; only \"mp3\" is supported"
            ))),
        }
    }
}

/// Selection source. Spec §9 asks for the provider behind a small abstraction
/// "to allow additional extraction methods later"; the config shape therefore
/// stays an enum even though the MVP has a single variant. This is deliberate,
/// not speculative generality.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SelectionProvider {
    #[default]
    WaylandPrimary,
}

impl<'de> Deserialize<'de> for SelectionProvider {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)
            .map_err(|err| de::Error::custom(format!("selection.provider: {err}")))?;
        match value.as_str() {
            "wayland-primary" => Ok(Self::WaylandPrimary),
            other => Err(de::Error::custom(format!(
                "unsupported selection.provider {other:?}; expected \"wayland-primary\""
            ))),
        }
    }
}

/// Settings for the OpenAI-compatible TTS server.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct TtsConfig {
    #[serde(deserialize_with = "deserialize_base_url")]
    pub base_url: String,
    /// Name of the environment variable holding the API key (never the key itself).
    #[serde(deserialize_with = "deserialize_api_key_env")]
    pub api_key_env: Option<String>,
    #[serde(deserialize_with = "deserialize_tts_model")]
    pub model: String,
    #[serde(deserialize_with = "deserialize_tts_voice")]
    pub voice: String,
    #[serde(deserialize_with = "deserialize_tts_speed")]
    pub speed: f32,
    pub response_format: ResponseFormat,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:8880/v1".to_owned(),
            api_key_env: None,
            model: "tts-1".to_owned(),
            voice: "alloy".to_owned(),
            speed: 1.0,
            response_format: ResponseFormat::Mp3,
        }
    }
}

impl TtsConfig {
    /// Resolve the API key from the configured environment variable.
    ///
    /// Returns `Ok(None)` when no variable is configured (servers without
    /// authentication are supported). The key value is never logged.
    pub fn resolve_api_key(&self) -> Result<Option<String>, ConfigError> {
        match &self.api_key_env {
            None => Ok(None),
            Some(name) if name.trim().is_empty() => Err(ConfigError::Invalid(
                "tts.api_key_env must not be empty when set".to_owned(),
            )),
            Some(name) => std::env::var(name)
                .ok()
                .filter(|value| !value.is_empty())
                .map(Some)
                .ok_or_else(|| {
                    ConfigError::Invalid(format!(
                        "tts.api_key_env references environment variable {name:?}, which is not set or is empty"
                    ))
                }),
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        validate_base_url(&self.base_url)?;
        if self.model.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "tts.model must not be empty".to_owned(),
            ));
        }
        if self.voice.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "tts.voice must not be empty".to_owned(),
            ));
        }
        if !self.speed.is_finite() || self.speed <= 0.0 {
            return Err(ConfigError::Invalid(format!(
                "tts.speed must be a positive finite number (got {})",
                self.speed
            )));
        }
        Ok(())
    }
}

/// Audio playback settings.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    #[serde(deserialize_with = "deserialize_volume")]
    pub volume: f32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self { volume: 1.0 }
    }
}

impl AudioConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if !self.volume.is_finite() || !(0.0..=1.0).contains(&self.volume) {
            return Err(ConfigError::Invalid(format!(
                "audio.volume must be between 0.0 and 1.0 (got {})",
                self.volume
            )));
        }
        Ok(())
    }
}

/// PRIMARY-selection settings.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SelectionConfig {
    pub provider: SelectionProvider,
    #[serde(deserialize_with = "deserialize_max_chars")]
    pub max_chars: usize,
    #[serde(deserialize_with = "deserialize_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for SelectionConfig {
    fn default() -> Self {
        Self {
            provider: SelectionProvider::WaylandPrimary,
            max_chars: 20_000,
            timeout_ms: 2_000,
        }
    }
}

impl SelectionConfig {
    /// The configured timeout as a `Duration` — the value used at the domain
    /// boundary by [`crate::selection::WaylandPrimary`]. `timeout_ms` remains a
    /// `u64` only because that is the documented TOML key.
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.timeout_ms)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.max_chars == 0 {
            return Err(ConfigError::Invalid(
                "selection.max_chars must be greater than 0".to_owned(),
            ));
        }
        if self.timeout_ms == 0 {
            return Err(ConfigError::Invalid(
                "selection.timeout_ms must be greater than 0".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Structured-logging settings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// A tracing level filter such as `info`, `debug` or `warn`. Stored as the
    /// documented TOML string and converted once by [`Self::level_filter`];
    /// making the field itself a `LevelFilter` would change the public config
    /// shape and the documented parsing, so the typed boundary is that method.
    #[serde(deserialize_with = "deserialize_log_level")]
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_owned(),
        }
    }
}

impl LoggingConfig {
    /// Parse the configured level into a tracing filter.
    pub fn level_filter(&self) -> Result<LevelFilter, ConfigError> {
        LevelFilter::from_str(&self.level).map_err(|_| {
            ConfigError::Invalid(format!(
                "logging.level must be one of: off, error, warn, info, debug, trace (got {:?})",
                self.level
            ))
        })
    }

    fn validate(&self) -> Result<(), ConfigError> {
        self.level_filter().map(|_| ())
    }
}

/// Complete Murmduck configuration.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Config {
    pub tts: TtsConfig,
    pub audio: AudioConfig,
    pub selection: SelectionConfig,
    pub logging: LoggingConfig,
}

impl Config {
    /// Load configuration from `path`. A missing file yields documented defaults.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = fs::read_to_string(path).map_err(|err| ConfigError::Read {
            path: path.to_path_buf(),
            detail: err.to_string(),
        })?;
        let config: Self = toml::from_str(&contents).map_err(|err| ConfigError::Parse {
            path: path.to_path_buf(),
            detail: err.to_string(),
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Validate all values. Called automatically by [`Config::load`].
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.tts.validate()?;
        self.audio.validate()?;
        self.selection.validate()?;
        self.logging.validate()?;
        Ok(())
    }
}

/// Resolve the config path from explicit environment values.
///
/// `xdg` is `XDG_CONFIG_HOME`; when absent or empty, `home/.config` is used.
pub fn config_path_from(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Result<PathBuf, ConfigError> {
    let base = match xdg.filter(|value| !value.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => {
            let home = home
                .filter(|value| !value.is_empty())
                .ok_or(ConfigError::NoHome)?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("murmduck").join("config.toml"))
}

/// Resolve the config path from the process environment.
pub fn config_path() -> Result<PathBuf, ConfigError> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");
    config_path_from(xdg.as_deref(), home.as_deref())
}

fn validate_base_url(url: &str) -> Result<(), ConfigError> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| {
            ConfigError::Invalid(format!(
                "tts.base_url must start with \"http://\" or \"https://\" (got {url:?})"
            ))
        })?;
    if rest.is_empty() || rest.starts_with('/') || rest.chars().any(char::is_whitespace) {
        return Err(ConfigError::Invalid(format!(
            "tts.base_url must include a host (got {url:?})"
        )));
    }
    Ok(())
}

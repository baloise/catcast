use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const CURRENT_CONFIG_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing required field: version")]
    MissingVersion,
    #[error("unsupported config version: {0} (this build understands {CURRENT_CONFIG_VERSION})")]
    UnsupportedVersion(u32),
    #[error("invalid duration {0:?}: {1}")]
    InvalidDuration(String, humantime::DurationError),
    #[error("invalid cron expression {0:?}: {1}")]
    InvalidCron(String, cron::error::Error),
    #[error("yaml parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub rotation: Rotation,
    #[serde(default)]
    pub fixed: Vec<FixedEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rotation {
    /// Duration string parsable by `humantime` (e.g. `30s`, `2m`).
    /// Falls back to 30s when missing.
    #[serde(default = "default_duration_string")]
    pub default_duration: String,
    #[serde(default)]
    pub urls: Vec<RotationEntry>,
}

impl Default for Rotation {
    fn default() -> Self {
        Self {
            default_duration: default_duration_string(),
            urls: Vec::new(),
        }
    }
}

fn default_duration_string() -> String {
    "30s".to_string()
}

/// Either a bare URL string (uses the rotation's default_duration)
/// or `{ url, duration }` for a per-entry override.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum RotationEntry {
    Short(String),
    Long { url: String, duration: String },
}

impl RotationEntry {
    pub fn url(&self) -> &str {
        match self {
            Self::Short(u) => u,
            Self::Long { url, .. } => url,
        }
    }
    pub fn duration(&self, default: Duration) -> Result<Duration, ConfigError> {
        match self {
            Self::Short(_) => Ok(default),
            Self::Long { duration, .. } => parse_duration(duration),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FixedEntry {
    /// 5- or 6-field cron expression accepted by the `cron` crate.
    pub cron: String,
    pub url: String,
    /// Duration string parsable by `humantime`.
    pub duration: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Warning(pub String);

impl Config {
    pub fn from_yaml(s: &str) -> Result<Self, ConfigError> {
        let raw: serde_yaml::Value = serde_yaml::from_str(s)?;
        // Surface a friendlier error than serde's "missing field `version`"
        // for the most common authoring mistake.
        if raw.get("version").is_none() {
            return Err(ConfigError::MissingVersion);
        }
        let cfg: Config = serde_yaml::from_value(raw)?;
        if cfg.version != CURRENT_CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion(cfg.version));
        }
        Ok(cfg)
    }

    pub fn to_yaml(&self) -> String {
        serde_yaml::to_string(self).expect("Config is always serializable")
    }

    /// Validate the config and return non-fatal warnings (overlapping
    /// fixed entries, etc.). Hard errors come back as `Err`.
    pub fn validate(&self) -> Result<Vec<Warning>, ConfigError> {
        let default = parse_duration(&self.rotation.default_duration)?;
        for entry in &self.rotation.urls {
            entry.duration(default)?; // surface bad per-URL durations
        }
        let mut intervals: Vec<(
            usize,
            chrono::DateTime<chrono::Utc>,
            chrono::DateTime<chrono::Utc>,
        )> = Vec::new();
        let now = chrono::Utc::now();
        let horizon = now + chrono::Duration::days(7);
        for (i, e) in self.fixed.iter().enumerate() {
            let dur = parse_duration(&e.duration)?;
            let schedule = parse_cron(&e.cron)?;
            for start in schedule.upcoming(chrono::Utc).take_while(|t| *t < horizon) {
                let end = start + chrono::Duration::from_std(dur).unwrap_or_default();
                intervals.push((i, start, end));
            }
        }
        let mut warnings = Vec::new();
        for a in 0..intervals.len() {
            for b in (a + 1)..intervals.len() {
                let (ia, sa, ea) = intervals[a];
                let (ib, sb, eb) = intervals[b];
                if ia == ib {
                    continue;
                }
                if sa < eb && sb < ea {
                    warnings.push(Warning(format!(
                        "fixed[{}] ({:?}) overlaps fixed[{}] ({:?}) around {}",
                        ia, self.fixed[ia].cron, ib, self.fixed[ib].cron, sa
                    )));
                }
            }
        }
        warnings.sort_by(|a, b| a.0.cmp(&b.0));
        warnings.dedup();
        Ok(warnings)
    }
}

pub fn parse_duration(s: &str) -> Result<Duration, ConfigError> {
    humantime::parse_duration(s).map_err(|e| ConfigError::InvalidDuration(s.to_string(), e))
}

/// Accept the common 5-field cron syntax (`min hour dom month dow`) as well
/// as the 6-field (`+ year`) and the `cron` crate's native 7-field
/// (`sec min hour dom month dow year`) forms. Internally we always feed the
/// crate the 7-field form.
pub fn parse_cron(expr: &str) -> Result<cron::Schedule, ConfigError> {
    let trimmed = expr.trim();
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    let normalized = match parts.len() {
        5 => format!("0 {trimmed} *"),
        6 => format!("{trimmed} *"),
        _ => trimmed.to_string(),
    };
    normalized
        .parse::<cron::Schedule>()
        .map_err(|err| ConfigError::InvalidCron(expr.to_string(), err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_short_and_long_rotation_entries() {
        let yaml = r#"
version: 1
rotation:
  default_duration: 30s
  urls:
    - https://a/
    - url: https://b/
      duration: 60s
"#;
        let cfg = Config::from_yaml(yaml).unwrap();
        assert_eq!(cfg.rotation.urls.len(), 2);
        let default = parse_duration(&cfg.rotation.default_duration).unwrap();
        assert_eq!(
            cfg.rotation.urls[0].duration(default).unwrap(),
            Duration::from_secs(30)
        );
        assert_eq!(
            cfg.rotation.urls[1].duration(default).unwrap(),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn rejects_missing_version() {
        let yaml = "rotation:\n  urls: []\n";
        assert!(matches!(
            Config::from_yaml(yaml),
            Err(ConfigError::MissingVersion)
        ));
    }

    #[test]
    fn rejects_unknown_version() {
        let yaml = "version: 99\n";
        assert!(matches!(
            Config::from_yaml(yaml),
            Err(ConfigError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn warns_on_overlapping_fixed_entries() {
        // Two cron entries that fire at the same minute, both lasting 30 minutes.
        // 5-field standard cron — the parser promotes to 7-field internally.
        let yaml = r#"
version: 1
fixed:
  - cron: "0 9 * * MON-FRI"
    url: https://a/
    duration: 30m
  - cron: "0 9 * * MON-FRI"
    url: https://b/
    duration: 15m
"#;
        let cfg = Config::from_yaml(yaml).unwrap();
        let warnings = cfg.validate().unwrap();
        assert!(
            !warnings.is_empty(),
            "expected at least one overlap warning"
        );
    }

    #[test]
    fn accepts_five_six_and_seven_field_cron() {
        for expr in [
            "0 9 * * MON-FRI",
            "0 0 9 * * MON-FRI",
            "0 0 9 * * MON-FRI *",
        ] {
            parse_cron(expr).unwrap_or_else(|e| panic!("{expr:?}: {e}"));
        }
    }
}

//! YAML config-file loader for `PortalConfig`.
//!
//! Gated behind the `config-file` Cargo feature. Lets users describe the
//! wire contract (schemas, video tracks, sync knobs) once in a YAML file
//! and load it as a `PortalConfig` at runtime, supplying `session` and
//! `role` as kwargs since those are per-process identity, not shareable.
//!
//! The file format is versioned: every document must declare `version: 1`
//! at the top level. Unknown majors are rejected so additive changes can
//! land without silently misparsing older files.
//!
//! Identity-only fields (`session`, `role`, `shared_key`) are deliberately
//! NOT in the file. They're supplied at load time. This keeps a single
//! `arm.yaml` usable by both the robot and the operator side, and keeps
//! E2EE keys out of config repos.

use std::path::Path;

use serde::Deserialize;

use crate::codec::Codec;
use crate::config::{DEFAULT_MJPEG_QUALITY, PortalConfig};
use crate::dtype::DType;
use crate::types::Role;

/// Errors raised by `PortalConfig::from_yaml_*`.
#[derive(Debug, thiserror::Error)]
pub enum ConfigFileError {
    #[error("yaml parse error: {0}")]
    Parse(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("unsupported config-file version {got}; this build supports version {supported}")]
    UnsupportedVersion { got: u32, supported: u32 },

    #[error("invalid config: {0}")]
    Invalid(String),
}

/// The single supported major version. Bump when an incompatible change
/// lands.
const SUPPORTED_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFileV1 {
    version: u32,

    #[serde(default)]
    fps: Option<u32>,
    #[serde(default)]
    slack: Option<u32>,
    #[serde(default)]
    tolerance: Option<f32>,
    #[serde(default)]
    state_reliable: Option<bool>,
    #[serde(default)]
    action_reliable: Option<bool>,
    #[serde(default)]
    reuse_stale_frames: Option<bool>,
    #[serde(default)]
    ping_ms: Option<u64>,
    #[serde(default)]
    action_subscription: Option<bool>,

    #[serde(default)]
    videos: Vec<VideoEntry>,
    #[serde(default)]
    state: Vec<FieldEntry>,
    #[serde(default)]
    action: Vec<FieldEntry>,
    #[serde(default)]
    action_chunks: Vec<ChunkEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VideoEntry {
    name: String,
    #[serde(deserialize_with = "deserialize_codec")]
    codec: Codec,
    #[serde(default)]
    quality: Option<u8>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldEntry {
    name: String,
    #[serde(deserialize_with = "deserialize_dtype")]
    dtype: DType,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChunkEntry {
    name: String,
    horizon: u32,
    fields: Vec<FieldEntry>,
}

fn deserialize_codec<'de, D>(d: D) -> Result<Codec, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    match s.to_ascii_lowercase().as_str() {
        "h264" => Ok(Codec::H264),
        "raw" => Ok(Codec::Raw),
        "png" => Ok(Codec::Png),
        "mjpeg" => Ok(Codec::Mjpeg),
        other => Err(serde::de::Error::custom(format!(
            "unknown codec '{other}' (expected one of h264, raw, png, mjpeg)"
        ))),
    }
}

fn deserialize_dtype<'de, D>(d: D) -> Result<DType, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    match s.to_ascii_lowercase().as_str() {
        "f64" => Ok(DType::F64),
        "f32" => Ok(DType::F32),
        "i32" => Ok(DType::I32),
        "i16" => Ok(DType::I16),
        "i8" => Ok(DType::I8),
        "u32" => Ok(DType::U32),
        "u16" => Ok(DType::U16),
        "u8" => Ok(DType::U8),
        "bool" => Ok(DType::Bool),
        other => Err(serde::de::Error::custom(format!(
            "unknown dtype '{other}' (expected one of f64, f32, i32, i16, i8, u32, u16, u8, bool)"
        ))),
    }
}

impl PortalConfig {
    /// Load a `PortalConfig` from a YAML string. `session` and `role` are
    /// supplied here because they're per-process identity, not part of the
    /// shareable wire contract. The shared E2EE key, when used, must be
    /// applied with `set_e2ee_key` after loading.
    pub fn from_yaml_str(
        yaml: &str,
        session: impl Into<String>,
        role: Role,
    ) -> Result<Self, ConfigFileError> {
        let parsed: ConfigFileV1 = serde_saphyr::from_str(yaml)
            .map_err(|e| ConfigFileError::Parse(e.to_string()))?;

        if parsed.version != SUPPORTED_VERSION {
            return Err(ConfigFileError::UnsupportedVersion {
                got: parsed.version,
                supported: SUPPORTED_VERSION,
            });
        }

        validate(&parsed)?;

        let mut cfg = PortalConfig::new(session, role);

        for v in &parsed.videos {
            let quality = v.quality.unwrap_or(match v.codec {
                Codec::Mjpeg => DEFAULT_MJPEG_QUALITY,
                _ => 0,
            });
            cfg.add_video(&v.name, v.codec, quality);
        }

        if !parsed.state.is_empty() {
            cfg.add_state_typed(parsed.state.iter().map(|f| (f.name.clone(), f.dtype)));
        }
        if !parsed.action.is_empty() {
            cfg.add_action_typed(parsed.action.iter().map(|f| (f.name.clone(), f.dtype)));
        }
        for chunk in &parsed.action_chunks {
            cfg.add_action_chunk(
                &chunk.name,
                chunk.horizon,
                chunk.fields.iter().map(|f| (f.name.clone(), f.dtype)),
            );
        }

        if let Some(v) = parsed.fps {
            cfg.set_fps(v);
        }
        if let Some(v) = parsed.slack {
            cfg.set_slack(v);
        }
        if let Some(v) = parsed.tolerance {
            cfg.set_tolerance(v);
        }
        if let Some(v) = parsed.state_reliable {
            cfg.set_state_reliable(v);
        }
        if let Some(v) = parsed.action_reliable {
            cfg.set_action_reliable(v);
        }
        if let Some(v) = parsed.reuse_stale_frames {
            cfg.set_reuse_stale_frames(v);
        }
        if let Some(v) = parsed.ping_ms {
            cfg.set_ping_ms(v);
        }
        if let Some(v) = parsed.action_subscription {
            cfg.set_action_subscription(v);
        }

        Ok(cfg)
    }

    /// Load a `PortalConfig` from a YAML file on disk. See `from_yaml_str`
    /// for the field semantics.
    pub fn from_yaml_file(
        path: impl AsRef<Path>,
        session: impl Into<String>,
        role: Role,
    ) -> Result<Self, ConfigFileError> {
        let text = std::fs::read_to_string(path)?;
        Self::from_yaml_str(&text, session, role)
    }
}

/// Pre-flight validation: catches everything `PortalConfig`'s setters
/// would assert on, and returns a typed error rather than letting the
/// loader panic mid-build.
fn validate(p: &ConfigFileV1) -> Result<(), ConfigFileError> {
    let mut seen_video = std::collections::HashSet::new();
    for v in &p.videos {
        if !seen_video.insert(v.name.as_str()) {
            return Err(ConfigFileError::Invalid(format!(
                "duplicate video track '{}'",
                v.name
            )));
        }
        if v.codec == Codec::Mjpeg {
            let q = v.quality.unwrap_or(DEFAULT_MJPEG_QUALITY);
            if !(1..=100).contains(&q) {
                return Err(ConfigFileError::Invalid(format!(
                    "video '{}': mjpeg quality must be in 1..=100, got {q}",
                    v.name
                )));
            }
        }
    }

    if let Some(fps) = p.fps {
        if fps == 0 {
            return Err(ConfigFileError::Invalid("fps must be > 0".into()));
        }
    }
    if let Some(slack) = p.slack {
        if slack == 0 {
            return Err(ConfigFileError::Invalid("slack must be > 0".into()));
        }
    }
    if let Some(tol) = p.tolerance {
        if !(tol > 0.0) {
            return Err(ConfigFileError::Invalid("tolerance must be > 0".into()));
        }
    }

    let mut seen_chunk = std::collections::HashSet::new();
    for c in &p.action_chunks {
        if !seen_chunk.insert(c.name.as_str()) {
            return Err(ConfigFileError::Invalid(format!(
                "duplicate action chunk '{}'",
                c.name
            )));
        }
        if c.horizon == 0 {
            return Err(ConfigFileError::Invalid(format!(
                "action chunk '{}' horizon must be > 0",
                c.name
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml_full() -> &'static str {
        r#"
version: 1
fps: 60
slack: 8
tolerance: 1.0
state_reliable: false
action_reliable: false
reuse_stale_frames: true
ping_ms: 500
action_subscription: true
videos:
  - { name: front, codec: h264 }
  - { name: wrist, codec: mjpeg, quality: 80 }
  - { name: depth, codec: png }
state:
  - { name: joint_pos, dtype: f32 }
  - { name: gripper, dtype: bool }
action:
  - { name: joint_pos, dtype: f32 }
action_chunks:
  - name: vla
    horizon: 16
    fields:
      - { name: joint_pos, dtype: f32 }
"#
    }

    #[test]
    fn full_config_round_trip() {
        let cfg = PortalConfig::from_yaml_str(yaml_full(), "demo", Role::Robot).unwrap();
        assert_eq!(cfg.video_tracks(), &["front".to_string()]);
        assert_eq!(cfg.frame_video_tracks().len(), 2);
        assert_eq!(cfg.frame_video_tracks()[0].name, "wrist");
        assert_eq!(cfg.frame_video_tracks()[0].codec, Codec::Mjpeg);
        assert_eq!(cfg.frame_video_tracks()[0].quality, 80);
        assert_eq!(cfg.frame_video_tracks()[1].codec, Codec::Png);

        let state: Vec<&str> = cfg.state_fields().collect();
        assert_eq!(state, vec!["joint_pos", "gripper"]);
        assert_eq!(cfg.state_schema()[1].dtype, DType::Bool);

        let action: Vec<&str> = cfg.action_fields().collect();
        assert_eq!(action, vec!["joint_pos"]);

        assert_eq!(cfg.action_chunks().len(), 1);
        assert_eq!(cfg.action_chunks()[0].horizon, 16);

        assert!(cfg.action_subscription());
    }

    #[test]
    fn defaults_apply_when_fields_omitted() {
        let yaml = "version: 1\n";
        let cfg = PortalConfig::from_yaml_str(yaml, "demo", Role::Robot).unwrap();
        // Same defaults as `PortalConfig::new`.
        assert_eq!(cfg.video_tracks().len(), 0);
        assert_eq!(cfg.frame_video_tracks().len(), 0);
        assert_eq!(cfg.state_schema().len(), 0);
        assert_eq!(cfg.action_schema().len(), 0);
        assert_eq!(cfg.action_chunks().len(), 0);
        assert!(!cfg.action_subscription());
    }

    #[test]
    fn mjpeg_quality_defaults_to_90() {
        let yaml = r#"
version: 1
videos:
  - { name: cam, codec: mjpeg }
"#;
        let cfg = PortalConfig::from_yaml_str(yaml, "demo", Role::Robot).unwrap();
        assert_eq!(cfg.frame_video_tracks()[0].quality, DEFAULT_MJPEG_QUALITY);
    }

    #[test]
    fn unknown_version_rejected() {
        let yaml = "version: 2\n";
        let err = PortalConfig::from_yaml_str(yaml, "demo", Role::Robot).unwrap_err();
        assert!(matches!(err, ConfigFileError::UnsupportedVersion { got: 2, supported: 1 }));
    }

    #[test]
    fn missing_version_rejected() {
        let yaml = "fps: 30\n";
        let err = PortalConfig::from_yaml_str(yaml, "demo", Role::Robot).unwrap_err();
        assert!(matches!(err, ConfigFileError::Parse(_)));
    }

    #[test]
    fn unknown_field_rejected() {
        let yaml = "version: 1\nbogus_field: 7\n";
        let err = PortalConfig::from_yaml_str(yaml, "demo", Role::Robot).unwrap_err();
        assert!(matches!(err, ConfigFileError::Parse(_)));
    }

    #[test]
    fn duplicate_video_rejected() {
        let yaml = r#"
version: 1
videos:
  - { name: cam, codec: h264 }
  - { name: cam, codec: mjpeg, quality: 80 }
"#;
        let err = PortalConfig::from_yaml_str(yaml, "demo", Role::Robot).unwrap_err();
        match err {
            ConfigFileError::Invalid(msg) => assert!(msg.contains("duplicate video")),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_chunk_rejected() {
        let yaml = r#"
version: 1
action_chunks:
  - { name: vla, horizon: 4, fields: [{ name: x, dtype: f32 }] }
  - { name: vla, horizon: 4, fields: [{ name: x, dtype: f32 }] }
"#;
        let err = PortalConfig::from_yaml_str(yaml, "demo", Role::Robot).unwrap_err();
        assert!(matches!(err, ConfigFileError::Invalid(_)));
    }

    #[test]
    fn zero_horizon_rejected() {
        let yaml = r#"
version: 1
action_chunks:
  - { name: vla, horizon: 0, fields: [{ name: x, dtype: f32 }] }
"#;
        let err = PortalConfig::from_yaml_str(yaml, "demo", Role::Robot).unwrap_err();
        assert!(matches!(err, ConfigFileError::Invalid(_)));
    }

    #[test]
    fn bad_dtype_rejected() {
        let yaml = r#"
version: 1
state:
  - { name: x, dtype: float64 }
"#;
        let err = PortalConfig::from_yaml_str(yaml, "demo", Role::Robot).unwrap_err();
        assert!(matches!(err, ConfigFileError::Parse(_)));
    }

    #[test]
    fn bad_codec_rejected() {
        let yaml = r#"
version: 1
videos:
  - { name: cam, codec: vp8 }
"#;
        let err = PortalConfig::from_yaml_str(yaml, "demo", Role::Robot).unwrap_err();
        assert!(matches!(err, ConfigFileError::Parse(_)));
    }

    #[test]
    fn out_of_range_mjpeg_quality_rejected() {
        let yaml = r#"
version: 1
videos:
  - { name: cam, codec: mjpeg, quality: 0 }
"#;
        let err = PortalConfig::from_yaml_str(yaml, "demo", Role::Robot).unwrap_err();
        assert!(matches!(err, ConfigFileError::Invalid(_)));
    }

    #[test]
    fn dtype_and_codec_strings_are_case_insensitive() {
        let yaml = r#"
version: 1
videos:
  - { name: cam, codec: H264 }
state:
  - { name: x, dtype: F32 }
"#;
        let cfg = PortalConfig::from_yaml_str(yaml, "demo", Role::Robot).unwrap();
        assert_eq!(cfg.video_tracks(), &["cam".to_string()]);
        assert_eq!(cfg.state_schema()[0].dtype, DType::F32);
    }

    #[test]
    fn fps_zero_rejected() {
        let yaml = "version: 1\nfps: 0\n";
        let err = PortalConfig::from_yaml_str(yaml, "demo", Role::Robot).unwrap_err();
        assert!(matches!(err, ConfigFileError::Invalid(_)));
    }

    #[test]
    fn role_is_supplied_at_load_time() {
        // Same YAML loaded twice with different roles produces two configs
        // that disagree only on role.
        let robot = PortalConfig::from_yaml_str(yaml_full(), "demo", Role::Robot).unwrap();
        let operator = PortalConfig::from_yaml_str(yaml_full(), "demo", Role::Operator).unwrap();
        assert_eq!(robot.state_schema(), operator.state_schema());
        assert_eq!(robot.action_schema(), operator.action_schema());
    }
}

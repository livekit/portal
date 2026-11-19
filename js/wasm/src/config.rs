// Copyright 2026 LiveKit, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use livekit_portal_core::codec::Codec;
use livekit_portal_core::config::{FieldSpec, PortalConfig};
use livekit_portal_core::dtype::DType;
use livekit_portal_core::transport::{parse_role, role_name};
use livekit_portal_core::types::TimeSyncSource;
use serde::Serialize;
use wasm_bindgen::prelude::*;

use crate::js;

/// A session's wire contract plus this peer's role. Built only from the YAML
/// contract (JSON is YAML), so the core's loader validates everything and
/// none of `PortalConfig`'s setter assertions can abort the module.
#[wasm_bindgen]
pub struct Config {
    pub(crate) inner: PortalConfig,
}

#[wasm_bindgen]
impl Config {
    #[wasm_bindgen(js_name = fromYaml)]
    pub fn from_yaml(yaml: &str, session: &str, role: &str) -> Result<Config, JsError> {
        let role = parse_role(role).ok_or_else(|| {
            JsError::new(&format!("unknown role '{role}' (expected robot, operator or observer)"))
        })?;
        let inner = PortalConfig::from_yaml_str(yaml, session, role)
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(Config { inner })
    }

    #[wasm_bindgen(js_name = setE2eeKey)]
    pub fn set_e2ee_key(&mut self, key: Vec<u8>) {
        self.inner.set_e2ee_key(key);
    }

    /// Test hook: shifts this peer's raw clock.
    #[wasm_bindgen(js_name = setClockSkewUs)]
    pub fn set_clock_skew_us(&mut self, skew_us: f64) {
        self.inner.set_clock_skew_us(skew_us as i64);
    }

    /// What the driver needs to know about the config, as a plain object.
    pub fn describe(&self) -> Result<JsValue, JsError> {
        js::to_value(&ConfigInfo::from(&self.inner))
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigInfo {
    session: String,
    role: &'static str,
    fps: u32,
    slack: u32,
    videos: Vec<VideoInfo>,
    state: Vec<FieldInfo>,
    action: Vec<FieldInfo>,
    telemetry: Vec<FieldInfo>,
    observation_sync: bool,
    time_sync_source: &'static str,
    clock_skew_us: i64,
    has_e2ee_key: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VideoInfo {
    name: String,
    codec: &'static str,
    webrtc: bool,
    max_bitrate_kbps: Option<u32>,
    simulcast: bool,
    screencast: bool,
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Serialize)]
struct FieldInfo {
    name: String,
    dtype: &'static str,
}

impl From<&PortalConfig> for ConfigInfo {
    fn from(c: &PortalConfig) -> Self {
        let fields = |schema: &[FieldSpec]| schema.iter().map(FieldInfo::from).collect();
        Self {
            session: c.session().to_string(),
            role: role_name(c.role()),
            fps: c.fps(),
            slack: c.slack(),
            videos: c
                .video_tracks()
                .iter()
                .map(|s| VideoInfo {
                    name: s.name.clone(),
                    codec: codec_name(s.codec),
                    webrtc: s.is_webrtc(),
                    max_bitrate_kbps: s.max_bitrate_kbps,
                    simulcast: s.simulcast,
                    screencast: s.screencast,
                    width: s.resolution.map(|r| r.width),
                    height: s.resolution.map(|r| r.height),
                })
                .collect(),
            state: fields(c.state_schema()),
            action: fields(c.action_schema()),
            telemetry: fields(c.telemetry_schema()),
            observation_sync: c.observation_sync(),
            time_sync_source: match c.time_sync_source() {
                TimeSyncSource::Portal => "portal",
                TimeSyncSource::System => "system",
            },
            clock_skew_us: c.clock_skew_us(),
            has_e2ee_key: c.has_e2ee_key(),
        }
    }
}

impl From<&FieldSpec> for FieldInfo {
    fn from(f: &FieldSpec) -> Self {
        Self { name: f.name.clone(), dtype: dtype_name(f.dtype) }
    }
}

/// The names the YAML contract spells codecs with.
fn codec_name(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264",
        Codec::Vp8 => "vp8",
        Codec::Vp9 => "vp9",
        Codec::Av1 => "av1",
        Codec::H265 => "h265",
        Codec::Raw => "raw",
        Codec::Png => "png",
        Codec::Mjpeg => "mjpeg",
    }
}

fn dtype_name(dtype: DType) -> &'static str {
    match dtype {
        DType::F64 => "f64",
        DType::F32 => "f32",
        DType::I32 => "i32",
        DType::I16 => "i16",
        DType::I8 => "i8",
        DType::U32 => "u32",
        DType::U16 => "u16",
        DType::U8 => "u8",
        DType::Bool => "bool",
    }
}

#[cfg(test)]
mod tests {
    use livekit_portal_core::types::Role;

    use super::*;

    /// The TS layer hands over a JSON object, which must load exactly like
    /// the YAML contract.
    #[test]
    fn json_loads_through_the_yaml_loader() {
        let json = r#"{"version":1,"fps":60,"videos":[{"name":"cam","codec":"mjpeg","quality":70}],
            "state":[{"name":"j1","dtype":"f32"}],"action":[{"name":"j1","dtype":"f32"}]}"#;
        let cfg = PortalConfig::from_yaml_str(json, "lab", Role::Operator).unwrap();
        let info = ConfigInfo::from(&cfg);
        assert_eq!((info.fps, info.role), (60, "operator"));
        assert_eq!((info.videos[0].codec, info.videos[0].webrtc), ("mjpeg", false));
        assert_eq!(info.state[0].dtype, "f32");
    }

    #[test]
    fn invalid_contract_is_an_error_not_a_panic() {
        let json = r#"{"version":1,"fps":0}"#;
        assert!(PortalConfig::from_yaml_str(json, "lab", Role::Robot).is_err());
        let dup =
            r#"{"version":1,"videos":[{"name":"a","codec":"png"},{"name":"a","codec":"png"}]}"#;
        assert!(PortalConfig::from_yaml_str(dup, "lab", Role::Robot).is_err());
    }
}

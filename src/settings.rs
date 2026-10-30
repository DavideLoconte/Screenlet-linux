//! The window's choices, kept in ~/.config/screenlet/settings.json.

use crate::link::config;
use crate::session::Mode;
use serde_json::{json, Value};

#[derive(Clone)]
pub struct Settings {
    pub mode: Mode,
    pub mirroring: bool,
    pub mirror: String,      // the connector to mirror
    pub connection: String,  // "auto", "wifi", "cable"
    pub auto_start: bool,    // show the desktop when a paired tablet is plugged in
    pub tablet_screen: Option<(i32, i32)>,
}

impl Settings {
    pub fn load() -> Settings {
        let v: Value = std::fs::read(config().join("settings.json")).ok()
            .and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or(json!({}));
        let s = |k: &str, d: &str| v[k].as_str().unwrap_or(d).to_string();
        let defaults = Mode::default();
        Settings {
            mode: Mode {
                resolution: s("resolution", "auto"),
                mirror: None,
                priority: s("priority", "balanced"),
                fps: v["fps"].as_u64().unwrap_or(0) as u32,
                codec: s("codec", "auto"),
                kbps: v["kbps"].as_u64().unwrap_or(0) as u32,
                vbv_frames: v["vbv_frames"].as_f64().unwrap_or(0.0),
                packets: s("packets", "auto"),
                keyframe_seconds: v["keyframe_seconds"].as_u64().unwrap_or(defaults.keyframe_seconds as u64) as u32,
            },
            mirroring: v["mirroring"].as_bool().unwrap_or(false),
            mirror: s("mirror", ""),
            connection: s("connection", "auto"),
            auto_start: v["auto_start"].as_bool().unwrap_or(true),
            tablet_screen: v["tablet_screen"].as_array().and_then(|a| Some((a.first()?.as_i64()? as i32, a.get(1)?.as_i64()? as i32))),
        }
    }

    pub fn save(&self) {
        let m = &self.mode;
        let v = json!({
            "resolution": m.resolution, "priority": m.priority, "fps": m.fps, "codec": m.codec, "kbps": m.kbps,
            "vbv_frames": m.vbv_frames, "packets": m.packets, "keyframe_seconds": m.keyframe_seconds,
            "mirroring": self.mirroring, "mirror": self.mirror, "connection": self.connection,
            "auto_start": self.auto_start, "tablet_screen": self.tablet_screen.map(|s| [s.0, s.1]),
        });
        let _ = std::fs::create_dir_all(config());
        let _ = std::fs::write(config().join("settings.json"), serde_json::to_vec_pretty(&v).unwrap_or_default());
    }

    /// The session's mode: the monitor to mirror only when mirroring.
    pub fn session_mode(&self) -> Mode {
        let mut mode = self.mode.clone();
        mode.mirror = (self.mirroring && !self.mirror.is_empty()).then(|| self.mirror.clone());
        mode
    }
}

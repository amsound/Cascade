/// Cascade configuration — persisted as TOML.
///
/// Location: `--config`, else `cascade.toml` in the working directory if one exists there,
/// else the per-user application directory — see `resolve_config_path` in main.rs.

use serde::{Deserialize, Serialize};
use anyhow::Result;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub audio:   AudioConfig,
    pub tone:    ToneConfig,
    pub api:     ApiConfig,
    #[serde(default)]
    pub remotes: Vec<RemoteConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub name:              String,
    pub port:              u16,
    #[serde(default = "default_interface")]
    pub network_interface: String,
}

/// Opus encoder application mode.
///
///   Voice → OPUS_APPLICATION_VOIP  + OPUS_SET_INBAND_FEC=1 + OPUS_SET_PACKET_LOSS_PERC=1
///   Audio → OPUS_APPLICATION_AUDIO (neural-network analysis runs every frame)
///
/// FEC is only meaningful in Voice mode (adds redundancy for lossy links) and is not a
/// separate option — choosing the mode selects the whole encoder configuration.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum AudioMode {
    /// OPUS_APPLICATION_VOIP + FEC. Best for speech on lossy links.
    Voice,
    /// OPUS_APPLICATION_AUDIO — CELT + neural-net pitch/tonal analysis.
    ///
    /// `"lowdelay"` is accepted here as an alias so existing configs still load: it was
    /// once offered as RESTRICTED_LOWDELAY but never reached the encoder (it always
    /// constructed as AUDIO), and CASCADE_AUDIO_SEND_SPEC §6 defines only VOIP and AUDIO.
    /// An old config resolves to Audio, which is what it was already doing.
    #[serde(alias = "lowdelay")]
    Audio,
}

impl Default for AudioMode {
    // Default to Audio (OPUS_APPLICATION_AUDIO = pure CELT at 128kbps).
    // Voice (VOIP) uses SILK+CELT hybrid which is significantly more expensive
    // at bitrates >= 64kbps. Use Voice only for low-bitrate (<=64kbps) channels.
    fn default() -> Self { AudioMode::Audio }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Exact device name from device enumeration, or EMPTY for no device at all. The two
    /// directions are independent: either may be empty while the other names a device,
    /// giving a receive-only or send-only daemon with no other configuration. Empty is
    /// the default for both, so a fresh install opens no hardware until asked to.
    ///
    /// This is the human-readable label, and the FALLBACK identity when no UID is stored
    /// yet (or the UID's device is absent).
    pub input_device:  String,
    pub output_device: String,
    /// Stable per-device identifier — the PREFERRED identity. Names are not stable: two
    /// identical interfaces share a name, users can rename devices, and an aggregate can be
    /// rebuilt under the same name. The identifier survives reboots and re-plugs, so it is
    /// matched first and the name is only a fallback. Empty until learned (older configs, or
    /// a device resolved by name). Available on every host; on CoreAudio it is backed by
    /// kAudioDevicePropertyDeviceUID.
    #[serde(default)]
    pub input_device_uid:  String,
    #[serde(default)]
    pub output_device_uid: String,
    /// Claim EXCLUSIVE (hog) access to the OUTPUT device — CoreAudio
    /// kAudioDevicePropertyHogMode. Taking the device out of the shared mix engine gives a
    /// direct hardware path and lowers output latency. Output only: the input device stays
    /// shared so other apps can still capture. While active, other applications cannot open
    /// the output device.
    ///
    /// macOS only as an explicit claim. Off macOS there is nothing to claim — a `hw:`/
    /// `plughw:` ALSA device is already exclusive and a shared one (`default`, `dmix`)
    /// cannot be made exclusive — so the flag is tracked but performs no action.
    #[serde(default)]
    pub exclusive_output: bool,
    /// Opus encoder target bitrate in KBPS. 0 = VBR auto.
    /// Examples: 64, 96, 128, 192, 256, 512
    pub bitrate_kbps:  u32,

    /// Persistent outgoing channel labels (global — shown to peers). Index = 0-based
    /// channel. Empty/missing entries default to "Ch N". Saved on label edits so
    /// labels survive restart. A short list is fine (missing = default name).
    #[serde(default)]
    pub channel_labels: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToneConfig {
    /// Line-up tone level in dBFS. Broadcast standard is -18.0 (PPM 4 / VU 0).
    /// Clamped to -60.0..=0.0 when the generator is built; 0.0 is full scale.
    ///
    /// There is no enable flag: the two tone legs are generated continuously and
    /// carry audio only where a tone crosspoint is routed to a remote, so routing
    /// is the on/off control. Read when the capture engine is built, which is at
    /// startup and on an input-device change.
    pub level_db: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    /// HTTP port for the web UI and REST API
    pub port: u16,
    /// Bind address (0.0.0.0 = all interfaces, 127.0.0.1 = loopback only)
    pub bind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub name:    String,
    pub host:    String,
    pub port:    u16,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Phase lock enabled for this remote — all channels play sample-locked
    #[serde(default)]
    pub phase_lock: bool,
    /// Password for this remote. Token = MD5(UPPER(remote_name)+UPPER(password)).
    /// Required for remotes that have a password set.
    #[serde(default)]
    pub password: String,
    /// "Receive buffer" in the web UI. Incoming playout buffer for channels received from
    /// this remote, in ms. Clamped 5–10000, with a further floor of 20 when phase lock is
    /// on. Higher values help on lossy/high-jitter links.
    #[serde(default = "default_receive_buffer_ms")]
    pub receive_buffer_ms: u32,
    /// "Mode" in the web UI. Per remote, always present — there is no instance-wide mode
    /// and no unset state.
    #[serde(default)]
    pub mode: AudioMode,
    /// Outgoing channel routing matrix for this remote.
    /// Format: "row:col:value,..." (0-based, matching wire protocol).
    /// row = local encoder channel, col = remote slot in packet header.
    /// None: nothing is sent to this remote (nothing routed unless specified).
    /// Example: "0:0:1,1:1:1" sends local ch 0 as slot 0, ch 1 as slot 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_matrix: Option<String>,

    /// Incoming channel routing matrix for this remote.
    /// Format: "row:col:value,..." (0-based).
    /// row = incoming packet channel, col = local output channel.
    /// None: nothing from this remote is played (nothing routed unless specified).
    /// Example: "0:0:1,1:1:1" routes incoming slot 0 → output 0, slot 1 → output 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receive_matrix: Option<String>,

    /// "Frame size" in the web UI. Outgoing Opus frame size in ms for packets sent to this
    /// remote. Valid: 2.5, 5, 10, 20. Every channel routed to this remote is sent at exactly
    /// this size, whatever other remotes use (CASCADE_AUDIO_SEND_SPEC §4.1).
    #[serde(default = "default_frame_ms")]
    pub frame_ms: f32,

    /// End-to-end encryption for audio sent to / received from this remote
    /// (CASCADE_ENCRYPTION_SPEC). Fully wired: X25519 key agreement on the poke,
    /// AES-256-GCM on every audio payload. While enabled but the handshake is still
    /// pending, packets are DROPPED rather than sent in the clear.
    #[serde(default)]
    pub encryption: bool,
}

/// Outgoing frame size for a remote that does not name one, and the seed for the
/// boot-time minimum when no remote is enabled. Not a setting: there is no instance-wide
/// frame size, so nothing reads this except those two defaults.
pub const DEFAULT_FRAME_MS: f32 = 20.0;

/// The frame size Voice mode runs at — the only one it exists at
/// (CASCADE_AUDIO_SEND_SPEC §6).
pub const VOICE_FRAME_MS: f32 = 20.0;

impl RemoteConfig {
    /// Select an encoder mode. Voice exists only at `VOICE_FRAME_MS`, so choosing it on a
    /// remote with a shorter frame raises the frame to `VOICE_FRAME_MS`.
    pub fn select_mode(&mut self, mode: AudioMode) {
        self.mode = mode;
        if mode == AudioMode::Voice && self.frame_ms < VOICE_FRAME_MS {
            self.frame_ms = VOICE_FRAME_MS;
        }
    }
    /// Select a frame size. A frame shorter than `VOICE_FRAME_MS` on a remote in Voice
    /// mode switches that remote to Audio, the mode that frame size runs in.
    pub fn select_frame(&mut self, frame_ms: f32) {
        self.frame_ms = frame_ms;
        if self.mode == AudioMode::Voice && frame_ms < VOICE_FRAME_MS {
            self.mode = AudioMode::Audio;
        }
    }
    /// A stored Voice setting with a shorter frame (a hand-edited file, an API client) is
    /// taken as Voice at `VOICE_FRAME_MS`. Returns whether anything changed.
    pub fn normalise_voice(&mut self) -> bool {
        if self.mode == AudioMode::Voice && self.frame_ms < VOICE_FRAME_MS {
            self.frame_ms = VOICE_FRAME_MS;
            true
        } else { false }
    }
}

/// Receive buffer for a remote that does not name one, and the fallback when no remote is
/// configured at all. Not a setting, for the same reason.
pub const DEFAULT_RECEIVE_BUFFER_MS: u32 = 120;

fn default_frame_ms()           -> f32 { DEFAULT_FRAME_MS }
fn default_receive_buffer_ms()  -> u32 { DEFAULT_RECEIVE_BUFFER_MS }

/// Convert a frame size in ms to its Opus sample count at 48kHz.
/// Valid: 2.5→120, 5→240, 10→480, 20→960. Unknown values clamp to 960.
pub fn frame_ms_to_samples(ms: f32) -> usize {
    match ms {
        m if (m - 2.5).abs() < 0.1 => 120,
        m if (m - 5.0).abs() < 0.1 => 240,
        m if (m - 10.0).abs() < 0.1 => 480,
        m if (m - 20.0).abs() < 0.1 => 960,
        _ => 960,
    }
}

fn default_true() -> bool { true }

// ── Defaults ──────────────────────────────────────────────────────────────

impl Default for Config {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            audio:   AudioConfig::default(),
            tone:    ToneConfig::default(),
            api:     ApiConfig::default(),
            remotes: vec![],
        }
    }
}

fn default_interface() -> String { "any".into() }

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            name:              "Cascade".into(),
            port:              20102,
            network_interface: default_interface(),
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            input_device:      "".into(),
            output_device:     "".into(),
            input_device_uid:  "".into(),
            output_device_uid: "".into(),
            exclusive_output:  false,   // opt-in: it locks the device to other apps
            bitrate_kbps:      0,
            channel_labels:    vec![],
        }
    }
}

impl Default for ToneConfig {
    fn default() -> Self {
        Self { level_db: -18.0 }
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self { port: 8080, bind: "0.0.0.0".into() }
    }
}

// ── I/O ───────────────────────────────────────────────────────────────────

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            let cfg = Self::default();
            cfg.save(path)?;
            return Ok(cfg);
        }
        let s = std::fs::read_to_string(path)?;
        let mut cfg: Self = toml::from_str(&s)?;
        // Cap any hand-edited or migrated labels to the same bound enforced on the
        // live set-label and LABEL-receive paths, so a long label in the file can't
        // bypass the limit. Capping here (the load boundary) means every downstream
        // consumer sees an already-bounded label.
        for l in cfg.audio.channel_labels.iter_mut() {
            *l = crate::net::peer::cap_label(l);
        }
        // Voice below 20 ms is taken as Voice at 20 ms, and written back so the file says
        // what runs.
        let mut voice_fixed = false;
        for r in cfg.remotes.iter_mut() { voice_fixed |= r.normalise_voice(); }
        if voice_fixed { cfg.save(path)?; }
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let s = self.to_toml_string()?;
        // ATOMIC write: serialize to a UNIQUE temp file in the same directory, then
        // rename over the target. rename(2) is atomic on POSIX so a reader can never
        // see a half-written file.
        //
        // The temp name is unique per call (pid + atomic counter + nanos). Saves can run
        // concurrently — routing_tx and routing_rx each save on their own spawn_blocking
        // task, milliseconds apart — and a shared temp path would let two writers
        // interleave into one file and one rename find it already moved.
        use std::sync::atomic::{AtomicU64, Ordering};
        static SAVE_SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SAVE_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| Path::new(".").to_path_buf());
        let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("cascade.toml");
        let tmp = dir.join(format!(".{}.tmp.{}.{}.{}",
            fname, std::process::id(), seq, nanos));
        std::fs::write(&tmp, s.as_bytes())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Serialize to TOML with EXPLICIT section ordering.
    ///
    /// `toml::to_string_pretty(&Config)` can emit a remote's fields straight after the
    /// `[api]` table without a `[[remotes]]` header when the struct is all tables followed
    /// by an array-of-tables — a duplicate `port` key and a file that fails to load. So
    /// each top-level table is serialized on its own and the `[[remotes]]` blocks are
    /// emitted LAST, explicitly. Each section is individually a valid
    /// standalone table, so there is no cross-table attribution hazard.
    pub fn to_toml_string(&self) -> Result<String> {
        let mut out = String::new();
        // Each section serialized in isolation (no sibling tables to mis-attribute).
        out.push_str("[general]\n");
        out.push_str(&toml::to_string(&self.general)?);
        out.push_str("\n[audio]\n");
        out.push_str(&toml::to_string(&self.audio)?);
        out.push_str("\n[tone]\n");
        out.push_str(&toml::to_string(&self.tone)?);
        out.push_str("\n[api]\n");
        out.push_str(&toml::to_string(&self.api)?);
        // Remotes LAST, each as an explicit [[remotes]] array-of-tables entry.
        //
        // Every remote field is written explicitly. `toml::to_string(remote)` mis-serializes
        // RemoteConfig when its `skip_serializing_if` Option fields (send_matrix /
        // receive_matrix) sit among plain values — a bare quoted value with no key and a
        // duplicate `frame_ms`. Optional matrices are emitted only when present.
        // Escape a string for a TOML basic (double-quoted) string. TOML basic strings
        // cannot contain a literal control character, so each must be written as an
        // escape sequence. This ENCODES (round-trips losslessly) rather than removing
        // anything: a newline in a value is written as \n and decoded back to a newline
        // on load. The [general]/[audio]/[tone]/[api] sections above go through
        // toml::to_string, which already does this; esc() covers the hand-written
        // [[remotes]] fields (name/host/password), which can hold arbitrary text.
        fn esc(s: &str) -> String {
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '"'  => out.push_str("\\\""),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    // Other C0 control chars (and DEL) have no short escape: use \uXXXX.
                    c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                        out.push_str(&format!("\\u{:04X}", c as u32));
                    }
                    c => out.push(c),
                }
            }
            out
        }
        for r in &self.remotes {
            out.push_str("\n[[remotes]]\n");
            out.push_str(&format!("name = \"{}\"\n", esc(&r.name)));
            out.push_str(&format!("host = \"{}\"\n", esc(&r.host)));
            out.push_str(&format!("port = {}\n", r.port));
            out.push_str(&format!("enabled = {}\n", r.enabled));
            out.push_str(&format!("phase_lock = {}\n", r.phase_lock));
            out.push_str(&format!("password = \"{}\"\n", esc(&r.password)));
            out.push_str(&format!("receive_buffer_ms = {}\n", r.receive_buffer_ms));
            out.push_str(&format!("mode = \"{}\"\n", match r.mode {
                AudioMode::Voice => "voice",
                AudioMode::Audio => "audio",
            }));
            if let Some(ref m) = r.send_matrix {
                out.push_str(&format!("send_matrix = \"{}\"\n", esc(m)));
            }
            if let Some(ref m) = r.receive_matrix {
                out.push_str(&format!("receive_matrix = \"{}\"\n", esc(m)));
            }
            out.push_str(&format!("frame_ms = {}\n", r.frame_ms));
            out.push_str(&format!("encryption = {}\n", r.encryption));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    /// Keys that no longer exist must not fail the load — an existing file still carries
    /// `atomic_clock` on every remote and `enabled`/`channels` under [tone].
    #[test]
    fn removed_keys_are_ignored_not_errors() {
        let cfg: Config = toml::from_str(concat!(
            "[tone]\nenabled = false\nchannels = [0, 1]\nlevel_db = -20.0\n",
            "\n[[remotes]]\nname = \"a\"\nhost = \"h\"\nport = 1\n",
            "frame_ms = 20\natomic_clock = false\n")).expect("parse");
        assert_eq!(cfg.tone.level_db, -20.0);
        assert_eq!(cfg.remotes.len(), 1);
    }

    /// Serializing must not re-emit any retired key, or the next save would write a file
    /// that only round-trips by luck.
    #[test]
    fn serialization_drops_the_retired_keys() {
        let mut cfg = Config::default();
        cfg.remotes.push(RemoteConfig {
            name: "a".into(), host: "h".into(), port: 1, enabled: true,
            phase_lock: false, password: String::new(), receive_buffer_ms: 120,
            mode: AudioMode::Audio, frame_ms: 20.0,
            encryption: false, send_matrix: None, receive_matrix: None,
        });
        let s = cfg.to_toml_string().expect("serialize");
        assert!(!s.contains("outgoing_channels"), "{s}");
        assert!(!s.contains("transmit_enabled"), "{s}");
        assert!(!s.contains("atomic_clock"), "{s}");
        assert!(!s.contains("enabled = false\nchannels"), "{s}");
        assert!(s.contains("level_db"), "{s}");
    }
}



#[cfg(test)]
mod voice_frame_tests {
    use super::*;

    fn remote(mode: AudioMode, frame_ms: f32) -> RemoteConfig {
        RemoteConfig {
            name: "a".into(), host: "h".into(), port: 1, enabled: true,
            phase_lock: false, password: String::new(), receive_buffer_ms: 120,
            mode, frame_ms, encryption: false, send_matrix: None, receive_matrix: None,
        }
    }

    /// Choosing Voice raises a shorter frame to 20 ms; a 20 ms frame is left alone.
    #[test]
    fn selecting_voice_raises_the_frame() {
        let mut r = remote(AudioMode::Audio, 5.0);
        r.select_mode(AudioMode::Voice);
        assert_eq!((r.mode, r.frame_ms), (AudioMode::Voice, 20.0));
        let mut r = remote(AudioMode::Audio, 20.0);
        r.select_mode(AudioMode::Voice);
        assert_eq!((r.mode, r.frame_ms), (AudioMode::Voice, 20.0));
    }

    /// Choosing a frame under 20 ms on a Voice remote moves it to Audio.
    #[test]
    fn selecting_a_short_frame_leaves_voice() {
        let mut r = remote(AudioMode::Voice, 20.0);
        r.select_frame(2.5);
        assert_eq!((r.mode, r.frame_ms), (AudioMode::Audio, 2.5));
        let mut r = remote(AudioMode::Voice, 20.0);
        r.select_frame(20.0);
        assert_eq!(r.mode, AudioMode::Voice);
    }

    /// A stored Voice setting below 20 ms is read as Voice at 20 ms.
    #[test]
    fn stored_short_voice_is_normalised() {
        let mut r = remote(AudioMode::Voice, 10.0);
        assert!(r.normalise_voice());
        assert_eq!((r.mode, r.frame_ms), (AudioMode::Voice, 20.0));
        let mut r = remote(AudioMode::Audio, 10.0);
        assert!(!r.normalise_voice());
        assert_eq!(r.frame_ms, 10.0);
    }
}

#[cfg(test)]
mod example_tests {
    use super::*;

    /// The shipped example must load, and say what it claims to.
    #[test]
    fn the_example_config_parses() {
        let cfg: Config = toml::from_str(include_str!("../../cascade.toml.example"))
            .expect("cascade.toml.example must parse");
        assert_eq!(cfg.general.port, 20102);
        assert_eq!(cfg.api.port, 8080);
        assert_eq!(cfg.tone.level_db, -18.0);
        let r = &cfg.remotes[0];
        assert_eq!((r.receive_buffer_ms, r.frame_ms, r.mode), (120, 20.0, AudioMode::Audio));
        assert!(!r.encryption && !r.phase_lock && r.enabled);
        assert!(r.send_matrix.is_none() && r.receive_matrix.is_none());
    }
}

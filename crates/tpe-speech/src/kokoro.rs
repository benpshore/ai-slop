//! Kokoro-82M ONNX engine (feature `kokoro`, every OS).
//!
//! Uses `kokoro-tts` 0.3.3, whose API is async (`KokoroTts::new` and
//! `KokoroTts::synth` in its `src/lib.rs`), so this engine owns a small tokio
//! current-thread runtime and blocks on it.
//!
//! Model files, resolved from the directory in `TPE_KOKORO_DIR`:
//! - `kokoro-v1.0.int8.onnx` (or `kokoro-v1.0.onnx`)
//! - `voices.bin`: the **bincode** voice pack from the mzdk100/kokoro `V1.0`
//!   release. kokoro-tts decodes it with `bincode::decode_from_slice`, so the
//!   `voices-v1.0.bin` from kokoro-onnx (a `NumPy` archive) does not load.

use std::path::{Path, PathBuf};

use kokoro_tts::{KokoroTts, Voice};
use tokio::runtime::Runtime;

use crate::audio::Audio;
use crate::{SpeechError, Tts, VoiceInfo, VoiceQuality};

/// Output sample rate of Kokoro v1.0 (the crate's examples play at 24000).
pub const SAMPLE_RATE: u32 = 24_000;

/// Environment variable naming the model directory.
pub const DIR_ENV: &str = "TPE_KOKORO_DIR";

/// Model file names tried in order.
pub const MODEL_FILES: [&str; 2] = ["kokoro-v1.0.int8.onnx", "kokoro-v1.0.onnx"];

/// The bincode voice pack.
pub const VOICES_FILE: &str = "voices.bin";

/// English v1.0 voices this wrapper exposes: (id, language, description).
/// Each id is a `Voice` variant name in kokoro-tts-0.3.3/src/voice.rs:170-225.
pub const VOICES: [(&str, &str, &str); 28] = [
    ("af_heart", "en-US", "Heart (female)"),
    ("af_bella", "en-US", "Bella (female)"),
    ("af_nicole", "en-US", "Nicole (female)"),
    ("af_sarah", "en-US", "Sarah (female)"),
    ("af_sky", "en-US", "Sky (female)"),
    ("af_nova", "en-US", "Nova (female)"),
    ("af_alloy", "en-US", "Alloy (female)"),
    ("af_aoede", "en-US", "Aoede (female)"),
    ("af_jessica", "en-US", "Jessica (female)"),
    ("af_kore", "en-US", "Kore (female)"),
    ("af_river", "en-US", "River (female)"),
    ("am_adam", "en-US", "Adam (male)"),
    ("am_michael", "en-US", "Michael (male)"),
    ("am_echo", "en-US", "Echo (male)"),
    ("am_eric", "en-US", "Eric (male)"),
    ("am_fenrir", "en-US", "Fenrir (male)"),
    ("am_liam", "en-US", "Liam (male)"),
    ("am_onyx", "en-US", "Onyx (male)"),
    ("am_puck", "en-US", "Puck (male)"),
    ("am_santa", "en-US", "Santa (male)"),
    ("bf_emma", "en-GB", "Emma (female)"),
    ("bf_isabella", "en-GB", "Isabella (female)"),
    ("bf_alice", "en-GB", "Alice (female)"),
    ("bf_lily", "en-GB", "Lily (female)"),
    ("bm_george", "en-GB", "George (male)"),
    ("bm_lewis", "en-GB", "Lewis (male)"),
    ("bm_daniel", "en-GB", "Daniel (male)"),
    ("bm_fable", "en-GB", "Fable (male)"),
];

/// Map a voice id to the kokoro-tts `Voice` at normal speed (1.0).
fn voice_for(id: &str) -> Option<Voice> {
    let speed: f32 = 1.0;
    let voice = match id {
        "af_heart" => Voice::AfHeart(speed),
        "af_bella" => Voice::AfBella(speed),
        "af_nicole" => Voice::AfNicole(speed),
        "af_sarah" => Voice::AfSarah(speed),
        "af_sky" => Voice::AfSky(speed),
        "af_nova" => Voice::AfNova(speed),
        "af_alloy" => Voice::AfAlloy(speed),
        "af_aoede" => Voice::AfAoede(speed),
        "af_jessica" => Voice::AfJessica(speed),
        "af_kore" => Voice::AfKore(speed),
        "af_river" => Voice::AfRiver(speed),
        "am_adam" => Voice::AmAdam(speed),
        "am_michael" => Voice::AmMichael(speed),
        "am_echo" => Voice::AmEcho(speed),
        "am_eric" => Voice::AmEric(speed),
        "am_fenrir" => Voice::AmFenrir(speed),
        "am_liam" => Voice::AmLiam(speed),
        "am_onyx" => Voice::AmOnyx(speed),
        "am_puck" => Voice::AmPuck(speed),
        "am_santa" => Voice::AmSanta(speed),
        "bf_emma" => Voice::BfEmma(speed),
        "bf_isabella" => Voice::BfIsabella(speed),
        "bf_alice" => Voice::BfAlice(speed),
        "bf_lily" => Voice::BfLily(speed),
        "bm_george" => Voice::BmGeorge(speed),
        "bm_lewis" => Voice::BmLewis(speed),
        "bm_daniel" => Voice::BmDaniel(speed),
        "bm_fable" => Voice::BmFable(speed),
        _ => return None,
    };
    Some(voice)
}

/// Model and voice-pack paths inside `dir`, when both exist.
pub fn model_files(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let voices = dir.join(VOICES_FILE);
    if !voices.is_file() {
        return None;
    }
    for name in MODEL_FILES {
        let model = dir.join(name);
        if model.is_file() {
            return Some((model, voices));
        }
    }
    None
}

/// The directory in `TPE_KOKORO_DIR` if it holds both model files.
pub fn model_dir_from_env() -> Option<PathBuf> {
    let dir = std::env::var_os(DIR_ENV)?;
    if dir.is_empty() {
        return None;
    }
    let dir = PathBuf::from(dir);
    model_files(&dir).map(|_| dir)
}

/// Kokoro-82M synthesizer.
pub struct Kokoro {
    runtime: Runtime,
    tts: KokoroTts,
}

impl Kokoro {
    /// Load from the directory in `TPE_KOKORO_DIR`.
    pub fn from_env() -> Result<Self, SpeechError> {
        let dir = model_dir_from_env().ok_or_else(|| {
            SpeechError::ModelsMissing(format!(
                "set {DIR_ENV} to a directory holding {} (or {}) and the bincode {VOICES_FILE} \
                 from https://github.com/mzdk100/kokoro/releases/tag/V1.0",
                MODEL_FILES[0], MODEL_FILES[1]
            ))
        })?;
        Self::open(&dir)
    }

    /// Load the model and voice pack from `dir`.
    pub fn open(dir: &Path) -> Result<Self, SpeechError> {
        let (model, voices) = model_files(dir).ok_or_else(|| {
            SpeechError::ModelsMissing(format!(
                "{} lacks {} or {VOICES_FILE}",
                dir.display(),
                MODEL_FILES[0]
            ))
        })?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let model_path: &Path = &model;
        let voices_path: &Path = &voices;
        let tts = runtime
            .block_on(KokoroTts::new(model_path, voices_path))
            .map_err(|error| SpeechError::Engine(error.to_string()))?;
        Ok(Self { runtime, tts })
    }
}

impl Tts for Kokoro {
    fn name(&self) -> &'static str {
        "kokoro"
    }

    fn voices(&self) -> Result<Vec<VoiceInfo>, SpeechError> {
        Ok(VOICES
            .iter()
            .map(|(id, language, name)| VoiceInfo {
                id: (*id).to_string(),
                name: (*name).to_string(),
                language: (*language).to_string(),
                quality: VoiceQuality::Premium,
            })
            .collect())
    }

    /// `voice` is a Kokoro id such as `af_heart`; empty means `af_heart`.
    fn synthesize(&self, text: &str, voice: &str) -> Result<Audio, SpeechError> {
        if text.trim().is_empty() {
            return Err(SpeechError::InvalidInput("text is empty".to_string()));
        }
        let id = if voice.is_empty() { "af_heart" } else { voice };
        let chosen = voice_for(id).ok_or_else(|| SpeechError::VoiceNotFound(id.to_string()))?;
        let (samples, _elapsed) = self
            .runtime
            .block_on(self.tts.synth(text, chosen))
            .map_err(|error| SpeechError::Engine(error.to_string()))?;
        Ok(Audio {
            sample_rate: SAMPLE_RATE,
            samples,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_voice_maps_to_a_kokoro_voice() {
        for (id, _, _) in VOICES {
            assert!(voice_for(id).is_some(), "{id}");
        }
        assert!(voice_for("xx_nobody").is_none());
    }

    #[test]
    fn missing_directory_has_no_model_files() {
        let dir = std::env::temp_dir().join("tpe-speech-no-kokoro-here");
        assert!(model_files(&dir).is_none());
    }

    #[test]
    fn kokoro_synthesizes_or_skips() {
        let Some(dir) = model_dir_from_env() else {
            eprintln!("skipped: {DIR_ENV} unset or missing {VOICES_FILE}/model");
            return;
        };
        let engine = Kokoro::open(&dir).expect("load kokoro");
        let audio = engine
            .synthesize("Hello from the text processing engine.", "af_heart")
            .expect("synthesize");
        assert_eq!(audio.sample_rate, SAMPLE_RATE);
        assert!(!audio.is_silent(), "kokoro produced silence");
    }
}

//! Text-to-speech for the workbench, validated by speech recognition.
//!
//! Two engines implement [`Tts`]: [`AvSpeech`] (macOS `AVSpeechSynthesizer`,
//! captured to PCM rather than played) and, behind the `kokoro` feature,
//! `Kokoro` (Kokoro-82M ONNX on every OS). [`validate::round_trip`] feeds the
//! synthesized audio to docling's Whisper (feature `asr`) and scores the word
//! error rate. An empty transcript means the synthesis failed.

#![allow(
    clippy::must_use_candidate,
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

pub mod audio;
pub mod avspeech;
#[cfg(feature = "kokoro")]
pub mod kokoro;
pub mod play;
pub mod validate;
pub mod wav;

#[cfg(target_os = "macos")]
mod av_ffi;

pub use audio::{Audio, resample_to_16k};
pub use avspeech::AvSpeech;
#[cfg(feature = "kokoro")]
pub use kokoro::Kokoro;
pub use validate::{RoundTrip, round_trip, word_error_rate};

/// Errors from synthesis, playback and validation.
#[derive(Debug, thiserror::Error)]
pub enum SpeechError {
    /// The engine or operation is not available on this OS or build.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The requested voice is not installed or unknown to the engine.
    #[error("voice not found: {0}")]
    VoiceNotFound(String),
    /// The synthesized audio was empty or silent, or the transcript was empty.
    #[error("the TTS produced no intelligible speech (empty audio or empty transcript)")]
    SilentOutput,
    /// Required model files are absent.
    #[error("model files missing: {0}")]
    ModelsMissing(String),
    /// The engine did not finish within the allowed time.
    #[error("timed out: {0}")]
    Timeout(String),
    /// The caller passed unusable input.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// The engine reported an error.
    #[error("engine error: {0}")]
    Engine(String),
    /// The speech recogniser reported an error.
    #[error("asr error: {0}")]
    Asr(String),
    /// Filesystem error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Voice quality tier as the engine reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceQuality {
    /// Compact default voice.
    Default,
    /// Enhanced (downloadable) voice.
    Enhanced,
    /// Premium (neural) voice.
    Premium,
    /// The engine reported a value this crate does not know.
    Unknown,
}

impl VoiceQuality {
    /// Lower-case label for display.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Enhanced => "enhanced",
            Self::Premium => "premium",
            Self::Unknown => "unknown",
        }
    }
}

/// One voice an engine can speak with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceInfo {
    /// Identifier to pass as `voice` to [`Tts::synthesize`].
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// BCP-47 language tag.
    pub language: String,
    /// Quality tier.
    pub quality: VoiceQuality,
}

/// A speech synthesizer that returns mono PCM instead of playing it.
pub trait Tts {
    /// Short engine name (`avspeech`, `kokoro`).
    fn name(&self) -> &'static str;
    /// Voices this engine can use on this machine.
    fn voices(&self) -> Result<Vec<VoiceInfo>, SpeechError>;
    /// Synthesize `text` with `voice` into mono samples.
    fn synthesize(&self, text: &str, voice: &str) -> Result<Audio, SpeechError>;
}

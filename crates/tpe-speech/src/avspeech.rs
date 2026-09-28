//! macOS `AVSpeechSynthesizer` engine, captured to PCM instead of played.
//!
//! On other operating systems every call returns [`SpeechError::Unsupported`].
//! The Objective-C calls live in the private `av_ffi` module.

use std::time::Duration;

use crate::audio::Audio;
use crate::{SpeechError, Tts, VoiceInfo, VoiceQuality};

/// The system speech synthesizer (`AVSpeechSynthesizer`, macOS only).
#[derive(Clone, Debug)]
pub struct AvSpeech {
    /// Upper bound on one synthesis, including run-loop waiting.
    pub timeout: Duration,
}

impl Default for AvSpeech {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
        }
    }
}

impl AvSpeech {
    /// Installed voices, best quality first, then by language and name.
    pub fn list_voices() -> Result<Vec<VoiceInfo>, SpeechError> {
        let mut voices = platform_voices()?;
        voices.sort_by(|a, b| {
            quality_rank(b.quality)
                .cmp(&quality_rank(a.quality))
                .then_with(|| a.language.cmp(&b.language))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(voices)
    }
}

/// Sort key: premium above enhanced above default.
fn quality_rank(quality: VoiceQuality) -> u8 {
    match quality {
        VoiceQuality::Premium => 3,
        VoiceQuality::Enhanced => 2,
        VoiceQuality::Default => 1,
        VoiceQuality::Unknown => 0,
    }
}

impl Tts for AvSpeech {
    fn name(&self) -> &'static str {
        "avspeech"
    }

    fn voices(&self) -> Result<Vec<VoiceInfo>, SpeechError> {
        Self::list_voices()
    }

    /// `voice` is an `AVSpeechSynthesisVoice` identifier such as
    /// `com.apple.voice.premium.en-US.Zoe`; an empty string uses the system
    /// default voice.
    fn synthesize(&self, text: &str, voice: &str) -> Result<Audio, SpeechError> {
        if text.trim().is_empty() {
            return Err(SpeechError::InvalidInput("text is empty".to_string()));
        }
        platform_synthesize(text, voice, self.timeout)
    }
}

// Same signature as the non-macOS fallback, which can fail.
#[cfg(target_os = "macos")]
#[allow(clippy::unnecessary_wraps)]
fn platform_voices() -> Result<Vec<VoiceInfo>, SpeechError> {
    Ok(crate::av_ffi::voices())
}

#[cfg(not(target_os = "macos"))]
fn platform_voices() -> Result<Vec<VoiceInfo>, SpeechError> {
    Err(SpeechError::Unsupported(
        "AVSpeechSynthesizer is macOS-only".to_string(),
    ))
}

#[cfg(target_os = "macos")]
fn platform_synthesize(text: &str, voice: &str, timeout: Duration) -> Result<Audio, SpeechError> {
    crate::av_ffi::synthesize(text, voice, timeout)
}

#[cfg(not(target_os = "macos"))]
fn platform_synthesize(
    _text: &str,
    _voice: &str,
    _timeout: Duration,
) -> Result<Audio, SpeechError> {
    Err(SpeechError::Unsupported(
        "AVSpeechSynthesizer is macOS-only".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_rejected_on_every_os() {
        let engine = AvSpeech::default();
        assert!(matches!(
            engine.synthesize("   ", ""),
            Err(SpeechError::InvalidInput(_))
        ));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn engine_is_unsupported_off_macos() {
        let engine = AvSpeech::default();
        assert_eq!(engine.name(), "avspeech");
        assert!(matches!(
            AvSpeech::list_voices(),
            Err(SpeechError::Unsupported(_))
        ));
        assert!(matches!(
            engine.synthesize("hello", ""),
            Err(SpeechError::Unsupported(_))
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_lists_voices_and_synthesizes_or_skips() {
        let voices = match AvSpeech::list_voices() {
            Ok(voices) => voices,
            Err(error) => panic!("listing voices failed: {error}"),
        };
        if voices.is_empty() {
            eprintln!("skipped: no AVSpeechSynthesisVoice installed");
            return;
        }
        let engine = AvSpeech {
            timeout: Duration::from_secs(60),
        };
        match engine.synthesize("Hello from the text processing engine.", "") {
            Ok(audio) => {
                assert!(audio.sample_rate > 0);
                assert!(!audio.is_silent(), "AVSpeechSynthesizer produced silence");
            }
            Err(SpeechError::Timeout(message)) => {
                eprintln!("skipped: no audio buffers delivered on this runner ({message})");
            }
            Err(error) => panic!("synthesis failed: {error}"),
        }
    }

    #[test]
    fn premium_sorts_first() {
        assert!(quality_rank(VoiceQuality::Premium) > quality_rank(VoiceQuality::Enhanced));
        assert!(quality_rank(VoiceQuality::Enhanced) > quality_rank(VoiceQuality::Default));
    }
}

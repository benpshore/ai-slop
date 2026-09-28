//! Audible playback (macOS `AVAudioPlayer` from a temporary WAV file).

use crate::SpeechError;
use crate::audio::Audio;

/// Play `audio` and block until playback ends. macOS only.
#[cfg(target_os = "macos")]
pub fn play(audio: &Audio) -> Result<(), SpeechError> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = std::env::temp_dir().join(format!(
        "tpe-speech-play-{}-{nanos}.wav",
        std::process::id()
    ));
    crate::wav::write(&path, audio)?;
    let result = crate::av_ffi::play_file(&path);
    let _ = std::fs::remove_file(&path);
    result
}

/// Playback is macOS-only; elsewhere this returns `Unsupported`.
#[cfg(not(target_os = "macos"))]
pub fn play(_audio: &Audio) -> Result<(), SpeechError> {
    Err(SpeechError::Unsupported(
        "playback uses AVAudioPlayer and is macOS-only; write a WAV with `say --out` instead"
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "macos"))]
    use super::*;

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn play_is_unsupported_off_macos() {
        let audio = Audio {
            sample_rate: 16_000,
            samples: vec![0.0; 16],
        };
        assert!(matches!(play(&audio), Err(SpeechError::Unsupported(_))));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn play_is_not_run_in_ci() {
        eprintln!("skipped: audible playback is exercised manually with `tpe-speak say --play`");
    }
}

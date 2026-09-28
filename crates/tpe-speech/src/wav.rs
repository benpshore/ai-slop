//! 16-bit PCM mono WAV encoding.

use std::path::Path;

use crate::SpeechError;
use crate::audio::Audio;

/// Size of the canonical RIFF/WAVE header written by [`encode`].
pub const HEADER_LEN: usize = 44;

/// Encode `audio` as a 16-bit PCM mono WAV file in memory.
///
/// Samples are clamped to `-1.0..=1.0` and scaled by 32767.
pub fn encode(audio: &Audio) -> Result<Vec<u8>, SpeechError> {
    if audio.sample_rate == 0 {
        return Err(SpeechError::InvalidInput(
            "sample rate must be positive".to_string(),
        ));
    }
    let data_len = u32::try_from(audio.samples.len())
        .ok()
        .and_then(|n| n.checked_mul(2))
        .filter(|n| n.checked_add(36).is_some())
        .ok_or_else(|| SpeechError::InvalidInput("audio too long for WAV".to_string()))?;
    let byte_rate = audio
        .sample_rate
        .checked_mul(2)
        .ok_or_else(|| SpeechError::InvalidInput("sample rate too large".to_string()))?;
    let mut out: Vec<u8> = Vec::with_capacity(HEADER_LEN + audio.samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&audio.sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in &audio.samples {
        out.extend_from_slice(&to_i16(*sample).to_le_bytes());
    }
    Ok(out)
}

/// Write `audio` to `path` as a 16-bit PCM mono WAV file.
pub fn write(path: &Path, audio: &Audio) -> Result<(), SpeechError> {
    let bytes = encode(audio)?;
    std::fs::write(path, bytes)?;
    Ok(())
}

/// Convert one float sample to 16-bit PCM (NaN becomes 0).
fn to_i16(sample: f32) -> i16 {
    if sample.is_nan() {
        return 0;
    }
    (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_bytes_are_canonical() {
        let audio = Audio {
            sample_rate: 16_000,
            samples: vec![0.0, 1.0, -1.0],
        };
        let bytes = encode(&audio).expect("encode");
        assert_eq!(bytes.len(), HEADER_LEN + 6);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[4..8], &42u32.to_le_bytes());
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(&bytes[16..20], &16u32.to_le_bytes());
        assert_eq!(&bytes[20..22], &1u16.to_le_bytes());
        assert_eq!(&bytes[22..24], &1u16.to_le_bytes());
        assert_eq!(&bytes[24..28], &16_000u32.to_le_bytes());
        assert_eq!(&bytes[28..32], &32_000u32.to_le_bytes());
        assert_eq!(&bytes[32..34], &2u16.to_le_bytes());
        assert_eq!(&bytes[34..36], &16u16.to_le_bytes());
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(&bytes[40..44], &6u32.to_le_bytes());
        assert_eq!(&bytes[44..46], &0i16.to_le_bytes());
        assert_eq!(&bytes[46..48], &32767i16.to_le_bytes());
        assert_eq!(&bytes[48..50], &(-32767i16).to_le_bytes());
    }

    #[test]
    fn samples_are_clamped_and_nan_is_zero() {
        assert_eq!(to_i16(2.0), 32767);
        assert_eq!(to_i16(-2.0), -32767);
        assert_eq!(to_i16(f32::NAN), 0);
        assert_eq!(to_i16(0.5), 16384);
    }

    #[test]
    fn zero_sample_rate_is_rejected() {
        let audio = Audio {
            sample_rate: 0,
            samples: vec![0.0],
        };
        assert!(matches!(encode(&audio), Err(SpeechError::InvalidInput(_))));
    }

    #[test]
    fn write_round_trips_to_disk() {
        let audio = Audio {
            sample_rate: 24_000,
            samples: vec![0.25; 10],
        };
        let path =
            std::env::temp_dir().join(format!("tpe-speech-wav-test-{}.wav", std::process::id()));
        write(&path, &audio).expect("write");
        let bytes = std::fs::read(&path).expect("read");
        let _ = std::fs::remove_file(&path);
        assert_eq!(bytes, encode(&audio).expect("encode"));
    }
}

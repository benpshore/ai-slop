//! Synthesis -> Whisper transcription -> word error rate.
//!
//! Ben's rule: an empty transcript means the TTS failed, so it is reported as
//! [`SpeechError::SilentOutput`], never as a WER of 1.0.

use crate::audio::Audio;
use crate::{SpeechError, Tts};

/// Result of one synthesis round trip.
#[derive(Clone, Debug, PartialEq)]
pub struct RoundTrip {
    /// What Whisper heard.
    pub transcript: String,
    /// Word-level edit distance divided by the reference word count.
    pub word_error_rate: f32,
    /// Duration of the synthesized audio.
    pub audio_secs: f32,
}

/// Lowercase, keep alphanumerics, turn everything else into spaces and
/// collapse runs of whitespace.
pub fn normalise(text: &str) -> String {
    let mut mapped = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            mapped.extend(ch.to_lowercase());
        } else {
            mapped.push(' ');
        }
    }
    mapped.split_whitespace().collect::<Vec<&str>>().join(" ")
}

/// Word error rate of `hypothesis` against `reference`, both normalised.
///
/// Errors: `InvalidInput` when the reference has no words, `SilentOutput`
/// when the hypothesis has none.
pub fn word_error_rate(reference: &str, hypothesis: &str) -> Result<f32, SpeechError> {
    let reference = normalise(reference);
    let hypothesis = normalise(hypothesis);
    let reference_words: Vec<&str> = reference.split(' ').filter(|w| !w.is_empty()).collect();
    let hypothesis_words: Vec<&str> = hypothesis.split(' ').filter(|w| !w.is_empty()).collect();
    if reference_words.is_empty() {
        return Err(SpeechError::InvalidInput(
            "reference text has no words".to_string(),
        ));
    }
    if hypothesis_words.is_empty() {
        return Err(SpeechError::SilentOutput);
    }
    let distance = edit_distance(&reference_words, &hypothesis_words);
    Ok(distance as f32 / reference_words.len() as f32)
}

/// Levenshtein distance over words (substitution, insertion, deletion cost 1).
fn edit_distance(reference: &[&str], hypothesis: &[&str]) -> usize {
    let mut previous: Vec<usize> = (0..=hypothesis.len()).collect();
    let mut current: Vec<usize> = vec![0; hypothesis.len() + 1];
    for (i, ref_word) in reference.iter().enumerate() {
        current[0] = i + 1;
        for (j, hyp_word) in hypothesis.iter().enumerate() {
            let substitution = previous[j] + usize::from(ref_word != hyp_word);
            let deletion = previous[j + 1] + 1;
            let insertion = current[j] + 1;
            current[j + 1] = substitution.min(deletion).min(insertion);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[hypothesis.len()]
}

/// Score a transcript against the input text.
pub fn score(text: &str, transcript: &str, audio_secs: f32) -> Result<RoundTrip, SpeechError> {
    let word_error_rate = word_error_rate(text, transcript)?;
    Ok(RoundTrip {
        transcript: transcript.trim().to_string(),
        word_error_rate,
        audio_secs,
    })
}

/// Synthesize `text` with `voice`, transcribe it with docling's Whisper and
/// score the transcript.
///
/// Silent audio and an empty transcript are both `SilentOutput`. Without the
/// `asr` feature this returns `Unsupported` after the silence check.
pub fn round_trip(tts: &dyn Tts, text: &str, voice: &str) -> Result<RoundTrip, SpeechError> {
    if normalise(text).is_empty() {
        return Err(SpeechError::InvalidInput(
            "reference text has no words".to_string(),
        ));
    }
    let audio = tts.synthesize(text, voice)?;
    if audio.is_silent() {
        return Err(SpeechError::SilentOutput);
    }
    let transcript = transcribe(&audio)?;
    score(text, &transcript, audio.duration_secs())
}

/// Transcribe mono audio with docling's Whisper (English, default preset).
///
/// The audio is resampled to 16 kHz and handed to docling as WAV bytes.
#[cfg(feature = "asr")]
pub fn transcribe(audio: &Audio) -> Result<String, SpeechError> {
    if !docling_asr::models_available() {
        return Err(SpeechError::ModelsMissing(
            "docling Whisper files .models/asr/{encoder_model.onnx,decoder_model.onnx,vocab.json} \
             (relative to the working directory, or DOCLING_ASR_{ENCODER,DECODER,VOCAB})"
                .to_string(),
        ));
    }
    let bytes = crate::wav::encode(&crate::audio::resample_to_16k(audio))?;
    let segments = docling_asr::transcribe_with_options(&bytes, "tts.wav", None, Some("en"))
        .map_err(|e| SpeechError::Asr(e.0))?;
    let texts: Vec<String> = segments
        .iter()
        .map(|segment| segment.text.trim().to_string())
        .filter(|text| !text.is_empty())
        .collect();
    Ok(texts.join(" "))
}

/// Transcription needs the `asr` feature.
#[cfg(not(feature = "asr"))]
pub fn transcribe(_audio: &Audio) -> Result<String, SpeechError> {
    Err(SpeechError::Unsupported(
        "built without the `asr` feature (docling Whisper)".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VoiceInfo;

    #[test]
    fn normalise_lowercases_and_strips_punctuation() {
        assert_eq!(
            normalise("  Hello, World!  It's 2026. "),
            "hello world it s 2026"
        );
        assert_eq!(normalise("...!"), "");
    }

    #[test]
    fn exact_match_has_zero_wer() {
        let wer = word_error_rate("The quick brown fox.", "the quick brown fox").expect("wer");
        assert!(wer.abs() < f32::EPSILON);
    }

    #[test]
    fn one_substitution_in_four_words_is_a_quarter() {
        let wer = word_error_rate("the quick brown fox", "the quick red fox").expect("wer");
        assert!((wer - 0.25).abs() < 1.0e-6);
    }

    #[test]
    fn insertion_and_deletion_count() {
        let deletion = word_error_rate("a b c d", "a b d").expect("wer");
        assert!((deletion - 0.25).abs() < 1.0e-6);
        let insertion = word_error_rate("a b", "a x b").expect("wer");
        assert!((insertion - 0.5).abs() < 1.0e-6);
    }

    #[test]
    fn empty_transcript_is_silent_output() {
        assert!(matches!(
            word_error_rate("hello world", ""),
            Err(SpeechError::SilentOutput)
        ));
        assert!(matches!(
            word_error_rate("hello world", " ... "),
            Err(SpeechError::SilentOutput)
        ));
        assert!(matches!(
            score("hello world", "  ", 1.0),
            Err(SpeechError::SilentOutput)
        ));
    }

    #[test]
    fn empty_reference_is_invalid() {
        assert!(matches!(
            word_error_rate("", "hello"),
            Err(SpeechError::InvalidInput(_))
        ));
    }

    struct SilentEngine;

    impl Tts for SilentEngine {
        fn name(&self) -> &'static str {
            "silent"
        }

        fn voices(&self) -> Result<Vec<VoiceInfo>, SpeechError> {
            Ok(Vec::new())
        }

        fn synthesize(&self, _text: &str, _voice: &str) -> Result<Audio, SpeechError> {
            Ok(Audio {
                sample_rate: 22_050,
                samples: vec![0.0; 22_050],
            })
        }
    }

    #[test]
    fn silent_synthesis_fails_before_asr() {
        assert!(matches!(
            round_trip(&SilentEngine, "hello world", "any"),
            Err(SpeechError::SilentOutput)
        ));
    }

    #[test]
    fn asr_round_trip_with_models_or_skip() {
        #[cfg(not(feature = "asr"))]
        {
            eprintln!("skipped: built without the `asr` feature");
        }
        #[cfg(feature = "asr")]
        {
            if !docling_asr::models_available() {
                eprintln!("skipped: docling Whisper models missing under .models/asr/");
                return;
            }
            // A pure tone is not speech: Whisper must not produce a
            // transcript matching the text, and an empty one is SilentOutput.
            let tone: Vec<f32> = (0..16_000)
                .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 16_000.0).sin() * 0.3)
                .collect();
            let audio = Audio {
                sample_rate: 16_000,
                samples: tone,
            };
            match transcribe(&audio) {
                Ok(text) => match score("hello world", &text, 1.0) {
                    Ok(trip) => assert!(trip.word_error_rate > 0.0),
                    Err(error) => assert!(matches!(error, SpeechError::SilentOutput)),
                },
                Err(error) => panic!("transcription failed: {error}"),
            }
        }
    }
}

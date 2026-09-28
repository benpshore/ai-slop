//! `tpe-speak`: list voices, synthesize to WAV, and check synthesis with a
//! Whisper round trip.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use tpe_speech::{AvSpeech, SpeechError, Tts, play, round_trip, wav};

/// Neural text-to-speech validated by speech recognition.
#[derive(Parser)]
#[command(name = "tpe-speak", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Which synthesizer to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Engine {
    /// macOS `AVSpeechSynthesizer`.
    Av,
    /// Kokoro-82M ONNX (needs the `kokoro` feature and `TPE_KOKORO_DIR`).
    Kokoro,
}

#[derive(Subcommand)]
enum Command {
    /// List the voices an engine can use, best quality first.
    Voices {
        #[arg(long, value_enum, default_value_t = Engine::Av)]
        engine: Engine,
    },
    /// Synthesize text to a WAV file and/or play it.
    Say {
        #[arg(long, value_enum, default_value_t = Engine::Av)]
        engine: Engine,
        /// Voice identifier (see `voices`); empty uses the engine default.
        #[arg(long, default_value = "")]
        voice: String,
        /// Write 16-bit PCM mono WAV here.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Play the audio (macOS only).
        #[arg(long)]
        play: bool,
        /// Text to speak.
        text: String,
    },
    /// Synthesize, transcribe with Whisper and print the word error rate.
    /// Exits nonzero when the transcript is empty or the WER is too high.
    Check {
        #[arg(long, value_enum, default_value_t = Engine::Av)]
        engine: Engine,
        /// Voice identifier (see `voices`); empty uses the engine default.
        #[arg(long, default_value = "")]
        voice: String,
        /// Largest acceptable word error rate.
        #[arg(long, default_value_t = 0.3)]
        max_wer: f32,
        /// Text to speak.
        text: String,
    },
}

/// Construct the requested engine.
fn make_engine(engine: Engine) -> Result<Box<dyn Tts>, SpeechError> {
    match engine {
        Engine::Av => Ok(Box::new(AvSpeech::default())),
        Engine::Kokoro => make_kokoro(),
    }
}

#[cfg(feature = "kokoro")]
fn make_kokoro() -> Result<Box<dyn Tts>, SpeechError> {
    Ok(Box::new(tpe_speech::Kokoro::from_env()?))
}

#[cfg(not(feature = "kokoro"))]
fn make_kokoro() -> Result<Box<dyn Tts>, SpeechError> {
    Err(SpeechError::Unsupported(
        "built without the `kokoro` feature".to_string(),
    ))
}

/// Run one subcommand; `Ok(false)` means the check failed.
fn run(cli: Cli) -> Result<bool, SpeechError> {
    match cli.command {
        Command::Voices { engine } => {
            let tts = make_engine(engine)?;
            for voice in tts.voices()? {
                println!(
                    "{}\t{}\t{}\t{}",
                    voice.id,
                    voice.quality.as_str(),
                    voice.language,
                    voice.name
                );
            }
            Ok(true)
        }
        Command::Say {
            engine,
            voice,
            out,
            play: should_play,
            text,
        } => {
            if out.is_none() && !should_play {
                return Err(SpeechError::InvalidInput(
                    "give --out <file.wav>, --play, or both".to_string(),
                ));
            }
            let tts = make_engine(engine)?;
            let audio = tts.synthesize(&text, &voice)?;
            if audio.is_silent() {
                return Err(SpeechError::SilentOutput);
            }
            if let Some(path) = out {
                wav::write(&path, &audio)?;
                println!(
                    "wrote {} ({:.2} s at {} Hz)",
                    path.display(),
                    audio.duration_secs(),
                    audio.sample_rate
                );
            }
            if should_play {
                play::play(&audio)?;
            }
            Ok(true)
        }
        Command::Check {
            engine,
            voice,
            max_wer,
            text,
        } => {
            let tts = make_engine(engine)?;
            let trip = round_trip(&*tts, &text, &voice)?;
            let passed = trip.word_error_rate <= max_wer;
            println!("transcript: {}", trip.transcript);
            println!(
                "wer: {:.3}\taudio: {:.2} s\t{}",
                trip.word_error_rate,
                trip.audio_secs,
                if passed { "PASS" } else { "FAIL" }
            );
            Ok(passed)
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("tpe-speak: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn say_requires_an_output() {
        let cli =
            Cli::try_parse_from(["tpe-speak", "say", "--voice", "x", "hello"]).expect("parse");
        assert!(matches!(run(cli), Err(SpeechError::InvalidInput(_))));
    }

    #[test]
    fn check_parses_threshold() {
        let cli = Cli::try_parse_from(["tpe-speak", "check", "--max-wer", "0.5", "hello"])
            .expect("parse");
        match cli.command {
            Command::Check { max_wer, text, .. } => {
                assert!((max_wer - 0.5).abs() < f32::EPSILON);
                assert_eq!(text, "hello");
            }
            _ => panic!("expected check"),
        }
    }

    #[cfg(not(feature = "kokoro"))]
    #[test]
    fn kokoro_without_feature_is_unsupported() {
        assert!(matches!(
            make_engine(Engine::Kokoro),
            Err(SpeechError::Unsupported(_))
        ));
    }
}

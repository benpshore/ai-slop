//! The only `unsafe` Objective-C code in the crate (macOS only).
//!
//! Every `AVFAudio` call is quoted from the offline `objc2-avf-audio-0.3.2`
//! sources (`src/generated/...`); Foundation calls from
//! `objc2-foundation-0.3.2`, blocks from `block2-0.6.2`.

use std::path::Path;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::AnyThread;
use objc2_avf_audio::{
    AVAudioBuffer, AVAudioCommonFormat, AVAudioPCMBuffer, AVAudioPlayer, AVSpeechSynthesisVoice,
    AVSpeechSynthesisVoiceQuality, AVSpeechSynthesizer, AVSpeechUtterance,
};
use objc2_foundation::{NSDate, NSRunLoop, NSString, NSURL};

use crate::audio::Audio;
use crate::{SpeechError, VoiceInfo, VoiceQuality};

/// How long to pump the run loop per iteration, in seconds.
const PUMP_SECS: f64 = 0.05;

/// Stop waiting this long after the last buffer when no terminating
/// zero-length buffer arrives.
const IDLE_AFTER_LAST_BUFFER: Duration = Duration::from_secs(3);

/// State shared with the buffer callback block.
#[derive(Default)]
struct Capture {
    samples: Vec<f32>,
    sample_rate: Option<f64>,
    finished: bool,
    last_buffer: Option<Instant>,
    error: Option<String>,
}

/// Map the `AVFAudio` quality enum.
fn quality_of(quality: AVSpeechSynthesisVoiceQuality) -> VoiceQuality {
    // verified against objc2-avf-audio-0.3.2/src/generated/AVSpeechSynthesis.rs:35-41
    if quality == AVSpeechSynthesisVoiceQuality::Premium {
        VoiceQuality::Premium
    } else if quality == AVSpeechSynthesisVoiceQuality::Enhanced {
        VoiceQuality::Enhanced
    } else if quality == AVSpeechSynthesisVoiceQuality::Default {
        VoiceQuality::Default
    } else {
        VoiceQuality::Unknown
    }
}

/// All installed `AVSpeechSynthesisVoice`s.
pub fn voices() -> Vec<VoiceInfo> {
    // verified against objc2-avf-audio-0.3.2/src/generated/AVSpeechSynthesis.rs:232
    let array = unsafe { AVSpeechSynthesisVoice::speechVoices() };
    // verified against objc2-foundation-0.3.2/src/generated/NSArray.rs:93
    let count = array.count();
    let mut out: Vec<VoiceInfo> = Vec::with_capacity(count);
    for index in 0..count {
        // verified against objc2-foundation-0.3.2/src/generated/NSArray.rs:97
        let voice = array.objectAtIndex(index);
        // verified against objc2-avf-audio-0.3.2/src/generated/AVSpeechSynthesis.rs:281
        let id = unsafe { voice.identifier() }.to_string();
        // verified against objc2-avf-audio-0.3.2/src/generated/AVSpeechSynthesis.rs:290
        let name = unsafe { voice.name() }.to_string();
        // verified against objc2-avf-audio-0.3.2/src/generated/AVSpeechSynthesis.rs:272
        let language = unsafe { voice.language() }.to_string();
        // verified against objc2-avf-audio-0.3.2/src/generated/AVSpeechSynthesis.rs:299
        let quality = quality_of(unsafe { voice.quality() });
        out.push(VoiceInfo {
            id,
            name,
            language,
            quality,
        });
    }
    out
}

/// Append one callback buffer to the capture, mixing channels to mono.
fn append_buffer(state: &mut Capture, buffer: &AVAudioBuffer) {
    state.last_buffer = Some(Instant::now());
    // `AVAudioPCMBuffer` subclasses `AVAudioBuffer`
    // (objc2-avf-audio-0.3.2/src/generated/AVAudioBuffer.rs:89-91);
    // `downcast_ref` is objc2-0.6.4/src/runtime/mod.rs:1489.
    let Some(pcm) = buffer.downcast_ref::<AVAudioPCMBuffer>() else {
        state.error = Some("AVSpeechSynthesizer delivered a non-PCM buffer".to_string());
        state.finished = true;
        return;
    };
    // verified against objc2-avf-audio-0.3.2/src/generated/AVAudioBuffer.rs:187
    let frames = unsafe { pcm.frameLength() } as usize;
    if frames == 0 {
        // A zero-length buffer marks the end of the utterance.
        state.finished = true;
        return;
    }
    // verified against objc2-avf-audio-0.3.2/src/generated/AVAudioBuffer.rs:41
    let format = unsafe { buffer.format() };
    // verified against objc2-avf-audio-0.3.2/src/generated/AVAudioFormat.rs:289
    let rate = unsafe { format.sampleRate() };
    // verified against objc2-avf-audio-0.3.2/src/generated/AVAudioFormat.rs:278
    let channels = unsafe { format.channelCount() } as usize;
    // verified against objc2-avf-audio-0.3.2/src/generated/AVAudioFormat.rs:266
    let common = unsafe { format.commonFormat() };
    // verified against objc2-avf-audio-0.3.2/src/generated/AVAudioBuffer.rs:200
    let stride = unsafe { pcm.stride() };
    if channels == 0 || stride == 0 || rate.is_nan() || rate <= 0.0 {
        state.error = Some(format!(
            "unusable buffer format: {channels} channels, stride {stride}, {rate} Hz"
        ));
        state.finished = true;
        return;
    }
    match state.sample_rate {
        None => state.sample_rate = Some(rate),
        Some(previous) if (previous - rate).abs() > 0.5 => {
            state.error = Some(format!("sample rate changed from {previous} to {rate} Hz"));
            state.finished = true;
            return;
        }
        Some(_) => {}
    }
    let scale = 1.0 / channels as f32;
    if common == AVAudioCommonFormat::PCMFormatFloat32 {
        // verified against objc2-avf-audio-0.3.2/src/generated/AVAudioBuffer.rs:217
        let data = unsafe { pcm.floatChannelData() };
        if data.is_null() {
            state.error = Some("float buffer without channel data".to_string());
            state.finished = true;
            return;
        }
        for frame in 0..frames {
            let mut sum: f32 = 0.0;
            for channel in 0..channels {
                // SAFETY: `data` points at `channelCount` channel pointers, each
                // valid for `frameLength` samples spaced by `stride`
                // (AVAudioBuffer.rs:203-216).
                let value = unsafe { *(*data.add(channel)).as_ptr().add(frame * stride) };
                sum += value;
            }
            state.samples.push(sum * scale);
        }
    } else if common == AVAudioCommonFormat::PCMFormatInt16 {
        // verified against objc2-avf-audio-0.3.2/src/generated/AVAudioBuffer.rs:227
        let data = unsafe { pcm.int16ChannelData() };
        if data.is_null() {
            state.error = Some("int16 buffer without channel data".to_string());
            state.finished = true;
            return;
        }
        for frame in 0..frames {
            let mut sum: f32 = 0.0;
            for channel in 0..channels {
                // SAFETY: as above, for 16-bit samples.
                let value = unsafe { *(*data.add(channel)).as_ptr().add(frame * stride) };
                sum += f32::from(value) / 32768.0;
            }
            state.samples.push(sum * scale);
        }
    } else {
        state.error = Some(format!("unsupported PCM format {common:?}"));
        state.finished = true;
    }
}

/// Pump the current thread's run loop for one short slice.
fn pump_run_loop() {
    // verified against objc2-foundation-0.3.2/src/generated/NSRunLoop.rs:38
    let run_loop = NSRunLoop::currentRunLoop();
    // verified against objc2-foundation-0.3.2/src/generated/NSDate.rs:189
    let until = NSDate::dateWithTimeIntervalSinceNow(PUMP_SECS);
    // verified against objc2-foundation-0.3.2/src/generated/NSRunLoop.rs:111
    run_loop.runUntilDate(&until);
}

/// Synthesize `text` into PCM with `writeUtterance:toBufferCallback:`.
pub fn synthesize(text: &str, voice: &str, timeout: Duration) -> Result<Audio, SpeechError> {
    let shared: Arc<Mutex<Capture>> = Arc::new(Mutex::new(Capture::default()));
    let for_block = Arc::clone(&shared);
    // Block type: objc2-avf-audio-0.3.2/src/generated/AVSpeechSynthesis.rs:129
    // (`*mut DynBlock<dyn Fn(NonNull<AVAudioBuffer>)>`); `RcBlock::new` is
    // block2-0.6.2/src/rc_block.rs:143.
    let block: RcBlock<dyn Fn(NonNull<AVAudioBuffer>)> =
        RcBlock::new(move |buffer: NonNull<AVAudioBuffer>| {
            // SAFETY: AVFAudio passes a valid buffer for the duration of the call.
            let buffer: &AVAudioBuffer = unsafe { buffer.as_ref() };
            if let Ok(mut state) = for_block.lock() {
                append_buffer(&mut state, buffer);
            }
        });

    // verified against objc2-avf-audio-0.3.2/src/generated/AVSpeechSynthesis.rs:379
    let utterance =
        unsafe { AVSpeechUtterance::speechUtteranceWithString(&NSString::from_str(text)) };
    if !voice.is_empty() {
        // verified against objc2-avf-audio-0.3.2/src/generated/AVSpeechSynthesis.rs:261
        let chosen =
            unsafe { AVSpeechSynthesisVoice::voiceWithIdentifier(&NSString::from_str(voice)) }
                .ok_or_else(|| SpeechError::VoiceNotFound(voice.to_string()))?;
        let chosen_ref: &AVSpeechSynthesisVoice = &chosen;
        // verified against objc2-avf-audio-0.3.2/src/generated/AVSpeechSynthesis.rs:433
        unsafe { utterance.setVoice(Some(chosen_ref)) };
    }
    // verified against objc2-avf-audio-0.3.2/src/generated/AVSpeechSynthesis.rs:671
    let synthesizer = unsafe { AVSpeechSynthesizer::new() };
    // verified against objc2-avf-audio-0.3.2/src/generated/AVSpeechSynthesis.rs:567
    // `RcBlock::as_ptr` is block2-0.6.2/src/rc_block.rs:51.
    unsafe { synthesizer.writeUtterance_toBufferCallback(&utterance, RcBlock::as_ptr(&block)) };

    let started = Instant::now();
    loop {
        pump_run_loop();
        {
            let state = shared
                .lock()
                .map_err(|_| SpeechError::Engine("capture state poisoned".to_string()))?;
            if state.finished {
                break;
            }
            if state
                .last_buffer
                .is_some_and(|last| last.elapsed() > IDLE_AFTER_LAST_BUFFER)
            {
                break;
            }
        }
        if started.elapsed() > timeout {
            // A timeout is a failure even when some buffers arrived: returning
            // the partial samples would pass truncated audio off as complete.
            let received = shared.lock().map_or(0, |state| state.samples.len());
            return Err(SpeechError::Timeout(format!(
                "AVSpeechSynthesizer did not finish within {} s ({received} samples received)",
                timeout.as_secs()
            )));
        }
    }
    // Keep the synthesizer, utterance and block alive until capture ends.
    drop(synthesizer);
    drop(utterance);
    drop(block);

    let mut state = shared
        .lock()
        .map_err(|_| SpeechError::Engine("capture state poisoned".to_string()))?;
    if let Some(error) = state.error.take() {
        return Err(SpeechError::Engine(error));
    }
    let sample_rate = state.sample_rate.unwrap_or(0.0).round();
    let samples = std::mem::take(&mut state.samples);
    if samples.is_empty() || sample_rate < 1.0 {
        return Err(SpeechError::SilentOutput);
    }
    Ok(Audio {
        sample_rate: sample_rate as u32,
        samples,
    })
}

/// Play a WAV file through `AVAudioPlayer`, blocking until it ends.
pub fn play_file(path: &Path) -> Result<(), SpeechError> {
    let path_text = path
        .to_str()
        .ok_or_else(|| SpeechError::InvalidInput("path is not UTF-8".to_string()))?;
    // verified against objc2-foundation-0.3.2/src/generated/NSURL.rs:1136
    let url = NSURL::fileURLWithPath(&NSString::from_str(path_text));
    // `alloc` is objc2-0.6.4/src/top_level_traits.rs:437;
    // verified against objc2-avf-audio-0.3.2/src/generated/AVAudioPlayer.rs:25
    let player =
        unsafe { AVAudioPlayer::initWithContentsOfURL_error(AVAudioPlayer::alloc(), &url) }
            .map_err(|error| SpeechError::Engine(format!("AVAudioPlayer: {error:?}")))?;
    // verified against objc2-avf-audio-0.3.2/src/generated/AVAudioPlayer.rs:55
    let _prepared = unsafe { player.prepareToPlay() };
    // verified against objc2-avf-audio-0.3.2/src/generated/AVAudioPlayer.rs:59
    if !unsafe { player.play() } {
        return Err(SpeechError::Engine(
            "AVAudioPlayer refused to play".to_string(),
        ));
    }
    // verified against objc2-avf-audio-0.3.2/src/generated/AVAudioPlayer.rs:83
    let duration = unsafe { player.duration() };
    let limit = Duration::from_secs_f64(duration.max(0.0) + 5.0);
    let started = Instant::now();
    // verified against objc2-avf-audio-0.3.2/src/generated/AVAudioPlayer.rs:75
    while unsafe { player.isPlaying() } {
        if started.elapsed() > limit {
            return Err(SpeechError::Timeout("playback did not finish".to_string()));
        }
        pump_run_loop();
    }
    Ok(())
}

//! In-process macOS audio playback backed by AVAudioPlayer.
//!
//! API signatures follow Apple's AVFAudio/AVAudioPlayer.h SDK header:
//! https://developer.apple.com/documentation/avfaudio/avaudioplayer
//! No temporary files, external player, or dynamically downloaded codecs are used.

// objc 0.2's selector macro checks this legacy cfg inside the consuming crate.
#![allow(unexpected_cfgs)]

use std::{ffi::CStr, marker::PhantomData, os::raw::c_char, ptr::NonNull, rc::Rc};

use anyhow::{Result, anyhow, bail};
use objc::{
    msg_send,
    rc::autoreleasepool,
    runtime::{BOOL, Class, Object, YES},
    sel, sel_impl,
};

#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {}

#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {}

/// Owns one retained Objective-C player. Keep this on the creating UI thread.
/// The Rc marker deliberately prevents Send/Sync across the application's tasks.
pub struct AudioPlayer {
    player: NonNull<Object>,
    _ui_thread: PhantomData<Rc<()>>,
}

/// ABI mirror of CoreAudioTypes/CoreAudioBaseTypes.h. Keep all fields in SDK
/// order, including the padding member; streamDescription returns this C type.
#[repr(C)]
#[derive(Clone, Copy)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    _bytes_per_packet: u32,
    _frames_per_packet: u32,
    _bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    _reserved: u32,
}

impl AudioPlayer {
    /// Decode a complete audio file from memory. This does not start playback or
    /// reserve audio hardware; AVAudioPlayer prepares the hardware when play runs.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        if bytes.is_empty() {
            bail!("This audio cell is empty.");
        }
        autoreleasepool(|| {
            let data_class =
                Class::get("NSData").ok_or_else(|| anyhow!("macOS Foundation is unavailable."))?;
            let player_class = Class::get("AVAudioPlayer")
                .ok_or_else(|| anyhow!("macOS audio playback is unavailable."))?;

            // SAFETY: All selectors and argument/return encodings match the
            // Foundation and AVFAudio SDK declarations. NSData copies the bytes,
            // so the Vec can be dropped after initialization. Both init methods
            // return retained objects (or nil), which we release exactly once.
            unsafe {
                let data_allocated: *mut Object = msg_send![data_class, alloc];
                if data_allocated.is_null() {
                    bail!("Could not allocate memory for the audio cell.");
                }
                let data: *mut Object = msg_send![data_allocated,
                    initWithBytes: bytes.as_ptr() length: bytes.len()
                ];
                if data.is_null() {
                    bail!("Could not copy the audio cell into native memory.");
                }

                let allocated: *mut Object = msg_send![player_class, alloc];
                if allocated.is_null() {
                    let _: () = msg_send![data, release];
                    bail!("Could not allocate a native audio player.");
                }
                let mut error: *mut Object = std::ptr::null_mut();
                let player: *mut Object = msg_send![allocated,
                    initWithData: data error: &mut error
                ];
                // The player retains its NSData after a successful initializer.
                let _: () = msg_send![data, release];
                let Some(player) = NonNull::new(player) else {
                    bail!("Cannot open this audio cell: {}", error_description(error));
                };
                Ok(Self {
                    player,
                    _ui_thread: PhantomData,
                })
            }
        })
    }

    /// Start or resume playback. AVAudioPlayer reports output/decoder failures
    /// through its BOOL result, which the caller can display in the preview.
    pub fn play(&self) -> Result<()> {
        autoreleasepool(|| {
            // SAFETY: self owns a live retained AVAudioPlayer on this thread.
            let success: BOOL = unsafe { msg_send![self.player.as_ptr(), play] };
            if success == YES {
                Ok(())
            } else {
                bail!(
                    "macOS could not start audio playback. Check the audio format and output device."
                );
            }
        })
    }

    /// Pause while preserving the current position.
    pub fn pause(&self) {
        // SAFETY: pause is a void AVAudioPlayer instance method.
        unsafe {
            let _: () = msg_send![self.player.as_ptr(), pause];
        }
    }

    /// Release playback hardware and reset the transport to the beginning.
    pub fn stop(&self) {
        // SAFETY: Both selectors use their AVAudioPlayer SDK signatures.
        unsafe {
            let _: () = msg_send![self.player.as_ptr(), stop];
            let _: () = msg_send![self.player.as_ptr(), setCurrentTime: 0.0_f64];
        }
    }

    /// Seek within the clip, preserving its playing/paused state. Ignore NaN;
    /// negative and over-duration positions are clamped to the valid interval.
    pub fn seek(&self, seconds: f64) {
        if seconds.is_nan() {
            return;
        }
        let seconds = seconds.clamp(0., self.duration());
        // SAFETY: NSTimeInterval is double and the value is finite/in bounds.
        unsafe {
            let _: () = msg_send![self.player.as_ptr(), setCurrentTime: seconds];
        }
    }

    pub fn position(&self) -> f64 {
        // SAFETY: currentTime returns NSTimeInterval (double).
        let position: f64 = unsafe { msg_send![self.player.as_ptr(), currentTime] };
        if position.is_finite() {
            position.clamp(0., self.duration())
        } else {
            0.
        }
    }

    pub fn duration(&self) -> f64 {
        // SAFETY: duration returns NSTimeInterval (double).
        let duration: f64 = unsafe { msg_send![self.player.as_ptr(), duration] };
        if duration.is_finite() {
            duration.max(0.)
        } else {
            0.
        }
    }

    pub fn is_playing(&self) -> bool {
        // SAFETY: isPlaying returns the architecture-correct Objective-C BOOL.
        let playing: BOOL = unsafe { msg_send![self.player.as_ptr(), isPlaying] };
        playing == YES
    }

    /// Describe the audio data's native format, without starting playback or
    /// reserving hardware. Apple documents `AVAudioPlayer.format` as the format
    /// of its audio data and `AVAudioFormat.streamDescription` as its ASBD:
    /// https://developer.apple.com/documentation/avfaudio/avaudioplayer/format
    /// https://developer.apple.com/documentation/avfaudio/avaudioformat/streamdescription
    pub fn format_description(&self) -> String {
        autoreleasepool(|| {
            // SAFETY: self owns a retained, live AVAudioPlayer on this UI
            // thread. The format getter returns AVAudioFormat (available since
            // macOS 10.12). Its borrowed ASBD is copied while both that object
            // and this autorelease pool are alive; no native pointer escapes.
            let description = unsafe {
                let format: *mut Object = msg_send![self.player.as_ptr(), format];
                if format.is_null() {
                    return "Audio format unavailable".into();
                }
                let stream: *const AudioStreamBasicDescription =
                    msg_send![format, streamDescription];
                let Some(stream) = stream.as_ref() else {
                    return "Audio format unavailable".into();
                };
                *stream
            };
            describe_stream_format(description)
        })
    }
}

fn describe_stream_format(format: AudioStreamBasicDescription) -> String {
    // SDK constants kAudioFormatLinearPCM, kLinearPCMFormatFlagIsFloat and
    // kLinearPCMFormatFlagIsSignedInteger, from CoreAudioBaseTypes.h.
    const LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
    const FLOAT: u32 = 1 << 0;
    const SIGNED_INTEGER: u32 = 1 << 2;
    let code = format.format_id.to_be_bytes();
    let format_id = if code.iter().all(|byte| (32..=126).contains(byte)) {
        format!("'{}'", String::from_utf8_lossy(&code))
    } else {
        format!("0x{:08X}", format.format_id)
    };
    let codec = match format.format_id {
        LINEAR_PCM => "PCM",
        value if value == u32::from_be_bytes(*b".mp3") => "MP3",
        value if value == u32::from_be_bytes(*b"aac ") => "AAC",
        value if value == u32::from_be_bytes(*b"alac") => "Apple Lossless",
        _ => "Audio",
    };
    let rate = if format.sample_rate.is_finite() && format.sample_rate > 0. {
        format!("{} Hz", format.sample_rate)
    } else {
        "unknown sample rate".into()
    };
    let channels = match format.channels_per_frame {
        0 => "unknown channels".into(),
        1 => "mono (1 channel)".into(),
        2 => "stereo (2 channels)".into(),
        count => format!("{count} channels"),
    };
    let mut parts = vec![format!("{codec} ({format_id})"), rate, channels];
    if format.bits_per_channel > 0 {
        let representation = if format.format_id != LINEAR_PCM {
            ""
        } else if format.format_flags & FLOAT != 0 {
            " floating point"
        } else if format.format_flags & SIGNED_INTEGER != 0 {
            " signed integer"
        } else {
            " unsigned integer"
        };
        parts.push(format!("{}-bit{representation}", format.bits_per_channel));
    }
    parts.join(" · ")
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        // SAFETY: self owns the only Rust wrapper for this retained object. Stop
        // before releasing so closing a preview cannot leave playback running.
        unsafe {
            let _: () = msg_send![self.player.as_ptr(), stop];
            let _: () = msg_send![self.player.as_ptr(), release];
        }
    }
}

/// Copy NSError's UTF-8 description while its autorelease pool is still alive.
unsafe fn error_description(error: *mut Object) -> String {
    if error.is_null() {
        return "the data is corrupt or its codec is unsupported by macOS".into();
    }
    // SAFETY: A non-null outError from initWithData:error: is an NSError.
    let description: *mut Object = unsafe { msg_send![error, localizedDescription] };
    if description.is_null() {
        return "macOS could not decode the audio data".into();
    }
    let utf8: *const c_char = unsafe { msg_send![description, UTF8String] };
    if utf8.is_null() {
        "macOS could not decode the audio data".into()
    } else {
        // SAFETY: NSString UTF8String returns a NUL-terminated pointer valid
        // during this autorelease pool; to_string_lossy copies it immediately.
        unsafe { CStr::from_ptr(utf8) }
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn silent_wav() -> Vec<u8> {
        let sample_rate = 8_000_u32;
        let sample_count = 2_000_u32;
        let data_len = sample_count * 2;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1_u16.to_le_bytes()); // mono
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.resize(wav.len() + data_len as usize, 0);
        wav
    }

    #[test]
    fn rejects_empty_and_corrupt_audio_without_panicking() {
        assert!(AudioPlayer::from_bytes(Vec::new()).is_err());
        assert!(AudioPlayer::from_bytes(b"This is not an audio file.".to_vec()).is_err());
    }

    #[test]
    fn decodes_wav_seeks_and_resets_without_playing_sound() {
        let player = AudioPlayer::from_bytes(silent_wav()).expect("decode PCM WAV");
        assert!((player.duration() - 0.25).abs() < 0.001);
        assert!(!player.is_playing());
        player.seek(0.125);
        assert!((player.position() - 0.125).abs() < 0.001);
        player.pause();
        assert!((player.position() - 0.125).abs() < 0.001);
        player.seek(f64::NAN);
        assert!((player.position() - 0.125).abs() < 0.001);
        player.seek(-4.);
        assert!(player.position() < 0.001);
        player.seek(100.);
        assert!(player.position() <= player.duration());
        player.stop();
        assert!(player.position() < 0.001);
    }

    #[test]
    fn native_wav_format_reports_sample_rate_channels_and_pcm_bit_depth() {
        let player = AudioPlayer::from_bytes(silent_wav()).expect("decode PCM WAV");
        player.seek(0.125);
        let description = player.format_description();
        assert!(description.contains("8000 Hz"), "{description}");
        assert!(description.contains("mono (1 channel)"), "{description}");
        assert!(
            description.contains("16-bit signed integer"),
            "{description}"
        );
        assert!(description.contains("PCM ('lpcm')"), "{description}");
        assert!(!player.is_playing());
        assert!((player.position() - 0.125).abs() < 0.001);
        assert_eq!(std::mem::size_of::<AudioStreamBasicDescription>(), 40);
        assert_eq!(std::mem::align_of::<AudioStreamBasicDescription>(), 8);
    }

    #[test]
    fn compressed_format_does_not_invent_a_pcm_bit_depth() {
        let description = describe_stream_format(AudioStreamBasicDescription {
            sample_rate: 44_100.,
            format_id: u32::from_be_bytes(*b".mp3"),
            format_flags: 0,
            _bytes_per_packet: 0,
            _frames_per_packet: 1152,
            _bytes_per_frame: 0,
            channels_per_frame: 2,
            bits_per_channel: 0,
            _reserved: 0,
        });
        assert!(description.contains("MP3 ('.mp3')"));
        assert!(description.contains("44100 Hz"));
        assert!(description.contains("stereo (2 channels)"));
        assert!(!description.contains("0-bit"));
    }

    #[test]
    #[ignore = "requires an available macOS audio output device"]
    fn silent_playback_advances_pauses_resumes_and_stops() {
        let player = AudioPlayer::from_bytes(silent_wav()).expect("decode PCM WAV");
        player.play().expect("start silent playback");
        assert!(player.is_playing());
        std::thread::sleep(std::time::Duration::from_millis(40));
        player.pause();
        assert!(!player.is_playing());
        let paused = player.position();
        assert!(paused > 0.);
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!((player.position() - paused).abs() < 0.005);
        player.play().expect("resume silent playback");
        assert!(player.is_playing());
        player.stop();
        assert!(!player.is_playing());
        assert!(player.position() < 0.001);
    }
}

// crates/audiolib-audio/src/soundtouch_processor.rs
//! Stereo SoundTouch ownership with a matching native destructor on every platform.
use audiolib_core::{Error, Result};
use soundtouch_ffi as ffi;

/// The public bindgen native object, constructed in stable storage and exclusively
/// owned by the audio control worker. The high-level `soundtouch` 0.5.4 wrapper
/// omits its native destructor on Windows, so it cannot own audition processors.
pub(super) struct SoundTouchProcessor {
    native: Box<ffi::SoundTouch>,
}

// SAFETY: SoundTouch has no thread affinity. Its allocation and all nested native
// buffers have one owner; every processing operation requires &mut self. Moving
// this owner between control/test workers keeps the boxed native address stable.
// It is deliberately not Sync and is never accessed by an audio callback.
unsafe impl Send for SoundTouchProcessor {}

impl SoundTouchProcessor {
    /// Construct a stereo processor with the already supported audition settings.
    pub(super) fn new(sample_rate: u32, rate: f32, pitch_semitones: f32) -> Result<Self> {
        super::AuditionSettings {
            rate,
            pitch_semitones,
            ..Default::default()
        }
        .validate()?;
        if sample_rate == 0 || sample_rate > i32::MAX as u32 {
            return Err(Error::Validation("Invalid SoundTouch sample rate".into()));
        }
        let mut storage = Box::<ffi::SoundTouch>::new_uninit();
        // SAFETY: This is the public repr(C) bindgen type for the linked native
        // library. The native constructor initializes its correctly aligned,
        // uninitialized allocation in place. The allocation stays at this address
        // until its matching destructor has run exactly once in Drop.
        let native = unsafe {
            ffi::SoundTouch_SoundTouch(storage.as_mut_ptr());
            storage.assume_init()
        };
        let mut processor = Self { native };
        // SAFETY: The object is initialized and uniquely borrowed. Stereo is the
        // only layout this adapter accepts; validated finite tempo/pitch and a
        // nonzero signed-int-compatible sample rate satisfy the native API.
        unsafe {
            processor.native.setChannels(2);
            processor.native.setSampleRate(sample_rate);
            processor.native.setTempo(f64::from(rate));
            processor
                .native
                .setPitch(2.0_f64.powf(f64::from(pitch_semitones) / 12.0));
        }
        Ok(processor)
    }

    /// Copy complete stereo frames into the native input queue.
    pub(super) fn put_samples(&mut self, samples: &[f32]) -> Result<()> {
        if !samples.len().is_multiple_of(2) {
            return Err(Error::Validation(
                "SoundTouch requires complete stereo frames".into(),
            ));
        }
        let frames = u32::try_from(samples.len() / 2)
            .map_err(|_| Error::Validation("SoundTouch input block is too large".into()))?;
        if frames == 0 {
            return Ok(());
        }
        // SAFETY: The native object has two channels and exclusive access. The
        // slice retains exactly frames * 2 readable f32 samples for this call;
        // SoundTouch copies them into its own queue before returning.
        unsafe {
            ffi::SoundTouch_putSamples(
                (&mut *self.native as *mut ffi::SoundTouch).cast(),
                samples.as_ptr(),
                frames,
            );
        }
        Ok(())
    }

    /// Receive at most the complete stereo frames that fit in `samples`.
    pub(super) fn receive_samples(&mut self, samples: &mut [f32]) -> usize {
        let frames = (samples.len() / 2).min(u32::MAX as usize) as u32;
        if frames == 0 {
            return 0;
        }
        // SAFETY: The native stereo processor is exclusively borrowed and the
        // output slice has room for frames * 2 samples. The native return value
        // is a frame count bounded by frames, not an interleaved sample count.
        unsafe {
            ffi::SoundTouch_receiveSamples(
                (&mut *self.native as *mut ffi::SoundTouch).cast(),
                samples.as_mut_ptr(),
                frames,
            ) as usize
        }
    }

    /// Finish this bounded input stream; the processor is never reused afterward.
    pub(super) fn flush(&mut self) {
        // SAFETY: The initialized processor is uniquely borrowed. All real input
        // was already copied into its queue; native padding owns its own memory.
        unsafe { self.native.flush() }
    }
}

impl Drop for SoundTouchProcessor {
    fn drop(&mut self) {
        // SAFETY: The public native constructor initialized this exact boxed
        // object, and no other owner can destroy it. This is the ordinary C++
        // destructor (not a deleting destructor): it releases nested DSP buffers;
        // Box then releases the outer allocation without running native Drop.
        unsafe { ffi::SoundTouch_SoundTouch_destructor(&mut *self.native) }
    }
}

#[cfg(all(test, windows))]
#[path = "soundtouch_lifecycle_tests.rs"]
mod lifecycle_tests;

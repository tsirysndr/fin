//! Second Rockbox DSP instance bound to `CODEC_IDX_VOICE`.
//!
//! The safe [`rockbox_dsp::Dsp`] wrapper is hardcoded to the audio config
//! (`CODEC_IDX_AUDIO`). During a crossfade we need a *second* independent
//! pipeline for the incoming track so its biquad delay lines don't share
//! state with the outgoing track's. Rockbox exposes exactly two configs
//! for that purpose (audio + voice) — coefficients set via
//! `dsp_set_eq_coefs` / `tone_set_*` are global (both configs share the
//! same EQ curve, exactly what we want), and only the internal delay
//! lines are per-config.
//!
//! This module wraps `CODEC_IDX_VOICE` with a `process()` shaped like the
//! safe wrapper's so `decode_one_packet` can plug it in interchangeably.

use std::os::raw::{c_int, c_void};

use rockbox_dsp::{
    dsp_buffer, dsp_buffer_count, dsp_buffer_ptrs, dsp_config, dsp_configure, dsp_get_config,
    dsp_process, sample_format, CODEC_IDX_VOICE, DSP_FLUSH, DSP_RESET, DSP_SET_FREQUENCY,
    DSP_SET_OUT_FREQUENCY, DSP_SET_SAMPLE_DEPTH, DSP_SET_STEREO_MODE, STEREO_INTERLEAVED,
};

/// Minimal wrapper around Rockbox's voice DSP config.
///
/// Uses the same `dsp_init` singleton as [`rockbox_dsp::Dsp`], so an audio
/// DSP must have been constructed at least once (or `dsp_init` called
/// externally) before this is created. `SymphoniaPlayer` always creates
/// the audio-side DSP first.
pub struct VoiceDsp {
    cfg: *mut dsp_config,
}

impl VoiceDsp {
    /// Initialize the voice config for interleaved S16LE stereo at
    /// `sample_rate`. Assumes `dsp_init()` has already been called by the
    /// safe [`rockbox_dsp::Dsp::new`] constructor.
    pub fn new(sample_rate: u32) -> Self {
        let cfg = unsafe { dsp_get_config(CODEC_IDX_VOICE) };
        assert!(!cfg.is_null(), "dsp_get_config(voice) returned null");
        unsafe {
            dsp_configure(cfg, DSP_RESET, 0);
            dsp_configure(cfg, DSP_SET_OUT_FREQUENCY, sample_rate as isize);
            dsp_configure(cfg, DSP_SET_FREQUENCY, sample_rate as isize);
            dsp_configure(cfg, DSP_SET_SAMPLE_DEPTH, 16);
            dsp_configure(cfg, DSP_SET_STEREO_MODE, STEREO_INTERLEAVED);
        }
        Self { cfg }
    }

    pub fn set_input_frequency(&mut self, hz: u32) {
        unsafe { dsp_configure(self.cfg, DSP_SET_FREQUENCY, hz as isize) };
    }

    pub fn flush(&mut self) {
        unsafe { dsp_configure(self.cfg, DSP_FLUSH, 0) };
    }

    /// Run `input` (interleaved stereo S16) through the voice pipeline,
    /// appending processed samples to `out`. Mirrors the shape of
    /// [`rockbox_dsp::Dsp::process`] so it can be swapped in unchanged.
    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) -> usize {
        assert!(input.len() % 2 == 0, "input must be interleaved stereo");
        let mut produced = 0usize;
        let mut chunk = [0i16; 8192]; // 4096 frames per dsp_process call

        let mut src = dsp_buffer {
            remcount: (input.len() / 2) as i32,
            ptrs: dsp_buffer_ptrs {
                pin: [input.as_ptr() as *const c_void; 2],
            },
            count: dsp_buffer_count { proc_mask: 0 },
            format: sample_format::default(),
        };

        loop {
            let mut dst = dsp_buffer {
                remcount: 0,
                ptrs: dsp_buffer_ptrs {
                    p16out: chunk.as_mut_ptr(),
                },
                count: dsp_buffer_count {
                    bufcount: (chunk.len() / 2) as c_int,
                },
                format: sample_format::default(),
            };
            unsafe { dsp_process(self.cfg, &mut src, &mut dst, false) };
            let frames = dst.remcount as usize;
            if frames == 0 && src.remcount <= 0 {
                break;
            }
            out.extend_from_slice(&chunk[..frames * 2]);
            produced += frames;
            if src.remcount <= 0 && frames < chunk.len() / 2 {
                break;
            }
        }
        produced
    }
}

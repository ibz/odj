//! Minimal safe wrapper around the system librubberband C API, used for
//! Master Tempo (key lock): we varispeed the audio ourselves and let
//! Rubber Band shift the pitch back to the original key.

use std::os::raw::{c_double, c_int, c_uint};

#[repr(C)]
struct RubberBandState_ {
    _private: [u8; 0],
}
type State = *mut RubberBandState_;

const OPTION_PROCESS_REALTIME: c_int = 0x0000_0001;
const OPTION_PITCH_HIGH_CONSISTENCY: c_int = 0x0400_0000;
const OPTION_CHANNELS_TOGETHER: c_int = 0x1000_0000;

#[link(name = "rubberband")]
unsafe extern "C" {
    fn rubberband_new(
        sample_rate: c_uint,
        channels: c_uint,
        options: c_int,
        initial_time_ratio: c_double,
        initial_pitch_scale: c_double,
    ) -> State;
    fn rubberband_delete(state: State);
    fn rubberband_reset(state: State);
    fn rubberband_set_pitch_scale(state: State, scale: c_double);
    fn rubberband_get_preferred_start_pad(state: State) -> c_uint;
    fn rubberband_get_start_delay(state: State) -> c_uint;
    fn rubberband_get_latency(state: State) -> c_uint;
    fn rubberband_get_samples_required(state: State) -> c_uint;
    fn rubberband_set_max_process_size(state: State, samples: c_uint);
    fn rubberband_process(state: State, input: *const *const f32, samples: c_uint, last: c_int);
    fn rubberband_available(state: State) -> c_int;
    fn rubberband_retrieve(state: State, output: *const *mut f32, samples: c_uint) -> c_uint;
}

/// Real-time stereo pitch shifter.
pub struct Stretcher {
    state: State,
    pitch: f64,
}

// The state is only ever used by one thread at a time (behind the deck mutex).
unsafe impl Send for Stretcher {}

impl Stretcher {
    pub fn new(sample_rate: u32, max_block: usize) -> Option<Self> {
        let options =
            OPTION_PROCESS_REALTIME | OPTION_PITCH_HIGH_CONSISTENCY | OPTION_CHANNELS_TOGETHER;
        let state = unsafe { rubberband_new(sample_rate, 2, options, 1.0, 1.0) };
        if state.is_null() {
            return None;
        }
        unsafe { rubberband_set_max_process_size(state, max_block as c_uint) };
        Some(Self { state, pitch: 1.0 })
    }

    pub fn reset(&mut self) {
        unsafe { rubberband_reset(self.state) }
    }

    pub fn set_pitch_scale(&mut self, scale: f64) {
        if (scale - self.pitch).abs() > 1e-9 {
            self.pitch = scale;
            unsafe { rubberband_set_pitch_scale(self.state, scale) }
        }
    }

    pub fn start_pad(&self) -> usize {
        unsafe { rubberband_get_preferred_start_pad(self.state) as usize }
    }

    pub fn start_delay(&self) -> usize {
        unsafe { rubberband_get_start_delay(self.state) as usize }
    }

    pub fn latency(&self) -> usize {
        unsafe { rubberband_get_latency(self.state) as usize }
    }

    pub fn samples_required(&self) -> usize {
        unsafe { rubberband_get_samples_required(self.state) as usize }
    }

    pub fn process(&mut self, left: &[f32], right: &[f32]) {
        let n = left.len().min(right.len());
        let ptrs = [left.as_ptr(), right.as_ptr()];
        unsafe { rubberband_process(self.state, ptrs.as_ptr(), n as c_uint, 0) }
    }

    pub fn available(&self) -> usize {
        unsafe { rubberband_available(self.state).max(0) as usize }
    }

    pub fn retrieve(&mut self, left: &mut [f32], right: &mut [f32]) -> usize {
        let n = left.len().min(right.len());
        let ptrs = [left.as_mut_ptr(), right.as_mut_ptr()];
        unsafe { rubberband_retrieve(self.state, ptrs.as_ptr(), n as c_uint) as usize }
    }
}

impl Drop for Stretcher {
    fn drop(&mut self) {
        unsafe { rubberband_delete(self.state) }
    }
}

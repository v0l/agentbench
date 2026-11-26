use agent_voice::voice::audio::{DUPLEX_FRAME_SAMPLES, DUPLEX_RATE, Resampler, to_pcm16};
use std::collections::VecDeque;

pub const FRAME_BYTES: usize = DUPLEX_FRAME_SAMPLES * 2;
pub const JITTER_FRAMES: usize = 2;

pub struct Framer {
    resample: Resampler,
    pending: Vec<f32>,
}

impl Framer {
    pub fn new(mic_rate: u32) -> Framer {
        Framer { resample: Resampler::new(mic_rate, DUPLEX_RATE), pending: Vec::new() }
    }

    pub fn push(&mut self, pcm: &[f32]) -> Vec<Vec<u8>> {
        self.resample.push(pcm, &mut self.pending);
        let whole = self.pending.len() / DUPLEX_FRAME_SAMPLES * DUPLEX_FRAME_SAMPLES;
        let out = self.pending[..whole].chunks(DUPLEX_FRAME_SAMPLES).map(to_pcm16).collect();
        self.pending.drain(..whole);
        out
    }
}

#[derive(Default)]
pub struct Jitter {
    queue: VecDeque<Vec<f32>>,
    started: bool,
    owed: usize,
    pub late: usize,
    pub dropped: usize,
}

impl Jitter {
    pub fn push(&mut self, frame: Vec<f32>) {
        if self.owed > 0 {
            self.owed -= 1;
            self.dropped += 1;
            return;
        }
        self.queue.push_back(frame);
        while self.queue.len() > JITTER_FRAMES + 1 {
            self.queue.pop_front();
            self.dropped += 1;
        }
    }

    pub fn pull(&mut self) -> Vec<f32> {
        if !self.started {
            if self.queue.len() < JITTER_FRAMES {
                return vec![0.0; DUPLEX_FRAME_SAMPLES];
            }
            self.started = true;
        }
        match self.queue.pop_front() {
            Some(f) => f,
            None => {
                self.late += 1;
                self.owed += 1;
                vec![0.0; DUPLEX_FRAME_SAMPLES]
            }
        }
    }

    pub fn clear(&mut self) {
        *self = Jitter { late: self.late, dropped: self.dropped, ..Default::default() };
    }

    pub fn buffered(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(v: f32) -> Vec<f32> {
        vec![v; DUPLEX_FRAME_SAMPLES]
    }

    #[test]
    fn microphone_samples_leave_as_frames_of_6400_bytes_at_24_khz() {
        let mut f = Framer::new(16_000);
        let tone: Vec<f32> = (0..16_000).map(|i| 0.5 * (i as f32 * 0.05).sin()).collect();
        let mut frames = Vec::new();
        for c in tone.chunks(320) {
            frames.extend(f.push(c));
        }
        assert_eq!(frames.len(), 7, "one second at 24 kHz is seven and a half frames");
        assert!(frames.iter().all(|b| b.len() == FRAME_BYTES));
        let mut f = Framer::new(24_000);
        assert!(f.push(&frame(0.1)[..3199]).is_empty(), "a frame leaves once it is full");
        assert_eq!(f.push(&[0.1; 2]).len(), 1);
    }

    #[test]
    fn replies_play_in_order_after_two_frames_are_buffered() {
        let mut j = Jitter::default();
        j.push(frame(0.1));
        assert_eq!(j.pull(), frame(0.0), "one frame is not enough to start");
        assert_eq!(j.buffered(), 1);
        j.push(frame(0.2));
        j.push(frame(0.3));
        assert_eq!(j.pull(), frame(0.1));
        assert_eq!(j.pull(), frame(0.2));
        assert_eq!(j.pull(), frame(0.3));
    }

    #[test]
    fn a_late_frame_plays_silence_and_is_dropped_when_it_comes() {
        let mut j = Jitter::default();
        j.push(frame(0.1));
        j.push(frame(0.2));
        assert_eq!(j.pull(), frame(0.1));
        assert_eq!(j.pull(), frame(0.2));
        assert_eq!(j.pull(), frame(0.0), "the third frame is late");
        assert_eq!(j.late, 1);
        j.push(frame(0.3));
        j.push(frame(0.4));
        assert_eq!(j.pull(), frame(0.4), "the late one is dropped so the delay does not grow");
        assert_eq!(j.dropped, 1);
    }

    #[test]
    fn a_burst_never_holds_more_than_the_buffer_and_the_frame_arriving() {
        let mut j = Jitter::default();
        for i in 1..=6 {
            j.push(frame(i as f32 / 10.0));
        }
        assert_eq!(j.buffered(), JITTER_FRAMES + 1);
        assert!(j.pull() == frame(0.4));
        assert!(j.pull() == frame(0.5));
        assert!(j.pull() == frame(0.6));
    }
}

pub const TAIL_SECONDS: f32 = 0.128;

const BLOCK: usize = 128;
const MU: f32 = 1.0;
const SMOOTH: f32 = 0.3;

#[derive(Clone, Copy, Default, Debug)]
struct C {
    re: f32,
    im: f32,
}

impl C {
    fn mul(self, o: C) -> C {
        C { re: self.re * o.re - self.im * o.im, im: self.re * o.im + self.im * o.re }
    }
    fn conj(self) -> C {
        C { re: self.re, im: -self.im }
    }
    fn norm(self) -> f32 {
        self.re * self.re + self.im * self.im
    }
}

struct Fft {
    twiddles: Vec<C>,
    order: Vec<usize>,
}

impl Fft {
    fn new(n: usize) -> Fft {
        let bits = n.trailing_zeros();
        let order = (0..n).map(|i| i.reverse_bits() >> (usize::BITS - bits)).collect();
        let twiddles = (0..n / 2)
            .map(|k| {
                let a = -std::f32::consts::TAU * k as f32 / n as f32;
                C { re: a.cos(), im: a.sin() }
            })
            .collect();
        Fft { twiddles, order }
    }

    fn run(&self, data: &mut [C], inverse: bool) {
        let n = data.len();
        for i in 0..n {
            let j = self.order[i];
            if i < j {
                data.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let stride = n / len;
            for start in (0..n).step_by(len) {
                for k in 0..len / 2 {
                    let mut w = self.twiddles[k * stride];
                    if inverse {
                        w = w.conj();
                    }
                    let a = data[start + k];
                    let b = data[start + k + len / 2].mul(w);
                    data[start + k] = C { re: a.re + b.re, im: a.im + b.im };
                    data[start + k + len / 2] = C { re: a.re - b.re, im: a.im - b.im };
                }
            }
            len *= 2;
        }
        if inverse {
            let s = 1.0 / n as f32;
            data.iter_mut().for_each(|c| {
                c.re *= s;
                c.im *= s;
            });
        }
    }
}

struct Filter {
    parts: Vec<Vec<C>>,
    err: f32,
    avg: f32,
}

impl Filter {
    fn new(parts: usize) -> Filter {
        Filter { parts: vec![vec![C::default(); BLOCK * 2]; parts], err: 0.0, avg: 0.0 }
    }

    fn estimate(&self, far: &std::collections::VecDeque<Vec<C>>, fft: &Fft) -> Vec<f32> {
        let mut y = vec![C::default(); BLOCK * 2];
        for (w, x) in self.parts.iter().zip(far) {
            for k in 0..BLOCK * 2 {
                let p = w[k].mul(x[k]);
                y[k].re += p.re;
                y[k].im += p.im;
            }
        }
        fft.run(&mut y, true);
        y[BLOCK..].iter().map(|c| c.re).collect()
    }
}

pub struct Canceller {
    fft: Fft,
    front: Filter,
    back: Filter,
    far: std::collections::VecDeque<Vec<C>>,
    psd: Vec<f32>,
    last_far: Vec<f32>,
    mic_in: Vec<f32>,
    far_in: Vec<f32>,
    ready: std::collections::VecDeque<f32>,
    copies: usize,
}

impl Canceller {
    pub fn new(rate: u32) -> Self {
        let parts = ((rate as f32 * TAIL_SECONDS) as usize).div_ceil(BLOCK).max(1);
        Canceller {
            fft: Fft::new(BLOCK * 2),
            front: Filter::new(parts),
            back: Filter::new(parts),
            far: (0..parts).map(|_| vec![C::default(); BLOCK * 2]).collect(),
            psd: vec![0.0; BLOCK * 2],
            last_far: vec![0.0; BLOCK],
            mic_in: Vec::with_capacity(BLOCK),
            far_in: Vec::with_capacity(BLOCK),
            ready: Default::default(),
            copies: 0,
        }
    }

    pub fn copies(&self) -> usize {
        self.copies
    }

    pub fn delay(&self) -> usize {
        BLOCK - 1
    }

    pub fn process(&mut self, mic: &[f32], far: &[f32], out: &mut Vec<f32>) {
        for (i, &m) in mic.iter().enumerate() {
            self.mic_in.push(m);
            self.far_in.push(far.get(i).copied().unwrap_or(0.0));
            if self.mic_in.len() == BLOCK {
                self.block();
            }
            out.push(self.ready.pop_front().unwrap_or(0.0));
        }
    }

    fn block(&mut self) {
        let mut x: Vec<C> = self
            .last_far
            .iter()
            .chain(&self.far_in)
            .map(|&re| C { re, im: 0.0 })
            .collect();
        self.fft.run(&mut x, false);
        for (p, c) in self.psd.iter_mut().zip(&x) {
            *p = 0.9 * *p + 0.1 * c.norm();
        }
        self.far.pop_back();
        self.far.push_front(x);
        self.last_far.copy_from_slice(&self.far_in);
        self.far_in.clear();

        let fy = self.front.estimate(&self.far, &self.fft);
        let by = self.back.estimate(&self.far, &self.fft);
        let mic = std::mem::take(&mut self.mic_in);
        let fe: Vec<f32> = mic.iter().zip(&fy).map(|(m, y)| m - y).collect();
        let be: Vec<f32> = mic.iter().zip(&by).map(|(m, y)| m - y).collect();
        self.ready.extend(fe.iter().copied());
        self.mic_in = mic;
        self.mic_in.clear();

        let far_power: f32 = self.psd.iter().sum::<f32>() / self.psd.len() as f32;
        if far_power > 1e-6 {
            let mut e: Vec<C> = std::iter::repeat_n(C::default(), BLOCK)
                .chain(be.iter().map(|&re| C { re, im: 0.0 }))
                .collect();
            self.fft.run(&mut e, false);
            let floor = far_power * 1e-3 + 1e-7;
            let mu = MU / self.far.len() as f32;
            for (w, x) in self.back.parts.iter_mut().zip(&self.far) {
                let mut g: Vec<C> = (0..BLOCK * 2)
                    .map(|k| {
                        let s = mu / (self.psd[k] + floor);
                        let p = x[k].conj().mul(e[k]);
                        C { re: p.re * s, im: p.im * s }
                    })
                    .collect();
                self.fft.run(&mut g, true);
                g[BLOCK..].iter_mut().for_each(|c| *c = C::default());
                self.fft.run(&mut g, false);
                for k in 0..BLOCK * 2 {
                    w[k].re += g[k].re;
                    w[k].im += g[k].im;
                }
            }
        }

        self.front.err = fe.iter().map(|v| v * v).sum();
        self.back.err = be.iter().map(|v| v * v).sum();
        self.front.avg += SMOOTH * (self.front.err - self.front.avg);
        self.back.avg += SMOOTH * (self.back.err - self.back.avg);
        if self.back.avg < 0.8 * self.front.avg {
            self.front.parts.clone_from(&self.back.parts);
            self.front.avg = self.back.avg;
            self.copies += 1;
        } else if self.back.avg > 4.0 * self.front.avg && self.front.avg > 0.0 {
            self.back.parts.clone_from(&self.front.parts);
            self.back.avg = self.front.avg;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::{RATE, read_wav, to_whisper_rate};

    fn fixture(bytes: &[u8]) -> Vec<f32> {
        let (pcm, rate) = read_wav(bytes).unwrap();
        to_whisper_rate(&pcm, rate)
    }

    fn rms(v: &[f32]) -> f32 {
        (v.iter().map(|x| x * x).sum::<f32>() / v.len().max(1) as f32).sqrt()
    }

    fn scaled(v: &[f32], to: f32) -> Vec<f32> {
        let g = to / rms(v).max(1e-9);
        v.iter().map(|x| x * g).collect()
    }

    fn room() -> Vec<f32> {
        let mut seed = 7u64;
        let mut noise = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            ((seed >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        };
        let ms = |m: f32| (m * RATE as f32 / 1000.0) as usize;
        let mut h = vec![0.0f32; ms(120.0)];
        h[ms(25.0)] = 0.6;
        h[ms(25.0) + 1] = 0.25;
        for (i, v) in h.iter_mut().enumerate().skip(ms(28.0)) {
            let t = (i - ms(28.0)) as f32 / RATE as f32;
            *v += 0.12 * noise() * (-t / 0.03).exp();
        }
        h
    }

    fn convolve(x: &[f32], h: &[f32], len: usize) -> Vec<f32> {
        (0..len)
            .map(|n| h.iter().enumerate().take(n + 1).map(|(k, hk)| hk * x[n - k]).sum())
            .collect()
    }

    #[test]
    fn the_transform_round_trips() {
        let f = Fft::new(8);
        let src: Vec<C> = (0..8).map(|i| C { re: i as f32, im: 0.0 }).collect();
        let mut d = src.clone();
        f.run(&mut d, false);
        assert!((d[0].re - 28.0).abs() < 1e-4);
        f.run(&mut d, true);
        for (a, b) in d.iter().zip(&src) {
            assert!((a.re - b.re).abs() < 1e-4 && a.im.abs() < 1e-4);
        }
    }

    fn erle(echo: &[f32], out: &[f32]) -> f32 {
        20.0 * (rms(echo) / rms(out).max(1e-9)).log10()
    }

    #[test]
    fn a_reply_played_into_the_room_is_cancelled_and_the_voice_is_kept() {
        let reply = scaled(&fixture(include_bytes!("../../tests/fixtures/voice/speech.wav")), 0.1);
        let voice = scaled(&fixture(include_bytes!("../../tests/fixtures/voice/ask.wav")), 0.05);
        let second = reply.len();
        let start = second + reply.len() - voice.len() / 2;
        let total = start + voice.len();
        let mut far = reply.clone();
        far.extend_from_slice(&reply);
        far.resize(total, 0.0);
        let echo = convolve(&far, &room(), total);
        let mut near = vec![0.0f32; total];
        near[start..].copy_from_slice(&voice);
        let mic: Vec<f32> = echo.iter().zip(&near).map(|(e, v)| e + v).collect();

        let mut c = Canceller::new(RATE);
        let mut out = Vec::new();
        for (m, f) in mic.chunks(320).zip(far.chunks(320)) {
            c.process(m, f, &mut out);
        }
        let d = c.delay();
        let out = &out[d..];

        let half = RATE as usize / 2;
        let curve: Vec<String> = (0..start / half)
            .map(|i| format!("{:.0}", erle(&echo[i * half..(i + 1) * half], &out[i * half..(i + 1) * half])))
            .collect();
        eprintln!("dB removed per half second from a cold start: {}", curve.join(" "));
        let erle = erle(&echo[second..start], &out[second..start]);
        let end = total - d;
        let leftover: Vec<f32> =
            out[start..end].iter().zip(&voice).map(|(o, v)| o - v).collect();
        let kept = 1.0 - rms(&leftover) / rms(&voice[..end - start]);
        eprintln!(
            "echo removed {erle:.1} dB, voice kept {:.1} percent, {} copies",
            kept * 100.0,
            c.copies()
        );
        assert!(erle >= 20.0, "removed only {erle:.1} dB of the reply");
        assert!(kept >= 0.9, "kept only {:.1} percent of the voice", kept * 100.0);
    }

    #[test]
    fn with_nothing_played_the_microphone_passes_through_a_block_late() {
        let voice = fixture(include_bytes!("../../tests/fixtures/voice/ask.wav"));
        let mut c = Canceller::new(RATE);
        let mut out = Vec::new();
        c.process(&voice, &[], &mut out);
        assert_eq!(out.len(), voice.len());
        assert_eq!(&out[c.delay()..], &voice[..voice.len() - c.delay()]);
    }
}

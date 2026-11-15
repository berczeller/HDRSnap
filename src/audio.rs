//! Platform-independent audio helpers: channel downmix and the timeline that
//! keeps captured audio aligned with wall-clock time (and so with the video).

/// Interleaved samples with `channels` channels -> interleaved stereo,
/// appended to `out`. Layouts follow WAVEFORMATEXTENSIBLE channel order
/// (FL, FR, FC, LFE, BL, BR, SL, SR); LFE is left out.
pub fn downmix_to_stereo(input: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        0 => {}
        1 => {
            for &s in input {
                out.push(s);
                out.push(s);
            }
        }
        2 => out.extend_from_slice(input),
        n => {
            const C: f32 = std::f32::consts::FRAC_1_SQRT_2;
            for f in input.chunks_exact(n) {
                let (mut l, mut r) = (f[0], f[1]);
                if n >= 3 {
                    l += C * f[2];
                    r += C * f[2];
                }
                // surrounds: even index = left, odd = right (index 3 is LFE)
                for (i, &s) in f.iter().enumerate().skip(4) {
                    if i % 2 == 0 {
                        l += C * s;
                    } else {
                        r += C * s;
                    }
                }
                out.push(l);
                out.push(r);
            }
        }
    }
}

/// Places captured audio packets on a timeline that starts when the video
/// starts. WASAPI loopback delivers nothing while the PC is silent and device
/// clocks drift slightly from the system clock, so the stream is padded with
/// silence or trimmed to stay within a few ms of real time.
pub struct Timeline {
    rate: u64,
    written: u64,
}

impl Timeline {
    pub fn new(sample_rate: u32) -> Self {
        Self { rate: sample_rate.max(1) as u64, written: 0 }
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    fn expected(&self, elapsed: f64) -> u64 {
        (elapsed.max(0.0) * self.rate as f64) as u64
    }

    /// A packet of `frames` arrived `elapsed` seconds after the start.
    /// Returns (silent frames to insert before it, frames to drop from its start).
    pub fn place(&mut self, frames: u64, elapsed: f64) -> (u64, u64) {
        let exp = self.expected(elapsed);
        let tol = self.rate / 50; // 20 ms
        let (mut pad, mut skip) = (0, 0);
        if self.written + frames + tol < exp {
            // gap (silence, or capture started late): the packet ends "now"
            pad = exp - frames - self.written;
        } else if self.written + frames > exp + self.rate / 10 {
            // more than 100 ms ahead (device clock faster than ours)
            skip = (self.written + frames - exp).min(frames);
        }
        self.written += pad + frames - skip;
        (pad, skip)
    }

    /// No packets right now: silence to insert so the stream keeps flowing
    /// (keeps 100 ms of slack for a packet that is merely late).
    pub fn idle(&mut self, elapsed: f64) -> u64 {
        let exp = self.expected(elapsed);
        if self.written + self.rate / 5 < exp {
            let pad = exp - self.rate / 10 - self.written;
            self.written += pad;
            pad
        } else {
            0
        }
    }

    /// Recording stopped: silence needed to reach the end time.
    pub fn finish(&mut self, elapsed: f64) -> u64 {
        let pad = self.expected(elapsed).saturating_sub(self.written);
        self.written += pad;
        pad
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix() {
        let mut o = Vec::new();
        downmix_to_stereo(&[0.5, -0.5], 1, &mut o);
        assert_eq!(o, [0.5, 0.5, -0.5, -0.5]);
        o.clear();
        // 5.1: FL FR FC LFE BL BR
        downmix_to_stereo(&[1.0, 0.0, 0.0, 9.0, 0.0, 1.0], 6, &mut o);
        assert!((o[0] - 1.0).abs() < 1e-6);
        assert!((o[1] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    }

    #[test]
    fn timeline_pads_gaps_and_trims_drift() {
        let mut t = Timeline::new(48000);
        // steady 10 ms packets arriving on time: nothing added or removed
        for i in 1..=100 {
            assert_eq!(t.place(480, i as f64 * 0.01), (0, 0));
        }
        assert_eq!(t.written(), 48000);
        // two seconds of silence (no packets): idle keeps it ~100 ms behind
        let pad = t.idle(3.0);
        assert_eq!(t.written(), 3 * 48000 - 4800);
        assert!(pad > 0);
        // sound resumes: the packet is placed so that it ends "now"
        let (pad, skip) = t.place(480, 3.01);
        assert_eq!(skip, 0);
        assert_eq!(pad, 4800);
        assert_eq!(t.written(), (3.01 * 48000.0) as u64);
        // a device running fast gets trimmed once 100 ms ahead
        let (_, skip) = t.place(48000, 3.02);
        assert!(skip > 0);
        assert!(t.written() <= (3.02 * 48000.0) as u64 + 4800);
        // stop: pad to the end
        t.finish(4.0);
        assert_eq!(t.written(), 4 * 48000);
    }
}

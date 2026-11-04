//! Per-input sound treatment: channel select, noise gate, compressor,
//! graphic equaliser, gain and delay. Works on interleaved stereo blocks.

use crate::model::AudioCfg;
use crate::sources::AUDIO_RATE;
use std::collections::VecDeque;

pub const EQ_BANDS: [f32; 10] = [31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];

#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z: [[f32; 2]; 2],
}

impl Biquad {
    /// Peaking filter of the audio cookbook.
    fn peaking(freq: f32, gain_db: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w = std::f32::consts::TAU * freq / AUDIO_RATE as f32;
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * 1.4);
        let a0 = 1.0 + alpha / a;
        Self {
            b0: (1.0 + alpha * a) / a0,
            b1: -2.0 * c / a0,
            b2: (1.0 - alpha * a) / a0,
            a1: -2.0 * c / a0,
            a2: (1.0 - alpha / a) / a0,
            z: [[0.0; 2]; 2],
        }
    }

    #[inline]
    fn run(&mut self, x: f32, ch: usize) -> f32 {
        let y = self.b0 * x + self.z[ch][0];
        self.z[ch][0] = self.b1 * x - self.a1 * y + self.z[ch][1];
        self.z[ch][1] = self.b2 * x - self.a2 * y;
        y
    }
}

fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn lin_to_db(v: f32) -> f32 {
    20.0 * v.max(1e-6).log10()
}

/// Filter state of one input.
pub struct Chain {
    eq_set: [f32; 10],
    eq: Vec<(usize, Biquad)>,
    gate_level: f32,
    comp_level: f32,
    gate_gain: f32,
    delay: VecDeque<f32>,
}

impl Default for Chain {
    fn default() -> Self {
        Self {
            eq_set: [0.0; 10],
            eq: Vec::new(),
            gate_level: 0.0,
            comp_level: 0.0,
            gate_gain: 1.0,
            delay: VecDeque::new(),
        }
    }
}

impl Chain {
    fn refresh_eq(&mut self, set: &[f32; 10]) {
        if self.eq_set == *set {
            return;
        }
        self.eq_set = *set;
        self.eq = set
            .iter()
            .enumerate()
            .filter(|(_, g)| g.abs() > 0.01)
            .map(|(i, g)| (i, Biquad::peaking(EQ_BANDS[i], *g)))
            .collect();
    }

    /// Treats one block in place. Nothing set means nothing computed.
    pub fn process(&mut self, block: &mut [f32], cfg: &AudioCfg) {
        match cfg.channel {
            1 => block.as_chunks_mut::<2>().0.iter_mut().for_each(|p| p[1] = p[0]),
            2 => block.as_chunks_mut::<2>().0.iter_mut().for_each(|p| p[0] = p[1]),
            _ => {}
        }
        if cfg.gate_threshold > -89.0 {
            self.gate(block, cfg.gate_threshold);
        }
        if cfg.comp_ratio > 1.001 {
            self.compress(block, cfg.comp_ratio, cfg.comp_threshold);
        }
        self.refresh_eq(&cfg.eq);
        for (_, f) in &mut self.eq {
            for p in block.as_chunks_mut::<2>().0 {
                p[0] = f.run(p[0], 0);
                p[1] = f.run(p[1], 1);
            }
        }
        if cfg.gain_db.abs() > 0.01 {
            let g = db_to_lin(cfg.gain_db);
            block.iter_mut().for_each(|v| *v *= g);
        }
        self.apply_delay(block, cfg.delay_ms);
    }

    fn gate(&mut self, block: &mut [f32], threshold_db: f32) {
        let open = db_to_lin(threshold_db);
        for p in block.as_chunks_mut::<2>().0 {
            let level = p[0].abs().max(p[1].abs());
            self.gate_level = if level > self.gate_level { level } else { self.gate_level * 0.9995 };
            let target = if self.gate_level >= open { 1.0 } else { 0.0 };
            self.gate_gain += (target - self.gate_gain) * 0.002;
            p[0] *= self.gate_gain;
            p[1] *= self.gate_gain;
        }
    }

    fn compress(&mut self, block: &mut [f32], ratio: f32, threshold_db: f32) {
        for p in block.as_chunks_mut::<2>().0 {
            let level = p[0].abs().max(p[1].abs());
            let coef = if level > self.comp_level { 0.01 } else { 0.0005 };
            self.comp_level += (level - self.comp_level) * coef;
            let over = lin_to_db(self.comp_level) - threshold_db;
            if over > 0.0 {
                let g = db_to_lin(-over * (1.0 - 1.0 / ratio));
                p[0] *= g;
                p[1] *= g;
            }
        }
    }

    fn apply_delay(&mut self, block: &mut [f32], ms: f32) {
        let want = ((ms.max(0.0) / 1000.0) * AUDIO_RATE as f32) as usize * 2;
        if want == 0 {
            self.delay.clear();
            return;
        }
        while self.delay.len() < want {
            self.delay.push_back(0.0);
        }
        self.delay.truncate(want);
        for v in block.iter_mut() {
            self.delay.push_back(*v);
            *v = self.delay.pop_front().unwrap_or(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freq: f32, amp: f32) -> Vec<f32> {
        (0..4800)
            .flat_map(|n| {
                let v = (std::f32::consts::TAU * freq * n as f32 / AUDIO_RATE as f32).sin() * amp;
                [v, v]
            })
            .collect()
    }

    fn peak(b: &[f32]) -> f32 {
        b[4800..].iter().fold(0.0, |m, v| m.max(v.abs()))
    }

    #[test]
    fn untouched_settings_leave_sound_alone() {
        let mut b = tone(440.0, 0.5);
        let before = b.clone();
        Chain::default().process(&mut b, &AudioCfg::default());
        assert_eq!(b, before);
    }

    #[test]
    fn gain_raises_by_the_right_amount() {
        let mut b = tone(440.0, 0.1);
        let cfg = AudioCfg { gain_db: 6.0, ..AudioCfg::default() };
        Chain::default().process(&mut b, &cfg);
        assert!((peak(&b) - 0.2).abs() < 0.01);
    }

    #[test]
    fn compressor_lowers_loud_sound() {
        let mut b = tone(440.0, 0.9);
        let cfg = AudioCfg { comp_ratio: 4.0, comp_threshold: -20.0, ..AudioCfg::default() };
        Chain::default().process(&mut b, &cfg);
        assert!(peak(&b) < 0.4);
    }

    #[test]
    fn gate_closes_on_quiet_sound() {
        let mut b = tone(440.0, 0.001);
        let cfg = AudioCfg { gate_threshold: -30.0, ..AudioCfg::default() };
        Chain::default().process(&mut b, &cfg);
        assert!(peak(&b) < 0.0002);
    }

    #[test]
    fn equaliser_boosts_its_band_only() {
        let mut eq = [0.0; 10];
        eq[5] = 12.0;
        let cfg = AudioCfg { eq, ..AudioCfg::default() };
        let mut near = tone(1000.0, 0.1);
        Chain::default().process(&mut near, &cfg);
        assert!(peak(&near) > 0.3);
        let mut far = tone(60.0, 0.1);
        Chain::default().process(&mut far, &cfg);
        assert!(peak(&far) < 0.13);
    }

    #[test]
    fn delay_holds_the_sound_back() {
        let mut b = vec![1.0; 200];
        let cfg = AudioCfg { delay_ms: 1.0, ..AudioCfg::default() };
        Chain::default().process(&mut b, &cfg);
        assert_eq!(b[0], 0.0);
        assert_eq!(b[199], 1.0);
    }

    #[test]
    fn left_only_copies_left_to_both() {
        let mut b = vec![0.5, 0.1];
        let cfg = AudioCfg { channel: 1, ..AudioCfg::default() };
        Chain::default().process(&mut b, &cfg);
        assert_eq!(b, [0.5, 0.5]);
    }
}

//! Video Delay: another input shown again a set time later, picture and sound.
//!
//! Pictures are kept as they were shown, in memory, not encoded to a file as
//! the replay does. A delay is often a fraction of a second, to line up a
//! late source, which is less than an encoder and a decoder take to hand a
//! picture back, and it must land on the right picture. Keeping a picture
//! only keeps it alive: a still source holds one picture for the whole delay,
//! and the buffers let go return to the frame pool. A moving source holds one
//! picture a tick, and `BUDGET` bounds that: it sets the longest delay at the
//! project's size and frame rate, which the settings show.
//!
//! The sound is the source's own, taken in the sound loop after its own
//! treatment and before its fader, and held back by the same time to the
//! sample. Both run on the shared clock, so the delayed picture and sound
//! stay together whatever the loops skip.

use crate::frame::Frame;
use crate::sources::AUDIO_RATE;
use std::collections::VecDeque;
use std::sync::Arc;

/// What one delay may hold in pictures, in bytes.
pub const BUDGET: usize = 2 << 30;
/// The longest delay offered, whatever memory allows.
pub const MOST_MS: u32 = 10_000;

fn picture_bytes(w: usize, h: usize) -> usize {
    (w * h * 4).max(1)
}

/// Pictures one delay may hold at this size.
pub fn most_pictures(w: usize, h: usize) -> usize {
    (BUDGET / picture_bytes(w, h)).max(2)
}

/// The longest delay at this size and frame rate, in milliseconds: the
/// picture shown and every one after it must fit in `BUDGET`.
pub fn longest_ms(w: usize, h: usize, fps: u32) -> u32 {
    let ticks = most_pictures(w, h) as u64 - 1;
    (ticks * 1000 / fps.max(1) as u64).min(MOST_MS as u64) as u32
}

/// The most a delay of `ms` holds in pictures, in bytes: a new picture at
/// every tick.
pub fn bytes_held(ms: u32, w: usize, h: usize, fps: u32) -> usize {
    let ticks = (ms.min(longest_ms(w, h, fps)) as usize * fps.max(1) as usize).div_ceil(1000);
    (ticks + 1).min(most_pictures(w, h)) * picture_bytes(w, h)
}

/// A delay in nanoseconds of the shared clock.
pub fn nanos(ms: u32) -> u64 {
    ms as u64 * 1_000_000
}

/// A delay in frames of sound.
pub fn sound_frames(ms: u32) -> usize {
    ms.min(MOST_MS) as usize * AUDIO_RATE / 1000
}

fn same(a: &Option<Arc<Frame>>, b: &Option<Arc<Frame>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

/// What one delay keeps of its source's pictures, oldest first, each with
/// the tick it was first shown at: a picture is kept once for as long as the
/// source shows it. `None` is a moment the source showed nothing.
#[derive(Default)]
pub struct Pictures {
    kept: VecDeque<(u64, Option<Arc<Frame>>)>,
    size: (usize, usize),
}

impl Pictures {
    /// Pictures of another size are of no use: the format changed.
    pub fn fit(&mut self, w: usize, h: usize, let_go: &mut Vec<Arc<Frame>>) {
        if self.size != (w, h) {
            self.size = (w, h);
            let_go.extend(self.kept.drain(..).filter_map(|(_, f)| f));
        }
    }

    /// Notes what the source shows at the tick stamped `at` and returns what
    /// it showed `delay` earlier, to the nearest tick of `period`. Pictures no
    /// longer needed go to `let_go`, for the caller to free where it costs
    /// nothing; at most `most` are kept.
    ///
    /// Asked further back than it holds, as when the delay has just grown or
    /// the input has just started, it shows the oldest picture it has: the
    /// picture holds while the delay fills, as the sound is silent meanwhile.
    pub fn step(
        &mut self,
        at: u64,
        shown: Option<Arc<Frame>>,
        delay: u64,
        period: u64,
        most: usize,
        let_go: &mut Vec<Arc<Frame>>,
    ) -> Option<Arc<Frame>> {
        if self.kept.back().is_none_or(|(_, last)| !same(last, &shown)) {
            self.kept.push_back((at, shown));
        }
        // Half a tick either way picks the nearest one, and a stamp a
        // nanosecond off its tick, as integer division leaves them, still
        // lands on it.
        let wanted = (at + period / 2).saturating_sub(delay);
        let up_to = self.kept.partition_point(|(t, _)| *t <= wanted);
        let first = up_to.saturating_sub(1).max(self.kept.len().saturating_sub(most.max(1)));
        let_go.extend(self.kept.drain(..first).filter_map(|(_, f)| f));
        self.kept.front().and_then(|(_, f)| f.clone())
    }

    /// Pictures held, counting each once.
    #[cfg(test)]
    fn held(&self) -> usize {
        self.kept.len()
    }
}

/// What one delay keeps of its source's sound: interleaved stereo, oldest
/// first, behind `lead` samples of silence still to play.
#[derive(Default)]
pub struct Sound {
    line: VecDeque<f32>,
    lead: usize,
}

impl Sound {
    /// One block of the sound loop: takes what the source sent for it, `None`
    /// for silence, after `skipped` blocks the loop gave up, and fills `out`
    /// with what came `delay` frames earlier.
    ///
    /// A delay that grows plays silence until it is filled, as the picture
    /// holds meanwhile; one that shrinks drops what it no longer needs.
    pub fn step(&mut self, fresh: Option<&[f32]>, skipped: usize, delay: usize, out: &mut [f32]) {
        let block = out.len();
        let delay = delay.min(sound_frames(MOST_MS));
        // The blocks given up went by for the delay too: silence comes in for
        // them, and as much goes out unheard.
        let gap = skipped * block;
        self.line.extend(std::iter::repeat_n(0.0, gap));
        match fresh {
            Some(s) if s.len() == block => self.line.extend(s.iter().copied()),
            _ => self.line.extend(std::iter::repeat_n(0.0, block)),
        }
        let want = delay * 2 + gap + block;
        let held = self.lead + self.line.len();
        if held < want {
            self.lead += want - held;
        } else {
            let cut = held - want;
            let quiet = cut.min(self.lead);
            self.lead -= quiet;
            self.line.drain(..cut - quiet);
        }
        self.take(gap, None);
        self.take(block, Some(out));
    }

    fn take(&mut self, n: usize, out: Option<&mut [f32]>) {
        let quiet = n.min(self.lead);
        self.lead -= quiet;
        let sound = self.line.drain(..n - quiet);
        if let Some(out) = out {
            out[..quiet].fill(0.0);
            out[quiet..].iter_mut().zip(sound).for_each(|(o, s)| *o = s);
        }
    }

    /// Samples held, silence still to play included.
    #[cfg(test)]
    fn held(&self) -> usize {
        self.lead + self.line.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::NS;

    const FPS: u32 = 30;
    const PERIOD: u64 = NS / FPS as u64;

    /// The stamp of tick `n`, as the mixer's pace gives them.
    fn tick(n: u64) -> u64 {
        5 * NS + (n as u128 * NS as u128 / FPS as u128) as u64
    }

    /// A picture that says which tick it was shown at.
    fn numbered(n: u64) -> Arc<Frame> {
        let mut f = Frame::new(1, 1);
        f.data[..4].copy_from_slice(&(n as u32).to_le_bytes());
        Arc::new(f)
    }

    fn number(f: &Option<Arc<Frame>>) -> Option<u64> {
        f.as_ref().map(|f| u32::from_le_bytes(f.data[..4].try_into().unwrap()) as u64)
    }

    /// Runs a source that shows a new picture each tick from `from` to `to`,
    /// and returns what the delay shows at each.
    fn run(p: &mut Pictures, from: u64, to: u64, ms: impl Fn(u64) -> u32) -> Vec<Option<u64>> {
        let mut let_go = Vec::new();
        (from..to)
            .map(|n| number(&p.step(tick(n), Some(numbered(n)), nanos(ms(n)), PERIOD, 1000, &mut let_go)))
            .collect()
    }

    #[test]
    fn a_moving_source_comes_back_the_set_number_of_ticks_later() {
        let mut p = Pictures::default();
        let shown = run(&mut p, 0, 100, |_| 500);
        for n in 15..100 {
            assert_eq!(shown[n], Some(n as u64 - 15), "tick {n}");
        }
        assert!(shown[..15].iter().all(|s| *s == Some(0)), "the first picture holds while the delay fills");
        assert_eq!(p.held(), 16, "the picture shown and the fifteen after it");
    }

    /// Half a frame at most either way: 510 ms is 15.3 ticks, 520 ms 15.6.
    #[test]
    fn a_delay_between_ticks_lands_on_the_nearest_picture() {
        for (ms, ticks) in [(510, 15), (520, 16), (0, 0), (33, 1), (16, 0), (17, 1)] {
            let mut p = Pictures::default();
            let shown = run(&mut p, 0, 60, |_| ms);
            assert_eq!(shown[59], Some(59 - ticks), "{ms} ms");
        }
    }

    #[test]
    fn a_still_source_costs_one_picture() {
        let mut p = Pictures::default();
        let still = numbered(7);
        let mut let_go = Vec::new();
        for n in 0..300 {
            let out = p.step(tick(n), Some(still.clone()), nanos(5000), PERIOD, 1000, &mut let_go);
            assert!(out.is_some_and(|f| Arc::ptr_eq(&f, &still)));
        }
        assert_eq!(p.held(), 1);
    }

    #[test]
    fn pictures_held_stay_within_the_budget() {
        let mut p = Pictures::default();
        let mut let_go = Vec::new();
        let mut last = None;
        for n in 0..200 {
            last = number(&p.step(tick(n), Some(numbered(n)), nanos(9000), PERIOD, 10, &mut let_go));
            assert!(p.held() <= 10, "tick {n}: {}", p.held());
        }
        assert_eq!(last, Some(190), "the oldest picture memory allows");
        assert_eq!(let_go.len(), 190, "the rest is let go for the caller to free");
    }

    /// The memory a setting costs is what the settings say, and the longest
    /// delay offered fits in it.
    #[test]
    fn the_longest_delay_follows_size_and_rate() {
        assert_eq!(longest_ms(1920, 1080, 30), 8566);
        assert_eq!(longest_ms(1920, 1080, 60), 4283);
        assert_eq!(longest_ms(3840, 2160, 30), 2100);
        assert_eq!(longest_ms(1280, 720, 30), MOST_MS, "smaller pictures stop at ten seconds");
        for (w, h, fps) in [(1920, 1080, 30), (1920, 1080, 60), (3840, 2160, 25), (1280, 720, 50)] {
            let longest = longest_ms(w, h, fps);
            assert!(bytes_held(longest, w, h, fps) <= BUDGET, "{w}x{h} {fps}");
            let ticks = (longest as f64 * fps as f64 / 1000.0).round() as usize;
            assert!(ticks < most_pictures(w, h), "{w}x{h} {fps}: the picture shown fits too");
        }
        assert_eq!(bytes_held(1000, 1920, 1080, 30), 31 * 1920 * 1080 * 4);
    }

    #[test]
    fn a_longer_delay_holds_the_picture_then_carries_on() {
        let mut p = Pictures::default();
        let shown = run(&mut p, 0, 100, |n| if n < 50 { 200 } else { 500 });
        assert_eq!(shown[49], Some(43));
        // Ticks before 43 were let go: the picture on screen holds until the
        // delay is filled, then the source goes on from there.
        assert!(shown[50..59].iter().all(|s| *s == Some(43)), "{:?}", &shown[50..60]);
        assert_eq!(shown[59], Some(44));
        assert_eq!(shown[60], Some(45));
        assert_eq!(shown[99], Some(84));
    }

    #[test]
    fn a_shorter_delay_jumps_ahead_and_lets_pictures_go() {
        let mut p = Pictures::default();
        let mut let_go = Vec::new();
        for n in 0..60 {
            p.step(tick(n), Some(numbered(n)), nanos(1000), PERIOD, 1000, &mut let_go);
        }
        let before = let_go.len();
        let out = p.step(tick(60), Some(numbered(60)), nanos(100), PERIOD, 1000, &mut let_go);
        assert_eq!(number(&out), Some(57));
        assert_eq!(let_go.len() - before, 28, "from tick 29 to 56");
        assert_eq!(p.held(), 4);
    }

    #[test]
    fn a_source_that_went_away_leaves_black_once_its_pictures_have_played() {
        let mut p = Pictures::default();
        let mut let_go = Vec::new();
        let mut shown = Vec::new();
        for n in 0..60 {
            let source = (n < 30).then(|| numbered(n));
            shown.push(number(&p.step(tick(n), source, nanos(300), PERIOD, 1000, &mut let_go)));
        }
        assert_eq!(shown[38], Some(29), "what it showed before going still plays");
        assert_eq!(shown[39], None);
        assert_eq!(shown[59], None);
    }

    #[test]
    fn late_ticks_keep_their_time() {
        let mut p = Pictures::default();
        let mut let_go = Vec::new();
        // Ticks 20 to 29 never ran: the mixer gave them up.
        let ticks: Vec<u64> = (0..20).chain(30..80).collect();
        let mut shown = Vec::new();
        for &n in &ticks {
            shown.push((
                n,
                number(&p.step(tick(n), Some(numbered(n)), nanos(500), PERIOD, 1000, &mut let_go)),
            ));
        }
        for (n, s) in shown {
            let expected = match n.saturating_sub(15) {
                k if (20..30).contains(&k) => 19,
                k => k,
            };
            assert_eq!(s, Some(expected), "tick {n}");
        }
    }

    const BLOCK: usize = 1920;

    /// Blocks numbering each stereo frame of the source from `first`.
    fn ramp(block: u64) -> Vec<f32> {
        (0..BLOCK / 2).flat_map(|n| [(block as usize * BLOCK / 2 + n) as f32 + 1.0; 2]).collect()
    }

    /// The source frame each output frame carries, 0 for silence.
    fn frames(out: &[f32]) -> Vec<u64> {
        out.iter().step_by(2).map(|v| *v as u64).collect()
    }

    #[test]
    fn sound_comes_back_exactly_the_set_number_of_frames_later() {
        let mut s = Sound::default();
        let delay = 24_000 + 7;
        let mut out = vec![0f32; BLOCK];
        let mut heard = Vec::new();
        for b in 0..60 {
            s.step(Some(&ramp(b)), 0, delay, &mut out);
            heard.extend(frames(&out));
            assert_eq!(s.held(), delay * 2, "block {b}");
        }
        assert!(heard[..delay].iter().all(|v| *v == 0), "silence while the delay fills");
        for (n, v) in heard.iter().enumerate().skip(delay) {
            assert_eq!(*v as usize, n - delay + 1, "frame {n}");
        }
    }

    #[test]
    fn blocks_the_loop_gave_up_keep_the_sound_in_step() {
        let mut s = Sound::default();
        let delay = 4800;
        let mut out = vec![0f32; BLOCK];
        let mut heard: Vec<(usize, u64)> = Vec::new();
        let mut b = 0;
        while b < 40 {
            // Blocks 10, 11 and 12 never ran.
            let skipped = if b == 13 { 3 } else { 0 };
            s.step(Some(&ramp(b as u64)), skipped, delay, &mut out);
            let at = b * BLOCK / 2;
            heard.extend(frames(&out).into_iter().enumerate().map(|(n, v)| (at + n, v)));
            b += if b == 9 { 4 } else { 1 };
        }
        for (at, v) in heard {
            let source = at.checked_sub(delay);
            let lost = source.is_some_and(|f| (10 * BLOCK / 2..13 * BLOCK / 2).contains(&f));
            let expected = match source {
                Some(f) if !lost => f as u64 + 1,
                _ => 0,
            };
            assert_eq!(v, expected, "frame {at}");
        }
    }

    #[test]
    fn a_longer_delay_plays_silence_then_carries_on_and_a_shorter_one_jumps() {
        let mut s = Sound::default();
        let mut out = vec![0f32; BLOCK];
        for b in 0..20 {
            s.step(Some(&ramp(b)), 0, 960, &mut out);
        }
        // Block 19 is the next one due at one block of delay; three blocks
        // more of delay put three blocks of silence before it.
        s.step(Some(&ramp(20)), 0, 960 + 2880, &mut out);
        assert!(frames(&out).iter().all(|v| *v == 0), "three blocks of silence for the time added");
        for b in 21..23 {
            s.step(Some(&ramp(b)), 0, 3840, &mut out);
            assert!(frames(&out).iter().all(|v| *v == 0));
        }
        s.step(Some(&ramp(23)), 0, 3840, &mut out);
        assert_eq!(frames(&out)[0], 19 * 960 + 1, "then the sound goes on from where it was");
        s.step(Some(&ramp(24)), 0, 0, &mut out);
        assert_eq!(frames(&out)[0], 24 * 960 + 1, "no delay: the block itself");
        assert_eq!(s.held(), 0);
    }

    #[test]
    fn a_silent_source_makes_a_silent_delay() {
        let mut s = Sound::default();
        let mut out = vec![1f32; BLOCK];
        for _ in 0..10 {
            s.step(None, 0, 1000, &mut out);
        }
        assert!(out.iter().all(|v| *v == 0.0));
        assert_eq!(s.held(), 2000);
    }
}

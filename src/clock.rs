//! One clock for sound and picture. The picture loop, the sound loop and every
//! encoder read the same clock, and what the loops make is stamped with the
//! time it stands for on it. An encoder places each picture and each block of
//! sound by its stamp instead of counting what arrived, so a late tick, a full
//! pipe or a skipped block costs a repeated picture or a moment of silence,
//! never a gap between sound and picture that grows through a long recording.

use crate::sources::AUDIO_RATE;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// The clock counts nanoseconds.
pub const NS: u64 = 1_000_000_000;

static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Now, on the shared clock.
pub fn now() -> u64 {
    ORIGIN.elapsed().as_nanos() as u64
}

/// The moment a time on the clock stands for.
pub fn instant(t: u64) -> Instant {
    *ORIGIN + Duration::from_nanos(t)
}

/// Where `at` falls in a stream of `rate` units a second that began at
/// `origin`, to the nearest unit: a stamp a nanosecond off its tick, as
/// integer division leaves them, still lands on that tick.
fn position(at: u64, origin: u64, rate: u64) -> i64 {
    let since = at as i128 - origin as i128;
    (since * rate as i128 + NS as i128 / 2).div_euclid(NS as i128) as i64
}

/// Paces a loop making `rate` things a second. Each tick stands for a fixed
/// time on the clock, so running late never shifts the ticks after it.
pub struct Pace {
    start: u64,
    rate: u64,
    n: u64,
}

pub struct Step {
    pub wait: Duration,
    /// Ticks given up because the loop fell too far behind.
    pub skipped: u64,
}

impl Pace {
    pub fn new(now: u64, rate: u32) -> Self {
        Self { start: now, rate: rate.max(1) as u64, n: 0 }
    }

    fn at(&self, n: u64) -> u64 {
        self.start + (n as u128 * NS as u128 / self.rate as u128) as u64
    }

    /// The time the current tick stands for.
    pub fn stamp(&self) -> u64 {
        self.at(self.n)
    }

    /// A new rate takes over from the current tick.
    pub fn set_rate(&mut self, rate: u32) {
        let rate = rate.max(1) as u64;
        if rate != self.rate {
            self.start = self.stamp();
            self.n = 0;
            self.rate = rate;
        }
    }

    /// Moves to the next tick and says how long until it is due. Ticks more
    /// than `catch_up` behind are given up, up to the one under way: a long
    /// stall is skipped, not replayed at full speed.
    pub fn advance(&mut self, now: u64, catch_up: u64) -> Step {
        self.n += 1;
        let due = self.at(self.n);
        if now < due {
            return Step { wait: Duration::from_nanos(due - now), skipped: 0 };
        }
        if now - due <= catch_up {
            return Step { wait: Duration::ZERO, skipped: 0 };
        }
        let under_way = ((now - self.start) as u128 * self.rate as u128 / NS as u128) as u64;
        let skipped = under_way.saturating_sub(self.n);
        self.n += skipped;
        Step { wait: Duration::ZERO, skipped }
    }
}

/// An encoder's pictures, one per frame period from the moment it started.
pub struct FrameLine {
    origin: u64,
    fps: u64,
    written: u64,
}

impl FrameLine {
    pub fn new(origin: u64, fps: u32) -> Self {
        Self { origin, fps: fps.max(1) as u64, written: 0 }
    }

    /// For a picture stamped `at`: how many times to repeat the picture before
    /// it to fill the periods nothing arrived for, or `None` when its period
    /// is already written and it must be left out.
    pub fn place(&mut self, at: u64) -> Option<u64> {
        let slot = position(at, self.origin, self.fps);
        if slot < self.written as i64 {
            return None;
        }
        let gap = slot as u64 - self.written;
        self.written = slot as u64 + 1;
        Some(gap)
    }

    pub fn written(&self) -> u64 {
        self.written
    }
}

/// How a block of sound joins an encoder's stream.
#[derive(Debug, PartialEq)]
pub struct Fit {
    /// Frames of silence first, for sound that never arrived.
    pub silence: u64,
    /// Frames at the start of the block that the stream already holds.
    pub skip: usize,
}

/// An encoder's sound, one frame per sample period from the moment it started.
pub struct SampleLine {
    origin: u64,
    written: u64,
}

impl SampleLine {
    pub fn new(origin: u64) -> Self {
        Self { origin, written: 0 }
    }

    /// Places a block of `frames` stamped `at`, the time of its first frame.
    pub fn place(&mut self, at: u64, frames: usize) -> Fit {
        let pos = position(at, self.origin, AUDIO_RATE as u64);
        let fit = if pos >= self.written as i64 {
            Fit { silence: pos as u64 - self.written, skip: 0 }
        } else {
            Fit { silence: 0, skip: ((self.written as i64 - pos) as u64).min(frames as u64) as usize }
        };
        self.written += fit.silence + (frames - fit.skip) as u64;
        fit
    }

    pub fn written(&self) -> u64 {
        self.written
    }
}

/// Where an encoder's picture and sound both end, as (pictures, samples):
/// the later of the two, on a picture boundary, so neither stops short.
pub fn even_end(frames: u64, samples: u64, fps: u32) -> (u64, u64) {
    let (fps, rate) = (fps.max(1) as u128, AUDIO_RATE as u128);
    let frames = frames.max((samples as u128 * fps).div_ceil(rate) as u64);
    (frames, (frames as u128 * rate).div_ceil(fps) as u64)
}

/// Sound a live input holds beyond the block about to be taken, in frames:
/// enough to ride out uneven deliveries, little enough to stay with the picture.
const SPARE_LOW: i64 = AUDIO_RATE as i64 / 200; // 5 ms
const SPARE_HIGH: i64 = AUDIO_RATE as i64 * 3 / 200; // 15 ms
const SPARE_TARGET: i64 = (SPARE_LOW + SPARE_HIGH) / 2;
/// Beyond this, the input is cut back at once rather than eased back.
const SPARE_CUT: i64 = AUDIO_RATE as i64 / 10; // 100 ms
/// Blocks watched before deciding: devices deliver in bursts, and only the
/// least sound left over a whole second says how much is really extra.
const WINDOW: u32 = 50;

/// What to take from a live input's ring for one block.
#[derive(Debug, PartialEq)]
pub struct Take {
    /// Old frames to throw away first.
    pub discard: usize,
    /// Frames to spread over the block: one more or one less than a block
    /// changes the speed by a thousandth, which nobody hears.
    pub frames: usize,
}

/// Keeps a live input in step with the clock. A microphone or a network
/// stream runs on its own clock, a little faster or slower than ours: left
/// alone, a fast one piles up sound that is heard later and later, up to a
/// second behind the picture, and a slow one runs dry and clicks.
pub struct Drift {
    low: i64,
    seen: u32,
    held: bool,
    step: i64,
    resync: bool,
}

impl Default for Drift {
    fn default() -> Self {
        Self { low: i64::MAX, seen: 0, held: false, step: 0, resync: false }
    }
}

impl Drift {
    /// After the sound loop skipped ahead, whatever waited is old: the next
    /// block cuts back to the margin straight away.
    pub fn resync(&mut self) {
        self.resync = true;
    }

    /// `queued` is what the input's ring holds before taking a block of
    /// `block`. `held` says the source is waiting for room in it, as a sender
    /// streaming a recording faster than it plays does: that is sound still to
    /// come, like a file's, and none of it is cut.
    pub fn take(&mut self, queued: usize, block: usize, held: bool) -> Take {
        let spare = queued as i64 - block as i64;
        if std::mem::take(&mut self.resync) && !held {
            *self = Self::default();
            let discard = (spare - SPARE_TARGET).max(0) as usize;
            return Take { discard, frames: block };
        }
        self.low = self.low.min(spare);
        self.held |= held;
        self.seen += 1;
        let mut discard = 0;
        if self.seen == WINDOW {
            self.step = match self.low {
                _ if self.held => 0,
                low if low > SPARE_CUT => {
                    discard = (low - SPARE_TARGET) as usize;
                    0
                }
                low if low > SPARE_HIGH => 1,
                low if low < SPARE_LOW => -1,
                _ => 0,
            };
            (self.low, self.seen, self.held) = (i64::MAX, 0, false);
        }
        Take { discard, frames: (block as i64 + self.step) as usize }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    const MS: u64 = 1_000_000;
    const FPS: u32 = 30;
    const BLOCK: usize = AUDIO_RATE / 50;

    /// The same numbers on every run.
    struct Dice(u64);

    impl Dice {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % n
        }
    }

    /// How far sound strays from picture in what the encoder received.
    #[derive(Debug)]
    struct Report {
        /// Lowest and highest offset, sound after picture, in ms.
        span: (f64, f64),
        first_hour_mean: f64,
        last_hour_mean: f64,
        picture_s: f64,
        sound_s: f64,
    }

    /// A pipe that stops reading for a moment now and then, as an encoder
    /// does while the disk or the processor is busy: 250 ms every three
    /// minutes, and 3 s, longer than the sound queue, every fifty.
    fn pipe_stalled(t: u64) -> bool {
        t % (181 * NS) < 250 * MS || t % (3001 * NS) < 3 * NS
    }

    struct Pipe<T> {
        queue: VecDeque<T>,
        cap: usize,
    }

    impl<T> Pipe<T> {
        /// What the writer gets to see now, in order; a full queue drops the
        /// newest, as `try_send` does.
        fn push(&mut self, now: u64, item: T) -> Vec<T> {
            if pipe_stalled(now) {
                if self.queue.len() < self.cap {
                    self.queue.push_back(item);
                }
                return Vec::new();
            }
            let mut out: Vec<T> = self.queue.drain(..).collect();
            out.push(item);
            out
        }
    }

    /// Hours of a show through the real pacing, drift and placing code: a
    /// microphone `mic_ppm` off our clock in 10 ms deliveries, pictures that
    /// take too long one tick in fifty, a sound loop that stalls now and then,
    /// and an encoder whose pipes stop reading for a while.
    fn simulate(hours: u64, mic_ppm: f64) -> Report {
        let end = hours * 3600 * NS;
        let origin = 5 * NS + 370_000;
        let mut dice = Dice(42);

        let mic_rate = AUDIO_RATE as f64 * (1.0 + mic_ppm * 1e-6);
        let captured = |frame: u64| (frame as f64 / mic_rate * NS as f64) as i64;
        let (mut delivered, mut head, mut queued) = (0u64, 0u64, 0u64);

        let (mut sound_pace, mut picture_pace) = (Pace::new(0, 50), Pace::new(0, FPS));
        let (mut sound_at, mut picture_at) = (0u64, 0u64);
        let mut drift = Drift::default();
        let (mut short_stall, mut long_stall) = (1201 * NS, 4001 * NS);

        let mut pictures = Pipe { queue: VecDeque::new(), cap: 6 };
        let mut sound = Pipe { queue: VecDeque::new(), cap: 100 };
        let mut frames = FrameLine::new(origin, FPS);
        let mut samples = SampleLine::new(origin);
        let mut picture_lag: Option<i64> = None;

        let mut span = (f64::MAX, f64::MIN);
        let mut hours_seen: Vec<(f64, u64)> = vec![(0.0, 0); hours as usize];

        while sound_at.min(picture_at) < end {
            if sound_at <= picture_at {
                let t = sound_at;
                while captured(delivered + 480) as u64 <= t {
                    delivered += 480;
                    queued += 480;
                    // The ring holds one second and drops the oldest.
                    let over = queued.saturating_sub(AUDIO_RATE as u64);
                    (head, queued) = (head + over, queued - over);
                }
                let take = drift.take(queued as usize, BLOCK, false);
                let gone = (take.discard as u64).min(queued);
                (head, queued) = (head + gone, queued - gone);
                let have = (take.frames as u64).min(queued);
                let first = (have > 0).then_some(head);
                (head, queued) = (head + have, queued - have);

                let mut done = t + 200_000;
                if done >= short_stall {
                    short_stall += 1201 * NS;
                    done += 300 * MS;
                }
                if done >= long_stall {
                    long_stall += 4001 * NS;
                    done += 1200 * MS;
                }
                let stamp = sound_pace.stamp();
                if done >= origin {
                    for (at, first, took) in sound.push(done, (stamp, first, take.frames)) {
                        let fit = samples.place(at, BLOCK);
                        let (Some(first), Some(picture)) = (first, picture_lag) else { continue };
                        if fit.skip == BLOCK || at < origin + 10 * NS {
                            continue;
                        }
                        let pos = samples.written() - (BLOCK - fit.skip) as u64;
                        let frame = first + fit.skip as u64 * took as u64 / BLOCK as u64;
                        let heard = (origin + pos * NS / AUDIO_RATE as u64) as i64 - captured(frame);
                        let offset = (heard - picture) as f64 / MS as f64;
                        span = (span.0.min(offset), span.1.max(offset));
                        let hour = &mut hours_seen[(at / (3600 * NS)) as usize];
                        *hour = (hour.0 + offset, hour.1 + 1);
                    }
                }
                let step = sound_pace.advance(done, 500 * MS);
                if step.skipped > 0 {
                    drift.resync();
                }
                sound_at = done + step.wait.as_nanos() as u64 + dice.below(1_500_000);
            } else {
                let t = picture_at;
                let stamp = picture_pace.stamp();
                let busy = if dice.below(50) == 0 { 35 + dice.below(85) } else { 3 + dice.below(10) };
                let done = t + busy * MS;
                if done >= origin {
                    for (at, shown) in pictures.push(done, (stamp, t)) {
                        if frames.place(at).is_some() {
                            let slot = frames.written() - 1;
                            picture_lag = Some((origin + slot * NS / FPS as u64) as i64 - shown as i64);
                        }
                    }
                }
                let step = picture_pace.advance(done, 0);
                picture_at = done + step.wait.as_nanos() as u64 + dice.below(1_000_000);
            }
        }
        let mean = |h: &(f64, u64)| h.0 / h.1.max(1) as f64;
        Report {
            span,
            first_hour_mean: mean(&hours_seen[0]),
            last_hour_mean: mean(&hours_seen[hours as usize - 1]),
            picture_s: frames.written() as f64 / FPS as f64,
            sound_s: samples.written() as f64 / AUDIO_RATE as f64,
        }
    }

    fn check(r: &Report) {
        let frame_ms = 1000.0 / FPS as f64;
        // Sound comes a block and the device's delivery after the picture; what
        // matters is that it stays there, however long the show.
        assert!(r.span.0 > -frame_ms && r.span.1 < 2.0 * frame_ms + 20.0, "{r:?}");
        assert!((r.last_hour_mean - r.first_hour_mean).abs() < 2.0, "{r:?}");
        assert!((r.picture_s - r.sound_s).abs() * 1000.0 < frame_ms, "{r:?}");
    }

    #[test]
    fn hours_with_a_fast_microphone_and_late_pictures_stay_in_step() {
        check(&simulate(6, 300.0));
    }

    #[test]
    fn hours_with_a_slow_microphone_and_late_pictures_stay_in_step() {
        check(&simulate(6, -300.0));
    }

    #[test]
    fn a_late_picture_tick_is_skipped_not_shifted() {
        let mut p = Pace::new(0, 25);
        let step = p.advance(10 * MS, 0);
        assert_eq!((step.wait, step.skipped), (Duration::from_millis(30), 0));
        // Done 130 ms in: tick 2 is gone, tick 3 started at 120 ms.
        let step = p.advance(130 * MS, 0);
        assert_eq!((step.wait, step.skipped, p.stamp()), (Duration::ZERO, 1, 120 * MS));
        let step = p.advance(130 * MS, 0);
        assert_eq!(step.wait, Duration::from_millis(30));
    }

    #[test]
    fn sound_catches_up_a_short_delay_and_skips_a_long_one() {
        let mut p = Pace::new(0, 50);
        assert_eq!(p.advance(300 * MS, 500 * MS).skipped, 0);
        assert_eq!(p.stamp(), 20 * MS, "every block still comes, back to back");
        let step = p.advance(2 * NS, 500 * MS);
        assert_eq!((step.skipped, p.stamp()), (98, 2 * NS));
    }

    #[test]
    fn missing_pictures_are_filled_and_extra_ones_left_out() {
        let mut line = FrameLine::new(1000, 25);
        assert_eq!(line.place(1000), Some(0));
        assert_eq!(line.place(1000 + 40 * MS - 1), Some(0), "a nanosecond early is still its tick");
        assert_eq!(line.place(1000 + 160 * MS), Some(2));
        assert_eq!(line.place(1000 + 150 * MS), None);
        assert_eq!(line.written(), 5);
    }

    #[test]
    fn a_first_picture_after_the_start_covers_the_start() {
        let mut line = FrameLine::new(0, 30);
        assert_eq!(line.place(100 * MS), Some(3));
    }

    #[test]
    fn lost_sound_becomes_silence_and_overlap_is_trimmed() {
        let mut line = SampleLine::new(0);
        assert_eq!(line.place(0, 960), Fit { silence: 0, skip: 0 });
        assert_eq!(line.place(60 * MS, 960), Fit { silence: 1920, skip: 0 });
        assert_eq!(line.place(70 * MS, 960), Fit { silence: 0, skip: 480 });
        assert_eq!(line.place(0, 960), Fit { silence: 0, skip: 960 });
        assert_eq!(line.written(), 4320);
    }

    #[test]
    fn sound_from_before_the_start_is_cut_at_the_start() {
        let mut line = SampleLine::new(10 * MS);
        assert_eq!(line.place(0, 960), Fit { silence: 0, skip: 480 });
        assert_eq!(line.written(), 480);
    }

    #[test]
    fn both_streams_end_together() {
        assert_eq!(even_end(300, 480_000, 30), (300, 480_000));
        assert_eq!(even_end(300, 480_100, 30), (301, 481_600));
        assert_eq!(even_end(302, 480_000, 30), (302, 483_200));
    }

    #[test]
    fn a_live_input_that_piled_up_is_cut_back_then_eased() {
        let mut d = Drift::default();
        let mut takes: Vec<Take> = (0..WINDOW).map(|_| d.take(960 + 9600, 960, false)).collect();
        let last = takes.pop().unwrap();
        assert!(takes.iter().all(|t| *t == Take { discard: 0, frames: 960 }));
        assert_eq!(last, Take { discard: 9600 - SPARE_TARGET as usize, frames: 960 });
        for _ in 0..WINDOW {
            d.take(960 + 1000, 960, false);
        }
        assert_eq!(d.take(960 + 1000, 960, false).frames, 961, "a little extra is eased away");
        for _ in 0..WINDOW {
            d.take(900, 960, false);
        }
        assert_eq!(d.take(900, 960, false).frames, 959, "running dry stretches");
    }

    /// A sender streaming a recording runs ahead until its ring is full and
    /// then waits: what it holds is sound to come, which must all be heard.
    #[test]
    fn a_sender_waiting_for_room_is_left_alone() {
        let mut d = Drift::default();
        for n in 0..3 * WINDOW {
            let take = d.take(48_000, 960, n % 7 == 0);
            assert_eq!(take, Take { discard: 0, frames: 960 });
        }
        d.resync();
        assert_eq!(d.take(48_000, 960, true), Take { discard: 0, frames: 960 });
    }
}

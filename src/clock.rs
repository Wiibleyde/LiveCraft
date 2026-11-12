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
    cut: u64,
}

impl FrameLine {
    pub fn new(origin: u64, fps: u32) -> Self {
        Self { origin, fps: fps.max(1) as u64, written: 0, cut: 0 }
    }

    /// For a picture stamped `at`: how many times to repeat the picture before
    /// it to fill the periods nothing arrived for, or `None` when its period
    /// is already written and it must be left out.
    #[cfg(test)]
    pub fn place(&mut self, at: u64) -> Option<u64> {
        self.place_within(at, u64::MAX)
    }

    /// As `place`, repeating at most `most` times: the rest of the gap is cut
    /// from the line, and the sound must lose as much (`cut_ns`) to stay with it.
    pub fn place_within(&mut self, at: u64, most: u64) -> Option<u64> {
        let slot = position(at, self.origin + self.cut_ns(), self.fps);
        if slot < self.written as i64 {
            return None;
        }
        let gap = slot as u64 - self.written;
        let cut = gap.saturating_sub(most);
        self.cut += cut;
        self.written = slot as u64 + 1 - cut;
        Some(gap - cut)
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    /// Pictures cut from the line so far.
    pub fn cut(&self) -> u64 {
        self.cut
    }

    /// The time cut from the line so far, on the clock.
    pub fn cut_ns(&self) -> u64 {
        (self.cut as u128 * NS as u128 / self.fps as u128) as u64
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
const SPARE_TARGET: i64 = AUDIO_RATE as i64 / 100; // 10 ms
/// Beyond this, the input is cut back at once rather than eased back.
const SPARE_CUT: i64 = AUDIO_RATE as i64 / 10; // 100 ms
/// Blocks watched before deciding: devices deliver in bursts, and only the
/// least sound left over a whole second says how much is really extra.
const WINDOW: u32 = 50;
/// Seconds whose least sound is remembered: a device that runs late now and
/// then needs that margin, and easing it away would make it run dry again.
const MEMORY: usize = 10;
/// How strongly the speed follows the margin, per second: about a 40 s time
/// constant. Fast enough to follow a device 300 ppm off, slow enough that the
/// speed changes by a few dozen ppm at a time. A tone keeps its pitch, which a
/// whole second taken a frame per block off (a thousandth) would not.
const PULL: f64 = 0.05;
/// Frames per block of speed change per frame the margin is off.
const GAIN_P: f64 = PULL / WINDOW as f64;
/// Frames per block the learnt speed moves per frame off, each second: with
/// `GAIN_P`, critically damped, so it settles without swinging.
const GAIN_I: f64 = PULL * PULL / 4.0 / WINDOW as f64;
/// At most a frame more or less per block: 1/960, about 1041 ppm. A device
/// further off than that cannot be followed: a faster one is cut back at
/// `SPARE_CUT` now and then, a slower one runs dry and gets its margin back
/// as a moment of silence. Real devices are within a few hundred ppm.
const MOST_ADJUST: f64 = 1.0;

/// What to take from a live input's ring for one block.
#[derive(Debug, PartialEq)]
pub struct Take {
    /// Old frames to throw away first.
    pub discard: usize,
    /// Frames to take and spread over the block after its silence: one more
    /// or one less than that changes the speed by a thousandth, which nobody
    /// hears.
    pub frames: usize,
    /// Frames of silence the block starts with: the margin the input lacked,
    /// rebuilt at once.
    pub silence: usize,
}

/// Keeps a live input in step with the clock. A microphone or a network
/// stream runs on its own clock, a little faster or slower than ours: left
/// alone, a fast one piles up sound that is heard later and later, up to a
/// second behind the picture, and a slow one runs dry and clicks.
pub struct Drift {
    low: i64,
    seen: u32,
    held: bool,
    lows: Vec<i64>,
    /// The input's speed against our clock as learnt, in frames per block.
    rate: f64,
    /// Frames per block taken beyond the block: the speed, plus a pull back
    /// towards the margin.
    adjust: f64,
    /// The part of a frame not taken yet.
    owed: f64,
    resync: bool,
}

impl Default for Drift {
    fn default() -> Self {
        Self {
            low: i64::MAX,
            seen: 0,
            held: false,
            lows: Vec::new(),
            rate: 0.0,
            adjust: 0.0,
            owed: 0.0,
            resync: false,
        }
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
        let mut take = Take { discard: 0, frames: block, silence: 0 };
        if std::mem::take(&mut self.resync) && !held {
            // The device's clock did not change: its speed is kept.
            self.restart_window(SPARE_TARGET);
            take.discard = (spare - SPARE_TARGET).max(0) as usize;
            return take;
        }
        self.low = self.low.min(spare);
        self.held |= held;
        self.seen += 1;
        if self.seen == WINDOW {
            let (low, held) = (self.low, self.held);
            (self.low, self.seen, self.held) = (i64::MAX, 0, false);
            if held {
                self.adjust = 0.0;
                self.lows.clear();
            } else if low > SPARE_CUT {
                take.discard = (low - SPARE_TARGET) as usize;
                self.restart_window(SPARE_TARGET);
            } else if low < 0 {
                // It ran dry, at the start or after a late delivery: that gap
                // is there already, and taking the margin with it spares the
                // seconds a frame per block would take to build it up.
                take.silence = (SPARE_TARGET as usize).min(block);
                take.frames = block - take.silence;
                self.restart_window(SPARE_TARGET);
            } else {
                self.lows.push(low);
                if self.lows.len() > MEMORY {
                    self.lows.remove(0);
                }
                let off = (self.lows.iter().min().copied().unwrap_or(low) - SPARE_TARGET) as f64;
                self.rate = (self.rate + GAIN_I * off).clamp(-MOST_ADJUST, MOST_ADJUST);
                self.adjust = (self.rate + GAIN_P * off).clamp(-MOST_ADJUST, MOST_ADJUST);
            }
        }
        if take.silence == 0 {
            self.owed += self.adjust;
            let step = self.owed.trunc();
            self.owed -= step;
            take.frames = (block as i64 + step as i64) as usize;
        }
        take
    }

    fn restart_window(&mut self, margin: i64) {
        (self.low, self.seen, self.held) = (i64::MAX, 0, false);
        self.lows = vec![margin];
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
                let first = (have > 0 && take.silence == 0).then_some(head);
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

    /// A stream 10 s stalled repeats half a second, not 250 pictures; the
    /// rest is cut from pictures and sound alike, which stay together.
    #[test]
    fn a_long_gap_in_a_stream_is_cut_from_both() {
        let mut pictures = FrameLine::new(0, 25);
        let mut sound = SampleLine::new(0);
        assert_eq!(pictures.place_within(0, 12), Some(0));
        assert_eq!(sound.place(0, 960), Fit { silence: 0, skip: 0 });
        assert_eq!(pictures.place_within(10 * NS, 12), Some(12));
        assert_eq!(pictures.cut(), 237);
        assert_eq!(pictures.place_within(10 * NS + 40 * MS, 12), Some(0));
        let cut = pictures.cut_ns();
        assert_eq!(cut, 237 * 40 * MS);
        // The picture and the sound of 10.04 s start at the same place in the file.
        assert_eq!(sound.place(10 * NS + 40 * MS - cut, 960), Fit { silence: 14 * 1920 - 960, skip: 0 });
        assert_eq!((pictures.written() - 1) * 1920, sound.written() - 960);
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
        let block = Take { discard: 0, frames: 960, silence: 0 };
        let mut d = Drift::default();
        let mut takes: Vec<Take> = (0..WINDOW).map(|_| d.take(960 + 9600, 960, false)).collect();
        let last = takes.pop().unwrap();
        assert!(takes.iter().all(|t| *t == block));
        assert_eq!(last, Take { discard: 9600 - SPARE_TARGET as usize, ..block });
        let eased: Vec<usize> =
            (0..WINDOW * 2 * MEMORY as u32).map(|_| d.take(960 + 1000, 960, false).frames).collect();
        assert!(eased.iter().all(|&f| f == 960 || f == 961), "a frame at a time at most");
        assert!(eased[eased.len() - WINDOW as usize..].contains(&961), "a little extra is eased away");
        let dry: Vec<Take> = (0..WINDOW).map(|_| d.take(900, 960, false)).collect();
        let margin = SPARE_TARGET as usize;
        assert_eq!(dry[WINDOW as usize - 1], Take { discard: 0, frames: 960 - margin, silence: margin });
    }

    /// The speed moves a few dozen ppm at a time, never a frame per block for
    /// a whole second: a microphone 300 ppm fast, delivering 10 ms at a time,
    /// is followed within a minute and then kept to the margin.
    #[test]
    fn a_fast_device_is_followed_without_swinging() {
        let per_block = AUDIO_RATE as f64 * 1.0003 / 50.0;
        let mut d = Drift::default();
        let (mut queued, mut sent, mut delivered) = (0usize, 0f64, 0usize);
        let mut extra = Vec::new();
        for n in 0..50 * 600 {
            sent += per_block;
            while delivered + 480 <= sent as usize {
                (delivered, queued) = (delivered + 480, queued + 480);
            }
            let take = d.take(queued, BLOCK, false);
            queued -= take.discard.min(queued);
            queued -= take.frames.min(queued);
            if n % WINDOW as usize == 0 {
                extra.push(0);
            }
            *extra.last_mut().unwrap() += take.frames as i64 - BLOCK as i64 + take.silence as i64;
            if n >= 50 * 590 {
                let spare_ms = queued as i64 * 1000 / AUDIO_RATE as i64;
                assert!((0..30).contains(&spare_ms), "{spare_ms} ms waiting");
            }
        }
        // 300 ppm is 14.4 frames a second.
        let late: Vec<i64> = extra[60..].to_vec();
        assert!(late.iter().all(|&e| (0..=40).contains(&e)), "{late:?}");
        let mean = late.iter().sum::<i64>() as f64 / late.len() as f64;
        assert!((mean - 14.4).abs() < 1.0, "{mean}");
    }

    /// A sender streaming a recording runs ahead until its ring is full and
    /// then waits: what it holds is sound to come, which must all be heard.
    #[test]
    fn a_sender_waiting_for_room_is_left_alone() {
        let mut d = Drift::default();
        let block = Take { discard: 0, frames: 960, silence: 0 };
        for n in 0..3 * WINDOW {
            assert_eq!(d.take(48_000, 960, n % 7 == 0), block);
        }
        d.resync();
        assert_eq!(d.take(48_000, 960, true), block);
    }
}

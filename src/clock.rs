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

/// The picture boundary a file begun at `origin` ends on when it is told to
/// end at `t`: the first one at or after it, so that its last picture is whole.
pub fn end_slot(origin: u64, t: u64, fps: u32) -> u64 {
    let since = t.saturating_sub(origin) as u128;
    (since * fps.max(1) as u128).div_ceil(NS as u128) as u64
}

/// Sound frames up to a picture boundary. One that falls between two sound
/// frames, as at 144 pictures a second, ends on the later one.
pub fn samples_to(slot: u64, fps: u32) -> u64 {
    (slot as u128 * AUDIO_RATE as u128).div_ceil(fps.max(1) as u128) as u64
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
        let slot = self.slot(at);
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

    fn slot(&self, at: u64) -> i64 {
        position(at, self.origin + self.cut_ns(), self.fps)
    }

    /// The picture boundary the line ends on when told to end at `t`.
    pub fn end_at(&self, t: u64) -> u64 {
        end_slot(self.origin + self.cut_ns(), t, self.fps as u32)
    }

    /// For a picture stamped `at` once the line ends at picture `end`: how
    /// many times to repeat the picture before it so that the line reaches
    /// the end, or `None` while this picture still belongs before the end.
    pub fn close(&mut self, at: u64, end: u64) -> Option<u64> {
        if self.slot(at) < end as i64 {
            return None;
        }
        let gap = end.saturating_sub(self.written);
        self.written += gap;
        Some(gap)
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
    /// Frames of the block written after those: all the rest, unless the
    /// stream ends within it.
    pub take: usize,
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
    #[cfg(test)]
    pub fn place(&mut self, at: u64, frames: usize) -> Fit {
        self.place_before(at, frames, u64::MAX)
    }

    /// As `place`, for a stream that ends after `end` frames: nothing goes
    /// beyond them.
    pub fn place_before(&mut self, at: u64, frames: usize, end: u64) -> Fit {
        let pos = position(at, self.origin, AUDIO_RATE as u64);
        let (silence, skip) = if pos >= self.written as i64 {
            (pos as u64 - self.written, 0)
        } else {
            (0, ((self.written as i64 - pos) as u64).min(frames as u64) as usize)
        };
        let room = end.saturating_sub(self.written);
        let silence = silence.min(room);
        let take = ((frames - skip) as u64).min(room - silence) as usize;
        self.written += silence + take as u64;
        Fit { silence, skip, take }
    }

    pub fn written(&self) -> u64 {
        self.written
    }
}

/// Where an encoder's picture and sound both end, as (pictures, samples):
/// the later of the two, on a picture boundary, so neither stops short.
pub fn even_end(frames: u64, samples: u64, fps: u32) -> (u64, u64) {
    // The first boundary whose sound, as `samples_to` counts it, holds all
    // of it: sound already ending on a boundary is not taken past it.
    let covered = match samples {
        0 => 0,
        s => ((s as u128 - 1) * fps.max(1) as u128 / AUDIO_RATE as u128) as u64 + 1,
    };
    let frames = frames.max(covered);
    (frames, samples_to(frames, fps))
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
    /// The next block is made behind time, the sound loop catching up.
    hurried: bool,
    /// Only for sound sent beside pictures shown as they arrive.
    beside: Option<Beside>,
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
            hurried: false,
            beside: None,
        }
    }
}

/// Sound waiting beyond this, beside pictures shown as they arrive, is cut
/// back at once rather than at the end of the second: a burst after a network
/// stall would otherwise be heard up to a second behind its pictures.
const MOST_WAITING: i64 = AUDIO_RATE as i64 / 4; // 250 ms
/// Blocks after running short before what waits says whether the margin came
/// back: a delivery or two.
const SETTLING: u32 = 10;

/// What `Drift` also watches of sound sent beside pictures that are shown as
/// they arrive, as an NDI or OMT sender's or a video call guest's are. When
/// the network or the page holds such a source up, its pictures stop, then
/// jump to the latest; meanwhile its sound ran short and silence was played.
/// The sound that then comes late is skipped as its pictures were, so the two
/// stay together: heard, it would trail them for a minute or more, and the
/// margin rebuilt on top of it as silence would cost a second gap. A source
/// held up again within `MEMORY` seconds needs that margin, and keeps it.
struct Beside {
    /// The most that waited beyond a block, in this window and in the last one
    /// that ran normally: where the ring usually is just after a delivery.
    high: i64,
    usual_high: i64,
    /// Frames of silence played for sound that had not come, to skip if it
    /// comes late.
    behind: i64,
    /// The hold-up under way is the first for a while: its late sound is skipped.
    skipping: bool,
    /// Blocks since the ring last ran short, the sound loop being on time.
    since_short: u32,
    /// This window ran short, and the least waiting since it last did.
    short: bool,
    low_after: i64,
    /// Whether the margin came back was left for this window to say.
    deferred: bool,
}

impl Default for Beside {
    fn default() -> Self {
        Self {
            high: 0,
            usual_high: 0,
            behind: 0,
            skipping: false,
            since_short: u32::MAX,
            short: false,
            low_after: i64::MAX,
            deferred: false,
        }
    }
}

impl Beside {
    /// Where the ring usually is just after a delivery: cutting back further
    /// at an unlucky moment would make it run short before the next one.
    fn top(&self) -> i64 {
        self.usual_high.max(SPARE_TARGET)
    }

    /// What to skip now of what waits beyond the block, `spare`: of late sound,
    /// as much as the silence played for it, down to the margin, which puts the
    /// sound back where it was before the hold-up; and beyond `MOST_WAITING`,
    /// all down to where the ring usually is. Nothing while the sound loop
    /// catches up: what waits is what it is behind on.
    fn skip(&mut self, spare: i64, hurried: bool) -> i64 {
        let skip = if hurried {
            0
        } else if spare > MOST_WAITING {
            self.behind = 0;
            spare - self.top()
        } else {
            self.behind.min(spare - SPARE_TARGET)
        };
        if skip > 0 {
            self.behind = (self.behind - skip).max(0);
        } else if !hurried {
            // A burst after a hold-up says nothing of the usual rhythm.
            self.high = self.high.max(spare);
        }
        skip.max(0)
    }

    /// At the end of a window whose least waiting was `low`: whether it only
    /// ran short for a moment, its sound back since, which is a hold-up and
    /// says nothing of the source's speed nor of a margin too thin. Running
    /// short just before the end, it is left to the next window to say.
    fn window_done(&mut self, low: i64) -> bool {
        let passing = if !self.short {
            false
        } else if self.since_short >= SETTLING {
            self.low_after >= SPARE_TARGET / 2
        } else {
            // Waited for once; still running short, it lacks a margin.
            !std::mem::replace(&mut self.deferred, true)
        };
        if !self.short && (0..=SPARE_CUT).contains(&low) {
            self.usual_high = self.high;
        }
        self.high = 0;
        if !self.short || self.since_short >= SETTLING || !passing {
            (self.short, self.deferred) = (false, false);
        }
        passing
    }

    /// After a block took `frames` of the `have` waiting, `spare` beyond it.
    fn taken(&mut self, have: i64, frames: usize, spare: i64, hurried: bool) {
        let short = frames as i64 - have;
        if short > 0 {
            (self.short, self.low_after) = (true, i64::MAX);
            // Running short while the sound loop catches up is its doing.
            if !hurried {
                // Blocks short in a row are one hold-up.
                if self.since_short > 2 {
                    self.skipping = self.since_short > MEMORY as u32 * WINDOW;
                }
                self.since_short = 0;
                // Once its usual rhythm is known: at the start nothing came
                // late, the source had not begun.
                if self.skipping && self.usual_high > 0 {
                    self.behind += short;
                }
            }
        } else {
            self.low_after = self.low_after.min(spare);
            self.since_short = self.since_short.saturating_add(1);
            // Sound that has not come within a second was lost, not late.
            if self.since_short > WINDOW {
                self.behind = 0;
            }
        }
    }
}

impl Drift {
    /// For sound sent beside pictures shown as they arrive: NDI, OMT and video
    /// call guests. See `Beside`.
    pub fn beside_pictures() -> Self {
        Self { beside: Some(Beside::default()), ..Self::default() }
    }

    /// After the sound loop skipped ahead, whatever waited is old: the next
    /// block cuts back to the margin straight away.
    pub fn resync(&mut self) {
        self.resync = true;
    }

    /// The next block is made behind time, as the sound loop catches up after
    /// a stall: the sound it finds waiting, or lacks, is not the source's doing.
    pub fn hurry(&mut self) {
        self.hurried = true;
    }

    /// `queued` is what the input's ring holds before taking a block of
    /// `block`. `held` says the source is waiting for room in it, as a sender
    /// streaming a recording faster than it plays does: that is sound still to
    /// come, like a file's, and none of it is cut.
    pub fn take(&mut self, queued: usize, block: usize, held: bool) -> Take {
        let mut spare = queued as i64 - block as i64;
        let mut take = Take { discard: 0, frames: block, silence: 0 };
        let hurried = std::mem::take(&mut self.hurried);
        if std::mem::take(&mut self.resync) && !held {
            // The device's clock did not change: its speed is kept.
            self.restart_window(SPARE_TARGET);
            let keep = self.beside.as_ref().map_or(SPARE_TARGET, Beside::top);
            take.discard = (spare - keep).max(0) as usize;
            if let Some(b) = &mut self.beside {
                b.behind = 0;
            }
            return take;
        }
        if let Some(b) = self.beside.as_mut().filter(|_| !held) {
            let skip = b.skip(spare, hurried);
            take.discard = skip as usize;
            spare -= skip;
        }
        self.low = self.low.min(spare);
        self.held |= held;
        self.seen += 1;
        if self.seen == WINDOW {
            let (low, held) = (self.low, self.held);
            (self.low, self.seen, self.held) = (i64::MAX, 0, false);
            let passing = self.beside.as_mut().is_some_and(|b| b.window_done(low));
            if held {
                self.adjust = 0.0;
                self.lows.clear();
            } else if passing {
                // Its margin is back by itself: a second silence would only
                // be a second gap.
            } else if low > SPARE_CUT {
                take.discard += (low - SPARE_TARGET) as usize;
                self.restart_window(SPARE_TARGET);
            } else if low < 0 {
                // It ran dry, at the start or after a late delivery: that gap
                // is there already, and taking the margin with it spares the
                // seconds a frame per block would take to build it up.
                take.silence = (SPARE_TARGET as usize).min(block);
                take.frames = block - take.silence;
                self.restart_window(SPARE_TARGET);
                if let Some(b) = &mut self.beside {
                    b.behind = 0;
                }
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
        if let Some(b) = &mut self.beside {
            b.taken(queued as i64 - take.discard as i64, take.frames, spare, hurried);
        }
        take
    }

    fn restart_window(&mut self, margin: i64) {
        (self.low, self.seen, self.held) = (i64::MAX, 0, false);
        self.lows = vec![margin];
    }
}

/// The pitch of the tone the live check and the drift test listen for.
#[cfg(any(windows, test))]
pub(crate) const TONE_HZ: f32 = 1000.0;

/// The tone must be there, loud enough, mostly alone and at its pitch: its
/// strength is taken over the whole stretch at once, which only counts it
/// within about 70 ppm of 1 kHz and in step with itself throughout.
#[cfg(test)]
pub(crate) fn judge_tone(samples: &[f32], rate: usize) -> Result<String, String> {
    judge(samples, rate, usize::MAX)
}

/// The tone must be there, loud enough and mostly alone, as the live check
/// hears its streams: its strength is taken 50 ms at a time, 50 periods. A
/// live input kept in step with our clock plays up to a thousandth faster or
/// slower (`MOST_ADJUST`), which nobody hears, and a virtual cable on a busy
/// machine drops 10 ms now and then. Taken over the whole stretch at once,
/// either left the tone a few percent of a sound that was nothing else
/// (#40). Another sound, noise, or a pitch a percent off still fail.
#[cfg(any(windows, test))]
pub(crate) fn judge_heard_tone(samples: &[f32], rate: usize) -> Result<String, String> {
    judge(samples, rate, rate / 20)
}

/// The tone's power, by Goertzel (the strength of one frequency without a
/// full spectrum) over each `piece` of `samples`, against all their power.
#[cfg(any(windows, test))]
fn judge(samples: &[f32], rate: usize, piece: usize) -> Result<String, String> {
    // The first half second may still hold what the encoder started with.
    let samples = samples.get(rate / 2..).unwrap_or_default();
    if samples.len() < rate {
        return Err(format!("{} ms de son reçus", samples.len() * 1000 / rate));
    }
    let piece = piece.min(samples.len());
    let samples = &samples[..samples.len() / piece * piece];
    let power = samples.iter().map(|v| v * v).sum::<f32>() / samples.len() as f32;
    let k = 2.0 * (std::f32::consts::TAU * TONE_HZ / rate as f32).cos();
    let n = piece as f32;
    let tone_power = samples
        .chunks_exact(piece)
        .map(|p| {
            let (mut s1, mut s2) = (0f32, 0f32);
            for &v in p {
                let s0 = v + k * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            2.0 * (s1 * s1 + s2 * s2 - k * s1 * s2) / (n * n)
        })
        .sum::<f32>()
        / (samples.len() / piece) as f32;
    let db = |p: f32| 10.0 * p.max(1e-12).log10();
    if tone_power < 1e-4 {
        return Err(format!("pas de son à {TONE_HZ} Hz : {:.0} dB au total", db(power)));
    }
    if tone_power < power * 0.8 {
        return Err(format!(
            "son brouillé : la tonalité ne fait que {:.0} % du son",
            tone_power / power * 100.0
        ));
    }
    Ok(format!(
        "tonalité {TONE_HZ} Hz à {:.0} dB, {:.0} % du son",
        db(tone_power),
        tone_power / power * 100.0
    ))
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
        /// Lowest and highest offset, sound after picture, in ms, over the
        /// show and after its first half hour, once the speed is learnt.
        span: (f64, f64),
        settled: (f64, f64),
        first_hour_mean: f64,
        last_hour_mean: f64,
        picture_s: f64,
        sound_s: f64,
        /// After the first half hour, each hour: times the ring ran short and
        /// silence was played, the input was cut back or skipped ahead, and
        /// silence was added to rebuild its margin; seconds its sound came more
        /// than 50 ms later than it usually does, and more than 125 ms after
        /// its picture, where people start to notice (ITU-R BT.1359).
        gaps: f64,
        cuts: f64,
        silences: f64,
        late_s: f64,
        noticed_s: f64,
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

    /// How a live input's sound reaches its ring.
    #[derive(Clone, Copy)]
    struct Feed {
        /// How far the sender's clock is off ours.
        ppm: f64,
        /// Frames in each delivery.
        chunk: u64,
        /// Each delivery is up to this many ms late, at random.
        jitter_ms: u64,
        /// Every 30 to 120 s, the network or the page holds a delivery up 30
        /// to 80 ms, and those behind it with it, as Wi-Fi or a busy page does.
        holds: bool,
        /// Frames the input's ring holds; beyond, it drops the oldest.
        ring: u64,
        /// The sender's pictures come with its sound and the mixer shows the
        /// latest, as an NDI or OMT input's do. Otherwise a picture is of the
        /// moment the mixer takes it, as a camera's or a web page's is.
        pictures: bool,
        /// Kept in step as sound beside pictures shown as they arrive.
        beside: bool,
    }

    impl Feed {
        /// A microphone delivering 10 ms at a time.
        fn microphone(ppm: f64) -> Self {
            let ring = AUDIO_RATE as u64;
            Self { ppm, chunk: 480, jitter_ms: 0, holds: false, ring, pictures: false, beside: false }
        }

        /// An NDI or OMT sender at 30 pictures a second, sending the sound of
        /// each picture with it, with a few ms of network jitter.
        fn network_video(ppm: f64) -> Self {
            let ring = AUDIO_RATE as u64;
            Self { ppm, chunk: 1600, jitter_ms: 4, holds: true, ring, pictures: true, beside: true }
        }

        /// A video call guest, as the browser receiving the call hands their
        /// sound over: 2048 frames at a time, up to 15 ms late while the page
        /// or the connection is busy, into a ring of 0.3 s. The page shows
        /// their picture with the sound it plays, which it then hands over.
        fn guest(ppm: f64) -> Self {
            let ring = AUDIO_RATE as u64 * 3 / 10;
            Self { ppm, chunk: 2048, jitter_ms: 15, holds: true, ring, pictures: true, beside: true }
        }
    }

    /// A live source on its own clock, and what waits in its input's ring:
    /// `queued` frames from frame `head` on.
    struct Source {
        feed: Feed,
        rate: f64,
        dice: Dice,
        delivered: u64,
        head: u64,
        queued: u64,
        /// When the next piece lands, and when the next hold-up comes.
        due: u64,
        hold_at: u64,
        /// When the latest picture delivered was taken.
        picture: Option<i64>,
    }

    impl Source {
        fn new(feed: Feed) -> Self {
            let rate = AUDIO_RATE as f64 * (1.0 + feed.ppm * 1e-6);
            let (dice, hold_at) = (Dice(7), 60 * NS);
            let mut source =
                Self { feed, rate, dice, delivered: 0, head: 0, queued: 0, due: 0, hold_at, picture: None };
            source.due = source.captured(feed.chunk) as u64;
            source
        }

        /// When the source took `frame`, on our clock.
        fn captured(&self, frame: u64) -> i64 {
            (frame as f64 / self.rate * NS as f64) as i64
        }

        /// Everything that has landed by `t`, in order.
        fn deliver_until(&mut self, t: u64) {
            let feed = self.feed;
            while self.due <= t {
                self.delivered += feed.chunk;
                self.queued += feed.chunk;
                let over = self.queued.saturating_sub(feed.ring);
                (self.head, self.queued) = (self.head + over, self.queued - over);
                self.picture = Some(self.captured(self.delivered));
                let next = self.captured(self.delivered + feed.chunk) as u64;
                let mut late = if feed.jitter_ms > 0 { self.dice.below(feed.jitter_ms * MS) } else { 0 };
                if feed.holds && next >= self.hold_at {
                    late += (30 + self.dice.below(51)) * MS;
                    self.hold_at = next + (30 + self.dice.below(91)) * NS;
                }
                self.due = self.due.max(next + late);
            }
        }

        /// Takes a block from the ring as `take` says, and gives the first
        /// frame heard when the block starts with sound.
        fn take(&mut self, take: &Take) -> Option<u64> {
            let gone = (take.discard as u64).min(self.queued);
            (self.head, self.queued) = (self.head + gone, self.queued - gone);
            let have = (take.frames as u64).min(self.queued);
            let first = (have > 0 && take.silence == 0).then_some(self.head);
            (self.head, self.queued) = (self.head + have, self.queued - have);
            first
        }
    }

    /// Hours of a show through the real pacing, drift and placing code: a
    /// live source off our clock, pictures that take too long one tick in
    /// fifty, a sound loop that stalls now and then, and an encoder whose
    /// pipes stop reading for a while.
    fn simulate(hours: u64, feed: Feed) -> Report {
        let end = hours * 3600 * NS;
        let origin = 5 * NS + 370_000;
        let settled = origin + 1800 * NS;
        let mut dice = Dice(42);
        let mut source = Source::new(feed);

        let (mut sound_pace, mut picture_pace) = (Pace::new(0, 50), Pace::new(0, FPS));
        let (mut sound_at, mut picture_at) = (0u64, 0u64);
        let mut drift = if feed.beside { Drift::beside_pictures() } else { Drift::default() };
        let (mut resynced, mut hurried) = (false, false);
        let (mut short_stall, mut long_stall) = (1201 * NS, 4001 * NS);

        let mut pictures = Pipe { queue: VecDeque::new(), cap: 6 };
        let mut sound = Pipe { queue: VecDeque::new(), cap: 100 };
        let mut frames = FrameLine::new(origin, FPS);
        let mut samples = SampleLine::new(origin);
        let mut picture_lag: Option<i64> = None;

        let (mut span, mut settled_span) = ((f64::MAX, f64::MIN), (f64::MAX, f64::MIN));
        let mut hours_seen: Vec<(f64, u64)> = vec![(0.0, 0); hours as usize];
        let (mut gaps, mut cuts, mut silences) = (0u64, 0u64, 0u64);
        let mut offsets: Vec<f32> = Vec::new();
        let (mut was_short, mut was_cut) = (false, false);

        while sound_at.min(picture_at) < end {
            if sound_at <= picture_at {
                let t = sound_at;
                source.deliver_until(t);
                if hurried {
                    drift.hurry();
                }
                let take = drift.take(source.queued as usize, BLOCK, false);
                let short = take.frames as u64 > source.queued.saturating_sub(take.discard as u64);
                // A cut right after the sound loop skipped ahead is not the
                // source's doing.
                let cut = take.discard > 0 && !resynced;
                if t >= settled {
                    gaps += (short && !was_short) as u64;
                    cuts += (cut && !was_cut) as u64;
                    silences += (take.silence > 0) as u64;
                }
                (was_short, was_cut, resynced) = (short, cut, false);
                let first = source.take(&take);

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
                        let heard = (origin + pos * NS / AUDIO_RATE as u64) as i64 - source.captured(frame);
                        let offset = (heard - picture) as f64 / MS as f64;
                        span = (span.0.min(offset), span.1.max(offset));
                        if at >= settled {
                            settled_span = (settled_span.0.min(offset), settled_span.1.max(offset));
                        }
                        let hour = &mut hours_seen[(at / (3600 * NS)) as usize];
                        *hour = (hour.0 + offset, hour.1 + 1);
                        if at >= settled {
                            offsets.push(offset as f32);
                        }
                    }
                }
                let step = sound_pace.advance(done, 500 * MS);
                if step.skipped > 0 {
                    drift.resync();
                    resynced = true;
                }
                hurried = step.wait.is_zero();
                sound_at = done + step.wait.as_nanos() as u64 + dice.below(1_500_000);
            } else {
                let t = picture_at;
                source.deliver_until(t);
                let shown = if feed.pictures { source.picture } else { Some(t as i64) };
                let stamp = picture_pace.stamp();
                let busy = if dice.below(50) == 0 { 35 + dice.below(85) } else { 3 + dice.below(10) };
                let done = t + busy * MS;
                if done >= origin {
                    for (at, shown) in pictures.push(done, (stamp, shown)) {
                        if frames.place(at).is_some()
                            && let Some(shown) = shown
                        {
                            let slot = frames.written() - 1;
                            picture_lag = Some((origin + slot * NS / FPS as u64) as i64 - shown);
                        }
                    }
                }
                let step = picture_pace.advance(done, 0);
                picture_at = done + step.wait.as_nanos() as u64 + dice.below(1_000_000);
            }
        }
        let mean = |h: &(f64, u64)| h.0 / h.1.max(1) as f64;
        let per_hour = |n: u64| n as f64 / (hours as f64 - 0.5);
        let blocks_s = |n: usize| per_hour(n as u64) * BLOCK as f64 / AUDIO_RATE as f64;
        let mut sorted = offsets.clone();
        sorted.sort_by(f32::total_cmp);
        let usual = sorted.get(sorted.len() / 2).copied().unwrap_or(0.0);
        Report {
            span,
            settled: settled_span,
            first_hour_mean: mean(&hours_seen[0]),
            last_hour_mean: mean(&hours_seen[hours as usize - 1]),
            picture_s: frames.written() as f64 / FPS as f64,
            sound_s: samples.written() as f64 / AUDIO_RATE as f64,
            gaps: per_hour(gaps),
            cuts: per_hour(cuts),
            silences: per_hour(silences),
            late_s: blocks_s(offsets.iter().filter(|&&o| o > usual + 50.0).count()),
            noticed_s: blocks_s(offsets.iter().filter(|&&o| o > 125.0).count()),
        }
    }

    /// Sound comes after its picture by up to `behind` ms, and before it by
    /// less than `ahead`; what matters is that it stays there, however long
    /// the show.
    fn check_within(what: &str, r: &Report, ahead: f64, behind: f64) {
        let frame_ms = 1000.0 / FPS as f64;
        assert!(r.span.0 > -ahead && r.span.1 < behind, "{what}: {r:?}");
        assert!((r.last_hour_mean - r.first_hour_mean).abs() < 2.0, "{what}: {r:?}");
        assert!((r.picture_s - r.sound_s).abs() * 1000.0 < frame_ms, "{what}: {r:?}");
    }

    /// Sound comes a block and the device's delivery after the picture.
    fn check(r: &Report) {
        let frame_ms = 1000.0 / FPS as f64;
        check_within("microphone", r, frame_ms, 2.0 * frame_ms + 20.0);
    }

    const FEEDS: [(&str, fn(f64) -> Feed); 2] = [("NDI/OMT", Feed::network_video), ("guest", Feed::guest)];

    #[test]
    fn hours_with_a_fast_microphone_and_late_pictures_stay_in_step() {
        check(&simulate(6, Feed::microphone(300.0)));
    }

    #[test]
    fn hours_with_a_slow_microphone_and_late_pictures_stay_in_step() {
        check(&simulate(6, Feed::microphone(-300.0)));
    }

    /// An NDI or OMT sender, or the browser a guest is heard through, whose
    /// clock is 0.1 % off ours, further than real devices drift, held up 30 to
    /// 80 ms every minute or so: its speed is followed for hours and its sound
    /// stays in lip sync. Never more than 150 ms after its picture, and less
    /// than a second an hour beyond the 125 ms where people notice (ITU-R
    /// BT.1359), or 50 ms later than usual: a hold-up's late sound is skipped
    /// as its pictures are. Right after one, sound is back to now a picture
    /// before the pictures are, so it may come up to two pictures early.
    #[test]
    fn hours_of_network_video_or_a_guest_a_thousandth_off_stay_in_lip_sync() {
        let frame_ms = 1000.0 / FPS as f64;
        for (name, feed) in FEEDS {
            for ppm in [1000.0, -1000.0] {
                let r = simulate(6, feed(ppm));
                let what = format!("{name} {ppm} ppm");
                check_within(&what, &r, 2.0 * frame_ms, 150.0);
                assert!(r.noticed_s < 1.0 && r.late_s < 10.0, "{what}: {r:?}");
                assert!(r.silences < 6.0, "{what}: {r:?}");
            }
        }
    }

    /// The same hold-ups on our clock: each costs the silence it played and
    /// nothing after, where kept as a microphone is, its late sound trailed
    /// the pictures and the margin was rebuilt on top as a second silence.
    #[test]
    fn a_hold_up_beside_pictures_costs_one_gap_not_a_minute_behind() {
        for (name, feed) in FEEDS {
            let now = simulate(6, feed(0.0));
            let before = simulate(6, Feed { beside: false, ..feed(0.0) });
            let what = format!("{name}: {now:?} against {before:?}");
            assert!(2.0 * (now.gaps + now.silences) < before.gaps + before.silences, "{what}");
            assert!(now.late_s * 10.0 < before.late_s && now.noticed_s < 1.0, "{what}");
            assert!(now.settled.1 < before.settled.1, "{what}");
        }
    }

    /// 1 % off is more than a frame per block can follow: a fast sender is
    /// cut back to the margin now and then, and a slow one given it back as
    /// a moment of silence. That is not lip sync, which such a sender cannot
    /// keep: hours later its sound must only still be where it was, and never
    /// pile up past a quarter of a second.
    #[test]
    fn hours_of_network_video_or_a_guest_a_hundredth_off_stay_near_the_margin() {
        for (name, feed) in FEEDS {
            for ppm in [10_000.0, -10_000.0] {
                let r = simulate(6, feed(ppm));
                // A cut while the sound loop catches up after a stall puts
                // sound of now in a block stamped earlier: for a moment, it
                // comes before its picture.
                check_within(&format!("{name} {ppm} ppm"), &r, 100.0, 250.0);
                assert!(r.cuts + r.silences > 0.0, "{name} {ppm}: {r:?}");
            }
        }
    }

    /// After a network stall the ring fills at once, up to all it holds. Sound
    /// beside pictures is cut back to the margin as it lands, rather than
    /// heard up to a second behind them until the end of the second.
    #[test]
    fn a_burst_after_a_stall_is_cut_back_at_once() {
        let most = |mut d: Drift| {
            let (mut queued, mut sent, mut most) = (0usize, 0usize, 0usize);
            for n in 0..50 * 6 {
                // Thirty 1600-frame pieces a second, held up for 1.5 s from
                // 3 s on, then all at once, as much as the ring holds.
                if !(150..225).contains(&n) {
                    let due = (n + 1) * 30 / 50;
                    queued = (queued + (due - sent) * 1600).min(AUDIO_RATE);
                    sent = due;
                }
                let take = d.take(queued, BLOCK, false);
                queued -= (take.discard + take.frames).min(queued);
                if n >= 225 {
                    most = most.max(queued);
                }
            }
            most * 1000 / AUDIO_RATE
        };
        let (beside, kept) = (most(Drift::beside_pictures()), most(Drift::default()));
        assert!(beside < 80, "{beside} ms waiting");
        assert!(kept > 250, "as a microphone, {kept} ms waiting");
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
        assert_eq!(sound.place(0, 960), Fit { silence: 0, skip: 0, take: 960 });
        assert_eq!(pictures.place_within(10 * NS, 12), Some(12));
        assert_eq!(pictures.cut(), 237);
        assert_eq!(pictures.place_within(10 * NS + 40 * MS, 12), Some(0));
        let cut = pictures.cut_ns();
        assert_eq!(cut, 237 * 40 * MS);
        // The picture and the sound of 10.04 s start at the same place in the file.
        let fit = sound.place(10 * NS + 40 * MS - cut, 960);
        assert_eq!(fit, Fit { silence: 14 * 1920 - 960, skip: 0, take: 960 });
        assert_eq!((pictures.written() - 1) * 1920, sound.written() - 960);
    }

    #[test]
    fn lost_sound_becomes_silence_and_overlap_is_trimmed() {
        let mut line = SampleLine::new(0);
        assert_eq!(line.place(0, 960), Fit { silence: 0, skip: 0, take: 960 });
        assert_eq!(line.place(60 * MS, 960), Fit { silence: 1920, skip: 0, take: 960 });
        assert_eq!(line.place(70 * MS, 960), Fit { silence: 0, skip: 480, take: 480 });
        assert_eq!(line.place(0, 960), Fit { silence: 0, skip: 960, take: 0 });
        assert_eq!(line.written(), 4320);
    }

    #[test]
    fn sound_from_before_the_start_is_cut_at_the_start() {
        let mut line = SampleLine::new(10 * MS);
        assert_eq!(line.place(0, 960), Fit { silence: 0, skip: 480, take: 480 });
        assert_eq!(line.written(), 480);
    }

    /// Told to end 1.02 s after a start 7 ms off the picture loop's ticks, a
    /// file at 25 pictures a second ends on its 26th picture boundary, at
    /// 1.04 s: the picture under way when it was stopped is whole, and its
    /// sound ends there too, whichever of the two reached the encoder first.
    #[test]
    fn a_stopped_file_ends_on_the_next_picture_boundary_with_both() {
        let origin = 7 * MS;
        let mut pictures = FrameLine::new(origin, 25);
        let mut sound = SampleLine::new(origin);
        let end = pictures.end_at(origin + 1020 * MS);
        assert_eq!(end, 26);
        let samples = samples_to(end, 25);
        assert_eq!(samples, 49_920);
        for n in 0..24 {
            assert_eq!(pictures.close(origin + n * 40 * MS, end), None);
            assert_eq!(pictures.place(origin + n * 40 * MS), Some(0));
        }
        // The 25th picture was late and given up, the 27th is past the end:
        // the 24th is repeated for the 25th, and the line stops at the 26th.
        assert_eq!(pictures.close(origin + 25 * 40 * MS, end), None);
        assert_eq!(pictures.place(origin + 25 * 40 * MS), Some(1));
        assert_eq!(pictures.close(origin + 26 * 40 * MS, end), Some(0));
        assert_eq!(pictures.written(), end);
        let fits: Vec<Fit> =
            (0..56).map(|n| sound.place_before(origin + n * 20 * MS, 960, samples)).collect();
        assert_eq!(fits[51], Fit { silence: 0, skip: 0, take: 960 });
        assert_eq!(fits[52], Fit { silence: 0, skip: 0, take: 0 });
        assert_eq!(sound.written(), samples);
        // Sound lost just before the end is silence up to the end, no further.
        let mut gap = SampleLine::new(origin);
        gap.place_before(origin, 960, 2000);
        assert_eq!(gap.place_before(origin + 40 * MS, 960, 2000), Fit { silence: 960, skip: 0, take: 80 });
        assert_eq!(gap.written(), 2000);
    }

    /// A picture placed after the last one before the end fills what is
    /// missing up to the end, then nothing more is written.
    #[test]
    fn pictures_missing_before_the_end_repeat_the_last_one() {
        let mut pictures = FrameLine::new(0, 30);
        for n in 0..10 {
            pictures.place(n * NS / 30);
        }
        assert_eq!(pictures.close(14 * NS / 30, 12), Some(2));
        assert_eq!(pictures.written(), 12);
        assert_eq!(pictures.close(15 * NS / 30, 12), Some(0));
        assert_eq!(pictures.written(), 12);
    }

    #[test]
    fn both_streams_end_together() {
        assert_eq!(even_end(300, 480_000, 30), (300, 480_000));
        assert_eq!(even_end(300, 480_100, 30), (301, 481_600));
        assert_eq!(even_end(302, 480_000, 30), (302, 483_200));
    }

    /// At 144 pictures a second a boundary falls between two sound frames: a
    /// stopped file whose sound ends on one, as `samples_to` puts it, is not
    /// padded a picture more, at any rate.
    #[test]
    fn sound_ending_between_two_of_its_frames_is_not_padded() {
        assert_eq!(samples_to(10, 144), 3334);
        for fps in [144, 30, 29, 60, 7] {
            for end in [0, 1, 10, 143, 100_000] {
                let samples = samples_to(end, fps);
                assert_eq!(even_end(end, samples, fps), (end, samples), "{end} pictures at {fps}/s");
            }
        }
        assert_eq!(even_end(10, 3335, 144), (11, 3667));
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

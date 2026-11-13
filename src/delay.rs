//! Video Delay: another input shown again a set time later, picture and sound.
//!
//! Pictures are kept as they were shown, in memory, not encoded to a file as
//! the replay does. A delay is often a fraction of a second, to line up a
//! late source, which is less than an encoder and a decoder take to hand a
//! picture back, and it must land on the right picture. Keeping a picture
//! only keeps it alive: a still source holds one picture for the whole delay,
//! and the buffers let go return to the frame pool. A moving source holds one
//! picture a tick, and one `BUDGET` for all the delays bounds that. Delays of
//! one source keep the same pictures, which count once; each delay's longest
//! time is what the others leave, at the project's size and frame rate, and
//! the settings show it, with what all of them hold.
//!
//! The sound is the source's own, taken in the sound loop after its own
//! treatment and before its fader, and held back by the same time to the
//! sample. Both run on the shared clock, so the delayed picture and sound
//! stay together whatever the loops skip.

use crate::frame::Frame;
use crate::sources::AUDIO_RATE;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, mpsc};
use std::thread;

/// What all the delays together may hold in pictures, in bytes.
pub const BUDGET: usize = 3 << 30;
/// The longest delay offered, whatever memory allows.
pub const MOST_MS: u32 = 10_000;

fn picture_bytes(w: usize, h: usize) -> usize {
    (w * h * 4).max(1)
}

/// Pictures a delay of `ms` holds at most at this rate: the one shown, and
/// one for every tick after it.
fn pictures_for(ms: u32, fps: u32) -> usize {
    (ms.min(MOST_MS) as usize * fps.max(1) as usize).div_ceil(1000) + 1
}

/// The longest delay that `pictures` cover at this rate.
fn ms_for(pictures: usize, fps: u32) -> u32 {
    (pictures.saturating_sub(1) as u64 * 1000 / fps.max(1) as u64).min(MOST_MS as u64) as u32
}

/// A delay as the loops run it: its time held to the memory it is given,
/// and the pictures that memory keeps.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Run {
    pub id: u64,
    pub source: Option<u64>,
    pub ms: u32,
    pub pictures: usize,
}

/// The longest time asked of each source, sources in the order of their
/// first delay, leaving out delay `skip`. Delays of one source keep the same
/// pictures, so a source counts once, at its longest.
fn longest_per_source(delays: &[(u64, Option<u64>, u32)], skip: Option<u64>) -> Vec<(u64, u32)> {
    let mut longest: Vec<(u64, u32)> = Vec::new();
    for &(_, source, ms) in delays.iter().filter(|d| Some(d.0) != skip) {
        let Some(source) = source else { continue };
        match longest.iter_mut().find(|(s, _)| *s == source) {
            Some((_, most)) => *most = (*most).max(ms),
            None => longest.push((source, ms)),
        }
    }
    longest
}

/// Shares `BUDGET` among `delays`, (input, source, time) in the project's
/// order: each source in turn takes what its longest delay needs, as far as
/// what is left goes, and each of its delays is held to that. The settings
/// keep the times within it; a larger project format or a file can ask for
/// more, and the last sources then get less. Returns how each delay runs,
/// and the bytes all of them may hold together.
pub fn share(delays: &[(u64, Option<u64>, u32)], w: usize, h: usize, fps: u32) -> (Vec<Run>, usize) {
    let mut left = BUDGET / picture_bytes(w, h);
    let mut given: HashMap<u64, usize> = HashMap::new();
    for (source, ms) in longest_per_source(delays, None) {
        // The picture on screen is there anyway.
        let n = pictures_for(ms, fps).min(left).max(1);
        left = left.saturating_sub(n);
        given.insert(source, n);
    }
    let used = given.values().sum::<usize>() * picture_bytes(w, h);
    let runs = delays
        .iter()
        .map(|&(id, source, ms)| match source.and_then(|s| given.get(&s)) {
            Some(&pictures) => Run { id, source, ms: ms.min(ms_for(pictures, fps)), pictures },
            // Nothing new to keep, but what it kept of a source just closed
            // still plays out: it takes no more memory than it had.
            None => Run { id, source, ms: ms.min(MOST_MS), pictures: pictures_for(ms, fps) },
        })
        .collect();
    (runs, used)
}

/// The longest delay `id` may be set to: what the delays of the other
/// sources leave, at this size and rate.
pub fn room_ms(delays: &[(u64, Option<u64>, u32)], id: u64, w: usize, h: usize, fps: u32) -> u32 {
    let mine = delays.iter().find(|d| d.0 == id).and_then(|d| d.1);
    let others: usize = longest_per_source(delays, Some(id))
        .into_iter()
        .filter(|(s, _)| Some(*s) != mine)
        .map(|(_, ms)| pictures_for(ms, fps))
        .sum();
    ms_for((BUDGET / picture_bytes(w, h)).saturating_sub(others), fps)
}

/// A delay in nanoseconds of the shared clock.
pub fn nanos(ms: u32) -> u64 {
    ms as u64 * 1_000_000
}

/// A delay in frames of sound.
pub fn sound_frames(ms: u32) -> usize {
    ms.min(MOST_MS) as usize * AUDIO_RATE / 1000
}

/// Frees pictures the delays let go on a thread of its own, which lives as
/// long as the program: a delay made shorter lets hundreds go at once, which
/// takes longer to free than a tick lasts, and the mixer must not wait.
pub fn free_later(pictures: Vec<Arc<Frame>>) {
    static FREER: LazyLock<Option<mpsc::Sender<Vec<Arc<Frame>>>>> = LazyLock::new(|| {
        let (tx, rx) = mpsc::channel::<Vec<Arc<Frame>>>();
        let freer = thread::Builder::new().name("retard-liberation".into());
        freer.spawn(move || rx.into_iter().for_each(drop)).ok()?;
        Some(tx)
    });
    if !pictures.is_empty()
        && let Some(tx) = FREER.as_ref()
    {
        let _ = tx.send(pictures);
    }
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

/// How long the sound before and after a change of delay overlap, in
/// frames: cut straight from one to the other, the seam clicks.
const SEAM: usize = AUDIO_RATE / 125;

/// What one delay keeps of its source's sound: interleaved stereo, oldest
/// first, behind `lead` samples of silence still to play.
#[derive(Default)]
pub struct Sound {
    line: VecDeque<f32>,
    lead: usize,
    /// The delay of the last block, in frames.
    delay: Option<usize>,
    /// Frames of sound still to fade in after that silence, which may be
    /// more than what is left of the block it came back in.
    fade_in: usize,
}

impl Sound {
    /// One block of the sound loop: takes what the source sent for it, `None`
    /// for silence, after `skipped` blocks the loop gave up, and fills `out`
    /// with what came `delay` frames earlier.
    ///
    /// A delay that grows plays silence until it is filled, as the picture
    /// holds meanwhile; one that shrinks drops what it no longer needs. Either
    /// way what the old delay would have played next fades into the new one
    /// over a few milliseconds, and sound coming back after that silence
    /// fades in.
    pub fn step(&mut self, fresh: Option<&[f32]>, skipped: usize, delay: usize, out: &mut [f32]) {
        let block = out.len();
        let delay = delay.min(sound_frames(MOST_MS));
        // The blocks given up went by for the delay too: silence comes in for
        // them, and as much goes out unheard. Silence beyond the delay itself
        // changes nothing more, so after a computer slept for hours it is not
        // counted out sample by sample.
        let gap = skipped.saturating_mul(block).min(delay * 2);
        self.line.extend(std::iter::repeat_n(0.0, gap));
        match fresh {
            Some(s) if s.len() == block => self.line.extend(s.iter().copied()),
            _ => self.line.extend(std::iter::repeat_n(0.0, block)),
        }
        let seam = (SEAM * 2).min(block);
        let mut before = [0f32; SEAM * 2];
        let moved = self.delay.is_some_and(|d| d != delay);
        if moved {
            // Without the change, the next block would begin after the gap.
            before[..seam].iter_mut().enumerate().for_each(|(n, v)| *v = self.peek(gap + n));
        }
        self.delay = Some(delay);
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
        if moved {
            let frames = seam / 2;
            for (n, pair) in out[..seam].as_chunks_mut::<2>().0.iter_mut().enumerate() {
                let t = (n + 1) as f32 / frames as f32;
                pair[0] = before[n * 2] * (1.0 - t) + pair[0] * t;
                pair[1] = before[n * 2 + 1] * (1.0 - t) + pair[1] * t;
            }
        }
        // A delay made much shorter gives back the room it no longer needs.
        let need = delay * 2 + block;
        if self.line.capacity() > 4 * need.max(AUDIO_RATE * 2) {
            self.line.shrink_to(need + block);
        }
    }

    /// Sample `n` of what is held, its silence first.
    fn peek(&self, n: usize) -> f32 {
        n.checked_sub(self.lead).map_or(0.0, |i| self.line.get(i).copied().unwrap_or(0.0))
    }

    fn take(&mut self, n: usize, out: Option<&mut [f32]>) {
        let quiet = n.min(self.lead);
        self.lead -= quiet;
        let sound = self.line.drain(..n - quiet);
        let Some(out) = out else { return };
        out[..quiet].fill(0.0);
        out[quiet..].iter_mut().zip(sound).for_each(|(o, s)| *o = s);
        if quiet > 0 {
            self.fade_in = SEAM;
        }
        let frames = self.fade_in.min((n - quiet) / 2);
        let done = SEAM - self.fade_in;
        for (k, pair) in out[quiet..quiet + frames * 2].as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let t = (done + k + 1) as f32 / SEAM as f32;
            pair[0] *= t;
            pair[1] *= t;
        }
        self.fade_in -= frames;
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

    /// One budget for all the delays: a source's pictures count once however
    /// many delays show it, and each delay may take what the others leave.
    #[test]
    fn one_budget_is_shared_and_a_source_counts_once() {
        let (w, h, fps) = (1920, 1080, 30);
        let bytes = 1920 * 1080 * 4;
        assert_eq!(room_ms(&[(10, Some(1), 0)], 10, w, h, fps), MOST_MS, "ten seconds of 1080p30 fit");
        assert_eq!(room_ms(&[(10, Some(1), 0)], 10, w, h, 60), 6450);
        assert_eq!(room_ms(&[(10, Some(1), 0)], 10, 3840, 2160, 30), 3200);
        let same = [(10, Some(1), 10_000), (11, Some(1), 4000)];
        let (runs, used) = share(&same, w, h, fps);
        assert_eq!(used, 301 * bytes, "two delays of one camera keep the same pictures");
        assert_eq!((runs[0].ms, runs[1].ms), (10_000, 4000));
        assert_eq!(room_ms(&same, 11, w, h, fps), MOST_MS);
        // 388 pictures in all, 301 taken by the first camera.
        let two = [(10, Some(1), 10_000), (12, Some(2), 10_000)];
        assert_eq!(room_ms(&two, 12, w, h, fps), 2866);
        assert_eq!(room_ms(&two, 10, w, h, fps), 2866, "each is held to what the other leaves");
        // Asked for more than there is, as a file or a larger format can, the
        // later source gets what is left.
        let (runs, used) = share(&two, w, h, fps);
        assert_eq!((runs[0].ms, runs[1].ms, runs[1].pictures), (10_000, 2866, 87));
        assert!(used <= BUDGET);
        let (runs, used) = share(&[(13, None, 5000)], w, h, fps);
        assert_eq!((runs[0].ms, used), (5000, 0), "a delay of nothing keeps nothing new");
        assert_eq!(runs[0].pictures, 151, "but what it kept of a closed source plays out");
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
        // Then the sound fades in, and comes on to the frame.
        assert!(heard[delay..delay + SEAM].iter().enumerate().all(|(k, v)| *v as usize <= k + 1));
        for (n, v) in heard.iter().enumerate().skip(delay + SEAM) {
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
        // The first frames after the delay filled fade in.
        for (at, v) in heard.into_iter().filter(|(at, _)| !(delay..delay + SEAM).contains(at)) {
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
        // more of delay put three blocks of silence before it, once the
        // sound on its way has faded out.
        s.step(Some(&ramp(20)), 0, 960 + 2880, &mut out);
        assert!(frames(&out)[SEAM..].iter().all(|v| *v == 0), "three blocks of silence for the time added");
        assert!(out[0] > 19.0 * 960.0 * 0.99 && out[2 * SEAM - 2] == 0.0, "faded out");
        for b in 21..23 {
            s.step(Some(&ramp(b)), 0, 3840, &mut out);
            assert!(frames(&out).iter().all(|v| *v == 0));
        }
        s.step(Some(&ramp(23)), 0, 3840, &mut out);
        assert!(out[0] < 19.0 * 960.0 / 100.0, "the sound comes back faded in");
        assert_eq!(frames(&out)[SEAM], 19 * 960 + 1 + SEAM as u64, "from where it was");
        s.step(Some(&ramp(24)), 0, 0, &mut out);
        assert_eq!(frames(&out)[SEAM], 24 * 960 + 1 + SEAM as u64, "no delay: the block itself");
        assert_eq!(s.held(), 0);
    }

    /// A tone at half level, one block of the sound loop after another.
    fn tone(block: u64) -> Vec<f32> {
        let step = 440.0 * std::f64::consts::TAU / AUDIO_RATE as f64;
        (0..BLOCK as u64 / 2)
            .flat_map(|n| [((block * 960 + n) as f64 * step).sin() as f32 * 0.5; 2])
            .collect()
    }

    /// Changing the delay on air, longer then shorter, never jumps further
    /// from one sample to the next than the tone itself does, give or take
    /// what a fade over a few milliseconds adds.
    #[test]
    fn a_change_of_delay_fades_rather_than_clicks() {
        let mut s = Sound::default();
        let mut out = vec![0f32; BLOCK];
        let mut heard: Vec<f32> = Vec::new();
        // The second increase ends its silence 260 frames before the end of
        // a block: the fade carries on into the next one.
        for b in 0..150u64 {
            let delay = match b {
                0..30 => 960,
                30..60 => 960 + 2880 + 333,
                60..90 => 300,
                90..120 => 300 + 2880 + 700,
                _ => 300,
            };
            s.step(Some(&tone(b)), 0, delay, &mut out);
            heard.extend(out.iter().step_by(2));
        }
        let slope = 0.5 * 440.0 * std::f32::consts::TAU / AUDIO_RATE as f32;
        let steepest = heard.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
        assert!(steepest < slope + 1.0 / SEAM as f32 + 1e-3, "a click: {steepest} against {slope}");
    }

    /// After hours asleep the loop gives up millions of blocks at once: no
    /// more than the delay of silence is held for them, and the sound carries
    /// on in step afterwards.
    #[test]
    fn hours_given_up_cost_no_more_than_the_delay() {
        let mut s = Sound::default();
        let delay = 4800;
        let mut out = vec![0f32; BLOCK];
        for b in 0..10 {
            s.step(Some(&ramp(b)), 0, delay, &mut out);
        }
        let eight_hours = 50 * 3600 * 8;
        s.step(Some(&ramp(1000)), eight_hours, delay, &mut out);
        assert!(out.iter().all(|v| *v == 0.0));
        assert_eq!(s.held(), delay * 2);
        assert!(s.line.capacity() < 100_000, "{}", s.line.capacity());
        for b in 1001..1006 {
            s.step(Some(&ramp(b)), 0, delay, &mut out);
        }
        assert_eq!(frames(&out)[0], 1000 * 960 + 1, "five blocks later, what came in after the sleep");
    }

    #[test]
    fn a_much_shorter_delay_gives_its_room_back() {
        let mut s = Sound::default();
        let mut out = vec![0f32; BLOCK];
        for b in 0..600 {
            s.step(Some(&ramp(b)), 0, sound_frames(MOST_MS), &mut out);
        }
        let big = s.line.capacity();
        s.step(Some(&ramp(600)), 0, 480, &mut out);
        assert!(s.line.capacity() * 4 < big, "{big} then {}", s.line.capacity());
        assert_eq!(s.held(), 960);
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

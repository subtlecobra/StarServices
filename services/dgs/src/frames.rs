//! A DGS's tick: a loop at a fixed rate that measures itself and moves the players (`movement`). What it shows is how well
//! the pod keeps the rate, which is what `Server FPS` means on a client: 30 FPS is a frame every 333333 ticks.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use sc_proto::internal::DgsFrames;
use tokio::time::{interval, Instant, MissedTickBehavior};

/// 30 frames a second.
pub const FRAME_PERIOD: Duration = Duration::from_nanos(33_333_333);
/// A frame at least this long is a hitch.
const HITCH: Duration = FRAME_PERIOD.saturating_mul(2);
/// Five seconds of frames, so that a hitch shows for a while and then clears.
const WINDOW_FRAMES: u32 = 150;

fn ticks(time: Duration) -> i64 {
    (time.as_nanos() / 100) as i64
}

#[derive(Clone, Copy, Default)]
struct Window {
    frames: u32,
    total: Duration,
    fastest: Duration,
    slowest: Duration,
    hitches: u32,
}

impl Window {
    fn add(&mut self, frame: Duration) {
        self.fastest = if self.frames == 0 { frame } else { self.fastest.min(frame) };
        self.slowest = self.slowest.max(frame);
        self.total += frame;
        self.frames += 1;
        self.hitches += u32::from(frame >= HITCH);
    }
}

#[derive(Default)]
struct Meter {
    current: Window,
    finished: Option<Window>,
    last: Duration,
}

/// Shared between the loop and whoever reports it.
#[derive(Default)]
pub struct FrameMeter(Mutex<Meter>);

impl FrameMeter {
    pub fn record(&self, frame: Duration) {
        let mut meter = self.0.lock().unwrap();
        meter.last = frame;
        meter.current.add(frame);
        if meter.current.frames == WINDOW_FRAMES {
            meter.finished = Some(std::mem::take(&mut meter.current));
        }
    }

    /// The last full window, or what there is of the first. All 0 before the first frame.
    pub fn frames(&self) -> DgsFrames {
        let meter = self.0.lock().unwrap();
        let window = meter.finished.unwrap_or(meter.current);
        let average = window.total.checked_div(window.frames).unwrap_or_default();
        DgsFrames {
            fastest: ticks(window.fastest),
            slowest: ticks(window.slowest),
            average: ticks(average),
            last: ticks(meter.last),
            hitches: window.hitches,
        }
    }
}

/// Runs for as long as the DGS, calling `frame` with the time since the last one. Frames stay on the grid of the period, so
/// one that runs late is a hitch and the rate does not drift.
pub async fn run(meter: Arc<FrameMeter>, mut frame: impl FnMut(Duration)) {
    let mut ticker = interval(FRAME_PERIOD);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker.tick().await;
    let mut started = Instant::now();
    loop {
        ticker.tick().await;
        let now = Instant::now();
        meter.record(now - started);
        frame(now - started);
        started = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MILLISECOND: i64 = 10_000;

    fn milliseconds(count: u64) -> Duration {
        Duration::from_millis(count)
    }

    #[test]
    fn nothing_measured_is_all_zero() {
        assert_eq!(FrameMeter::default().frames(), DgsFrames::default());
    }

    #[test]
    fn the_first_window_reports_what_it_has() {
        let meter = FrameMeter::default();
        for frame in [30, 40, 35] {
            meter.record(milliseconds(frame));
        }
        let frames = meter.frames();
        assert_eq!((frames.fastest, frames.slowest), (30 * MILLISECOND, 40 * MILLISECOND));
        assert_eq!((frames.average, frames.last, frames.hitches), (35 * MILLISECOND, 35 * MILLISECOND, 0));
    }

    #[test]
    fn a_hitch_counts_for_its_window_and_then_clears() {
        let meter = FrameMeter::default();
        meter.record(milliseconds(100));
        for _ in 1..WINDOW_FRAMES {
            meter.record(milliseconds(33));
        }
        assert_eq!(meter.frames().hitches, 1);
        assert_eq!(meter.frames().slowest, 100 * MILLISECOND);
        for _ in 0..WINDOW_FRAMES {
            meter.record(milliseconds(33));
        }
        assert_eq!(meter.frames().hitches, 0);
        assert_eq!(meter.frames().slowest, 33 * MILLISECOND);
    }

    #[test]
    fn the_last_frame_is_the_latest_even_inside_a_new_window() {
        let meter = FrameMeter::default();
        for _ in 0..WINDOW_FRAMES {
            meter.record(milliseconds(33));
        }
        meter.record(milliseconds(50));
        assert_eq!((meter.frames().last, meter.frames().slowest), (50 * MILLISECOND, 33 * MILLISECOND));
    }

    #[tokio::test]
    async fn the_loop_measures_frames_near_its_period() {
        let meter = Arc::new(FrameMeter::default());
        let running = tokio::spawn(run(meter.clone(), |_| {}));
        tokio::time::sleep(FRAME_PERIOD * 8).await;
        running.abort();
        let frames = meter.frames();
        assert!(frames.fastest > 0 && frames.last > 0);
        // A loaded test machine runs late, never early, by more than the timer's millisecond.
        assert!(frames.average >= ticks(FRAME_PERIOD) - 2 * MILLISECOND, "{frames:?}");
    }
}

//! Trackpad scroll input resampled to the display.
//!
//! A trackpad gesture arrives as scroll events ~120 times a second, but not evenly: on a
//! recorded gesture the gaps ranged from 2.8 to 20 ms, and the per-event deltas are whole
//! pixels that do not track those gaps closely. WebKit applies each event in the frame after it
//! arrives, so the content moved by 21, 3, 18, 11, 15, 10 px on consecutive frames while the
//! finger moved steadily. The finger's position plotted against the events' hardware timestamps
//! is close to a straight line, though (within ~3 px).
//!
//! So the events are treated as noisy samples of that position. An alpha-beta filter tracks
//! position and velocity from them, the position is predicted for the time each frame is shown,
//! lightly smoothed, and handed to WebKit as one event per frame. On the recorded gesture this
//! gave 15, 14, 15, 14, 13, 14 px steps, and the content trailed the finger by ~2 px on average,
//! like the raw events did.

/// Weight of a new position sample against the prediction.
const ALPHA: f64 = 0.4;
/// Weight of a new sample's residual in the velocity.
const BETA: f64 = 0.04;
/// Shortest interval the velocity correction is divided by, in s. Events can come 1-3 ms
/// apart; dividing a pixel of noise by that made the velocity jump, and with it the content.
const MIN_INTERVAL: f64 = 0.008;
/// Furthest the position is extrapolated past the last sample, in s.
const MAX_AHEAD: f64 = 0.03;
/// Without samples for this long, in s, the finger is taken to rest where it is.
const STALE: f64 = 0.04;
/// Time constant of the output smoothing, in s.
const SMOOTHING: f64 = 0.008;

#[derive(Debug, Default, Clone, Copy)]
struct Axis {
    /// Sum of the input deltas.
    input: f64,
    /// Filtered position and velocity (px, px/s) as of the last sample.
    position: f64,
    velocity: f64,
    smoothed: f64,
    /// Sum of the deltas handed out.
    sent: f64,
}

impl Axis {
    fn sample(&mut self, delta: f64, interval: f64) {
        self.input += delta;
        let predicted = self.position + self.velocity * interval;
        let residual = self.input - predicted;
        self.position = predicted + ALPHA * residual;
        self.velocity += BETA * residual / interval.max(MIN_INTERVAL);
    }

    fn frame(&mut self, ahead: f64, frame_period: f64) -> f64 {
        let target = if ahead < STALE {
            self.position + self.velocity * ahead.clamp(0.0, MAX_AHEAD)
        } else {
            self.input
        };
        self.smoothed += (target - self.smoothed) * (1.0 - (-frame_period / SMOOTHING).exp());
        if (target - self.smoothed).abs() < 0.5 {
            self.smoothed = target;
        }
        let step = (self.smoothed - self.sent).round();
        self.sent += step;
        step
    }

    fn finish(&mut self) -> f64 {
        let rest = self.input - self.sent;
        *self = Self::default();
        rest
    }
}

/// Resamples one gesture's deltas ([x, y], in px) to display frames. Times are in seconds on
/// the `CACurrentMediaTime` / `NSEvent.timestamp` clock.
#[derive(Debug, Default)]
pub struct WheelResampler {
    axes: [Axis; 2],
    last_sample: f64,
    active: bool,
}

impl WheelResampler {
    /// Starts a gesture at `time`; nothing of it has been handed out yet.
    pub fn begin(&mut self, time: f64) {
        self.axes = Default::default();
        self.last_sample = time;
        self.active = true;
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// An input event of the gesture.
    pub fn sample(&mut self, time: f64, delta: [f64; 2]) {
        let interval = (time - self.last_sample).max(0.0);
        for (axis, delta) in self.axes.iter_mut().zip(delta) {
            axis.sample(delta, interval);
        }
        self.last_sample = time;
    }

    /// Whole-pixel deltas to hand out for the frame shown at `target`.
    pub fn frame(&mut self, target: f64, frame_period: f64) -> [f64; 2] {
        let ahead = target - self.last_sample;
        let mut steps = [0.0; 2];
        for (step, axis) in steps.iter_mut().zip(&mut self.axes) {
            *step = axis.frame(ahead, frame_period);
        }
        steps
    }

    /// Whether the finger has rested longer than samples are extrapolated: nothing more will be
    /// handed out until the next sample.
    pub fn is_idle(&self, now: f64) -> bool {
        now - self.last_sample > STALE && self.axes.iter().all(|axis| axis.sent == axis.input)
    }

    /// Ends the gesture: what is left of the input, so the total handed out equals it.
    pub fn finish(&mut self) -> [f64; 2] {
        self.active = false;
        let [x, y] = &mut self.axes;
        [x.finish(), y.finish()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: f64 = 1.0 / 120.0;

    /// Feeds `events` (time, dy) and returns the per-frame y steps over `frames` frames.
    fn run(events: &[(f64, f64)], frames: usize) -> (Vec<f64>, f64) {
        let mut resampler = WheelResampler::default();
        resampler.begin(0.0);
        let mut next = 0;
        let mut steps = Vec::new();
        for i in 1..=frames {
            let now = i as f64 * FRAME;
            while next < events.len() && events[next].0 <= now {
                resampler.sample(events[next].0, [0.0, events[next].1]);
                next += 1;
            }
            steps.push(resampler.frame(now + FRAME, FRAME)[1]);
        }
        let rest = resampler.finish()[1];
        (steps, rest)
    }

    #[test]
    fn uneven_events_give_even_steps() {
        // 1.5 px/ms, with events alternating 4 and 12.67 ms apart (deltas in proportion).
        let mut events = Vec::new();
        let mut t = 0.0;
        for i in 0..60 {
            let gap = if i % 2 == 0 { 0.004 } else { 0.01267 };
            t += gap;
            events.push((t, (gap * 1500.0_f64).round()));
        }
        let (steps, _) = run(&events, 60);
        let steady = &steps[30..55];
        let (min, max) =
            steady.iter().fold((f64::MAX, f64::MIN), |(a, b), &s| (a.min(s), b.max(s)));
        assert!(max - min <= 2.0, "steps {steady:?}");
    }

    #[test]
    fn hands_out_exactly_the_input() {
        let events: Vec<_> = (1..40).map(|i| (i as f64 * 0.0083, (i % 7) as f64)).collect();
        let total: f64 = events.iter().map(|e| e.1).sum();
        let (steps, rest) = run(&events, 45);
        assert_eq!(steps.iter().sum::<f64>() + rest, total);
        assert!(steps.iter().all(|s| s.fract() == 0.0));
    }

    #[test]
    fn close_events_do_not_jump() {
        // A gesture's first events often come 1-3 ms apart.
        let events = [(0.001, 0.0), (0.002, 3.0), (0.010, 2.0), (0.018, 2.0), (0.026, 2.0)];
        let (steps, _) = run(&events, 6);
        assert!(steps.iter().all(|s| s.abs() <= 4.0), "steps {steps:?}");
    }

    #[test]
    fn resting_finger_settles_on_the_input() {
        let events: Vec<_> = (1..20).map(|i| (i as f64 * 0.0083, 10.0)).collect();
        let (steps, rest) = run(&events, 40);
        assert_eq!(rest, 0.0, "steps {steps:?}");
    }
}

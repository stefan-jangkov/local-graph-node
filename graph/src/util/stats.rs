use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// One bin of durations. The bin starts at time `start`, and we've added `count`
/// entries to it whose durations add up to `duration`
struct Bin {
    start: Instant,
    duration: Duration,
    count: u32,
}

impl Bin {
    fn new(start: Instant) -> Self {
        Self {
            start,
            duration: Duration::from_millis(0),
            count: 0,
        }
    }

    /// Add a new measurement to the bin
    fn add(&mut self, duration: Duration) {
        self.count += 1;
        self.duration += duration;
    }

    /// Remove the measurements for `other` from this bin. Only used to
    /// keep a running total of measurements in `MovingStats`
    fn remove(&mut self, other: &Bin) {
        self.count -= other.count;
        self.duration -= other.duration;
    }

    /// Return `true` if the average of measurements in this bin is above
    /// `duration`
    fn average_gt(&self, duration: Duration) -> bool {
        // Compute self.duration / self.count > duration as
        // self.duration > duration * self.count. If the RHS
        // oveflows, we assume the average would have been smaller
        // than any duration
        duration
            .checked_mul(self.count)
            .map(|rhs| self.duration > rhs)
            .unwrap_or(false)
    }
}

/// Collect statistics over a moving window of size `window_size`. To keep
/// the amount of memory needed to store the values inside the window
/// constant, values are put into bins of size `bin_size`. For example, using
/// a `window_size` of 5 minutes and a bin size of one second would use
/// 300 bins. Each bin has constant size
pub struct MovingStats {
    window_size: Duration,
    bin_size: Duration,
    /// The buffer with measurements. The back has the most recent entries,
    /// and the front has the oldest entries
    bins: VecDeque<Bin>,
    /// Sum over the values in `elements` The `start` of this bin
    /// is meaningless
    total: Bin,
}

impl MovingStats {
    pub fn new(window_size: Duration, bin_size: Duration) -> Self {
        let capacity = if bin_size.as_millis() > 0 {
            window_size.as_millis() as usize / bin_size.as_millis() as usize
        } else {
            1
        };
        MovingStats {
            window_size,
            bin_size,
            bins: VecDeque::with_capacity(capacity),
            total: Bin::new(Instant::now()),
        }
    }

    /// Return `true` if the average of measurements in within `window_size`
    /// is above `duration`
    pub fn average_gt(&self, duration: Duration) -> bool {
        // Depending on how often add() is called, we should
        // call expire_bins first, but that would require taking a
        // `&mut self`
        self.total.average_gt(duration)
    }

    /// Return the average over the current window in milliseconds
    pub fn average(&self) -> Option<Duration> {
        self.total.duration.checked_div(self.total.count)
    }

    pub fn add(&mut self, duration: Duration) {
        self.add_at(Instant::now(), duration);
    }

    /// Add an entry with the given timestamp. Note that the entry will
    /// still be added either to the current latest bin or a new
    /// latest bin. It is expected that subsequent calls to `add_at` still
    /// happen with monotonically increasing `now` values. If the `now`
    /// values do not monotonically increase, the average calculation
    /// becomes imprecise because values are expired later than they
    /// should be.
    pub fn add_at(&mut self, now: Instant, duration: Duration) {
        let need_new_bin = self
            .bins
            .back()
            .map(|bin| now.saturating_duration_since(bin.start) >= self.bin_size)
            .unwrap_or(true);
        if need_new_bin {
            self.bins.push_back(Bin::new(now));
        }
        self.expire_bins(now);
        // unwrap is fine because we just added a bin if there wasn't one
        // before
        let bin = self.bins.back_mut().unwrap();
        bin.add(duration);
        self.total.add(duration);
    }

    fn expire_bins(&mut self, now: Instant) {
        while self
            .bins
            .front()
            .map(|existing| now.saturating_duration_since(existing.start) >= self.window_size)
            .unwrap_or(false)
        {
            self.bins.pop_front().map(|existing| {
                self.total.remove(&existing);
            });
        }
    }

    pub fn duration(&self) -> Duration {
        self.total.duration
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[allow(dead_code)]
    fn dump_bin(msg: &str, bin: &Bin, start: Instant) {
        println!(
            "bin[{}]: age={}ms count={} duration={}ms",
            msg,
            bin.start.saturating_duration_since(start).as_millis(),
            bin.count,
            bin.duration.as_millis()
        );
    }

    #[test]
    fn add_one_const() {
        let mut stats = MovingStats::new(Duration::from_secs(5), Duration::from_secs(1));
        let start = Instant::now();
        for i in 0..10 {
            stats.add_at(start + Duration::from_secs(i), Duration::from_secs(1));
        }
        assert_eq!(5, stats.bins.len());
        for (i, bin) in stats.bins.iter().enumerate() {
            assert_eq!(1, bin.count);
            assert_eq!(Duration::from_secs(1), bin.duration);
            assert_eq!(Duration::from_secs(i as u64 + 5), (bin.start - start));
        }
        assert_eq!(5, stats.total.count);
        assert_eq!(Duration::from_secs(5), stats.total.duration);
        assert!(stats.average_gt(Duration::from_millis(900)));
        assert!(!stats.average_gt(Duration::from_secs(1)));
    }

    #[test]
    fn add_four_linear() {
        let mut stats = MovingStats::new(Duration::from_secs(5), Duration::from_secs(1));
        let start = Instant::now();
        for i in 0..40 {
            stats.add_at(
                start + Duration::from_millis(250 * i),
                Duration::from_secs(i),
            );
        }
        assert_eq!(5, stats.bins.len());
        for (b, bin) in stats.bins.iter().enumerate() {
            assert_eq!(4, bin.count);
            assert_eq!(Duration::from_secs(86 + 16 * b as u64), bin.duration);
        }
        assert_eq!(20, stats.total.count);
        assert_eq!(Duration::from_secs(5 * 86 + 16 * 10), stats.total.duration);
    }
}

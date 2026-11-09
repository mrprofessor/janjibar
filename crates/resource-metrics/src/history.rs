//! Per-object usage history for the session: one ring buffer per pod and
//! node, trimmed to a time window.
//!
//! Points are packed into 12 bytes (seconds as `u32`, values as `f32`). Two
//! [`model::Sample`] series would take 48 bytes a point. At one point every
//! 15 seconds, an hour for 5,000 pods is 14 MB packed against 58 MB as
//! samples. `f32` keeps about seven significant digits, plenty for a chart.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, SystemTime};

use model::{MetricSeries, Sample, Usage, UsageHistory, UsageObject};

/// The most points one ring holds, whatever the window: an hour at one
/// point every five seconds, metrics-server's finest resolution.
const MAX_POINTS: usize = 720;

/// One poll's values for one object.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Point {
    /// Seconds since the Unix epoch, as metrics-server reports them.
    secs: u32,
    /// Cores.
    cpu: f32,
    /// Bytes.
    memory: f32,
}

/// Every object's recent usage.
#[derive(Debug)]
pub(crate) struct History {
    rings: HashMap<UsageObject, VecDeque<Point>>,
    window: Duration,
}

impl History {
    /// An empty history that keeps `window` of points per object.
    pub(crate) fn new(window: Duration) -> Self {
        Self {
            rings: HashMap::new(),
            window,
        }
    }

    /// Adds one object's usage. A sample no newer than the last one is
    /// skipped: metrics-server answers with the same sample until its next
    /// scrape.
    pub(crate) fn record(&mut self, object: &UsageObject, usage: &Usage) {
        let point = Point {
            secs: unix_secs(usage.cpu.time),
            cpu: narrow(usage.cpu.value),
            memory: narrow(usage.memory.value),
        };
        let ring = self.rings.entry(object.clone()).or_default();
        if ring.back().is_some_and(|last| last.secs >= point.secs) {
            return;
        }
        if ring.len() == MAX_POINTS {
            ring.pop_front();
        }
        ring.push_back(point);
    }

    /// Drops points older than the window before `now`, and objects left
    /// with none. `now` should be the cluster's time, from the samples, so
    /// clock skew does not empty the history.
    pub(crate) fn trim(&mut self, now: SystemTime) {
        let cutoff = unix_secs(
            now.checked_sub(self.window)
                .unwrap_or(SystemTime::UNIX_EPOCH),
        );
        self.rings.retain(|_, ring| {
            while ring.front().is_some_and(|p| p.secs < cutoff) {
                ring.pop_front();
            }
            !ring.is_empty()
        });
    }

    /// One object's history as series. Empty series for an object never seen.
    pub(crate) fn series(&self, object: &UsageObject) -> UsageHistory {
        let points = self.rings.get(object);
        let samples = |value: fn(&Point) -> f32| -> Vec<Sample> {
            points
                .into_iter()
                .flatten()
                .map(|p| Sample {
                    time: SystemTime::UNIX_EPOCH + Duration::from_secs(u64::from(p.secs)),
                    value: f64::from(value(p)),
                })
                .collect()
        };
        UsageHistory {
            object: object.clone(),
            cpu: MetricSeries {
                labels: object.labels(),
                samples: samples(|p| p.cpu),
            },
            memory: MetricSeries {
                labels: object.labels(),
                samples: samples(|p| p.memory),
            },
        }
    }

    /// Objects with at least one point.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.rings.len()
    }
}

fn unix_secs(time: SystemTime) -> u32 {
    let secs = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    // u32 seconds last until 2106.
    u32::try_from(secs).unwrap_or(u32::MAX)
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "chart values need about seven significant digits, which f32 keeps"
)]
fn narrow(value: f64) -> f32 {
    value as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn usage(secs: u64, cpu: f64, memory: f64) -> Usage {
        Usage {
            cpu: Sample {
                time: at(secs),
                value: cpu,
            },
            memory: Sample {
                time: at(secs),
                value: memory,
            },
        }
    }

    fn pod(name: &str) -> UsageObject {
        UsageObject::Pod {
            namespace: "shop".into(),
            name: name.into(),
        }
    }

    #[test]
    fn keeps_samples_in_order_as_series() {
        let mut history = History::new(Duration::from_hours(1));
        history.record(&pod("web-1"), &usage(1_000, 0.25, 1024.0));
        history.record(&pod("web-1"), &usage(1_015, 0.5, 2048.0));
        let series = history.series(&pod("web-1"));
        assert_eq!(
            series.cpu.samples,
            [
                Sample {
                    time: at(1_000),
                    value: 0.25
                },
                Sample {
                    time: at(1_015),
                    value: 0.5
                },
            ]
        );
        assert_eq!(series.memory.samples[1].value, 2048.0);
        assert_eq!(series.cpu.labels["pod"], "web-1");
        assert_eq!(series.memory.labels["namespace"], "shop");
    }

    #[test]
    fn skips_a_repeated_sample() {
        let mut history = History::new(Duration::from_hours(1));
        history.record(&pod("web-1"), &usage(1_000, 0.25, 1.0));
        history.record(&pod("web-1"), &usage(1_000, 0.25, 1.0));
        history.record(&pod("web-1"), &usage(990, 0.1, 1.0));
        assert_eq!(history.series(&pod("web-1")).cpu.samples.len(), 1);
    }

    #[test]
    fn trims_to_the_window_and_forgets_gone_objects() {
        let mut history = History::new(Duration::from_mins(1));
        history.record(&pod("old"), &usage(1_000, 1.0, 1.0));
        history.record(&pod("web-1"), &usage(1_000, 1.0, 1.0));
        history.record(&pod("web-1"), &usage(1_050, 2.0, 1.0));
        history.trim(at(1_070));
        assert_eq!(history.len(), 1);
        let samples = history.series(&pod("web-1")).cpu.samples;
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].value, 2.0);
        assert_eq!(history.series(&pod("old")).cpu.samples, []);
    }

    #[test]
    fn a_ring_never_outgrows_its_cap() {
        let mut history = History::new(Duration::from_hours(24));
        for i in 0..(MAX_POINTS as u64 + 10) {
            history.record(&pod("busy"), &usage(i, 1.0, 1.0));
        }
        let samples = history.series(&pod("busy")).cpu.samples;
        assert_eq!(samples.len(), MAX_POINTS);
        assert_eq!(samples[0].time, at(10));
    }

    #[test]
    fn an_unknown_object_has_empty_series() {
        let history = History::new(Duration::from_hours(1));
        let series = history.series(&UsageObject::Node("n1".into()));
        assert_eq!(series.cpu.samples, []);
        assert_eq!(series.cpu.labels["node"], "n1");
    }
}

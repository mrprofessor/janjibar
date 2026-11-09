//! kubectl-style ages, like `45s`, `7m12s`, `3d4h`.

/// Formats an age the way `kubectl get` does.
///
/// This is a port of `HumanDuration` from
/// `k8s.io/apimachinery/pkg/util/duration`: precise when young, coarser with
/// time. A clock skew of up to one second in the future reads as `0s`; more
/// than that is `<invalid>`.
pub fn human_age(seconds: i64) -> String {
    if seconds < -1 {
        return "<invalid>".to_owned();
    }
    if seconds < 0 {
        return "0s".to_owned();
    }
    if seconds < 60 * 2 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 10 {
        let s = seconds % 60;
        return if s == 0 {
            format!("{minutes}m")
        } else {
            format!("{minutes}m{s}s")
        };
    }
    if minutes < 60 * 3 {
        return format!("{minutes}m");
    }
    let hours = seconds / 3600;
    if hours < 8 {
        let m = minutes % 60;
        return if m == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h{m}m")
        };
    }
    if hours < 48 {
        return format!("{hours}h");
    }
    let days = hours / 24;
    if hours < 24 * 8 {
        let h = hours % 24;
        return if h == 0 {
            format!("{days}d")
        } else {
            format!("{days}d{h}h")
        };
    }
    if hours < 24 * 365 * 2 {
        return format!("{days}d");
    }
    let years = days / 365;
    if hours < 24 * 365 * 8 {
        let dy = days % 365;
        return if dy == 0 {
            format!("{years}y")
        } else {
            format!("{years}y{dy}d")
        };
    }
    format!("{years}y")
}

#[cfg(test)]
mod tests {
    use super::human_age;

    const MIN: i64 = 60;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;

    #[test]
    fn clock_skew() {
        assert_eq!(human_age(-2), "<invalid>");
        assert_eq!(human_age(-1), "0s");
    }

    #[test]
    fn seconds_up_to_two_minutes() {
        assert_eq!(human_age(0), "0s");
        assert_eq!(human_age(59), "59s");
        assert_eq!(human_age(119), "119s");
    }

    #[test]
    fn minutes_and_seconds_under_ten_minutes() {
        assert_eq!(human_age(2 * MIN), "2m");
        assert_eq!(human_age(2 * MIN + 5), "2m5s");
        assert_eq!(human_age(9 * MIN + 59), "9m59s");
    }

    #[test]
    fn minutes_under_three_hours() {
        assert_eq!(human_age(10 * MIN + 30), "10m");
        assert_eq!(human_age(179 * MIN), "179m");
    }

    #[test]
    fn hours_and_minutes_under_eight_hours() {
        assert_eq!(human_age(3 * HOUR), "3h");
        assert_eq!(human_age(6 * HOUR + 12 * MIN), "6h12m");
    }

    #[test]
    fn hours_under_two_days() {
        assert_eq!(human_age(8 * HOUR + 30 * MIN), "8h");
        assert_eq!(human_age(47 * HOUR), "47h");
    }

    #[test]
    fn days_and_hours_under_eight_days() {
        assert_eq!(human_age(2 * DAY), "2d");
        assert_eq!(human_age(3 * DAY + 4 * HOUR), "3d4h");
        assert_eq!(human_age(7 * DAY + 23 * HOUR), "7d23h");
    }

    #[test]
    fn days_under_two_years() {
        assert_eq!(human_age(8 * DAY), "8d");
        assert_eq!(human_age(12 * DAY + 5 * HOUR), "12d");
        assert_eq!(human_age(729 * DAY), "729d");
    }

    #[test]
    fn years() {
        assert_eq!(human_age(730 * DAY), "2y");
        assert_eq!(human_age(800 * DAY), "2y70d");
        assert_eq!(human_age(9 * 365 * DAY), "9y");
    }
}

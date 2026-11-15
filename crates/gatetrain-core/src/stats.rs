//! Numeric helpers that match Python: statistics.median, statistics.quantiles(n=4) and analysis.fmt_dur.

fn sorted(v: &[f64]) -> Vec<f64> {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    s
}

/// statistics.median. Precondition: `v` is not empty.
pub fn median(v: &[f64]) -> f64 {
    let s = sorted(v);
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

/// statistics.quantiles(v, n=4), the default "exclusive" method. Precondition: `v.len() >= 2`.
pub fn quartiles(v: &[f64]) -> [f64; 3] {
    let d = sorted(v);
    let ld = d.len() as i64;
    let m = ld + 1;
    let mut out = [0.0; 3];
    for i in 1..=3i64 {
        let j = (i * m / 4).clamp(1, ld - 1);
        let delta = i * m - j * 4;
        out[(i - 1) as usize] = (d[(j - 1) as usize] * (4 - delta) as f64 + d[j as usize] * delta as f64) / 4.0;
    }
    out
}

/// analysis.fmt_dur: whole seconds (ties to even), as `1h02m03s`, `2m03s` or `3s`.
pub fn fmt_dur(sec: f64) -> String {
    let sec = sec.round_ties_even() as i64;
    let (h, m, s) = (sec / 3600, sec % 3600 / 60, sec % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_matches_python() {
        assert_eq!(median(&[1.0]), 1.0);
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
        assert_eq!(median(&[1.5, 2.5, 10.0, 0.25, 7.0]), 2.5);
    }

    #[test]
    fn quartiles_match_python() {
        assert_eq!(quartiles(&[3.0, 1.0, 2.0]), [1.0, 2.0, 3.0]);
        assert_eq!(quartiles(&[4.0, 1.0, 3.0, 2.0]), [1.25, 2.5, 3.75]);
        assert_eq!(quartiles(&[1.5, 2.5, 10.0, 0.25, 7.0]), [0.875, 2.5, 8.5]);
        assert_eq!(quartiles(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]), [2.25, 4.5, 6.75]);
    }

    #[test]
    fn fmt_dur_matches_python() {
        let cases = [(0.0, "0s"), (0.5, "0s"), (1.5, "2s"), (2.5, "2s"), (59.5, "1m00s"), (60.0, "1m00s"), (3599.5, "1h00m00s"), (3661.0, "1h01m01s"), (86400.0, "24h00m00s"), (0.49, "0s"), (90061.4, "25h01m01s")];
        for (x, want) in cases {
            assert_eq!(fmt_dur(x), want, "{x}");
        }
    }

    #[test]
    fn one_decimal_rounding_agrees_with_python() {
        assert_eq!(format!("{:.1}", 0.25), "0.2");
        assert_eq!(format!("{:.1}", 0.35), "0.3");
        assert_eq!(format!("{:.1}", 0.45), "0.5");
    }
}

//! Times as people read them: UTC (`utc`) and this computer's local time (`local_time`).

/// `secs` as `YYYY-MM-DD HH:MM UTC`.
pub fn utc(secs: i64) -> String {
    // Days to a civil date (Howard Hinnant's `civil_from_days`).
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// `secs` in local time: `HH:MM`, after the date (`YYYY-MM-DD HH:MM`) when it is not today.
pub fn local_time(secs: i64) -> String {
    let tm = |t: i64| {
        // SAFETY: `tm` is plain data, and `localtime_r` writes only the one we pass it.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&(t as libc::time_t), &mut tm) };
        tm
    };
    let (t, today) = (tm(secs), tm(crate::daemon::ipc::now() as i64));
    let hm = format!("{:02}:{:02}", t.tm_hour, t.tm_min);
    if (t.tm_year, t.tm_yday) == (today.tm_year, today.tm_yday) {
        hm
    } else {
        format!(
            "{:04}-{:02}-{:02} {hm}",
            t.tm_year + 1900,
            t.tm_mon + 1,
            t.tm_mday
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn utc_formats_a_unix_time() {
        assert_eq!(super::utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(super::utc(1_792_728_600), "2026-10-23 04:10 UTC");
        assert_eq!(super::utc(951_782_400), "2000-02-29 00:00 UTC");
    }
}

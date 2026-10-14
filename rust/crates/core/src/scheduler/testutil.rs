use std::sync::atomic::AtomicU64;

use chrono::{TimeZone as _, Utc};

pub(super) static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

pub(super) fn utc_ms(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
        .unwrap()
        .timestamp_millis()
}

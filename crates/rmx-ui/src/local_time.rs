//! Wall-clock time in the local zone, from the C library: no time crate is worth its size here.

/// A moment broken down in the local zone.
#[derive(Clone, Copy, Debug)]
pub struct LocalTime {
    pub year: i32,
    pub month: i32,
    pub day: i32,
    pub hour: i32,
    pub minute: i32,
}

/// `seconds` since the Unix epoch, in the local zone.
pub fn local_time(seconds: u64) -> Option<LocalTime> {
    let seconds = seconds as libc::time_t;
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    // Safety: both pointers are to live values for the length of the call.
    #[cfg(unix)]
    let converted = !unsafe { libc::localtime_r(&seconds, &mut local) }.is_null();
    #[cfg(windows)]
    let converted = unsafe { libc::localtime_s(&mut local, &seconds) } == 0;
    converted.then_some(LocalTime {
        year: local.tm_year + 1900,
        month: local.tm_mon + 1,
        day: local.tm_mday,
        hour: local.tm_hour,
        minute: local.tm_min,
    })
}

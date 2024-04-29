use std::ffi::{OsStr, OsString};
use std::fmt::Write;
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Datelike, Offset, Timelike, Utc};
use chrono_tz::Tz;

use crate::concurrency::thread::MachineCallback;
use crate::*;

/// Returns the time elapsed between the provided time and the unix epoch as a `Duration`.
pub fn system_time_to_duration<'tcx>(time: &SystemTime) -> InterpResult<'tcx, Duration> {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| err_unsup_format!("times before the Unix epoch are not supported").into())
}

impl<'mir, 'tcx: 'mir> EvalContextExt<'mir, 'tcx> for crate::MiriInterpCx<'mir, 'tcx> {}
pub trait EvalContextExt<'mir, 'tcx: 'mir>: crate::MiriInterpCxExt<'mir, 'tcx> {
    fn clock_gettime(
        &mut self,
        clk_id_op: &OpTy<'tcx, Provenance>,
        tp_op: &OpTy<'tcx, Provenance>,
    ) -> InterpResult<'tcx, Scalar<Provenance>> {
        // This clock support is deliberately minimal because a lot of clock types have fiddly
        // properties (is it possible for Miri to be suspended independently of the host?). If you
        // have a use for another clock type, please open an issue.

        let this = self.eval_context_mut();

        this.assert_target_os_is_unix("clock_gettime");

        let clk_id = this.read_scalar(clk_id_op)?.to_i32()?;
        let tp = this.deref_pointer_as(tp_op, this.libc_ty_layout("timespec"))?;

        let absolute_clocks;
        let mut relative_clocks;

        match this.tcx.sess.target.os.as_ref() {
            "linux" | "freebsd" => {
                // Linux and FreeBSD have two main kinds of clocks. REALTIME clocks return the actual time since the
                // Unix epoch, including effects which may cause time to move backwards such as NTP.
                // Linux further distinguishes regular and "coarse" clocks, but the "coarse" version
                // is just specified to be "faster and less precise", so we implement both the same way.
                absolute_clocks = vec![
                    this.eval_libc_i32("CLOCK_REALTIME"),
                    this.eval_libc_i32("CLOCK_REALTIME_COARSE"),
                ];
                // The second kind is MONOTONIC clocks for which 0 is an arbitrary time point, but they are
                // never allowed to go backwards. We don't need to do any additional monotonicity
                // enforcement because std::time::Instant already guarantees that it is monotonic.
                relative_clocks = vec![
                    this.eval_libc_i32("CLOCK_MONOTONIC"),
                    this.eval_libc_i32("CLOCK_MONOTONIC_COARSE"),
                ];
            }
            "macos" => {
                absolute_clocks = vec![this.eval_libc_i32("CLOCK_REALTIME")];
                relative_clocks = vec![this.eval_libc_i32("CLOCK_MONOTONIC")];
                // `CLOCK_UPTIME_RAW` supposed to not increment while the system is asleep... but
                // that's not really something a program running inside Miri can tell, anyway.
                // We need to support it because std uses it.
                relative_clocks.push(this.eval_libc_i32("CLOCK_UPTIME_RAW"));
            }
            target => throw_unsup_format!("`clock_gettime` is not supported on target OS {target}"),
        }

        let duration = if absolute_clocks.contains(&clk_id) {
            this.check_no_isolation("`clock_gettime` with `REALTIME` clocks")?;
            system_time_to_duration(&SystemTime::now())?
        } else if relative_clocks.contains(&clk_id) {
            this.machine.clock.now().duration_since(this.machine.clock.anchor())
        } else {
            let einval = this.eval_libc("EINVAL");
            this.set_last_error(einval)?;
            return Ok(Scalar::from_i32(-1));
        };

        let tv_sec = duration.as_secs();
        let tv_nsec = duration.subsec_nanos();

        this.write_int_fields(&[tv_sec.into(), tv_nsec.into()], &tp)?;

        Ok(Scalar::from_i32(0))
    }

    fn gettimeofday(
        &mut self,
        tv_op: &OpTy<'tcx, Provenance>,
        tz_op: &OpTy<'tcx, Provenance>,
    ) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        this.assert_target_os_is_unix("gettimeofday");
        this.check_no_isolation("`gettimeofday`")?;

        let tv = this.deref_pointer_as(tv_op, this.libc_ty_layout("timeval"))?;

        // Using tz is obsolete and should always be null
        let tz = this.read_pointer(tz_op)?;
        if !this.ptr_is_null(tz)? {
            let einval = this.eval_libc("EINVAL");
            this.set_last_error(einval)?;
            return Ok(-1);
        }

        let duration = system_time_to_duration(&SystemTime::now())?;
        let tv_sec = duration.as_secs();
        let tv_usec = duration.subsec_micros();

        this.write_int_fields(&[tv_sec.into(), tv_usec.into()], &tv)?;

        Ok(0)
    }

    // The localtime() function shall convert the time in seconds since the Epoch pointed to by
    // timer into a broken-down time, expressed as a local time.
    // https://linux.die.net/man/3/localtime_r
    fn localtime_r(
        &mut self,
        timep: &OpTy<'tcx, Provenance>,
        result_op: &OpTy<'tcx, Provenance>,
    ) -> InterpResult<'tcx, Pointer<Option<Provenance>>> {
        let this = self.eval_context_mut();

        this.assert_target_os_is_unix("localtime_r");
        this.check_no_isolation("`localtime_r`")?;

        let timep = this.deref_pointer(timep)?;
        let result = this.deref_pointer_as(result_op, this.libc_ty_layout("tm"))?;

        // The input "represents the number of seconds elapsed since the Epoch,
        // 1970-01-01 00:00:00 +0000 (UTC)".
        let sec_since_epoch: i64 = this
            .read_scalar(&timep)?
            .to_int(this.libc_ty_layout("time_t").size)?
            .try_into()
            .unwrap();
        let dt_utc: DateTime<Utc> =
            DateTime::from_timestamp(sec_since_epoch, 0).expect("Invalid timestamp");

        // Figure out what time zone is in use
        let tz = this.get_env_var(OsStr::new("TZ"))?.unwrap_or_else(|| OsString::from("UTC"));
        let tz = match tz.into_string() {
            Ok(tz) => Tz::from_str(&tz).unwrap_or(Tz::UTC),
            _ => Tz::UTC,
        };

        // Convert that to local time, then return the broken-down time value.
        let dt: DateTime<Tz> = dt_utc.with_timezone(&tz);

        // This value is always set to -1, because there is no way to know if dst is in effect with
        // chrono crate yet.
        // This may not be consistent with libc::localtime_r's result.
        let tm_isdst = -1;

        // tm_zone represents the timezone value in the form of: +0730, +08, -0730 or -08.
        // This may not be consistent with libc::localtime_r's result.
        let offset_in_seconds = dt.offset().fix().local_minus_utc();
        let tm_gmtoff = offset_in_seconds;
        let mut tm_zone = String::new();
        if offset_in_seconds < 0 {
            tm_zone.push('-');
        } else {
            tm_zone.push('+');
        }
        let offset_hour = offset_in_seconds.abs() / 3600;
        write!(tm_zone, "{:02}", offset_hour).unwrap();
        let offset_min = (offset_in_seconds.abs() % 3600) / 60;
        if offset_min != 0 {
            write!(tm_zone, "{:02}", offset_min).unwrap();
        }

        // FIXME: String de-duplication is needed so that we only allocate this string only once
        // even when there are multiple calls to this function.
        let tm_zone_ptr =
            this.alloc_os_str_as_c_str(&OsString::from(tm_zone), MiriMemoryKind::Machine.into())?;

        this.write_pointer(tm_zone_ptr, &this.project_field_named(&result, "tm_zone")?)?;
        this.write_int_fields_named(
            &[
                ("tm_sec", dt.second().into()),
                ("tm_min", dt.minute().into()),
                ("tm_hour", dt.hour().into()),
                ("tm_mday", dt.day().into()),
                ("tm_mon", dt.month0().into()),
                ("tm_year", dt.year().checked_sub(1900).unwrap().into()),
                ("tm_wday", dt.weekday().num_days_from_sunday().into()),
                ("tm_yday", dt.ordinal0().into()),
                ("tm_isdst", tm_isdst),
                ("tm_gmtoff", tm_gmtoff.into()),
            ],
            &result,
        )?;

        Ok(result.ptr())
    }
    #[allow(non_snake_case, clippy::arithmetic_side_effects)]
    fn GetSystemTimeAsFileTime(
        &mut self,
        shim_name: &str,
        LPFILETIME_op: &OpTy<'tcx, Provenance>,
    ) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();

        this.assert_target_os("windows", shim_name);
        this.check_no_isolation(shim_name)?;

        let filetime = this.deref_pointer_as(LPFILETIME_op, this.windows_ty_layout("FILETIME"))?;

        let NANOS_PER_SEC = this.eval_windows_u64("time", "NANOS_PER_SEC");
        let INTERVALS_PER_SEC = this.eval_windows_u64("time", "INTERVALS_PER_SEC");
        let INTERVALS_TO_UNIX_EPOCH = this.eval_windows_u64("time", "INTERVALS_TO_UNIX_EPOCH");
        let NANOS_PER_INTERVAL = NANOS_PER_SEC / INTERVALS_PER_SEC;
        let SECONDS_TO_UNIX_EPOCH = INTERVALS_TO_UNIX_EPOCH / INTERVALS_PER_SEC;

        let duration = system_time_to_duration(&SystemTime::now())?
            + Duration::from_secs(SECONDS_TO_UNIX_EPOCH);
        let duration_ticks = u64::try_from(duration.as_nanos() / u128::from(NANOS_PER_INTERVAL))
            .map_err(|_| err_unsup_format!("programs running more than 2^64 Windows ticks after the Windows epoch are not supported"))?;

        let dwLowDateTime = u32::try_from(duration_ticks & 0x00000000FFFFFFFF).unwrap();
        let dwHighDateTime = u32::try_from((duration_ticks & 0xFFFFFFFF00000000) >> 32).unwrap();
        this.write_int_fields(&[dwLowDateTime.into(), dwHighDateTime.into()], &filetime)?;

        Ok(())
    }

    #[allow(non_snake_case)]
    fn QueryPerformanceCounter(
        &mut self,
        lpPerformanceCount_op: &OpTy<'tcx, Provenance>,
    ) -> InterpResult<'tcx, Scalar<Provenance>> {
        let this = self.eval_context_mut();

        this.assert_target_os("windows", "QueryPerformanceCounter");

        // QueryPerformanceCounter uses a hardware counter as its basis.
        // Miri will emulate a counter with a resolution of 1 nanosecond.
        let duration = this.machine.clock.now().duration_since(this.machine.clock.anchor());
        let qpc = i64::try_from(duration.as_nanos()).map_err(|_| {
            err_unsup_format!("programs running longer than 2^63 nanoseconds are not supported")
        })?;
        this.write_scalar(Scalar::from_i64(qpc), &this.deref_pointer(lpPerformanceCount_op)?)?;
        Ok(Scalar::from_i32(-1)) // return non-zero on success
    }

    #[allow(non_snake_case)]
    fn QueryPerformanceFrequency(
        &mut self,
        lpFrequency_op: &OpTy<'tcx, Provenance>,
    ) -> InterpResult<'tcx, Scalar<Provenance>> {
        let this = self.eval_context_mut();

        this.assert_target_os("windows", "QueryPerformanceFrequency");

        // Retrieves the frequency of the hardware performance counter.
        // The frequency of the performance counter is fixed at system boot and
        // is consistent across all processors.
        // Miri emulates a "hardware" performance counter with a resolution of 1ns,
        // and thus 10^9 counts per second.
        this.write_scalar(
            Scalar::from_i64(1_000_000_000),
            &this.deref_pointer_as(lpFrequency_op, this.machine.layouts.u64)?,
        )?;
        Ok(Scalar::from_i32(-1)) // Return non-zero on success
    }

    fn mach_absolute_time(&self) -> InterpResult<'tcx, Scalar<Provenance>> {
        let this = self.eval_context_ref();

        this.assert_target_os("macos", "mach_absolute_time");

        // This returns a u64, with time units determined dynamically by `mach_timebase_info`.
        // We return plain nanoseconds.
        let duration = this.machine.clock.now().duration_since(this.machine.clock.anchor());
        let res = u64::try_from(duration.as_nanos()).map_err(|_| {
            err_unsup_format!("programs running longer than 2^64 nanoseconds are not supported")
        })?;
        Ok(Scalar::from_u64(res))
    }

    fn mach_timebase_info(
        &mut self,
        info_op: &OpTy<'tcx, Provenance>,
    ) -> InterpResult<'tcx, Scalar<Provenance>> {
        let this = self.eval_context_mut();

        this.assert_target_os("macos", "mach_timebase_info");

        let info = this.deref_pointer_as(info_op, this.libc_ty_layout("mach_timebase_info"))?;

        // Since our emulated ticks in `mach_absolute_time` *are* nanoseconds,
        // no scaling needs to happen.
        let (numer, denom) = (1, 1);
        this.write_int_fields(&[numer.into(), denom.into()], &info)?;

        Ok(Scalar::from_i32(0)) // KERN_SUCCESS
    }

    fn nanosleep(
        &mut self,
        req_op: &OpTy<'tcx, Provenance>,
        _rem: &OpTy<'tcx, Provenance>, // Signal handlers are not supported, so rem will never be written to.
    ) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        this.assert_target_os_is_unix("nanosleep");

        let req = this.deref_pointer_as(req_op, this.libc_ty_layout("timespec"))?;

        let duration = match this.read_timespec(&req)? {
            Some(duration) => duration,
            None => {
                let einval = this.eval_libc("EINVAL");
                this.set_last_error(einval)?;
                return Ok(-1);
            }
        };
        // If adding the duration overflows, let's just sleep for an hour. Waking up early is always acceptable.
        let now = this.machine.clock.now();
        let timeout_time = now
            .checked_add(duration)
            .unwrap_or_else(|| now.checked_add(Duration::from_secs(3600)).unwrap());

        let active_thread = this.get_active_thread();
        this.block_thread(active_thread, BlockReason::Sleep);

        this.register_timeout_callback(
            active_thread,
            CallbackTime::Monotonic(timeout_time),
            Box::new(UnblockCallback { thread_to_unblock: active_thread }),
        );

        Ok(0)
    }

    #[allow(non_snake_case)]
    fn Sleep(&mut self, timeout: &OpTy<'tcx, Provenance>) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();

        this.assert_target_os("windows", "Sleep");

        let timeout_ms = this.read_scalar(timeout)?.to_u32()?;

        let duration = Duration::from_millis(timeout_ms.into());
        let timeout_time = this.machine.clock.now().checked_add(duration).unwrap();

        let active_thread = this.get_active_thread();
        this.block_thread(active_thread, BlockReason::Sleep);

        this.register_timeout_callback(
            active_thread,
            CallbackTime::Monotonic(timeout_time),
            Box::new(UnblockCallback { thread_to_unblock: active_thread }),
        );

        Ok(())
    }
}

struct UnblockCallback {
    thread_to_unblock: ThreadId,
}

impl VisitProvenance for UnblockCallback {
    fn visit_provenance(&self, _visit: &mut VisitWith<'_>) {}
}

impl<'mir, 'tcx: 'mir> MachineCallback<'mir, 'tcx> for UnblockCallback {
    fn call(&self, ecx: &mut MiriInterpCx<'mir, 'tcx>) -> InterpResult<'tcx> {
        ecx.unblock_thread(self.thread_to_unblock, BlockReason::Sleep);
        Ok(())
    }
}

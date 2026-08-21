//! Small OS performance levers for the media-plane threads.
//!
//! Windows-focused today: the capture/encode loops pace themselves with
//! `thread::sleep`, whose quantum is the process's timer resolution — the
//! system default is ~15.6 ms, a hard fps ceiling for a 60 fps budget loop
//! and dead time in the async-MFT poll. Desktops often *appear* fine only
//! because some other app (typically a browser) holds the resolution at
//! 1 ms; a headless or idle host loses that by luck of what else is
//! running. Holding it ourselves while a stream is live removes the luck.
//!
//! Same story for scheduling: the capture/encode and input-injection
//! threads exist precisely for the moments the machine is loaded, and at
//! normal priority they degrade in lockstep with the load they're trying
//! to stream through. A single step above normal keeps them responsive
//! without starving anything (well below the audio/realtime classes).
//!
//! macOS gets the QoS-class arm of the same idea: on Apple silicon there is
//! **no thread-affinity API** — a thread's QoS class is the only mechanism
//! that steers it onto performance vs efficiency cores (and drives timer
//! coalescing and I/O throttling). An untagged worker thread can sit on
//! E-cores while the P-cores idle. The timer guard is Windows-only (macOS
//! timers aren't quantized the same way); everything is a silent no-op on
//! the remaining platforms.

/// Holds the OS timer resolution at 1 ms for as long as any guard lives.
/// winmm refcounts `timeBeginPeriod`/`timeEndPeriod` process-wide, so
/// nested guards are fine and drop order is free.
#[cfg(any(feature = "host", test))]
pub(crate) struct TimerResolutionGuard;

#[cfg(any(feature = "host", test))]
impl TimerResolutionGuard {
    pub(crate) fn hold() -> TimerResolutionGuard {
        #[cfg(windows)]
        unsafe {
            let _ = windows_sys::Win32::Media::timeBeginPeriod(1);
        }
        TimerResolutionGuard
    }
}

#[cfg(any(feature = "host", test))]
impl Drop for TimerResolutionGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            let _ = windows_sys::Win32::Media::timeEndPeriod(1);
        }
    }
}

/// Media threads registered for the telemetry line's per-thread CPU
/// split: (label, real thread handle). Every media thread passes through
/// [`boost_media_thread`], which makes this the one free hook.
#[cfg(windows)]
pub(crate) static MEDIA_THREADS: std::sync::Mutex<Vec<(String, isize)>> =
    std::sync::Mutex::new(Vec::new());

/// Process-wide scheduling honesty, once per process (first media thread
/// arms it). Two opt-outs that exist because Windows 11 quietly
/// second-guesses background processes:
///
///  1. **Execution-speed throttling** — the per-thread EcoQoS opt-out in
///     [`boost_media_thread`] covers the *named* media threads, but the
///     pacer's microsecond gaps run on tokio worker threads and the
///     control plane on others; the process-wide bit covers them all.
///  2. **Timer-resolution honesty** — since Windows 11, the kernel
///     IGNORES `timeBeginPeriod` for processes it classifies as
///     background/occluded. `allmystuff-serve` is a windowless sidecar —
///     exactly the shape at risk — so without this bit the 1 ms guard can
///     be a silent no-op and every sleep in the pipeline quantizes at up
///     to 15.6 ms on a stock field box.
///
/// Best-effort like every lever here; pre-Win11 boxes simply don't know
/// the second flag and honor the first.
fn opt_out_process_throttling() {
    #[cfg(windows)]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| unsafe {
            use windows_sys::Win32::System::Threading::{
                GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
                PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
                PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
            };
            let state = PROCESS_POWER_THROTTLING_STATE {
                Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                    | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
                StateMask: 0, // "never throttle, never ignore my timer raise"
            };
            let ok = SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                &state as *const PROCESS_POWER_THROTTLING_STATE as *const core::ffi::c_void,
                core::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
            tracing::info!(
                "process scheduling honesty: power-throttling + timer-resolution opt-outs {}",
                if ok != 0 {
                    "armed"
                } else {
                    "unavailable (pre-Win11 — fine)"
                }
            );
        });
    }
}

/// Sleep `d` with ~50–100 µs accuracy instead of the timer-wheel/quantum
/// milliseconds: a high-resolution waitable timer carries the wait to
/// within a short tail, and a bounded spin walks the rest on the
/// monotonic clock. The pacer's final tail waits can sit below the ordinary
/// timer wheel's resolution; rounding each one up by a millisecond materially
/// distorts its configured drain rate (measured by the `pace gaps` line). The
/// spin tail costs one core for ≤200 µs per call; callers only use this for the
/// final few milliseconds of a pacing reservation, never bulk waits.
/// Never returns early — the spin's exit is the monotonic elapsed test.
pub(crate) fn precise_sleep(d: std::time::Duration) {
    let start = std::time::Instant::now();
    const SPIN_TAIL: std::time::Duration = std::time::Duration::from_micros(200);
    if d > SPIN_TAIL {
        let coarse = d - SPIN_TAIL;
        #[cfg(windows)]
        {
            use std::cell::Cell;
            use windows_sys::Win32::Foundation::HANDLE;
            use windows_sys::Win32::System::Threading::{
                CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject,
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE, TIMER_ALL_ACCESS,
            };
            thread_local! {
                // One timer per thread, created on first use; 0 = tried
                // and unavailable (pre-1803), fall through to plain sleep.
                static TIMER: Cell<Option<HANDLE>> = const { Cell::new(None) };
            }
            let handle = TIMER.with(|t| match t.get() {
                Some(h) => h,
                None => {
                    let h = unsafe {
                        CreateWaitableTimerExW(
                            std::ptr::null(),
                            std::ptr::null(),
                            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                            TIMER_ALL_ACCESS,
                        )
                    };
                    t.set(Some(h));
                    h
                }
            });
            if !handle.is_null() {
                // Negative due time = relative, in 100 ns units.
                let due = -i64::try_from(coarse.as_nanos() / 100).unwrap_or(i64::MAX);
                unsafe {
                    if SetWaitableTimer(handle, &due, 0, None, std::ptr::null(), 0) != 0 {
                        WaitForSingleObject(handle, INFINITE);
                    } else {
                        std::thread::sleep(coarse);
                    }
                }
            } else {
                std::thread::sleep(coarse);
            }
        }
        #[cfg(not(windows))]
        std::thread::sleep(coarse);
    }
    while start.elapsed() < d {
        std::hint::spin_loop();
    }
}

/// Join this thread to the Multimedia Class Scheduler ("Games" class) —
/// the 16–26 priority band with a scheduler-managed quota, far above
/// `ABOVE_NORMAL` without `REALTIME`'s starvation hazard. Zero effect on
/// an idle box; under real contention (a game pegging every core — the
/// Game posture's whole environment) it is what keeps scheduling-latency
/// tails sub-millisecond. **Opt-in** (`ALLMYSTUFF_MMCSS=1`) until field
/// soak: MMCSS's companion network throttle (`NetworkThrottlingIndex`,
/// default ~120 Mbps of DPCs) can clip Studio-class rates while an MMCSS
/// task runs — the field checklist documents the registry pairing.
/// Runtime-loaded from avrt.dll like every driver-adjacent dependency;
/// the handle is deliberately kept for the thread's life.
fn mmcss_join() {
    #[cfg(windows)]
    {
        static WANT: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
            let on = std::env::var("ALLMYSTUFF_MMCSS").is_ok_and(|v| !v.is_empty() && v != "0");
            if on {
                tracing::info!("ALLMYSTUFF_MMCSS on: media threads join the MMCSS Games class");
            }
            on
        });
        if !*WANT {
            return;
        }
        type AvSet = unsafe extern "system" fn(*const u16, *mut u32) -> isize;
        static AV_SET: std::sync::LazyLock<Option<AvSet>> = std::sync::LazyLock::new(|| unsafe {
            use windows::core::PCSTR;
            use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
            let dll = LoadLibraryA(PCSTR(c"avrt.dll".as_ptr() as *const u8)).ok()?;
            let p = GetProcAddress(
                dll,
                PCSTR(c"AvSetMmThreadCharacteristicsW".as_ptr() as *const u8),
            )?;
            Some(std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                AvSet,
            >(p))
        });
        if let Some(join) = *AV_SET {
            let task: Vec<u16> = "Games\0".encode_utf16().collect();
            let mut index = 0u32;
            let handle = unsafe { join(task.as_ptr(), &mut index) };
            if handle == 0 {
                tracing::debug!("MMCSS join failed for {:?}", std::thread::current().name());
            }
        }
    }
}

/// Raise this process's WDDM GPU-scheduling class to High — the queue
/// our per-frame `VideoProcessorBlt` and encode submits ride, which a
/// foreground game can otherwise flood (zero effect uncontended;
/// ms-class under a saturated 3D engine — the telemetry line's
/// "3d 99%" signature). **Opt-in** (`ALLMYSTUFF_GPU_SCHED=1`) like
/// MMCSS: on a host that IS the game box, raising our class taxes the
/// game's own submits, and that trade belongs to the operator. Realtime
/// needs privilege and starves compositors — deliberately not offered.
/// Semi-documented gdi32 export, runtime-loaded, best-effort like every
/// lever in this file.
fn raise_gpu_scheduling_class() {
    #[cfg(windows)]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let on = std::env::var("ALLMYSTUFF_GPU_SCHED").is_ok_and(|v| !v.is_empty() && v != "0");
            if !on {
                return;
            }
            type SetClass =
                unsafe extern "system" fn(windows_sys::Win32::Foundation::HANDLE, i32) -> i32;
            let set: Option<SetClass> = unsafe {
                use windows::core::PCSTR;
                use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
                LoadLibraryA(PCSTR(c"gdi32.dll".as_ptr() as *const u8))
                    .ok()
                    .and_then(|dll| {
                        GetProcAddress(
                            dll,
                            PCSTR(c"D3DKMTSetProcessSchedulingPriorityClass".as_ptr() as *const u8),
                        )
                    })
                    .map(|p| {
                        std::mem::transmute::<unsafe extern "system" fn() -> isize, SetClass>(p)
                    })
            };
            let Some(set) = set else {
                tracing::debug!("D3DKMTSetProcessSchedulingPriorityClass unavailable");
                return;
            };
            // D3DKMT_SCHEDULINGPRIORITYCLASS_HIGH = 4.
            let status = unsafe {
                set(
                    windows_sys::Win32::System::Threading::GetCurrentProcess(),
                    4,
                )
            };
            tracing::info!(
                "ALLMYSTUFF_GPU_SCHED on: WDDM scheduling class High {}",
                if status == 0 {
                    "armed"
                } else {
                    "refused (NTSTATUS != 0)"
                }
            );
        });
    }
}

/// Boost the current thread for media work — called at the top of each
/// capture/encode thread and the input-injector thread. Three levers, all
/// best-effort:
///
///  1. **Priority** one step above normal — under load the stream must not
///     degrade in lockstep with the load it exists to carry.
///  2. **EcoQoS opt-out** — Windows 11 can classify a background process's
///     threads as "efficiency" work and park them on E-cores at low clocks
///     (the headless node is exactly the shape that gets tagged). The
///     power-throttling opt-out declares this thread latency-sensitive.
///  3. **Performance-core preference** on hybrid CPUs (Intel P+E) — a CPU-set
///     hint listing the highest-efficiency-class cores. An encode/convert
///     pass that fits a 16 ms budget on a P-core can blow it on a
///     downclocked E-core; the hint keeps the media threads where the
///     budget holds while the scheduler retains the right to overrule
///     (CPU sets are a preference, unlike hard affinity — nothing starves
///     if a game owns the P-cores; our raised priority arbitrates).
pub(crate) fn boost_media_thread() {
    opt_out_process_throttling();
    mmcss_join();
    raise_gpu_scheduling_class();
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::DUPLICATE_SAME_ACCESS;
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, GetCurrentThread, SetThreadInformation, SetThreadPriority,
            SetThreadSelectedCpuSets, ThreadPowerThrottling,
            THREAD_POWER_THROTTLING_CURRENT_VERSION, THREAD_POWER_THROTTLING_EXECUTION_SPEED,
            THREAD_POWER_THROTTLING_STATE, THREAD_PRIORITY_ABOVE_NORMAL,
        };
        let thread = GetCurrentThread();
        // Register for the telemetry per-thread CPU split — the pseudo
        // handle only means "me", so duplicate a real one.
        let mut real: windows_sys::Win32::Foundation::HANDLE = std::ptr::null_mut();
        if windows_sys::Win32::Foundation::DuplicateHandle(
            GetCurrentProcess(),
            thread,
            GetCurrentProcess(),
            &mut real,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        ) != 0
        {
            let name = std::thread::current().name().unwrap_or("media").to_string();
            if let Ok(mut reg) = MEDIA_THREADS.lock() {
                reg.push((name, real as isize));
            }
        }
        let _ = SetThreadPriority(thread, THREAD_PRIORITY_ABOVE_NORMAL);
        // ControlMask names the knob, StateMask=0 turns throttling OFF —
        // i.e. "never EcoQoS this thread".
        let throttle = THREAD_POWER_THROTTLING_STATE {
            Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0,
        };
        let _ = SetThreadInformation(
            thread,
            ThreadPowerThrottling,
            &throttle as *const THREAD_POWER_THROTTLING_STATE as *const core::ffi::c_void,
            core::mem::size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
        );
        let p_cores = performance_core_ids();
        if !p_cores.is_empty() {
            let _ = SetThreadSelectedCpuSets(thread, p_cores.as_ptr(), p_cores.len() as u32);
        }
    }
    #[cfg(target_os = "macos")]
    unsafe {
        // USER_INTERACTIVE is the P-core / no-coalescing tier; the relative
        // priority offset stays 0. Apple's guidance reserves this tier for
        // work the user is actively perceiving — a live stream's capture,
        // encode, and input-injection threads are exactly that.
        // Declared locally (symbol lives in libSystem, linked by every
        // macOS process): the libc crate's QoS bindings vary by vintage.
        const QOS_CLASS_USER_INTERACTIVE: core::ffi::c_uint = 0x21; // <sys/qos.h>
        extern "C" {
            fn pthread_set_qos_class_self_np(
                qos_class: core::ffi::c_uint,
                relative_priority: core::ffi::c_int,
            ) -> core::ffi::c_int;
        }
        let _ = pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

/// CPU-set ids of the "performance" cores — the highest efficiency class
/// the machine reports. Empty on homogeneous CPUs (nothing to prefer) and
/// on query failure. Queried once per process.
#[cfg(windows)]
fn performance_core_ids() -> &'static [u32] {
    use std::sync::LazyLock;
    static IDS: LazyLock<Vec<u32>> = LazyLock::new(|| unsafe {
        use windows_sys::Win32::System::SystemInformation::{
            GetSystemCpuSetInformation, CPU_SET_INFORMATION_TYPE, SYSTEM_CPU_SET_INFORMATION,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        const CPU_SET_INFORMATION: CPU_SET_INFORMATION_TYPE = 0; // CpuSetInformation
        let mut needed = 0u32;
        let _ = GetSystemCpuSetInformation(
            core::ptr::null_mut(),
            0,
            &mut needed,
            GetCurrentProcess(),
            0,
        );
        if needed == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u8; needed as usize];
        if GetSystemCpuSetInformation(
            buf.as_mut_ptr() as *mut SYSTEM_CPU_SET_INFORMATION,
            needed,
            &mut needed,
            GetCurrentProcess(),
            0,
        ) == 0
        {
            return Vec::new();
        }
        // The buffer is a packed run of variable-size records; walk by each
        // record's own Size.
        let mut sets: Vec<(u32, u8)> = Vec::new();
        let mut at = 0usize;
        // Guard the FULL record size, not just the 8-byte Size/Type header:
        // the reference below spans the whole struct, so a truncated final
        // record (malformed OS buffer) would otherwise form a reference past
        // the allocation (UB / OOB read of the CpuSet union).
        while at + core::mem::size_of::<SYSTEM_CPU_SET_INFORMATION>() <= needed as usize {
            let info = &*(buf.as_ptr().add(at) as *const SYSTEM_CPU_SET_INFORMATION);
            if info.Size == 0 {
                break;
            }
            if info.Type == CPU_SET_INFORMATION {
                let cpu = &info.Anonymous.CpuSet;
                sets.push((cpu.Id, cpu.EfficiencyClass));
            }
            at += info.Size as usize;
        }
        let max_class = sets.iter().map(|&(_, c)| c).max().unwrap_or(0);
        let min_class = sets.iter().map(|&(_, c)| c).min().unwrap_or(0);
        if max_class == min_class {
            return Vec::new(); // homogeneous — nothing to prefer
        }
        let total = sets.len();
        let ids: Vec<u32> = sets
            .into_iter()
            .filter(|&(_, c)| c == max_class)
            .map(|(id, _)| id)
            .collect();
        tracing::info!(
            "hybrid CPU: media threads prefer the {} performance cores (of {total} logical)",
            ids.len()
        );
        ids
    });
    &IDS
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn boost_raises_priority_and_prefers_p_cores_on_hybrid() {
        std::thread::spawn(|| {
            use windows_sys::Win32::System::Threading::{
                GetCurrentThread, GetThreadPriority, GetThreadSelectedCpuSets,
                THREAD_PRIORITY_ABOVE_NORMAL,
            };
            boost_media_thread();
            let got = unsafe { GetThreadPriority(GetCurrentThread()) };
            assert_eq!(got, THREAD_PRIORITY_ABOVE_NORMAL);
            // Production deliberately skips the CPU-set call when discovery
            // reports no P-cores, so there is no CPU-set contract to inspect
            // on homogeneous machines or Windows versions lacking the API.
            let expect = performance_core_ids();
            if expect.is_empty() {
                return;
            }

            let mut selected = vec![0u32; expect.len()];
            let mut count = selected.len() as u32;
            let ok = unsafe {
                GetThreadSelectedCpuSets(
                    GetCurrentThread(),
                    selected.as_mut_ptr(),
                    selected.len() as u32,
                    &mut count,
                )
            };
            assert_ne!(ok, 0, "GetThreadSelectedCpuSets failed");
            selected.truncate(count as usize);
            assert_eq!(selected, expect, "selected CPU sets mirror the P-core list");
        })
        .join()
        .expect("boost thread");
    }

    /// The precise sleep must never return early, and its typical overshoot
    /// must be micro-scale, not quantum-scale — the property the pacer's
    /// sub-millisecond gaps lean on. Windows may deschedule a correct sleeper
    /// for an arbitrary interval, especially on a shared CI host, so judge the
    /// median rather than treating one scheduler outlier as a timer failure.
    /// A broken waitable-timer path still quantizes every sample near 15.6 ms.
    #[test]
    fn precise_sleep_holds_sub_quantum_accuracy() {
        let _guard = TimerResolutionGuard::hold();
        let mut overshoots = Vec::new();
        for req_us in [300u64, 500, 900, 1500] {
            let req = std::time::Duration::from_micros(req_us);
            for _ in 0..5 {
                let t = std::time::Instant::now();
                precise_sleep(req);
                let got = t.elapsed();
                assert!(got >= req, "returned early: {got:?} < {req:?}");
                overshoots.push(got - req);
            }
        }
        overshoots.sort_unstable();
        let median = overshoots[overshoots.len() / 2];
        let worst = overshoots[overshoots.len() - 1];
        assert!(
            median < std::time::Duration::from_millis(2),
            "median overshoot {median:?} looks quantized, not precise (worst {worst:?})"
        );
        println!("precise_sleep median overshoot: {median:?}; worst: {worst:?}");
    }

    #[test]
    fn timer_guard_is_balanced_and_reentrant() {
        // Two nested guards, dropped out of order — winmm refcounts, so the
        // only observable contract is "no panic, no error"; the precise-sleep
        // test above covers the timing floor separately.
        let a = TimerResolutionGuard::hold();
        let b = TimerResolutionGuard::hold();
        drop(a);
        drop(b);
    }
}

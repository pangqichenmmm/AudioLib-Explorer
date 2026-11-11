// crates/audiolib-audio/src/soundtouch_lifecycle_tests.rs
//! Isolated Windows native-heap regression for real SoundTouch construction/drop.
use super::SoundTouchProcessor;

const CHILD_NAME: &str =
    "audition::soundtouch_processor::lifecycle_tests::native_lifecycle_memory_child";
const CHILD_MARKER: &str = "AUDIOLIB_SOUNDTOUCH_LIFECYCLE_CHILD";
const RESULT_MARKER: &str = "SoundTouch native lifecycle memory accepted";

#[test]
fn repeated_native_processing_releases_windows_heap_allocations() {
    // This test owns a fresh test process, so concurrently running Rust tests and
    // the application's normal data/device state cannot affect the measurement.
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            CHILD_NAME,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_MARKER, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        result.status.success() && stdout.contains(RESULT_MARKER),
        "Native lifecycle child failed: {}\n{stdout}\n{stderr}",
        result.status
    );
}

#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCountersEx {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_usage: usize,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut core::ffi::c_void;
    fn K32GetProcessMemoryInfo(
        process: *mut core::ffi::c_void,
        counters: *mut ProcessMemoryCountersEx,
        size: u32,
    ) -> i32;
}

fn private_bytes() -> usize {
    let size = u32::try_from(std::mem::size_of::<ProcessMemoryCountersEx>()).unwrap();
    let mut counters = ProcessMemoryCountersEx {
        cb: size,
        ..Default::default()
    };
    // SAFETY: The pseudo-handle is this owned child process. The repr(C) buffer
    // matches PROCESS_MEMORY_COUNTERS_EX and remains writable for its exact size;
    // Windows only fills the buffer and does not retain its address.
    let success = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) };
    assert_ne!(success, 0, "{}", std::io::Error::last_os_error());
    counters.private_usage
}

fn process_and_drop(samples: &[f32]) {
    let mut processor = SoundTouchProcessor::new(48_000, 1.5, 7.0).unwrap();
    processor.put_samples(samples).unwrap();
    processor.flush();
    let mut output = [0.0_f32; 512];
    let mut total = 0;
    for _ in 0..64 {
        let received = processor.receive_samples(&mut output);
        assert!(received <= output.len() / 2);
        assert!(
            output[..received * 2]
                .iter()
                .all(|sample| sample.is_finite())
        );
        total += received;
        if received == 0 {
            assert!(total > 0, "The native processor did not produce audio");
            return;
        }
    }
    panic!("A short SoundTouch stream did not finish in bounded receives");
}

#[test]
#[ignore = "Executed by the isolated native lifecycle parent test"]
fn native_lifecycle_memory_child() {
    assert_eq!(std::env::var(CHILD_MARKER).as_deref(), Ok("1"));
    let samples: Vec<_> = (0..256)
        .flat_map(|frame| {
            let value = (std::f32::consts::TAU * 440.0 * frame as f32 / 48_000.0).sin() * 0.2;
            [value, -value]
        })
        .collect();
    // Warm native SIMD dispatch, Rust/C++ allocator caches and processor buffers
    // before observing growth. Freed blocks may remain cached, so this measures
    // bounded private commit rather than requiring every byte to return to the OS.
    for _ in 0..64 {
        process_and_drop(&samples);
    }
    let baseline = private_bytes();
    let mut observations = Vec::new();
    for _ in 0..2 {
        for _ in 0..512 {
            process_and_drop(&samples);
        }
        observations.push(private_bytes());
    }
    const MAX_GROWTH: usize = 16 * 1024 * 1024;
    assert!(
        observations
            .iter()
            .all(|bytes| bytes.saturating_sub(baseline) <= MAX_GROWTH),
        "Native heap grows across 1024 complete processor lifetimes: baseline={baseline}, after batches={observations:?}, allowed growth={MAX_GROWTH}"
    );
    println!("{RESULT_MARKER}: baseline={baseline}, after batches={observations:?}");
}

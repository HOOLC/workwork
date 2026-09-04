use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use zork_agent_testkit::{
    measure_real_startup, prepare_real_startup_fixture, RealStartupLimits, RealStartupPerformance,
};

const CONTRACT_SESSION_COUNT: usize = 100_000;
const MAX_PEAK_RSS_BYTES: u64 = 512 * 1024 * 1024;

// Contract: docs/zork-agent-architecture.md [STARTUP-03, PERF-03]
fn main() {
    let arguments = std::env::args()
        .skip(1)
        .filter(|value| value != "--bench")
        .collect::<Vec<_>>();
    if arguments.first().map(String::as_str) == Some("--measure-child") {
        run_measurement_child(&arguments);
        return;
    }

    let session_count = arguments
        .first()
        .map(|value| {
            value
                .parse::<usize>()
                .expect("session count must be an integer")
        })
        .unwrap_or(CONTRACT_SESSION_COUNT);
    assert!(
        session_count >= 3,
        "startup benchmark requires at least three sessions"
    );

    let fixture_root = tempfile::tempdir().expect("create startup benchmark fixture root");
    let data_root = fixture_root.path().join("data");
    let fixture_started = Instant::now();
    prepare_real_startup_fixture(&data_root, session_count)
        .expect("create real startup benchmark fixture");
    let fixture_elapsed = fixture_started.elapsed();

    let output = Command::new(std::env::current_exe().expect("resolve benchmark executable"))
        .arg("--measure-child")
        .arg(&data_root)
        .arg(session_count.to_string())
        .output()
        .expect("start clean startup measurement process");
    assert!(
        output.status.success(),
        "startup measurement process failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let measured: RealStartupPerformance =
        serde_json::from_slice(&output.stdout).expect("parse startup measurement report");
    // Keep the phase timings even when an acceptance threshold fails.
    eprintln!(
        "startup measurement: {}",
        serde_json::to_string(&measured).unwrap()
    );

    assert_eq!(measured.session_count, session_count);
    assert!(
        measured.exact_recovery_ms <= 100,
        "exact requested recovery took {} ms",
        measured.exact_recovery_ms
    );
    assert!(
        measured.prioritized_recovery_ms <= 1_000,
        "prioritized recovery took {} ms",
        measured.prioritized_recovery_ms
    );
    assert!(
        measured.total_startup_ms <= 10_000,
        "complete startup recovery took {} ms",
        measured.total_startup_ms
    );
    let peak_rss_bytes = measured
        .peak_rss_bytes
        .expect("the release startup benchmark requires Unix RSS measurement");
    assert!(
        peak_rss_bytes <= MAX_PEAK_RSS_BYTES,
        "startup peak RSS was {:.1} MiB",
        peak_rss_bytes as f64 / (1024.0 * 1024.0)
    );

    println!(
        "real startup: {session_count} sessions; fixture {:.3}s (excluded); scan {} ms; exact {} ms; prioritized {} ms; background {} ms; total {} ms; peak RSS {:.1} MiB",
        fixture_elapsed.as_secs_f64(),
        measured.discovery_scan_ms,
        measured.exact_recovery_ms,
        measured.prioritized_recovery_ms,
        measured.background_recovery_ms,
        measured.total_startup_ms,
        peak_rss_bytes as f64 / (1024.0 * 1024.0),
    );
}

fn run_measurement_child(arguments: &[String]) {
    assert_eq!(arguments.len(), 3, "invalid measurement child arguments");
    let data_root = Path::new(&arguments[1]);
    let session_count = arguments[2]
        .parse::<usize>()
        .expect("measurement child session count must be an integer");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("create startup measurement runtime");
    match runtime.block_on(measure_real_startup(
        data_root,
        session_count,
        RealStartupLimits {
            // The parent enforces the 10 second contract. A wider watchdog
            // lets a failed run report its measured duration instead of only
            // saying which stage crossed the deadline.
            complete_startup: Duration::from_secs(60),
            ..RealStartupLimits::default()
        },
    )) {
        Ok(measured) => println!(
            "{}",
            serde_json::to_string(&measured).expect("serialize startup measurement")
        ),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

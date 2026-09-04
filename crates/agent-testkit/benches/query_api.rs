use std::process::Command;
use std::time::Duration;

use zork_agent_testkit::{measure_history_fragment_queries, measure_query_apis};

const MAX_HISTORY_PEAK_RSS_BYTES: u64 = 256 * 1024 * 1024;
const MIN_HISTORY_QUERIES_PER_SECOND: f64 = 20.0;

// Contract: docs/zork-agent-architecture.md [QUERY-01, PERF-04]
fn main() {
    let arguments = std::env::args()
        .skip(1)
        .filter(|value| value != "--bench")
        .collect::<Vec<_>>();
    if arguments.first().map(String::as_str) == Some("--history-child") {
        run_history_child(&arguments);
        return;
    }
    let mut arguments = arguments.into_iter();
    let (default_segments, default_events_per_segment, default_iterations) =
        if cfg!(debug_assertions) {
            (32, 128, 5)
        } else {
            (256, 256, 100)
        };
    let segment_count = parse(arguments.next(), default_segments, "segment count");
    let events_per_segment = parse(
        arguments.next(),
        default_events_per_segment,
        "events per segment",
    );
    let iterations = parse(arguments.next(), default_iterations, "iteration count");
    let measured = measure_query_apis(segment_count, events_per_segment, iterations)
        .expect("measure real-file SessionQuery APIs");

    let api_limit = |release_ms| {
        Duration::from_millis(if cfg!(debug_assertions) {
            250
        } else {
            release_ms
        })
    };
    for (name, duration, limit) in [
        ("last_commit", measured.last_commit, api_limit(10)),
        ("snapshot_window", measured.snapshot_window, api_limit(10)),
        ("history_after", measured.history_after, api_limit(20)),
        ("history_before", measured.history_before, api_limit(250)),
        ("event_lookup", measured.event_lookup, api_limit(20)),
        ("discovery", measured.discovery, api_limit(20)),
        (
            "exists",
            measured.exists,
            Duration::from_millis(if cfg!(debug_assertions) { 10 } else { 1 }),
        ),
    ] {
        assert!(
            duration <= limit,
            "{name} averaged {duration:?}, exceeding {limit:?}"
        );
    }
    let minimum_throughput = if cfg!(debug_assertions) {
        5_000.0
    } else {
        50_000.0
    };
    assert!(
        measured.all_commits_per_second() >= minimum_throughput,
        "all_commits_forward processed only {:.0} events/s",
        measured.all_commits_per_second()
    );

    println!(
        "SessionQuery: {} segments / {} events; last {:?}; snapshot {:?}; after {:?}; before {:?}; event {:?}; discover {:?}; exists {:?}; all {:?} ({:.0} events/s)",
        measured.segment_count,
        measured.event_count,
        measured.last_commit,
        measured.snapshot_window,
        measured.history_after,
        measured.history_before,
        measured.event_lookup,
        measured.discovery,
        measured.exists,
        measured.all_commits,
        measured.all_commits_per_second(),
    );

    let (stress_events_per_segment, fragment_len) = if cfg!(debug_assertions) {
        (8_192, 256)
    } else {
        (65_536, 1_024)
    };
    let output = Command::new(std::env::current_exe().expect("resolve benchmark executable"))
        .arg("--history-child")
        .arg("2")
        .arg(stress_events_per_segment.to_string())
        .arg(fragment_len.to_string())
        .arg("1024")
        .arg("8")
        .output()
        .expect("start clean history query measurement process");
    assert!(
        output.status.success(),
        "history query measurement process failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let fragments =
        serde_json::from_slice::<zork_agent_testkit::HistoryFragmentPerformance>(&output.stdout)
            .expect("parse history query measurement report");
    for (mode, queries_per_second) in [
        ("serial", fragments.serial_queries_per_second()),
        ("parallel", fragments.parallel_queries_per_second()),
    ] {
        assert!(
            queries_per_second >= MIN_HISTORY_QUERIES_PER_SECOND,
            "{mode} history fragments sustained {queries_per_second:.1} queries/s, below {MIN_HISTORY_QUERIES_PER_SECOND:.0} queries/s"
        );
    }
    let peak_rss_bytes = fragments
        .peak_rss_bytes
        .expect("the release history benchmark requires Unix RSS measurement");
    assert!(
        peak_rss_bytes <= MAX_HISTORY_PEAK_RSS_BYTES,
        "history query peak RSS was {:.1} MiB, exceeding {:.1} MiB",
        mib(peak_rss_bytes),
        mib(MAX_HISTORY_PEAK_RSS_BYTES),
    );
    let rss_before_queries_bytes = fragments
        .rss_before_queries_bytes
        .expect("the release history benchmark requires Unix RSS measurement");
    println!(
        "History fragments: {} events in one session; {} queries x {} events; serial {:?} ({:.0}/s); parallel {:?} ({:.0}/s, {} workers); RSS {:.1} -> {:.1} MiB (limit {:.0} MiB)",
        fragments.event_count,
        fragments.query_count,
        fragments.fragment_len,
        fragments.serial,
        fragments.serial_queries_per_second(),
        fragments.parallel,
        fragments.parallel_queries_per_second(),
        fragments.worker_count,
        mib(rss_before_queries_bytes),
        mib(peak_rss_bytes),
        mib(MAX_HISTORY_PEAK_RSS_BYTES),
    );
}

fn run_history_child(arguments: &[String]) {
    assert_eq!(arguments.len(), 6, "invalid history child arguments");
    let values = arguments[1..]
        .iter()
        .map(|value| {
            value
                .parse::<usize>()
                .expect("history child argument must be an integer")
        })
        .collect::<Vec<_>>();
    match measure_history_fragment_queries(values[0], values[1], values[2], values[3], values[4]) {
        Ok(measured) => println!(
            "{}",
            serde_json::to_string(&measured).expect("serialize history query measurement")
        ),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn parse(value: Option<String>, default: usize, name: &str) -> usize {
    value.map_or(default, |value| {
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be an integer"))
    })
}

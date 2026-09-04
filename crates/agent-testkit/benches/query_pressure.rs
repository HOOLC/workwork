use std::fs::File;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use zork_agent_testkit::{
    measure_query_pressure, prepare_query_pressure_fixture, QueryPressureManifest,
    QueryPressurePerformance,
};

const MIB: usize = 1024 * 1024;
const MAX_PEAK_RSS_BYTES: u64 = 256 * MIB as u64;

// Contract: docs/zork-agent-architecture.md [QUERY-01, PERF-04]
fn main() {
    let arguments = std::env::args()
        .skip(1)
        .filter(|value| value != "--bench")
        .collect::<Vec<_>>();
    if arguments.first().map(String::as_str) == Some("--measure-child") {
        run_measurement_child(&arguments);
        return;
    }

    let session_count = parse(arguments.first(), 100, "session count");
    let fragments_per_session = parse(arguments.get(1), 100, "fragments per session");
    let target_fragment_bytes = parse(arguments.get(2), 16 * MIB, "fragment bytes");
    let event_payload_bytes = parse(arguments.get(3), 16 * 1024, "event payload bytes");
    let query_count = parse(
        arguments.get(4),
        session_count.saturating_mul(fragments_per_session),
        "query count",
    );
    let max_query_events = parse(arguments.get(5), 1_000, "maximum query events");

    let root = tempfile::tempdir().expect("create query pressure fixture root");
    let data_root = root.path().join("data");
    eprintln!(
        "preparing {session_count} sessions x {fragments_per_session} fragments x {:.0} MiB logical data",
        target_fragment_bytes as f64 / MIB as f64,
    );
    let fixture_started = Instant::now();
    let manifest = prepare_query_pressure_fixture(
        &data_root,
        session_count,
        fragments_per_session,
        target_fragment_bytes,
        event_payload_bytes,
    )
    .expect("prepare query pressure fixture");
    let fixture_elapsed = fixture_started.elapsed();
    let manifest_path = root.path().join("query-pressure-manifest.json");
    serde_json::to_writer(
        File::create(&manifest_path).expect("create query pressure manifest"),
        &manifest,
    )
    .expect("write query pressure manifest");
    println!(
        "fixture: {:.1} GiB logical / {:.1} GiB physical; {} events; prepared in {:.1}s",
        gib(manifest.logical_bytes),
        gib(manifest.physical_bytes),
        manifest.event_count,
        fixture_elapsed.as_secs_f64(),
    );

    let workers = worker_sweep();
    let report_path = root.path().join("query-pressure-report.json");
    eprintln!("measuring uncached query throughput");
    let status = Command::new(std::env::current_exe().expect("resolve benchmark executable"))
        .arg("--measure-child")
        .arg(&data_root)
        .arg(&manifest_path)
        .arg(query_count.to_string())
        .arg(max_query_events.to_string())
        .arg(
            workers
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(","),
        )
        .arg(&report_path)
        .status()
        .expect("start query pressure measurement child");
    assert!(status.success(), "query pressure measurement child failed");
    let report: QueryPressurePerformance =
        serde_json::from_reader(File::open(&report_path).expect("open query pressure report"))
            .expect("parse query pressure report");
    let peak_rss = report
        .peak_rss_bytes
        .expect("the pressure benchmark requires Unix RSS measurement");
    print_report(&report);
    assert!(
        peak_rss <= MAX_PEAK_RSS_BYTES,
        "query pressure peak RSS was {:.1} MiB, exceeding {:.1} MiB",
        mib(peak_rss),
        mib(MAX_PEAK_RSS_BYTES),
    );
}

fn run_measurement_child(arguments: &[String]) {
    assert_eq!(arguments.len(), 7, "invalid pressure child arguments");
    let data_root = Path::new(&arguments[1]);
    let manifest: QueryPressureManifest =
        serde_json::from_reader(File::open(&arguments[2]).expect("open query pressure manifest"))
            .expect("parse query pressure manifest");
    let query_count = arguments[3]
        .parse::<usize>()
        .expect("query count must be an integer");
    let max_query_events = arguments[4]
        .parse::<usize>()
        .expect("maximum query events must be an integer");
    let workers = arguments[5]
        .split(',')
        .map(|value| {
            value
                .parse::<usize>()
                .expect("worker count must be an integer")
        })
        .collect::<Vec<_>>();
    let report = measure_query_pressure(
        data_root,
        &manifest,
        query_count,
        max_query_events,
        &workers,
    )
    .unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(1);
    });
    serde_json::to_writer(
        File::create(&arguments[6]).expect("create query pressure report"),
        &report,
    )
    .expect("write query pressure report");
}

fn print_report(report: &QueryPressurePerformance) {
    println!(
        "uncached: warmup {:.2}s; peak RSS {:.1} MiB (limit {:.0} MiB)",
        report.warmup.as_secs_f64(),
        report.peak_rss_bytes.map(mib).unwrap_or_default(),
        mib(MAX_PEAK_RSS_BYTES),
    );
    for mode in &report.modes {
        println!(
            "  {:>2} workers: {:>8.1} queries/s; p50 {:?}; p95 {:?}; p99 {:?}",
            mode.workers, mode.queries_per_second, mode.p50, mode.p95, mode.p99,
        );
    }
}

fn worker_sweep() -> Vec<usize> {
    let maximum = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(8);
    [1, 2, 4, 8, maximum]
        .into_iter()
        .filter(|workers| *workers <= maximum)
        .fold(Vec::new(), |mut values, workers| {
            if values.last() != Some(&workers) {
                values.push(workers);
            }
            values
        })
}

fn parse(value: Option<&String>, default: usize, name: &str) -> usize {
    value.map_or(default, |value| {
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be an integer"))
    })
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / MIB as f64
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * MIB as f64)
}

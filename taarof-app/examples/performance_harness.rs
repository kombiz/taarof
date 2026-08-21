use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let workload = args.next().unwrap_or_else(|| usage());
    let iterations = args
        .next()
        .unwrap_or_else(|| usage())
        .parse::<usize>()
        .unwrap_or_else(|_| usage());
    if iterations == 0 || args.next().is_some() {
        usage();
    }

    let started = Instant::now();
    let result = match workload.as_str() {
        "runtime-probe" => taarof_app::performance::runtime_probe(iterations),
        "session-restore" => taarof_app::performance::session_restore(iterations),
        _ => usage(),
    };
    let elapsed_ns = started.elapsed().as_nanos();
    println!(
        "{{\"workload\":\"{workload}\",\"iterations\":{iterations},\"elapsed_ns\":{elapsed_ns},\"checksum\":{},\"source_reads\":{}}}",
        result.checksum, result.source_reads
    );
}

fn usage() -> ! {
    eprintln!("usage: performance_harness <runtime-probe|session-restore> <iterations>");
    std::process::exit(2);
}

use preempt_store::{default_database_path, HistoryStore};
use std::error::Error;

fn main() {
    if let Err(error) = run() {
        eprintln!("preempt-feedback-report: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None => {}
        Some("--help" | "-h") => {
            println!("usage: preempt-feedback-report");
            println!("Report aggregate local prediction acceptance by tier.");
            return Ok(());
        }
        Some(argument) => return Err(format!("unknown argument: {argument}").into()),
    }
    if args.next().is_some() {
        return Err("this command takes no arguments".into());
    }

    let path = default_database_path().ok_or("no application data directory is available")?;
    if !path.is_file() {
        println!("No prediction feedback yet; the local database does not exist.");
        return Ok(());
    }

    let store = HistoryStore::open_default_with_keyring()?;
    let counts = store.feedback_counts()?;
    println!("Local prediction feedback (resolved outcomes only)");
    print_tier("T0 history", counts.t0_accepted, counts.t0_rejected);
    print_tier("T1 n-gram", counts.t1_accepted, counts.t1_rejected);
    print_tier("T2 local model", counts.t2_accepted, counts.t2_rejected);
    print_tier("T3 cloud", counts.t3_accepted, counts.t3_rejected);
    print_tier(
        "Overall",
        counts
            .t0_accepted
            .saturating_add(counts.t1_accepted)
            .saturating_add(counts.t2_accepted)
            .saturating_add(counts.t3_accepted),
        counts
            .t0_rejected
            .saturating_add(counts.t1_rejected)
            .saturating_add(counts.t2_rejected)
            .saturating_add(counts.t3_rejected),
    );
    println!("Only aggregate counts are shown; command text and fingerprints are not read.");
    Ok(())
}

fn print_tier(label: &str, accepted: u64, rejected: u64) {
    let total = accepted.saturating_add(rejected);
    if total == 0 {
        println!("{label}: no outcomes");
        return;
    }

    let rate = accepted as f64 * 100.0 / total as f64;
    println!("{label}: {accepted} accepted, {rejected} rejected ({rate:.1}% accepted)");
}

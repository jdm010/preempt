use at_predict::engine::Engine;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: at-suggest [--file PATH] [--top N] <prefix>");
        std::process::exit(2);
    }

    let mut file = at_predict::history::default_history_path();
    let mut top = 5usize;
    let mut prefix = String::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--file" => {
                i += 1;
                file = args.get(i).cloned().map(std::path::PathBuf::from).unwrap_or(file);
            }
            "--top" => {
                i += 1;
                top = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(top);
            }
            _ => prefix = args[i].clone(),
        }
        i += 1;
    }

    let entries = match at_predict::history::load(&file) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("at-suggest: cannot read {}: {err}", file.display());
            std::process::exit(1);
        }
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let engine = Engine::build(&entries, now);
    println!("history: {} entries from {}", entries.len(), file.display());

    match engine.suggest(&prefix, top) {
        empty if empty.is_empty() => println!("no suggestions for {prefix:?}"),
        suggestions => {
            for s in suggestions {
                println!("{prefix}\x1b[2m{}\x1b[0m    (score {:.2}, {})", s.completion, s.score, "T0");
            }
        }
    }
}

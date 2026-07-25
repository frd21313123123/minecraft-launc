fn main() {
    match mine_launcher::drive::fetch_builds() {
        Ok(builds) => {
            println!("OK: {} builds", builds.len());
            for b in &builds {
                println!("  - {} | id={} | file_id={} | file={}", b.name, b.id, b.file_id, b.filename);
            }
            if builds.is_empty() {
                std::process::exit(2);
            }
        }
        Err(e) => {
            eprintln!("ERR: {e}");
            std::process::exit(1);
        }
    }
}

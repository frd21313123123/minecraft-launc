fn main() {
    match mine_launcher::drive::fetch_builds() {
        Ok(builds) => {
            println!("OK: {} builds", builds.len());
            for b in &builds {
                let revision = b.revision_id().unwrap_or_else(|error| format!("ERR: {error}"));
                println!(
                    "  - {} | id={} | file_id={} | file={} | revision={revision}",
                    b.name, b.id, b.file_id, b.filename
                );
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

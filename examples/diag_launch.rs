fn main() {
    let game = dirs::data_dir().unwrap().join("MineLauncher").join("instances").join("createa2").join("minecraft");
    println!("game dir: {}", game.display());
    println!("installed createa2: {}", mine_launcher::drive::is_build_installed("createa2"));
    
    // ensure natives
    match mine_launcher::install::ensure_natives_for_version("neoforge-21.1.238") {
        Ok(()) => {
            let n = dirs::data_dir().unwrap().join("MineLauncher").join("minecraft").join("versions").join("neoforge-21.1.238").join("natives");
            let count = std::fs::read_dir(&n).map(|d| d.count()).unwrap_or(0);
            println!("natives after ensure (neoforge): {count} in {}", n.display());
        }
        Err(e) => println!("ensure natives err: {e}"),
    }
    match mine_launcher::install::ensure_natives_for_version("1.21.1") {
        Ok(()) => {
            let n = dirs::data_dir().unwrap().join("MineLauncher").join("minecraft").join("versions").join("1.21.1").join("natives");
            let count = std::fs::read_dir(&n).map(|d| d.count()).unwrap_or(0);
            println!("natives after ensure (1.21.1): {count} in {}", n.display());
        }
        Err(e) => println!("ensure natives 1.21.1 err: {e}"),
    }

    match mine_launcher::launch::build_launch_command_with_dir(
        "neoforge-21.1.238",
        "dimalox",
        4096,
        "",
        &game,
    ) {
        Ok(cmd) => {
            println!("cmd len: {}", cmd.len());
            println!("java: {}", cmd[0]);
            // print args containing library.path, main class-ish
            for (i, a) in cmd.iter().enumerate() {
                if a.contains("library.path") || a.contains("Bootstrap") || a.contains("main") || i < 8 || a == "-cp" {
                    println!("  [{i}] {}", if a.len() > 120 { format!("{}...", &a[..120]) } else { a.clone() });
                }
            }
            // find main class index
            if let Some(idx) = cmd.iter().position(|a| a.contains("BootstrapLauncher") || a.contains("Main")) {
                println!("main at {idx}: {}", cmd[idx]);
            }
            
            // launch with log
            let log = dirs::data_dir().unwrap().join("MineLauncher").join("last_launch.log");
            let out = std::fs::File::create(&log).unwrap();
            let err = out.try_clone().unwrap();
            let mut c = std::process::Command::new(&cmd[0]);
            c.args(&cmd[1..]).current_dir(&game).stdout(out).stderr(err);
            match c.spawn() {
                Ok(mut child) => {
                    println!("spawned pid {:?}", child.id());
                    // wait a few seconds
                    std::thread::sleep(std::time::Duration::from_secs(8));
                    match child.try_wait() {
                        Ok(Some(status)) => println!("exited early: {status}"),
                        Ok(None) => {
                            println!("still running after 8s — killing for test");
                            let _ = child.kill();
                        }
                        Err(e) => println!("wait err: {e}"),
                    }
                    println!("log: {}", log.display());
                    if let Ok(t) = std::fs::read_to_string(&log) {
                        let lines: Vec<_> = t.lines().collect();
                        println!("log lines: {}", lines.len());
                        for l in lines.iter().take(40) { println!("  {l}"); }
                        if lines.len() > 40 {
                            println!("  ...");
                            for l in lines.iter().rev().take(30).collect::<Vec<_>>().into_iter().rev() {
                                println!("  {l}");
                            }
                        }
                    }
                }
                Err(e) => println!("spawn err: {e}"),
            }
        }
        Err(e) => println!("build cmd err: {e}"),
    }
}

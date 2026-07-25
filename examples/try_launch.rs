fn main() {
    let game = dirs::data_dir().unwrap().join("MineLauncher").join("instances").join("createa2").join("minecraft");
    println!("ensure natives...");
    let _ = mine_launcher::install::ensure_natives_for_version("neoforge-21.1.238");
    let _ = mine_launcher::install::ensure_natives_for_version("1.21.1");
    match mine_launcher::launch::launch_game_with_dir("neoforge-21.1.238", "dimalox", 4096, "", &game) {
        Ok(mut child) => {
            println!("launch OK, pid {:?}", child.id());
            // wait up to 20s
            for i in 0..40 {
                std::thread::sleep(std::time::Duration::from_millis(500));
                match child.try_wait() {
                    Ok(Some(st)) => {
                        println!("exited after ~{}s: {st}", (i+1) as f32 * 0.5);
                        let log = dirs::data_dir().unwrap().join("MineLauncher").join("last_launch.log");
                        if let Ok(t) = std::fs::read_to_string(&log) {
                            let lines: Vec<_> = t.lines().collect();
                            println!("log lines {}", lines.len());
                            for l in lines.iter().rev().take(25).collect::<Vec<_>>().into_iter().rev() {
                                println!("{l}");
                            }
                        }
                        return;
                    }
                    Ok(None) => {}
                    Err(e) => { println!("wait err {e}"); return; }
                }
            }
            println!("still running after 20s — SUCCESS (killing test process)");
            let _ = child.kill();
        }
        Err(e) => {
            println!("launch ERR: {e}");
        }
    }
}

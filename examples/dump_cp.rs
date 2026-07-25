fn main() {
    let game = dirs::data_dir().unwrap().join("MineLauncher").join("instances").join("createa2").join("minecraft");
    let cmd = mine_launcher::launch::build_launch_command_with_dir("neoforge-21.1.238", "dimalox", 4096, "", &game).unwrap();
    // find -cp
    if let Some(i) = cmd.iter().position(|a| a == "-cp" || a == "-classpath") {
        let cp = &cmd[i+1];
        let sep = if cfg!(windows) { ';' } else { ':' };
        println!("classpath entries:");
        for p in cp.split(sep) {
            let lower = p.to_lowercase();
            if lower.contains("1.21.1") || lower.contains("client") || lower.contains("neoforge") || lower.contains("minecraft") {
                println!("  {p}");
            }
        }
        println!("total entries: {}", cp.split(sep).count());
        // check if 1.21.1.jar is there
        let has_vanilla = cp.split(sep).any(|p| p.ends_with("1.21.1.jar") || p.ends_with("1.21.1\\1.21.1.jar") || p.replace('/', "\\").ends_with("1.21.1\\1.21.1.jar"));
        println!("has vanilla 1.21.1.jar: {has_vanilla}");
    }
    // module path -p
    if let Some(i) = cmd.iter().position(|a| a == "-p" || a == "--module-path") {
        println!("-p: {}", &cmd[i+1][..cmd[i+1].len().min(200)]);
    }
}

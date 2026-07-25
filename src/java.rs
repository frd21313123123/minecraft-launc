use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn find_java(custom: &str) -> Option<PathBuf> {
    let custom = custom.trim();
    if !custom.is_empty() {
        let p = PathBuf::from(custom);
        if p.exists() {
            return Some(p);
        }
    }

    if let Ok(home) = std::env::var("JAVA_HOME") {
        for name in ["javaw.exe", "java.exe", "java", "javaw"] {
            let candidate = Path::new(&home).join("bin").join(name);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    if let Some(p) = which("javaw").or_else(|| which("java")) {
        return Some(p);
    }

    #[cfg(windows)]
    {
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Ok(pf) = std::env::var("ProgramFiles") {
            roots.push(PathBuf::from(&pf).join("Java"));
            roots.push(PathBuf::from(&pf).join("Eclipse Adoptium"));
            roots.push(PathBuf::from(&pf).join("Microsoft"));
            roots.push(PathBuf::from(&pf).join("Zulu"));
            roots.push(PathBuf::from(&pf).join("Amazon Corretto"));
        }
        if let Ok(pf86) = std::env::var("ProgramFiles(x86)") {
            roots.push(PathBuf::from(pf86).join("Java"));
        }
        if let Some(home) = dirs::home_dir() {
            roots.push(home.join(".jdks"));
        }

        let mut found: Vec<PathBuf> = Vec::new();
        for root in roots {
            if !root.exists() {
                continue;
            }
            collect_java_bins(&root, &mut found);
        }
        found.sort();
        found.reverse();
        if let Some(p) = found.into_iter().next() {
            return Some(p);
        }
    }

    None
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let with_exe = dir.join(format!("{name}.exe"));
            if with_exe.exists() {
                return Some(with_exe);
            }
        }
    }
    None
}

#[cfg(windows)]
fn collect_java_bins(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let javaw = path.join("bin").join("javaw.exe");
            if javaw.exists() {
                out.push(javaw);
            } else {
                let java = path.join("bin").join("java.exe");
                if java.exists() {
                    out.push(java);
                } else {
                    collect_java_bins(&path, out);
                }
            }
        }
    }
}

pub fn java_version_string(java: &Path) -> Option<String> {
    let mut cmd = Command::new(java);
    cmd.arg("-version");
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let output = cmd.output().ok()?;
    let text = String::from_utf8_lossy(&output.stderr);
    let text = if text.trim().is_empty() {
        String::from_utf8_lossy(&output.stdout).into_owned()
    } else {
        text.into_owned()
    };
    text.lines().next().map(|s| s.trim().to_string())
}

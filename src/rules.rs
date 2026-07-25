use crate::models::{Argument, ArgumentValue, Library, Rule};

#[cfg(target_os = "windows")]
const OS_NAME: &str = "windows";
#[cfg(target_os = "linux")]
const OS_NAME: &str = "linux";
#[cfg(target_os = "macos")]
const OS_NAME: &str = "osx";
#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
const OS_NAME: &str = "unknown";

#[cfg(target_arch = "x86_64")]
const ARCH: &str = "x86_64";
#[cfg(target_arch = "x86")]
const ARCH: &str = "x86";
#[cfg(target_arch = "aarch64")]
const ARCH: &str = "arm64";
#[cfg(not(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64")))]
const ARCH: &str = "unknown";

pub fn rules_allow(rules: Option<&[Rule]>) -> bool {
    let Some(rules) = rules else {
        return true;
    };
    if rules.is_empty() {
        return true;
    }

    // По спецификации Mojang: начинаем с allow, применяем правила по порядку.
    // Фактически: last matching rule wins; if no os/features match, action doesn't apply.
    let mut allowed = false;
    for rule in rules {
        if rule_matches(rule) {
            allowed = rule.action == "allow";
        }
    }
    allowed
}

fn rule_matches(rule: &Rule) -> bool {
    if let Some(features) = &rule.features {
        // demo, custom resolution и т.п. — не включаем
        for enabled in features.values() {
            if *enabled {
                return false;
            }
        }
        // features map present with all false — still a match for "disallow if feature"
        // If features is Some and we don't enable any, rules that require features fail.
        // For is_demo_user: false — we don't match rules that need demo.
        // Standard approach: if features specified, only match when our feature state equals.
        // We treat all features as false.
        for v in features.values() {
            if *v {
                return false;
            }
        }
        // If rule says features: {is_demo_user: false} — rare. Usually it's true for optional args.
        // When features key exists with true values we already returned false.
        // When features key exists, Minecraft only applies if feature equals.
        // Simplified: any features object means this is a feature-gated rule we skip (return false)
        // unless all values are false... Actually for `{"is_demo_user": true}` we skip.
        // For empty features {} match.
        if features.values().any(|v| *v) {
            return false;
        }
        // features with only false: match only if we also have those features false — yes.
    }

    if let Some(os) = &rule.os {
        if let Some(name) = &os.name {
            if name != OS_NAME {
                return false;
            }
        }
        if let Some(arch) = &os.arch {
            // Mojang uses "x86" for 32-bit; sometimes "arm"
            let ok = match arch.as_str() {
                "x86" => ARCH == "x86",
                "x86_64" | "amd64" => ARCH == "x86_64",
                "arm" | "aarch64" | "arm64" => ARCH == "arm64",
                other => other == ARCH,
            };
            if !ok {
                return false;
            }
        }
        // version regex — ignore (rarely used for libraries)
    }

    true
}

pub fn library_applies(lib: &Library) -> bool {
    rules_allow(lib.rules.as_deref())
}

pub fn expand_argument(arg: &Argument) -> Vec<String> {
    match arg {
        Argument::Simple(s) => vec![s.clone()],
        Argument::Ruled { rules, value } => {
            if !rules_allow(rules.as_deref()) {
                return Vec::new();
            }
            match value {
                ArgumentValue::Single(s) => vec![s.clone()],
                ArgumentValue::Many(v) => v.clone(),
            }
        }
    }
}

pub fn native_classifier(lib: &Library) -> Option<String> {
    let natives = lib.natives.as_ref()?;
    let key = natives.get(OS_NAME)?;
    // ${arch} substitution
    let arch_short = if ARCH == "x86_64" { "64" } else { "32" };
    Some(key.replace("${arch}", arch_short))
}

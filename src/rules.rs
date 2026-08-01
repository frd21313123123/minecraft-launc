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
    if !rules_allow(lib.rules.as_deref()) {
        return false;
    }
    // Современный формат 1.19+: `group:artifact:ver:natives-windows` — отдельная запись.
    // В JSON у windows-arm64/x86 часто только `os.name=windows`, без arch → без фильтра
    // на classpath попадают чужие natives и игра падает.
    if let Some(classifier) = name_native_classifier(&lib.name) {
        return native_classifier_matches_host(classifier);
    }
    true
}

/// Classifier из Maven-имени (`a:b:c:natives-windows` → `natives-windows`).
pub fn name_native_classifier(name: &str) -> Option<&str> {
    let mut parts = name.split(':');
    let _g = parts.next()?;
    let _a = parts.next()?;
    let _v = parts.next()?;
    let classifier = parts.next()?;
    if classifier.starts_with("natives-") {
        Some(classifier)
    } else {
        None
    }
}

/// Подходит ли classifier natives текущей ОС/архитектуре.
pub fn native_classifier_matches_host(classifier: &str) -> bool {
    match classifier {
        "natives-windows" => OS_NAME == "windows" && ARCH == "x86_64",
        "natives-windows-x86" => OS_NAME == "windows" && ARCH == "x86",
        "natives-windows-arm64" => OS_NAME == "windows" && ARCH == "arm64",
        "natives-linux" => OS_NAME == "linux" && ARCH == "x86_64",
        "natives-linux-x86" | "natives-linux-i386" => OS_NAME == "linux" && ARCH == "x86",
        "natives-linux-arm64" | "natives-linux-aarch64" => OS_NAME == "linux" && ARCH == "arm64",
        "natives-linux-arm32" | "natives-linux-arm" => OS_NAME == "linux" && ARCH == "arm64", // rare
        "natives-macos" | "natives-osx" => OS_NAME == "osx" && ARCH == "x86_64",
        "natives-macos-arm64" | "natives-osx-arm64" => OS_NAME == "osx" && ARCH == "arm64",
        // macOS patch builds / generic — только osx
        other if other.starts_with("natives-macos") || other.starts_with("natives-osx") => {
            OS_NAME == "osx"
        }
        other if other.starts_with("natives-") => false,
        _ => true,
    }
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
    // Старый формат: поле natives: { "windows": "natives-windows" }
    if let Some(natives) = lib.natives.as_ref() {
        if let Some(key) = natives.get(OS_NAME) {
            let arch_short = if ARCH == "x86_64" { "64" } else { "32" };
            return Some(key.replace("${arch}", arch_short));
        }
    }
    // Новый формат: имя `…:natives-windows` — jar сам и есть natives.
    if let Some(c) = name_native_classifier(&lib.name) {
        if native_classifier_matches_host(c) {
            return Some(c.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        expand_argument, library_applies, name_native_classifier, native_classifier,
        native_classifier_matches_host, rules_allow, ARCH, OS_NAME,
    };
    use crate::models::{Argument, ArgumentValue, Library, OsRule, Rule};
    use std::collections::HashMap;

    fn rule(action: &str) -> Rule {
        Rule {
            action: action.to_string(),
            os: None,
            features: None,
        }
    }

    fn host_rule(action: &str) -> Rule {
        Rule {
            action: action.to_string(),
            os: Some(OsRule {
                name: Some(OS_NAME.to_string()),
                arch: None,
                version: None,
            }),
            features: None,
        }
    }

    fn library(name: &str) -> Library {
        Library {
            name: name.to_string(),
            downloads: None,
            natives: None,
            rules: None,
            extract: None,
            url: None,
        }
    }

    fn host_native_classifier() -> Option<&'static str> {
        match (OS_NAME, ARCH) {
            ("windows", "x86_64") => Some("natives-windows"),
            ("windows", "x86") => Some("natives-windows-x86"),
            ("windows", "arm64") => Some("natives-windows-arm64"),
            ("linux", "x86_64") => Some("natives-linux"),
            ("linux", "x86") => Some("natives-linux-x86"),
            ("linux", "arm64") => Some("natives-linux-arm64"),
            ("osx", "x86_64") => Some("natives-macos"),
            ("osx", "arm64") => Some("natives-macos-arm64"),
            _ => None,
        }
    }

    #[test]
    fn absent_or_empty_rules_allow_an_item() {
        assert!(rules_allow(None));
        assert!(rules_allow(Some(&[])));
    }

    #[test]
    fn rules_use_the_last_matching_action_and_skip_other_platforms() {
        assert!(rules_allow(Some(&[rule("disallow"), rule("allow")])));
        assert!(!rules_allow(Some(&[
            host_rule("allow"),
            host_rule("disallow"),
        ])));
        assert!(rules_allow(Some(&[
            host_rule("disallow"),
            host_rule("allow"),
        ])));

        let non_matching_disallow = Rule {
            action: "disallow".to_string(),
            os: Some(OsRule {
                name: Some("not-this-platform".to_string()),
                arch: None,
                version: None,
            }),
            features: None,
        };
        assert!(rules_allow(Some(&[
            host_rule("allow"),
            non_matching_disallow,
        ])));
    }

    #[test]
    fn rules_match_host_architecture_and_disabled_features() {
        let matching_arch_disallow = Rule {
            action: "disallow".to_string(),
            os: Some(OsRule {
                name: Some(OS_NAME.to_string()),
                arch: Some(ARCH.to_string()),
                version: None,
            }),
            features: None,
        };
        assert!(!rules_allow(Some(&[
            host_rule("allow"),
            matching_arch_disallow,
        ])));

        let non_matching_arch_disallow = Rule {
            action: "disallow".to_string(),
            os: Some(OsRule {
                name: Some(OS_NAME.to_string()),
                arch: Some("not-this-architecture".to_string()),
                version: None,
            }),
            features: None,
        };
        assert!(rules_allow(Some(&[
            host_rule("allow"),
            non_matching_arch_disallow,
        ])));

        let required_feature = Rule {
            action: "disallow".to_string(),
            os: None,
            features: Some(HashMap::from([("is_demo_user".to_string(), true)])),
        };
        assert!(rules_allow(Some(&[host_rule("allow"), required_feature])));

        let disabled_feature = Rule {
            action: "disallow".to_string(),
            os: None,
            features: Some(HashMap::from([("is_demo_user".to_string(), false)])),
        };
        assert!(!rules_allow(Some(&[host_rule("allow"), disabled_feature])));
    }

    #[test]
    fn extracts_native_classifier_from_maven_coordinates() {
        assert_eq!(
            name_native_classifier("org.lwjgl:lwjgl:3.3.3:natives-windows"),
            Some("natives-windows")
        );
        assert_eq!(
            name_native_classifier("org.lwjgl:lwjgl:3.3.3:natives-linux:jar"),
            Some("natives-linux")
        );
        assert_eq!(name_native_classifier("org.lwjgl:lwjgl:3.3.3"), None);
        assert_eq!(
            name_native_classifier("org.lwjgl:lwjgl:3.3.3:sources"),
            None
        );
        assert_eq!(name_native_classifier("not-a-maven-name"), None);
    }

    #[test]
    fn native_classifiers_match_only_their_supported_platform_and_architecture() {
        let cases = [
            ("natives-windows", OS_NAME == "windows" && ARCH == "x86_64"),
            ("natives-windows-x86", OS_NAME == "windows" && ARCH == "x86"),
            (
                "natives-windows-arm64",
                OS_NAME == "windows" && ARCH == "arm64",
            ),
            ("natives-linux", OS_NAME == "linux" && ARCH == "x86_64"),
            ("natives-linux-x86", OS_NAME == "linux" && ARCH == "x86"),
            ("natives-linux-i386", OS_NAME == "linux" && ARCH == "x86"),
            (
                "natives-linux-aarch64",
                OS_NAME == "linux" && ARCH == "arm64",
            ),
            ("natives-macos", OS_NAME == "osx" && ARCH == "x86_64"),
            ("natives-osx-arm64", OS_NAME == "osx" && ARCH == "arm64"),
            ("natives-macos-3.3.7", OS_NAME == "osx"),
        ];

        for (classifier, expected) in cases {
            assert_eq!(
                native_classifier_matches_host(classifier),
                expected,
                "unexpected result for {classifier} on {OS_NAME}/{ARCH}"
            );
        }
        assert!(!native_classifier_matches_host("natives-unknown"));
        assert!(native_classifier_matches_host("sources"));
    }

    #[test]
    fn library_applicability_combines_rules_and_native_classifier() {
        assert!(library_applies(&library("group:artifact:1.0")));

        let mut denied_by_rule = library("group:artifact:1.0");
        denied_by_rule.rules = Some(vec![host_rule("disallow")]);
        assert!(!library_applies(&denied_by_rule));

        assert!(!library_applies(&library(
            "group:artifact:1.0:natives-unknown"
        )));

        if let Some(classifier) = host_native_classifier() {
            assert!(library_applies(&library(&format!(
                "group:artifact:1.0:{classifier}"
            ))));
        }
    }

    #[test]
    fn expands_simple_and_allowed_ruled_arguments() {
        assert_eq!(
            expand_argument(&Argument::Simple("--demo".to_string())),
            vec!["--demo"]
        );

        let allowed_single = Argument::Ruled {
            rules: Some(vec![host_rule("allow")]),
            value: ArgumentValue::Single("--quick-play".to_string()),
        };
        assert_eq!(expand_argument(&allowed_single), vec!["--quick-play"]);

        let unruled_many = Argument::Ruled {
            rules: None,
            value: ArgumentValue::Many(vec!["-cp".to_string(), "libraries/*".to_string()]),
        };
        assert_eq!(expand_argument(&unruled_many), vec!["-cp", "libraries/*"]);
    }

    #[test]
    fn ruled_arguments_are_omitted_when_rules_disallow_them() {
        let blocked = Argument::Ruled {
            rules: Some(vec![host_rule("disallow")]),
            value: ArgumentValue::Many(vec!["--should-not".to_string(), "appear".to_string()]),
        };
        assert!(expand_argument(&blocked).is_empty());
    }

    #[test]
    fn native_classifier_prefers_legacy_mapping_and_expands_architecture() {
        let mut lib = library("group:artifact:1.0:natives-unknown");
        lib.natives = Some(HashMap::from([(
            OS_NAME.to_string(),
            "legacy-${arch}".to_string(),
        )]));

        let expected_arch = if ARCH == "x86_64" { "64" } else { "32" };
        assert_eq!(
            native_classifier(&lib),
            Some(format!("legacy-{expected_arch}"))
        );
    }

    #[test]
    fn native_classifier_falls_back_to_a_matching_maven_classifier() {
        let unmatched = library("group:artifact:1.0:natives-unknown");
        assert_eq!(native_classifier(&unmatched), None);

        if let Some(classifier) = host_native_classifier() {
            let matching = library(&format!("group:artifact:1.0:{classifier}"));
            assert_eq!(native_classifier(&matching), Some(classifier.to_string()));
        }
    }
}

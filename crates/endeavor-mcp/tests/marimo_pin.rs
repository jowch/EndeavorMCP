const REQUIREMENTS_IN: &str = include_str!("../../../runtime/marimo/requirements.in");
const REQUIREMENTS_TXT: &str = include_str!("../../../runtime/marimo/requirements.txt");
const CLOUD_SETUP: &str = include_str!("../../../scripts/cloud-setup.sh");
const PINNED_ELSEWHERE: &[(&str, &str)] = &[
    ("crates/endeavor-mcp/tests/e2e_marimo.rs", include_str!("e2e_marimo.rs")),
    ("smoke/tasks/P2-fix-python/project/analysis.py", include_str!("../../../smoke/tasks/P2-fix-python/project/analysis.py")),
    ("smoke/tasks/P3-preview-python/project/old.py", include_str!("../../../smoke/tasks/P3-preview-python/project/old.py")),
    ("smoke/tasks/P4-heading-python/project/report.py", include_str!("../../../smoke/tasks/P4-heading-python/project/report.py")),
];

fn pinned(requirements: &str) -> &str {
    requirements.lines().find_map(|l| l.strip_prefix("marimo==")).expect("marimo is pinned")
}

fn pins(text: &str) -> Vec<&str> {
    text.match_indices("marimo==")
        .map(|(at, pin)| {
            let rest = &text[at + pin.len()..];
            &rest[..rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len())]
        })
        .filter(|version| !version.is_empty())
        .collect()
}

#[test]
fn marimo_moves_together_in_the_adapters_requirements_the_cloud_setup_and_the_tests_notebooks() {
    let version = pinned(REQUIREMENTS_IN);
    assert_eq!(pinned(REQUIREMENTS_TXT), version, "runtime/marimo/requirements.txt: run uv pip compile again");
    assert!(CLOUD_SETUP.lines().any(|l| l == format!("MARIMO_VERSION={version}")), "scripts/cloud-setup.sh should have MARIMO_VERSION={version}");
    for (path, text) in PINNED_ELSEWHERE {
        let found = pins(text);
        assert!(!found.is_empty(), "{path} should pin marimo");
        assert!(found.iter().all(|pin| *pin == version), "{path} pins marimo at {found:?}, not {version}");
    }
}

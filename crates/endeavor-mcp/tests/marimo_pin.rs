const REQUIREMENTS_IN: &str = include_str!("../../../runtime/marimo/requirements.in");
const REQUIREMENTS_TXT: &str = include_str!("../../../runtime/marimo/requirements.txt");
const CLOUD_SETUP: &str = include_str!("../../../scripts/cloud-setup.sh");

fn pinned(requirements: &str) -> &str {
    requirements.lines().find_map(|l| l.strip_prefix("marimo==")).expect("marimo is pinned")
}

#[test]
fn marimo_moves_together_in_the_adapters_requirements_and_the_cloud_setup() {
    let version = pinned(REQUIREMENTS_IN);
    assert_eq!(pinned(REQUIREMENTS_TXT), version, "runtime/marimo/requirements.txt: run uv pip compile again");
    assert!(CLOUD_SETUP.lines().any(|l| l == format!("MARIMO_VERSION={version}")), "scripts/cloud-setup.sh should have MARIMO_VERSION={version}");
}

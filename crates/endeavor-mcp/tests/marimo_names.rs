use std::io::Write;
use std::process::{Command, Stdio};

const ADAPTER: &str = include_str!("../../../runtime/marimo/adapter.py");

fn near_misses(names: &[&str]) -> Option<Vec<String>> {
    let start = ADAPTER.find("# Import names of widely used packages").unwrap();
    let end = ADAPTER.find("def pypi_name(").unwrap();
    let script = format!(
        "import re, sys\n{}\nfor n in sys.stdin.read().split():\n    print(near_miss(n) or '-')\n",
        &ADAPTER[start..end]
    );
    let works = Command::new("python3").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if !works {
        return None;
    }
    let mut python = Command::new("python3").args(["-I", "-c", &script]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().ok()?;
    python.stdin.take().unwrap().write_all(names.join("\n").as_bytes()).unwrap();
    let out = python.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    Some(String::from_utf8(out.stdout).unwrap().lines().map(str::to_owned).collect())
}

#[test]
fn a_typo_of_a_widely_used_package_gets_a_hint_and_a_real_package_close_to_one_doesnt() {
    let typos = [("pandsa", "pandas"), ("numpi", "numpy"), ("reqeusts", "requests"), ("scikitlearn", "scikit-learn"), ("matplotllib", "matplotlib"), ("tensorflwo", "tensorflow"), ("Pandas", "pandas")];
    let real = ["pysam", "boto", "pymc3", "nilearn", "flax", "dipy", "dataset", "moto", "cytoolz", "stumpy", "polars", "plotly"];
    let names: Vec<&str> = typos.iter().map(|(t, _)| *t).chain(real).collect();
    let Some(found) = near_misses(&names) else {
        eprintln!("no python3: skipped");
        return;
    };
    for ((typo, close), got) in typos.iter().zip(&found) {
        assert_eq!(got, close, "{typo}");
    }
    for (name, got) in real.iter().zip(&found[typos.len()..]) {
        assert_eq!(got, "-", "{name} is a real package");
    }
}

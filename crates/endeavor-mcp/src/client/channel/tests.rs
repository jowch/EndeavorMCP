use super::*;

#[test]
fn says_plainly_why_julia_stopped() {
    assert_eq!(died_reason("exited", &[]), "");
    assert_eq!(died_reason("exit status: 3", &[]), "It exited with code 3.");
    assert_eq!(died_reason("signal: 9 (SIGKILL)", &[]), "It was killed (signal 9 (SIGKILL)), perhaps for using too much memory.");
    assert_eq!(died_reason("Its Slurm job reached its time limit.", &[]), "Its Slurm job reached its time limit.");
    assert!(died_reason("exited", &["IOError: listen: address already in use (EADDRINUSE)".into()]).contains("port"));
}

#[test]
fn diagnoses_common_failures_or_shows_the_log() {
    let lines = |s: &str| s.lines().map(String::from).collect::<Vec<_>>();
    assert!(diagnose(&lines("ERROR: Could not resolve host: github.com")).contains("internet"));
    assert!(diagnose(&lines("ERROR: Unsatisfiable requirements detected")).contains("conflict"));
    assert!(diagnose(&lines("IOError: listen: address already in use (EADDRINUSE)")).contains("port"));
    let other = diagnose(&lines("ERROR: LoadError: boom\nStacktrace: …"));
    assert!(other.starts_with("Last output:") && other.contains("boom"));
    assert_eq!(diagnose(&[]), "It printed nothing.");
}

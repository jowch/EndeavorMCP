//! The askpass mode over loopback TCP, as ssh runs it: the `endeavor` binary
//! with the prompt as its only argument, `ENDEAVOR_ASKPASS_ADDRESS` and
//! `ENDEAVOR_ASKPASS_TOKEN` set, answered by `client::Asker`. Every platform:
//! it is the only way on Windows.

use std::path::Path;
use std::process::{Command, Output};

use endeavor_mcp::client::Asker;
use wire::askpass::{ADDRESS_ENV, SOCKET_ENV, TOKEN_ENV};

/// The binary as ssh runs it, with `env` and nothing else of askpass's.
fn askpass(prompt: &str, env: &[(String, String)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_endeavor"))
        .arg(prompt)
        .env_remove(SOCKET_ENV)
        .env_remove("SSH_ASKPASS_PROMPT")
        .envs(env.iter().filter(|(k, _)| k == ADDRESS_ENV || k == TOKEN_ENV).map(|(k, v)| (k, v)))
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn an_answer_is_printed_for_ssh() {
    let asker = Asker::start(|ask| Some(format!("answer to {}", ask.prompt))).unwrap();
    let out = askpass("jc@lab's password: ", &asker.env(Path::new("endeavor")));
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout).trim_end_matches(['\r', '\n']), "answer to jc@lab's password:");
}

#[test]
fn a_cancel_exits_1_with_nothing_printed() {
    let asker = Asker::start(|_| None).unwrap();
    let out = askpass("jc@lab's password: ", &asker.env(Path::new("endeavor")));
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}

#[test]
fn the_wrong_token_gets_no_answer() {
    let asker = Asker::start(|_| Some("never".into())).unwrap();
    let mut env = asker.env(Path::new("endeavor"));
    env.iter_mut().filter(|(k, _)| k == TOKEN_ENV).for_each(|(_, v)| *v = "0".repeat(32));
    let out = askpass("jc@lab's password: ", &env);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(text(&out.stderr).contains("without answering"), "{}", text(&out.stderr));
}

#[test]
fn an_address_off_this_computer_is_refused_before_connecting() {
    let env = [(ADDRESS_ENV.to_owned(), "10.0.0.1:1".to_owned()), (TOKEN_ENV.to_owned(), "t".to_owned())];
    let out = askpass("jc@lab's password: ", &env);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("isn't a loopback address"), "{}", text(&out.stderr));
}

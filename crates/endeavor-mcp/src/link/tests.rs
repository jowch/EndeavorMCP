use super::*;

#[test]
fn ids_that_cannot_be_folders_on_every_system_are_refused() {
    for good in ["server-18f3a9c2b", "lab", "a.b", "com10", "console", "lab_2"] {
        assert!(valid_id(good).is_ok(), "{good}");
    }
    for bad in ["", ".hidden", "lab.", "..", "a/b", "a b", "Lab", "LAB", "con", "nul", "aux", "prn", "com1", "lpt9", "nul.txt", "com3.x", "é"] {
        assert!(valid_id(bad).is_err(), "{bad:?}");
    }
    assert!(valid_id("Lab").unwrap_err().contains("lower-case letters"));
}

#[test]
fn a_record_with_no_protocol_number_reads_as_0() {
    let old = r#"{"machine": "lab", "pid": 7, "port": 1, "token": "t", "build": "b"}"#;
    assert_eq!(serde_json::from_str::<Record>(old).unwrap().protocol, 0);
}

#[test]
fn a_status_from_a_newer_link_still_reads() {
    let newer = r#"{
        "machine": "lab", "name": "lab", "state": "hibernating", "pid": 7, "build": "newer",
        "something_new": {"a": 1},
        "hello": {"node": "n", "something_new": true},
        "job": {"id": "9", "something_new": 1},
        "needs_install": {"what": "firmware", "helper": {"os": "Linux", "arch": "x86_64", "folder": "/f", "running": {"unit": "x"}, "something_new": 1}}
    }"#;
    let status: Status = serde_json::from_str(newer).expect("unknown state, kinds and fields read");
    assert_eq!(status.state, State::Unknown);
    assert_eq!(status.needs_install.as_ref().map(|n| n.what), Some(InstallWhat::Unknown));
    let helper = status.needs_install.unwrap().helper.unwrap();
    assert_eq!((helper.bytes, helper.running), (None, None), "a size and a runtime of a shape this build doesn't know are left out");
    assert_eq!(status.hello.map(|h| (h.node, h.slurm)), Some(("n".to_owned(), false)), "what is missing has its default");
    assert_eq!(status.job.map(|j| j.id), Some("9".to_owned()));
    assert_eq!(status.protocol, 0, "a status with no protocol number is older than any");
    // The words of the states this build knows are the ones it always had.
    for (word, state) in [("connecting", State::Connecting), ("needs_install", State::NeedsInstall), ("ready", State::Ready)] {
        assert_eq!(serde_json::from_str::<State>(&format!("\"{word}\"")).unwrap(), state);
    }
}

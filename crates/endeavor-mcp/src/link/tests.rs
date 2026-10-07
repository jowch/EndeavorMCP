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
fn a_status_from_a_link_of_another_protocol_still_reads() {
    let newer = r#"{
        "machine": "lab", "name": "lab", "state": "hibernating", "pid": 7, "build": "newer",
        "something_new": {"a": 1},
        "hello": {"node": "n", "something_new": true},
        "job": {"id": "9", "something_new": 1},
        "needs_install": {"items": [{"kind": "firmware", "name": "Firmware 2", "size_mb": null, "place": null, "something_new": 1}], "helper": {"os": "Linux", "arch": "x86_64", "folder": "/f", "something_new": 1}}
    }"#;
    let status: Status = serde_json::from_str(newer).expect("unknown state, kinds and fields read");
    assert_eq!(status.state, State::Unknown);
    let needs = status.needs_install.unwrap();
    assert_eq!(needs.items.iter().map(|i| i.kind.as_str()).collect::<Vec<_>>(), ["firmware"], "a kind this build doesn't know reads");
    assert!(!needs.needs_helper());
    let helper = needs.helper.unwrap();
    assert_eq!((helper.bytes, helper.running), (None, None), "what is missing has its default");
    assert_eq!(status.hello.map(|h| (h.node, h.slurm)), Some(("n".to_owned(), false)), "what is missing has its default");
    assert_eq!(status.job.map(|j| j.id), Some("9".to_owned()));
    // The words of the states this build knows are the ones it always had.
    for (word, state) in [("connecting", State::Connecting), ("needs_install", State::NeedsInstall), ("ready", State::Ready)] {
        assert_eq!(serde_json::from_str::<State>(&format!("\"{word}\"")).unwrap(), state);
    }
}

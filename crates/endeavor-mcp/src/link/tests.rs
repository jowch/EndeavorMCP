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
        "needs_install": {"items": [{"kind": "firmware", "name": "Firmware 2", "size_mb": null, "place": null, "something_new": 1}], "helper": {"os": "Linux", "arch": "x86_64", "folder": "/f", "running": {"unit": "x"}, "something_new": 1}}
    }"#;
    let status = Status::read(serde_json::from_str(newer).unwrap(), PROTOCOL + 1).expect("unknown state, kinds and fields read");
    assert_eq!(status.state, State::Unknown);
    assert!(status.needs_install.is_none(), "a field in a shape this build can't read is dropped, not an error");
    let kinds = r#"{"items": [{"kind": "firmware", "name": "Firmware 2", "size_mb": null, "place": null, "something_new": 1}], "helper": {"os": "Linux", "arch": "x86_64", "folder": "/f", "something_new": 1}}"#;
    let needs: InstallInfo = serde_json::from_str(kinds).expect("a kind this build doesn't know reads");
    assert_eq!(needs.items.iter().map(|i| i.kind.as_str()).collect::<Vec<_>>(), ["firmware"]);
    assert!(needs.needs_helper(), "it has what was found for the helper");
    let helper = needs.helper.unwrap();
    assert_eq!((helper.bytes, helper.running), (None, None), "what is missing has its default");
    assert_eq!(status.hello.map(|h| (h.node, h.slurm)), Some(("n".to_owned(), false)), "what is missing has its default");
    assert_eq!(status.job.map(|j| j.id), Some("9".to_owned()));
    // The words of the states this build knows are the ones it always had.
    for (word, state) in [("connecting", State::Connecting), ("needs_install", State::NeedsInstall), ("ready", State::Ready)] {
        assert_eq!(serde_json::from_str::<State>(&format!("\"{word}\"")).unwrap(), state);
    }
}

#[test]
fn a_status_of_the_previous_protocol_reads_as_far_as_it_is_needed() {
    let old = serde_json::json!({
        "machine": "lab", "name": "lab", "state": "needs_install", "pid": 7, "build": "older",
        "needs_install": {"what": "julia", "helper": null, "found": 3},
        "hello": {"node": "n", "found": {"julia": "1.10"}},
    });
    assert!(Status::read(old.clone(), PROTOCOL).is_err(), "this protocol's shape is strict");
    let status = Status::read(old, PROTOCOL - 1).expect("an older shape reads");
    assert_eq!((status.machine.as_str(), status.pid, status.state), ("lab", 7, State::NeedsInstall));
    assert!(status.runtime.is_none() && status.needs_install.is_none() && status.hello.is_none());

    let busy = serde_json::json!({"machine": "lab", "pid": 7, "state": "hibernating", "runtime": {"unit": "x"}});
    assert_eq!(Status::read(busy, 0).unwrap().state, State::Unknown);
    assert!(Status::read(serde_json::json!({"machine": "lab"}), 0).is_err());
}

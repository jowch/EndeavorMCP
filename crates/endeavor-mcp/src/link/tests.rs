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

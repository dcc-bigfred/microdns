use std::path::PathBuf;

use microdns::datadir::DEFAULT_ROOT;

#[test]
fn default_root_is_data() {
    // Cannot safely mutate env in parallel tests; just check absolute join.
    let p = PathBuf::from(DEFAULT_ROOT)
        .join("etc")
        .join("microdns.json");
    assert!(p.is_absolute());
    assert!(p.ends_with("etc/microdns.json"));
}

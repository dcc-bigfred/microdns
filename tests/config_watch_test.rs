use std::path::Path;

use microdns::config_watch::is_relevant_path;

#[test]
fn relevant_filters() {
    assert!(is_relevant_path(
        Path::new("/data/etc/microdns.json"),
        "microdns.json"
    ));
    assert!(!is_relevant_path(
        Path::new("/data/etc/.microdns.json"),
        "microdns.json"
    ));
    assert!(!is_relevant_path(
        Path::new("/data/etc/microdns.json~"),
        "microdns.json"
    ));
    assert!(!is_relevant_path(
        Path::new("/data/etc/other.json"),
        "microdns.json"
    ));
}

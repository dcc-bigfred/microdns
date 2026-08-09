use std::path::Path;

use microdns::version::{info, read_section_from};

#[test]
fn info_has_build_commit() {
    let i = info();
    assert!(!i.build_commit.is_empty());
    assert_eq!(i.version, "dev"); // no injected section in test binary
}

#[test]
fn non_elf_returns_none() {
    assert!(read_section_from(Path::new("/etc/hosts")).is_none());
}

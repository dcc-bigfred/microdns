use microdns::beacon::{serial_reply, virtual_serial, DEFAULT_VIRTUAL_SERIAL};

#[test]
fn default_serial_frame() {
    let frame = serial_reply(DEFAULT_VIRTUAL_SERIAL);
    // length=8, header=0x0010, serial LE
    assert_eq!(frame.len(), 8);
    assert_eq!(u16::from_le_bytes([frame[0], frame[1]]), 8);
    assert_eq!(u16::from_le_bytes([frame[2], frame[3]]), 0x0010);
    assert_eq!(
        u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]),
        258_000_000
    );
}

#[test]
fn virtual_serial_matches_go() {
    assert_eq!(virtual_serial(0, 0), 258_000_000);
    assert_eq!(virtual_serial(2, 1), 258_002_001);
    assert_eq!(virtual_serial(1, 2), 258_001_002);
}

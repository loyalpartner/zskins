use zbar::backend::sway::encode_message;

#[test]
fn encodes_subscribe_message_with_correct_header() {
    let payload = br#"["workspace"]"#;
    let buf = encode_message(2, payload);

    assert_eq!(&buf[0..6], b"i3-ipc");
    assert_eq!(u32::from_le_bytes(buf[6..10].try_into().unwrap()), payload.len() as u32);
    assert_eq!(u32::from_le_bytes(buf[10..14].try_into().unwrap()), 2);
    assert_eq!(&buf[14..], payload);
}

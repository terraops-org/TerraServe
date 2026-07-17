use terraserve::vector::mvt::wire::PbfWriter;

#[test]
fn varint_encodes_leb128() {
    let mut w = PbfWriter::new();
    w.varint(0);
    w.varint(1);
    w.varint(300); // 0xac 0x02
    assert_eq!(w.into_bytes(), vec![0x00, 0x01, 0xac, 0x02]);
}

#[test]
fn field_varint_tags_and_value() {
    let mut w = PbfWriter::new();
    // field 15 (Layer.version), wire type 0 (varint): tag = 15<<3|0 = 120 = 0x78; value 2.
    w.field_varint(15, 2);
    assert_eq!(w.into_bytes(), vec![0x78, 0x02]);
}

#[test]
fn field_bytes_length_delimited() {
    let mut w = PbfWriter::new();
    // field 1 (Layer.name), wire type 2: tag = 1<<3|2 = 10 = 0x0a; len 3; "abc".
    w.field_bytes(1, b"abc");
    assert_eq!(w.into_bytes(), vec![0x0a, 0x03, b'a', b'b', b'c']);
}

#[test]
fn field_double_le() {
    let mut w = PbfWriter::new();
    // field 3, wire type 1 (64-bit): tag = 3<<3|1 = 25 = 0x19; 8 bytes LE.
    w.field_double(3, 1.5);
    let mut want = vec![0x19];
    want.extend_from_slice(&1.5f64.to_le_bytes());
    assert_eq!(w.into_bytes(), want);
}

#[test]
fn packed_u32_is_length_delimited_varints() {
    let mut w = PbfWriter::new();
    // field 4 (Feature.geometry), packed uint32: tag 0x22, then len, then varints.
    w.field_packed_u32(4, &[9, 300]); // 9 -> [0x09]; 300 -> [0xac,0x02]; len 3.
    assert_eq!(w.into_bytes(), vec![0x22, 0x03, 0x09, 0xac, 0x02]);
}

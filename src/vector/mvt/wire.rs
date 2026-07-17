// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! The MVT wire layer: a minimal, bespoke, WRITE-ONLY protobuf encoder (no external crate).
//! Only the field kinds the MVT schema uses: varint, length-delimited (bytes/string/embedded
//! message), 64-bit double, and packed repeated uint32 (geometry command arrays).

#[derive(Default)]
pub struct PbfWriter {
    buf: Vec<u8>,
}

impl PbfWriter {
    pub fn new() -> Self {
        PbfWriter { buf: Vec::new() }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// LEB128 base-128 varint.
    pub fn varint(&mut self, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(byte);
                return;
            }
            self.buf.push(byte | 0x80);
        }
    }

    /// A field tag: `(field_number << 3) | wire_type`.
    fn tag(&mut self, field: u32, wire: u32) {
        self.varint(((field << 3) | wire) as u64);
    }

    /// Wire type 0 — varint value.
    pub fn field_varint(&mut self, field: u32, v: u64) {
        self.tag(field, 0);
        self.varint(v);
    }

    /// Wire type 2 — length-delimited bytes/string/embedded-message.
    pub fn field_bytes(&mut self, field: u32, bytes: &[u8]) {
        self.tag(field, 2);
        self.varint(bytes.len() as u64);
        self.buf.extend_from_slice(bytes);
    }

    /// Wire type 1 — 64-bit little-endian double.
    pub fn field_double(&mut self, field: u32, v: f64) {
        self.tag(field, 1);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Packed repeated uint32 (a length-delimited run of concatenated varints) — MVT geometry.
    pub fn field_packed_u32(&mut self, field: u32, vals: &[u32]) {
        let mut inner = PbfWriter::new();
        for &v in vals {
            inner.varint(v as u64);
        }
        self.field_bytes(field, &inner.into_bytes());
    }
}

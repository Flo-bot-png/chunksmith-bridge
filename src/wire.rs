//! Java `DataOutputStream` byte order (big-endian, `writeUTF` strings) and Chunksmith's framing:
//! every message on `chunksmith:lod` is a VarInt length followed by the body (Fabric's
//! `writeByteArray`, which the Paper plugin copies so both kinds of server talk the same).

pub struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    pub fn left(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    pub fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    pub fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    pub fn bool(&mut self) -> Option<bool> {
        Some(self.u8()? != 0)
    }

    pub fn u16(&mut self) -> Option<u16> {
        let b = self.take(2)?;
        Some(u16::from_be_bytes([b[0], b[1]]))
    }

    pub fn i32(&mut self) -> Option<i32> {
        let b = self.take(4)?;
        Some(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn i64(&mut self) -> Option<i64> {
        let b = self.take(8)?;
        Some(i64::from_be_bytes(b.try_into().ok()?))
    }

    /// `DataInput.readUTF`: a u16 byte length, then (modified) UTF-8. Block and biome names are
    /// plain ASCII, so ordinary UTF-8 decoding is enough.
    pub fn utf(&mut self) -> Option<String> {
        let len = usize::from(self.u16()?);
        Some(String::from_utf8_lossy(self.take(len)?).into_owned())
    }

    pub fn varint(&mut self) -> Option<i32> {
        let mut value: u32 = 0;
        for shift in (0..35).step_by(7) {
            let b = self.u8()?;
            value |= u32::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(value as i32);
            }
        }
        None
    }
}

#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }

    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(u8::from(v))
    }

    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn utf(&mut self, v: &str) -> &mut Self {
        let bytes = v.as_bytes();
        let len = bytes.len().min(u16::MAX as usize);
        self.0.extend_from_slice(&(len as u16).to_be_bytes());
        self.0.extend_from_slice(&bytes[..len]);
        self
    }
}

pub fn write_varint(out: &mut Vec<u8>, value: u32) {
    let mut v = value;
    while v & !0x7f != 0 {
        out.push((v & 0x7f) as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Length prefix for a message body.
pub fn frame(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    write_varint(&mut out, body.len() as u32);
    out.extend_from_slice(body);
    out
}

/// The body of a framed message (a message without the prefix, from an older server, is taken
/// as it is).
pub fn unframe(data: &[u8]) -> &[u8] {
    let mut r = Reader::new(data);
    match r.varint() {
        Some(len) if len >= 0 && len as usize == r.left() => &data[data.len() - len as usize..],
        _ => data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let body = vec![7u8; 300];
        let framed = frame(&body);
        assert_eq!(&framed[..2], &[0xac, 0x02]);
        assert_eq!(unframe(&framed), &body[..]);
    }

    #[test]
    fn utf_reads_java_strings() {
        let mut w = Writer::default();
        w.utf("minecraft:stone").i32(-5);
        let mut r = Reader::new(&w.0);
        assert_eq!(r.utf().as_deref(), Some("minecraft:stone"));
        assert_eq!(r.i32(), Some(-5));
    }
}

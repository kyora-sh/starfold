//! MSB-first bit writing for the headers the encoder packs itself.

#[derive(Default)]
pub struct Bits {
    buf: Vec<u8>,
    len: usize,
}

impl Bits {
    pub fn new() -> Self {
        Bits::default()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn put(&mut self, n: u32, v: u64) {
        for i in (0..n).rev() {
            if self.len.is_multiple_of(8) {
                self.buf.push(0);
            }
            if (v >> i) & 1 == 1 {
                *self.buf.last_mut().unwrap() |= 0x80 >> (self.len % 8);
            }
            self.len += 1;
        }
    }

    pub fn flag(&mut self, b: bool) {
        self.put(1, b as u64);
    }

    pub fn ue(&mut self, v: u32) {
        let v = v as u64 + 1;
        let n = 64 - v.leading_zeros();
        self.put(n - 1, 0);
        self.put(n, v);
    }

    pub fn se(&mut self, v: i32) {
        self.ue(if v > 0 {
            (2 * v - 1) as u32
        } else {
            (-2 * v as i64) as u32
        });
    }

    /// A one, then zeros up to the byte boundary.
    pub fn trailing(&mut self) {
        self.put(1, 1);
        self.align(0);
    }

    pub fn align(&mut self, bit: u64) {
        while !self.len.is_multiple_of(8) {
            self.put(1, bit);
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// An RBSP as an Annex-B NAL unit: start code, the header bytes, emulation prevention.
pub fn nal(header: &[u8], rbsp: &[u8]) -> Vec<u8> {
    let mut out = vec![0, 0, 0, 1];
    out.extend_from_slice(header);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// `v` as leb128 in exactly `n` bytes.
pub fn leb128_fixed(v: u32, n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| {
            let b = ((v >> (7 * i)) & 0x7f) as u8;
            if i + 1 < n {
                b | 0x80
            } else {
                b
            }
        })
        .collect()
}

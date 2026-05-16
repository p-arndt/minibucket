// MD5 implementation (RFC 1321). Used only for S3 ETag values.

const S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22,
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20,
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

#[derive(Clone)]
pub struct Md5 {
    a: u32, b: u32, c: u32, d: u32,
    buf: [u8; 64],
    len: usize,
    total: u64,
}

impl Md5 {
    pub fn new() -> Self {
        Self {
            a: 0x67452301, b: 0xefcdab89, c: 0x98badcfe, d: 0x10325476,
            buf: [0; 64], len: 0, total: 0,
        }
    }
    pub fn update(&mut self, data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        let mut i = 0;
        if self.len > 0 {
            let need = 64 - self.len;
            let take = need.min(data.len());
            self.buf[self.len..self.len + take].copy_from_slice(&data[..take]);
            self.len += take;
            i = take;
            if self.len == 64 {
                let blk = self.buf;
                self.process(&blk);
                self.len = 0;
            }
        }
        while data.len() - i >= 64 {
            let mut blk = [0u8; 64];
            blk.copy_from_slice(&data[i..i + 64]);
            self.process(&blk);
            i += 64;
        }
        if i < data.len() {
            let rem = data.len() - i;
            self.buf[..rem].copy_from_slice(&data[i..]);
            self.len = rem;
        }
    }
    fn process(&mut self, blk: &[u8; 64]) {
        let mut m = [0u32; 16];
        for i in 0..16 {
            m[i] = u32::from_le_bytes([blk[i * 4], blk[i * 4 + 1], blk[i * 4 + 2], blk[i * 4 + 3]]);
        }
        let mut a = self.a;
        let mut b = self.b;
        let mut c = self.c;
        let mut d = self.d;
        for i in 0..64 {
            let (f, g) = if i < 16 {
                ((b & c) | ((!b) & d), i)
            } else if i < 32 {
                ((d & b) | ((!d) & c), (5 * i + 1) % 16)
            } else if i < 48 {
                (b ^ c ^ d, (3 * i + 5) % 16)
            } else {
                (c ^ (b | (!d)), (7 * i) % 16)
            };
            let t = a
                .wrapping_add(f)
                .wrapping_add(K[i])
                .wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(t.rotate_left(S[i]));
        }
        self.a = self.a.wrapping_add(a);
        self.b = self.b.wrapping_add(b);
        self.c = self.c.wrapping_add(c);
        self.d = self.d.wrapping_add(d);
    }
    pub fn finalize(mut self) -> [u8; 16] {
        let bits = self.total.wrapping_mul(8);
        self.buf[self.len] = 0x80;
        self.len += 1;
        if self.len > 56 {
            for i in self.len..64 { self.buf[i] = 0; }
            let blk = self.buf;
            self.process(&blk);
            self.len = 0;
        }
        for i in self.len..56 { self.buf[i] = 0; }
        self.buf[56..64].copy_from_slice(&bits.to_le_bytes());
        let blk = self.buf;
        self.process(&blk);
        let mut out = [0u8; 16];
        out[0..4].copy_from_slice(&self.a.to_le_bytes());
        out[4..8].copy_from_slice(&self.b.to_le_bytes());
        out[8..12].copy_from_slice(&self.c.to_le_bytes());
        out[12..16].copy_from_slice(&self.d.to_le_bytes());
        out
    }
}

pub fn md5(data: &[u8]) -> [u8; 16] {
    let mut h = Md5::new();
    h.update(data);
    h.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha256::hex;
    #[test]
    fn empty() {
        assert_eq!(hex(&md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
    }
    #[test]
    fn abc() {
        assert_eq!(hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
    }
    #[test]
    fn quick() {
        assert_eq!(
            hex(&md5(b"The quick brown fox jumps over the lazy dog")),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }
}

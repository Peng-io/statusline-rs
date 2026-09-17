//! 极简 zlib 解压（RFC 1950/1951）：为读 git 对象（取附注 tag 对象首行的目标
//! commit）而写。覆盖 stored / 固定 Huffman / 动态 Huffman 三种块类型，不处理
//! FDICT 预置字典（git 从不使用）。输出达到 `max_out` 时提前返回已解出的部分——
//! 对象首行（47 字节）远在上限之内，足够调用方使用；完整解出时校验 adler32。
//!
//! 解码流程沿 zlib 参考实现 puff.c 的规范做法：码长计数 → 首次出现位置 →
//! 按码字序线性查表。

/// 解压 zlib 流（含 2 字节头）。数据非法或截断返回 None。
pub fn zlib_decompress(data: &[u8], max_out: usize) -> Option<Vec<u8>> {
    if data.len() < 2 {
        return None;
    }
    // CM 必须为 8（deflate）；FDICT 置位表示流带预置字典，无法独立解码
    if data[0] & 0x0f != 8 || data[1] & 0x20 != 0 {
        return None;
    }
    let mut bits = Bits::new(&data[2..]);
    let mut out = Vec::new();

    loop {
        let bfinal = bits.get(1)?;
        let flow = match bits.get(2)? {
            0 => stored(&mut bits, &mut out, max_out)?,
            1 => {
                let (lit, dist) = fixed_tables();
                decode_block(&mut bits, &lit, &dist, &mut out, max_out)?
            }
            2 => {
                let (lit, dist) = dynamic_tables(&mut bits)?;
                decode_block(&mut bits, &lit, &dist, &mut out, max_out)?
            }
            _ => return None,
        };
        if matches!(flow, Flow::Capped) {
            return Some(out);
        }
        if bfinal == 1 {
            break;
        }
    }

    // 完整解出后校验 adler32；尾部被截断缺 4 字节校验和时宽容放行
    bits.align();
    if let Some(tail) = bits.take_bytes(4) {
        let expect = u32::from_be_bytes(tail.try_into().ok()?);
        if adler32(&out) != expect {
            return None;
        }
    }
    Some(out)
}

/// 块解码结果：遇到块尾自然结束，或输出达到上限提前收尾
enum Flow {
    Continue,
    Capped,
}

/// stored 块：头部之后对齐到字节边界，校验 LEN/NLEN 互补后原样拷贝
fn stored(bits: &mut Bits, out: &mut Vec<u8>, max_out: usize) -> Option<Flow> {
    bits.align();
    let len = bits.get(16)? as usize;
    let nlen = bits.get(16)? as usize;
    if len != !nlen & 0xffff {
        return None;
    }
    let take = len.min(max_out.saturating_sub(out.len()));
    out.extend_from_slice(bits.take_bytes(take)?);
    if take < len {
        return Some(Flow::Capped);
    }
    Some(Flow::Continue)
}

/// Huffman 块：字面量直写；长度码从已解出的历史里做距离拷贝（LZ77）
fn decode_block(
    bits: &mut Bits,
    lit: &Huffman,
    dist: &Huffman,
    out: &mut Vec<u8>,
    max_out: usize,
) -> Option<Flow> {
    loop {
        let sym = lit.decode(bits)?;
        match sym {
            0..=255 => out.push(sym as u8),
            256 => return Some(Flow::Continue),
            257..=285 => {
                let idx = (sym - 257) as usize;
                let len = LEN_BASE[idx] as usize + bits.get(LEN_EXT[idx] as u32)? as usize;
                let dsym = dist.decode(bits)? as usize;
                if dsym >= DIST_BASE.len() {
                    return None;
                }
                let d = DIST_BASE[dsym] as usize + bits.get(DIST_EXT[dsym] as u32)? as usize;
                if d > out.len() {
                    return None;
                }
                for _ in 0..len {
                    out.push(out[out.len() - d]);
                    if out.len() >= max_out {
                        return Some(Flow::Capped);
                    }
                }
            }
            _ => return None,
        }
        if out.len() >= max_out {
            return Some(Flow::Capped);
        }
    }
}

/// 长度码 257-285、距离码 0-29 的基准值与额外位数（RFC 1951 3.2.5）
const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXT: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXT: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// 固定 Huffman 表（RFC 1951 3.2.6）：
/// 0-143 → 8 位，144-255 → 9 位，256-279 → 7 位，280-287 → 8 位；距离码统一 5 位
fn fixed_tables() -> (Huffman, Huffman) {
    let mut lit_lengths = [0u8; 288];
    for (sym, len) in lit_lengths.iter_mut().enumerate() {
        *len = match sym {
            0..=143 | 280..=287 => 8,
            144..=255 => 9,
            _ => 7,
        };
    }
    let lit = Huffman::build(&lit_lengths).expect("固定表必然合法");
    let dist = Huffman::build(&[5u8; 30]).expect("固定表必然合法");
    (lit, dist)
}

/// 动态 Huffman 表：先读码长码表，再用它展开字面量/长度表与距离表的码长
fn dynamic_tables(bits: &mut Bits) -> Option<(Huffman, Huffman)> {
    // 码长码自身按此顺序存放（RFC 1951 3.2.7）
    const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
    let hlit = bits.get(5)? as usize + 257;
    let hdist = bits.get(5)? as usize + 1;
    let hclen = bits.get(4)? as usize + 4;
    let mut cl_lengths = [0u8; 19];
    for &i in ORDER.iter().take(hclen) {
        cl_lengths[i] = bits.get(3)? as u8;
    }
    let cl = Huffman::build(&cl_lengths)?;

    let mut lengths = vec![0u8; hlit + hdist];
    let mut i = 0;
    while i < lengths.len() {
        let sym = cl.decode(bits)?;
        let (value, repeat) = match sym {
            0..=15 => (sym as u8, 1),
            16 => (*lengths.get(i.checked_sub(1)?)?, bits.get(2)? as usize + 3),
            17 => (0, bits.get(3)? as usize + 3),
            18 => (0, bits.get(7)? as usize + 11),
            _ => return None,
        };
        if i + repeat > lengths.len() {
            return None;
        }
        lengths[i..i + repeat].fill(value);
        i += repeat;
    }

    Some((
        Huffman::build(&lengths[..hlit])?,
        Huffman::build(&lengths[hlit..])?,
    ))
}

/// 规范 Huffman 表：count[len] 为码长为 len 的符号个数；symbol 按码字序排列
struct Huffman {
    count: [u16; 16],
    symbol: Vec<u16>,
}

impl Huffman {
    fn build(lengths: &[u8]) -> Option<Huffman> {
        let mut count = [0u16; 16];
        for &len in lengths {
            count[len as usize] += 1;
        }
        // 过订阅的码长集合非法；欠订阅不拦截——动态表允许只用一部分码字，
        // 命中缺失码字时 decode 自然返回 None
        let mut left = 1i32;
        for len in 1..16 {
            left <<= 1;
            left -= count[len] as i32;
            if left < 0 {
                return None;
            }
        }
        let mut offs = [0u16; 16];
        for len in 1..15 {
            offs[len + 1] = offs[len] + count[len];
        }
        let mut symbol = vec![0u16; lengths.iter().filter(|&&len| len != 0).count()];
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                symbol[offs[len as usize] as usize] = sym as u16;
                offs[len as usize] += 1;
            }
        }
        Some(Huffman { count, symbol })
    }

    fn decode(&self, bits: &mut Bits) -> Option<u16> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..16 {
            code |= bits.get(1)? as i32;
            let count = self.count[len] as i32;
            if code - first < count {
                return Some(self.symbol[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        None
    }
}

/// 位读取器：DEFLATE 按字节小端存放、位低位在前
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Bits<'a> {
        Bits { data, pos: 0 }
    }

    fn get(&mut self, n: u32) -> Option<u32> {
        let mut value = 0u32;
        for i in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            value |= (((byte >> (self.pos % 8)) & 1) as u32) << i;
            self.pos += 1;
        }
        Some(value)
    }

    /// 跳过填充位对齐到字节边界（stored 块开头与 adler32 之前）
    fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }

    fn take_bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        debug_assert_eq!(self.pos % 8, 0);
        let start = self.pos / 8;
        let slice = self.data.get(start..start.checked_add(n)?)?;
        self.pos = (start + n) * 8;
        Some(slice)
    }
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::zlib_decompress;

    // 向量由 target/gen-vectors.js 用 node zlib 生成（target 不随仓库分发）
    const REAL: &[u8] = &[120,1,13,140,65,10,195,32,16,0,123,246,21,123,47,136,70,99,54,80,74,75,15,125,135,110,54,198,196,52,37,145,66,251,250,122,26,24,152,41,62,130,182,253,105,11,51,83,1,107,201,40,203,132,118,104,199,48,96,139,72,218,143,24,168,211,65,243,16,122,231,140,65,37,202,247,205,64,219,186,166,34,74,125,124,26,169,100,213,62,70,222,225,49,37,90,248,149,103,184,84,200,60,203,233,119,139,171,79,89,214,230,10,186,67,227,218,166,119,29,156,21,42,37,68,205,97,76,251,81,224,121,135,157,51,251,131,197,31,139,161,46,180];
    const L0: &[u8] = &[120,1,1,123,0,132,255,111,98,106,101,99,116,32,49,98,56,101,51,100,99,100,50,51,51,56,53,53,48,97,101,53,49,57,56,98,100,98,50,97,55,98,97,101,53,54,100,102,53,98,102,50,101,48,10,116,121,112,101,32,116,97,103,10,116,97,103,32,118,50,46,48,46,51,10,116,97,103,103,101,114,32,83,111,109,101,111,110,101,32,60,97,64,98,46,99,62,32,49,55,53,53,48,48,48,48,48,48,32,43,48,56,48,48,10,10,114,101,108,101,97,115,101,32,50,46,48,46,51,10,247,212,35,18];
    const L1: &[u8] = &[120,1,37,203,65,10,194,48,16,70,225,125,78,49,123,33,76,18,198,70,16,241,14,158,96,38,249,27,16,53,210,6,193,219,139,237,91,125,155,215,237,142,50,40,88,70,170,165,198,148,178,8,43,36,156,178,85,139,58,153,66,142,117,22,155,35,216,141,239,27,52,180,185,161,141,62,209,179,79,127,54,44,116,235,79,244,23,232,172,87,243,229,66,97,18,225,45,58,112,102,118,110,193,3,186,130,246,237,7,247,212,35,18];
    const L6: &[u8] = &[120,156,37,203,65,10,194,48,16,70,225,125,78,49,123,33,76,18,198,70,16,241,14,158,96,38,249,27,16,53,210,6,193,219,139,237,91,125,155,215,237,142,50,40,88,70,170,165,198,148,178,8,43,36,156,178,85,139,58,153,66,142,117,22,155,35,216,141,239,27,52,180,185,161,141,62,209,179,79,127,54,44,116,235,79,244,23,232,172,87,243,229,66,97,18,225,45,58,112,102,118,110,193,3,186,130,246,237,7,247,212,35,18];
    const L9: &[u8] = &[120,218,37,203,65,10,194,48,16,70,225,125,78,49,123,33,76,18,198,70,16,241,14,158,96,38,249,27,16,53,210,6,193,219,139,237,91,125,155,215,237,142,50,40,88,70,170,165,198,148,178,8,43,36,156,178,85,139,58,153,66,142,117,22,155,35,216,141,239,27,52,180,185,161,141,62,209,179,79,127,54,44,116,235,79,244,23,232,172,87,243,229,66,97,18,225,45,58,112,102,118,110,193,3,186,130,246,237,7,247,212,35,18];
    const BIG_Z: &[u8] = &[120,218,203,79,202,74,77,46,81,72,36,18,112,149,84,22,164,42,148,36,166,115,21,165,22,164,38,150,40,228,100,230,165,42,140,178,135,31,155,11,0,136,68,197,100];
    const TEXT: &str = "object 1b8e3dcd2338550ae5198bdb2a7bae56df5bf2e0\ntype tag\ntag v2.0.3\ntagger Someone <a@b.c> 1755000000 +0800\n\nrelease 2.0.3\n";
    const BIG_TEXT: &str = "object aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\ntype tag\nrepeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line repeat line \n";

    /// 真实数据：agentscope-java 仓库 v2.0.0 附注 tag 的松散对象文件，
    /// 正文含 git 对象头 `tag <大小>\0`，其后才是 `object <sha>` 行
    #[test]
    fn real_loose_tag_object() {
        let out = zlib_decompress(REAL, 4096).unwrap();
        assert_eq!(out.len(), 157);
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("tag 149\0object 44c304ec84d5fbd8588c1af8bc71b1edb9663380\n"));
        assert!(text.contains("\ntype commit\n"));
    }

    /// stored / 各级别动态与固定 Huffman 均还原原文，顺带校验 adler32
    #[test]
    fn all_levels_roundtrip() {
        for compressed in [L0, L1, L6, L9] {
            assert_eq!(zlib_decompress(compressed, 4096).unwrap(), TEXT.as_bytes());
        }
    }

    /// 高重复内容逼出长度/距离拷贝路径
    #[test]
    fn distance_copies() {
        assert_eq!(zlib_decompress(BIG_Z, 4096).unwrap(), BIG_TEXT.as_bytes());
    }

    /// 达到输出上限时返回已解出的前缀（调用方只读首行）
    #[test]
    fn output_cap() {
        let out = zlib_decompress(BIG_Z, 16).unwrap();
        assert_eq!(out, BIG_TEXT.as_bytes()[..16]);
    }

    #[test]
    fn rejects_garbage() {
        assert!(zlib_decompress(&[], 256).is_none());
        assert!(zlib_decompress(&[0x11, 0x22], 256).is_none()); // CM 非法
        assert!(zlib_decompress(&[0x78, 0x9c], 256).is_none()); // 无块数据
        assert!(zlib_decompress(&[0x78, 0x9c, 0xff, 0xff, 0xff, 0xff], 256).is_none()); // 块类型非法
    }

    #[test]
    fn adler_mismatch() {
        let mut broken = L6.to_vec();
        *broken.last_mut().unwrap() ^= 0xff;
        assert!(zlib_decompress(&broken, 4096).is_none());
    }
}

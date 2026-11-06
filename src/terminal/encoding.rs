//! Small shared encodings for terminal graphics and Windows launch arguments.

pub fn base64(bytes: &[u8]) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[usize::from(a >> 2)]);
        out.push(ALPHABET[usize::from((a & 0x03) << 4 | b >> 4)]);
        out.push(if chunk.len() > 1 { ALPHABET[usize::from((b & 0x0f) << 2 | c >> 6)] } else { b'=' });
        out.push(if chunk.len() > 2 { ALPHABET[usize::from(c & 0x3f)] } else { b'=' });
    }
    out
}

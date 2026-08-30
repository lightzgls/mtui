//! Kitty graphics protocol encoding.
//!
//! Kitty accepts raw RGB pixels as base64 split across APC escape sequences.
//! Covers are already resampled to their exact on-screen pixel size, so the
//! terminal only has to place them at the current cursor position.

/// Image numbers occupy a non-unique namespace specifically so terminal apps
/// do not have to guess an unused global image id. Commands act on the newest
/// image with this number, which is always MTUI's current cover.
const IMAGE_NUMBER: u32 = 1_296_326_409;
/// Kitty permits at most 4096 base64 bytes in one protocol chunk.
const CHUNK: usize = 4096;

/// Encodes raw 24-bit RGB pixels as a transmit-and-display command.
pub fn encode(rgb: &[u8], width: u32, height: u32, cols: u16, rows: u16) -> Vec<u8> {
    debug_assert_eq!(rgb.len(), width as usize * height as usize * 3);
    let data = base64(rgb);
    let chunks = data.chunks(CHUNK);
    let mut out = Vec::with_capacity(data.len() + chunks.len() * 32);

    for (index, chunk) in chunks.enumerate() {
        let more = usize::from((index + 1) * CHUNK < data.len());
        if index == 0 {
            out.extend_from_slice(
                format!(
                    "\x1b_Ga=T,f=24,s={width},v={height},c={cols},r={rows},C=1,I={IMAGE_NUMBER},q=2,m={more};"
                )
                .as_bytes(),
            );
        } else {
            out.extend_from_slice(format!("\x1b_Gm={more};").as_bytes());
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

/// Deletes MTUI's image and all of its placements.
pub fn delete() -> Vec<u8> {
    format!("\x1b_Ga=d,d=N,I={IMAGE_NUMBER},q=2;\x1b\\").into_bytes()
}

fn base64(bytes: &[u8]) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[usize::from(a >> 2)]);
        out.push(ALPHABET[usize::from((a & 0x03) << 4 | b >> 4)]);
        out.push(if chunk.len() > 1 {
            ALPHABET[usize::from((b & 0x0f) << 2 | c >> 6)]
        } else {
            b'='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[usize::from(c & 0x3f)]
        } else {
            b'='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encodes_complete_and_partial_groups() {
        assert_eq!(base64(b"Man"), b"TWFu");
        assert_eq!(base64(b"Ma"), b"TWE=");
        assert_eq!(base64(b"M"), b"TQ==");
    }

    #[test]
    fn small_image_is_one_complete_command() {
        assert_eq!(
            String::from_utf8(encode(&[255, 0, 0], 1, 1, 1, 1)).unwrap(),
            format!("\x1b_Ga=T,f=24,s=1,v=1,c=1,r=1,C=1,I={IMAGE_NUMBER},q=2,m=0;/wAA\x1b\\")
        );
    }

    #[test]
    fn large_image_is_chunked_on_base64_boundaries() {
        let rgb = vec![0; 48 * 24 * 3];
        let encoded = String::from_utf8(encode(&rgb, 48, 24, 6, 2)).unwrap();
        assert!(encoded.contains("q=2,m=1;"));
        assert!(encoded.contains("\x1b_Gm=0;"));
        assert_eq!(encoded.matches("\x1b_G").count(), 2);
    }

    #[test]
    fn delete_targets_only_mtuis_image() {
        assert_eq!(
            String::from_utf8(delete()).unwrap(),
            format!("\x1b_Ga=d,d=N,I={IMAGE_NUMBER},q=2;\x1b\\")
        );
    }
}

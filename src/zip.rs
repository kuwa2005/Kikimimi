//! 無圧縮（stored）の ZIP を作る。議事録の Markdown をまとめて渡すだけなので、圧縮はしない。
//! ファイル名は UTF-8（汎用フラグのビット 11）で入れる。

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, entry) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *entry = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc = table[((crc ^ u32::from(*byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// （ファイル名, 中身）の一覧を ZIP にする。
pub fn store(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    const UTF8: u16 = 1 << 11;
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in files {
        let name = name.as_bytes();
        let crc = crc32(data);
        let size = data.len() as u32;
        let offset = out.len() as u32;
        // ローカルファイルヘッダ
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // 展開に必要な版
        out.extend_from_slice(&UTF8.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // 無圧縮
        out.extend_from_slice(&0u16.to_le_bytes()); // 時刻
        out.extend_from_slice(&0x0021u16.to_le_bytes()); // 日付（1980-01-01）
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(data);
        // 中央ディレクトリ
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&UTF8.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0x0021u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0u8; 12]); // 拡張・コメント長、ディスク番号、内部・外部属性
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name);
    }
    let central_offset = out.len() as u32;
    let central_size = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0u8; 4]);
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_known_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn archive_has_entries_and_end_record() {
        let zip = store(&[("議事録.md".into(), "# 見出し\n".as_bytes().to_vec()), ("b.md".into(), b"x".to_vec())]);
        assert_eq!(&zip[..4], &[0x50, 0x4b, 0x03, 0x04]);
        let end = zip.len() - 22;
        assert_eq!(&zip[end..end + 4], &[0x50, 0x4b, 0x05, 0x06]);
        assert_eq!(u16::from_le_bytes([zip[end + 10], zip[end + 11]]), 2);
        assert!(zip.windows("議事録.md".len()).any(|w| w == "議事録.md".as_bytes()));
    }
}

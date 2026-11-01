//! 16kHz / mono / 16bit PCM の WAV。
//! クライアントが区間ごとに完結した WAV を送り、サーバは連結して再生する。

pub fn encode_wav(pcm_s16le: &[u8], sample_rate: u32) -> Vec<u8> {
    let n = pcm_s16le.len() as u32;
    let mut out = Vec::with_capacity(44 + pcm_s16le.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + n).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(pcm_s16le);
    out
}

pub fn decode_wav(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("WAV ではありません".into());
    }
    let mut offset = 12;
    let mut rate = None;
    let mut channels = None;
    let mut bits = None;
    let mut data = None;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let start = offset + 8;
        let end = start.saturating_add(size).min(bytes.len());
        if id == b"fmt " && end - start >= 16 {
            let format = u16::from_le_bytes(bytes[start..start + 2].try_into().unwrap());
            if format != 1 {
                return Err("PCM 以外の WAV です".into());
            }
            channels = Some(u16::from_le_bytes(
                bytes[start + 2..start + 4].try_into().unwrap(),
            ));
            rate = Some(u32::from_le_bytes(
                bytes[start + 4..start + 8].try_into().unwrap(),
            ));
            bits = Some(u16::from_le_bytes(
                bytes[start + 14..start + 16].try_into().unwrap(),
            ));
        } else if id == b"data" {
            data = Some(bytes[start..end].to_vec());
        }
        offset = start + size + (size % 2);
    }
    let rate = rate.ok_or("fmt がありません")?;
    let channels = channels.ok_or("fmt がありません")?;
    let bits = bits.ok_or("fmt がありません")?;
    let data = data.ok_or("data がありません")?;
    if channels != 1 || bits != 16 || rate != 16_000 {
        return Err("音声は 16kHz・モノラル・16bit である必要があります".into());
    }
    if data.is_empty() || data.len() % 2 != 0 {
        return Err("空の音声です".into());
    }
    Ok(data)
}

/// WAV の先頭部分から、PCM（data チャンク）の位置と長さを返す。
/// 区間ファイルを読み込まずに、つなげた録音の任意の位置を返すために使う。
pub fn data_span(head: &[u8], file_len: u64) -> Option<(u64, u64)> {
    if head.len() < 12 || &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        return None;
    }
    let mut offset = 12usize;
    while offset + 8 <= head.len() {
        let id = &head[offset..offset + 4];
        let size = u32::from_le_bytes(head[offset + 4..offset + 8].try_into().ok()?) as u64;
        let start = offset as u64 + 8;
        if id == b"data" {
            let len = size.min(file_len.saturating_sub(start));
            return Some((start, len - len % 2));
        }
        offset = (start + size + size % 2) as usize;
    }
    None
}

pub fn duration_sec(pcm_s16le: &[u8]) -> f64 {
    pcm_s16le.len() as f64 / 2.0 / 16_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_roundtrip() {
        let pcm = vec![0u8, 0, 0xff, 0x7f, 0, 0x80];
        let wav = encode_wav(&pcm, 16_000);
        let back = decode_wav(&wav).unwrap();
        assert_eq!(back, pcm);
        assert!((duration_sec(&pcm) - 3.0 / 16_000.0).abs() < 1e-9);
    }

    #[test]
    fn finds_data_span() {
        let wav = encode_wav(&[1, 0, 2, 0], 16_000);
        assert_eq!(data_span(&wav, wav.len() as u64), Some((44, 4)));
        assert_eq!(data_span(b"nope", 4), None);
    }

    #[test]
    fn reject_wrong_rate() {
        let wav = encode_wav(&[0, 0, 0, 0], 48_000);
        assert!(decode_wav(&wav).is_err());
    }
}

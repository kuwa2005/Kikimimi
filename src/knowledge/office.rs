//! Word（.docx）と Excel（.xlsx）から文字を取り出す（#97）。
//!
//! どちらも ZIP にまとめた XML なので、ZIP の中央ディレクトリを読み、必要な XML だけを展開する。
//! 書式・図・数式の式そのものは読まない（数式のセルは保存されている計算結果の値を使う）。

use std::io::Read;

/// 展開後の上限。巨大な（または圧縮爆弾の）ファイルで止まらないようにする。
const MAX_ENTRY: u64 = 64 * 1024 * 1024;
const MAX_TEXT_CHARS: usize = 2_000_000;
/// 1 ファイルで展開する合計の上限と、読むシート・項目の数の上限。
/// 同じ圧縮データを指す項目を大量に並べた細工ファイルで、CPU とメモリを使い尽くされないようにする。
const MAX_TOTAL: u64 = 256 * 1024 * 1024;
const MAX_SHEETS: usize = 200;
const MAX_ENTRIES: usize = 10_000;

struct Entry {
    name: String,
    method: u16,
    compressed: usize,
    offset: usize,
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn entries(data: &[u8]) -> Result<Vec<Entry>, String> {
    // 末尾から「中央ディレクトリの終わり」を探す（コメントは最大 64KB）。
    let floor = data.len().saturating_sub(22 + 65_535);
    let end = (floor..=data.len().saturating_sub(22))
        .rev()
        .find(|&at| u32_at(data, at) == Some(0x0605_4b50))
        .ok_or("Office の形式（ZIP）として読めません")?;
    let count = u16_at(data, end + 10).ok_or("壊れた ZIP です")? as usize;
    let mut at = u32_at(data, end + 16).ok_or("壊れた ZIP です")? as usize;
    if count > MAX_ENTRIES {
        return Err("ZIP の項目が多すぎます".into());
    }
    let mut out = Vec::with_capacity(count);
    let mut offsets = std::collections::HashSet::new();
    for _ in 0..count {
        if u32_at(data, at) != Some(0x0201_4b50) {
            return Err("壊れた ZIP です".into());
        }
        let method = u16_at(data, at + 10).ok_or("壊れた ZIP です")?;
        let compressed = u32_at(data, at + 20).ok_or("壊れた ZIP です")? as usize;
        let name_len = u16_at(data, at + 28).ok_or("壊れた ZIP です")? as usize;
        let extra_len = u16_at(data, at + 30).ok_or("壊れた ZIP です")? as usize;
        let comment_len = u16_at(data, at + 32).ok_or("壊れた ZIP です")? as usize;
        let offset = u32_at(data, at + 42).ok_or("壊れた ZIP です")? as usize;
        let name = String::from_utf8_lossy(data.get(at + 46..at + 46 + name_len).ok_or("壊れた ZIP です")?).to_string();
        // 同じ位置を指す項目は正しい Office ファイルには無い（展開を何度も繰り返させる細工）。
        if !offsets.insert(offset) {
            return Err("壊れた ZIP です（同じ位置を指す項目があります）".into());
        }
        out.push(Entry { name, method, compressed, offset });
        at += 46 + name_len + extra_len + comment_len;
    }
    Ok(out)
}

fn read_entry(data: &[u8], entry: &Entry) -> Result<String, String> {
    let at = entry.offset;
    if u32_at(data, at) != Some(0x0403_4b50) {
        return Err("壊れた ZIP です".into());
    }
    let name_len = u16_at(data, at + 26).ok_or("壊れた ZIP です")? as usize;
    let extra_len = u16_at(data, at + 28).ok_or("壊れた ZIP です")? as usize;
    let start = at + 30 + name_len + extra_len;
    let body = data.get(start..start + entry.compressed).ok_or("壊れた ZIP です")?;
    let mut out = Vec::new();
    match entry.method {
        0 => out.extend_from_slice(body),
        8 => {
            flate2::read::DeflateDecoder::new(body)
                .take(MAX_ENTRY + 1)
                .read_to_end(&mut out)
                .map_err(|_| "ZIP を展開できません".to_string())?;
        }
        _ => return Err("対応していない圧縮形式です（暗号化されたファイルは読めません）".into()),
    }
    if out.len() as u64 > MAX_ENTRY {
        return Err("ファイルの中身が大きすぎます".into());
    }
    Ok(String::from_utf8_lossy(&out).to_string())
}

fn find<'a>(list: &'a [Entry], name: &str) -> Option<&'a Entry> {
    list.iter().find(|entry| entry.name == name)
}

/// XML の文字参照を戻す。
fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        let Some(end) = rest.find(';').filter(|&end| end <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if entity.starts_with("#x") => u32::from_str_radix(&entity[2..], 16).ok().and_then(char::from_u32),
            _ if entity.starts_with('#') => entity[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `<tag ...>中身</tag>` の中身を順に返す（`tag` は名前空間つきの完全な名前）。
fn texts_of<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(pos) = rest.find(&open) {
        let after = &rest[pos + open.len()..];
        // `<w:t>` と `<w:tab/>` などを区別する。
        let Some(first) = after.chars().next() else { break };
        if first != '>' && first != ' ' && first != '/' {
            rest = after;
            continue;
        }
        let Some(gt) = after.find('>') else { break };
        if after[..gt].ends_with('/') {
            rest = &after[gt + 1..];
            continue;
        }
        let body = &after[gt + 1..];
        let Some(end) = body.find(&close) else { break };
        out.push(&body[..end]);
        rest = &body[end + close.len()..];
    }
    out
}

/// Word: 段落ごとに 1 行。表のセルは「 | 」でつなぐ。
pub fn docx_text(data: &[u8]) -> Result<String, String> {
    let list = entries(data)?;
    let document = find(&list, "word/document.xml").ok_or("Word の本文が見つかりません")?;
    let xml = read_entry(data, document)?;
    let mut out = String::new();
    for paragraph in xml.split("</w:p>") {
        let mut line = String::new();
        for (index, cell) in paragraph.split("</w:tc>").enumerate() {
            let text: String = texts_of(cell, "w:t").into_iter().map(unescape).collect();
            if index > 0 && !text.is_empty() {
                line.push_str(" | ");
            }
            line.push_str(&text);
        }
        let line = line.trim();
        if !line.is_empty() {
            out.push_str(line);
            out.push('\n');
        }
        if out.len() > MAX_TEXT_CHARS * 3 {
            break;
        }
    }
    if out.trim().is_empty() {
        return Err("テキストを抽出できません".into());
    }
    Ok(out)
}

/// 列番号（A=0, B=1, …）。
fn column_of(reference: &str) -> usize {
    reference
        .chars()
        .take_while(|ch| ch.is_ascii_alphabetic())
        .fold(0usize, |acc, ch| acc * 26 + (ch.to_ascii_uppercase() as usize - 'A' as usize + 1))
        .saturating_sub(1)
}

fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let start = tag.find(&key)? + key.len();
    let end = tag[start..].find('"')?;
    Some(&tag[start..start + end])
}

/// Excel: シートごとに「## シート名」、行ごとにセルを「 | 」でつなぐ。数式のセルは保存済みの値。
pub fn xlsx_text(data: &[u8]) -> Result<String, String> {
    let list = entries(data)?;
    let shared: Vec<String> = match find(&list, "xl/sharedStrings.xml") {
        Some(entry) => {
            let xml = read_entry(data, entry)?;
            texts_of(&xml, "si")
                .into_iter()
                .map(|item| texts_of(item, "t").into_iter().map(unescape).collect::<String>())
                .collect()
        }
        None => Vec::new(),
    };
    let names: Vec<String> = match find(&list, "xl/workbook.xml") {
        Some(entry) => {
            let xml = read_entry(data, entry)?;
            xml.split("<sheet ")
                .skip(1)
                .filter_map(|tag| attribute(&format!(" {tag}"), "name").map(unescape))
                .collect()
        }
        None => Vec::new(),
    };
    let mut sheets: Vec<(usize, &Entry)> = list
        .iter()
        .filter_map(|entry| {
            let number = entry.name.strip_prefix("xl/worksheets/sheet")?.strip_suffix(".xml")?;
            Some((number.parse().ok()?, entry))
        })
        .collect();
    sheets.sort_by_key(|(number, _)| *number);
    if sheets.is_empty() {
        return Err("Excel のシートが見つかりません".into());
    }
    if sheets.len() > MAX_SHEETS {
        return Err(format!("シートが多すぎます（{} 枚。{MAX_SHEETS} 枚まで）", sheets.len()));
    }
    let mut out = String::new();
    let mut expanded: u64 = 0;
    for (index, (number, entry)) in sheets.iter().enumerate() {
        if out.len() > MAX_TEXT_CHARS * 3 {
            break;
        }
        let xml = read_entry(data, entry)?;
        expanded += xml.len() as u64;
        if expanded > MAX_TOTAL {
            return Err("ファイルの中身が大きすぎます".into());
        }
        let name = names.get(index).cloned().unwrap_or_else(|| format!("シート{number}"));
        out.push_str(&format!("## {name}\n"));
        for row in texts_of(&xml, "row") {
            let mut cells: Vec<(usize, String)> = Vec::new();
            let mut rest = row;
            while let Some(pos) = rest.find("<c ") {
                let after = &rest[pos..];
                let Some(gt) = after.find('>') else { break };
                let tag = &after[..gt];
                let (body, next) = if tag.ends_with('/') {
                    ("", &after[gt + 1..])
                } else {
                    let inner = &after[gt + 1..];
                    match inner.find("</c>") {
                        Some(end) => (&inner[..end], &inner[end + 4..]),
                        None => break,
                    }
                };
                rest = next;
                let column = attribute(tag, "r").map(column_of).unwrap_or(cells.len());
                let kind = attribute(tag, "t").unwrap_or("");
                let value = match kind {
                    "s" => texts_of(body, "v")
                        .first()
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .and_then(|i| shared.get(i).cloned())
                        .unwrap_or_default(),
                    "inlineStr" => texts_of(body, "t").into_iter().map(unescape).collect(),
                    _ => texts_of(body, "v").first().map(|v| unescape(v)).unwrap_or_default(),
                };
                let value = value.trim().to_string();
                if !value.is_empty() {
                    cells.push((column, value));
                }
            }
            if !cells.is_empty() {
                cells.sort_by_key(|(column, _)| *column);
                let line = cells.into_iter().map(|(_, value)| value).collect::<Vec<_>>().join(" | ");
                out.push_str(&line);
                out.push('\n');
            }
            if out.len() > MAX_TEXT_CHARS * 3 {
                break;
            }
        }
        out.push('\n');
    }
    if out.lines().all(|line| line.starts_with("## ") || line.trim().is_empty()) {
        return Err("テキストを抽出できません".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn office(files: &[(&str, &str)]) -> Vec<u8> {
        // テスト用: 無圧縮の ZIP と、Deflate の ZIP の両方で読めることを確かめる。
        let stored: Vec<(String, Vec<u8>)> = files.iter().map(|(n, b)| (n.to_string(), b.as_bytes().to_vec())).collect();
        crate::zip::store(&stored)
    }

    #[test]
    fn reads_docx_paragraphs_and_tables() {
        let xml = r#"<w:document><w:body><w:p><w:r><w:t>見積の条件</w:t></w:r></w:p>
            <w:p><w:r><w:t xml:space="preserve">価格は </w:t></w:r><w:r><w:t>300万円 &amp; 税別</w:t></w:r></w:p>
            <w:tbl><w:tr><w:tc><w:p><w:r><w:t>品目</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>数量</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
            </w:body></w:document>"#;
        let text = docx_text(&office(&[("word/document.xml", xml)])).unwrap();
        assert!(text.contains("見積の条件\n"));
        assert!(text.contains("価格は 300万円 & 税別"));
        assert!(text.contains("品目"));
    }

    #[test]
    fn reads_xlsx_with_shared_strings_and_numbers() {
        let workbook = r#"<workbook><sheets><sheet name="見積" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
        let shared = r#"<sst><si><t>品目</t></si><si><t>単価</t></si><si><r><t>配管</t></r><r><t>工事</t></r></si></sst>"#;
        let sheet = r#"<worksheet><sheetData>
            <row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row>
            <row r="2"><c r="A2" t="s"><v>2</v></c><c r="B2"><f>1+2</f><v>120000</v></c><c r="C2" t="inlineStr"><is><t>税別</t></is></c></row>
            </sheetData></worksheet>"#;
        let text = xlsx_text(&office(&[("xl/workbook.xml", workbook), ("xl/sharedStrings.xml", shared), ("xl/worksheets/sheet1.xml", sheet)])).unwrap();
        assert!(text.contains("## 見積"));
        assert!(text.contains("品目 | 単価"));
        assert!(text.contains("配管工事 | 120000 | 税別"));
    }

    #[test]
    fn deflated_entries_are_read() {
        use std::io::Write;
        let xml = "<w:document><w:p><w:r><w:t>圧縮された本文</w:t></w:r></w:p></w:document>";
        let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(xml.as_bytes()).unwrap();
        let deflated = encoder.finish().unwrap();
        // 無圧縮で作った ZIP の本体を、Deflate の本体に入れ替える。
        let mut zip = office(&[("word/document.xml", xml)]);
        let name = "word/document.xml";
        let header = 30 + name.len();
        let body_end = header + xml.len();
        let mut rebuilt = zip[..header].to_vec();
        rebuilt.extend_from_slice(&deflated);
        let shift = deflated.len() as i64 - xml.len() as i64;
        rebuilt.extend_from_slice(&zip[body_end..]);
        zip = rebuilt;
        // ローカルヘッダと中央ディレクトリの方式・サイズを直す。
        zip[8..10].copy_from_slice(&8u16.to_le_bytes());
        zip[18..22].copy_from_slice(&(deflated.len() as u32).to_le_bytes());
        let central = (body_end as i64 + shift) as usize;
        zip[central + 10..central + 12].copy_from_slice(&8u16.to_le_bytes());
        zip[central + 20..central + 24].copy_from_slice(&(deflated.len() as u32).to_le_bytes());
        let end = zip.len() - 22;
        zip[end + 16..end + 20].copy_from_slice(&(central as u32).to_le_bytes());
        assert!(docx_text(&zip).unwrap().contains("圧縮された本文"));
    }

    #[test]
    fn not_a_zip_is_an_error() {
        assert!(docx_text(b"plain text").is_err());
    }
}

#[cfg(test)]
mod bomb_tests {
    use super::xlsx_text;

    /// 1 つの圧縮データを、シート名の項目から何度も指させた細工ファイルは読まない。
    #[test]
    fn rejects_entries_sharing_one_payload() {
        let mut data = Vec::new();
        // ローカルヘッダ（無圧縮・名前 1 文字）と中身
        data.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        data.extend_from_slice(&[0u8; 22]);
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.push(b'x');
        let central = data.len();
        let count = 3u16;
        for i in 0..count {
            let name = format!("xl/worksheets/sheet{}.xml", i + 1);
            data.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            data.extend_from_slice(&[0u8; 6]);
            data.extend_from_slice(&0u16.to_le_bytes()); // method
            data.extend_from_slice(&[0u8; 8]);
            data.extend_from_slice(&0u32.to_le_bytes()); // compressed
            data.extend_from_slice(&0u32.to_le_bytes());
            data.extend_from_slice(&(name.len() as u16).to_le_bytes());
            data.extend_from_slice(&[0u8; 12]);
            data.extend_from_slice(&0u32.to_le_bytes()); // offset（全部 0）
            data.extend_from_slice(name.as_bytes());
        }
        let size = data.len() - central;
        data.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        data.extend_from_slice(&[0u8; 6]);
        data.extend_from_slice(&count.to_le_bytes());
        data.extend_from_slice(&(size as u32).to_le_bytes());
        data.extend_from_slice(&(central as u32).to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        let err = xlsx_text(&data).unwrap_err();
        assert!(err.contains("同じ位置"), "{err}");
    }
}

use super::*;

// A seal records only an incomplete JSON row observed at EOF. Keeping this
// receipt beside the original bytes lets strict writers distinguish it from
// unrelated corruption after subsequent appends (including after a restart).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TornTailSeal {
    session_torn_tail: u64,
    bytes: usize,
    hash: u64,
}

fn incomplete_json(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line).is_err_and(|error| error.is_eof())
        && !line.trim().is_empty()
}

pub(super) fn seal_suffix(tail: &str, offset: u64) -> String {
    if offset > 0 && incomplete_json(tail) {
        format!(
            "\n{{\"session_torn_tail\":{offset},\"bytes\":{},\"hash\":{}}}\n",
            tail.len(),
            fnv1a_64(tail.as_bytes())
        )
    } else {
        "\n".to_owned()
    }
}

pub(super) fn is_sealed(tail: &str, offset: u64, receipt: &str) -> bool {
    incomplete_json(tail)
        && serde_json::from_str::<TornTailSeal>(receipt).is_ok_and(|seal| {
            seal.session_torn_tail == offset
                && seal.bytes == tail.len()
                && seal.hash == fnv1a_64(tail.as_bytes())
        })
}

pub(super) fn is_incomplete(tail: &str) -> bool {
    incomplete_json(tail)
}

pub(super) fn seal(file: &mut std::fs::File, len: u64) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    if len == 0 {
        return Ok(());
    }
    // Read only the final row, backwards in chunks; do not scan the transcript
    // on every append. A normal newline-terminated file takes one byte.
    file.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0];
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(());
    }
    let mut chunks = Vec::new();
    let mut offset = len;
    while offset > 0 {
        let start = offset.saturating_sub(4096);
        let mut chunk = vec![0; (offset - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut chunk)?;
        if let Some(index) = chunk.iter().rposition(|byte| *byte == b'\n') {
            offset = start + index as u64 + 1;
            chunks.push(chunk.split_off(index + 1));
            break;
        }
        chunks.push(chunk);
        offset = start;
    }
    let bytes: Vec<u8> = chunks.into_iter().rev().flatten().collect();
    let tail = std::str::from_utf8(&bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    file.write_all(seal_suffix(tail, offset).as_bytes())
}

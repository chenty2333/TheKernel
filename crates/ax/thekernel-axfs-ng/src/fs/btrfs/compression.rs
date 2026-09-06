use alloc::vec::Vec;

use axerrno::{AxError, AxResult};

/// Btrfs extent compression encoding.  Zlib uses raw DEFLATE and Zstd uses
/// its native bounded frame decoder; neither path accepts a decoder-selected
/// output length.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Compression {
    None,
    Zlib,
    Lzo,
    Zstd,
}

#[derive(Clone, Debug)]
// Writer-side API kept for the gated Btrfs COW writer.
#[allow(dead_code)]
pub struct CompressedExtent {
    pub compression: Compression,
    pub logical_len: usize,
    pub bytes: Vec<u8>,
}

// Writer-side encode/decode entry points kept for the gated Btrfs COW writer.
#[allow(dead_code)]
impl Compression {
    pub fn encode(self, input: &[u8]) -> AxResult<CompressedExtent> {
        let bytes = match self {
            Self::None => {
                let mut copy = Vec::new();
                copy.try_reserve_exact(input.len())
                    .map_err(|_| AxError::NoMemory)?;
                copy.extend_from_slice(input);
                copy
            }
            // Btrfs stores a raw DEFLATE payload for zlib-compressed extents,
            // not a zlib wrapper; miniz's raw API matches that representation.
            Self::Zlib => miniz_oxide::deflate::compress_to_vec(input, 6),
            // A Zstd frame may carry raw blocks.  This deliberately chooses
            // that deterministic representation when no bounded entropy
            // compressor is available: the on-media extent is still native
            // Zstd and round-trips through Linux, without a heuristic memory
            // budget or an invented private framing.
            Self::Zstd => encode_zstd_raw_blocks(input)?,
            Self::Lzo => encode_btrfs_lzo(input)?,
        };
        Ok(CompressedExtent {
            compression: self,
            logical_len: input.len(),
            bytes,
        })
    }

    pub fn decode(self, input: &[u8], logical_len: usize) -> AxResult<Vec<u8>> {
        let output = match self {
            Self::None => {
                if input.len() != logical_len {
                    return Err(AxError::Io);
                }
                let mut copy = Vec::new();
                copy.try_reserve_exact(input.len())
                    .map_err(|_| AxError::NoMemory)?;
                copy.extend_from_slice(input);
                copy
            }
            Self::Zlib => miniz_oxide::inflate::decompress_to_vec_with_limit(input, logical_len)
                .map_err(|_| AxError::Io)?,
            // Btrfs Zstd extent payloads are standard frames.  The caller's
            // inode/extent metadata supplies the exact decompressed length,
            // which bounds the allocation and rejects expansion attacks.
            Self::Zstd => {
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(logical_len)
                    .map_err(|_| AxError::NoMemory)?;
                bytes.resize(logical_len, 0);
                let written = ruzstd::decoding::FrameDecoder::new()
                    .decode_all(input, &mut bytes)
                    .map_err(|_| AxError::Io)?;
                bytes.truncate(written);
                bytes
            }
            Self::Lzo => decode_btrfs_lzo(input, logical_len)?,
        };
        if output.len() != logical_len {
            return Err(AxError::Io);
        }
        Ok(output)
    }
}

#[allow(dead_code)]
impl CompressedExtent {
    pub fn decode(&self) -> AxResult<Vec<u8>> {
        self.compression.decode(&self.bytes, self.logical_len)
    }
}

// Writer-side encoder kept for the gated Btrfs COW writer.
#[allow(dead_code)]
fn encode_zstd_raw_blocks(input: &[u8]) -> AxResult<Vec<u8>> {
    const BLOCK: usize = 128 * 1024;
    let blocks = input
        .len()
        .checked_add(BLOCK - 1)
        .ok_or(AxError::NoMemory)?
        / BLOCK;
    let capacity = 4usize
        .checked_add(1)
        .and_then(|value| value.checked_add(8))
        .and_then(|value| value.checked_add(blocks.checked_mul(3)?))
        .and_then(|value| value.checked_add(input.len()))
        .ok_or(AxError::NoMemory)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(capacity)
        .map_err(|_| AxError::NoMemory)?;
    output.extend_from_slice(&[0x28, 0xb5, 0x2f, 0xfd]);
    // Single segment + 8-byte frame content size; no dictionary/checksum.
    output.push(0xe0);
    output.extend_from_slice(&(input.len() as u64).to_le_bytes());
    if input.is_empty() {
        output.extend_from_slice(&1u32.to_le_bytes()[..3]);
        return Ok(output);
    }
    for (index, block) in input.chunks(BLOCK).enumerate() {
        let last = index + 1 == blocks;
        let header = (u32::try_from(block.len()).map_err(|_| AxError::NoMemory)? << 3)
            | if last { 1 } else { 0 };
        output.extend_from_slice(&header.to_le_bytes()[..3]);
        output.extend_from_slice(block);
    }
    Ok(output)
}

const LZO_PAGE: usize = 4096;

/// Btrfs LZO is a length-prefixed sequence of independently decodable LZO1X
/// streams.  This encoder deliberately uses the valid literal-only subset:
/// it is native LZO1X (not a private "stored" substitute), keeps each
/// uncompressed segment within one page, and interoperates with Linux while
/// leaving match selection as a future performance improvement.
// Writer-side encoder kept for the gated Btrfs COW writer.
#[allow(dead_code)]
fn encode_btrfs_lzo(input: &[u8]) -> AxResult<Vec<u8>> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(
            4usize
                .checked_add(input.len())
                .and_then(|n| n.checked_add(input.len() / 16))
                .and_then(|n| n.checked_add(128))
                .ok_or(AxError::NoMemory)?,
        )
        .map_err(|_| AxError::NoMemory)?;
    output.resize(4, 0);
    if input.is_empty() {
        let raw = encode_lzo1x_literals(input)?;
        output.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        output.extend_from_slice(&raw);
    } else {
        for segment in input.chunks(LZO_PAGE) {
            let raw = encode_lzo1x_literals(segment)?;
            output.extend_from_slice(&(raw.len() as u32).to_le_bytes());
            output.extend_from_slice(&raw);
            // A segment header must not straddle a Btrfs sector/page.  The
            // padding is part of the total compressed length.
            let page_offset = output.len() % LZO_PAGE;
            if page_offset > LZO_PAGE - 4 {
                output.resize(output.len() + LZO_PAGE - page_offset, 0);
            }
        }
    }
    let total = u32::try_from(output.len()).map_err(|_| AxError::StorageFull)?;
    output[..4].copy_from_slice(&total.to_le_bytes());
    Ok(output)
}

// Writer-side encoder kept for the gated Btrfs COW writer.
#[allow(dead_code)]
fn encode_lzo1x_literals(input: &[u8]) -> AxResult<Vec<u8>> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(
            input
                .len()
                .checked_add(input.len() / 255)
                .and_then(|n| n.checked_add(8))
                .ok_or(AxError::NoMemory)?,
        )
        .map_err(|_| AxError::NoMemory)?;
    match input.len() {
        0 => {}
        1..=238 => output.push(u8::try_from(input.len() + 17).map_err(|_| AxError::InvalidInput)?),
        length => {
            output.push(0);
            let mut encoded = length.checked_sub(18).ok_or(AxError::InvalidInput)?;
            while encoded >= 255 {
                output.push(0);
                encoded -= 255;
            }
            // The terminating length byte is non-zero.  Move one 255 unit
            // into it for exact multiples.
            if encoded == 0 {
                let last = output.pop().ok_or(AxError::InvalidInput)?;
                if last != 0 {
                    return Err(AxError::InvalidInput);
                }
                output.push(255);
            } else {
                output.push(u8::try_from(encoded).map_err(|_| AxError::InvalidInput)?);
            }
        }
    }
    output.extend_from_slice(input);
    output.extend_from_slice(&[0x11, 0, 0]);
    Ok(output)
}

fn decode_btrfs_lzo(input: &[u8], logical_len: usize) -> AxResult<Vec<u8>> {
    if input.len() < 8 {
        return Err(AxError::Io);
    }
    let total = usize::try_from(u32::from_le_bytes(
        input[..4].try_into().map_err(|_| AxError::Io)?,
    ))
    .map_err(|_| AxError::Io)?;
    if total < 8 || total > input.len() {
        return Err(AxError::Io);
    }
    let mut cursor = 4usize;
    let mut output = Vec::new();
    output
        .try_reserve_exact(logical_len)
        .map_err(|_| AxError::NoMemory)?;
    while cursor < total {
        if total - cursor < 4 {
            return Err(AxError::Io);
        }
        let size = usize::try_from(u32::from_le_bytes(
            input[cursor..cursor + 4]
                .try_into()
                .map_err(|_| AxError::Io)?,
        ))
        .map_err(|_| AxError::Io)?;
        cursor = cursor.checked_add(4).ok_or(AxError::Io)?;
        let end = cursor.checked_add(size).ok_or(AxError::Io)?;
        if size == 0 || end > total {
            return Err(AxError::Io);
        }
        decode_lzo1x(&input[cursor..end], &mut output, logical_len)?;
        cursor = end;
        let page_offset = cursor % LZO_PAGE;
        if page_offset > LZO_PAGE - 4 {
            cursor = cursor
                .checked_add(LZO_PAGE - page_offset)
                .ok_or(AxError::Io)?;
            if cursor > total {
                return Err(AxError::Io);
            }
        }
    }
    if cursor != total || output.len() != logical_len {
        return Err(AxError::Io);
    }
    Ok(output)
}

fn decode_lzo1x(input: &[u8], output: &mut Vec<u8>, limit: usize) -> AxResult<()> {
    let mut cursor = 0usize;
    let mut state = 0usize;
    let mut first = true;
    loop {
        let token = *input.get(cursor).ok_or(AxError::Io)?;
        cursor += 1;
        if first && token > 17 {
            let literal_count = usize::from(token - 17);
            copy_literals(input, &mut cursor, output, literal_count, limit)?;
            state = if literal_count < 4 { literal_count } else { 4 };
            first = false;
            continue;
        }
        first = false;
        let (length, distance, next_state, end) = match token {
            0..=15 if state == 0 => {
                let length = 3usize
                    .checked_add(lzo_length(input, &mut cursor, usize::from(token), 15)?)
                    .ok_or(AxError::Io)?;
                copy_literals(input, &mut cursor, output, length, limit)?;
                state = 4;
                continue;
            }
            0..=15 => {
                let high = usize::from(*input.get(cursor).ok_or(AxError::Io)?);
                cursor += 1;
                (
                    2,
                    high.checked_mul(4)
                        .and_then(|n| n.checked_add(usize::from(token >> 2)))
                        .and_then(|n| n.checked_add(if state == 4 { 2049 } else { 1 }))
                        .ok_or(AxError::Io)?,
                    usize::from(token & 3),
                    false,
                )
            }
            16..=31 => {
                let length = 2usize
                    .checked_add(lzo_length(input, &mut cursor, usize::from(token & 7), 7)?)
                    .ok_or(AxError::Io)?;
                let word = le16(input, &mut cursor)?;
                let distance = 16_384usize
                    .checked_add(
                        usize::from(token & 8)
                            .checked_mul(2048)
                            .ok_or(AxError::Io)?,
                    )
                    .and_then(|n| n.checked_add(usize::from(word >> 2)))
                    .ok_or(AxError::Io)?;
                (length, distance, usize::from(word & 3), distance == 16_384)
            }
            32..=63 => {
                let length = 2usize
                    .checked_add(lzo_length(input, &mut cursor, usize::from(token & 31), 31)?)
                    .ok_or(AxError::Io)?;
                let word = le16(input, &mut cursor)?;
                (
                    length,
                    usize::from(word >> 2).checked_add(1).ok_or(AxError::Io)?,
                    usize::from(word & 3),
                    false,
                )
            }
            64..=127 => {
                let high = usize::from(*input.get(cursor).ok_or(AxError::Io)?);
                cursor += 1;
                (
                    3 + usize::from((token >> 5) & 1),
                    high.checked_mul(8)
                        .and_then(|n| n.checked_add(usize::from((token >> 2) & 7)))
                        .and_then(|n| n.checked_add(1))
                        .ok_or(AxError::Io)?,
                    usize::from(token & 3),
                    false,
                )
            }
            _ => {
                let high = usize::from(*input.get(cursor).ok_or(AxError::Io)?);
                cursor += 1;
                (
                    5 + usize::from((token >> 5) & 3),
                    high.checked_mul(8)
                        .and_then(|n| n.checked_add(usize::from((token >> 2) & 7)))
                        .and_then(|n| n.checked_add(1))
                        .ok_or(AxError::Io)?,
                    usize::from(token & 3),
                    false,
                )
            }
        };
        if end {
            if cursor != input.len() {
                return Err(AxError::Io);
            }
            return Ok(());
        }
        copy_match(output, distance, length, limit)?;
        state = next_state;
        copy_literals(input, &mut cursor, output, state, limit)?;
    }
}

fn lzo_length(input: &[u8], cursor: &mut usize, small: usize, base: usize) -> AxResult<usize> {
    if small != 0 {
        return Ok(small);
    }
    let mut length = base;
    loop {
        let byte = usize::from(*input.get(*cursor).ok_or(AxError::Io)?);
        *cursor += 1;
        if byte != 0 {
            return length.checked_add(byte).ok_or(AxError::Io);
        }
        length = length.checked_add(255).ok_or(AxError::Io)?;
    }
}
fn le16(input: &[u8], cursor: &mut usize) -> AxResult<u16> {
    let end = cursor.checked_add(2).ok_or(AxError::Io)?;
    let bytes: [u8; 2] = input
        .get(*cursor..end)
        .ok_or(AxError::Io)?
        .try_into()
        .map_err(|_| AxError::Io)?;
    *cursor = end;
    Ok(u16::from_le_bytes(bytes))
}
fn copy_literals(
    input: &[u8],
    cursor: &mut usize,
    output: &mut Vec<u8>,
    count: usize,
    limit: usize,
) -> AxResult<()> {
    let end = cursor.checked_add(count).ok_or(AxError::Io)?;
    let bytes = input.get(*cursor..end).ok_or(AxError::Io)?;
    if output
        .len()
        .checked_add(count)
        .map_or(true, |size| size > limit)
    {
        return Err(AxError::Io);
    }
    output.try_reserve(count).map_err(|_| AxError::NoMemory)?;
    output.extend_from_slice(bytes);
    *cursor = end;
    Ok(())
}
fn copy_match(output: &mut Vec<u8>, distance: usize, count: usize, limit: usize) -> AxResult<()> {
    if distance == 0
        || distance > output.len()
        || output
            .len()
            .checked_add(count)
            .map_or(true, |size| size > limit)
    {
        return Err(AxError::Io);
    }
    output.try_reserve(count).map_err(|_| AxError::NoMemory)?;
    for _ in 0..count {
        let byte = output[output.len() - distance];
        output.push(byte);
    }
    Ok(())
}

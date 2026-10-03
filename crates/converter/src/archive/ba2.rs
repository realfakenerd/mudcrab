use color_eyre::{
    Result,
    eyre::{WrapErr, bail, ensure, eyre},
};
use flate2::read::{DeflateDecoder, ZlibDecoder};
use std::io::Read;

const GENERAL_RECORD_SIZE: usize = 36;
const DX10_RECORD_SIZE: usize = 24;
const DX10_CHUNK_SIZE: usize = 24;
const MAX_FILES: usize = 1_000_000;
const MAX_ENTRY_SIZE: usize = 1024 * 1024 * 1024;
/// Deflate cannot expand a stream by more than 1032:1.
const MAX_ZLIB_EXPANSION: usize = 1032;
/// LZ4 cannot expand a block by more than 255:1.
const MAX_LZ4_EXPANSION: usize = 255;
/// Headroom for the framing of a tiny stream, whose own block headers cost
/// more than the data it carries.
const DECOMPRESSION_SLACK: usize = 4096;
/// Reserved up front for a decompressed entry; the rest grows on demand.
const RESERVE_CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    General,
    Dx10,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compression {
    Zlib,
    Lz4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    version: u32,
    kind: ArchiveKind,
    file_count: usize,
    names_offset: usize,
    records_offset: usize,
    compression: Compression,
}

#[derive(Debug)]
struct GeneralRecord {
    offset: u64,
    packed_size: u32,
    unpacked_size: u32,
}

#[derive(Debug)]
struct Dx10Record {
    height: u16,
    width: u16,
    mip_count: u8,
    format: u8,
    is_cubemap: bool,
    chunks: Vec<Dx10Chunk>,
}

#[derive(Debug)]
struct Dx10Chunk {
    offset: u64,
    packed_size: u32,
    unpacked_size: u32,
    start_mip: u16,
    end_mip: u16,
}

enum Record {
    General(GeneralRecord),
    Dx10(Dx10Record),
}

#[cfg(test)]
pub(crate) fn read_entries(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    read_entries_matching(bytes, |_| Ok(true))
}

pub(crate) fn read_entries_matching(
    bytes: &[u8],
    include: impl Fn(&str) -> Result<bool>,
) -> Result<Vec<(String, Vec<u8>)>> {
    let header = parse_header(bytes)?;
    let records = match header.kind {
        ArchiveKind::General => parse_general_records(bytes, header)?,
        ArchiveKind::Dx10 => parse_dx10_records(bytes, header)?,
    };
    let names = parse_names(bytes, header.names_offset, header.file_count)?;

    records
        .into_iter()
        .zip(names)
        .filter_map(|(record, name)| match include(&name) {
            Ok(true) => Some(Ok((record, name))),
            Ok(false) => None,
            Err(error) => Some(Err(error)),
        })
        .map(|entry| {
            let (record, name) = entry?;
            let data = match record {
                Record::General(record) => extract_chunk(
                    bytes,
                    record.offset,
                    record.packed_size,
                    record.unpacked_size,
                    header.compression,
                    &name,
                )?,
                Record::Dx10(record) => extract_dx10(bytes, record, header.compression, &name)?,
            };
            Ok((name, data))
        })
        .collect()
}

fn parse_header(bytes: &[u8]) -> Result<Header> {
    ensure!(bytes.len() >= 24, "truncated BA2 header");
    ensure!(&bytes[..4] == b"BTDX", "invalid BA2 magic");
    let version = u32_at(bytes, 4)?;
    ensure!(
        matches!(version, 1 | 2 | 3 | 7 | 8),
        "unsupported BA2 version {version}"
    );
    let kind = match bytes.get(8..12) {
        Some(b"GNRL") => ArchiveKind::General,
        Some(b"DX10") => ArchiveKind::Dx10,
        Some(other) => bail!(
            "unsupported BA2 archive type {:?}",
            String::from_utf8_lossy(other)
        ),
        None => unreachable!(),
    };
    let file_count = u32_at(bytes, 12)? as usize;
    ensure!(
        file_count <= MAX_FILES,
        "BA2 file count exceeds safety limit"
    );
    let names_offset = usize::try_from(u64_at(bytes, 16)?)
        .map_err(|_| eyre!("BA2 name table offset does not fit usize"))?;
    let (records_offset, compression) = match version {
        1 | 7 | 8 => (24, Compression::Zlib),
        2 => {
            ensure!(bytes.len() >= 32, "truncated BA2 v2 header");
            (32, Compression::Zlib)
        }
        3 => {
            ensure!(bytes.len() >= 36, "truncated BA2 v3 header");
            let compression = match u32_at(bytes, 32)? {
                0 => Compression::Zlib,
                3 => Compression::Lz4,
                method => bail!("unsupported BA2 v3 compression method {method}"),
            };
            (36, compression)
        }
        _ => unreachable!(),
    };
    ensure!(
        names_offset >= records_offset && names_offset <= bytes.len(),
        "invalid BA2 name table offset"
    );
    Ok(Header {
        version,
        kind,
        file_count,
        names_offset,
        records_offset,
        compression,
    })
}

fn parse_general_records(bytes: &[u8], header: Header) -> Result<Vec<Record>> {
    let records_end = header
        .records_offset
        .checked_add(
            header
                .file_count
                .checked_mul(GENERAL_RECORD_SIZE)
                .ok_or_else(|| eyre!("BA2 general record table overflow"))?,
        )
        .ok_or_else(|| eyre!("BA2 general record table overflow"))?;
    ensure!(
        records_end <= header.names_offset && records_end <= bytes.len(),
        "truncated BA2 general record table"
    );
    (0..header.file_count)
        .map(|index| {
            let base = header.records_offset + index * GENERAL_RECORD_SIZE;
            let record = GeneralRecord {
                offset: u64_at(bytes, base + 16)?,
                packed_size: u32_at(bytes, base + 24)?,
                unpacked_size: u32_at(bytes, base + 28)?,
            };
            validate_sizes(record.packed_size, record.unpacked_size, "general record")?;
            ensure!(
                record.unpacked_size > 0 || record.packed_size == 0,
                "empty BA2 general record cannot contain packed data"
            );
            Ok(Record::General(record))
        })
        .collect()
}

fn parse_dx10_records(bytes: &[u8], header: Header) -> Result<Vec<Record>> {
    let mut cursor = header.records_offset;
    let mut records = Vec::with_capacity(header.file_count);
    for index in 0..header.file_count {
        let base_end = cursor
            .checked_add(DX10_RECORD_SIZE)
            .ok_or_else(|| eyre!("BA2 DX10 record table overflow"))?;
        ensure!(
            base_end <= header.names_offset && base_end <= bytes.len(),
            "truncated BA2 DX10 record {index}"
        );
        let chunk_count = bytes[cursor + 13] as usize;
        ensure!(chunk_count > 0, "BA2 DX10 record {index} has no chunks");
        ensure!(
            u16_at(bytes, cursor + 14)? as usize == DX10_CHUNK_SIZE,
            "unsupported BA2 DX10 chunk header size in record {index}"
        );
        let height = u16_at(bytes, cursor + 16)?;
        let width = u16_at(bytes, cursor + 18)?;
        let mip_count = bytes[cursor + 20];
        ensure!(
            width > 0 && height > 0,
            "invalid BA2 DX10 dimensions in record {index}"
        );
        ensure!(
            mip_count > 0,
            "invalid BA2 DX10 mip count in record {index}"
        );
        let format = bytes[cursor + 21];
        let is_cubemap = u16_at(bytes, cursor + 22)? & 1 != 0;
        cursor = base_end;
        let chunks_end = cursor
            .checked_add(
                chunk_count
                    .checked_mul(DX10_CHUNK_SIZE)
                    .ok_or_else(|| eyre!("BA2 DX10 chunk table overflow"))?,
            )
            .ok_or_else(|| eyre!("BA2 DX10 chunk table overflow"))?;
        ensure!(
            chunks_end <= header.names_offset && chunks_end <= bytes.len(),
            "truncated BA2 DX10 chunks in record {index}"
        );

        let mut chunks = Vec::with_capacity(chunk_count);
        for chunk_index in 0..chunk_count {
            let base = cursor + chunk_index * DX10_CHUNK_SIZE;
            let chunk = Dx10Chunk {
                offset: u64_at(bytes, base)?,
                packed_size: u32_at(bytes, base + 8)?,
                unpacked_size: u32_at(bytes, base + 12)?,
                start_mip: u16_at(bytes, base + 16)?,
                end_mip: u16_at(bytes, base + 18)?,
            };
            validate_sizes(chunk.packed_size, chunk.unpacked_size, "DX10 chunk")?;
            ensure!(
                chunk.unpacked_size > 0,
                "empty BA2 DX10 chunk in record {index}, chunk {chunk_index}"
            );
            ensure!(
                chunk.start_mip <= chunk.end_mip && chunk.end_mip < mip_count as u16,
                "invalid BA2 DX10 mip range in record {index}, chunk {chunk_index}"
            );
            chunks.push(chunk);
        }
        cursor = chunks_end;
        records.push(Record::Dx10(Dx10Record {
            height,
            width,
            mip_count,
            format,
            is_cubemap,
            chunks,
        }));
    }
    Ok(records)
}

fn parse_names(bytes: &[u8], mut cursor: usize, count: usize) -> Result<Vec<String>> {
    let mut names = Vec::with_capacity(count);
    for index in 0..count {
        let length = u16_at(bytes, cursor)? as usize;
        cursor = cursor
            .checked_add(2)
            .ok_or_else(|| eyre!("BA2 filename offset overflow"))?;
        ensure!(length > 0, "empty BA2 filename at index {index}");
        let end = cursor
            .checked_add(length)
            .ok_or_else(|| eyre!("BA2 filename range overflow"))?;
        let name = bytes
            .get(cursor..end)
            .ok_or_else(|| eyre!("truncated BA2 filename at index {index}"))?;
        let name = std::str::from_utf8(name)
            .wrap_err_with(|| format!("invalid UTF-8 in BA2 filename at index {index}"))?
            .trim_end_matches('\0')
            .to_string();
        ensure!(!name.is_empty(), "empty BA2 filename at index {index}");
        names.push(name);
        cursor = end;
    }
    Ok(names)
}

fn validate_sizes(packed_size: u32, unpacked_size: u32, context: &str) -> Result<()> {
    ensure!(
        unpacked_size as usize <= MAX_ENTRY_SIZE,
        "BA2 {context} exceeds unpacked size safety limit"
    );
    ensure!(
        packed_size as usize <= MAX_ENTRY_SIZE,
        "BA2 {context} exceeds packed size safety limit"
    );
    Ok(())
}

fn extract_chunk(
    bytes: &[u8],
    offset: u64,
    packed_size: u32,
    unpacked_size: u32,
    compression: Compression,
    name: &str,
) -> Result<Vec<u8>> {
    let stored_size = if packed_size == 0 {
        unpacked_size
    } else {
        packed_size
    } as usize;
    let start = usize::try_from(offset).map_err(|_| eyre!("BA2 offset does not fit usize"))?;
    let end = start
        .checked_add(stored_size)
        .ok_or_else(|| eyre!("BA2 payload range overflow: {name}"))?;
    let payload = bytes
        .get(start..end)
        .ok_or_else(|| eyre!("BA2 payload out of bounds: {name}"))?;
    if packed_size == 0 {
        return Ok(payload.to_vec());
    }
    decompress(payload, unpacked_size as usize, compression)
        .wrap_err_with(|| format!("failed to decompress BA2 payload: {name}"))
}

/// The largest unpacked size an entry with this packed length can honestly
/// declare: no compressed stream expands past its format's own ratio.
fn expansion_limit(packed_len: usize, compression: Compression) -> usize {
    let ratio = match compression {
        Compression::Zlib => MAX_ZLIB_EXPANSION,
        Compression::Lz4 => MAX_LZ4_EXPANSION,
    };
    packed_len
        .saturating_mul(ratio)
        .saturating_add(DECOMPRESSION_SLACK)
        .min(MAX_ENTRY_SIZE)
}

fn decompress(payload: &[u8], expected_size: usize, compression: Compression) -> Result<Vec<u8>> {
    let limit = expansion_limit(payload.len(), compression);
    ensure!(
        expected_size <= limit,
        "BA2 {compression:?} entry declares {expected_size} unpacked bytes from a {} byte payload, over the {limit} byte safety limit",
        payload.len()
    );
    match compression {
        Compression::Lz4 => {
            let decoded = lz4_flex::block::decompress(payload, expected_size)
                .map_err(|error| eyre!("invalid LZ4 stream: {error}"))?;
            ensure!(
                decoded.len() == expected_size,
                "LZ4 decompressed size mismatch"
            );
            Ok(decoded)
        }
        Compression::Zlib => {
            let zlib_error = match decode_zlib(payload, expected_size) {
                Ok(decoded) => return Ok(decoded),
                Err(error) => error,
            };
            decode_deflate(payload, expected_size).map_err(|deflate_error| {
                eyre!(
                    "invalid zlib/Deflate stream (zlib: {zlib_error}; raw Deflate: {deflate_error})"
                )
            })
        }
    }
}

fn decode_zlib(payload: &[u8], expected_size: usize) -> Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(payload);
    let decoded = read_bounded(&mut decoder, expected_size)?;
    ensure!(
        decoder.total_in() == payload.len() as u64,
        "zlib stream has trailing data"
    );
    Ok(decoded)
}

fn decode_deflate(payload: &[u8], expected_size: usize) -> Result<Vec<u8>> {
    let mut decoder = DeflateDecoder::new(payload);
    let decoded = read_bounded(&mut decoder, expected_size)?;
    ensure!(
        decoder.total_in() == payload.len() as u64,
        "Deflate stream has trailing data"
    );
    Ok(decoded)
}

fn read_bounded(reader: &mut impl Read, expected_size: usize) -> Result<Vec<u8>> {
    let limit = u64::try_from(expected_size)
        .map_err(|_| eyre!("BA2 decompressed size does not fit u64"))?
        .checked_add(1)
        .ok_or_else(|| eyre!("BA2 decompressed size overflow"))?;
    let mut decoded = Vec::with_capacity(expected_size.min(RESERVE_CHUNK));
    reader.take(limit).read_to_end(&mut decoded)?;
    ensure!(
        decoded.len() == expected_size,
        "decompressed size mismatch: expected {expected_size}, got {}",
        decoded.len()
    );
    Ok(decoded)
}

fn extract_dx10(
    bytes: &[u8],
    record: Dx10Record,
    compression: Compression,
    name: &str,
) -> Result<Vec<u8>> {
    let total_size = record.chunks.iter().try_fold(0usize, |total, chunk| {
        total
            .checked_add(chunk.unpacked_size as usize)
            .ok_or_else(|| eyre!("BA2 DX10 expanded size overflow: {name}"))
    })?;
    ensure!(
        total_size <= MAX_ENTRY_SIZE,
        "BA2 DX10 texture exceeds expanded size safety limit: {name}"
    );
    let mut pixels = Vec::with_capacity(total_size.min(RESERVE_CHUNK));
    for chunk in &record.chunks {
        pixels.extend(extract_chunk(
            bytes,
            chunk.offset,
            chunk.packed_size,
            chunk.unpacked_size,
            compression,
            name,
        )?);
    }
    let mut dds = build_dds_header(&record);
    dds.extend_from_slice(&pixels);
    Ok(dds)
}

fn build_dds_header(record: &Dx10Record) -> Vec<u8> {
    const DDSD_MIPMAPCOUNT: u32 = 0x20_000;
    const DDSCAPS_COMPLEX: u32 = 0x8;
    const DDSCAPS_TEXTURE: u32 = 0x1000;
    const DDSCAPS_MIPMAP: u32 = 0x40_0000;
    const DDSCAPS2_CUBEMAP_ALL_FACES: u32 = 0xFE00;
    let has_mips = record.mip_count > 1;
    let mut header = Vec::with_capacity(148);
    push_u32(&mut header, u32::from_le_bytes(*b"DDS "));
    push_u32(&mut header, 124);
    push_u32(
        &mut header,
        0x1 | 0x2 | 0x4 | 0x1000 | if has_mips { DDSD_MIPMAPCOUNT } else { 0 },
    );
    push_u32(&mut header, record.height as u32);
    push_u32(&mut header, record.width as u32);
    push_u32(&mut header, 0); // pitch is inferred from the DXGI format
    push_u32(&mut header, 0);
    push_u32(&mut header, record.mip_count as u32);
    for _ in 0..11 {
        push_u32(&mut header, 0);
    }
    push_u32(&mut header, 32);
    push_u32(&mut header, 0x4); // DDPF_FOURCC
    push_u32(&mut header, u32::from_le_bytes(*b"DX10"));
    for _ in 0..5 {
        push_u32(&mut header, 0);
    }
    push_u32(
        &mut header,
        DDSCAPS_TEXTURE
            | if has_mips || record.is_cubemap {
                DDSCAPS_COMPLEX
            } else {
                0
            }
            | if has_mips { DDSCAPS_MIPMAP } else { 0 },
    );
    push_u32(
        &mut header,
        if record.is_cubemap {
            DDSCAPS2_CUBEMAP_ALL_FACES
        } else {
            0
        },
    );
    for _ in 0..3 {
        push_u32(&mut header, 0);
    }
    push_u32(&mut header, record.format as u32);
    push_u32(&mut header, 3); // D3D10_RESOURCE_DIMENSION_TEXTURE2D
    push_u32(&mut header, if record.is_cubemap { 0x4 } else { 0 });
    push_u32(&mut header, 1);
    push_u32(&mut header, 0);
    debug_assert_eq!(header.len(), 148);
    header
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    let end = offset
        .checked_add(2)
        .ok_or_else(|| eyre!("u16 offset overflow"))?;
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..end)
            .ok_or_else(|| eyre!("truncated u16 at {offset}"))?
            .try_into()
            .unwrap(),
    ))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| eyre!("u32 offset overflow"))?;
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..end)
            .ok_or_else(|| eyre!("truncated u32 at {offset}"))?
            .try_into()
            .unwrap(),
    ))
}

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| eyre!("u64 offset overflow"))?;
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..end)
            .ok_or_else(|| eyre!("truncated u64 at {offset}"))?
            .try_into()
            .unwrap(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_strategies::{arbitrary_bytes, config, corrupted};
    use flate2::{
        Compression as FlateCompression,
        write::{DeflateEncoder, ZlibEncoder},
    };
    use proptest::prelude::*;
    use std::io::Write;

    fn base_archive(version: u32, kind: &[u8; 4], count: u32, names_offset: usize) -> Vec<u8> {
        let header_size = match version {
            2 => 32,
            3 => 36,
            _ => 24,
        };
        let mut bytes = vec![0; header_size];
        bytes[..4].copy_from_slice(b"BTDX");
        bytes[4..8].copy_from_slice(&version.to_le_bytes());
        bytes[8..12].copy_from_slice(kind);
        bytes[12..16].copy_from_slice(&count.to_le_bytes());
        bytes[16..24].copy_from_slice(&(names_offset as u64).to_le_bytes());
        bytes
    }

    fn general_archive(packed: bool, raw_deflate: bool) -> Vec<u8> {
        let data = b"fixture data";
        let name = b"meshes/test.nif";
        let names_offset = 24 + GENERAL_RECORD_SIZE;
        let payload = if packed {
            let mut encoder: Box<dyn WriteFinish> = if raw_deflate {
                Box::new(DeflateEncoder::new(Vec::new(), FlateCompression::default()))
            } else {
                Box::new(ZlibEncoder::new(Vec::new(), FlateCompression::default()))
            };
            encoder.write_all(data).unwrap();
            encoder.finish()
        } else {
            data.to_vec()
        };
        let payload_offset = names_offset + 2 + name.len();
        let mut bytes = base_archive(1, b"GNRL", 1, names_offset);
        bytes.resize(payload_offset, 0);
        bytes[40..48].copy_from_slice(&(payload_offset as u64).to_le_bytes());
        bytes[48..52]
            .copy_from_slice(&(if packed { payload.len() as u32 } else { 0 }).to_le_bytes());
        bytes[52..56].copy_from_slice(&(data.len() as u32).to_le_bytes());
        bytes[names_offset..names_offset + 2].copy_from_slice(&(name.len() as u16).to_le_bytes());
        bytes[names_offset + 2..payload_offset].copy_from_slice(name);
        bytes.extend_from_slice(&payload);
        bytes
    }

    trait WriteFinish: Write {
        fn finish(self: Box<Self>) -> Vec<u8>;
    }
    impl WriteFinish for ZlibEncoder<Vec<u8>> {
        fn finish(self: Box<Self>) -> Vec<u8> {
            (*self).finish().unwrap()
        }
    }
    impl WriteFinish for DeflateEncoder<Vec<u8>> {
        fn finish(self: Box<Self>) -> Vec<u8> {
            (*self).finish().unwrap()
        }
    }

    #[test]
    fn generated_archives_never_panic_under_truncation_or_mutation() {
        let fixtures = [
            dummy_content::ba2::general(
                &[
                    dummy_content::Entry::new("textures/one.dds", b"DDS DATA"),
                    dummy_content::Entry::new("meshes/two.nif", b"NIF DATA"),
                ],
                dummy_content::ba2::Compression::None,
            )
            .unwrap(),
            dummy_content::ba2::general(
                &[dummy_content::Entry::new("textures/one.dds", b"DDS DATA")],
                dummy_content::ba2::Compression::Zlib,
            )
            .unwrap(),
            dummy_content::ba2::dx10(&[dummy_content::ba2::Dx10Texture::new(
                "textures/dx10.dds",
                4,
                4,
                71,
                &[0xAB; 8],
            )])
            .unwrap(),
        ];
        let mut rng = dummy_content::rng::Rng::new(43);
        for fixture in fixtures {
            for length in 0..fixture.len() {
                let result = std::panic::catch_unwind(|| read_entries(&fixture[..length]));
                assert!(result.is_ok(), "BA2 parser panicked at length {length}");
            }
            for _ in 0..256 {
                let mut mutated = fixture.clone();
                let index = rng.next_u64() as usize % mutated.len();
                mutated[index] ^= 0xff;
                let result = std::panic::catch_unwind(|| read_entries(&mutated));
                assert!(result.is_ok(), "BA2 parser panicked on mutation at {index}");
            }
        }
    }

    #[test]
    fn parses_versioned_headers_and_compression() {
        assert_eq!(
            parse_header(&base_archive(1, b"GNRL", 0, 24))
                .unwrap()
                .records_offset,
            24
        );
        let mut v3 = base_archive(3, b"DX10", 0, 36);
        v3[32..36].copy_from_slice(&3u32.to_le_bytes());
        let header = parse_header(&v3).unwrap();
        assert_eq!(header.records_offset, 36);
        assert_eq!(header.compression, Compression::Lz4);
    }

    #[test]
    fn extracts_general_entries_and_deflate_fallback() {
        for (packed, raw) in [(false, false), (true, false), (true, true)] {
            let entries = read_entries(&general_archive(packed, raw)).unwrap();
            assert_eq!(
                entries,
                vec![("meshes/test.nif".into(), b"fixture data".to_vec())]
            );
        }
    }

    #[test]
    fn rejects_decompression_size_mismatch_and_bad_offset() {
        let mut bytes = general_archive(true, false);
        bytes[52..56].copy_from_slice(&99u32.to_le_bytes());
        assert!(read_entries(&bytes).is_err());
        let mut bytes = general_archive(false, false);
        bytes[40..48].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(read_entries(&bytes).is_err());
    }

    #[test]
    fn extracts_dx10_chunks_as_dds() {
        let name = b"textures/test.dds";
        let names_offset = 24 + DX10_RECORD_SIZE + DX10_CHUNK_SIZE;
        let payload_offset = names_offset + 2 + name.len();
        let pixels = [0xAB; 8];
        let mut bytes = base_archive(1, b"DX10", 1, names_offset);
        bytes.resize(payload_offset, 0);
        bytes[37] = 1;
        bytes[38..40].copy_from_slice(&(DX10_CHUNK_SIZE as u16).to_le_bytes());
        bytes[40..42].copy_from_slice(&4u16.to_le_bytes());
        bytes[42..44].copy_from_slice(&4u16.to_le_bytes());
        bytes[44] = 1;
        bytes[45] = 71;
        bytes[48..56].copy_from_slice(&(payload_offset as u64).to_le_bytes());
        bytes[60..64].copy_from_slice(&(pixels.len() as u32).to_le_bytes());
        bytes[names_offset..names_offset + 2].copy_from_slice(&(name.len() as u16).to_le_bytes());
        bytes[names_offset + 2..payload_offset].copy_from_slice(name);
        bytes.extend_from_slice(&pixels);
        let entries = read_entries(&bytes).unwrap();
        assert_eq!(&entries[0].1[..4], b"DDS ");
        assert_eq!(entries[0].1.len(), 148 + pixels.len());
        assert_eq!(u32_at(&entries[0].1, 128).unwrap(), 71);
        assert_eq!(&entries[0].1[148..], pixels);
        let parsed = ddsfile::Dds::read(entries[0].1.as_slice()).unwrap();
        assert_eq!(parsed.get_width(), 4);
        assert_eq!(parsed.get_height(), 4);
        assert_eq!(parsed.data, pixels);
    }

    /// A v1 GNRL archive holding one entry whose stored payload is `payload`
    /// and whose record declares `unpacked_size` bytes.
    fn archive_with_declared_size(payload: &[u8], unpacked_size: usize) -> Vec<u8> {
        let name = b"meshes/test.nif";
        let names_offset = 24 + GENERAL_RECORD_SIZE;
        let payload_offset = names_offset + 2 + name.len();
        let mut bytes = base_archive(1, b"GNRL", 1, names_offset);
        bytes.resize(payload_offset, 0);
        bytes[40..48].copy_from_slice(&(payload_offset as u64).to_le_bytes());
        bytes[48..52].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes[52..56].copy_from_slice(&(unpacked_size as u32).to_le_bytes());
        bytes[names_offset..names_offset + 2].copy_from_slice(&(name.len() as u16).to_le_bytes());
        bytes[names_offset + 2..payload_offset].copy_from_slice(name);
        bytes.extend_from_slice(payload);
        bytes
    }

    /// The same, as a v3 archive whose entries are LZ4 blocks.
    fn lz4_archive_with_declared_size(payload: &[u8], unpacked_size: usize) -> Vec<u8> {
        let name = b"textures/test.dds";
        let records_offset = 36;
        let names_offset = records_offset + GENERAL_RECORD_SIZE;
        let payload_offset = names_offset + 2 + name.len();
        let mut bytes = vec![0; records_offset];
        bytes[..4].copy_from_slice(b"BTDX");
        bytes[4..8].copy_from_slice(&3u32.to_le_bytes());
        bytes[8..12].copy_from_slice(b"GNRL");
        bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
        bytes[16..24].copy_from_slice(&(names_offset as u64).to_le_bytes());
        bytes[32..36].copy_from_slice(&3u32.to_le_bytes());
        bytes.resize(payload_offset, 0);
        let base = records_offset;
        bytes[base + 16..base + 24].copy_from_slice(&(payload_offset as u64).to_le_bytes());
        bytes[base + 24..base + 28].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes[base + 28..base + 32].copy_from_slice(&(unpacked_size as u32).to_le_bytes());
        bytes[names_offset..names_offset + 2].copy_from_slice(&(name.len() as u16).to_le_bytes());
        bytes[names_offset + 2..payload_offset].copy_from_slice(name);
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn rejects_a_declared_unpacked_size_the_zlib_payload_cannot_hold() {
        let mut encoder = ZlibEncoder::new(Vec::new(), FlateCompression::default());
        encoder.write_all(b"tiny payload").unwrap();
        let payload = encoder.finish().unwrap();

        let archive = archive_with_declared_size(&payload, 1024 * 1024 * 1024);
        let error = read_entries(&archive).unwrap_err();
        let chain = format!("{error:#}");
        assert!(chain.contains("safety limit"), "{chain}");
    }

    #[test]
    fn rejects_a_declared_unpacked_size_the_lz4_payload_cannot_hold() {
        let payload = lz4_flex::block::compress(b"tiny block");

        let archive = lz4_archive_with_declared_size(&payload, 1024 * 1024 * 1024);
        let error = read_entries(&archive).unwrap_err();
        let chain = format!("{error:#}");
        assert!(chain.contains("safety limit"), "{chain}");
    }

    #[test]
    fn accepts_an_lz4_block_compressed_at_the_format_worst_case_ratio() {
        let source = vec![0u8; 1024 * 1024];
        let payload = lz4_flex::block::compress(&source);
        // A run of zeros is the best LZ4 can do, so the fixture has to sit at
        // the format's own expansion limit for the test to mean anything.
        assert!(
            payload.len() * 255 + 4096 >= source.len(),
            "fixture is not at the LZ4 limit: {} bytes for {}",
            payload.len(),
            source.len()
        );

        let entries =
            read_entries(&lz4_archive_with_declared_size(&payload, source.len())).unwrap();
        assert_eq!(entries[0].1, source);
    }

    #[test]
    fn decompresses_lz4_blocks_for_version_three() {
        let expected = b"modern BA2 texture stream";
        let packed = lz4_flex::block::compress(expected);
        assert_eq!(
            decompress(&packed, expected.len(), Compression::Lz4).unwrap(),
            expected
        );
    }

    fn fixtures() -> Vec<Vec<u8>> {
        vec![
            dummy_content::ba2::general(
                &[
                    dummy_content::Entry::new("textures/one.dds", b"DDS DATA"),
                    dummy_content::Entry::new("meshes/two.nif", b"NIF DATA"),
                ],
                dummy_content::ba2::Compression::Zlib,
            )
            .unwrap(),
            dummy_content::ba2::dx10(&[dummy_content::ba2::Dx10Texture::new(
                "textures/dx10.dds",
                4,
                4,
                71,
                &[0xAB; 8],
            )])
            .unwrap(),
            general_archive(true, true),
        ]
    }

    proptest! {
        #![proptest_config(config(256))]

        #[test]
        fn headers_never_panic_on_arbitrary_bytes(
            version in prop_oneof![Just(1u32), Just(2), Just(3), Just(7), Just(8), any::<u32>()],
            kind in prop::sample::select(vec![*b"GNRL", *b"DX10"]),
            tail in arbitrary_bytes(512),
        ) {
            let mut bytes = b"BTDX".to_vec();
            bytes.extend_from_slice(&version.to_le_bytes());
            bytes.extend_from_slice(&kind);
            bytes.extend_from_slice(&tail);
            let _ = read_entries(&bytes);
        }

        #[test]
        fn corrupted_archives_never_panic(
            bytes in prop::sample::select(fixtures()).prop_flat_map(corrupted),
        ) {
            let _ = read_entries(&bytes);
        }
    }
}

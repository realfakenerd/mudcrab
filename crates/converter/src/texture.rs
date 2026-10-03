use color_eyre::{
    Result,
    eyre::{WrapErr, ensure},
};
use ddsfile::{Caps2, D3DFormat, Dds, DxgiFormat, MiscFlag, PixelFormatFlags};
use memmap2::Mmap;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    ffi::c_void,
    fs::{self, File},
    io::Cursor,
    path::Path,
    sync::Once,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextureSemantic {
    BaseColor,
    Normal,
    Emissive,
    MetallicRoughness,
    Occlusion,
    SpecularGlossiness,
    Height,
    Detail,
    EnvironmentCube,
    EnvironmentMask,
    InnerLayer,
    Greyscale,
    Unclassified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextureEncoding {
    ColorSrgb,
    NormalLinear,
    DataLinear,
}

impl TextureEncoding {
    pub fn from_semantics(semantics: &BTreeSet<TextureSemantic>) -> Result<Self> {
        let color = semantics.iter().any(|semantic| {
            matches!(
                semantic,
                TextureSemantic::BaseColor
                    | TextureSemantic::SpecularGlossiness
                    | TextureSemantic::Detail
                    | TextureSemantic::EnvironmentCube
            )
        });
        let emissive = semantics.contains(&TextureSemantic::Emissive);
        let normal = semantics.contains(&TextureSemantic::Normal);
        let data = semantics.iter().any(|semantic| {
            matches!(
                semantic,
                TextureSemantic::MetallicRoughness
                    | TextureSemantic::Occlusion
                    | TextureSemantic::Height
                    | TextureSemantic::EnvironmentMask
                    | TextureSemantic::InnerLayer
                    | TextureSemantic::Greyscale
            )
        });
        Ok(if normal {
            // A shared normal must stay linear and use the normal-map encoder;
            // sampling it as sRGB would corrupt its direction vectors.
            Self::NormalLinear
        } else if data {
            // Bethesda reuses some color/emissive images as masks or height
            // data. Linear encoding preserves those channel values.
            Self::DataLinear
        } else if color || emissive {
            Self::ColorSrgb
        } else {
            Self::DataLinear
        })
    }

    const fn is_srgb(self) -> bool {
        matches!(self, Self::ColorSrgb)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ktx2Metadata {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub layers: u32,
    pub faces: u32,
    pub levels: u32,
    pub encoding: TextureEncoding,
    pub format: String,
    pub supercompression: String,
    pub encoded_bytes: u64,
    pub expanded_rgba_bytes: u64,
    pub sha256: String,
}

const KTX2_IDENTIFIER: &[u8; 12] = b"\xABKTX 20\xBB\r\n\x1A\n";
const FLAG_KTX2: u32 = 1 << 11;
const FLAG_SRGB: u32 = 1 << 13;
const FLAG_GENERATE_MIPS_CLAMP: u32 = 1 << 14;
const FLAG_UASTC: u32 = 1 << 17;
pub(crate) const UASTC_LEVEL_DEFAULT: u8 = 2;
pub(crate) const ETC1S_QUALITY_DEFAULT: u8 = 192;
const ZSTD_LEVEL_DEFAULT: i32 = 6;
static BASIS_INIT: Once = Once::new();
type EncodedVolumeLevels = Vec<Vec<Vec<u8>>>;
type EncodedVolume = (EncodedVolumeLevels, Vec<u8>);

unsafe extern "C" {
    fn opensky_basis_compress_ktx2(
        rgba: *const u8,
        width: u32,
        height: u32,
        flags_and_quality: u32,
        uastc_rdo_quality: f32,
        size: *mut usize,
    ) -> *mut c_void;
    fn opensky_basis_free(data: *mut c_void);
    fn opensky_basis_quiet_stdout();
}

pub struct TextureConverter;

impl TextureConverter {
    pub fn convert_dds_to_ktx2(
        input: &Path,
        output: &Path,
        encoding: TextureEncoding,
    ) -> Result<Ktx2Metadata> {
        Self::convert_dds_to_ktx2_with_options(
            input,
            output,
            encoding,
            ETC1S_QUALITY_DEFAULT,
            UASTC_LEVEL_DEFAULT,
            ZSTD_LEVEL_DEFAULT,
        )
    }

    pub fn convert_dds_to_ktx2_with_options(
        input: &Path,
        output: &Path,
        encoding: TextureEncoding,
        etc1s_quality: u8,
        uastc_level: u8,
        zstd_level: i32,
    ) -> Result<Ktx2Metadata> {
        let file =
            File::open(input).wrap_err_with(|| format!("failed to open {}", input.display()))?;
        let mmap = unsafe { Mmap::map(&file) }
            .wrap_err_with(|| format!("failed to memory-map {}", input.display()))?;

        let ktx2 =
            Self::convert_with_options(&mmap, encoding, etc1s_quality, uastc_level, zstd_level)?;
        let metadata = inspect_ktx2(&ktx2, encoding)?;
        publish_ktx2_file(output, &ktx2)?;
        Ok(metadata)
    }

    pub fn convert(dds_bytes: &[u8], encoding: TextureEncoding) -> Result<Vec<u8>> {
        Self::convert_with_options(
            dds_bytes,
            encoding,
            ETC1S_QUALITY_DEFAULT,
            UASTC_LEVEL_DEFAULT,
            ZSTD_LEVEL_DEFAULT,
        )
    }

    /// Converts without supercompression, for callers that assert on raw
    /// level bytes or target runtimes without Zstandard support.
    pub fn convert_uncompressed(dds_bytes: &[u8], encoding: TextureEncoding) -> Result<Vec<u8>> {
        Self::convert_with_options(
            dds_bytes,
            encoding,
            ETC1S_QUALITY_DEFAULT,
            UASTC_LEVEL_DEFAULT,
            0,
        )
    }

    pub fn convert_with_options(
        dds_bytes: &[u8],
        encoding: TextureEncoding,
        etc1s_quality: u8,
        uastc_level: u8,
        zstd_level: i32,
    ) -> Result<Vec<u8>> {
        let dds = Dds::read(Cursor::new(dds_bytes)).wrap_err("invalid DDS")?;
        let depth = dds.get_depth();
        let is_cubemap = dds.header.caps2.contains(Caps2::CUBEMAP)
            || dds
                .header10
                .as_ref()
                .is_some_and(|header| header.misc_flag.contains(MiscFlag::TEXTURECUBE));
        let layer_count = dds.get_num_array_layers();
        ensure!(
            layer_count <= 1 || (is_cubemap && layer_count == 6),
            "DDS texture arrays are not supported"
        );
        if let Some(format) = native_ktx2_format(&dds, encoding) {
            let result = assemble_native_ktx2(&dds, format, is_cubemap, zstd_level)?;
            validate_ktx2_against_dds(&result, &dds, encoding, is_cubemap)?;
            return Ok(result);
        }
        // Uncompressed 8-bit-per-channel colour skips the UASTC encoder (the slow
        // path): its decoded mips are block-compressed on the CPU to a native BC
        // format (see `compress_packed_levels`), which also keeps GPU memory at a
        // quarter of RGBA8 for BC7 and an eighth for BC1. Cube maps and volumes of
        // these layouts are rare and still fall back, as do L8, 16-bit and
        // palettes. If anything in the packed path fails (odd pitch, size
        // mismatch, oversize), the texture is not failed: it continues to the
        // generic decoder and UASTC below, as on main. X8R8G8B8, which
        // `image_dds` cannot decode, keeps main's dedicated decoder in that
        // fallback. `packed_failure` keeps the packed reason so a later failure
        // can chain it.
        let mut packed_failure = None;
        if !is_cubemap
            && depth <= 1
            && layer_count <= 1
            && let Some(layout) = packed_rgba8_layout(&dds)
        {
            match convert_packed_to_native(&dds, layout, encoding, zstd_level) {
                Ok(result) => return Ok(result),
                Err(error) => packed_failure = Some(format!("{error:#}")),
            }
        }
        ensure!(
            !is_cubemap || layer_count == 6,
            "DDS cubemap does not contain exactly six faces"
        );
        if depth > 1 {
            ensure!(!is_cubemap, "DDS cannot be both a volume and a cubemap");
            ensure!(layer_count <= 1, "volume DDS arrays are not supported");
            let (encoded_levels, template) = match image_dds::SurfaceRgba8::decode_dds(&dds) {
                Ok(surface) => {
                    encode_decoded_volume(&surface, encoding, etc1s_quality, uastc_level)?
                }
                Err(_) if is_l8_volume(&dds) => {
                    encode_l8_volume(&dds, encoding, etc1s_quality, uastc_level)?
                }
                Err(error) => return Err(error).wrap_err("DDS volume cannot be decoded"),
            };
            let result = combine_ktx2_volume(
                &template,
                &encoded_levels,
                dds.get_width(),
                dds.get_height(),
                depth,
            )?;
            let result = supercompress_ktx2_levels(&result, zstd_level)?;
            validate_ktx2_against_dds(&result, &dds, encoding, false)?;
            return Ok(result);
        }
        if is_cubemap {
            let mut encoded_faces = Vec::with_capacity(6);
            for face in 0..6 {
                let surface = image_dds::SurfaceRgba8::decode_layers_mipmaps_dds(
                    &dds,
                    face..face + 1,
                    0..dds.get_num_mipmap_levels(),
                )
                .wrap_err_with(|| format!("DDS cubemap face {face} cannot be decoded"))?;
                encoded_faces.push(encode_2d_surface(
                    &surface,
                    encoding,
                    etc1s_quality,
                    uastc_level,
                )?);
            }
            let result = combine_ktx2_cubemap_faces(&encoded_faces)?;
            let result = supercompress_ktx2_levels(&result, zstd_level)?;
            validate_ktx2_against_dds(&result, &dds, encoding, true)?;
            return Ok(result);
        }

        // Reused by whichever fallback failure follows, so the report shows that
        // the packed path was attempted first and why it refused the texture.
        let packed_reason = packed_failure
            .as_deref()
            .map(|reason| format!(" (packed path failed first: {reason})"))
            .unwrap_or_default();
        let result = match image_dds::SurfaceRgba8::decode_dds(&dds) {
            Ok(surface) => encode_2d_surface(&surface, encoding, etc1s_quality, uastc_level)?,
            // `image_dds` has no X8R8G8B8 decoder. Main read its mips directly
            // (tight pitch, trailing payload bytes ignored) and stored UASTC.
            Err(_) if dds.get_d3d_format() == Some(D3DFormat::X8R8G8B8) => {
                let context = format!("X8R8G8B8 DDS cannot be decoded{packed_reason}");
                encode_x8r8g8b8(&dds, encoding, etc1s_quality, uastc_level).wrap_err(context)?
            }
            Err(error) => {
                let context = format!("DDS pixel format cannot be decoded{packed_reason}");
                return Err(error).wrap_err(context);
            }
        };
        let result = supercompress_ktx2_levels(&result, zstd_level)?;
        validate_ktx2_against_dds(&result, &dds, encoding, false)?;
        Ok(result)
    }
}

/// Writes `ktx2` to `output` through a temporary file, so a crash never
/// leaves a half-written texture there. An existing output is kept as a
/// backup until the new one is in place and restored if publication fails.
pub(crate) fn publish_ktx2_file(output: &Path, ktx2: &[u8]) -> Result<()> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = output.with_extension(format!("ktx2.{}.partial", std::process::id()));
    let backup = output.with_extension(format!("ktx2.{}.backup", std::process::id()));
    ensure!(
        !temporary.exists() && !backup.exists(),
        "stale texture publication file exists beside {}",
        output.display()
    );
    fs::write(&temporary, ktx2)
        .wrap_err_with(|| format!("failed to write {}", temporary.display()))?;
    let publish = (|| {
        let had_previous = output.is_file();
        if had_previous {
            fs::rename(output, &backup).wrap_err_with(|| {
                format!("failed to stage replacement for {}", output.display())
            })?;
        }
        if let Err(error) = fs::rename(&temporary, output) {
            if had_previous {
                let _ = fs::rename(&backup, output);
            }
            return Err(error).wrap_err_with(|| format!("failed to publish {}", output.display()));
        }
        if had_previous {
            fs::remove_file(&backup).wrap_err_with(|| {
                format!("failed to remove publication backup {}", backup.display())
            })?;
        }
        Ok(())
    })();
    if publish.is_err() {
        let _ = fs::remove_file(&temporary);
        if backup.is_file() && !output.is_file() {
            let _ = fs::rename(&backup, output);
        }
    }
    publish
}

/// Whether the converter copies this DDS's blocks instead of encoding them.
/// Only the remaining textures are worth sending to the GPU encoder.
pub(crate) fn preserves_native_blocks(dds: &Dds, encoding: TextureEncoding) -> bool {
    native_ktx2_format(dds, encoding).is_some()
}

/// Maps a DDS to its native KTX2 `VkFormat` when the source blocks can be
/// preserved byte-for-byte. Returns `None` for formats that must still go
/// through the UASTC path (uncompressed sources, legacy packed pixels).
///
/// sRGB vs linear comes from the material-slot `encoding`, never the file:
/// FourCC BC sources carry no color-space marker, and the DXGI sRGB spellings
/// describe the same blocks as their UNORM twins.
fn native_ktx2_format(dds: &Dds, encoding: TextureEncoding) -> Option<ktx2::Format> {
    use DxgiFormat as Dx;
    use ktx2::Format as Vk;
    let srgb = encoding.is_srgb();
    if let Some(dxgi) = dds.get_dxgi_format() {
        return Some(match dxgi {
            Dx::BC1_Typeless | Dx::BC1_UNorm | Dx::BC1_UNorm_sRGB if !srgb => {
                Vk::BC1_RGBA_UNORM_BLOCK
            }
            Dx::BC1_Typeless | Dx::BC1_UNorm | Dx::BC1_UNorm_sRGB => Vk::BC1_RGBA_SRGB_BLOCK,
            Dx::BC2_Typeless | Dx::BC2_UNorm | Dx::BC2_UNorm_sRGB if !srgb => Vk::BC2_UNORM_BLOCK,
            Dx::BC2_Typeless | Dx::BC2_UNorm | Dx::BC2_UNorm_sRGB => Vk::BC2_SRGB_BLOCK,
            Dx::BC3_Typeless | Dx::BC3_UNorm | Dx::BC3_UNorm_sRGB if !srgb => Vk::BC3_UNORM_BLOCK,
            Dx::BC3_Typeless | Dx::BC3_UNorm | Dx::BC3_UNorm_sRGB => Vk::BC3_SRGB_BLOCK,
            // BC4/BC5/BC6H have no sRGB VkFormat variant; an sRGB slot
            // falls back to UASTC, which honors the slot transfer function.
            Dx::BC4_Typeless | Dx::BC4_UNorm if !srgb => Vk::BC4_UNORM_BLOCK,
            Dx::BC4_SNorm if !srgb => Vk::BC4_SNORM_BLOCK,
            Dx::BC5_Typeless | Dx::BC5_UNorm if !srgb => Vk::BC5_UNORM_BLOCK,
            Dx::BC5_SNorm if !srgb => Vk::BC5_SNORM_BLOCK,
            Dx::BC6H_Typeless | Dx::BC6H_UF16 if !srgb => Vk::BC6H_UFLOAT_BLOCK,
            Dx::BC6H_SF16 if !srgb => Vk::BC6H_SFLOAT_BLOCK,
            Dx::BC7_Typeless | Dx::BC7_UNorm | Dx::BC7_UNorm_sRGB if !srgb => Vk::BC7_UNORM_BLOCK,
            Dx::BC7_Typeless | Dx::BC7_UNorm | Dx::BC7_UNorm_sRGB => Vk::BC7_SRGB_BLOCK,
            Dx::R8G8B8A8_Typeless | Dx::R8G8B8A8_UNorm | Dx::R8G8B8A8_UNorm_sRGB if !srgb => {
                Vk::R8G8B8A8_UNORM
            }
            Dx::R8G8B8A8_Typeless | Dx::R8G8B8A8_UNorm | Dx::R8G8B8A8_UNorm_sRGB => {
                Vk::R8G8B8A8_SRGB
            }
            Dx::R8_UNorm if !srgb => Vk::R8_UNORM,
            _ => return None,
        });
    }
    match dds.get_d3d_format() {
        Some(D3DFormat::DXT1) if !srgb => Some(Vk::BC1_RGBA_UNORM_BLOCK),
        Some(D3DFormat::DXT1) => Some(Vk::BC1_RGBA_SRGB_BLOCK),
        Some(D3DFormat::DXT2 | D3DFormat::DXT3) if !srgb => Some(Vk::BC2_UNORM_BLOCK),
        Some(D3DFormat::DXT2 | D3DFormat::DXT3) => Some(Vk::BC2_SRGB_BLOCK),
        Some(D3DFormat::DXT4 | D3DFormat::DXT5) if !srgb => Some(Vk::BC3_UNORM_BLOCK),
        Some(D3DFormat::DXT4 | D3DFormat::DXT5) => Some(Vk::BC3_SRGB_BLOCK),
        _ => None,
    }
}

/// Assembles a KTX2 container around the DDS payload without re-encoding:
/// header, generated DFD, level index, then each mip level's bytes copied
/// verbatim (faces concatenated per level for cubemaps, slices per level
/// for volumes). DDS stores one face's full mip chain contiguously, so
/// cubemap levels gather one slice from each face.
fn assemble_native_ktx2(
    dds: &Dds,
    format: ktx2::Format,
    is_cubemap: bool,
    zstd_level: i32,
) -> Result<Vec<u8>> {
    let max_levels = max_mip_levels(dds.get_width(), dds.get_height(), dds.get_depth()) as usize;
    let mip_count = dds.get_num_mipmap_levels().max(1) as usize;
    ensure!(
        mip_count <= max_levels,
        "DDS declares {mip_count} mip levels, but its {}x{}x{} dimensions allow at most {max_levels}",
        dds.get_width(),
        dds.get_height(),
        dds.get_depth()
    );
    let depth = dds.get_depth().max(1);
    let faces = if is_cubemap { 6u32 } else { 1 };
    let block_bytes = block_byte_size(dds)?;
    let mut face_stride = 0usize;
    for mip in 0..mip_count {
        face_stride = face_stride
            .checked_add(native_mip_byte_size(dds, mip, block_bytes)?)
            .ok_or_else(|| color_eyre::eyre::eyre!("DDS payload size overflow"))?;
    }
    let required = face_stride
        .checked_mul(faces as usize)
        .ok_or_else(|| color_eyre::eyre::eyre!("DDS payload size overflow"))?;
    ensure!(
        dds.data.len() >= required,
        "DDS payload is truncated: {} bytes, expected at least {}",
        dds.data.len(),
        required
    );

    let mut levels = Vec::with_capacity(mip_count);
    let mut mip_offset_in_face = 0usize;
    for mip in 0..mip_count {
        let mip_len = native_mip_byte_size(dds, mip, block_bytes)?;
        let mut level_data = Vec::with_capacity(mip_len * faces as usize);
        for face in 0..faces as usize {
            let start = face * face_stride + mip_offset_in_face;
            level_data.extend_from_slice(&dds.data[start..start + mip_len]);
        }
        levels.push(level_data);
        mip_offset_in_face += mip_len;
    }
    write_native_ktx2(
        format,
        dds.get_width(),
        dds.get_height(),
        depth,
        faces,
        &levels,
        zstd_level,
    )
}

/// Writes a native KTX2 container around finished level bytes (faces or slices
/// already concatenated per level). Each level is Zstandard compressed whole
/// when `zstd_level > 0`.
fn write_native_ktx2(
    format: ktx2::Format,
    width: u32,
    height: u32,
    depth: u32,
    faces: u32,
    levels: &[Vec<u8>],
    zstd_level: i32,
) -> Result<Vec<u8>> {
    let mip_count = levels.len();
    let (dfd, type_size) = ktx2::dfd::Basic::from_format(format)
        .map_err(|error| color_eyre::eyre::eyre!("no KTX2 descriptor for {format:?}: {error:?}"))?;
    let dfd_bytes = ktx2::dfd::Block::Basic(dfd).to_vec();
    let level_count = u32::try_from(mip_count).wrap_err("too many DDS mip levels")?;
    let header = ktx2::Header {
        format: Some(format),
        type_size,
        pixel_width: width,
        pixel_height: height,
        pixel_depth: if depth > 1 { depth } else { 0 },
        layer_count: 0,
        face_count: faces,
        level_count,
        supercompression_scheme: None,
        index: ktx2::Index {
            dfd_byte_offset: 0,
            dfd_byte_length: 0,
            kvd_byte_offset: 0,
            kvd_byte_length: 0,
            sgd_byte_offset: 0,
            sgd_byte_length: 0,
        },
    };

    let level_table_end = ktx2::Header::LENGTH + mip_count * ktx2::LevelIndex::LENGTH;
    let dfd_offset = align_up(level_table_end, 4);
    let mut output = vec![0u8; dfd_offset];
    output.extend_from_slice(&(dfd_bytes.len() as u32 + 4).to_le_bytes());
    output.extend_from_slice(&dfd_bytes);

    let mut indexes = Vec::with_capacity(mip_count);
    for level_data in levels {
        let uncompressed = level_data.len() as u64;
        // Supercompression applies to the complete assembled level (all of
        // its faces/slices), never to faces independently: independently
        // compressed faces would differ in length and break level assembly.
        let stored = compress_level(level_data, zstd_level)?;
        while !output.len().is_multiple_of(16) {
            output.push(0);
        }
        let offset = output.len() as u64;
        output.extend_from_slice(&stored);
        indexes.push(ktx2::LevelIndex {
            byte_offset: offset,
            byte_length: stored.len() as u64,
            uncompressed_byte_length: uncompressed,
        });
    }

    let mut header = header;
    if zstd_level > 0 {
        header.supercompression_scheme = Some(ktx2::SupercompressionScheme::Zstandard);
    }
    header.index.dfd_byte_offset = dfd_offset as u32;
    header.index.dfd_byte_length = dfd_bytes.len() as u32 + 4;
    output[..ktx2::Header::LENGTH].copy_from_slice(&header.as_bytes());
    for (level, index) in indexes.iter().enumerate() {
        let start = ktx2::Header::LENGTH + level * ktx2::LevelIndex::LENGTH;
        output[start..start + ktx2::LevelIndex::LENGTH].copy_from_slice(&index.as_bytes());
    }
    Ok(output)
}

/// Compresses one complete KTX2 mip level with Zstandard, or returns
/// the input unchanged when `level` is 0 (supercompression off).
fn compress_level(level: &[u8], zstd_level: i32) -> Result<Vec<u8>> {
    if zstd_level <= 0 {
        return Ok(level.to_vec());
    }
    zstd::stream::encode_all(level, zstd_level).wrap_err("Zstandard supercompression failed")
}

/// Rewrites a finished uncompressed KTX2 so each mip level is Zstandard
/// compressed: header scheme set, level index rewritten with compressed
/// and uncompressed lengths. Used by the UASTC fallback path, whose levels
/// assemble uncompressed; the native path compresses during assembly.
/// Levels that do not shrink are still stored compressed: the scheme is
/// per-file, and a conformant reader handles any per-level ratio.
pub(crate) fn supercompress_ktx2_levels(ktx2: &[u8], zstd_level: i32) -> Result<Vec<u8>> {
    if zstd_level <= 0 {
        return Ok(ktx2.to_vec());
    }
    let reader = ktx2::Reader::new(ktx2)
        .map_err(|error| color_eyre::eyre::eyre!("invalid KTX2 for supercompression: {error:?}"))?;
    let mut header = reader.header();
    ensure!(
        header.supercompression_scheme.is_none(),
        "KTX2 is already supercompressed"
    );
    let first_data_offset = reader
        .levels()
        .map(|level| level.data.as_ptr() as usize - ktx2.as_ptr() as usize)
        .min()
        .unwrap_or(ktx2.len());
    let mut output = ktx2[..first_data_offset].to_vec();
    let mut indexes = Vec::with_capacity(reader.levels().len());
    for level in reader.levels() {
        let stored = compress_level(level.data, zstd_level)?;
        while !output.len().is_multiple_of(16) {
            output.push(0);
        }
        let offset = output.len() as u64;
        output.extend_from_slice(&stored);
        indexes.push(ktx2::LevelIndex {
            byte_offset: offset,
            byte_length: stored.len() as u64,
            uncompressed_byte_length: level.data.len() as u64,
        });
    }
    header.supercompression_scheme = Some(ktx2::SupercompressionScheme::Zstandard);
    output[..ktx2::Header::LENGTH].copy_from_slice(&header.as_bytes());
    for (level, index) in indexes.iter().enumerate() {
        let start = ktx2::Header::LENGTH + level * ktx2::LevelIndex::LENGTH;
        output[start..start + ktx2::LevelIndex::LENGTH].copy_from_slice(&index.as_bytes());
    }
    Ok(output)
}

/// Block size in bytes for a preservable DDS: 8 for BC1/BC4, 16 for
/// BC2/BC3/BC5/BC6H/BC7, texel-row size for uncompressed RGBA8/R8.
fn block_byte_size(dds: &Dds) -> Result<usize> {
    if let Some(dxgi) = dds.get_dxgi_format() {
        use DxgiFormat as Dx;
        let size = match dxgi {
            Dx::BC1_Typeless | Dx::BC1_UNorm | Dx::BC1_UNorm_sRGB => 8,
            Dx::BC4_Typeless | Dx::BC4_UNorm | Dx::BC4_SNorm => 8,
            Dx::BC2_Typeless
            | Dx::BC2_UNorm
            | Dx::BC2_UNorm_sRGB
            | Dx::BC3_Typeless
            | Dx::BC3_UNorm
            | Dx::BC3_UNorm_sRGB
            | Dx::BC5_Typeless
            | Dx::BC5_UNorm
            | Dx::BC5_SNorm
            | Dx::BC6H_Typeless
            | Dx::BC6H_UF16
            | Dx::BC6H_SF16
            | Dx::BC7_Typeless
            | Dx::BC7_UNorm
            | Dx::BC7_UNorm_sRGB => 16,
            Dx::R8_UNorm => 1,
            Dx::R8G8B8A8_Typeless | Dx::R8G8B8A8_UNorm | Dx::R8G8B8A8_UNorm_sRGB => 4,
            other => color_eyre::eyre::bail!("DDS format {other:?} has no native KTX2 mapping"),
        };
        return Ok(size);
    }
    match dds.get_d3d_format() {
        Some(D3DFormat::DXT1) => Ok(8),
        Some(D3DFormat::DXT2 | D3DFormat::DXT3 | D3DFormat::DXT4 | D3DFormat::DXT5) => Ok(16),
        other => color_eyre::eyre::bail!("DDS format {other:?} has no native KTX2 mapping"),
    }
}

/// Byte size of one face's mip level: ceil-to-block texel coverage times
/// the block size, times depth slices for volumes.
fn native_mip_byte_size(dds: &Dds, mip: usize, block_bytes: usize) -> Result<usize> {
    let shift = u32::try_from(mip).wrap_err("DDS mip index does not fit in u32")?;
    let width = dds
        .get_width()
        .checked_shr(shift)
        .ok_or_else(|| color_eyre::eyre::eyre!("DDS mip {mip} exceeds dimensions"))?
        .max(1) as usize;
    let height = dds
        .get_height()
        .checked_shr(shift)
        .ok_or_else(|| color_eyre::eyre::eyre!("DDS mip {mip} exceeds dimensions"))?
        .max(1) as usize;
    let depth = dds
        .get_depth()
        .max(1)
        .checked_shr(shift)
        .ok_or_else(|| color_eyre::eyre::eyre!("DDS mip {mip} exceeds dimensions"))?
        .max(1) as usize;
    let blocks_wide = width.div_ceil(4);
    let blocks_high = height.div_ceil(4);
    let bytes_per_slice = if is_uncompressed_native(dds) {
        width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(block_bytes))
    } else {
        blocks_wide
            .checked_mul(blocks_high)
            .and_then(|blocks| blocks.checked_mul(block_bytes))
    }
    .ok_or_else(|| color_eyre::eyre::eyre!("DDS mip {mip} size overflow"))?;
    bytes_per_slice
        .checked_mul(depth)
        .ok_or_else(|| color_eyre::eyre::eyre!("DDS mip {mip} size overflow"))
}

fn is_uncompressed_native(dds: &Dds) -> bool {
    matches!(
        dds.get_dxgi_format(),
        Some(
            DxgiFormat::R8_UNorm
                | DxgiFormat::R8G8B8A8_Typeless
                | DxgiFormat::R8G8B8A8_UNorm
                | DxgiFormat::R8G8B8A8_UNorm_sRGB
        )
    )
}

fn align_up(value: usize, alignment: usize) -> usize {
    value.div_ceil(alignment) * alignment
}

pub(crate) fn encode_2d_surface(
    surface: &image_dds::SurfaceRgba8<Vec<u8>>,
    encoding: TextureEncoding,
    etc1s_quality: u8,
    uastc_level: u8,
) -> Result<Vec<u8>> {
    ensure!(
        surface.layers == 1 && surface.depth == 1,
        "2D texture surface has an incompatible layout"
    );
    let mut encoded_levels = Vec::with_capacity(surface.mipmaps as usize);
    for mip in 0..surface.mipmaps {
        let image = surface
            .get_image(0, 0, mip)
            .ok_or_else(|| color_eyre::eyre::eyre!("DDS mip {mip} has invalid dimensions"))?;
        encoded_levels.push(encode_basis_ktx2(
            image.width(),
            image.height(),
            &image.into_raw(),
            encoding,
            false,
            etc1s_quality,
            uastc_level,
        )?);
    }
    if encoded_levels.len() == 1 {
        return Ok(encoded_levels.pop().expect("one encoded mip"));
    }
    let base = surface
        .get_image(0, 0, 0)
        .ok_or_else(|| color_eyre::eyre::eyre!("DDS has no base mip"))?;
    let template = encode_basis_ktx2(
        base.width(),
        base.height(),
        &base.into_raw(),
        encoding,
        true,
        etc1s_quality,
        uastc_level,
    )?;
    combine_ktx2_mip_levels(&template, &encoded_levels)
}

fn combine_ktx2_mip_levels(template: &[u8], levels: &[Vec<u8>]) -> Result<Vec<u8>> {
    ensure!(!levels.is_empty(), "KTX2 mip chain is empty");
    let template_reader = ktx2::Reader::new(template)
        .map_err(|error| color_eyre::eyre::eyre!("invalid KTX2 mip template: {error:?}"))?;
    let mut reference = template_reader.header();
    ensure!(
        reference.level_count as usize >= levels.len(),
        "generated KTX2 mip template has only {} levels, expected at least {}",
        reference.level_count,
        levels.len()
    );
    let level_count = levels.len();
    let first_data_offset = template_reader
        .levels()
        .enumerate()
        .map(|(level, _)| {
            let start = ktx2::Header::LENGTH + level * ktx2::LevelIndex::LENGTH;
            let bytes: &[u8; ktx2::LevelIndex::LENGTH] = template
                [start..start + ktx2::LevelIndex::LENGTH]
                .try_into()
                .expect("fixed-size KTX2 level index");
            ktx2::LevelIndex::from_bytes(bytes).byte_offset as usize
        })
        .min()
        .ok_or_else(|| color_eyre::eyre::eyre!("KTX2 mip template has no levels"))?;
    let mut output = template[..first_data_offset].to_vec();
    reference.level_count = u32::try_from(level_count).wrap_err("too many DDS mip levels")?;
    output[..ktx2::Header::LENGTH].copy_from_slice(&reference.as_bytes());
    let mut indexes = Vec::with_capacity(level_count);
    for (mip, level) in levels.iter().enumerate() {
        let reader = ktx2::Reader::new(level)
            .map_err(|error| color_eyre::eyre::eyre!("invalid encoded DDS mip {mip}: {error:?}"))?;
        let header = reader.header();
        ensure!(
            header.level_count == 1
                && header.face_count == 1
                && header.layer_count == 0
                && header.pixel_depth == 0
                && header.pixel_width == (reference.pixel_width >> mip).max(1)
                && header.pixel_height == (reference.pixel_height >> mip).max(1)
                && header.format == reference.format
                && header.supercompression_scheme == reference.supercompression_scheme,
            "encoded DDS mip {mip} has an incompatible layout"
        );
        while !output.len().is_multiple_of(16) {
            output.push(0);
        }
        let offset = output.len() as u64;
        let encoded = reader
            .levels()
            .next()
            .ok_or_else(|| color_eyre::eyre::eyre!("encoded DDS mip {mip} has no data"))?;
        output.extend_from_slice(encoded.data);
        indexes.push(ktx2::LevelIndex {
            byte_offset: offset,
            byte_length: encoded.data.len() as u64,
            uncompressed_byte_length: encoded.uncompressed_byte_length,
        });
    }
    for (level, index) in indexes.iter().enumerate() {
        let start = ktx2::Header::LENGTH + level * ktx2::LevelIndex::LENGTH;
        output[start..start + ktx2::LevelIndex::LENGTH].copy_from_slice(&index.as_bytes());
    }
    Ok(output)
}

pub fn inspect_ktx2(bytes: &[u8], encoding: TextureEncoding) -> Result<Ktx2Metadata> {
    validate_ktx2(bytes, encoding)?;
    let reader = ktx2::Reader::new(bytes)
        .map_err(|error| color_eyre::eyre::eyre!("generated invalid KTX2: {error:?}"))?;
    let header = reader.header();
    let levels = header.level_count.max(1);
    ensure!(
        reader.levels().count() == levels as usize,
        "KTX2 level index is incomplete"
    );
    let faces = header.face_count.max(1);
    let layers = header.layer_count.max(1);
    let base_depth = header.pixel_depth.max(1);
    let mut expanded_rgba_bytes = 0u64;
    for mip in 0..levels {
        let width = (header.pixel_width >> mip).max(1) as u64;
        let height = (header.pixel_height.max(1) >> mip).max(1) as u64;
        let depth = (base_depth >> mip).max(1) as u64;
        let level_bytes = width
            .checked_mul(height)
            .and_then(|value| value.checked_mul(depth))
            .and_then(|value| value.checked_mul(u64::from(faces)))
            .and_then(|value| value.checked_mul(u64::from(layers)))
            .and_then(|value| value.checked_mul(4))
            .ok_or_else(|| color_eyre::eyre::eyre!("KTX2 expanded size overflow"))?;
        expanded_rgba_bytes = expanded_rgba_bytes
            .checked_add(level_bytes)
            .ok_or_else(|| color_eyre::eyre::eyre!("KTX2 expanded size overflow"))?;
    }
    Ok(Ktx2Metadata {
        width: header.pixel_width,
        height: header.pixel_height.max(1),
        depth: base_depth,
        layers,
        faces,
        levels,
        encoding,
        format: format!("{:?}", reader.color_model()),
        supercompression: format!("{:?}", header.supercompression_scheme),
        encoded_bytes: u64::try_from(bytes.len()).wrap_err("KTX2 size does not fit in u64")?,
        expanded_rgba_bytes,
        sha256: crate::cache::hash_bytes(bytes),
    })
}

/// Validates an already-published runtime KTX2 when its source material
/// semantic is unavailable. Linear textures are reported as data textures;
/// normal/data distinction does not change the runtime container contract.
pub fn inspect_runtime_ktx2(bytes: &[u8]) -> Result<Ktx2Metadata> {
    let reader = ktx2::Reader::new(bytes)
        .map_err(|error| color_eyre::eyre::eyre!("invalid runtime KTX2: {error:?}"))?;
    let encoding = match reader.transfer_function() {
        Some(ktx2::TransferFunction::SRGB) => TextureEncoding::ColorSrgb,
        Some(ktx2::TransferFunction::Linear) => TextureEncoding::DataLinear,
        transfer => color_eyre::eyre::bail!("KTX2 has unsupported transfer function {transfer:?}"),
    };
    inspect_ktx2(bytes, encoding)
}

fn validate_ktx2_against_dds(
    bytes: &[u8],
    dds: &Dds,
    encoding: TextureEncoding,
    cubemap: bool,
) -> Result<Ktx2Metadata> {
    let metadata = inspect_ktx2(bytes, encoding)?;
    ensure!(
        metadata.width == dds.get_width()
            && metadata.height == dds.get_height()
            && metadata.depth == dds.get_depth().max(1),
        "KTX2 dimensions {:?} do not match DDS {}x{}x{}",
        (metadata.width, metadata.height, metadata.depth),
        dds.get_width(),
        dds.get_height(),
        dds.get_depth().max(1)
    );
    ensure!(
        metadata.levels == dds.get_num_mipmap_levels().max(1),
        "KTX2 has {} levels, but DDS has {}",
        metadata.levels,
        dds.get_num_mipmap_levels().max(1)
    );
    ensure!(
        metadata.faces == if cubemap { 6 } else { 1 },
        "KTX2 face count does not match DDS"
    );
    ensure!(metadata.layers == 1, "KTX2 arrays are not supported");
    Ok(metadata)
}

fn encode_decoded_volume(
    surface: &image_dds::SurfaceRgba8<Vec<u8>>,
    encoding: TextureEncoding,
    etc1s_quality: u8,
    uastc_level: u8,
) -> Result<EncodedVolume> {
    let mut encoded_levels = Vec::with_capacity(surface.mipmaps as usize);
    for mip in 0..surface.mipmaps {
        let mip_depth = (surface.depth >> mip).max(1);
        let mut encoded_slices = Vec::with_capacity(mip_depth as usize);
        for slice in 0..mip_depth {
            let image = surface.get_image(0, slice, mip).ok_or_else(|| {
                color_eyre::eyre::eyre!("DDS volume mip {mip} slice {slice} has invalid dimensions")
            })?;
            encoded_slices.push(encode_basis_ktx2(
                image.width(),
                image.height(),
                &image.into_raw(),
                encoding,
                false,
                etc1s_quality,
                uastc_level,
            )?);
        }
        encoded_levels.push(encoded_slices);
    }
    let base_image = surface
        .get_image(0, 0, 0)
        .ok_or_else(|| color_eyre::eyre::eyre!("DDS volume has no base slice"))?;
    let template = encode_basis_ktx2(
        base_image.width(),
        base_image.height(),
        &base_image.into_raw(),
        encoding,
        surface.mipmaps > 1,
        etc1s_quality,
        uastc_level,
    )?;
    Ok((encoded_levels, template))
}

fn is_l8_volume(dds: &Dds) -> bool {
    dds.header.spf.flags.contains(PixelFormatFlags::LUMINANCE)
        && dds.header.spf.rgb_bit_count == Some(8)
        && !dds.header.spf.flags.contains(PixelFormatFlags::ALPHA)
        && !dds
            .header
            .spf
            .flags
            .contains(PixelFormatFlags::ALPHA_PIXELS)
}

/// The longest mip chain a texture of these dimensions can have: one level per
/// halving until the longest edge is a single texel, and never more than 32.
///
/// Headers may claim any count they like; this bounds what the dimensions can
/// hold so that a hostile header cannot size a reservation.
pub(crate) fn max_mip_levels(width: u32, height: u32, depth: u32) -> u32 {
    let longest_edge = width.max(height).max(depth).max(1);
    u32::BITS - longest_edge.leading_zeros()
}

fn encode_l8_volume(
    dds: &Dds,
    encoding: TextureEncoding,
    etc1s_quality: u8,
    uastc_level: u8,
) -> Result<EncodedVolume> {
    // A header may declare zero mip levels; the base level is always there.
    let mip_count = dds.get_num_mipmap_levels().max(1);
    let max_levels = max_mip_levels(dds.get_width(), dds.get_height(), dds.get_depth());
    ensure!(
        mip_count <= max_levels,
        "L8 DDS volume declares {mip_count} mip levels, but its {}x{}x{} dimensions allow at most {max_levels}",
        dds.get_width(),
        dds.get_height(),
        dds.get_depth()
    );
    let mut offset = 0usize;
    let mut encoded_levels = Vec::with_capacity(mip_count as usize);
    let mut base_rgba = None;
    for mip in 0..mip_count {
        let width = (dds.get_width() >> mip).max(1);
        let height = (dds.get_height() >> mip).max(1);
        let depth = (dds.get_depth() >> mip).max(1);
        let slice_len = (width as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| color_eyre::eyre::eyre!("L8 DDS volume size overflow"))?;
        // The depth is the header's claim; the payload checks below reject a volume that does
        // not hold that many slices, so only a bounded reservation is made up front.
        let mut encoded_slices = Vec::with_capacity((depth as usize).min(256));
        for slice in 0..depth {
            let end = offset
                .checked_add(slice_len)
                .ok_or_else(|| color_eyre::eyre::eyre!("L8 DDS volume size overflow"))?;
            let luminance = dds.data.get(offset..end).ok_or_else(|| {
                color_eyre::eyre::eyre!("L8 DDS volume is truncated at mip {mip} slice {slice}")
            })?;
            let mut rgba = Vec::with_capacity(slice_len * 4);
            for &value in luminance {
                rgba.extend_from_slice(&[value, value, value, 255]);
            }
            if base_rgba.is_none() {
                base_rgba = Some(rgba.clone());
            }
            encoded_slices.push(encode_basis_ktx2(
                width,
                height,
                &rgba,
                encoding,
                false,
                etc1s_quality,
                uastc_level,
            )?);
            offset = end;
        }
        encoded_levels.push(encoded_slices);
    }
    ensure!(
        offset == dds.data.len(),
        "L8 DDS volume has {} unexpected trailing bytes",
        dds.data.len() - offset
    );
    let template = encode_basis_ktx2(
        dds.get_width(),
        dds.get_height(),
        &base_rgba.ok_or_else(|| color_eyre::eyre::eyre!("L8 DDS volume has no data"))?,
        encoding,
        mip_count > 1,
        etc1s_quality,
        uastc_level,
    )?;
    Ok((encoded_levels, template))
}

fn combine_ktx2_volume(
    template: &[u8],
    levels: &[Vec<Vec<u8>>],
    width: u32,
    height: u32,
    depth: u32,
) -> Result<Vec<u8>> {
    ensure!(
        width > 0 && height > 0 && depth > 1,
        "invalid KTX2 volume dimensions"
    );
    ensure!(!levels.is_empty(), "KTX2 volume has no levels");
    let template_reader = ktx2::Reader::new(template)
        .map_err(|error| color_eyre::eyre::eyre!("invalid KTX2 volume template: {error:?}"))?;
    let reference = template_reader.header();
    ensure!(
        reference.level_count as usize == levels.len(),
        "KTX2 volume template has {} levels, but the DDS has {}",
        reference.level_count,
        levels.len()
    );
    ensure!(
        reference.supercompression_scheme != Some(ktx2::SupercompressionScheme::BasisLZ),
        "BasisLZ volume assembly is not supported"
    );
    let level_count = levels.len();
    let level_table_end = ktx2::Header::LENGTH
        .checked_add(
            level_count
                .checked_mul(ktx2::LevelIndex::LENGTH)
                .ok_or_else(|| color_eyre::eyre::eyre!("KTX2 volume level table overflow"))?,
        )
        .ok_or_else(|| color_eyre::eyre::eyre!("KTX2 volume level table overflow"))?;
    let first_data_offset = (0..level_count)
        .map(|level| {
            let start = ktx2::Header::LENGTH + level * ktx2::LevelIndex::LENGTH;
            let bytes: &[u8; ktx2::LevelIndex::LENGTH] = template
                [start..start + ktx2::LevelIndex::LENGTH]
                .try_into()
                .expect("fixed-size KTX2 level index");
            ktx2::LevelIndex::from_bytes(bytes).byte_offset as usize
        })
        .min()
        .ok_or_else(|| color_eyre::eyre::eyre!("KTX2 volume template has no levels"))?;
    ensure!(
        first_data_offset >= level_table_end && first_data_offset <= template.len(),
        "invalid KTX2 volume metadata layout"
    );

    let mut output = template[..first_data_offset].to_vec();
    output[20..24].copy_from_slice(&width.to_le_bytes());
    output[24..28].copy_from_slice(&height.to_le_bytes());
    output[28..32].copy_from_slice(&depth.to_le_bytes());
    output[32..36].copy_from_slice(&0u32.to_le_bytes());
    output[36..40].copy_from_slice(&1u32.to_le_bytes());
    let mut indexes = Vec::with_capacity(level_count);
    for (mip, slices) in levels.iter().enumerate() {
        let expected_depth = (depth >> mip).max(1) as usize;
        ensure!(
            slices.len() == expected_depth,
            "KTX2 volume mip {mip} has {} slices, expected {expected_depth}",
            slices.len()
        );
        while !output.len().is_multiple_of(16) {
            output.push(0);
        }
        let offset = output.len() as u64;
        let mut uncompressed_length = 0u64;
        for slice in slices {
            let reader = ktx2::Reader::new(slice)
                .map_err(|error| color_eyre::eyre::eyre!("invalid KTX2 volume slice: {error:?}"))?;
            let header = reader.header();
            ensure!(
                header.level_count == 1
                    && header.pixel_depth == 0
                    && header.layer_count == 0
                    && header.face_count == 1
                    && header.format == reference.format
                    && header.supercompression_scheme == reference.supercompression_scheme,
                "KTX2 volume slice has incompatible layout"
            );
            let level = reader
                .levels()
                .next()
                .ok_or_else(|| color_eyre::eyre::eyre!("KTX2 volume slice has no level"))?;
            output.extend_from_slice(level.data);
            uncompressed_length = uncompressed_length
                .checked_add(level.uncompressed_byte_length)
                .ok_or_else(|| color_eyre::eyre::eyre!("KTX2 volume size overflow"))?;
        }
        indexes.push(ktx2::LevelIndex {
            byte_offset: offset,
            byte_length: output.len() as u64 - offset,
            uncompressed_byte_length: uncompressed_length,
        });
    }
    for (level, index) in indexes.iter().enumerate() {
        let start = ktx2::Header::LENGTH + level * ktx2::LevelIndex::LENGTH;
        output[start..start + ktx2::LevelIndex::LENGTH].copy_from_slice(&index.as_bytes());
    }
    Ok(output)
}

fn combine_ktx2_cubemap_faces(faces: &[Vec<u8>]) -> Result<Vec<u8>> {
    ensure!(
        faces.len() == 6,
        "a cubemap requires exactly six KTX2 faces"
    );
    let readers: Vec<_> = faces
        .iter()
        .map(|face| {
            ktx2::Reader::new(face)
                .map_err(|error| color_eyre::eyre::eyre!("invalid KTX2 cubemap face: {error:?}"))
        })
        .collect::<Result<_>>()?;
    let reference = readers[0].header();
    ensure!(reference.face_count == 1, "cubemap source face is not 2D");
    ensure!(
        reference.supercompression_scheme != Some(ktx2::SupercompressionScheme::BasisLZ),
        "BasisLZ cubemap assembly is not supported"
    );
    for (face, reader) in readers[1..].iter().enumerate() {
        let header = reader.header();
        ensure!(
            header.pixel_width == reference.pixel_width
                && header.pixel_height == reference.pixel_height
                && header.level_count == reference.level_count
                && header.format == reference.format
                && header.supercompression_scheme == reference.supercompression_scheme,
            "KTX2 cubemap face {} has incompatible layout: expected {reference:?}, got {header:?}",
            face + 1
        );
    }

    let level_count = reference.level_count.max(1) as usize;
    let level_table_end = ktx2::Header::LENGTH
        .checked_add(
            level_count
                .checked_mul(ktx2::LevelIndex::LENGTH)
                .ok_or_else(|| color_eyre::eyre::eyre!("KTX2 cubemap level table overflow"))?,
        )
        .ok_or_else(|| color_eyre::eyre::eyre!("KTX2 cubemap level table overflow"))?;
    let first_data_offset = (0..level_count)
        .map(|level| {
            let start = ktx2::Header::LENGTH + level * ktx2::LevelIndex::LENGTH;
            let bytes: &[u8; ktx2::LevelIndex::LENGTH] = faces[0]
                [start..start + ktx2::LevelIndex::LENGTH]
                .try_into()
                .expect("fixed-size KTX2 level index");
            ktx2::LevelIndex::from_bytes(bytes).byte_offset as usize
        })
        .min()
        .ok_or_else(|| color_eyre::eyre::eyre!("KTX2 cubemap has no levels"))?;
    ensure!(
        first_data_offset >= level_table_end && first_data_offset <= faces[0].len(),
        "invalid KTX2 cubemap metadata layout"
    );

    let face_levels: Vec<Vec<_>> = readers
        .iter()
        .map(|reader| reader.levels().collect())
        .collect();
    let mut output = faces[0][..first_data_offset].to_vec();
    output[36..40].copy_from_slice(&6u32.to_le_bytes());
    let mut indexes = Vec::with_capacity(level_count);
    for level in 0..level_count {
        while !output.len().is_multiple_of(16) {
            output.push(0);
        }
        let offset = output.len() as u64;
        let face_length = face_levels[0][level].data.len();
        let face_uncompressed = face_levels[0][level].uncompressed_byte_length;
        for levels in &face_levels {
            ensure!(
                levels[level].data.len() == face_length
                    && levels[level].uncompressed_byte_length == face_uncompressed,
                "KTX2 cubemap face levels have incompatible sizes"
            );
            output.extend_from_slice(levels[level].data);
        }
        indexes.push(ktx2::LevelIndex {
            byte_offset: offset,
            byte_length: (face_length * 6) as u64,
            uncompressed_byte_length: face_uncompressed.saturating_mul(6),
        });
    }
    for (level, index) in indexes.iter().enumerate() {
        let start = ktx2::Header::LENGTH + level * ktx2::LevelIndex::LENGTH;
        output[start..start + ktx2::LevelIndex::LENGTH].copy_from_slice(&index.as_bytes());
    }
    Ok(output)
}

/// Byte positions of the colour channels inside one packed 8-bit-per-channel
/// texel, for the uncompressed layouts the native path stores as RGBA8.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PackedRgba8 {
    bytes_per_pixel: usize,
    red: usize,
    green: usize,
    blue: usize,
    /// `None` when the layout has no alpha channel (X8R8G8B8, 24-bit): alpha 255.
    alpha: Option<usize>,
}

/// Recognises an uncompressed RGB(A) DDS whose channels are whole bytes
/// (24-bit B8G8R8, 32-bit X8R8G8B8, A8R8G8B8, A8B8G8R8, R8G8B8A8-as-bitmask,
/// DXGI B8G8R8A8/B8G8R8X8), so it maps exactly onto RGBA8. Anything else
/// (L8, 16-bit, palettes, FourCC) returns `None` and keeps the UASTC path.
fn packed_rgba8_layout(dds: &Dds) -> Option<PackedRgba8> {
    use DxgiFormat as Dx;
    let bgra = |alpha| PackedRgba8 {
        bytes_per_pixel: 4,
        red: 2,
        green: 1,
        blue: 0,
        alpha,
    };
    if dds.header10.is_some() {
        return match dds.get_dxgi_format()? {
            Dx::B8G8R8A8_Typeless | Dx::B8G8R8A8_UNorm | Dx::B8G8R8A8_UNorm_sRGB => {
                Some(bgra(Some(3)))
            }
            Dx::B8G8R8X8_Typeless | Dx::B8G8R8X8_UNorm | Dx::B8G8R8X8_UNorm_sRGB => {
                Some(bgra(None))
            }
            _ => None,
        };
    }
    let spf = &dds.header.spf;
    if !spf.flags.contains(PixelFormatFlags::RGB)
        || spf
            .flags
            .intersects(PixelFormatFlags::FOURCC | PixelFormatFlags::LUMINANCE)
    {
        return None;
    }
    let bytes_per_pixel = match spf.rgb_bit_count? {
        24 => 3,
        32 => 4,
        _ => return None,
    };
    let byte_of = |mask: u32| -> Option<usize> {
        let index = match mask {
            0xff => 0,
            0xff00 => 1,
            0xff_0000 => 2,
            0xff00_0000 => 3,
            _ => return None,
        };
        (index < bytes_per_pixel).then_some(index)
    };
    let red = byte_of(spf.r_bit_mask?)?;
    let green = byte_of(spf.g_bit_mask?)?;
    let blue = byte_of(spf.b_bit_mask?)?;
    // ddsfile only exposes the alpha mask when ALPHA_PIXELS or ALPHA is set, and
    // its own A8R8G8B8 detection accepts either, so both count here. A zero mask
    // has no alpha bits and is read as opaque.
    let alpha = match spf.a_bit_mask {
        Some(mask)
            if mask != 0
                && spf
                    .flags
                    .intersects(PixelFormatFlags::ALPHA_PIXELS | PixelFormatFlags::ALPHA) =>
        {
            Some(byte_of(mask)?)
        }
        _ => None,
    };
    let mut seen = [false; 4];
    for index in [Some(red), Some(green), Some(blue), alpha]
        .into_iter()
        .flatten()
    {
        if std::mem::replace(&mut seen[index], true) {
            return None;
        }
    }
    Some(PackedRgba8 {
        bytes_per_pixel,
        red,
        green,
        blue,
        alpha,
    })
}

/// Largest width or height the packed path block-compresses; bigger sources
/// fall back instead of risking a huge padded copy.
const PACKED_MAX_DIMENSION: u32 = 16384;

/// Largest total RGBA8 size the packed path allocates while decoding a mip
/// chain. A header may declare a texture far larger than its payload, so the
/// decoded size is bounded before any buffer is reserved.
const PACKED_MAX_OUTPUT_BYTES: usize = 256 * 1024 * 1024;

fn convert_packed_to_native(
    dds: &Dds,
    layout: PackedRgba8,
    encoding: TextureEncoding,
    zstd_level: i32,
) -> Result<Vec<u8>> {
    ensure!(
        dds.get_width() <= PACKED_MAX_DIMENSION && dds.get_height() <= PACKED_MAX_DIMENSION,
        "uncompressed DDS is larger than {PACKED_MAX_DIMENSION} texels on a side"
    );
    let levels = decode_packed_mips(dds, layout)?;
    let (format, level_bytes) = compress_packed_levels(&levels, layout, encoding)?;
    let result = write_native_ktx2(
        format,
        dds.get_width(),
        dds.get_height(),
        1,
        1,
        &level_bytes,
        zstd_level,
    )?;
    validate_ktx2_against_dds(&result, dds, encoding, false)?;
    Ok(result)
}

/// Block-compresses decoded RGBA8 mips with `intel_tex_2`. The format follows
/// the slot, not only the alpha channel:
/// - alpha layouts: BC7 (fast alpha profile);
/// - opaque layouts in an sRGB colour slot: BC1 (alpha 255);
/// - opaque layouts in a normal or data slot: BC7 with the fast opaque profile,
///   because BC1 fits one colour line and badly represents normals and
///   independent data channels.
///
/// Every mip is padded to whole 4x4 blocks by replicating its edge pixels, so
/// small and odd sized mips get the block counts the KTX2 level layout expects.
fn compress_packed_levels(
    levels: &[(u32, u32, Vec<u8>)],
    layout: PackedRgba8,
    encoding: TextureEncoding,
) -> Result<(ktx2::Format, Vec<Vec<u8>>)> {
    let srgb = encoding.is_srgb();
    let has_alpha = layout.alpha.is_some();
    let use_bc1 = !has_alpha && srgb;
    let format = match (use_bc1, srgb) {
        (true, _) => ktx2::Format::BC1_RGBA_SRGB_BLOCK,
        (false, false) => ktx2::Format::BC7_UNORM_BLOCK,
        (false, true) => ktx2::Format::BC7_SRGB_BLOCK,
    };
    let settings = if has_alpha {
        intel_tex_2::bc7::alpha_fast_settings()
    } else {
        intel_tex_2::bc7::opaque_fast_settings()
    };
    let overflow = || color_eyre::eyre::eyre!("packed texture size overflow");
    let mut compressed = Vec::with_capacity(levels.len());
    for (width, height, rgba) in levels {
        ensure!(
            *width <= PACKED_MAX_DIMENSION && *height <= PACKED_MAX_DIMENSION,
            "uncompressed DDS mip is larger than {PACKED_MAX_DIMENSION} texels on a side"
        );
        let padded_width = width.checked_next_multiple_of(4).ok_or_else(overflow)?;
        let padded_height = height.checked_next_multiple_of(4).ok_or_else(overflow)?;
        let (w, h) = (*width as usize, *height as usize);
        let (pw, ph) = (padded_width as usize, padded_height as usize);
        let stride = padded_width.checked_mul(4).ok_or_else(overflow)?;
        let padded_len = pw
            .checked_mul(ph)
            .and_then(|texels| texels.checked_mul(4))
            .ok_or_else(overflow)?;
        ensure!(
            w > 0 && h > 0 && rgba.len() == w * h * 4,
            "decoded mip has the wrong size"
        );
        let mut padded = Vec::with_capacity(padded_len);
        for y in 0..ph {
            let row = &rgba[y.min(h - 1) * w * 4..][..w * 4];
            padded.extend_from_slice(row);
            let edge = &row[(w - 1) * 4..];
            for _ in w..pw {
                padded.extend_from_slice(edge);
            }
        }
        let surface = intel_tex_2::RgbaSurface {
            data: &padded,
            width: padded_width,
            height: padded_height,
            stride,
        };
        compressed.push(if use_bc1 {
            intel_tex_2::bc1::compress_blocks(&surface)
        } else {
            intel_tex_2::bc7::compress_blocks(&settings, &surface)
        });
    }
    Ok((format, compressed))
}

fn decode_packed_mips(dds: &Dds, layout: PackedRgba8) -> Result<Vec<(u32, u32, Vec<u8>)>> {
    // A header may declare zero mip levels; the base level is always there.
    let mip_count = dds.get_num_mipmap_levels().max(1);
    let max_levels = max_mip_levels(dds.get_width(), dds.get_height(), 1);
    ensure!(
        mip_count <= max_levels,
        "uncompressed DDS declares {mip_count} mip levels, but its {}x{} dimensions allow at most {max_levels}",
        dds.get_width(),
        dds.get_height()
    );
    // Rows are either tight (`width * bpp`) or DWORD-aligned. Mip 0 header pitch
    // picks the convention, which then applies to every lower mip; any other
    // pitch is ambiguous and rejected.
    let row_bytes = |width: usize| -> Result<(usize, usize)> {
        let tight = width
            .checked_mul(layout.bytes_per_pixel)
            .ok_or_else(|| color_eyre::eyre::eyre!("DDS mip row size overflow"))?;
        let aligned = tight
            .checked_next_multiple_of(4)
            .ok_or_else(|| color_eyre::eyre::eyre!("DDS mip row size overflow"))?;
        Ok((tight, aligned))
    };
    let base_width =
        usize::try_from(dds.get_width()).wrap_err("DDS width does not fit in memory")?;
    let (base_tight, base_aligned) = row_bytes(base_width)?;
    // `Dds::get_pitch` recomputes a tight pitch for known formats; the raw
    // header field is the only place a DWORD-aligned writer says so.
    let header_pitch = match dds.header.pitch {
        Some(pitch) => usize::try_from(pitch).wrap_err("DDS pitch does not fit in memory")?,
        None => base_tight,
    };
    let aligned_rows = if header_pitch == base_tight && header_pitch == base_aligned {
        // Both conventions agree on mip 0 (a 24-bit width that is a multiple
        // of 4), so the header cannot tell them apart; they only differ in the
        // lower mips. The exact payload length picks the one that fits, and
        // anything else stays on the tight path to fail the checks below.
        let chain_bytes = |aligned: bool| -> Result<usize> {
            let mut total = 0usize;
            for mip in 0..mip_count {
                let width = usize::try_from((dds.get_width() >> mip).max(1))
                    .wrap_err("DDS width does not fit in memory")?;
                let height = usize::try_from((dds.get_height() >> mip).max(1))
                    .wrap_err("DDS height does not fit in memory")?;
                let (tight, padded) = row_bytes(width)?;
                total = total
                    .checked_add(
                        (if aligned { padded } else { tight })
                            .checked_mul(height)
                            .ok_or_else(|| color_eyre::eyre::eyre!("DDS payload size overflow"))?,
                    )
                    .ok_or_else(|| color_eyre::eyre::eyre!("DDS payload size overflow"))?;
            }
            Ok(total)
        };
        let tight_bytes = chain_bytes(false)?;
        dds.data.len() != tight_bytes && dds.data.len() == chain_bytes(true)?
    } else if header_pitch == base_tight {
        false
    } else if header_pitch == base_aligned {
        true
    } else {
        color_eyre::eyre::bail!(
            "uncompressed DDS pitch {header_pitch} is neither {base_tight} (tight) nor {base_aligned} (DWORD-aligned)"
        );
    };
    // Size the whole chain before allocating anything: a small file may claim a
    // huge texture, so the payload is checked against what the declared mips
    // need and the decoded RGBA8 is bounded before a single buffer is reserved.
    let mut plan = Vec::with_capacity(mip_count as usize);
    let mut required_bytes = 0usize;
    let mut rgba_bytes = 0usize;
    for mip in 0..mip_count {
        let width_u32 = (dds.get_width() >> mip).max(1);
        let height_u32 = (dds.get_height() >> mip).max(1);
        let width = usize::try_from(width_u32).wrap_err("DDS width does not fit in memory")?;
        let height = usize::try_from(height_u32).wrap_err("DDS height does not fit in memory")?;
        let (tight, aligned) = row_bytes(width)?;
        let source_pitch = if aligned_rows { aligned } else { tight };
        let level_bytes = source_pitch
            .checked_mul(height)
            .ok_or_else(|| color_eyre::eyre::eyre!("DDS payload size overflow"))?;
        required_bytes = required_bytes
            .checked_add(level_bytes)
            .ok_or_else(|| color_eyre::eyre::eyre!("DDS payload size overflow"))?;
        let level_rgba = width
            .checked_mul(height)
            .and_then(|texels| texels.checked_mul(4))
            .ok_or_else(|| color_eyre::eyre::eyre!("RGBA size overflow"))?;
        rgba_bytes = rgba_bytes
            .checked_add(level_rgba)
            .ok_or_else(|| color_eyre::eyre::eyre!("RGBA size overflow"))?;
        plan.push((width_u32, height_u32, width, height, source_pitch));
    }
    ensure!(
        dds.data.len() >= required_bytes,
        "uncompressed DDS is truncated: its mips need {required_bytes} bytes, but its payload is only {} bytes",
        dds.data.len()
    );
    ensure!(
        rgba_bytes <= PACKED_MAX_OUTPUT_BYTES,
        "uncompressed DDS would decode to {rgba_bytes} bytes of RGBA8, over the {PACKED_MAX_OUTPUT_BYTES} byte limit"
    );
    let mut offset = 0usize;
    let mut levels = Vec::with_capacity(plan.len());
    for (mip, level) in plan.into_iter().enumerate() {
        let (width_u32, height_u32, width, height, source_pitch) = level;
        let remaining = dds.data.get(offset..).ok_or_else(|| {
            color_eyre::eyre::eyre!("truncated uncompressed DDS before mip {mip}")
        })?;
        let (rgba, consumed) = decode_packed_level(remaining, width, height, source_pitch, layout)
            .wrap_err_with(|| format!("invalid uncompressed DDS mip {mip}"))?;
        offset = offset
            .checked_add(consumed)
            .ok_or_else(|| color_eyre::eyre::eyre!("DDS mip offset overflow"))?;
        levels.push((width_u32, height_u32, rgba));
    }
    ensure!(
        offset == dds.data.len(),
        "uncompressed DDS mips use {offset} bytes, but its payload is {} bytes",
        dds.data.len()
    );
    Ok(levels)
}

/// Decodes a packed 8-bit-per-channel DDS (tight or DWORD-aligned rows) to RGBA8
/// mips, for the GPU encoder's per-texel sources it cannot read in place.
pub(crate) fn decode_packed_rgba8_mips(dds: &Dds) -> Result<Vec<(u32, u32, Vec<u8>)>> {
    let layout = packed_rgba8_layout(dds)
        .ok_or_else(|| color_eyre::eyre::eyre!("DDS is not a packed 8-bit RGB(A) layout"))?;
    decode_packed_mips(dds, layout)
}

/// Main's X8R8G8B8 decoder, restored unchanged for the fallback: `image_dds`
/// refuses this format, and main ignored payload bytes after the last mip.
/// Also used by the GPU encoder, which uploads the RGBA8 mips it returns.
pub(crate) fn decode_x8r8g8b8_mips(dds: &Dds) -> Result<Vec<(u32, u32, Vec<u8>)>> {
    // A header may declare zero mip levels; the base level is always there.
    let mip_count = dds.get_num_mipmap_levels().max(1);
    let max_levels = max_mip_levels(dds.get_width(), dds.get_height(), 1);
    ensure!(
        mip_count <= max_levels,
        "X8R8G8B8 DDS declares {mip_count} mip levels, but its {}x{} dimensions allow at most {max_levels}",
        dds.get_width(),
        dds.get_height()
    );
    let mut offset = 0usize;
    let mut levels = Vec::with_capacity(mip_count as usize);
    for mip in 0..mip_count {
        let width_u32 = (dds.get_width() >> mip).max(1);
        let height_u32 = (dds.get_height() >> mip).max(1);
        let width = usize::try_from(width_u32).wrap_err("DDS width does not fit in memory")?;
        let height = usize::try_from(height_u32).wrap_err("DDS height does not fit in memory")?;
        let source_pitch = if mip == 0 {
            usize::try_from(
                dds.get_pitch()
                    .ok_or_else(|| color_eyre::eyre::eyre!("X8R8G8B8 DDS has no pitch"))?,
            )
            .wrap_err("DDS pitch does not fit in memory")?
        } else {
            width
                .checked_mul(4)
                .ok_or_else(|| color_eyre::eyre::eyre!("X8R8G8B8 DDS mip row size overflow"))?
        };
        let remaining = dds
            .data
            .get(offset..)
            .ok_or_else(|| color_eyre::eyre::eyre!("truncated X8R8G8B8 DDS before mip {mip}"))?;
        let (rgba, consumed) = decode_x8r8g8b8_level(remaining, width, height, source_pitch)
            .wrap_err_with(|| format!("invalid X8R8G8B8 DDS mip {mip}"))?;
        offset = offset
            .checked_add(consumed)
            .ok_or_else(|| color_eyre::eyre::eyre!("X8R8G8B8 DDS mip offset overflow"))?;
        levels.push((width_u32, height_u32, rgba));
    }
    Ok(levels)
}

fn decode_x8r8g8b8_level(
    data: &[u8],
    width: usize,
    height: usize,
    pitch: usize,
) -> Result<(Vec<u8>, usize)> {
    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| color_eyre::eyre::eyre!("X8R8G8B8 DDS row size overflow"))?;
    ensure!(
        pitch >= row_bytes,
        "X8R8G8B8 DDS pitch is smaller than a row"
    );
    let source_size = pitch
        .checked_mul(height)
        .ok_or_else(|| color_eyre::eyre::eyre!("X8R8G8B8 DDS payload size overflow"))?;
    ensure!(data.len() >= source_size, "truncated X8R8G8B8 DDS payload");
    let output_size = row_bytes
        .checked_mul(height)
        .ok_or_else(|| color_eyre::eyre::eyre!("X8R8G8B8 RGBA size overflow"))?;
    let mut rgba = Vec::with_capacity(output_size);
    for row in data[..source_size].chunks_exact(pitch) {
        let (pixels, remainder) = row[..row_bytes].as_chunks::<4>();
        debug_assert!(remainder.is_empty());
        for pixel in pixels {
            rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
    }
    Ok((rgba, source_size))
}

/// Encodes the X8R8G8B8 fallback with Basis UASTC, as main did: one encoded
/// level per decoded mip, assembled into a single KTX2 chain.
fn encode_x8r8g8b8(
    dds: &Dds,
    encoding: TextureEncoding,
    etc1s_quality: u8,
    uastc_level: u8,
) -> Result<Vec<u8>> {
    let levels = decode_x8r8g8b8_mips(dds)?;
    let mut encoded_levels = Vec::with_capacity(levels.len());
    for (width, height, rgba) in &levels {
        encoded_levels.push(encode_basis_ktx2(
            *width,
            *height,
            rgba,
            encoding,
            false,
            etc1s_quality,
            uastc_level,
        )?);
    }
    if encoded_levels.len() == 1 {
        return Ok(encoded_levels.pop().expect("one encoded mip"));
    }
    let (width, height, rgba) = &levels[0];
    let template = encode_basis_ktx2(
        *width,
        *height,
        rgba,
        encoding,
        true,
        etc1s_quality,
        uastc_level,
    )?;
    combine_ktx2_mip_levels(&template, &encoded_levels)
}

#[cfg(test)]
fn decode_x8r8g8b8(dds: &Dds) -> Result<Vec<u8>> {
    let layout = packed_rgba8_layout(dds).ok_or_else(|| color_eyre::eyre::eyre!("not packed"))?;
    decode_packed_mips(dds, layout)?
        .into_iter()
        .next()
        .map(|(_, _, rgba)| rgba)
        .ok_or_else(|| color_eyre::eyre::eyre!("X8R8G8B8 DDS has no mip levels"))
}

fn decode_packed_level(
    data: &[u8],
    width: usize,
    height: usize,
    pitch: usize,
    layout: PackedRgba8,
) -> Result<(Vec<u8>, usize)> {
    let row_bytes = width
        .checked_mul(layout.bytes_per_pixel)
        .ok_or_else(|| color_eyre::eyre::eyre!("DDS row size overflow"))?;
    ensure!(pitch >= row_bytes, "DDS pitch is smaller than a row");
    let source_size = pitch
        .checked_mul(height)
        .ok_or_else(|| color_eyre::eyre::eyre!("DDS payload size overflow"))?;
    ensure!(data.len() >= source_size, "truncated DDS payload");
    let output_size = width
        .checked_mul(4)
        .and_then(|row| row.checked_mul(height))
        .ok_or_else(|| color_eyre::eyre::eyre!("RGBA size overflow"))?;
    let mut rgba = Vec::with_capacity(output_size);
    for row in data[..source_size].chunks_exact(pitch) {
        for pixel in row[..row_bytes].chunks_exact(layout.bytes_per_pixel) {
            rgba.extend_from_slice(&[
                pixel[layout.red],
                pixel[layout.green],
                pixel[layout.blue],
                layout.alpha.map_or(255, |alpha| pixel[alpha]),
            ]);
        }
    }
    Ok((rgba, source_size))
}

fn encode_basis_ktx2(
    width: u32,
    height: u32,
    rgba: &[u8],
    encoding: TextureEncoding,
    generate_mips: bool,
    etc1s_quality: u8,
    uastc_level: u8,
) -> Result<Vec<u8>> {
    ensure!(
        width > 0 && height > 0,
        "texture dimensions must be non-zero"
    );
    ensure!(
        rgba.len() == width as usize * height as usize * 4,
        "RGBA payload size mismatch"
    );
    BASIS_INIT.call_once(|| {
        basis_universal::encoder_init();
        // Held so no Rust output is written while the bridge swaps the handles underneath it.
        use std::io::Write as _;
        let mut rust_stdout = std::io::stdout().lock();
        let _ = rust_stdout.flush();
        // SAFETY: points only the C library's stdout at the null device, once, before the encoder
        // first runs; the process's standard output, which Rust writes to, is kept.
        unsafe { opensky_basis_quiet_stdout() };
    });
    ensure!(etc1s_quality > 0, "ETC1S quality must be greater than zero");
    ensure!(uastc_level <= 4, "UASTC level must be between 0 and 4");
    let mut flags = FLAG_KTX2;
    if generate_mips {
        flags |= FLAG_GENERATE_MIPS_CLAMP;
    }
    // Bevy 0.19 can transcode UASTC payloads from KTX2, but its KTX2 loader
    // explicitly rejects the BasisLZ supercompression used by ETC1S. Keep the
    // offline/runtime contract compatible by emitting UASTC for every texture.
    flags |= FLAG_UASTC | u32::from(uastc_level);
    if encoding.is_srgb() {
        flags |= FLAG_SRGB;
    }
    let mut size = 0usize;
    // SAFETY: the encoder copies the complete RGBA slice during this call. The
    // returned allocation is owned by Basis and freed after copying below.
    let data =
        unsafe { opensky_basis_compress_ktx2(rgba.as_ptr(), width, height, flags, 0.0, &mut size) };
    ensure!(
        !data.is_null() && size > 0,
        "Basis Universal compression failed"
    );
    // SAFETY: a successful encoder call returns exactly `size` initialized bytes.
    let output = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size).to_vec() };
    // SAFETY: `data` was allocated by basis_compress and has not been freed yet.
    unsafe { opensky_basis_free(data) };
    Ok(output)
}

fn validate_ktx2(bytes: &[u8], encoding: TextureEncoding) -> Result<()> {
    ensure!(
        bytes.starts_with(KTX2_IDENTIFIER),
        "encoder did not produce KTX2"
    );
    let reader = ktx2::Reader::new(bytes)
        .map_err(|error| color_eyre::eyre::eyre!("generated invalid KTX2: {error:?}"))?;
    ensure!(
        reader.header().pixel_width > 0,
        "KTX2 has invalid dimensions"
    );
    ensure!(
        reader.header().supercompression_scheme != Some(ktx2::SupercompressionScheme::BasisLZ),
        "runtime-incompatible BasisLZ supercompression was emitted"
    );
    ensure!(
        reader.levels().next().is_some(),
        "KTX2 contains no image levels"
    );
    let expected_transfer = if encoding.is_srgb() {
        ktx2::TransferFunction::SRGB
    } else {
        ktx2::TransferFunction::Linear
    };
    ensure!(
        reader.transfer_function() == Some(expected_transfer),
        "KTX2 transfer function does not match {encoding:?}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use basis_universal::{
        DecodeFlags, LowLevelUastcTranscoder, SliceParametersUastc, TranscoderBlockFormat,
    };
    use ddsfile::{AlphaMode, D3D10ResourceDimension, DxgiFormat, NewD3dParams, NewDxgiParams};
    use std::io::{Read, Write};

    #[test]
    fn derives_encoding_from_slot_semantics_and_resolves_shared_textures() {
        assert_eq!(
            TextureEncoding::from_semantics(&BTreeSet::from([TextureSemantic::BaseColor])).unwrap(),
            TextureEncoding::ColorSrgb
        );
        assert_eq!(
            TextureEncoding::from_semantics(&BTreeSet::from([TextureSemantic::Normal])).unwrap(),
            TextureEncoding::NormalLinear
        );
        assert_eq!(
            TextureEncoding::from_semantics(&BTreeSet::from([TextureSemantic::Height])).unwrap(),
            TextureEncoding::DataLinear
        );
        assert_eq!(
            TextureEncoding::from_semantics(&BTreeSet::from([
                TextureSemantic::BaseColor,
                TextureSemantic::Normal,
            ]))
            .unwrap(),
            TextureEncoding::NormalLinear
        );
        assert_eq!(
            TextureEncoding::from_semantics(&BTreeSet::from([
                TextureSemantic::Emissive,
                TextureSemantic::Height,
            ]))
            .unwrap(),
            TextureEncoding::DataLinear
        );
        assert_eq!(
            TextureEncoding::from_semantics(&BTreeSet::from([
                TextureSemantic::BaseColor,
                TextureSemantic::EnvironmentMask,
            ]))
            .unwrap(),
            TextureEncoding::DataLinear
        );
    }

    #[test]
    fn creates_runtime_compatible_color_ktx2() {
        let pixels = [255, 0, 0, 255].repeat(16);
        let bytes =
            encode_basis_ktx2(4, 4, &pixels, TextureEncoding::ColorSrgb, false, 192, 2).unwrap();
        validate_ktx2(&bytes, TextureEncoding::ColorSrgb).unwrap();
        let reader = ktx2::Reader::new(&bytes).unwrap();
        assert_ne!(
            reader.header().supercompression_scheme,
            Some(ktx2::SupercompressionScheme::BasisLZ)
        );
    }

    #[test]
    fn basis_notices_stay_off_standard_output() {
        // The encoder's notices go to the C library's stdout, which only a fresh process shows.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "texture::tests::encode_and_print_for_the_standard_output_test",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(
            stdout.contains("printed by Rust after encoding"),
            "{stdout}\n{stderr}"
        );
        assert!(!stdout.contains("KTX2 validator bug"), "{stdout}\n{stderr}");
    }

    #[test]
    #[ignore = "run in a child process by basis_notices_stay_off_standard_output"]
    fn encode_and_print_for_the_standard_output_test() {
        for size in [4, 8, 16, 32] {
            let pixels = [90, 120, 60, 255].repeat(size * size);
            let size = size as u32;
            encode_basis_ktx2(
                size,
                size,
                &pixels,
                TextureEncoding::ColorSrgb,
                true,
                192,
                0,
            )
            .unwrap();
        }
        std::io::stdout()
            .write_all(b"printed by Rust after encoding\n")
            .unwrap();
    }

    #[test]
    fn creates_uastc_normal_map_ktx2() {
        let pixels = [128, 128, 255, 255].repeat(16);
        let bytes =
            encode_basis_ktx2(4, 4, &pixels, TextureEncoding::NormalLinear, false, 192, 2).unwrap();
        validate_ktx2(&bytes, TextureEncoding::NormalLinear).unwrap();
    }

    #[test]
    fn preserves_alpha_and_normal_channels_through_uastc_round_trip() {
        let alpha_pixels: Vec<u8> = (0..16)
            .flat_map(|index| [220, 60, 20, if index < 8 { 0 } else { 255 }])
            .collect();
        let alpha_ktx = encode_basis_ktx2(
            4,
            4,
            &alpha_pixels,
            TextureEncoding::ColorSrgb,
            false,
            192,
            2,
        )
        .unwrap();
        let alpha_round_trip = decode_uastc_level(&alpha_ktx, 0);
        let (alpha_pixels, remainder) = alpha_round_trip.as_chunks::<4>();
        assert!(remainder.is_empty());
        let alpha: Vec<_> = alpha_pixels.iter().map(|pixel| pixel[3]).collect();
        assert!(alpha.iter().filter(|value| **value < 64).count() >= 4);
        assert!(alpha.iter().filter(|value| **value > 191).count() >= 4);

        // Asymmetric tangent-space vectors catch swapped/inverted X/Y channels.
        let normal_pixels: Vec<u8> = (0..16)
            .flat_map(|index| {
                if index < 8 {
                    [224, 48, 196, 255]
                } else {
                    [40, 208, 180, 255]
                }
            })
            .collect();
        let normal_ktx = encode_basis_ktx2(
            4,
            4,
            &normal_pixels,
            TextureEncoding::NormalLinear,
            false,
            192,
            2,
        )
        .unwrap();
        let normal_round_trip = decode_uastc_level(&normal_ktx, 0);
        let first = &normal_round_trip[0..4];
        let last = &normal_round_trip[60..64];
        assert!(first[0] > first[1] && last[0] < last[1]);
        assert!(first[2] > 140 && last[2] > 140);
    }

    #[test]
    fn validates_metadata_hash_dimensions_levels_and_expanded_size() {
        let pixels = [30, 80, 160, 255].repeat(64);
        let bytes =
            encode_basis_ktx2(8, 8, &pixels, TextureEncoding::ColorSrgb, true, 192, 2).unwrap();
        let metadata = inspect_ktx2(&bytes, TextureEncoding::ColorSrgb).unwrap();
        assert_eq!(
            (metadata.width, metadata.height, metadata.levels),
            (8, 8, 4)
        );
        assert_eq!(metadata.expanded_rgba_bytes, (64 + 16 + 4 + 1) * 4);
        assert_eq!(metadata.encoded_bytes, bytes.len() as u64);
        assert_eq!(metadata.sha256, crate::cache::hash_bytes(&bytes));
        assert!(!metadata.format.is_empty() && !metadata.supercompression.is_empty());
    }

    #[test]
    fn inspects_published_runtime_ktx2_without_source_semantics() {
        let pixels = [30, 80, 160, 255].repeat(16);
        let color =
            encode_basis_ktx2(4, 4, &pixels, TextureEncoding::ColorSrgb, false, 192, 2).unwrap();
        let data =
            encode_basis_ktx2(4, 4, &pixels, TextureEncoding::DataLinear, false, 192, 2).unwrap();
        assert_eq!(
            inspect_runtime_ktx2(&color).unwrap().encoding,
            TextureEncoding::ColorSrgb
        );
        assert_eq!(
            inspect_runtime_ktx2(&data).unwrap().encoding,
            TextureEncoding::DataLinear
        );
        assert!(inspect_runtime_ktx2(b"truncated").is_err());
    }

    #[test]
    fn converts_bc1_through_bc7_reachable_variants() {
        let formats = [
            DxgiFormat::BC1_UNorm,
            DxgiFormat::BC2_UNorm,
            DxgiFormat::BC3_UNorm,
            DxgiFormat::BC4_UNorm,
            DxgiFormat::BC5_UNorm,
            DxgiFormat::BC6H_UF16,
            DxgiFormat::BC7_UNorm,
        ];
        for format in formats {
            let dds = Dds::new_dxgi(NewDxgiParams {
                height: 4,
                width: 4,
                depth: None,
                format,
                mipmap_levels: None,
                array_layers: None,
                caps2: None,
                is_cubemap: false,
                resource_dimension: D3D10ResourceDimension::Texture2D,
                alpha_mode: AlphaMode::Straight,
            })
            .unwrap();
            let mut bytes = Vec::new();
            dds.write(&mut bytes).unwrap();
            TextureConverter::convert(&bytes, TextureEncoding::DataLinear)
                .unwrap_or_else(|error| panic!("failed to convert {format:?}: {error:#}"));
        }
    }

    #[test]
    fn preserves_authored_dds_mip_levels_instead_of_regenerating_them() {
        let mut dds = Dds::new_dxgi(NewDxgiParams {
            height: 4,
            width: 4,
            depth: None,
            format: DxgiFormat::R8G8B8A8_UNorm,
            mipmap_levels: Some(3),
            array_layers: None,
            caps2: None,
            is_cubemap: false,
            resource_dimension: D3D10ResourceDimension::Texture2D,
            alpha_mode: AlphaMode::Straight,
        })
        .unwrap();
        dds.data[..64].copy_from_slice(&[255, 0, 0, 255].repeat(16));
        dds.data[64..80].copy_from_slice(&[0, 255, 0, 128].repeat(4));
        dds.data[80..84].copy_from_slice(&[0, 0, 255, 32]);
        let mut bytes = Vec::new();
        dds.write(&mut bytes).unwrap();

        let ktx = TextureConverter::convert(&bytes, TextureEncoding::ColorSrgb).unwrap();
        let metadata = inspect_ktx2(&ktx, TextureEncoding::ColorSrgb).unwrap();
        assert_eq!(metadata.levels, 3);
        // RGBA8 sources preserve natively, so authored mip bytes survive
        // verbatim through supercompression instead of approximately
        // through a UASTC round trip.
        let reader = ktx2::Reader::new(&ktx).unwrap();
        assert_eq!(reader.header().format, Some(ktx2::Format::R8G8B8A8_SRGB));
        assert_eq!(decode_zstd_level(&ktx, 0), &dds.data[..64]);
        assert_eq!(decode_zstd_level(&ktx, 1), &dds.data[64..80]);
        assert_eq!(decode_zstd_level(&ktx, 2), &dds.data[80..84]);
    }

    #[test]
    fn preserves_a_partial_authored_mip_chain() {
        let mut dds = Dds::new_dxgi(NewDxgiParams {
            height: 8,
            width: 8,
            depth: None,
            format: DxgiFormat::R8G8B8A8_UNorm,
            mipmap_levels: Some(2),
            array_layers: None,
            caps2: None,
            is_cubemap: false,
            resource_dimension: D3D10ResourceDimension::Texture2D,
            alpha_mode: AlphaMode::Straight,
        })
        .unwrap();
        dds.data[..256].copy_from_slice(&[255, 0, 0, 255].repeat(64));
        dds.data[256..320].copy_from_slice(&[0, 255, 0, 255].repeat(16));
        let mut bytes = Vec::new();
        dds.write(&mut bytes).unwrap();

        let ktx = TextureConverter::convert(&bytes, TextureEncoding::ColorSrgb).unwrap();
        assert_eq!(
            inspect_ktx2(&ktx, TextureEncoding::ColorSrgb)
                .unwrap()
                .levels,
            2
        );
        let authored_mip = decode_uastc_level(&ktx, 1);
        assert!(authored_mip[1] > authored_mip[0] && authored_mip[1] > authored_mip[2]);
    }

    #[test]
    fn generates_mipmap_chain_for_mipped_source() {
        let pixels = [64, 128, 192, 255].repeat(64);
        let bytes =
            encode_basis_ktx2(8, 8, &pixels, TextureEncoding::ColorSrgb, true, 192, 2).unwrap();
        let reader = ktx2::Reader::new(&bytes).unwrap();
        assert_eq!(reader.header().level_count, 4);
        assert_eq!(reader.levels().count(), 4);
    }

    #[test]
    fn assembles_six_faces_into_a_cubemap_ktx2() {
        let faces: Vec<_> = (0..6)
            .map(|face| {
                let pixels = [face * 20, 64, 128, 255].repeat(16);
                encode_basis_ktx2(4, 4, &pixels, TextureEncoding::ColorSrgb, true, 192, 2).unwrap()
            })
            .collect();

        let cubemap = combine_ktx2_cubemap_faces(&faces).unwrap();
        let reader = ktx2::Reader::new(&cubemap).unwrap();
        assert_eq!(reader.header().face_count, 6);
        assert_eq!(reader.header().layer_count, 0);
        assert_eq!(reader.header().level_count, 3);
        for (combined, single) in reader
            .levels()
            .zip(ktx2::Reader::new(&faces[0]).unwrap().levels())
        {
            assert_eq!(combined.data.len(), single.data.len() * 6);
            assert_eq!(
                combined.uncompressed_byte_length,
                single.uncompressed_byte_length * 6
            );
        }
    }

    #[test]
    fn assembles_depth_slices_into_a_volume_ktx2() {
        let template = encode_basis_ktx2(
            4,
            4,
            &[64, 64, 64, 255].repeat(16),
            TextureEncoding::DataLinear,
            true,
            192,
            2,
        )
        .unwrap();
        let levels: Vec<Vec<Vec<u8>>> = [(4, 4, 4), (2, 2, 2), (1, 1, 1)]
            .into_iter()
            .map(|(width, height, depth)| {
                (0..depth)
                    .map(|slice| {
                        encode_basis_ktx2(
                            width,
                            height,
                            &[slice as u8 * 20, 80, 120, 255].repeat((width * height) as usize),
                            TextureEncoding::DataLinear,
                            false,
                            192,
                            2,
                        )
                        .unwrap()
                    })
                    .collect()
            })
            .collect();

        let volume = combine_ktx2_volume(&template, &levels, 4, 4, 4).unwrap();
        let reader = ktx2::Reader::new(&volume).unwrap();
        assert_eq!(reader.header().pixel_width, 4);
        assert_eq!(reader.header().pixel_height, 4);
        assert_eq!(reader.header().pixel_depth, 4);
        assert_eq!(reader.header().level_count, 3);
        assert_eq!(reader.levels().count(), 3);
    }

    #[test]
    fn preserves_bc_blocks_byte_for_byte_in_native_ktx2() {
        let formats = [
            (DxgiFormat::BC1_UNorm, ktx2::Format::BC1_RGBA_UNORM_BLOCK),
            (DxgiFormat::BC2_UNorm, ktx2::Format::BC2_UNORM_BLOCK),
            (DxgiFormat::BC3_UNorm, ktx2::Format::BC3_UNORM_BLOCK),
            (DxgiFormat::BC4_UNorm, ktx2::Format::BC4_UNORM_BLOCK),
            (DxgiFormat::BC5_UNorm, ktx2::Format::BC5_UNORM_BLOCK),
            (DxgiFormat::BC6H_UF16, ktx2::Format::BC6H_UFLOAT_BLOCK),
            (DxgiFormat::BC7_UNorm, ktx2::Format::BC7_UNORM_BLOCK),
        ];
        for (dxgi, vk) in formats {
            let mut dds = Dds::new_dxgi(NewDxgiParams {
                height: 8,
                width: 8,
                depth: None,
                format: dxgi,
                mipmap_levels: Some(3),
                array_layers: None,
                caps2: None,
                is_cubemap: false,
                resource_dimension: D3D10ResourceDimension::Texture2D,
                alpha_mode: AlphaMode::Straight,
            })
            .unwrap();
            for (index, byte) in dds.data.iter_mut().enumerate() {
                *byte = (index * 7 + 3) as u8;
            }
            let mut bytes = Vec::new();
            dds.write(&mut bytes).unwrap();

            let ktx = TextureConverter::convert(&bytes, TextureEncoding::DataLinear)
                .unwrap_or_else(|error| panic!("failed to convert {dxgi:?}: {error:#}"));
            let reader = ktx2::Reader::new(&ktx).unwrap();
            assert_eq!(reader.header().format, Some(vk), "{dxgi:?}");
            assert_eq!(
                reader.header().supercompression_scheme,
                Some(ktx2::SupercompressionScheme::Zstandard),
                "{dxgi:?}"
            );
            assert_eq!(reader.levels().len(), 3, "{dxgi:?}");
            let mut offset = 0;
            for mip in 0..3 {
                let decoded = decode_zstd_level(&ktx, mip);
                assert_eq!(
                    decoded,
                    &dds.data[offset..offset + decoded.len()],
                    "{dxgi:?} mip {mip}"
                );
                offset += decoded.len();
            }
            assert_eq!(offset, dds.data.len(), "{dxgi:?} trailing bytes");
            let metadata = inspect_ktx2(&ktx, TextureEncoding::DataLinear).unwrap();
            assert_eq!(metadata.levels, 3);
            assert_eq!(metadata.faces, 1);
        }
    }

    #[test]
    fn native_srgb_uses_srgb_vkformat_from_slot_semantics() {
        let dds = Dds::new_dxgi(NewDxgiParams {
            height: 4,
            width: 4,
            depth: None,
            format: DxgiFormat::BC1_UNorm,
            mipmap_levels: None,
            array_layers: None,
            caps2: None,
            is_cubemap: false,
            resource_dimension: D3D10ResourceDimension::Texture2D,
            alpha_mode: AlphaMode::Straight,
        })
        .unwrap();
        let mut bytes = Vec::new();
        dds.write(&mut bytes).unwrap();

        let ktx = TextureConverter::convert(&bytes, TextureEncoding::ColorSrgb).unwrap();
        let reader = ktx2::Reader::new(&ktx).unwrap();
        assert_eq!(
            reader.header().format,
            Some(ktx2::Format::BC1_RGBA_SRGB_BLOCK)
        );
        assert_eq!(
            reader.transfer_function(),
            Some(ktx2::TransferFunction::SRGB)
        );
        inspect_ktx2(&ktx, TextureEncoding::ColorSrgb).unwrap();
    }

    #[test]
    fn linear_only_formats_in_srgb_slots_use_uastc_fallback() {
        // BC4/BC5/BC6H/R8 have no sRGB VkFormat; a ColorSrgb slot must fall
        // back to UASTC (format None) rather than fail transfer validation.
        for format in [
            DxgiFormat::BC4_UNorm,
            DxgiFormat::BC5_UNorm,
            DxgiFormat::BC6H_UF16,
            DxgiFormat::R8_UNorm,
        ] {
            let dds = Dds::new_dxgi(NewDxgiParams {
                height: 4,
                width: 4,
                depth: None,
                format,
                mipmap_levels: None,
                array_layers: None,
                caps2: None,
                is_cubemap: false,
                resource_dimension: D3D10ResourceDimension::Texture2D,
                alpha_mode: AlphaMode::Straight,
            })
            .unwrap();
            let mut bytes = Vec::new();
            dds.write(&mut bytes).unwrap();
            let ktx = TextureConverter::convert(&bytes, TextureEncoding::ColorSrgb)
                .unwrap_or_else(|error| panic!("failed to convert {format:?}: {error:#}"));
            let reader = ktx2::Reader::new(&ktx).unwrap();
            assert_eq!(reader.header().format, None, "{format:?} must use UASTC");
            assert_eq!(
                reader.transfer_function(),
                Some(ktx2::TransferFunction::SRGB),
                "{format:?}"
            );
            inspect_ktx2(&ktx, TextureEncoding::ColorSrgb).unwrap();
        }
    }

    #[test]
    fn native_cubemap_gathers_faces_per_mip_level() {
        let mut dds = Dds::new_dxgi(NewDxgiParams {
            height: 8,
            width: 8,
            depth: None,
            format: DxgiFormat::BC3_UNorm,
            mipmap_levels: Some(2),
            array_layers: None,
            caps2: None,
            is_cubemap: true,
            resource_dimension: D3D10ResourceDimension::Texture2D,
            alpha_mode: AlphaMode::Straight,
        })
        .unwrap();
        let face = dds.data.clone();
        dds.data = face.repeat(6);
        for (index, byte) in dds.data.iter_mut().enumerate() {
            *byte = (index % 251) as u8;
        }
        let mut bytes = Vec::new();
        dds.write(&mut bytes).unwrap();

        let ktx = TextureConverter::convert(&bytes, TextureEncoding::ColorSrgb).unwrap();
        let reader = ktx2::Reader::new(&ktx).unwrap();
        assert_eq!(reader.header().face_count, 6);
        assert_eq!(reader.levels().len(), 2);
        let levels = [decode_zstd_level(&ktx, 0), decode_zstd_level(&ktx, 1)];
        // DDS stores one face's full chain contiguously; KTX2 stores one
        // level's six faces contiguously. Face 3 mip 1 lives at
        // face_stride * 3 + mip0_len in the DDS and at faces 0..3 of mip 1
        // in the KTX2.
        let mip0_len = 64;
        let mip1_len = 16;
        let face_stride = mip0_len + mip1_len;
        for face in 0..6 {
            let dds_mip0 = &dds.data[face * face_stride..face * face_stride + mip0_len];
            let ktx_mip0 = &levels[0][face * mip0_len..face * mip0_len + mip0_len];
            assert_eq!(dds_mip0, ktx_mip0, "face {face} mip 0");
            let dds_mip1 =
                &dds.data[face * face_stride + mip0_len..face * face_stride + face_stride];
            let ktx_mip1 = &levels[1][face * mip1_len..face * mip1_len + mip1_len];
            assert_eq!(dds_mip1, ktx_mip1, "face {face} mip 1");
        }
        let metadata = inspect_ktx2(&ktx, TextureEncoding::ColorSrgb).unwrap();
        assert_eq!(metadata.faces, 6);
        assert_eq!(metadata.levels, 2);
    }

    #[test]
    fn uncompressed_rgba8_stays_native_and_x8r8g8b8_becomes_bc1() {
        let mut dds = Dds::new_dxgi(NewDxgiParams {
            height: 4,
            width: 4,
            depth: None,
            format: DxgiFormat::R8G8B8A8_UNorm,
            mipmap_levels: None,
            array_layers: None,
            caps2: None,
            is_cubemap: false,
            resource_dimension: D3D10ResourceDimension::Texture2D,
            alpha_mode: AlphaMode::Straight,
        })
        .unwrap();
        for (index, byte) in dds.data.iter_mut().enumerate() {
            *byte = index as u8;
        }
        let mut bytes = Vec::new();
        dds.write(&mut bytes).unwrap();
        let ktx = TextureConverter::convert(&bytes, TextureEncoding::DataLinear).unwrap();
        let reader = ktx2::Reader::new(&ktx).unwrap();
        assert_eq!(reader.header().format, Some(ktx2::Format::R8G8B8A8_UNORM));
        assert_eq!(reader.levels().len(), 1);
        assert_eq!(decode_zstd_level(&ktx, 0), dds.data.as_slice());

        let x8 = x8r8g8b8_fixture();
        let mut bytes = Vec::new();
        x8.write(&mut bytes).unwrap();
        let ktx = TextureConverter::convert(&bytes, TextureEncoding::ColorSrgb).unwrap();
        let reader = ktx2::Reader::new(&ktx).unwrap();
        assert_eq!(
            reader.header().format,
            Some(ktx2::Format::BC1_RGBA_SRGB_BLOCK),
            "X8R8G8B8 becomes native BC1"
        );
    }

    /// How an uncompressed DDS is routed under each `--texture-encoder`
    /// setting, without a GPU. The default CPU encoder block-compresses it to
    /// native BC. The GPU encoder claims the same texture (`takes`) and
    /// uploads the decoded pixels as they are (`PreparedTexture`), so the
    /// native compression of this module never runs for it; native block
    /// formats are copied in both modes and the GPU does not claim them. The
    /// CPU entry point is also what the pipeline calls when the GPU path
    /// declines a texture, so that fallback gets the same native BC output.
    #[test]
    fn uncompressed_dds_routing_per_texture_encoder() {
        use crate::texture_gpu::{self, PreparedTexture};

        let write_dds = |dds: &Dds, name: &str, dir: &tempfile::TempDir| {
            let path = dir.path().join(name);
            let mut bytes = Vec::new();
            dds.write(&mut bytes).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            (path, bytes)
        };
        let dir = tempfile::tempdir().unwrap();
        let argb = packed_dds(D3DFormat::A8R8G8B8, 8, 8, 2, gradient);
        let xrgb = x8r8g8b8_fixture();
        let bc1 = Dds::new_dxgi(NewDxgiParams {
            height: 8,
            width: 8,
            depth: None,
            format: DxgiFormat::BC1_UNorm,
            mipmap_levels: None,
            array_layers: None,
            caps2: None,
            is_cubemap: false,
            resource_dimension: D3D10ResourceDimension::Texture2D,
            alpha_mode: AlphaMode::Straight,
        })
        .unwrap();
        let encoding = TextureEncoding::ColorSrgb;

        for (dds, name, expected) in [
            (&argb, "argb.dds", ktx2::Format::BC7_SRGB_BLOCK),
            (&xrgb, "xrgb.dds", ktx2::Format::BC1_RGBA_SRGB_BLOCK),
        ] {
            let (path, bytes) = write_dds(dds, name, &dir);
            // GPU encoder: claims the texture and uploads the uncompressed payload.
            assert!(texture_gpu::takes(&path, encoding), "{name}: GPU takes it");
            let prepared = PreparedTexture::from_dds(bytes, encoding).unwrap();
            assert!(!prepared.upload().is_empty(), "{name}: pixels uploaded");
            // Default CPU encoder, and the GPU path's per-texture fallback.
            let output = dir.path().join(format!("{name}.ktx2"));
            TextureConverter::convert_dds_to_ktx2_with_options(
                &path,
                &output,
                encoding,
                ETC1S_QUALITY_DEFAULT,
                UASTC_LEVEL_DEFAULT,
                ZSTD_LEVEL_DEFAULT,
            )
            .unwrap();
            let ktx = std::fs::read(&output).unwrap();
            assert_eq!(
                ktx2::Reader::new(&ktx).unwrap().header().format,
                Some(expected),
                "{name}: CPU path is native BC"
            );
        }

        // Native block formats are copied by the CPU converter in either mode.
        let (path, _) = write_dds(&bc1, "bc1.dds", &dir);
        assert!(!texture_gpu::takes(&path, encoding), "native BC1 not taken");
    }

    /// Builds a packed uncompressed DDS whose every mip is the gradient at
    /// that mip size, stored in the byte order of the requested layout.
    fn packed_dds(
        format: D3DFormat,
        width: u32,
        height: u32,
        mips: u32,
        source: fn(u32, u32) -> Vec<u8>,
    ) -> Dds {
        let mut dds = Dds::new_d3d(NewD3dParams {
            height,
            width,
            depth: None,
            format,
            mipmap_levels: Some(mips),
            caps2: None,
        })
        .unwrap();
        let layout = packed_rgba8_layout(&dds).expect("packed layout");
        dds.data.clear();
        for mip in 0..mips {
            let (w, h) = ((width >> mip).max(1), (height >> mip).max(1));
            let rgba = source(w, h);
            for texel in rgba.as_chunks::<4>().0 {
                let mut pixel = vec![0u8; layout.bytes_per_pixel];
                pixel[layout.red] = texel[0];
                pixel[layout.green] = texel[1];
                pixel[layout.blue] = texel[2];
                if let Some(alpha) = layout.alpha {
                    pixel[alpha] = texel[3];
                } else if layout.bytes_per_pixel == 4 {
                    pixel[3] = 7;
                }
                dds.data.extend_from_slice(&pixel);
            }
        }
        dds
    }

    /// A diagonal ramp: colour lies near one line in RGB space, which BC1
    /// represents well, so the error bounds below stay tight.
    fn gradient(width: u32, height: u32) -> Vec<u8> {
        let mut rgba = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let t = (x + y) * 200 / (width + height);
                rgba.extend_from_slice(&[t as u8, (t * 3 / 4 + 20) as u8, 120, (40 + t / 2) as u8]);
            }
        }
        rgba
    }

    /// Decodes one stored level of a BC1/BC7 KTX2 back to RGBA8, cropped to
    /// the real size of the level.
    fn decode_bc_level(bytes: &[u8], mip: usize, dxgi: DxgiFormat) -> Vec<u8> {
        let header = ktx2::Reader::new(bytes).unwrap().header();
        let width = (header.pixel_width >> mip).max(1);
        let height = (header.pixel_height >> mip).max(1);
        let (padded_width, padded_height) = (width.next_multiple_of(4), height.next_multiple_of(4));
        let mut dds = Dds::new_dxgi(NewDxgiParams {
            height: padded_height,
            width: padded_width,
            depth: None,
            format: dxgi,
            mipmap_levels: None,
            array_layers: None,
            caps2: None,
            is_cubemap: false,
            resource_dimension: D3D10ResourceDimension::Texture2D,
            alpha_mode: AlphaMode::Straight,
        })
        .unwrap();
        dds.data = decode_zstd_level(bytes, mip);
        let full = image_dds::SurfaceRgba8::decode_dds(&dds)
            .unwrap()
            .get_image(0, 0, 0)
            .unwrap();
        let mut out = Vec::new();
        for y in 0..height {
            for x in 0..width {
                out.extend_from_slice(&full.get_pixel(x, y).0);
            }
        }
        out
    }

    fn mean_abs_error(a: &[u8], b: &[u8], channels: &[usize]) -> f64 {
        assert_eq!(a.len(), b.len());
        let mut total = 0u64;
        let mut count = 0u64;
        for (p, q) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
            for &c in channels {
                total += u64::from(p[c].abs_diff(q[c]));
                count += 1;
            }
        }
        total as f64 / count as f64
    }

    fn assert_packed_converts(
        format: D3DFormat,
        size: (u32, u32, u32),
        encoding: TextureEncoding,
        expected: ktx2::Format,
        has_alpha: bool,
    ) {
        assert_packed_converts_with(format, size, encoding, expected, has_alpha, gradient, 8.0);
    }

    fn assert_packed_converts_with(
        format: D3DFormat,
        (width, height, mips): (u32, u32, u32),
        encoding: TextureEncoding,
        expected: ktx2::Format,
        has_alpha: bool,
        gradient: fn(u32, u32) -> Vec<u8>,
        colour_bound: f64,
    ) {
        let dds = packed_dds(format, width, height, mips, gradient);
        let mut bytes = Vec::new();
        dds.write(&mut bytes).unwrap();
        let ktx = TextureConverter::convert(&bytes, encoding).unwrap();
        let reader = ktx2::Reader::new(&ktx).unwrap();
        assert_eq!(reader.header().format, Some(expected));
        assert_eq!(reader.header().level_count, mips);
        assert_eq!(reader.header().pixel_width, width);
        assert_eq!(reader.header().pixel_height, height);
        validate_ktx2_against_dds(&ktx, &dds, encoding, false).unwrap();
        let dxgi = if matches!(
            expected,
            ktx2::Format::BC1_RGBA_UNORM_BLOCK | ktx2::Format::BC1_RGBA_SRGB_BLOCK
        ) {
            DxgiFormat::BC1_UNorm
        } else {
            DxgiFormat::BC7_UNorm
        };
        for mip in 0..mips as usize {
            let (w, h) = ((width >> mip).max(1), (height >> mip).max(1));
            let decoded = decode_bc_level(&ktx, mip, dxgi);
            let source = gradient(w, h);
            let rgb = mean_abs_error(&decoded, &source, &[0, 1, 2]);
            assert!(rgb < colour_bound, "mip {mip} ({w}x{h}) colour error {rgb}");
            if has_alpha {
                let alpha = mean_abs_error(&decoded, &source, &[3]);
                assert!(alpha < 4.0, "mip {mip} ({w}x{h}) alpha error {alpha}");
            } else {
                // BC7 mode 6 can land one step below 255; the slot ignores alpha.
                assert!(decoded.as_chunks::<4>().0.iter().all(|p| p[3] >= 253));
            }
        }
    }

    #[test]
    fn opaque_packed_formats_become_bc1() {
        for format in [D3DFormat::R8G8B8, D3DFormat::X8R8G8B8] {
            assert_packed_converts(
                format,
                (16, 16, 5),
                TextureEncoding::ColorSrgb,
                ktx2::Format::BC1_RGBA_SRGB_BLOCK,
                false,
            );
        }
    }

    #[test]
    fn opaque_packed_formats_in_normal_and_data_slots_become_opaque_bc7() {
        for encoding in [TextureEncoding::NormalLinear, TextureEncoding::DataLinear] {
            for format in [D3DFormat::R8G8B8, D3DFormat::X8R8G8B8] {
                assert_packed_converts(
                    format,
                    (16, 16, 5),
                    encoding,
                    ktx2::Format::BC7_UNORM_BLOCK,
                    false,
                );
            }
        }
    }

    fn noise(width: u32, height: u32) -> Vec<u8> {
        // Independent R, G and B (and A) values: not on one colour line.
        let mut state = 0x1234_5678_u32;
        let mut next = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (100 + (state >> 24) % 40) as u8
        };
        let mut rgba = Vec::new();
        for _ in 0..width * height {
            rgba.extend_from_slice(&[next(), next(), next(), next()]);
        }
        rgba
    }

    #[test]
    fn uncorrelated_channels_keep_a_sensible_bc7_error() {
        assert_packed_converts_with(
            D3DFormat::A8R8G8B8,
            (16, 16, 1),
            TextureEncoding::DataLinear,
            ktx2::Format::BC7_UNORM_BLOCK,
            true,
            noise,
            8.0,
        );
        assert_packed_converts_with(
            D3DFormat::R8G8B8,
            (16, 16, 1),
            TextureEncoding::NormalLinear,
            ktx2::Format::BC7_UNORM_BLOCK,
            false,
            noise,
            8.0,
        );
    }

    #[test]
    fn packed_alpha_formats_become_bc7_with_alpha() {
        for format in [D3DFormat::A8R8G8B8, D3DFormat::A8B8G8R8] {
            assert_packed_converts(
                format,
                (16, 16, 5),
                TextureEncoding::DataLinear,
                ktx2::Format::BC7_UNORM_BLOCK,
                true,
            );
        }
    }

    #[test]
    fn packed_formats_pad_small_and_odd_mips_to_whole_blocks() {
        assert_packed_converts(
            D3DFormat::R8G8B8,
            (4, 4, 3),
            TextureEncoding::ColorSrgb,
            ktx2::Format::BC1_RGBA_SRGB_BLOCK,
            false,
        );
        assert_packed_converts(
            D3DFormat::A8R8G8B8,
            (6, 5, 3),
            TextureEncoding::DataLinear,
            ktx2::Format::BC7_UNORM_BLOCK,
            true,
        );
        assert_packed_converts(
            D3DFormat::X8R8G8B8,
            (9, 3, 4),
            TextureEncoding::ColorSrgb,
            ktx2::Format::BC1_RGBA_SRGB_BLOCK,
            false,
        );
    }

    #[test]
    fn packed_formats_follow_the_slot_encoding() {
        assert_packed_converts(
            D3DFormat::R8G8B8,
            (8, 8, 4),
            TextureEncoding::ColorSrgb,
            ktx2::Format::BC1_RGBA_SRGB_BLOCK,
            false,
        );
        assert_packed_converts(
            D3DFormat::A8R8G8B8,
            (8, 8, 4),
            TextureEncoding::ColorSrgb,
            ktx2::Format::BC7_SRGB_BLOCK,
            true,
        );
    }

    #[test]
    fn bc1_dds_stays_byte_identical_native() {
        let mut dds = Dds::new_dxgi(NewDxgiParams {
            height: 8,
            width: 8,
            depth: None,
            format: DxgiFormat::BC1_UNorm,
            mipmap_levels: Some(4),
            array_layers: None,
            caps2: None,
            is_cubemap: false,
            resource_dimension: D3D10ResourceDimension::Texture2D,
            alpha_mode: AlphaMode::Straight,
        })
        .unwrap();
        for (index, byte) in dds.data.iter_mut().enumerate() {
            *byte = (index * 7) as u8;
        }
        let mut bytes = Vec::new();
        dds.write(&mut bytes).unwrap();
        let ktx = TextureConverter::convert(&bytes, TextureEncoding::DataLinear).unwrap();
        let reader = ktx2::Reader::new(&ktx).unwrap();
        assert_eq!(
            reader.header().format,
            Some(ktx2::Format::BC1_RGBA_UNORM_BLOCK)
        );
        let mut offset = 0;
        for (mip, size) in [(0, 32), (1, 8), (2, 8), (3, 8)] {
            assert_eq!(
                decode_zstd_level(&ktx, mip),
                &dds.data[offset..offset + size]
            );
            offset += size;
        }
    }

    /// Hand-written DDS bytes: header fields set from the DDS specification,
    /// independent of `ddsfile` and of `packed_rgba8_layout`. `pixel_format` is
    /// `(flags, fourcc, bit_count, [r, g, b, a] masks)`.
    fn hand_built_dds(
        (width, height, mips): (u32, u32, u32),
        pitch: u32,
        pixel_format: (u32, &[u8; 4], u32, [u32; 4]),
        dxgi_format: Option<u32>,
        payload: &[u8],
    ) -> Vec<u8> {
        let (pf_flags, fourcc, bit_count, masks) = pixel_format;
        let mut out = b"DDS ".to_vec();
        let mut put = |value: u32| out.extend_from_slice(&value.to_le_bytes());
        put(124);
        // CAPS | HEIGHT | WIDTH | PITCH | PIXELFORMAT | MIPMAPCOUNT
        put(0x1 | 0x2 | 0x4 | 0x8 | 0x1000 | 0x2_0000);
        put(height);
        put(width);
        put(pitch);
        put(0);
        put(mips);
        for _ in 0..11 {
            put(0);
        }
        put(32);
        put(pf_flags);
        out.extend_from_slice(fourcc);
        let mut put = |value: u32| out.extend_from_slice(&value.to_le_bytes());
        put(bit_count);
        for mask in masks {
            put(mask);
        }
        put(0x1000 | if mips > 1 { 0x40_0008 } else { 0 });
        for _ in 0..4 {
            put(0);
        }
        if let Some(dxgi) = dxgi_format {
            for value in [dxgi, 3, 0, 1, 0] {
                put(value);
            }
        }
        out.extend_from_slice(payload);
        out
    }

    const RGB24: (u32, &[u8; 4], u32, [u32; 4]) = (0x40, &[0; 4], 24, [0xff_0000, 0xff00, 0xff, 0]);

    /// Pixel `i` of a hand-written 24-bit image: stored B, G, R.
    fn bgr_pixel(index: u32) -> [u8; 3] {
        [10 + index as u8, 20 + index as u8 * 2, 30 + index as u8 * 3]
    }

    /// Writes `rows` of `width` pixels with `row_bytes` per row (zero padded),
    /// numbering pixels consecutively across the whole chain via `first`.
    fn bgr_level(width: u32, height: u32, row_bytes: usize, first: u32) -> (Vec<u8>, Vec<u8>) {
        let mut stored = Vec::new();
        let mut expected = Vec::new();
        for y in 0..height {
            let mut row = Vec::new();
            for x in 0..width {
                let [b, g, r] = bgr_pixel(first + y * width + x);
                row.extend_from_slice(&[b, g, r]);
                expected.extend_from_slice(&[r, g, b, 255]);
            }
            row.resize(row_bytes, 0);
            stored.extend_from_slice(&row);
        }
        (stored, expected)
    }

    fn bgr_chain(aligned: bool) -> (Vec<u8>, Vec<Vec<u8>>, u32) {
        let mut payload = Vec::new();
        let mut expected = Vec::new();
        let mut first = 0;
        let mut base_pitch = 0;
        for (mip, (w, h)) in [(5u32, 4u32), (2, 2), (1, 1)].into_iter().enumerate() {
            let tight = w as usize * 3;
            let row_bytes = if aligned {
                tight.next_multiple_of(4)
            } else {
                tight
            };
            if mip == 0 {
                base_pitch = row_bytes as u32;
            }
            let (stored, level) = bgr_level(w, h, row_bytes, first);
            payload.extend_from_slice(&stored);
            expected.push(level);
            first += w * h;
        }
        (payload, expected, base_pitch)
    }

    #[test]
    fn hand_built_24_bit_chains_decode_with_tight_and_dword_aligned_rows() {
        for aligned in [false, true] {
            let (payload, expected, pitch) = bgr_chain(aligned);
            let bytes = hand_built_dds((5, 4, 3), pitch, RGB24, None, &payload);
            let dds = Dds::read(Cursor::new(&bytes)).unwrap();
            let layout = packed_rgba8_layout(&dds).expect("24-bit layout");
            let levels = decode_packed_mips(&dds, layout).unwrap();
            assert_eq!(levels.len(), 3);
            for (mip, ((_, _, rgba), want)) in levels.iter().zip(&expected).enumerate() {
                assert_eq!(rgba, want, "aligned={aligned} mip {mip}");
            }
            // The whole conversion also produces a valid native container.
            let ktx = TextureConverter::convert(&bytes, TextureEncoding::ColorSrgb).unwrap();
            let reader = ktx2::Reader::new(&ktx).unwrap();
            assert_eq!(
                reader.header().format,
                Some(ktx2::Format::BC1_RGBA_SRGB_BLOCK)
            );
            assert_eq!(reader.header().level_count, 3);
        }
    }

    #[test]
    fn hand_built_24_bit_equal_base_pitches_use_the_payload_length() {
        // Width 4: tight and DWORD-aligned rows are both 12 bytes at mip 0 and
        // only the lower mips (2x2, 1x1) differ, so the payload length decides.
        for aligned in [false, true] {
            let mut payload = Vec::new();
            let mut expected = Vec::new();
            let mut first = 0;
            for (w, h) in [(4u32, 4u32), (2, 2), (1, 1)] {
                let tight = w as usize * 3;
                let row_bytes = if aligned {
                    tight.next_multiple_of(4)
                } else {
                    tight
                };
                let (stored, level) = bgr_level(w, h, row_bytes, first);
                payload.extend_from_slice(&stored);
                expected.push(level);
                first += w * h;
            }
            let bytes = hand_built_dds((4, 4, 3), 12, RGB24, None, &payload);
            let dds = Dds::read(Cursor::new(&bytes)).unwrap();
            let layout = packed_rgba8_layout(&dds).expect("24-bit layout");
            let levels = decode_packed_mips(&dds, layout).unwrap();
            for (mip, ((_, _, rgba), want)) in levels.iter().zip(&expected).enumerate() {
                assert_eq!(rgba, want, "aligned={aligned} mip {mip}");
            }
        }
    }

    /// The GPU encoder reads a 24-bit chain in place only when its rows are tight;
    /// a DWORD-aligned chain (header pitch 16 for width 5, or lower mips of a
    /// width-4 chain) is decoded on the CPU, so no row is read shifted.
    #[test]
    fn gpu_reads_24_bit_chains_in_place_only_when_tight() {
        let encoding = TextureEncoding::ColorSrgb;
        // (width, height, mips, header pitch of the tight chain)
        for (width, mips, tight_pitch) in [(5u32, 3u32, 15u32), (4, 3, 12)] {
            for aligned in [false, true] {
                let mut payload = Vec::new();
                let mut expected_rgba = Vec::new();
                let mut first = 0;
                let mut base_pitch = 0;
                for mip in 0..mips {
                    let (w, h) = ((width >> mip).max(1), (4u32 >> mip).max(1));
                    let tight = w as usize * 3;
                    let row_bytes = if aligned {
                        tight.next_multiple_of(4)
                    } else {
                        tight
                    };
                    if mip == 0 {
                        base_pitch = row_bytes as u32;
                    }
                    let (stored, level) = bgr_level(w, h, row_bytes, first);
                    payload.extend_from_slice(&stored);
                    expected_rgba.extend_from_slice(&level);
                    first += w * h;
                }
                let bytes = hand_built_dds((width, 4, mips), base_pitch, RGB24, None, &payload);
                let prepared =
                    crate::texture_gpu::PreparedTexture::from_dds(bytes, encoding).unwrap();
                let in_place = prepared
                    .images
                    .iter()
                    .all(|image| image.format == crate::texture_gpu::SourceFormat::Bgr8);
                let label = format!("width {width} aligned={aligned} (tight pitch {tight_pitch})");
                if aligned && payload.len() != tight_chain_len(width, 4, mips) {
                    assert!(!in_place, "{label}: decoded on the CPU");
                    assert!(
                        prepared
                            .images
                            .iter()
                            .all(|image| image.format == crate::texture_gpu::SourceFormat::Rgba8),
                        "{label}: RGBA levels"
                    );
                    assert_eq!(prepared.upload(), expected_rgba, "{label}: pixels");
                } else {
                    assert!(in_place, "{label}: referenced in place");
                    assert_eq!(prepared.upload(), payload, "{label}: payload untouched");
                }
            }
        }
    }

    /// Byte size of a tight 24-bit chain for a `width` x `height` base with `mips` levels.
    fn tight_chain_len(width: u32, height: u32, mips: u32) -> usize {
        (0..mips)
            .map(|mip| ((width >> mip).max(1) * (height >> mip).max(1) * 3) as usize)
            .sum()
    }

    #[test]
    fn packed_decoding_rejects_ambiguous_pitch_and_length_mismatch() {
        let (payload, _, pitch) = bgr_chain(false);
        let decode = |pitch: u32, payload: &[u8]| {
            let bytes = hand_built_dds((5, 4, 3), pitch, RGB24, None, payload);
            let dds = Dds::read(Cursor::new(&bytes)).unwrap();
            let layout = packed_rgba8_layout(&dds).unwrap();
            decode_packed_mips(&dds, layout).map(|_| ())
        };
        decode(pitch, &payload).unwrap();
        assert!(decode(pitch + 1, &payload).is_err(), "odd pitch");
        let mut longer = payload.clone();
        longer.push(0);
        assert!(decode(pitch, &longer).is_err(), "trailing byte");
        assert!(decode(pitch, &payload[..payload.len() - 1]).is_err());
    }

    #[test]
    fn hand_built_24_bit_solid_colour_keeps_channel_order_through_bc1() {
        let pixel = [200u8, 100, 30]; // stored B, G, R
        let payload: Vec<u8> = pixel.iter().copied().cycle().take(4 * 4 * 3).collect();
        let bytes = hand_built_dds((4, 4, 1), 12, RGB24, None, &payload);
        let ktx = TextureConverter::convert(&bytes, TextureEncoding::ColorSrgb).unwrap();
        let decoded = decode_bc_level(&ktx, 0, DxgiFormat::BC1_UNorm);
        for texel in decoded.as_chunks::<4>().0 {
            assert!(texel[0].abs_diff(30) <= 8, "{texel:?}");
            assert!(texel[1].abs_diff(100) <= 8, "{texel:?}");
            assert!(texel[2].abs_diff(200) <= 8, "{texel:?}");
            assert_eq!(texel[3], 255);
        }
    }

    #[test]
    fn hand_built_dxgi_b8g8r8a8_keeps_channel_order_and_alpha_through_bc7() {
        let pixel = [200u8, 100, 30, 77]; // stored B, G, R, A
        let payload: Vec<u8> = pixel.iter().copied().cycle().take(4 * 4 * 4).collect();
        let bytes = hand_built_dds((4, 4, 1), 16, (0x4, b"DX10", 0, [0; 4]), Some(87), &payload);
        let ktx = TextureConverter::convert(&bytes, TextureEncoding::DataLinear).unwrap();
        let reader = ktx2::Reader::new(&ktx).unwrap();
        assert_eq!(reader.header().format, Some(ktx2::Format::BC7_UNORM_BLOCK));
        let decoded = decode_bc_level(&ktx, 0, DxgiFormat::BC7_UNorm);
        for texel in decoded.as_chunks::<4>().0 {
            assert!(texel[0].abs_diff(30) <= 3, "{texel:?}");
            assert!(texel[1].abs_diff(100) <= 3, "{texel:?}");
            assert!(texel[2].abs_diff(200) <= 3, "{texel:?}");
            assert!(texel[3].abs_diff(77) <= 3, "{texel:?}");
        }
    }

    #[test]
    fn hand_built_a8r8g8b8_keeps_channel_order_and_alpha_through_bc7() {
        let pixel = [200u8, 100, 30, 77]; // stored B, G, R, A
        let payload: Vec<u8> = pixel.iter().copied().cycle().take(4 * 4 * 4).collect();
        let argb = (0x41, &[0; 4], 32, [0xff_0000, 0xff00, 0xff, 0xff00_0000]);
        let bytes = hand_built_dds((4, 4, 1), 16, argb, None, &payload);
        let ktx = TextureConverter::convert(&bytes, TextureEncoding::DataLinear).unwrap();
        let reader = ktx2::Reader::new(&ktx).unwrap();
        assert_eq!(reader.header().format, Some(ktx2::Format::BC7_UNORM_BLOCK));
        let decoded = decode_bc_level(&ktx, 0, DxgiFormat::BC7_UNorm);
        for texel in decoded.as_chunks::<4>().0 {
            assert!(texel[0].abs_diff(30) <= 3, "{texel:?}");
            assert!(texel[1].abs_diff(100) <= 3, "{texel:?}");
            assert!(texel[2].abs_diff(200) <= 3, "{texel:?}");
            assert!(texel[3].abs_diff(77) <= 3, "{texel:?}");
        }
    }

    #[test]
    fn hand_built_a8b8g8r8_keeps_channel_order_and_alpha_through_bc7() {
        let pixel = [30u8, 100, 200, 77]; // stored R, G, B, A
        let payload: Vec<u8> = pixel.iter().copied().cycle().take(4 * 4 * 4).collect();
        let abgr = (0x41, &[0; 4], 32, [0xff, 0xff00, 0xff_0000, 0xff00_0000]);
        let bytes = hand_built_dds((4, 4, 1), 16, abgr, None, &payload);
        let ktx = TextureConverter::convert(&bytes, TextureEncoding::DataLinear).unwrap();
        let reader = ktx2::Reader::new(&ktx).unwrap();
        assert_eq!(reader.header().format, Some(ktx2::Format::BC7_UNORM_BLOCK));
        let decoded = decode_bc_level(&ktx, 0, DxgiFormat::BC7_UNorm);
        for texel in decoded.as_chunks::<4>().0 {
            assert!(texel[0].abs_diff(30) <= 3, "{texel:?}");
            assert!(texel[1].abs_diff(100) <= 3, "{texel:?}");
            assert!(texel[2].abs_diff(200) <= 3, "{texel:?}");
            assert!(texel[3].abs_diff(77) <= 3, "{texel:?}");
        }
    }

    #[test]
    fn packed_layout_reads_alpha_only_from_a_flagged_non_zero_mask() {
        let payload = [0u8; 4 * 4 * 4];
        let layout_of = |flags: u32, alpha_mask: u32| {
            let bytes = hand_built_dds(
                (4, 4, 1),
                16,
                (flags, &[0; 4], 32, [0xff_0000, 0xff00, 0xff, alpha_mask]),
                None,
                &payload,
            );
            let dds = Dds::read(Cursor::new(&bytes)).unwrap();
            packed_rgba8_layout(&dds).expect("32-bit RGB layout")
        };
        // ddsfile drops a mask without an alpha flag: opaque, as every decoder sees it.
        assert_eq!(layout_of(0x40, 0xff00_0000).alpha, None);
        // ALPHA_PIXELS with a zero mask has no alpha bits: opaque, still packed.
        assert_eq!(layout_of(0x41, 0).alpha, None);
        assert_eq!(layout_of(0x41, 0xff00_0000).alpha, Some(3));
        // DDPF_ALPHA (0x2) with RGB also carries alpha, as ddsfile's A8R8G8B8 detection says.
        assert_eq!(layout_of(0x42, 0xff00_0000).alpha, Some(3));
    }

    #[test]
    fn packed_decode_checks_the_payload_and_size_before_allocating() {
        // A header declaring 16384x16384 with a tiny payload is refused by the payload
        // check, before any RGBA8 buffer for it is reserved.
        let bytes = hand_built_dds(
            (16384, 16384, 1),
            16384 * 4,
            (0x40, &[0; 4], 32, [0xff_0000, 0xff00, 0xff, 0]),
            None,
            &[0u8; 64],
        );
        let dds = Dds::read(Cursor::new(&bytes)).unwrap();
        let layout = packed_rgba8_layout(&dds).unwrap();
        let error = format!("{:#}", decode_packed_mips(&dds, layout).unwrap_err());
        assert!(error.contains("its mips need"), "{error}");
        // With the payload present, 16384x16384 RGBA8 (1 GiB) is over the 256 MiB cap.
        const { assert!(16384usize * 16384 * 4 > PACKED_MAX_OUTPUT_BYTES) };
    }

    #[test]
    fn packed_path_failures_fall_back_instead_of_failing_the_texture() {
        // A8R8G8B8 with a bad pitch cannot take the packed path; the generic
        // decoder and UASTC still convert it, as on main.
        let pixel = [10u8, 20, 30, 40];
        let payload: Vec<u8> = pixel.iter().copied().cycle().take(4 * 4 * 4).collect();
        let argb = (0x41, &[0; 4], 32, [0xff_0000, 0xff00, 0xff, 0xff00_0000]);
        let bytes = hand_built_dds((4, 4, 1), 17, argb, None, &payload);
        let ktx = TextureConverter::convert(&bytes, TextureEncoding::DataLinear).unwrap();
        let reader = ktx2::Reader::new(&ktx).unwrap();
        assert_eq!(reader.header().format, None, "fell back to UASTC");
    }

    #[test]
    fn zstd_level_zero_disables_supercompression() {
        let dds = Dds::new_dxgi(NewDxgiParams {
            height: 4,
            width: 4,
            depth: None,
            format: DxgiFormat::BC3_UNorm,
            mipmap_levels: None,
            array_layers: None,
            caps2: None,
            is_cubemap: false,
            resource_dimension: D3D10ResourceDimension::Texture2D,
            alpha_mode: AlphaMode::Straight,
        })
        .unwrap();
        let mut bytes = Vec::new();
        dds.write(&mut bytes).unwrap();
        let ktx =
            TextureConverter::convert_uncompressed(&bytes, TextureEncoding::DataLinear).unwrap();
        let reader = ktx2::Reader::new(&ktx).unwrap();
        assert_eq!(reader.header().supercompression_scheme, None);
        let levels: Vec<_> = reader.levels().collect();
        assert_eq!(levels.len(), 1);
        assert_eq!(levels[0].data, dds.data.as_slice());
        inspect_ktx2(&ktx, TextureEncoding::DataLinear).unwrap();
    }

    #[test]
    fn uastc_fallback_levels_supercompress_after_assembly() {
        // BC5 has no sRGB VkFormat, so a colour slot still takes the UASTC path.
        let bc5 = Dds::new_dxgi(NewDxgiParams {
            height: 4,
            width: 4,
            depth: None,
            format: DxgiFormat::BC5_UNorm,
            mipmap_levels: None,
            array_layers: None,
            caps2: None,
            is_cubemap: false,
            resource_dimension: D3D10ResourceDimension::Texture2D,
            alpha_mode: AlphaMode::Straight,
        })
        .unwrap();
        let mut bytes = Vec::new();
        bc5.write(&mut bytes).unwrap();
        let ktx = TextureConverter::convert(&bytes, TextureEncoding::ColorSrgb).unwrap();
        let reader = ktx2::Reader::new(&ktx).unwrap();
        assert_eq!(reader.header().format, None, "BC5 in an sRGB slot is UASTC");
        assert_eq!(
            reader.header().supercompression_scheme,
            Some(ktx2::SupercompressionScheme::Zstandard)
        );
        // The existing UASTC decode helper already decompresses Zstandard,
        // proving the fallback path round-trips through supercompression.
        let pixels = decode_uastc_level(&ktx, 0);
        assert!(!pixels.is_empty());
        inspect_ktx2(&ktx, TextureEncoding::ColorSrgb).unwrap();
    }

    #[test]
    #[ignore = "requires OPENSKYRIM_DDS_FIXTURE with a locally installed cubemap"]
    fn converts_installed_cubemap_fixture() {
        let path = std::env::var_os("OPENSKYRIM_DDS_FIXTURE")
            .map(std::path::PathBuf::from)
            .expect("set OPENSKYRIM_DDS_FIXTURE to a cubemap DDS");
        let bytes = std::fs::read(&path).unwrap();

        let converted = TextureConverter::convert(&bytes, TextureEncoding::ColorSrgb)
            .unwrap_or_else(|error| panic!("failed to convert {}: {error:#}", path.display()));
        let reader = ktx2::Reader::new(&converted).unwrap();
        assert_eq!(reader.header().face_count, 6);
    }

    #[test]
    #[ignore = "requires OPENSKYRIM_COLOR_DDS_FIXTURE with a locally installed color DDS"]
    fn converts_installed_color_fixture() {
        convert_installed_2d_fixture("OPENSKYRIM_COLOR_DDS_FIXTURE", TextureEncoding::ColorSrgb);
    }

    #[test]
    #[ignore = "requires OPENSKYRIM_NORMAL_DDS_FIXTURE with a locally installed normal DDS"]
    fn converts_installed_normal_fixture() {
        convert_installed_2d_fixture(
            "OPENSKYRIM_NORMAL_DDS_FIXTURE",
            TextureEncoding::NormalLinear,
        );
    }

    #[test]
    #[ignore = "requires OPENSKYRIM_VOLUME_DDS_FIXTURE with a locally installed volume DDS"]
    fn converts_installed_volume_fixture() {
        let path = std::env::var_os("OPENSKYRIM_VOLUME_DDS_FIXTURE")
            .map(std::path::PathBuf::from)
            .expect("set OPENSKYRIM_VOLUME_DDS_FIXTURE to a volume DDS");
        let bytes = std::fs::read(&path).unwrap();

        let converted = TextureConverter::convert(&bytes, TextureEncoding::DataLinear)
            .unwrap_or_else(|error| panic!("failed to convert {}: {error:#}", path.display()));
        let reader = ktx2::Reader::new(&converted).unwrap();
        assert_eq!(reader.header().pixel_depth, 128);
        assert_eq!(reader.header().level_count, 8);
    }

    #[test]
    fn decodes_x8r8g8b8_as_opaque_rgba() {
        let mut dds = x8r8g8b8_fixture();
        dds.data.copy_from_slice(&[3, 2, 1, 99, 30, 20, 10, 88]);

        assert_eq!(
            decode_x8r8g8b8(&dds).unwrap(),
            [1, 2, 3, 255, 10, 20, 30, 255]
        );
    }

    #[test]
    fn rejects_truncated_x8r8g8b8() {
        let mut dds = x8r8g8b8_fixture();
        dds.data.pop();

        let error = decode_x8r8g8b8(&dds).unwrap_err();
        let error_chain = format!("{error:#}");
        assert!(error_chain.contains("truncated"), "{error_chain}");
    }

    #[test]
    fn converts_x8r8g8b8_to_runtime_compatible_ktx2() {
        let mut dds = x8r8g8b8_fixture();
        dds.data.copy_from_slice(&[3, 2, 1, 0, 30, 20, 10, 0]);
        let mut bytes = Vec::new();
        dds.write(&mut bytes).unwrap();

        let ktx2 = TextureConverter::convert(&bytes, TextureEncoding::ColorSrgb).unwrap();
        validate_ktx2(&ktx2, TextureEncoding::ColorSrgb).unwrap();
    }

    #[test]
    fn preserves_mipped_x8r8g8b8_chain() {
        let mut dds = Dds::new_d3d(NewD3dParams {
            height: 4,
            width: 4,
            depth: None,
            format: D3DFormat::X8R8G8B8,
            mipmap_levels: Some(3),
            caps2: None,
        })
        .unwrap();
        for (index, byte) in dds.data.iter_mut().enumerate() {
            *byte = index as u8;
        }
        let mut bytes = Vec::new();
        dds.write(&mut bytes).unwrap();

        let ktx2 = TextureConverter::convert(&bytes, TextureEncoding::DataLinear).unwrap();
        let metadata = inspect_ktx2(&ktx2, TextureEncoding::DataLinear).unwrap();
        assert_eq!(metadata.levels, 3);
        assert_eq!((metadata.width, metadata.height), (4, 4));
    }

    #[test]
    fn x8r8g8b8_with_trailing_payload_bytes_still_converts_to_uastc() {
        // The packed path rejects the length mismatch (as it must); main's
        // tolerant X8R8G8B8 decoder ignores bytes past the last mip and encodes
        // the chain with UASTC instead of failing the texture.
        let mut dds = Dds::new_d3d(NewD3dParams {
            height: 4,
            width: 4,
            depth: None,
            format: D3DFormat::X8R8G8B8,
            mipmap_levels: Some(3),
            caps2: None,
        })
        .unwrap();
        for (index, byte) in dds.data.iter_mut().enumerate() {
            *byte = (index * 3) as u8;
        }
        dds.data.extend_from_slice(&[0xAB; 7]);
        let mut bytes = Vec::new();
        dds.write(&mut bytes).unwrap();

        let ktx = TextureConverter::convert(&bytes, TextureEncoding::ColorSrgb).unwrap();
        let reader = ktx2::Reader::new(&ktx).unwrap();
        assert_eq!(reader.header().format, None, "falls back to UASTC");
        let metadata = inspect_ktx2(&ktx, TextureEncoding::ColorSrgb).unwrap();
        assert_eq!(metadata.levels, 3);
        assert_eq!((metadata.width, metadata.height), (4, 4));
    }

    /// Patches the DDS header mip count without touching the payload:
    /// `DDSD_MIPMAPCOUNT` lives in `dwFlags` (offset 8), the count in
    /// `dwMipMapCount` (offset 28), right after the four-byte magic.
    fn with_declared_mip_count(mut bytes: Vec<u8>, mip_count: u32) -> Vec<u8> {
        const DDSD_MIPMAPCOUNT: u32 = 0x2_0000;
        let flags = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) | DDSD_MIPMAPCOUNT;
        bytes[8..12].copy_from_slice(&flags.to_le_bytes());
        bytes[28..32].copy_from_slice(&mip_count.to_le_bytes());
        bytes
    }

    #[test]
    fn rejects_x8r8g8b8_mip_counts_larger_than_the_texture_dimensions() {
        let bytes = dummy_content::dds::generate(
            &dummy_content::dds::Spec::new(dummy_content::dds::Format::X8R8G8B8, 2, 1),
            &mut dummy_content::rng::Rng::new(0),
        )
        .unwrap();
        let bytes = with_declared_mip_count(bytes, u32::MAX);

        let error = TextureConverter::convert(&bytes, TextureEncoding::ColorSrgb).unwrap_err();
        // The packed path refuses the chain, and the generic decoder has no
        // X8R8G8B8 support either, so the dedicated decoder reports the failure.
        let chain = format!("{error:#}");
        assert!(chain.contains("cannot be decoded"), "{chain}");
        assert!(chain.contains("packed path failed first"), "{chain}");
    }

    #[test]
    fn rejects_native_bc_mip_counts_larger_than_the_texture_dimensions() {
        let dds = Dds::new_dxgi(NewDxgiParams {
            height: 8,
            width: 8,
            depth: None,
            format: DxgiFormat::BC1_UNorm,
            mipmap_levels: Some(4),
            array_layers: None,
            caps2: None,
            is_cubemap: false,
            resource_dimension: D3D10ResourceDimension::Texture2D,
            alpha_mode: AlphaMode::Straight,
        })
        .unwrap();
        let mut bytes = Vec::new();
        dds.write(&mut bytes).unwrap();
        let bytes = with_declared_mip_count(bytes, 40);

        let error = TextureConverter::convert(&bytes, TextureEncoding::DataLinear).unwrap_err();
        let chain = format!("{error:#}");
        assert!(chain.contains("declares 40 mip levels"), "{chain}");
    }

    #[test]
    fn a_declared_mip_count_of_zero_converts_the_base_level() {
        let bytes = dummy_content::dds::generate(
            &dummy_content::dds::Spec::new(dummy_content::dds::Format::X8R8G8B8, 2, 1),
            &mut dummy_content::rng::Rng::new(0),
        )
        .unwrap();
        let bytes = with_declared_mip_count(bytes, 0);

        TextureConverter::convert(&bytes, TextureEncoding::ColorSrgb).unwrap();
    }

    #[test]
    fn rejects_l8_volume_mip_counts_larger_than_the_texture_dimensions() {
        let mut dds = Dds::new_d3d(NewD3dParams {
            height: 4,
            width: 4,
            depth: Some(4),
            format: D3DFormat::L8,
            mipmap_levels: Some(3),
            caps2: None,
        })
        .unwrap();
        for (index, byte) in dds.data.iter_mut().enumerate() {
            *byte = index as u8;
        }
        let mut bytes = Vec::new();
        dds.write(&mut bytes).unwrap();
        // ddsfile writes L8 as uncompressed RGB; mark the pixel format as legacy
        // luminance (`DDS_PIXELFORMAT::dwFlags` at offset 80) so the volume
        // falls back to the L8 decoder.
        bytes[80..84].copy_from_slice(&0x2_0000_u32.to_le_bytes());
        let bytes = with_declared_mip_count(bytes, u32::MAX);

        let error = TextureConverter::convert(&bytes, TextureEncoding::DataLinear).unwrap_err();
        let chain = format!("{error:#}");
        assert!(chain.contains("mip levels"), "{chain}");
    }

    #[test]
    fn atomically_replaces_an_existing_published_texture() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("fixture.dds");
        let output = directory.path().join("fixture.ktx2");
        let mut dds = x8r8g8b8_fixture();
        dds.data.copy_from_slice(&[3, 2, 1, 0, 30, 20, 10, 0]);
        let mut bytes = Vec::new();
        dds.write(&mut bytes).unwrap();
        fs::write(&input, bytes).unwrap();

        TextureConverter::convert_dds_to_ktx2(&input, &output, TextureEncoding::ColorSrgb).unwrap();
        let first = fs::read(&output).unwrap();
        TextureConverter::convert_dds_to_ktx2(&input, &output, TextureEncoding::DataLinear)
            .unwrap();
        let second = fs::read(&output).unwrap();
        assert_ne!(first, second);
        inspect_ktx2(&second, TextureEncoding::DataLinear).unwrap();
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn generated_dds_never_panic_under_truncation_or_mutation() {
        let mut rng = dummy_content::rng::Rng::new(9);
        let specs = [
            dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc1Unorm, 8, 8)
                .with_mip_levels(4),
            dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc5Unorm, 8, 8),
            dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc7Unorm, 8, 8),
            dummy_content::dds::Spec::new(dummy_content::dds::Format::X8R8G8B8, 4, 4),
            dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc1Unorm, 4, 4).as_cubemap(),
            dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc1Unorm, 4, 4).with_depth(4),
        ];
        for spec in specs {
            let bytes = dummy_content::dds::generate(&spec, &mut rng).unwrap();
            let mut lengths: Vec<usize> = (0..bytes.len()).step_by(31).collect();
            lengths.extend(0..bytes.len().min(256));
            for length in lengths {
                let result = std::panic::catch_unwind(|| {
                    TextureConverter::convert(&bytes[..length], TextureEncoding::ColorSrgb)
                });
                assert!(result.is_ok(), "DDS converter panicked at length {length}");
            }
            for _ in 0..128 {
                let mut mutated = bytes.clone();
                let index = rng.next_u64() as usize % mutated.len();
                mutated[index] ^= 0xff;
                let result = std::panic::catch_unwind(|| {
                    TextureConverter::convert(&mutated, TextureEncoding::NormalLinear)
                });
                assert!(
                    result.is_ok(),
                    "DDS converter panicked on mutation at {index}"
                );
            }
        }
    }

    fn x8r8g8b8_fixture() -> Dds {
        let bytes = dummy_content::dds::generate(
            &dummy_content::dds::Spec::new(dummy_content::dds::Format::X8R8G8B8, 2, 1),
            &mut dummy_content::rng::Rng::new(0),
        )
        .unwrap();
        Dds::read(bytes.as_slice()).unwrap()
    }

    fn convert_installed_2d_fixture(variable: &str, encoding: TextureEncoding) {
        let path = std::env::var_os(variable)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| panic!("set {variable} to an installed DDS"));
        let dds_bytes = std::fs::read(&path).unwrap();
        let dds = Dds::read(Cursor::new(&dds_bytes)).unwrap();
        let converted = TextureConverter::convert(&dds_bytes, encoding)
            .unwrap_or_else(|error| panic!("failed to convert {}: {error:#}", path.display()));
        let metadata = inspect_ktx2(&converted, encoding).unwrap();
        assert_eq!(metadata.width, dds.get_width());
        assert_eq!(metadata.height, dds.get_height());
        assert_eq!(metadata.levels, dds.get_num_mipmap_levels());
        assert_eq!(metadata.faces, 1);
    }

    fn decode_zstd_level(bytes: &[u8], mip: usize) -> Vec<u8> {
        let reader = ktx2::Reader::new(bytes).unwrap();
        let level = reader.levels().nth(mip).unwrap();
        assert_eq!(
            reader.header().supercompression_scheme,
            Some(ktx2::SupercompressionScheme::Zstandard)
        );
        assert_eq!(
            level.uncompressed_byte_length as usize,
            {
                let mut cursor = Cursor::new(level.data);
                let mut decoder = ruzstd::decoding::StreamingDecoder::new(&mut cursor).unwrap();
                let mut out = Vec::new();
                decoder.read_to_end(&mut out).unwrap();
                out.len()
            },
            "uncompressed length must match decoded bytes"
        );
        let mut cursor = Cursor::new(level.data);
        let mut decoder = ruzstd::decoding::StreamingDecoder::new(&mut cursor).unwrap();
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).unwrap();
        out
    }

    fn decode_uastc_level(bytes: &[u8], mip: usize) -> Vec<u8> {
        let reader = ktx2::Reader::new(bytes).unwrap();
        let header = reader.header();
        let level = reader.levels().nth(mip).unwrap();
        let mut uastc = Vec::new();
        match header.supercompression_scheme {
            Some(ktx2::SupercompressionScheme::Zstandard) => {
                let mut cursor = Cursor::new(level.data);
                let mut decoder = ruzstd::decoding::StreamingDecoder::new(&mut cursor).unwrap();
                decoder.read_to_end(&mut uastc).unwrap();
            }
            Some(ktx2::SupercompressionScheme::ZLIB) => {
                let mut decoder = flate2::bufread::ZlibDecoder::new(level.data);
                decoder.read_to_end(&mut uastc).unwrap();
            }
            None => uastc.extend_from_slice(level.data),
            other => panic!("unsupported test supercompression {other:?}"),
        }
        let width = (header.pixel_width >> mip).max(1);
        let height = (header.pixel_height.max(1) >> mip).max(1);
        let bc7 = LowLevelUastcTranscoder::new()
            .transcode_slice(
                &uastc,
                SliceParametersUastc {
                    num_blocks_x: width.div_ceil(4),
                    num_blocks_y: height.div_ceil(4),
                    has_alpha: true,
                    original_width: width,
                    original_height: height,
                },
                DecodeFlags::HIGH_QUALITY,
                TranscoderBlockFormat::BC7,
            )
            .unwrap();
        let mut dds = Dds::new_dxgi(NewDxgiParams {
            height,
            width,
            depth: None,
            format: DxgiFormat::BC7_UNorm,
            mipmap_levels: None,
            array_layers: None,
            caps2: None,
            is_cubemap: false,
            resource_dimension: D3D10ResourceDimension::Texture2D,
            alpha_mode: AlphaMode::Straight,
        })
        .unwrap();
        dds.data.copy_from_slice(&bc7);
        image_dds::SurfaceRgba8::decode_dds(&dds)
            .unwrap()
            .get_image(0, 0, 0)
            .unwrap()
            .into_raw()
    }
}

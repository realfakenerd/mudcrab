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
const UASTC_LEVEL_DEFAULT: u8 = 2;
const ETC1S_QUALITY_DEFAULT: u8 = 192;
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
    pub(crate) fn encode_rgba_mips(
        width: u32,
        height: u32,
        mip_rgba: &[Vec<u8>],
        encoding: TextureEncoding,
    ) -> Result<Vec<u8>> {
        ensure!(
            width > 0 && height > 0,
            "texture dimensions must be non-zero"
        );
        ensure!(!mip_rgba.is_empty(), "RGBA mip chain is empty");
        let max_levels = u32::BITS - width.max(height).leading_zeros();
        ensure!(
            mip_rgba.len() <= max_levels as usize,
            "RGBA mip chain has more than {max_levels} levels"
        );

        let mut encoded_levels = Vec::with_capacity(mip_rgba.len());
        for (mip, rgba) in mip_rgba.iter().enumerate() {
            let mip_width = (width >> mip).max(1);
            let mip_height = (height >> mip).max(1);
            ensure!(
                rgba.len() == mip_width as usize * mip_height as usize * 4,
                "RGBA mip {mip} has an invalid byte length"
            );
            encoded_levels.push(encode_basis_ktx2(
                mip_width,
                mip_height,
                rgba,
                encoding,
                false,
                ETC1S_QUALITY_DEFAULT,
                UASTC_LEVEL_DEFAULT,
            )?);
        }
        let template = encode_basis_ktx2(
            width,
            height,
            &mip_rgba[0],
            encoding,
            true,
            ETC1S_QUALITY_DEFAULT,
            UASTC_LEVEL_DEFAULT,
        )?;
        let bytes = combine_ktx2_mip_levels(&template, &encoded_levels)?;
        validate_ktx2(&bytes, encoding)?;
        Ok(bytes)
    }

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

        let result = match image_dds::SurfaceRgba8::decode_dds(&dds) {
            Ok(surface) => encode_2d_surface(&surface, encoding, etc1s_quality, uastc_level)?,
            Err(_) if dds.get_d3d_format() == Some(D3DFormat::X8R8G8B8) => {
                encode_x8r8g8b8(&dds, encoding, etc1s_quality, uastc_level)?
            }
            Err(error) => return Err(error).wrap_err("DDS pixel format cannot be decoded"),
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

    let (dfd, type_size) = ktx2::dfd::Basic::from_format(format)
        .map_err(|error| color_eyre::eyre::eyre!("no KTX2 descriptor for {format:?}: {error:?}"))?;
    let dfd_bytes = ktx2::dfd::Block::Basic(dfd).to_vec();
    let level_count = u32::try_from(mip_count).wrap_err("too many DDS mip levels")?;
    let header = ktx2::Header {
        format: Some(format),
        type_size,
        pixel_width: dds.get_width(),
        pixel_height: dds.get_height(),
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
    let mut mip_offset_in_face = 0usize;
    for mip in 0..mip_count {
        let mip_len = native_mip_byte_size(dds, mip, block_bytes)?;
        let mut level_data = Vec::with_capacity(mip_len * faces as usize);
        for face in 0..faces as usize {
            let start = face * face_stride + mip_offset_in_face;
            level_data.extend_from_slice(&dds.data[start..start + mip_len]);
        }
        let uncompressed = level_data.len() as u64;
        // Supercompression applies to the complete assembled level (all of
        // its faces/slices), never to faces independently: independently
        // compressed faces would differ in length and break level assembly.
        let stored = compress_level(&level_data, zstd_level)?;
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
        mip_offset_in_face += mip_len;
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

fn encode_2d_surface(
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

fn encode_x8r8g8b8(
    dds: &Dds,
    encoding: TextureEncoding,
    etc1s_quality: u8,
    uastc_level: u8,
) -> Result<Vec<u8>> {
    let decoded = decode_x8r8g8b8_mips(dds)?;
    let mut levels = Vec::with_capacity(decoded.len());
    for (width, height, rgba) in &decoded {
        levels.push(encode_basis_ktx2(
            *width,
            *height,
            rgba,
            encoding,
            false,
            etc1s_quality,
            uastc_level,
        )?);
    }
    if levels.len() == 1 {
        return Ok(levels.pop().expect("one encoded X8R8G8B8 mip"));
    }
    let (width, height, rgba) = &decoded[0];
    let template = encode_basis_ktx2(
        *width,
        *height,
        rgba,
        encoding,
        true,
        etc1s_quality,
        uastc_level,
    )?;
    combine_ktx2_mip_levels(&template, &levels)
}

/// Decodes every mip level of an X8R8G8B8 DDS to RGBA8 with opaque alpha.
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

#[cfg(test)]
fn decode_x8r8g8b8(dds: &Dds) -> Result<Vec<u8>> {
    decode_x8r8g8b8_mips(dds)?
        .into_iter()
        .next()
        .map(|(_, _, rgba)| rgba)
        .ok_or_else(|| color_eyre::eyre::eyre!("X8R8G8B8 DDS has no mip levels"))
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
    fn encodes_an_explicit_partial_srgb_mip_chain() {
        let levels = vec![
            [96, 48, 24, 255].repeat(64),
            [96, 48, 24, 255].repeat(16),
            [96, 48, 24, 255].repeat(4),
        ];
        let bytes =
            TextureConverter::encode_rgba_mips(8, 8, &levels, TextureEncoding::ColorSrgb).unwrap();
        let reader = ktx2::Reader::new(&bytes).unwrap();
        assert_eq!(reader.header().level_count, 3);
        assert_eq!(reader.levels().count(), 3);
        assert_eq!(
            reader.transfer_function(),
            Some(ktx2::TransferFunction::SRGB)
        );
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
    fn uncompressed_rgba_preserves_and_legacy_packed_falls_back_to_uastc() {
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
        assert_eq!(reader.header().format, None, "X8R8G8B8 stays UASTC");
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
        let x8 = x8r8g8b8_fixture();
        let mut bytes = Vec::new();
        x8.write(&mut bytes).unwrap();
        let ktx = TextureConverter::convert(&bytes, TextureEncoding::ColorSrgb).unwrap();
        let reader = ktx2::Reader::new(&ktx).unwrap();
        assert_eq!(reader.header().format, None, "X8R8G8B8 stays UASTC");
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
        let chain = format!("{error:#}");
        assert!(chain.contains("mip levels"), "{chain}");
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

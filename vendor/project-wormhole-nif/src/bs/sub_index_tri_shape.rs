use super::prelude::*;

/// Skyrim's distant object LOD shape (`meshes/terrain/**/objects/*.bto`): the
/// ordinary triangle payload of a [`BSTriShape`] followed by the segment table
/// that maps each merged object onto its triangle range.
///
/// The layout below is measured, not inherited: it fits all 2392
/// `BSSubIndexTriShape` blocks in the 1078 shipped `.bto` files exactly, and in
/// every one of them the segment entries' primitive counts sum to that shape's
/// triangle count. The parse rejects a table that breaks either rule, so a
/// misread layout fails instead of silently keeping a wrong segment list.
#[derive(Debug, Clone)]
pub struct BSSubIndexTriShape {
    pub bs_tri_shape: BSTriShape,
    /// Number of segment entries that follow the shape payload.
    pub num_segments: u32,
    pub segments: Vec<BSGeometrySegmentData>,
}

/// One entry of a `BSSubIndexTriShape`'s segment table.
///
/// In every shipped block the flag byte is `0` and the middle value stays below
/// `u16::MAX`; the converter does not consume the middle value yet, so it is
/// kept verbatim rather than interpreted.
#[derive(Debug, Clone, Copy, NomLE)]
pub struct BSGeometrySegmentData {
    pub flag: u8,
    pub value: u32,
    pub num_primitives: u32,
}

impl Parse<&[u8]> for BSSubIndexTriShape {
    fn parse(i: &[u8]) -> IResult<&[u8], Self, nom::error::Error<&[u8]>> {
        let (i, bs_tri_shape) = BSTriShape::parse(i)?;

        // Every Skyrim shape ends with a `u32` that `BSTriShape::parse` leaves
        // unread, and the segment table starts after it. The value is zero in
        // every shipped LOD block, so only its width matters here.
        let (i, _trailing) = le_u32(i)?;
        let (i, num_segments) = le_u32(i)?;
        if num_segments as usize > i.len() / 9 {
            return Err(nom::Err::Failure(nom::error::Error::new(
                i,
                nom::error::ErrorKind::Count,
            )));
        }
        let (i, segments) = count(BSGeometrySegmentData::parse, num_segments as usize)(i)?;

        // The segments partition the shape's triangles. A wrong layout would
        // otherwise accept any count and only show up as leftover bytes.
        let primitives = segments
            .iter()
            .map(|segment| u64::from(segment.num_primitives))
            .sum::<u64>();
        if primitives != u64::from(bs_tri_shape.num_triangles) {
            return Err(nom::Err::Failure(nom::error::Error::new(
                i,
                nom::error::ErrorKind::Verify,
            )));
        }

        Ok((
            i,
            BSSubIndexTriShape {
                bs_tri_shape,
                num_segments,
                segments,
            },
        ))
    }
}

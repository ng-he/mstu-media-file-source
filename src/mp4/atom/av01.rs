use std::io::Seek;

use bytes::Bytes;
use byteorder::{BigEndian, ReadBytesExt};

use crate::mp4::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Av01 {
    pub data_reference_index: u16,
    pub width: u16,
    pub height: u16,
    pub horizontal_resolution: (u16, u16),
    pub vertical_resolution: (u16, u16),
    pub frame_count: u16,
    pub depth: u16,
    pub av1c: Av1c,
}

impl Av01 {
    pub fn read<R: Read + Seek>(reader: &mut R, size: u64) -> Result<Self> {
        let start = atom::read_position(reader)?;

        atom::read_skip_bytes(reader, 6)?; // reserved
        let data_reference_index = reader.read_u16::<BigEndian>()?;

        atom::read_skip_bytes(reader, 16)?; // pre-defined, reserved

        let width = reader.read_u16::<BigEndian>()?;
        let height = reader.read_u16::<BigEndian>()?;

        let horizontal_resolution = (
            reader.read_u16::<BigEndian>()?,
            reader.read_u16::<BigEndian>()?,
        );

        let vertical_resolution = (
            reader.read_u16::<BigEndian>()?,
            reader.read_u16::<BigEndian>()?,
        );

        atom::read_skip_bytes(reader, 4)?; // reserved
        let frame_count = reader.read_u16::<BigEndian>()?;
        atom::read_skip_bytes(reader, 32)?; // compressor name
        let depth = reader.read_u16::<BigEndian>()?;
        atom::read_skip_bytes(reader, 2)?; // pre-defined

        // av1C is required, but boxes may precede it in a sample entry.
        let end = start + size;
        let mut av1c = None;

        while reader.stream_position()? + atom::HEADER_SIZE <= end {
            let header = atom::read_header(reader)?;
            let at = reader.stream_position()? - atom::HEADER_SIZE;

            if header.size > size {
                return Err(Error::InvalidData(
                    "av01 box contains a box with a larger size than it",
                ));
            }

            if header.name == atom::constant_name::AV1C {
                av1c = Some(Av1c::read(reader, header.size)?);
                break;
            }

            atom::read_skip_bytes_to(reader, at + header.size)?;
        }

        atom::read_skip_bytes_to(reader, end)?;

        Ok(Self {
            data_reference_index,
            width,
            height,
            horizontal_resolution,
            vertical_resolution,
            frame_count,
            depth,
            av1c: av1c.ok_or(Error::AtomNotFound(atom::constant_name::AV1C))?,
        })
    }
}

/// The AV1 configuration record: a few fields, then the sequence header the
/// stream starts from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Av1c {
    pub profile: u8,
    pub level: u8,
    pub tier: u8,
    pub bit_depth: u8,
    pub monochrome: bool,
    pub config_obus: Bytes,
}

impl Av1c {
    pub fn read<R: Read + Seek>(reader: &mut R, size: u64) -> Result<Self> {
        let start = atom::read_position(reader)?;

        let marker_version = reader.read_u8()?;

        if marker_version & 0x80 == 0 {
            return Err(Error::InvalidData("av1C marker bit is not set"));
        }

        let byte = reader.read_u8()?;
        let profile = byte >> 5;
        let level = byte & 0x1F;

        let byte = reader.read_u8()?;
        let tier = byte >> 7;
        let high_bitdepth = byte & 0x40 != 0;
        let twelve_bit = byte & 0x20 != 0;
        let monochrome = byte & 0x10 != 0;

        reader.read_u8()?; // delay flags

        // Whatever is left is the sequence header, kept as it is written.
        let read = reader.stream_position()? - start;
        let mut config_obus = vec![0u8; size.saturating_sub(read) as usize];
        reader.read_exact(&mut config_obus)?;

        atom::read_skip_bytes_to(reader, start + size)?;

        Ok(Self {
            profile,
            level,
            tier,
            bit_depth: match (high_bitdepth, twelve_bit) {
                (true, true) => 12,
                (true, false) => 10,
                _ => 8,
            },
            monochrome,
            config_obus: Bytes::from(config_obus),
        })
    }
}

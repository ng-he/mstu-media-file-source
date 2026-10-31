use std::io::Seek;

use byteorder::{BigEndian, ReadBytesExt};
use bytes::Bytes;

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

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Seek, SeekFrom};

    use super::*;

    /// An av1C as ffmpeg writes one: four bytes of fields, then the sequence
    /// header obu the stream starts from.
    const AV1C: &[u8] = &[
        0x00, 0x00, 0x00, 0x15, 0x61, 0x76, 0x31, 0x43, // box header
        0x81, 0x00, 0x0c, 0x00, // marker, version, profile, level, flags
        0x0a, 0x0b, 0x00, 0x00, 0x00, 0x04, 0x3c, 0xff, 0xbc, 0x02, 0xf8, 0x00, 0x40,
    ];

    fn read(bytes: &[u8]) -> Result<Av1c> {
        let mut reader = Cursor::new(bytes);
        reader.seek(SeekFrom::Start(8)).unwrap();

        Av1c::read(&mut reader, bytes.len() as u64)
    }

    #[test]
    fn keeps_the_sequence_header_exactly_as_written() {
        let av1c = read(AV1C).unwrap();

        assert_eq!(av1c.profile, 0);
        assert_eq!(av1c.level, 0);
        assert_eq!(av1c.bit_depth, 8);
        assert!(!av1c.monochrome);
        assert_eq!(
            av1c.config_obus.as_ref(),
            &AV1C[12..],
            "the obus are passed through untouched"
        );
    }

    #[test]
    fn reads_profile_level_and_depth_out_of_their_bits() {
        let mut bytes = AV1C.to_vec();
        bytes[9] = 0b001_01101; // profile 1, level 13
        bytes[10] = 0b1_1_1_1_0000; // tier 1, high bitdepth, twelve bit, monochrome

        let av1c = read(&bytes).unwrap();

        assert_eq!(av1c.profile, 1);
        assert_eq!(av1c.level, 13);
        assert_eq!(av1c.tier, 1);
        assert_eq!(av1c.bit_depth, 12);
        assert!(av1c.monochrome);
    }

    #[test]
    fn refuses_a_record_without_its_marker() {
        let mut bytes = AV1C.to_vec();
        bytes[8] = 0x01; // marker bit cleared

        assert!(read(&bytes).is_err());
    }

    #[test]
    fn a_record_carrying_no_obus_is_still_read() {
        let mut bytes = AV1C[..12].to_vec();
        bytes[3] = 12; // the box is only its fields

        let av1c = read(&bytes).unwrap();

        assert!(av1c.config_obus.is_empty());
    }
}

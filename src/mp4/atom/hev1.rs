use std::io::Seek;

use byteorder::{BigEndian, ReadBytesExt};
use mstu_media::codec::video::h265;

use crate::media::nalu::Nalu;
use crate::mp4::*;
use crate::types::FixedU16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hev1 {
    pub data_reference_index: u16,
    pub width: u16,
    pub height: u16,
    pub horizontal_resolution: FixedU16,
    pub vertical_resolution: FixedU16,
    pub frame_count: u16,
    pub depth: u16,
    pub hvcc: HvcC,
}

impl Hev1 {
    pub fn read<R: Read + Seek>(reader: &mut R, size: u64) -> Result<Self> {
        let start = atom::read_position(reader)?;

        reader.read_u32::<BigEndian>()?; // reserved
        reader.read_u16::<BigEndian>()?; // reserved
        let data_reference_index = reader.read_u16::<BigEndian>()?;

        reader.read_u32::<BigEndian>()?; // pre-defined, reserved
        reader.read_u64::<BigEndian>()?; // pre-defined
        reader.read_u32::<BigEndian>()?; // pre-defined
        let width = reader.read_u16::<BigEndian>()?;
        let height = reader.read_u16::<BigEndian>()?;
        let horizontal_resolution = FixedU16::from_raw(reader.read_u32::<BigEndian>()?);
        let vertical_resolution = FixedU16::from_raw(reader.read_u32::<BigEndian>()?);
        reader.read_u32::<BigEndian>()?; // reserved
        let frame_count = reader.read_u16::<BigEndian>()?;
        atom::read_skip_bytes(reader, 32)?; // compressorname
        let depth = reader.read_u16::<BigEndian>()?;
        reader.read_i16::<BigEndian>()?; // pre-defined

        let header = atom::read_header(reader)?;
        let atom::Header { name, size: s } = header;
        if s > size {
            return Err(Error::InvalidData(
                "hev1 box contains a box with a larger size than it",
            ));
        }
        if name == atom::constant_name::HVCC {
            let hvcc = HvcC::read(reader, s)?;

            atom::read_skip_bytes_to(reader, start + size)?;

            Ok(Hev1 {
                data_reference_index,
                width,
                height,
                horizontal_resolution,
                vertical_resolution,
                frame_count,
                depth,
                hvcc,
            })
        } else {
            Err(Error::AtomNotFound(atom::constant_name::HVCC))
        }
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct HvcC {
    pub configuration_version: u8,
    pub general_profile_space: u8,
    pub general_tier_flag: bool,
    pub general_profile_idc: u8,
    pub general_profile_compatibility_flags: u32,
    pub general_constraint_indicator_flag: u64,
    pub general_level_idc: u8,
    pub min_spatial_segmentation_idc: u16,
    pub parallelism_type: u8,
    pub chroma_format_idc: u8,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub avg_frame_rate: u16,
    pub constant_frame_rate: u8,
    pub num_temporal_layers: u8,
    pub temporal_id_nested: bool,
    pub length_size_minus_one: u8,
    pub arrays: Vec<HvcCArray>,
}

impl HvcC {
    pub fn read<R: Read + Seek>(reader: &mut R, _size: u64) -> Result<Self> {
        let configuration_version = reader.read_u8()?;
        let params = reader.read_u8()?;
        let general_profile_space = (params >> 6) & 0b11;
        let general_tier_flag = (params >> 5) & 0b1 > 0;
        let general_profile_idc = params & 0b00011111;

        let general_profile_compatibility_flags = reader.read_u32::<BigEndian>()?;
        let general_constraint_indicator_flag = reader.read_u48::<BigEndian>()?;
        let general_level_idc = reader.read_u8()?;
        let min_spatial_segmentation_idc = reader.read_u16::<BigEndian>()? & 0x0FFF;
        let parallelism_type = reader.read_u8()? & 0b11;
        let chroma_format_idc = reader.read_u8()? & 0b11;
        let bit_depth_luma_minus8 = reader.read_u8()? & 0b111;
        let bit_depth_chroma_minus8 = reader.read_u8()? & 0b111;
        let avg_frame_rate = reader.read_u16::<BigEndian>()?;

        let params = reader.read_u8()?;
        let constant_frame_rate = (params >> 6) & 0b11;
        let num_temporal_layers = (params >> 3) & 0b111;
        let temporal_id_nested = (params >> 2) & 0b1 > 0;
        let length_size_minus_one = params & 0b000011;

        let num_of_arrays = reader.read_u8()?;

        let mut arrays = Vec::with_capacity(num_of_arrays as _);
        for _ in 0..num_of_arrays {
            let params = reader.read_u8()?;
            let num_nalus = reader.read_u16::<BigEndian>()?;
            let mut nalus = Vec::with_capacity(num_nalus as usize);

            for _ in 0..num_nalus {
                let size = reader.read_u16::<BigEndian>()?;
                let mut data = vec![0; size as usize];

                reader.read_exact(&mut data)?;

                nalus.push(Nalu::from(data.as_slice()))
            }

            arrays.push(HvcCArray {
                completeness: (params & 0b10000000) > 0,
                nalu_type: h265::NaluType::from(params & 0b111111),
                nalus,
            });
        }

        Ok(HvcC {
            configuration_version,
            general_profile_space,
            general_tier_flag,
            general_profile_idc,
            general_profile_compatibility_flags,
            general_constraint_indicator_flag,
            general_level_idc,
            min_spatial_segmentation_idc,
            parallelism_type,
            chroma_format_idc,
            bit_depth_luma_minus8,
            bit_depth_chroma_minus8,
            avg_frame_rate,
            constant_frame_rate,
            num_temporal_layers,
            temporal_id_nested,
            length_size_minus_one,
            arrays,
        })
    }

    pub fn nalus(&self, t: h265::NaluType) -> impl Iterator<Item = &Nalu> {
        self.arrays
            .iter()
            .filter(move |array| array.nalu_type == t)
            .flat_map(|array| array.nalus.iter())
    }

    /// The parameter sets as the pipeline carries them: video, then sequence,
    /// then picture, which is the order a decoder reads them in and the order
    /// this box is free not to store them in.
    pub fn parameters(&self) -> h265::Parameters {
        let kinds = [
            h265::NaluType::Vps,
            h265::NaluType::Sps,
            h265::NaluType::Pps,
        ];

        h265::Parameters::from_units(
            kinds
                .into_iter()
                .flat_map(|kind| self.nalus(kind))
                .map(|nalu| nalu.as_bytes()),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HvcCArray {
    pub completeness: bool,
    pub nalu_type: h265::NaluType,
    pub nalus: Vec<Nalu>,
}

#[cfg(test)]
mod tests {
    use crate::mp4::fixture;

    /// Every one of these is a bit field, and `>>` binds tighter than `&`:
    /// the masks were being shifted rather than the byte.
    #[test]
    fn the_bit_fields_are_read_out_of_the_right_bits() {
        let hvcc = fixture::read_hvcc();

        assert_eq!(hvcc.configuration_version, 1);
        assert_eq!(hvcc.general_profile_space, 0);
        assert!(!hvcc.general_tier_flag, "the fixture is main tier");
        assert_eq!(hvcc.general_profile_idc, 1, "main profile");

        assert_eq!(
            hvcc.constant_frame_rate, 0,
            "unknown, as a camera leaves it"
        );
        assert_eq!(hvcc.num_temporal_layers, 1);
        assert!(hvcc.temporal_id_nested);
        assert_eq!(hvcc.length_size_minus_one, 3, "four byte lengths");
    }

    #[test]
    fn the_sets_come_out_in_the_order_a_decoder_reads_them() {
        let params = fixture::read_hvcc().parameters();

        assert_eq!(params.units().count(), 1, "the fixture holds one sps");
        assert_eq!(params.sps().as_deref(), Some([0x42, 0x01, 0x01].as_slice()));
        assert_eq!(params.as_bytes()[..4], [0, 0, 0, 1]);
    }
}

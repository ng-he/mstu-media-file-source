use bytes::Bytes;

use std::{
    collections::HashMap,
    fs::{self},
    io::{BufReader, Read, Seek, SeekFrom},
    matches,
    path::PathBuf,
    time::Duration,
    write,
};

use crate::{
    file::{self, Sample, SampleInfo},
    media::FourCC,
};
use error::Error;

pub mod atom;

/// Small mp4 files, built byte by byte, for testing the reader on.
#[cfg(test)]
pub mod fixture;
pub mod error;
pub mod track;

pub use track::Track;
pub use track::TrackType;

pub type Result<T> = std::result::Result<T, Error>;

pub struct Reader {
    ftyp: atom::Ftyp,
    moov: atom::Moov,
    tracks: HashMap<u32, Track>,
    size: u64,
}

impl Reader {
    pub fn read<R: Read + Seek>(reader: &mut R, size: u64) -> Result<Reader> {
        let start = reader.stream_position()?;

        let mut ftyp = None;
        let mut moov = None;
        let mut current = start;

        while current < size {
            let header = atom::read_header(reader)?;
            if header.size > size {
                return Err(Error::InvalidData(
                    "Reader contains a atom with a larger size than it",
                ));
            }

            // Break if size zero Header, which can result in dead-loop.
            if header.size == 0 {
                break;
            }

            match header.name {
                atom::constant_name::FTYP => ftyp = Some(atom::Ftyp::read(reader, header.size)?),
                atom::constant_name::MOOV => moov = Some(atom::Moov::read(reader, header.size)?),
                _ => {
                    atom::read_skip(reader, header.size)?;
                }
            }
            current = reader.stream_position()?;
        }

        if ftyp.is_none() {
            return Err(Error::AtomNotFound(atom::constant_name::FTYP));
        }

        if moov.is_none() {
            return Err(Error::AtomNotFound(atom::constant_name::MOOV));
        }

        let size = current - start;
        let tracks = if let Some(ref moov) = moov {
            if moov.traks.iter().any(|trak| trak.tkhd.track_id == 0) {
                return Err(Error::InvalidData("illegal track id 0"));
            }

            moov.traks
                .iter()
                .map(|trak| (trak.tkhd.track_id, Track::from(trak)))
                .collect()
        } else {
            HashMap::new()
        };

        Ok(Reader {
            ftyp: ftyp.unwrap(),
            moov: moov.unwrap(),
            tracks: tracks,
            size: size,
        })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn major_brand(&self) -> &FourCC {
        &self.ftyp.major_brand
    }

    pub fn minor_version(&self) -> u32 {
        self.ftyp.minor_version
    }

    pub fn compatible_brands(&self) -> &[FourCC] {
        &self.ftyp.compatible_brands
    }

    pub fn duration(&self) -> Duration {
        Duration::from_millis(self.moov.mvhd.duration * 1000 / self.moov.mvhd.timescale as u64)
    }

    pub fn timescale(&self) -> u32 {
        self.moov.mvhd.timescale
    }

    pub fn tracks(&self) -> &HashMap<u32, Track> {
        &self.tracks
    }

    pub fn sample_count(&self, track_id: u32) -> Result<u32> {
        if let Some(track) = self.tracks.get(&track_id) {
            Ok(track.sample_count())
        } else {
            Err(Error::TrakNotFound(track_id))
        }
    }

    pub fn sample_offset(&mut self, track_id: u32, sample_id: u32) -> Result<u64> {
        if let Some(track) = self.tracks.get(&track_id) {
            track.sample_offset(sample_id)
        } else {
            Err(Error::TrakNotFound(track_id))
        }
    }

    pub fn read_sample<R: Read + Seek>(
        &mut self,
        reader: &mut R,
        track_id: u32,
        sample_id: u32,
    ) -> Result<Option<Sample>> {
        if let Some(track) = self.tracks.get(&track_id) {
            track.read_sample(reader, sample_id)
        } else {
            Err(Error::TrakNotFound(track_id))
        }
    }

    pub fn sample_info(&self, track_id: u32, sample_id: u32) -> Result<Option<SampleInfo>> {
        match self.tracks.get(&track_id) {
            Some(track) => track.sample_info(sample_id),
            None => Err(Error::TrakNotFound(track_id)),
        }
    }

    pub fn read_sample_into<R: Read + Seek>(
        &self,
        reader: &mut R,
        track_id: u32,
        sample_id: u32,
        into: &mut [u8],
    ) -> Result<()> {
        match self.tracks.get(&track_id) {
            Some(track) => track.read_sample_into(reader, sample_id, into),
            None => Err(Error::TrakNotFound(track_id)),
        }
    }

    /// Annex-B is the same size as AVCC only with 4-byte prefixes; anything
    /// shorter grows, so its sample cannot be read straight into place.
    pub fn reads_in_place(&self, track_id: u32) -> bool {
        self.tracks
            .get(&track_id)
            .is_some_and(|track| matches!(track.nalu_length_size(), None | Some(4)))
    }
}

pub struct File {
    buf_reader: BufReader<fs::File>,
    reader: Reader,
    video_track_id: u32,
    current_sample_id: u32,

    /// Whether the samples carry their own AV1 sequence header.
    inband_sequence_header: bool,

    /// Only for prefixes shorter than 4 bytes, where Annex-B does not fit
    /// where AVCC was.
    converted: Option<Bytes>,
}

impl File {
    fn peek_converted(&mut self) -> std::result::Result<Option<SampleInfo>, String> {
        if self.converted.is_none() {
            let sample = self
                .reader
                .read_sample(
                    &mut self.buf_reader,
                    self.video_track_id,
                    self.current_sample_id,
                )
                .map_err(|err| err.to_string())?;

            let Some(sample) = sample else {
                return Ok(None);
            };

            self.converted = Some(sample.bytes.clone());

            return Ok(Some(SampleInfo {
                start_time: sample.start_time,
                duration: sample.duration,
                rendering_offset: sample.rendering_offset,
                is_sync: sample.is_sync,
                size: sample.bytes.len(),
            }));
        }

        self.reader
            .sample_info(self.video_track_id, self.current_sample_id)
            .map_err(|err| err.to_string())
    }
}

pub fn open(path: PathBuf) -> Result<File> {
    let file = fs::File::open(path)?;
    let size = file.metadata()?.len();

    let mut buf_reader = BufReader::new(file);

    let reader = Reader::read(&mut buf_reader, size)?;

    let video_track_id = reader
        .tracks()
        .iter()
        .find(|(_, track)| matches!(track.track_type(), Ok(TrackType::Video)))
        .map(|(id, _)| *id)
        .ok_or(Error::InvalidData("video track not found"))?;

    let inband_sequence_header = starts_with_sequence_header(&reader, &mut buf_reader, video_track_id);

    Ok(File {
        buf_reader,
        reader,
        video_track_id,
        current_sample_id: 1,
        converted: None,
        inband_sequence_header,
    })
}

/// Whether the first sample opens with an AV1 sequence header.
///
/// One in the stream and a second from the configuration are two answers to
/// the same question, and a decoder given both reads neither.
fn starts_with_sequence_header<R: Read + Seek>(
    reader: &Reader,
    source: &mut R,
    track_id: u32,
) -> bool {
    let Some(track) = reader.tracks.get(&track_id) else {
        return false;
    };

    let Ok(offset) = track.sample_offset(1) else {
        return false;
    };

    if source.seek(SeekFrom::Start(offset)).is_err() {
        return false;
    }

    let mut header = [0u8; 1];

    if source.read_exact(&mut header).is_err() {
        return false;
    }

    // obu_type sits in the four bits under the forbidden bit.
    (header[0] >> 3) & 0x0F == OBU_SEQUENCE_HEADER
}

const OBU_SEQUENCE_HEADER: u8 = 1;

impl file::SampleReader for File {
    fn codec(&self) -> mstu_media::Codec {
        let codec = self.reader.tracks[&self.video_track_id]
            .media_format()
            .unwrap();

        match codec {
            mstu_media::Codec::AV1(params) if self.inband_sequence_header => {
                mstu_media::Codec::AV1(mstu_media::codec::video::av1::Parameters {
                    config_obus: bytes::Bytes::new(),
                    ..params
                })
            }
            other => other,
        }
    }

    fn peek_sample(&mut self) -> std::result::Result<Option<SampleInfo>, String> {
        // Odd prefix lengths grow on the way to Annex-B, so those samples are
        // converted into a buffer of the reader's own first.
        if !self.reader.reads_in_place(self.video_track_id) {
            return self.peek_converted();
        }

        self.reader
            .sample_info(self.video_track_id, self.current_sample_id)
            .map_err(|err| err.to_string())
    }

    fn read_sample(&mut self, into: &mut [u8]) -> std::result::Result<(), String> {
        if let Some(converted) = self.converted.take() {
            into.copy_from_slice(&converted);
            self.current_sample_id += 1;

            return Ok(());
        }

        self.reader
            .read_sample_into(
                &mut self.buf_reader,
                self.video_track_id,
                self.current_sample_id,
                into,
            )
            .map_err(|err| err.to_string())?;

        self.current_sample_id += 1;

        Ok(())
    }

    fn timescale(&self) -> u32 {
        self.reader.tracks[&self.video_track_id].timescale()
    }

    fn duration(&self) -> Option<u64> {
        Some(self.reader.tracks[&self.video_track_id].media_duration())
    }

    fn rewind(&mut self) -> std::result::Result<(), String> {
        self.converted = None;
        self.current_sample_id = 1;
        Ok(())
    }

    fn seek(&mut self, time: u64) -> bool {
        let track = &self.reader.tracks[&self.video_track_id];

        self.current_sample_id = track
            .sync_sample_at_or_before(track.sample_at_time(time))
            .max(1);

        true
    }
}

#[cfg(test)]
mod tests {
    use crate::file::SampleReader;

    use super::*;

    /// The file the fixture describes, read back as the engine reads it.
    fn reader(video: &fixture::Video) -> (Reader, u32) {
        let bytes = fixture::file(video);
        let reader = fixture::read(&bytes).expect("a readable file");
        let track = reader
            .tracks()
            .iter()
            .find(|(_, track)| matches!(track.track_type(), Ok(TrackType::Video)))
            .map(|(id, _)| *id)
            .expect("the video track");

        (reader, track)
    }

    #[test]
    fn a_file_is_read_down_to_its_video_track() {
        let video = fixture::Video::default();
        let (reader, track_id) = reader(&video);

        assert_eq!(reader.tracks().len(), 1);

        let track = &reader.tracks()[&track_id];

        assert_eq!(track.track_id(), 1);
        assert_eq!(track.timescale(), video.timescale);
        assert_eq!(track.sample_count(), video.samples());
    }

    /// Every sample is somewhere, and where it is follows from the chunk it
    /// sits in and the samples before it in that chunk.
    #[test]
    fn each_sample_is_found_where_the_tables_put_it() {
        let video = fixture::Video::default();
        let (reader, track_id) = reader(&video);
        let track = &reader.tracks()[&track_id];

        let mut expected = (fixture::boxed(b"ftyp", b"isom\0\0\x02\0isomiso2avc1mp41").len() + 8) as u64;

        for (index, size) in video.sizes.iter().enumerate() {
            let sample_id = index as u32 + 1;
            let info = track.sample_info(sample_id).unwrap().expect("a sample");

            assert_eq!(info.size, *size as usize, "sample {sample_id} size");
            assert_eq!(
                track.sample_offset(sample_id).unwrap(),
                expected,
                "sample {sample_id} offset"
            );

            expected += *size as u64;
        }
    }

    #[test]
    fn a_sample_that_is_not_there_is_nothing_rather_than_an_error() {
        let video = fixture::Video::default();
        let (reader, track_id) = reader(&video);

        let past_the_end = video.samples() + 1;

        assert!(
            reader
                .sample_info(track_id, past_the_end)
                .expect("an answer")
                .is_none()
        );
    }

    #[test]
    fn a_sample_lasts_as_long_as_the_table_says_and_starts_after_the_last() {
        let video = fixture::Video {
            delta: 512,
            ..Default::default()
        };

        let (reader, track_id) = reader(&video);
        let track = &reader.tracks()[&track_id];

        for index in 0..video.samples() {
            let info = track.sample_info(index + 1).unwrap().expect("a sample");

            assert_eq!(info.duration, 512);
            assert_eq!(info.start_time, index as u64 * 512);
        }
    }

    /// With no stss every sample can be decoded from cold; with one, only the
    /// samples it names.
    #[test]
    fn which_samples_can_be_decoded_from_cold_comes_from_the_table() {
        let all = fixture::Video::default();
        let (every, track_id) = reader(&all);
        let track = &every.tracks()[&track_id];

        for index in 0..all.samples() {
            assert!(track.sample_info(index + 1).unwrap().unwrap().is_sync);
        }

        let some = fixture::Video {
            sync: vec![1, 3],
            ..Default::default()
        };

        let (named, track_id) = reader(&some);
        let track = &named.tracks()[&track_id];

        let synced: Vec<bool> = (1..=some.samples())
            .map(|id| track.sample_info(id).unwrap().unwrap().is_sync)
            .collect();

        assert_eq!(synced, vec![true, false, true, false]);
    }

    /// Chunks of three with four samples leaves one on its own in the second,
    /// which is where an off-by-one in the chunk arithmetic shows up.
    #[test]
    fn samples_are_found_across_chunks_that_do_not_divide_evenly() {
        let video = fixture::Video {
            sizes: vec![11, 22, 33, 44],
            per_chunk: 3,
            ..Default::default()
        };

        let (reader, track_id) = reader(&video);
        let track = &reader.tracks()[&track_id];

        let first = track.sample_offset(1).unwrap();

        assert_eq!(track.sample_offset(2).unwrap(), first + 11);
        assert_eq!(track.sample_offset(3).unwrap(), first + 33);
        assert_eq!(track.sample_offset(4).unwrap(), first + 66);
    }

    /// Every sample the same size is written once rather than listed.
    #[test]
    fn a_table_of_one_size_stands_for_all_of_them() {
        let video = fixture::Video {
            sizes: vec![16, 16, 16],
            ..Default::default()
        };

        let bytes = {
            // The same file, with the sizes written as one.
            let mut bytes = fixture::file(&video);
            let at = find(&bytes, b"stsz").expect("the size table");

            // version and flags, then sample_size, then sample_count.
            bytes[at + 8..at + 12].copy_from_slice(&16u32.to_be_bytes());

            bytes
        };

        let reader = fixture::read(&bytes).expect("a readable file");
        let track = reader.tracks().values().next().expect("the track");

        for id in 1..=3 {
            assert_eq!(track.sample_info(id).unwrap().unwrap().size, 16);
        }
    }

    fn find(haystack: &[u8], needle: &[u8; 4]) -> Option<usize> {
        haystack.windows(4).position(|window| window == needle)
    }

    // ---------- reading the samples themselves ----------

    #[test]
    fn a_sample_is_read_back_as_the_bytes_it_was_written_as() {
        let video = fixture::Video {
            sizes: vec![8, 8],
            ..Default::default()
        };

        let bytes = fixture::file(&video);
        let mut file = open_bytes(&bytes);

        // Each sample is filled with its own number by the fixture, and the
        // reader rewrites the first four bytes into a start code.
        let first = file.peek_sample().unwrap().expect("a sample");
        let mut into = vec![0u8; first.size];

        file.read_sample(&mut into).expect("the bytes");

        assert_eq!(into[..4], [0, 0, 0, 1], "rewritten to Annex-B");
        assert_eq!(into[4..], [1, 1, 1, 1], "the first sample's own bytes");
    }

    #[test]
    fn samples_come_out_one_after_another_until_there_are_none() {
        let video = fixture::Video {
            sizes: vec![8, 8, 8],
            ..Default::default()
        };

        let bytes = fixture::file(&video);
        let mut file = open_bytes(&bytes);

        for expected in 1..=3u8 {
            let info = file.peek_sample().unwrap().expect("a sample");
            let mut into = vec![0u8; info.size];

            file.read_sample(&mut into).expect("the bytes");

            assert_eq!(into[4], expected, "sample {expected}");
        }

        assert!(file.peek_sample().unwrap().is_none(), "and then no more");
    }

    #[test]
    fn rewinding_starts_the_file_again() {
        let video = fixture::Video::default();
        let bytes = fixture::file(&video);
        let mut file = open_bytes(&bytes);

        let first = file.peek_sample().unwrap().expect("a sample");
        let mut into = vec![0u8; first.size];
        file.read_sample(&mut into).expect("the bytes");

        file.rewind().expect("to start again");

        let again = file.peek_sample().unwrap().expect("a sample");

        assert_eq!(again.start_time, 0);
    }

    /// A prefix shorter than a start code makes the sample grow, so it is
    /// built in a buffer of the reader's own rather than read into place.
    #[test]
    fn a_sample_whose_prefix_is_too_short_is_grown_on_the_way_out() {
        let video = fixture::Video {
            sizes: vec![10, 10],
            entry: fixture::avc1_with_short_lengths(),
            length_size: 2,
            ..Default::default()
        };

        let bytes = fixture::file(&video);
        let reader = fixture::read(&bytes).expect("a readable file");
        let track_id = *reader.tracks().keys().next().expect("the track");

        assert!(
            !reader.reads_in_place(track_id),
            "two byte lengths cannot become four in place"
        );

        let mut file = open_bytes(&bytes);
        let info = file.peek_sample().unwrap().expect("a sample");

        // Eight bytes of unit behind two of length, which become four.
        assert_eq!(info.size, 12);

        let mut into = vec![0u8; info.size];
        file.read_sample(&mut into).expect("the bytes");

        assert_eq!(into[..4], [0, 0, 0, 1]);
        assert_eq!(into[4..], [1u8; 8]);
    }

    #[test]
    fn a_sample_whose_prefix_already_fits_is_read_straight_into_place() {
        let video = fixture::Video::default();
        let bytes = fixture::file(&video);
        let reader = fixture::read(&bytes).expect("a readable file");
        let track_id = *reader.tracks().keys().next().expect("the track");

        assert!(reader.reads_in_place(track_id));
    }

    /// Opening the bytes the way `open` opens a path.
    fn open_bytes(bytes: &[u8]) -> File {
        let dir = std::env::temp_dir().join(format!("mstu-mp4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("somewhere to put it");

        let path = dir.join(format!("{}.mp4", bytes.len()));
        std::fs::write(&path, bytes).expect("a file to open");

        let file = open(path.clone()).expect("a readable file");

        let _ = std::fs::remove_file(&path);

        file
    }
}

#[cfg(test)]
mod codecs {
    use mstu_media::Codec;

    use super::*;

    fn codec_of(entry: Vec<u8>) -> Codec {
        let video = fixture::Video {
            entry,
            ..Default::default()
        };

        let bytes = fixture::file(&video);
        let reader = fixture::read(&bytes).expect("a readable file");
        let track = reader.tracks().values().next().expect("the track");

        track.media_format().expect("a codec")
    }

    /// avc3 and hvc1 differ from avc1 and hev1 only in where the parameter
    /// sets live, which is why they share a reader.
    #[test]
    fn every_entry_the_reader_claims_is_read_as_its_codec() {
        assert!(matches!(codec_of(fixture::avc1()), Codec::H264(_)));
        assert!(matches!(codec_of(fixture::avc3()), Codec::H264(_)));
        assert!(matches!(codec_of(fixture::hev1()), Codec::H265(_)));
        assert!(matches!(codec_of(fixture::hvc1()), Codec::H265(_)));
        assert!(matches!(codec_of(fixture::av01()), Codec::AV1(_)));
        assert!(matches!(codec_of(fixture::vp09()), Codec::VP9(_)));
    }

    #[test]
    fn an_h264_track_hands_over_the_parameter_sets_the_file_holds() {
        let Codec::H264(params) = codec_of(fixture::avc1()) else {
            panic!("h264");
        };

        assert_eq!(params.sps.len(), 1);
        assert_eq!(params.pps.len(), 1);
        assert_eq!(params.sps[0].as_bytes(), &[0x67, 0x64, 0x00, 0x1f]);
        assert_eq!(params.pps[0].as_bytes(), &[0x68, 0xee]);
    }

    #[test]
    fn an_av1_track_hands_over_the_sequence_header_as_written() {
        let Codec::AV1(params) = codec_of(fixture::av01()) else {
            panic!("av1");
        };

        assert_eq!(
            params.config_obus.as_ref(),
            &[0x0a, 0x0b, 0x00, 0x00, 0x00, 0x04, 0x3c, 0xff, 0xbc, 0x02, 0xf8, 0x00, 0x40]
        );
    }

    /// Only h26x samples are length-prefixed, so only they are rewritten.
    #[test]
    fn only_the_codecs_that_need_rewriting_declare_a_length_size() {
        let with = fixture::Video {
            entry: fixture::hvc1(),
            ..Default::default()
        };

        let bytes = fixture::file(&with);
        let reader = fixture::read(&bytes).expect("a readable file");
        let track_id = *reader.tracks().keys().next().expect("the track");

        assert!(reader.reads_in_place(track_id));

        let without = fixture::Video {
            entry: fixture::av01(),
            ..Default::default()
        };

        let bytes = fixture::file(&without);
        let reader = fixture::read(&bytes).expect("a readable file");
        let track = reader.tracks().values().next().expect("the track");

        assert_eq!(track.nalu_length_size(), None, "av1 is not rewritten");
    }
}

#[cfg(test)]
mod refusals {
    use super::*;

    fn at(bytes: &[u8], name: &[u8; 4]) -> usize {
        bytes
            .windows(4)
            .position(|window| window == name)
            .unwrap_or_else(|| panic!("no {} in the file", std::str::from_utf8(name).unwrap()))
    }

    #[test]
    fn a_file_with_no_video_track_is_refused() {
        let video = fixture::Video::default();
        let mut bytes = fixture::file(&video);

        // Call the track something the reader plays nothing of.
        let handler = at(&bytes, b"vide");
        bytes[handler..handler + 4].copy_from_slice(b"soun");

        let reader = fixture::read(&bytes).expect("the file still parses");

        assert!(
            !reader
                .tracks()
                .values()
                .any(|track| matches!(track.track_type(), Ok(TrackType::Video))),
            "nothing here is video"
        );
    }

    #[test]
    fn a_box_claiming_more_than_its_parent_holds_is_refused() {
        let video = fixture::Video::default();
        let mut bytes = fixture::file(&video);

        let stts = at(&bytes, b"stts") - 4;
        bytes[stts..stts + 4].copy_from_slice(&0xFFFF_0000u32.to_be_bytes());

        assert!(fixture::read(&bytes).is_err());
    }

    #[test]
    fn a_file_cut_short_is_refused() {
        let video = fixture::Video::default();
        let bytes = fixture::file(&video);

        let half = &bytes[..bytes.len() / 2];

        assert!(fixture::read(half).is_err());
    }

    #[test]
    fn a_track_without_its_sample_table_is_refused() {
        let video = fixture::Video::default();
        let mut bytes = fixture::file(&video);

        // Rename the table so the reader looks for one and finds none.
        let stsd = at(&bytes, b"stsd");
        bytes[stsd..stsd + 4].copy_from_slice(b"zzzz");

        assert!(fixture::read(&bytes).is_err());
    }

    #[test]
    fn a_table_promising_more_entries_than_it_holds_is_refused() {
        let video = fixture::Video::default();
        let mut bytes = fixture::file(&video);

        // stts: version and flags, then the count.
        let count = at(&bytes, b"stts") + 8;
        bytes[count..count + 4].copy_from_slice(&10_000u32.to_be_bytes());

        assert!(fixture::read(&bytes).is_err());
    }

    #[test]
    fn something_that_is_not_a_file_of_this_kind_is_refused() {
        assert!(fixture::read(b"@echo off\r\nexit\r\n").is_err());
        assert!(fixture::read(&[]).is_err());
    }
}

/// The paths a file takes when it is long, large, reordered, or simply has
/// boxes in it that nothing here reads.
#[cfg(test)]
mod variants {
    use super::*;

    fn track_of(video: &fixture::Video) -> (Reader, u32) {
        let bytes = fixture::file(video);
        let reader = fixture::read(&bytes).expect("a readable file");
        let id = *reader.tracks().keys().next().expect("the track");

        (reader, id)
    }

    /// Over four gigabytes the offsets no longer fit in 32 bits, and the file
    /// writes a different box for them.
    #[test]
    fn a_file_too_large_for_32_bit_offsets_is_read_the_same_way() {
        let video = fixture::Video {
            large_offsets: true,
            ..Default::default()
        };

        let (reader, id) = track_of(&video);
        let track = &reader.tracks()[&id];

        let plain = fixture::Video::default();
        let (ordinary, other) = track_of(&plain);

        for sample in 1..=video.samples() {
            assert_eq!(
                track.sample_offset(sample).unwrap(),
                ordinary.tracks()[&other].sample_offset(sample).unwrap(),
                "sample {sample} sits in the same place either way"
            );
        }
    }

    /// A long recording writes its times in 64 bits.
    #[test]
    fn a_header_in_its_long_form_is_read_the_same_way() {
        let video = fixture::Video {
            long_headers: true,
            timescale: 90_000,
            delta: 3_000,
            ..Default::default()
        };

        let (reader, id) = track_of(&video);
        let track = &reader.tracks()[&id];

        assert_eq!(track.timescale(), 90_000);
        assert_eq!(track.media_duration(), (video.samples() * video.delta) as u64);
        assert_eq!(track.sample_count(), video.samples());
    }

    /// Reordered frames are shown later than they are stored, and by how much
    /// is what the composition table says.
    #[test]
    fn a_reordered_stream_says_how_far_each_sample_moves() {
        let video = fixture::Video {
            sizes: vec![10, 10, 10, 10],
            delta: 100,
            composition: vec![0, 300, 100, 200],
            ..Default::default()
        };

        let (reader, id) = track_of(&video);
        let track = &reader.tracks()[&id];

        let offsets: Vec<i32> = (1..=video.samples())
            .map(|sample| track.sample_info(sample).unwrap().unwrap().rendering_offset)
            .collect();

        assert_eq!(offsets, video.composition);
    }

    #[test]
    fn a_stream_in_order_moves_nothing() {
        let video = fixture::Video::default();
        let (reader, id) = track_of(&video);
        let track = &reader.tracks()[&id];

        for sample in 1..=video.samples() {
            assert_eq!(
                track.sample_info(sample).unwrap().unwrap().rendering_offset,
                0
            );
        }
    }

    /// Most boxes in a real file are of no interest here. What matters is
    /// that each one is stepped over exactly, so everything after it is still
    /// found: one box read a byte short derails the whole file.
    #[test]
    fn boxes_nothing_here_reads_do_not_derail_the_ones_that_matter() {
        let plain = fixture::Video::default();
        let cluttered = fixture::Video {
            clutter: true,
            ..Default::default()
        };

        let (bare, bare_id) = track_of(&plain);
        let (full, full_id) = track_of(&cluttered);

        let bare = &bare.tracks()[&bare_id];
        let full = &full.tracks()[&full_id];

        assert_eq!(full.sample_count(), bare.sample_count());
        assert_eq!(full.timescale(), bare.timescale());

        for sample in 1..=plain.samples() {
            let one = bare.sample_info(sample).unwrap().unwrap();
            let other = full.sample_info(sample).unwrap().unwrap();

            assert_eq!(other.size, one.size, "sample {sample} size");
            assert_eq!(other.start_time, one.start_time, "sample {sample} time");

            // The offsets differ by however much the extra boxes took up,
            // but the samples still follow one another exactly.
            if sample > 1 {
                assert_eq!(
                    full.sample_offset(sample).unwrap() - full.sample_offset(sample - 1).unwrap(),
                    bare.sample_offset(sample).unwrap() - bare.sample_offset(sample - 1).unwrap(),
                );
            }
        }
    }
}

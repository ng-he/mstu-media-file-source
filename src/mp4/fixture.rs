//! Small mp4 files, built byte by byte, for testing the reader on.
//!
//! Real files are megabytes of encoded video and say nothing about which
//! sample sits where. These are a few hundred bytes with a sample table
//! written on purpose, so what the reader works out can be checked against
//! what was asked for.

use std::io::Cursor;

use crate::mp4::{Reader, Result};

/// A box: its length, its name, and what it holds.
pub fn boxed(name: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut bytes = ((payload.len() + 8) as u32).to_be_bytes().to_vec();

    bytes.extend_from_slice(name);
    bytes.extend_from_slice(payload);

    bytes
}

/// A box that opens with a version and three bytes of flags.
pub fn full_boxed(name: &[u8; 4], version: u8, payload: &[u8]) -> Vec<u8> {
    let mut body = vec![version, 0, 0, 0];

    body.extend_from_slice(payload);

    boxed(name, &body)
}

fn u32s(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|value| value.to_be_bytes()).collect()
}

/// A table of entries, counted as the sample tables count them.
pub fn table(name: &[u8; 4], values: &[u32]) -> Vec<u8> {
    let mut payload = (values.len() as u32 / columns(name)).to_be_bytes().to_vec();

    payload.extend(u32s(values));

    full_boxed(name, 0, &payload)
}

fn columns(name: &[u8; 4]) -> u32 {
    match name {
        b"stsc" => 3,
        b"stts" | b"ctts" => 2,
        _ => 1,
    }
}

/// What the file should say about its one video track.
pub struct Video {
    pub timescale: u32,

    /// One size per sample, in the order they are stored.
    pub sizes: Vec<u32>,

    /// How long each sample lasts, in timescale units.
    pub delta: u32,

    /// How many samples share a chunk.
    pub per_chunk: u32,

    /// The samples that can be decoded from cold. Empty means every one.
    pub sync: Vec<u32>,

    /// The sample entry, which names the codec.
    pub entry: Vec<u8>,

    /// How many bytes each unit's length is written in. Four fits a start
    /// code exactly; anything shorter has to grow on the way out.
    pub length_size: u32,

    /// How far each sample is shown from where it is stored, which a stream
    /// with reordered frames carries.
    pub composition: Vec<i32>,

    /// Chunk offsets written as 64 bits, as a file over four gigabytes needs.
    pub large_offsets: bool,

    /// Headers written in their long form, as a long recording needs.
    pub long_headers: bool,

    /// Boxes the reader stores and nobody reads, which still have to be
    /// stepped over exactly.
    pub clutter: bool,
}

impl Default for Video {
    fn default() -> Self {
        Self {
            timescale: 1000,
            sizes: vec![10, 20, 30, 40],
            delta: 100,
            per_chunk: 2,
            sync: Vec::new(),
            entry: avc1(),
            length_size: 4,
            composition: Vec::new(),
            large_offsets: false,
            long_headers: false,
            clutter: false,
        }
    }
}

impl Video {
    pub fn samples(&self) -> u32 {
        self.sizes.len() as u32
    }

    pub fn chunks(&self) -> u32 {
        self.samples().div_ceil(self.per_chunk)
    }

    /// Where the samples are, once the header in front of them is counted.
    fn chunk_offsets(&self, media_start: u64) -> Vec<u32> {
        let mut offsets = Vec::new();
        let mut at = media_start;

        for chunk in 0..self.chunks() {
            offsets.push(at as u32);

            let first = (chunk * self.per_chunk) as usize;
            let last = ((chunk + 1) * self.per_chunk).min(self.samples()) as usize;

            at += self.sizes[first..last].iter().map(|size| *size as u64).sum::<u64>();
        }

        offsets
    }

    /// The bytes of every sample: one unit behind its length, as a file in
    /// this format stores them, filled with the sample's own number so a
    /// sample read back can be told from its neighbours.
    pub fn media(&self) -> Vec<u8> {
        let mut media = Vec::new();

        for (index, size) in self.sizes.iter().enumerate() {
            let unit = size.saturating_sub(self.length_size);
            let length = unit.to_be_bytes();

            media.extend_from_slice(&length[4 - self.length_size as usize..]);
            media.extend(std::iter::repeat_n(index as u8 + 1, unit as usize));
        }

        media
    }
}

/// An avc1 entry with a 4-byte length prefix, as most files use.
pub fn avc1() -> Vec<u8> {
    sample_entry(b"avc1", &avcc(4))
}

/// An avc1 entry whose lengths are shorter, so Annex-B does not fit in place.
pub fn avc1_with_short_lengths() -> Vec<u8> {
    sample_entry(b"avc1", &avcc(2))
}

fn avcc(length_size: u8) -> Vec<u8> {
    let payload = vec![
        1,                      // configuration version
        0x64,                   // profile
        0x00,                   // compatibility
        0x1f,                   // level
        0xFC | (length_size - 1), // reserved bits, then the length size
        0xE0 | 1,               // reserved bits, then one sps
        0x00,
        0x04, // sps length
        0x67,
        0x64,
        0x00,
        0x1f, // sps
        1,    // one pps
        0x00,
        0x02, // pps length
        0x68,
        0xee, // pps
    ];

    boxed(b"avcC", &payload)
}

/// The same picture, as the other codecs name themselves.
pub fn avc3() -> Vec<u8> {
    sample_entry(b"avc3", &avcc(4))
}

pub fn hev1() -> Vec<u8> {
    sample_entry(b"hev1", &hvcc())
}

pub fn hvc1() -> Vec<u8> {
    sample_entry(b"hvc1", &hvcc())
}

pub fn av01() -> Vec<u8> {
    let config = vec![
        0x81, 0x00, 0x0c, 0x00, // marker, version, profile, level, flags
        0x0a, 0x0b, 0x00, 0x00, 0x00, 0x04, 0x3c, 0xff, 0xbc, 0x02, 0xf8, 0x00, 0x40,
    ];

    sample_entry(b"av01", &boxed(b"av1C", &config))
}

pub fn vp09() -> Vec<u8> {
    let config = full_boxed(b"vpcC", 1, &[0, 10, 8, 0, 1, 1, 0, 0]);
    let mut payload = vec![0u8; 4]; // version and flags, which vp09 carries

    payload.extend_from_slice(&0u16.to_be_bytes()); // start code
    payload.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    payload.extend_from_slice(&[0u8; 16]); // pre-defined and reserved
    payload.extend_from_slice(&320u16.to_be_bytes());
    payload.extend_from_slice(&240u16.to_be_bytes());
    payload.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    payload.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    payload.extend_from_slice(&[0u8; 4]);
    payload.extend_from_slice(&1u16.to_be_bytes());
    payload.extend_from_slice(&[0u8; 32]);
    payload.extend_from_slice(&0x0018u16.to_be_bytes());
    payload.extend_from_slice(&0xFFFFu16.to_be_bytes());
    payload.extend_from_slice(&config);

    boxed(b"vp09", &payload)
}

/// An hvcC with one array of one parameter set, which is enough to name it.
fn hvcc() -> Vec<u8> {
    let mut payload = vec![
        1, // configuration version
        0x01, 0x60, 0x00, 0x00, 0x00, 0x90, 0x00, 0x00, 0x00, 0x00, 0x00, 0x5d, // profile
        0xf0, 0x00, // spatial segmentation
        0xfc, // parallelism
        0xfd, // chroma
        0xf8, // bit depth luma
        0xf8, // bit depth chroma
        0x00, 0x00, // frame rate
        0x0f, // flags, then the length size minus one in the low bits
        1,    // one array
        0x21, // sps, not completeness-flagged
        0x00, 0x01, // one unit
        0x00, 0x03, // its length
        0x42, 0x01, 0x01, // the unit
    ];

    payload[21] = 0x0f | 0x03; // four byte lengths

    boxed(b"hvcC", &payload)
}

/// A visual sample entry: the fixed 78 bytes, then the codec's own box.
fn sample_entry(name: &[u8; 4], config: &[u8]) -> Vec<u8> {
    let mut payload = vec![0u8; 6]; // reserved

    payload.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    payload.extend_from_slice(&[0u8; 16]); // pre-defined and reserved
    payload.extend_from_slice(&320u16.to_be_bytes()); // width
    payload.extend_from_slice(&240u16.to_be_bytes()); // height
    payload.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // horizontal dpi
    payload.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // vertical dpi
    payload.extend_from_slice(&[0u8; 4]); // reserved
    payload.extend_from_slice(&1u16.to_be_bytes()); // frame count
    payload.extend_from_slice(&[0u8; 32]); // compressor name
    payload.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
    payload.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pre-defined
    payload.extend_from_slice(config);

    boxed(name, &payload)
}

fn stsd(entry: &[u8]) -> Vec<u8> {
    let mut payload = 1u32.to_be_bytes().to_vec(); // one entry

    payload.extend_from_slice(entry);

    full_boxed(b"stsd", 0, &payload)
}

fn stbl(video: &Video, media_start: u64) -> Vec<u8> {
    let mut payload = stsd(&video.entry);

    payload.extend(table(b"stts", &[video.samples(), video.delta]));

    // One run of chunks, all the same size, from the first chunk on.
    payload.extend(table(b"stsc", &[1, video.per_chunk, 1]));

    let mut sizes = vec![0u32, video.samples()]; // sizes differ, so one each
    sizes.extend_from_slice(&video.sizes);
    payload.extend(full_boxed(b"stsz", 0, &u32s(&sizes)));

    if video.large_offsets {
        let mut offsets = video.chunks().to_be_bytes().to_vec();

        for offset in video.chunk_offsets(media_start) {
            offsets.extend_from_slice(&(offset as u64).to_be_bytes());
        }

        payload.extend(full_boxed(b"co64", 0, &offsets));
    } else {
        payload.extend(table(b"stco", &video.chunk_offsets(media_start)));
    }

    if !video.sync.is_empty() {
        payload.extend(table(b"stss", &video.sync));
    }

    if !video.composition.is_empty() {
        // One entry per sample, so each says its own offset.
        let mut entries = (video.composition.len() as u32).to_be_bytes().to_vec();

        for offset in &video.composition {
            entries.extend_from_slice(&1u32.to_be_bytes());
            entries.extend_from_slice(&offset.to_be_bytes());
        }

        payload.extend(full_boxed(b"ctts", 0, &entries));
    }

    boxed(b"stbl", &payload)
}

fn minf(video: &Video, media_start: u64) -> Vec<u8> {
    let mut payload = boxed(b"vmhd", &[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);

    let url = full_boxed(b"url ", 0, &[]);
    let mut dref = 1u32.to_be_bytes().to_vec();
    dref.extend(url);

    payload.extend(boxed(b"dinf", &full_boxed(b"dref", 0, &dref)));
    payload.extend(stbl(video, media_start));

    boxed(b"minf", &payload)
}

fn mdia(video: &Video, media_start: u64) -> Vec<u8> {
    let mut mdhd = Vec::new();
    let duration = (video.samples() * video.delta) as u64;

    if video.long_headers {
        mdhd.extend_from_slice(&0u64.to_be_bytes()); // created
        mdhd.extend_from_slice(&0u64.to_be_bytes()); // modified
        mdhd.extend_from_slice(&video.timescale.to_be_bytes());
        mdhd.extend_from_slice(&duration.to_be_bytes());
    } else {
    mdhd.extend_from_slice(&0u32.to_be_bytes()); // created
    mdhd.extend_from_slice(&0u32.to_be_bytes()); // modified
    mdhd.extend_from_slice(&video.timescale.to_be_bytes());
    mdhd.extend_from_slice(&(video.samples() * video.delta).to_be_bytes());
    }
    mdhd.extend_from_slice(&0x55c4u16.to_be_bytes()); // language
    mdhd.extend_from_slice(&0u16.to_be_bytes()); // pre-defined

    let mut hdlr = vec![0u8; 4]; // pre-defined
    hdlr.extend_from_slice(b"vide");
    hdlr.extend_from_slice(&[0u8; 12]); // reserved
    hdlr.extend_from_slice(b"video\0");

    let mut payload = full_boxed(b"mdhd", u8::from(video.long_headers), &mdhd);

    payload.extend(full_boxed(b"hdlr", 0, &hdlr));
    payload.extend(minf(video, media_start));

    boxed(b"mdia", &payload)
}

fn tkhd(video: &Video) -> Vec<u8> {
    let mut payload = Vec::new();

    payload.extend_from_slice(&0u32.to_be_bytes()); // created
    payload.extend_from_slice(&0u32.to_be_bytes()); // modified
    payload.extend_from_slice(&1u32.to_be_bytes()); // track id
    payload.extend_from_slice(&0u32.to_be_bytes()); // reserved
    payload.extend_from_slice(&(video.samples() * video.delta).to_be_bytes());
    payload.extend_from_slice(&[0u8; 8]); // reserved
    payload.extend_from_slice(&0u16.to_be_bytes()); // layer
    payload.extend_from_slice(&0u16.to_be_bytes()); // alternate group
    payload.extend_from_slice(&0u16.to_be_bytes()); // volume
    payload.extend_from_slice(&0u16.to_be_bytes()); // reserved

    // The unity matrix, which every file writes.
    for value in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        payload.extend_from_slice(&value.to_be_bytes());
    }

    payload.extend_from_slice(&(320u32 << 16).to_be_bytes()); // width
    payload.extend_from_slice(&(240u32 << 16).to_be_bytes()); // height

    full_boxed(b"tkhd", 0, &payload)
}

fn mvhd(video: &Video) -> Vec<u8> {
    let mut payload = Vec::new();

    payload.extend_from_slice(&0u32.to_be_bytes()); // created
    payload.extend_from_slice(&0u32.to_be_bytes()); // modified
    payload.extend_from_slice(&video.timescale.to_be_bytes());
    payload.extend_from_slice(&(video.samples() * video.delta).to_be_bytes());
    payload.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate
    payload.extend_from_slice(&0x0100u16.to_be_bytes()); // volume
    payload.extend_from_slice(&[0u8; 10]); // reserved

    for value in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        payload.extend_from_slice(&value.to_be_bytes());
    }

    payload.extend_from_slice(&[0u8; 24]); // pre-defined
    payload.extend_from_slice(&2u32.to_be_bytes()); // next track id

    full_boxed(b"mvhd", 0, &payload)
}

/// A whole file: a brand, the media, and a header describing it.
///
/// The media comes first so that its offsets are known while the header that
/// points at it is being written.
pub fn file(video: &Video) -> Vec<u8> {
    let ftyp = boxed(b"ftyp", b"isom\0\0\x02\0isomiso2avc1mp41");
    let media = video.media();
    let media_start = (ftyp.len() + 8) as u64;

    let mut moov = mvhd(video);
    let mut trak = tkhd(video);

    if video.clutter {
        // An edit list, a user data box and free space, as a real file has.
        let elst = full_boxed(b"elst", 0, &{
            let mut entry = 1u32.to_be_bytes().to_vec();
            entry.extend_from_slice(&0u32.to_be_bytes()); // duration
            entry.extend_from_slice(&0i32.to_be_bytes()); // media time
            entry.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate
            entry
        });

        trak.extend(boxed(b"edts", &elst));
        trak.extend(boxed(b"udta", &boxed(b"name", b"a track")));
        trak.extend(boxed(b"free", &[0u8; 8]));
    }

    trak.extend(mdia(video, media_start));
    moov.extend(boxed(b"trak", &trak));

    let mut bytes = ftyp;

    bytes.extend(boxed(b"mdat", &media));
    bytes.extend(boxed(b"moov", &moov));

    bytes
}

/// The file read back, as the engine reads it.
pub fn read(bytes: &[u8]) -> Result<Reader> {
    Reader::read(&mut Cursor::new(bytes), bytes.len() as u64)
}

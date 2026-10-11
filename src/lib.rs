use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    ptr,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
    vec,
};

use mstu_media::{
    self as media,
    Codec::{AV1, H264, H265, VP9},
};
use mstu_sdk::{
    CommandDescriptor, EventDescriptor, HostContext, Logger, Message, Metadata, PluginDescriptor,
    PluginHandle, ProcessContext, Schema, Slice, Str, Value, Writer, log_debug, log_error,
    log_info, log_trace, log_warn, message, slice,
};

pub mod mp4;

pub mod file;
pub mod schemas;
pub mod types;

pub use file::Sample;

use crate::file::SampleReader;

static METADATA: Metadata = Metadata {
    name: Str::from_static("Media file source"),
    version: Str::from_static(env!("CARGO_PKG_VERSION")),
};

pub struct MediaFileSourcePlugin {
    id: String,
    ctx: *const HostContext,
    log: Logger,
    filename: String,
    sample_reader: Option<Box<dyn SampleReader>>,

    /// Samples read since the file was opened, and whether the end was
    /// already reported. Atomic: the host polls them while `process` runs.
    samples: AtomicU64,
    ended: AtomicBool,

    /// Where playback has reached and how long the file is, in microseconds.
    position: AtomicU64,
    duration: AtomicU64,

    /// Written in front of every sample. AV1 keeps its temporal delimiter
    /// outside the file, and a stream without one cannot be cut into frames.
    lead_in: &'static [u8],

    /// Playback speed: 1.0 real time, 0 as fast as the pipeline will take it.
    rate: f64,
    looping: bool,

    /// Wall clock and media time that pacing is measured from.
    anchor: Option<Instant>,
    anchor_time: u64,
}

/// Pacing resolution is bounded by the engine's idle poll, so a sample this
/// far behind means the clock jumped: a pause, a seek, or a slow consumer.
const MAX_LAG: Duration = Duration::from_millis(500);

/// The first box of a file in the ISO base media format: mp4, mov, m4v, 3gp
/// and the rest of the family, whatever the name says.
///
/// Read rather than taken from the extension, since one of those is a fact
/// and the other is a hint.
fn is_iso_bmff(path: &Path) -> bool {
    const FIRST: [&[u8; 4]; 7] = [
        b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"pnot",
    ];

    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };

    let mut header = [0u8; 8];

    if file.read_exact(&mut header).is_err() {
        return false;
    }

    FIRST.iter().any(|name| header[4..8] == **name)
}

impl MediaFileSourcePlugin {
    fn new(ctx: *const HostContext, id: String) -> Self {
        let log = Logger::new(ctx, unsafe { METADATA.name.as_str() }, id.as_str());

        Self {
            ctx,
            log,
            id,
            filename: String::new(),
            sample_reader: None,
            lead_in: &[],
            samples: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            position: AtomicU64::new(0),
            duration: AtomicU64::new(0),
            rate: 1.0,
            looping: false,
            anchor: None,
            anchor_time: 0,
        }
    }

    /// Pushes what the UI shows; the host keeps the latest.
    fn publish_live(&self) {
        let values = [
            self.samples.load(Ordering::Relaxed).into(),
            self.ended.load(Ordering::Relaxed).into(),
            self.position.load(Ordering::Relaxed).into(),
            self.duration.load(Ordering::Relaxed).into(),
        ];

        unsafe {
            ((*self.ctx).publish_live)(
                Str::new(self.id.as_str()),
                Message {
                    values: Slice::from_raw_parts(values.as_ptr(), values.len()),
                },
            );
        }
    }

    /// Media time in timescale units to microseconds.
    fn micros(&self, time: u64) -> u64 {
        match self.sample_reader.as_ref().map(|reader| reader.timescale()) {
            Some(timescale) if timescale > 0 => time * 1_000_000 / timescale as u64,
            _ => 0,
        }
    }

    /// Restarts the clock from `time`, so pacing resumes rather than bursting
    /// to catch up.
    fn anchor_at(&mut self, time: u64) {
        self.anchor = Some(Instant::now());
        self.anchor_time = time;
    }

    /// When a sample is due, measured from the anchor.
    fn due(&self, time: u64) -> Option<Instant> {
        let anchor = self.anchor?;

        if self.rate <= 0.0 {
            return Some(anchor);
        }

        let timescale = self.sample_reader.as_ref()?.timescale().max(1) as f64;
        let ahead = time.saturating_sub(self.anchor_time) as f64 / timescale / self.rate;

        Some(anchor + Duration::from_secs_f64(ahead))
    }

    fn seek_to(&mut self, micros: u64) {
        let Some(reader) = self.sample_reader.as_mut() else {
            return;
        };

        let time = micros * reader.timescale() as u64 / 1_000_000;

        if !reader.seek(time) {
            log_warn!(self.log, "this source cannot seek");
            return;
        }

        self.ended.store(false, Ordering::Relaxed);
        self.position.store(micros, Ordering::Relaxed);
        self.anchor_at(time);
        self.publish_live();

        log_info!(self.log, "seeked to {micros} us");
    }

    fn open(&mut self, path: PathBuf) -> bool {
        self.filename = path.to_string_lossy().to_string();
        self.samples.store(0, Ordering::Relaxed);
        self.ended.store(false, Ordering::Relaxed);
        self.position.store(0, Ordering::Relaxed);
        self.anchor = None;
        self.lead_in = &[];

        if !is_iso_bmff(&path) {
            log_error!(self.log, "unsupported media file '{}'", self.filename);
            return false;
        }

        match mp4::open(path) {
            Ok(reader) => {
                self.sample_reader = Some(Box::new(reader));
            }
            Err(err) => {
                log_error!(self.log, "cannot open '{}': {err}", self.filename);
                return false;
            }
        }

        // A file whose sample table is empty parses and plays nothing, which
        // is worse than refusing it: the pipeline would sit there waiting on
        // a source that will never produce. Fragmented files look like this,
        // since their samples are described in pieces further in.
        let empty = match self
            .sample_reader
            .as_mut()
            .map(|reader| reader.peek_sample())
        {
            Some(Ok(None)) => true,
            Some(Err(err)) => {
                log_error!(self.log, "cannot read '{}': {err}", self.filename);
                self.sample_reader = None;
                return false;
            }
            _ => false,
        };

        if empty {
            log_error!(
                self.log,
                "'{}' holds no samples to play; a fragmented file is not read yet",
                self.filename
            );

            self.sample_reader = None;
            return false;
        }

        let mut codec = Str::empty();
        let mut extras: Vec<u8> = vec![];

        if let Some(sample_reader) = self.sample_reader.as_ref() {
            match sample_reader.codec() {
                H264(params) => {
                    codec = Str::from_static("h264");
                    for nalus in [&params.sps, &params.pps] {
                        for nalu in nalus {
                            extras.extend_from_slice(&[0, 0, 0, 1]);
                            extras.extend_from_slice(nalu.as_bytes());
                        }
                    }
                }
                H265(params) => {
                    codec = Str::from_static("h265");

                    for nalus in [&params.vps, &params.sps, &params.pps] {
                        for nalu in nalus {
                            extras.extend_from_slice(&[0, 0, 0, 1]);
                            extras.extend_from_slice(nalu.as_bytes());
                        }
                    }
                }
                // The samples are already obus; the sequence header in front
                // of them is what makes the stream startable.
                AV1(params) => {
                    codec = Str::from_static("av1");
                    extras.extend_from_slice(&params.config_obus);

                    // A temporal delimiter per sample, which the file leaves
                    // out because each sample is one temporal unit already.
                    self.lead_in = &[0x12, 0x00];
                }

                // Frames as the file stores them, which carry their own
                // headers. Anything downstream that wants them framed has to
                // frame them itself.
                VP9(_) => {
                    codec = Str::from_static("vp9");
                }

                _ => {
                    log_error!(self.log, "unsupported codec type");
                    return false;
                }
            }
        }

        let duration = self
            .sample_reader
            .as_ref()
            .and_then(|reader| reader.duration())
            .unwrap_or(0);

        self.duration
            .store(self.micros(duration), Ordering::Relaxed);

        log_info!(
            self.log,
            "opened '{}': codec {codec}, {} byte(s) of extras",
            self.filename,
            extras.len()
        );

        unsafe {
            // The host hands back nothing if it has no such event to fill,
            // and a file that opened is still open either way.
            let writer = ((*self.ctx).new_event)(Str::new(self.id.as_str()), 0) as *mut Writer;

            if writer.is_null() {
                log_warn!(self.log, "the host took no file.changed to fill");
            } else {
                let event_writer = &mut *writer;

                let values = [
                    Str::new(self.filename.as_str()).into(),
                    codec.into(),
                    Slice::from(extras.as_slice()).into(),
                ];

                (event_writer.fill)(
                    event_writer,
                    Message {
                        values: Slice::from_raw_parts(values.as_ptr(), values.len()),
                    },
                );

                ((*self.ctx).publish_events)(Str::new(self.id.as_str()), 0);

                log_info!(self.log, "published file.changed for '{}'", self.filename);
            }
        }

        self.publish_live();

        true
    }

    fn process(&mut self, output: &mut message::Writer) -> Result<(), String> {
        // A source with no file yet produces nothing, it is not an error: the
        // worker asks again every few milliseconds until one is set.
        let Some(reader) = self.sample_reader.as_mut() else {
            return Ok(());
        };

        // Described before it is read, so nothing is read before it is due.
        let Some(sample) = reader.peek_sample()? else {
            return self.finish();
        };

        // Hold the sample back until its place in the timeline comes round.
        if self.rate > 0.0 {
            if self.anchor.is_none() {
                self.anchor_at(sample.start_time);
            }

            if let Some(due) = self.due(sample.start_time) {
                let now = Instant::now();

                if now < due {
                    return Ok(());
                }

                // Being this late means the clock jumped rather than drifted,
                // so start counting again from here.
                if now.duration_since(due) > MAX_LAG {
                    self.anchor_at(sample.start_time);
                }
            }
        }

        // Read straight into the message: the plugin never holds the payload.
        let size = self.lead_in.len() + sample.size;
        let payload = (output.reserve)(output, size);

        if payload.is_null() {
            return Err("the message would not take the sample".to_string());
        }

        let room = unsafe { std::slice::from_raw_parts_mut(payload, size) };
        let (lead_in, rest) = room.split_at_mut(self.lead_in.len());
        lead_in.copy_from_slice(self.lead_in);

        let Some(reader) = self.sample_reader.as_mut() else {
            return Ok(());
        };

        reader.read_sample(rest)?;

        let bytes = Slice::from_raw_parts(payload, size);
        let samples = self.samples.fetch_add(1, Ordering::Relaxed) + 1;

        self.position
            .store(self.micros(sample.start_time), Ordering::Relaxed);

        log_trace!(self.log, "sample {samples}: {sample}");

        let values = [
            sample.start_time.into(),
            (sample.duration as u64).into(),
            (sample.rendering_offset as i64).into(),
            sample.is_sync.into(),
            bytes.into(),
        ];

        (output.fill)(
            output,
            Message {
                values: Slice::from_raw_parts(values.as_ptr(), values.len()),
            },
        );

        self.publish_live();

        Ok(())
    }

    /// End of the file: start over, or say so once and go quiet.
    fn finish(&mut self) -> Result<(), String> {
        if self.looping {
            if let Some(reader) = self.sample_reader.as_mut() {
                reader.rewind()?;
            }

            self.anchor = None;
            self.position.store(0, Ordering::Relaxed);
            self.publish_live();

            log_debug!(self.log, "looping '{}'", self.filename);

            return Ok(());
        }

        // The source keeps spinning at the end of the file, so say so once
        // instead of every round.
        if !self.ended.swap(true, Ordering::Relaxed) {
            log_info!(
                self.log,
                "end of '{}' after {} sample(s)",
                self.filename,
                self.samples.load(Ordering::Relaxed)
            );

            // file.ended carries no payload, but it still has to be queued
            // before it can be published.
            unsafe {
                ((*self.ctx).new_event)(Str::new(self.id.as_str()), 1);
                ((*self.ctx).publish_events)(Str::new(self.id.as_str()), 1);
            }

            log_info!(self.log, "published file.ended");

            self.publish_live();
        }

        Ok(())
    }
}

extern "C" fn metadata() -> &'static Metadata {
    &METADATA
}

extern "C" fn create(ctx: *const HostContext, id: Str) -> PluginHandle {
    let plugin = MediaFileSourcePlugin::new(ctx, id.to_string());

    log_info!(plugin.log, "created");

    // So a page opening before anything happens has readings to show.
    plugin.publish_live();

    Box::into_raw(Box::new(plugin)) as PluginHandle
}

extern "C" fn id(instance: PluginHandle) -> Str {
    let plugin = unsafe { &*(instance as *mut MediaFileSourcePlugin) };
    Str::new(plugin.id.as_str())
}

extern "C" fn release(instance: PluginHandle) {
    unsafe {
        let plugin = Box::from_raw(instance as *mut MediaFileSourcePlugin);

        log_info!(
            plugin.log,
            "released after {} sample(s)",
            plugin.samples.load(Ordering::Relaxed)
        );

        drop(plugin);
    }
}

extern "C" fn process(instance: PluginHandle, p_ctx: *mut ProcessContext) -> bool {
    let plugin = unsafe { &mut *(instance as *mut MediaFileSourcePlugin) };

    match plugin.process(unsafe { &mut *(*p_ctx).output }) {
        Ok(()) => true,
        Err(err) => {
            log_error!(plugin.log, "{err}");
            false
        }
    }
}

extern "C" fn process_input_schema() -> *const Schema {
    ptr::null()
}

extern "C" fn process_output_schema() -> *const Schema {
    &schemas::VIDEO_OUTPUT_SCHEMA
}

extern "C" fn settings_schema() -> *const Schema {
    &schemas::SETTINGS_SCHEMA
}

extern "C" fn settings(_instance: PluginHandle, _output: *mut message::Writer) -> bool {
    true
}

extern "C" fn set_parameter(instance: PluginHandle, field: usize, value: Value) -> bool {
    let plugin = unsafe { &mut *(instance as *mut MediaFileSourcePlugin) };

    match field {
        0 => {
            let Some(path) = value.get::<Str>() else {
                log_error!(plugin.log, "parameter 0 (path) must be a string");
                return false;
            };

            unsafe { plugin.open(PathBuf::from(path.as_str())) }
        }

        // Changing speed re-anchors, otherwise the new rate is measured from
        // the old start and the reader bursts or stalls to catch up.
        1 => {
            plugin.rate = value.get::<f64>().unwrap_or(1.0).max(0.0);
            plugin.anchor = None;

            log_info!(plugin.log, "rate {}", plugin.rate);
            true
        }

        2 => {
            plugin.looping = value.get::<bool>().unwrap_or(false);

            log_info!(plugin.log, "loop {}", plugin.looping);
            true
        }

        3 => {
            plugin.seek_to(value.get::<u64>().unwrap_or(0));
            true
        }

        _ => {
            log_warn!(plugin.log, "ignored unknown parameter {field}");
            true
        }
    }
}

/// UI folder, resolved by the host against the plugin library directory.
extern "C" fn ui() -> Str {
    Str::from_static("mstu-media-file-source-ui")
}

extern "C" fn live_schema() -> *const Schema {
    &schemas::LIVE_SCHEMA
}

extern "C" fn events() -> Slice<EventDescriptor> {
    slice!(schemas::EVENTS)
}

extern "C" fn commands() -> Slice<CommandDescriptor> {
    Slice::empty()
}

extern "C" fn invoke(_instance: PluginHandle, _command: usize, _input: *const Message) -> bool {
    true
}

static PLUGIN: PluginDescriptor = PluginDescriptor {
    metadata,
    create,
    id,
    release,

    process_input_schema,
    process_output_schema,
    settings_schema,

    settings,
    set_parameter,

    ui,

    process,

    events,
    commands,
    invoke,

    live_schema,
};

#[unsafe(no_mangle)]
pub extern "C" fn plugin_descriptor() -> *const PluginDescriptor {
    &PLUGIN
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// A file of its own, named after the test that wants it.
    struct Scratch(PathBuf);

    impl Scratch {
        fn with(name: &str, bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!("mstu-{}-{name}", std::process::id()));
            fs::write(&path, bytes).expect("a file to test with");

            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn box_named(name: &[u8; 4]) -> Vec<u8> {
        let mut bytes = vec![0, 0, 0, 0x14];
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(b"qt  \0\0\0\0");

        bytes
    }

    #[test]
    fn knows_the_family_by_its_first_box() {
        for name in [
            b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"pnot",
        ] {
            let file = Scratch::with("first", &box_named(name));

            assert!(
                is_iso_bmff(&file.0),
                "{} should be read as iso base media",
                std::str::from_utf8(name).unwrap()
            );
        }
    }

    #[test]
    fn turns_down_what_is_not_media() {
        // A zip, a script, and something too short to say.
        for bytes in [&b"PK\x03\x04____"[..], &b"@echo off\r\n"[..], &b"abc"[..]] {
            let file = Scratch::with("other", bytes);

            assert!(!is_iso_bmff(&file.0));
        }
    }

    #[test]
    fn a_file_that_is_not_there_is_not_media() {
        assert!(!is_iso_bmff(Path::new("/nowhere/at/all.mp4")));
    }

    /// The name is a hint and the bytes are the fact.
    #[test]
    fn the_extension_has_no_say() {
        let file = Scratch::with("named.txt", &box_named(b"ftyp"));

        assert!(is_iso_bmff(&file.0));
    }

    // ---------- the plugin itself, without an engine behind it ----------

    extern "C" fn log_nothing(_: mstu_sdk::LogLevel, _: Str, _: Str) {}

    extern "C" fn log_disabled(_: mstu_sdk::LogLevel) -> bool {
        false
    }

    extern "C" fn no_event(_: Str, _: usize) -> *const message::Writer {
        ptr::null()
    }

    extern "C" fn no_publish(_: Str, _: usize) {}
    extern "C" fn no_live(_: Str, _: Message) {}

    /// A host that answers every call with nothing.
    pub fn host() -> Box<HostContext> {
        Box::new(HostContext {
            logger: log_nothing,
            log_enabled: log_disabled,
            new_event: no_event,
            publish_events: no_publish,
            publish_live: no_live,
        })
    }

    /// A plugin with a file open, built from the bytes of a fixture.
    fn opened(
        ctx: &HostContext,
        video: &crate::mp4::fixture::Video,
    ) -> (MediaFileSourcePlugin, PathBuf) {
        let dir = std::env::temp_dir().join(format!("mstu-source-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("somewhere to put it");

        let path = dir.join(format!(
            "{}.mp4",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        fs::write(&path, crate::mp4::fixture::file(video)).expect("a file to open");

        let mut plugin = MediaFileSourcePlugin::new(ctx, "TEST".to_string());
        assert!(plugin.open(path.clone()), "the fixture should open");

        (plugin, path)
    }

    #[test]
    fn a_file_that_opens_says_how_long_it_is() {
        let ctx = host();
        let video = crate::mp4::fixture::Video {
            timescale: 1000,
            delta: 100,
            sizes: vec![10, 20, 30, 40],
            ..Default::default()
        };

        let (plugin, path) = opened(&ctx, &video);

        // Four samples of a tenth of a second each.
        assert_eq!(plugin.duration.load(Ordering::Relaxed), 400_000);
        assert_eq!(plugin.samples.load(Ordering::Relaxed), 0);
        assert!(!plugin.ended.load(Ordering::Relaxed));

        let _ = fs::remove_file(path);
    }

    /// Media time is counted in whatever the file counts in, and the UI wants
    /// microseconds.
    #[test]
    fn media_time_becomes_microseconds() {
        let ctx = host();
        let video = crate::mp4::fixture::Video {
            timescale: 90_000,
            ..Default::default()
        };

        let (plugin, path) = opened(&ctx, &video);

        assert_eq!(plugin.micros(90_000), 1_000_000);
        assert_eq!(plugin.micros(45_000), 500_000);
        assert_eq!(plugin.micros(0), 0);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn with_no_file_open_nothing_has_a_time() {
        let ctx = host();
        let plugin = MediaFileSourcePlugin::new(&*ctx, "TEST".to_string());

        assert_eq!(plugin.micros(1000), 0);
    }

    /// Pacing: a sample is due as far after the anchor as it sits after the
    /// anchor's sample, divided by the speed.
    #[test]
    fn a_sample_is_due_at_its_place_in_the_timeline() {
        let ctx = host();
        let video = crate::mp4::fixture::Video {
            timescale: 1000,
            ..Default::default()
        };

        let (mut plugin, path) = opened(&ctx, &video);

        plugin.anchor_at(0);
        let anchor = plugin.anchor.expect("an anchor");

        // A second of media is a second away at normal speed.
        let due = plugin.due(1000).expect("a due time");
        assert_eq!(due.duration_since(anchor), Duration::from_secs(1));

        // Twice the speed is half the wait.
        plugin.rate = 2.0;
        let due = plugin.due(1000).expect("a due time");
        assert_eq!(due.duration_since(anchor), Duration::from_millis(500));

        // As fast as possible means everything is due at once.
        plugin.rate = 0.0;
        assert_eq!(plugin.due(100_000).expect("a due time"), anchor);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn nothing_is_due_until_the_clock_has_been_started() {
        let ctx = host();
        let video = crate::mp4::fixture::Video::default();
        let (mut plugin, path) = opened(&ctx, &video);

        plugin.anchor = None;

        assert!(plugin.due(1000).is_none());

        let _ = fs::remove_file(path);
    }

    /// Seeking moves where playback is and starts the clock again from there,
    /// so what follows is paced from the new place rather than bursting.
    #[test]
    fn seeking_moves_the_position_and_restarts_the_clock() {
        let ctx = host();
        let video = crate::mp4::fixture::Video {
            timescale: 1000,
            delta: 100,
            sizes: vec![10; 10],
            ..Default::default()
        };

        let (mut plugin, path) = opened(&ctx, &video);

        plugin.ended.store(true, Ordering::Relaxed);
        plugin.seek_to(500_000);

        assert_eq!(plugin.position.load(Ordering::Relaxed), 500_000);
        assert!(
            !plugin.ended.load(Ordering::Relaxed),
            "there is more to read"
        );
        assert!(plugin.anchor.is_some(), "the clock starts again from here");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn opening_a_second_file_forgets_the_first() {
        let ctx = host();

        let short = crate::mp4::fixture::Video {
            sizes: vec![10, 10],
            ..Default::default()
        };

        let (mut plugin, first) = opened(&ctx, &short);

        plugin.samples.store(7, Ordering::Relaxed);
        plugin.ended.store(true, Ordering::Relaxed);

        let longer = crate::mp4::fixture::Video {
            sizes: vec![10; 8],
            ..Default::default()
        };

        let dir = std::env::temp_dir().join(format!("mstu-source-{}", std::process::id()));
        let second = dir.join("second.mp4");
        fs::write(&second, crate::mp4::fixture::file(&longer)).expect("a second file");

        assert!(plugin.open(second.clone()));

        assert_eq!(plugin.samples.load(Ordering::Relaxed), 0);
        assert!(!plugin.ended.load(Ordering::Relaxed));
        assert_eq!(plugin.position.load(Ordering::Relaxed), 0);

        let _ = fs::remove_file(first);
        let _ = fs::remove_file(second);
    }

    /// An av1 stream needs a temporal delimiter in front of every sample; the
    /// others need nothing.
    #[test]
    fn only_av1_is_given_something_in_front_of_each_sample() {
        let ctx = host();

        let h264 = crate::mp4::fixture::Video::default();
        let (plugin, path) = opened(&ctx, &h264);

        assert!(plugin.lead_in.is_empty());
        let _ = fs::remove_file(path);

        let av1 = crate::mp4::fixture::Video {
            entry: crate::mp4::fixture::av01(),
            ..Default::default()
        };

        let (plugin, path) = opened(&ctx, &av1);

        assert_eq!(plugin.lead_in, &[0x12, 0x00]);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_file_that_is_not_media_does_not_open() {
        let ctx = host();
        let mut plugin = MediaFileSourcePlugin::new(&*ctx, "TEST".to_string());

        let dir = std::env::temp_dir().join(format!("mstu-source-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("somewhere to put it");

        let path = dir.join("script.mp4");
        fs::write(&path, b"@echo off\r\n").expect("a file that is not media");

        assert!(!plugin.open(path.clone()));

        let _ = fs::remove_file(path);
    }
}

/// The plugin against files a real encoder wrote.
///
/// The fixtures elsewhere prove the arithmetic on tables written on purpose;
/// these prove the reader against what encoders actually produce. The files
/// live in `test_samples/` and are committed, so nothing has to be encoded to
/// run the tests; `test_samples/generate.sh` says how each one was made and
/// prints the counts the table below is checked against.
#[cfg(test)]
mod real_files {
    use mstu_media::Codec;

    use super::tests::host;
    use super::*;

    /// What is in each file, as ffprobe read it when they were made.
    struct Sample {
        file: &'static str,
        codec: &'static str,
        frames: u64,

        /// How many of those can be decoded from cold.
        keys: u64,
    }

    const SAMPLES: [Sample; 9] = [
        Sample {
            file: "h264.mp4",
            codec: "h264",
            frames: 60,
            keys: 1,
        },
        Sample {
            file: "h264.mov",
            codec: "h264",
            frames: 60,
            keys: 1,
        },
        Sample {
            file: "hvc1.mp4",
            codec: "hevc",
            frames: 60,
            keys: 1,
        },
        Sample {
            file: "hev1.mp4",
            codec: "hevc",
            frames: 60,
            keys: 1,
        },
        Sample {
            file: "av1.mp4",
            codec: "av1",
            frames: 60,
            keys: 1,
        },
        Sample {
            file: "vp9.mp4",
            codec: "vp9",
            frames: 60,
            keys: 1,
        },
        Sample {
            file: "bframes.mp4",
            codec: "h264",
            frames: 60,
            keys: 1,
        },
        Sample {
            file: "longgop.mp4",
            codec: "h264",
            frames: 60,
            keys: 3,
        },
        Sample {
            file: "inorder.mp4",
            codec: "h264",
            frames: 60,
            keys: 1,
        },
    ];

    fn sample(name: &str) -> PathBuf {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test_samples")
            .join(name);

        assert!(
            path.is_file(),
            "{} is missing; test_samples/generate.sh writes it",
            path.display()
        );

        path
    }

    /// The plugin with that file open, or None if it would not open.
    fn opened(ctx: &HostContext, path: &Path) -> Option<MediaFileSourcePlugin> {
        let mut plugin = MediaFileSourcePlugin::new(ctx, "TEST".to_string());

        plugin.open(path.to_path_buf()).then_some(plugin)
    }

    fn codec_name(plugin: &MediaFileSourcePlugin) -> &'static str {
        match plugin.sample_reader.as_ref().expect("a reader").codec() {
            Codec::H264(_) => "h264",
            Codec::H265(_) => "hevc",
            Codec::AV1(_) => "av1",
            Codec::VP9(_) => "vp9",
            Codec::AAC(_) => "aac",
            Codec::TTXT(_) => "ttxt",
        }
    }

    /// Reads to the end: how many samples came out, how many said they can be
    /// decoded from cold, and how many are shown out of the order they are
    /// stored in.
    fn read_all(plugin: &mut MediaFileSourcePlugin) -> (u64, u64, u64) {
        let mut samples = 0;
        let mut sync = 0;
        let mut moved = 0;

        while let Some(info) = plugin
            .sample_reader
            .as_mut()
            .expect("a reader")
            .peek_sample()
            .expect("a sample or the end")
        {
            let mut into = vec![0u8; info.size];

            plugin
                .sample_reader
                .as_mut()
                .unwrap()
                .read_sample(&mut into)
                .expect("the bytes");

            assert!(info.size > 0, "a sample of nothing");
            assert!(into.iter().any(|byte| *byte != 0), "a sample of zeroes");

            samples += 1;
            sync += u64::from(info.is_sync);
            moved += u64::from(info.rendering_offset != 0);
        }

        (samples, sync, moved)
    }

    /// Every file in the folder, read to the end and counted against what is
    /// known to be in it.
    #[test]
    fn every_sample_file_opens_and_reads_to_the_end() {
        let ctx = host();

        for expected in SAMPLES {
            let path = sample(expected.file);
            let mut plugin =
                opened(&ctx, &path).unwrap_or_else(|| panic!("{} should open", expected.file));

            assert_eq!(codec_name(&plugin), expected.codec, "{}", expected.file);
            assert!(
                plugin.duration.load(Ordering::Relaxed) > 0,
                "{} should know how long it is",
                expected.file
            );

            let (samples, sync, _) = read_all(&mut plugin);

            assert_eq!(samples, expected.frames, "{} sample count", expected.file);
            assert_eq!(sync, expected.keys, "{} keyframe count", expected.file);
        }
    }

    /// Frames shown in a different order than they are stored, which is what
    /// the composition table is for.
    #[test]
    fn a_file_with_reordered_frames_says_how_far_each_one_moves() {
        let ctx = host();
        let mut plugin = opened(&ctx, &sample("bframes.mp4")).expect("it should open");

        let (_, _, moved) = read_all(&mut plugin);

        assert!(
            moved > 0,
            "three b-frames deep, something should be reordered"
        );

        // x264 reorders by default, so the file to compare against is the
        // one encoded with that turned off.
        let mut plain = opened(&ctx, &sample("inorder.mp4")).expect("it should open");
        let (_, _, moved) = read_all(&mut plain);

        assert_eq!(moved, 0, "in order, nothing moves");
    }

    /// Only some samples start a group, and only those can be seeked to.
    #[test]
    fn a_file_with_sparse_keyframes_marks_only_those() {
        let ctx = host();
        let mut plugin = opened(&ctx, &sample("longgop.mp4")).expect("it should open");

        let (samples, sync, _) = read_all(&mut plugin);

        assert_eq!(sync, 3, "three groups of pictures");
        assert!(samples > sync, "not every frame starts one");
    }

    /// The source plays video; a file with sound in it as well is common.
    #[test]
    fn a_file_with_sound_in_it_plays_its_video() {
        let ctx = host();
        let mut plugin = opened(&ctx, &sample("with-sound.mp4")).expect("it should open");

        assert_eq!(
            codec_name(&plugin),
            "h264",
            "the video track is the one played"
        );

        let (samples, _, _) = read_all(&mut plugin);

        assert_eq!(samples, 60);
    }

    /// The name of a file says nothing about what is in it.
    #[test]
    fn a_real_file_under_any_name_still_opens() {
        let source = sample("h264.mp4");
        let dir = std::env::temp_dir().join(format!("mstu-named-{}", std::process::id()));

        fs::create_dir_all(&dir).expect("somewhere to put the copies");

        for name in ["no-extension", "misleading.txt", "UPPER.MP4"] {
            let path = dir.join(name);
            fs::copy(&source, &path).expect("a copy under another name");

            let ctx = host();

            assert!(
                opened(&ctx, &path).is_some(),
                "{name} should open on what is in it"
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

    /// Written in pieces rather than one run, which a recorder does so the
    /// file survives being cut off. Those pieces are not read yet, and a
    /// source that opens and then plays nothing would leave the pipeline
    /// waiting forever, so the file is refused instead.
    #[test]
    fn a_fragmented_file_is_refused_rather_than_opened_empty() {
        let ctx = host();

        assert!(
            opened(&ctx, &sample("fragmented.mp4")).is_none(),
            "a file with nothing to play should not open"
        );
    }

    /// Seeking lands on the sample covering the time asked for.
    #[test]
    fn seeking_into_a_real_file_lands_where_it_was_asked_to() {
        let ctx = host();
        let mut plugin = opened(&ctx, &sample("longgop.mp4")).expect("it should open");

        // A second in, on a two second clip.
        plugin.seek_to(1_000_000);

        assert_eq!(plugin.position.load(Ordering::Relaxed), 1_000_000);

        let next = plugin
            .sample_reader
            .as_mut()
            .unwrap()
            .peek_sample()
            .expect("a sample")
            .expect("there is more to read");

        let micros = plugin.micros(next.start_time);

        assert!(
            micros <= 1_000_000,
            "a seek lands on or before the time asked for, not after it: {micros}"
        );
    }
}

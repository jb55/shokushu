//! Writing a recording out as a Broadcast Wave file, with the timecode in it.
//!
//! Reading a Tentacle is only half of a job that ends with a file something
//! else can sync. A plain WAV can say when a recording started only in its
//! filename, which nothing downstream reads. BWF adds a `bext` chunk whose
//! `TimeReference` is the position of the first sample as a count of samples
//! since midnight, and that is the field an NLE actually syncs on — so a file
//! written here drops onto a timeline in the right place with nothing typed in.
//!
//! `TimeReference` counts samples rather than frames, which is what makes it
//! worth pairing with this crate: at 48 kHz a sample is 21 µs, where a frame at
//! 25 fps is 40 ms, so it has room for the sub-frame placement an
//! advertisement's microsecond field gives you. What it has no room for is the
//! frame rate — `bext` has nowhere to put one — so an iXML chunk goes alongside
//! carrying `TIMECODE_RATE`, and repeats the sample count in iXML's own fields.
//!
//! Nothing here reads a clock or opens a device: it is handed a [`Stamp`] and
//! writes what it is told. Working out what the stamp should say is the
//! caller's job, and `src/bin/shokushu-rec.rs` is that job done.
//!
//! ```no_run
//! use std::path::Path;
//!
//! use shokushu::wav::{Bwf, Spec, Stamp};
//! use shokushu::{Rate, Timecode};
//!
//! # fn main() -> std::io::Result<()> {
//! let spec = Spec { sample_rate: 48_000, channels: 1 };
//! let start = Timecode::new(11, 37, 40, 21, Rate::whole(25));
//!
//! let mut file = Bwf::create(Path::new("take.wav"), spec, &Stamp {
//!     // Where the first sample lands. The frames field alone would place it
//!     // to 40 ms; this is the field that does better.
//!     samples_since_midnight: shokushu::wav::samples_since_midnight(&start, spec.sample_rate)
//!         .expect("25 fps is not a drop-frame rate"),
//!     timecode: start,
//!     date: Some("2026-09-04".to_string()),
//!     originator: "shokushu-rec".to_string(),
//!     note: "Ricki".to_string(),
//! })?;
//!
//! file.write(&[0.0; 480])?;
//! file.finish()
//! # }
//! ```
//!
//! Samples are stored as 24-bit PCM. It is what field recorders write, it costs
//! nothing to produce from the `f32` an audio callback hands over, and it
//! leaves no question about float-WAV support at the other end.
//!
//! Everything but the two size fields is known before the first sample goes
//! down, so the header is written complete and only those two are patched at
//! the end — which also means an interrupted recording differs from a finished
//! one in exactly two `u32`s.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use crate::Timecode;

/// Bytes per sample on disk.
const BYTES_PER_SAMPLE: u16 = 3;

/// Full scale for a 24-bit sample. Positive values reach one less than this,
/// which is what the clamp in [`Bwf::write`] is for.
const FULL_SCALE: f32 = 8_388_608.0;

/// Size of a `bext` chunk with no coding history, which the spec fixes.
const BEXT_SIZE: u32 = 602;

/// What the file is, as opposed to what is in it.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub sample_rate: u32,
    pub channels: u16,
}

impl Spec {
    fn block_align(&self) -> u16 {
        self.channels * BYTES_PER_SAMPLE
    }
}

/// Where the recording sits on a timeline, and who says so.
#[derive(Debug, Clone)]
pub struct Stamp {
    /// Timecode of the first sample. Its [`Rate`](crate::Rate) is what the iXML
    /// chunk reports; the fields themselves go into `bext`'s human-readable
    /// half.
    pub timecode: Timecode,
    /// The first sample's position on the timecode day, in samples at the
    /// file's rate — `bext`'s `TimeReference`, and the field anything
    /// downstream actually syncs on. [`samples_since_midnight`] computes it.
    ///
    /// Taken rather than derived because it is the number that matters, and
    /// because a caller with a finer measurement of where the first sample fell
    /// than the timecode's own fields carry should be able to write it.
    pub samples_since_midnight: u64,
    /// The date the timecode belongs to, `yyyy-mm-dd`. A Tentacle broadcasts
    /// one; an LTC frame carries one in its user bits if it was set.
    pub date: Option<String>,
    /// What wrote the file.
    pub originator: String,
    /// Anything worth saying about the recording — the device it was synced to,
    /// say. Lands in `bext`'s Description and iXML's `NOTE`.
    pub note: String,
}

/// Where a timecode sits on the day, in samples at `sample_rate`.
///
/// `None` at a drop-frame rate, where the count of frames since midnight isn't
/// a linear function of the fields and this crate doesn't implement the
/// arithmetic that says what it is — see
/// [`Timecode::frame_position`](crate::Timecode::frame_position). Bluetooth
/// never reports drop-frame, so a stamp taken from an advertisement always has
/// an answer; one taken from LTC may not.
pub fn samples_since_midnight(timecode: &Timecode, sample_rate: u32) -> Option<u64> {
    let position = timecode.checked_frame_position()?;
    let seconds = position / timecode.rate.exact_fps().max(1.0);
    Some((seconds * sample_rate as f64).round().max(0.0) as u64)
}

/// A Broadcast Wave file being written.
pub struct Bwf {
    file: BufWriter<File>,
    spec: Spec,
    /// Offset of the `data` chunk's size field, which isn't known until the end.
    data_size_at: u64,
    frames: u64,
    clipped: u64,
    /// Samples converted to bytes, reused between calls so that writing does
    /// not allocate once it is going.
    scratch: Vec<u8>,
}

impl Bwf {
    /// Creates the file and writes everything but the samples.
    pub fn create(path: &Path, spec: Spec, stamp: &Stamp) -> io::Result<Bwf> {
        let mut file = BufWriter::new(File::create(path)?);

        let mut header: Vec<u8> = Vec::new();
        header.extend(b"RIFF");
        header.extend(0u32.to_le_bytes()); // patched by finish
        header.extend(b"WAVE");

        header.extend(b"fmt ");
        header.extend(16u32.to_le_bytes());
        header.extend(1u16.to_le_bytes()); // PCM
        header.extend(spec.channels.to_le_bytes());
        header.extend(spec.sample_rate.to_le_bytes());
        header.extend((spec.sample_rate * spec.block_align() as u32).to_le_bytes());
        header.extend(spec.block_align().to_le_bytes());
        header.extend((BYTES_PER_SAMPLE * 8).to_le_bytes());

        header.extend(b"bext");
        header.extend(BEXT_SIZE.to_le_bytes());
        header.extend(bext(stamp));

        let xml = ixml(stamp, &spec);
        header.extend(b"iXML");
        header.extend((xml.len() as u32).to_le_bytes());
        header.extend(xml.as_bytes());

        header.extend(b"data");
        let data_size_at = header.len() as u64;
        header.extend(0u32.to_le_bytes()); // patched by finish

        file.write_all(&header)?;

        Ok(Bwf {
            file,
            spec,
            data_size_at,
            frames: 0,
            clipped: 0,
            scratch: Vec::new(),
        })
    }

    /// Appends interleaved samples, in the channel order the device sent them.
    pub fn write(&mut self, samples: &[f32]) -> io::Result<()> {
        self.scratch.clear();
        self.scratch
            .reserve(samples.len() * BYTES_PER_SAMPLE as usize);
        for &sample in samples {
            if sample.abs() > 1.0 {
                self.clipped += 1;
            }
            // Clamped rather than wrapped: wrapping turns a loud moment into a
            // burst of noise at the opposite polarity, which is a worse thing
            // to have recorded than a flat top.
            let value = (sample * FULL_SCALE).round().clamp(-FULL_SCALE, FULL_SCALE - 1.0) as i32;
            self.scratch
                .extend(&value.to_le_bytes()[..BYTES_PER_SAMPLE as usize]);
        }
        self.file.write_all(&self.scratch)?;
        self.frames += samples.len() as u64 / self.spec.channels.max(1) as u64;
        Ok(())
    }

    /// Sample frames written so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// How many samples arrived past full scale. Worth saying out loud at the
    /// end of a recording: they were clamped, so they are gone from the file.
    pub fn clipped(&self) -> u64 {
        self.clipped
    }

    /// Fills in the two sizes that weren't known when the header went down.
    pub fn finish(mut self) -> io::Result<()> {
        let data_size = self.frames * self.spec.block_align() as u64;
        // A RIFF chunk sits on an even boundary, and 24-bit samples make an odd
        // one often enough to matter. The pad byte is part of the file but not
        // of the chunk, so it goes in before the total is measured and stays
        // out of `data`'s own size.
        if data_size % 2 == 1 {
            self.file.write_all(&[0])?;
        }

        let total = self.file.seek(SeekFrom::End(0))?;
        self.patch(self.data_size_at, data_size as u32)?;
        self.patch(4, total.saturating_sub(8) as u32)?;
        self.file.flush()
    }

    fn patch(&mut self, at: u64, value: u32) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(at))?;
        self.file.write_all(&value.to_le_bytes())
    }
}

/// The `bext` chunk body: fixed-width fields at fixed offsets, null-padded.
///
/// The layout is EBU Tech 3285 and none of it is negotiable — a field in the
/// wrong place is not a file a reader complains about, it's a file that syncs
/// somewhere else.
fn bext(stamp: &Stamp) -> Vec<u8> {
    let mut body = vec![0u8; BEXT_SIZE as usize];
    ascii(&mut body[0..256], &stamp.note);
    ascii(&mut body[256..288], &stamp.originator);
    // OriginatorReference (288..320) is left empty on purpose. It is specified
    // as an unambiguous identifier for the source — a USID — and nothing here
    // has one to give. A timecode fits in the field and is not what the field
    // is; readers that show it would be showing a lie about what it means.
    ascii(&mut body[320..330], stamp.date.as_deref().unwrap_or(""));
    // OriginationTime is a clock time, and the only clock in play is the
    // device's, so it gets the timecode with the frames cut off.
    let tc = &stamp.timecode;
    ascii(
        &mut body[330..338],
        &format!("{:02}:{:02}:{:02}", tc.hours, tc.minutes, tc.seconds),
    );
    body[338..342].copy_from_slice(&(stamp.samples_since_midnight as u32).to_le_bytes());
    body[342..346].copy_from_slice(&((stamp.samples_since_midnight >> 32) as u32).to_le_bytes());
    body[346..348].copy_from_slice(&1u16.to_le_bytes()); // BWF version 1
    body
}

/// Copies `text` into a fixed-width field, truncated and null-padded.
///
/// The fields are ASCII, and a Tentacle will happily take a name that isn't —
/// which would otherwise put bytes in a field specified not to hold them.
fn ascii(field: &mut [u8], text: &str) {
    for (slot, byte) in field.iter_mut().zip(text.bytes()) {
        *slot = if byte.is_ascii_graphic() || byte == b' ' {
            byte
        } else {
            b'?'
        };
    }
}

/// The iXML chunk: the frame rate, which `bext` has nowhere to put, and the
/// sample count again in iXML's own fields.
fn ixml(stamp: &Stamp, spec: &Spec) -> String {
    let rate = stamp.timecode.rate;
    // iXML spells a rate as a ratio, which is how the fractional rates get said
    // exactly: 30000/1001, not 29.97.
    let (numerator, denominator, flag) = match rate.drop_frame {
        true => (rate.fps as u32 * 1000, 1001, "DF"),
        false => (rate.fps as u32, 1, "NDF"),
    };
    let sample_rate = spec.sample_rate;
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <BWFXML>\n\
         \x20<IXML_VERSION>1.5</IXML_VERSION>\n\
         \x20<PROJECT>{}</PROJECT>\n\
         \x20<NOTE>{}</NOTE>\n\
         \x20<SPEED>\n\
         \x20 <TIMECODE_RATE>{numerator}/{denominator}</TIMECODE_RATE>\n\
         \x20 <TIMECODE_FLAG>{flag}</TIMECODE_FLAG>\n\
         \x20 <FILE_SAMPLE_RATE>{sample_rate}</FILE_SAMPLE_RATE>\n\
         \x20 <TIMESTAMP_SAMPLE_RATE>{sample_rate}</TIMESTAMP_SAMPLE_RATE>\n\
         \x20 <TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_LO>{}</TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_LO>\n\
         \x20 <TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_HI>{}</TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_HI>\n\
         \x20</SPEED>\n\
         </BWFXML>\n",
        escape(&stamp.originator),
        escape(&stamp.note),
        stamp.samples_since_midnight as u32,
        stamp.samples_since_midnight >> 32,
    );
    // A chunk's payload sits on an even boundary. Padding the text itself,
    // rather than adding a pad byte outside the declared size, means a reader
    // that takes the length at its word still gets well-formed XML.
    if xml.len() % 2 == 1 {
        xml.push('\n');
    }
    xml
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rate;

    /// The chunks of a RIFF file, as (id, payload), checking the framing on the
    /// way through.
    fn chunks(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
        assert_eq!(&bytes[0..4], b"RIFF");
        let declared = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        assert_eq!(declared + 8, bytes.len(), "the RIFF size isn't the file");
        assert_eq!(&bytes[8..12], b"WAVE");

        let mut found = Vec::new();
        let mut at = 12;
        while at + 8 <= bytes.len() {
            let id = String::from_utf8_lossy(&bytes[at..at + 4]).to_string();
            let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
            found.push((id, bytes[at + 8..at + 8 + size].to_vec()));
            // Chunks are even-aligned; an odd one is followed by a pad byte.
            at += 8 + size + size % 2;
        }
        assert_eq!(at, bytes.len(), "a chunk ran off the end of the file");
        found
    }

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("shokushu-wav-{}-{name}.wav", std::process::id()))
    }

    fn stamp() -> Stamp {
        Stamp {
            timecode: Timecode::new(11, 37, 40, 21, Rate::whole(25)),
            samples_since_midnight: 2_008_660_032,
            date: Some("2026-09-04".to_string()),
            originator: "shokushu-rec".to_string(),
            note: "Ricki".to_string(),
        }
    }

    fn write_one(name: &str, spec: Spec, stamp: &Stamp, samples: &[f32]) -> Vec<u8> {
        let path = temp(name);
        let mut bwf = Bwf::create(&path, spec, stamp).unwrap();
        bwf.write(samples).unwrap();
        assert_eq!(bwf.frames(), samples.len() as u64 / spec.channels as u64);
        bwf.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        bytes
    }

    #[test]
    fn the_chunks_add_up_to_the_file() {
        // Three channels of 24-bit is nine bytes a frame, so an odd number of
        // frames makes an odd data chunk — the case the pad byte exists for,
        // and the one where a wrong RIFF size shows up.
        let spec = Spec {
            sample_rate: 48_000,
            channels: 3,
        };
        let bytes = write_one("chunks", spec, &stamp(), &[0.0; 33]);
        let chunks = chunks(&bytes);
        let ids: Vec<&str> = chunks.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["fmt ", "bext", "iXML", "data"]);
        assert_eq!(chunks[3].1.len(), 33 * 3);
    }

    #[test]
    fn the_format_is_what_the_spec_asked_for() {
        let spec = Spec {
            sample_rate: 44_100,
            channels: 2,
        };
        let bytes = write_one("format", spec, &stamp(), &[0.0; 8]);
        let fmt = &chunks(&bytes)[0].1;
        assert_eq!(u16::from_le_bytes(fmt[0..2].try_into().unwrap()), 1); // PCM
        assert_eq!(u16::from_le_bytes(fmt[2..4].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(fmt[4..8].try_into().unwrap()), 44_100);
        // Byte rate and block align have to agree with the sample width, or the
        // file plays at the wrong speed rather than being refused.
        assert_eq!(u32::from_le_bytes(fmt[8..12].try_into().unwrap()), 44_100 * 6);
        assert_eq!(u16::from_le_bytes(fmt[12..14].try_into().unwrap()), 6);
        assert_eq!(u16::from_le_bytes(fmt[14..16].try_into().unwrap()), 24);
    }

    #[test]
    fn the_timecode_lands_where_a_reader_looks_for_it() {
        // The point of the exercise: TimeReference at offset 338 of `bext`, as
        // two 32-bit halves. Swapping the halves puts the recording 24 days
        // out, which is the kind of thing that looks fine until someone opens
        // the file.
        let spec = Spec {
            sample_rate: 48_000,
            channels: 1,
        };
        let mut stamp = stamp();
        // Past 2^32 samples, which is 24 hours at 48 kHz — timecode wraps
        // before this, but the arithmetic mustn't.
        stamp.samples_since_midnight = 0x1_2345_6789;
        let bytes = write_one("bext", spec, &stamp, &[0.0; 4]);
        let bext = &chunks(&bytes)[1].1;
        assert_eq!(bext.len(), BEXT_SIZE as usize);
        assert_eq!(u32::from_le_bytes(bext[338..342].try_into().unwrap()), 0x2345_6789);
        assert_eq!(u32::from_le_bytes(bext[342..346].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(bext[346..348].try_into().unwrap()), 1);

        let text = |field: &[u8]| {
            String::from_utf8_lossy(field)
                .trim_end_matches('\0')
                .to_string()
        };
        assert_eq!(text(&bext[0..256]), "Ricki");
        assert_eq!(text(&bext[256..288]), "shokushu-rec");
        // OriginatorReference: empty, since we have no USID to put in it.
        assert_eq!(text(&bext[288..320]), "");
        assert_eq!(text(&bext[320..330]), "2026-09-04");
        assert_eq!(text(&bext[330..338]), "11:37:40");
    }

    #[test]
    fn the_frame_rate_survives_in_the_ixml() {
        // `bext` has nowhere to put a rate, so without this chunk the file
        // says when it starts and not what the numbering means.
        let spec = Spec {
            sample_rate: 48_000,
            channels: 1,
        };
        let bytes = write_one("ixml", spec, &stamp(), &[0.0; 4]);
        let xml = String::from_utf8(chunks(&bytes)[2].1.clone()).unwrap();
        assert!(xml.contains("<TIMECODE_RATE>25/1</TIMECODE_RATE>"), "{xml}");
        assert!(xml.contains("<TIMECODE_FLAG>NDF</TIMECODE_FLAG>"), "{xml}");
        assert!(xml.contains("<FILE_SAMPLE_RATE>48000<"), "{xml}");
        assert!(
            xml.contains("<TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_LO>2008660032<"),
            "{xml}"
        );
    }

    #[test]
    fn a_drop_frame_rate_is_written_as_a_ratio() {
        // 29.97 is 30000/1001 and not 2997/100; a reader that takes the
        // rounded figure runs 1.2 frames an hour off.
        let spec = Spec {
            sample_rate: 48_000,
            channels: 1,
        };
        let mut stamp = stamp();
        stamp.timecode = Timecode::new(1, 0, 0, 0, Rate::new(30, true));
        let bytes = write_one("df", spec, &stamp, &[0.0; 4]);
        let xml = String::from_utf8(chunks(&bytes)[2].1.clone()).unwrap();
        assert!(
            xml.contains("<TIMECODE_RATE>30000/1001</TIMECODE_RATE>"),
            "{xml}"
        );
        assert!(xml.contains("<TIMECODE_FLAG>DF</TIMECODE_FLAG>"), "{xml}");
    }

    #[test]
    fn samples_since_midnight_counts_from_the_position() {
        let tc = Timecode::new(1, 0, 0, 0, Rate::whole(25));
        assert_eq!(samples_since_midnight(&tc, 48_000), Some(3600 * 48_000));

        // The sub-frame field is the reason this is in samples and not frames:
        // half a frame at 25 fps is 960 samples at 48 kHz, and a reading that
        // carries it should not be rounded back to the frame boundary.
        let mut half = tc;
        half.subframe = std::time::Duration::from_micros(20_000);
        assert_eq!(samples_since_midnight(&half, 48_000), Some(3600 * 48_000 + 960));

        // Drop-frame has no answer here rather than a wrong one.
        let df = Timecode::new(1, 0, 0, 0, Rate::new(30, true));
        assert_eq!(samples_since_midnight(&df, 48_000), None);
    }

    #[test]
    fn samples_are_converted_and_clipping_is_counted() {
        let spec = Spec {
            sample_rate: 48_000,
            channels: 1,
        };
        let path = temp("convert");
        let mut bwf = Bwf::create(&path, spec, &stamp()).unwrap();
        bwf.write(&[0.0, 0.5, -0.5, 2.0, -2.0]).unwrap();
        assert_eq!(bwf.clipped(), 2);
        bwf.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        let data = &chunks(&bytes)[3].1;
        let sample = |n: usize| {
            let b = &data[n * 3..n * 3 + 3];
            // Sign-extend the 24-bit value.
            i32::from_le_bytes([b[0], b[1], b[2], if b[2] & 0x80 != 0 { 0xff } else { 0 }])
        };
        assert_eq!(sample(0), 0);
        assert_eq!(sample(1), 4_194_304);
        assert_eq!(sample(2), -4_194_304);
        assert_eq!(sample(3), 8_388_607);
        assert_eq!(sample(4), -8_388_608);
    }
}

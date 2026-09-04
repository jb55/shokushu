//! SMPTE Linear Timecode (LTC) decoding from a stream of audio samples.
//!
//! LTC carries 80 bits per video frame, biphase-mark encoded: every bit cell
//! starts with a transition, and a `1` adds a second transition in the middle
//! of the cell. So a `0` is one long interval between transitions and a `1` is
//! two short ones, which makes the code self-clocking and immune to the polarity
//! of the audio connection.
//!
//! The last 16 bits of every frame are the fixed sync word `0x3FFD`. Twelve
//! consecutive ones cannot occur in the payload, so the sync word is what lets
//! us find frame boundaries in a free-running bit stream.

/// Bits in one LTC frame.
pub const FRAME_BITS: usize = 80;

/// Trailing sync word, in transmission order (bit 64 first).
const SYNC_WORD: u16 = 0x3FFD;

/// Cutoff coefficient for the DC-blocking one-pole (~20 Hz at 48 kHz).
const DC_ALPHA: f64 = 0.0026;
/// Per-sample decay of the peak envelope (~50 ms at 48 kHz).
const ENV_DECAY: f64 = 0.99958;
/// Zero-crossing hysteresis, as a fraction of the envelope.
const HYSTERESIS: f64 = 0.25;
/// Envelope below this counts as no signal at all rather than as quiet LTC.
const SILENCE_FLOOR: f64 = 0.001;
/// How fast the bit-period estimate chases the observed interval.
const PERIOD_ADAPT: f64 = 0.1;

/// One decoded LTC frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LtcFrame {
    pub hours: u8,
    pub minutes: u8,
    pub seconds: u8,
    pub frames: u8,
    pub drop_frame: bool,
    pub color_frame: bool,
    /// The eight 4-bit user-data groups, in transmission order.
    pub user_bits: [u8; 8],
}

impl LtcFrame {
    /// Parses the 80 bits of a frame, newest bit in the low position of `reg`.
    ///
    /// Returns `None` if any BCD field is out of range, which is the cheapest
    /// check we have against a bit slip that happened to land on a sync word.
    fn from_register(reg: u128) -> Option<LtcFrame> {
        let frames = field(reg, 0, 4) + field(reg, 8, 2) * 10;
        let seconds = field(reg, 16, 4) + field(reg, 24, 3) * 10;
        let minutes = field(reg, 32, 4) + field(reg, 40, 3) * 10;
        let hours = field(reg, 48, 4) + field(reg, 56, 2) * 10;

        if frames > 30 || seconds > 59 || minutes > 59 || hours > 23 {
            return None;
        }

        let mut user_bits = [0u8; 8];
        for (group, slot) in user_bits.iter_mut().enumerate() {
            *slot = field(reg, 4 + group * 8, 4);
        }

        Some(LtcFrame {
            hours,
            minutes,
            seconds,
            frames,
            drop_frame: bit(reg, 10) == 1,
            color_frame: bit(reg, 11) == 1,
            user_bits,
        })
    }

    /// The user bits rendered as the eight hex digits they usually stand for.
    pub fn user_bits_hex(&self) -> String {
        self.user_bits.iter().map(|n| char::from_digit(*n as u32, 16).unwrap()).collect()
    }
}

impl std::fmt::Display for LtcFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Drop-frame timecode is conventionally written with a semicolon.
        let sep = if self.drop_frame { ';' } else { ':' };
        write!(
            f,
            "{:02}:{:02}:{:02}{}{:02}",
            self.hours, self.minutes, self.seconds, sep, self.frames
        )
    }
}

/// Frame bit `i` (0-based, transmission order) out of an 80-bit register whose
/// most recently shifted-in bit sits at position 0.
fn bit(reg: u128, i: usize) -> u8 {
    ((reg >> (FRAME_BITS - 1 - i)) & 1) as u8
}

/// A little-endian BCD-style field of `n` bits starting at frame bit `start`.
fn field(reg: u128, start: usize, n: usize) -> u8 {
    (0..n).map(|k| bit(reg, start + k) << k).sum()
}

/// A frame together with where in the stream it landed.
#[derive(Debug, Clone, Copy)]
pub struct DecodedFrame {
    pub frame: LtcFrame,
    /// Index, in samples since the decoder was created, of the end of this
    /// frame's audio. Timecode describes the instant the frame *started*, so
    /// this is roughly one frame period after the moment being labelled.
    pub end_sample: u64,
    /// Frame rate implied by the bit period the decoder is locked to.
    pub measured_fps: f64,
}

impl DecodedFrame {
    /// `measured_fps` snapped to the nearest rate LTC is actually run at.
    ///
    /// Note that 23.976 and 29.97 differ from 24 and 30 by only 0.1%, which is
    /// finer than the bit-period estimate resolves; the drop-frame flag is the
    /// only reliable hint, and only for 29.97.
    pub fn nominal_fps(&self) -> f64 {
        let nearest = [24.0f64, 25.0, 30.0]
            .into_iter()
            .min_by(|a, b| {
                (a - self.measured_fps)
                    .abs()
                    .total_cmp(&(b - self.measured_fps).abs())
            })
            .unwrap();
        if self.frame.drop_frame && nearest == 30.0 {
            29.97
        } else {
            nearest
        }
    }
}

/// Streaming LTC decoder: feed it mono samples, get frames out.
pub struct LtcDecoder {
    sample_rate: f64,

    // Transition detector.
    dc: f64,
    envelope: f64,
    polarity: bool,
    since_transition: f64,

    // Biphase-mark demodulator.
    bit_period: f64,
    /// Length of the first half-cell of a `1`, while we wait for the second.
    pending_half: Option<f64>,

    // Frame assembly.
    reg: u128,
    bits_since_sync: usize,
    synced: bool,

    samples_seen: u64,
}

impl LtcDecoder {
    pub fn new(sample_rate: f64) -> LtcDecoder {
        LtcDecoder {
            sample_rate,
            dc: 0.0,
            envelope: 0.0,
            polarity: false,
            since_transition: 0.0,
            // Seed between the 24 fps and 30 fps bit periods so the very first
            // interval already classifies correctly at any standard rate.
            bit_period: sample_rate / (FRAME_BITS as f64 * 27.0),
            pending_half: None,
            reg: 0,
            bits_since_sync: 0,
            synced: false,
            samples_seen: 0,
        }
    }

    /// Current peak envelope of the input, for level metering.
    pub fn level(&self) -> f64 {
        self.envelope
    }

    /// Feeds mono samples in, appending any completed frames to `out`.
    pub fn process(&mut self, samples: &[f32], out: &mut Vec<DecodedFrame>) {
        for &sample in samples {
            self.push_sample(sample as f64, out);
        }
    }

    fn push_sample(&mut self, x: f64, out: &mut Vec<DecodedFrame>) {
        self.samples_seen += 1;
        self.since_transition += 1.0;

        self.dc += (x - self.dc) * DC_ALPHA;
        let ac = x - self.dc;

        let mag = ac.abs();
        self.envelope = if mag > self.envelope {
            mag
        } else {
            self.envelope * ENV_DECAY
        };

        if self.envelope < SILENCE_FLOOR {
            // Nothing but noise on the wire; don't let it clock the demodulator.
            self.lose_lock();
            return;
        }

        let threshold = self.envelope * HYSTERESIS;
        let crossed = if self.polarity {
            ac < -threshold
        } else {
            ac > threshold
        };
        if !crossed {
            return;
        }

        self.polarity = !self.polarity;
        let interval = self.since_transition;
        self.since_transition = 0.0;
        self.on_transition(interval, out);
    }

    fn on_transition(&mut self, interval: f64, out: &mut Vec<DecodedFrame>) {
        if interval > self.bit_period * 2.0 {
            // Longer than any legal bit cell: a dropout, or we were listening
            // to something that isn't LTC.
            self.lose_lock();
            return;
        }
        if interval < self.bit_period * 0.25 {
            // Ringing on an edge rather than a real transition.
            return;
        }

        if interval > self.bit_period * 0.75 {
            self.bit_period += (interval - self.bit_period) * PERIOD_ADAPT;
            self.pending_half = None;
            self.push_bit(0, out);
        } else if let Some(first_half) = self.pending_half.take() {
            // Both halves together span one cell; using only the second would
            // inherit whatever rounding the generator applied to a half.
            let cell = first_half + interval;
            self.bit_period += (cell - self.bit_period) * PERIOD_ADAPT;
            self.push_bit(1, out);
        } else {
            self.pending_half = Some(interval);
        }
    }

    fn push_bit(&mut self, bit: u8, out: &mut Vec<DecodedFrame>) {
        self.reg = (self.reg << 1) | bit as u128;
        self.bits_since_sync += 1;

        if self.reg as u16 != SYNC_WORD {
            return;
        }

        if !self.synced {
            // First sync word: the register still holds pre-lock garbage ahead
            // of it, so take the alignment but not the frame.
            self.synced = true;
            self.bits_since_sync = 0;
        } else if self.bits_since_sync == FRAME_BITS {
            if let Some(frame) = LtcFrame::from_register(self.reg) {
                out.push(DecodedFrame {
                    frame,
                    end_sample: self.samples_seen,
                    measured_fps: self.sample_rate / (self.bit_period * FRAME_BITS as f64),
                });
            }
            self.bits_since_sync = 0;
        } else if self.bits_since_sync > FRAME_BITS {
            // We slipped past a sync word; re-acquire from this one.
            self.bits_since_sync = 0;
        }
        // A sync pattern short of 80 bits is payload that happens to match, so
        // leave the alignment alone.
    }

    fn lose_lock(&mut self) {
        self.synced = false;
        self.bits_since_sync = 0;
        self.pending_half = None;
        self.reg = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a little-endian field of `n` bits at frame bit `start`.
    fn put(bits: &mut [u8; FRAME_BITS], start: usize, n: usize, value: u8) {
        for k in 0..n {
            bits[start + k] = (value >> k) & 1;
        }
    }

    /// Lays out the 80 bits of one frame in transmission order.
    fn frame_bits(f: &LtcFrame) -> [u8; FRAME_BITS] {
        let mut bits = [0u8; FRAME_BITS];
        put(&mut bits, 0, 4, f.frames % 10);
        put(&mut bits, 8, 2, f.frames / 10);
        bits[10] = f.drop_frame as u8;
        bits[11] = f.color_frame as u8;
        put(&mut bits, 16, 4, f.seconds % 10);
        put(&mut bits, 24, 3, f.seconds / 10);
        put(&mut bits, 32, 4, f.minutes % 10);
        put(&mut bits, 40, 3, f.minutes / 10);
        put(&mut bits, 48, 4, f.hours % 10);
        put(&mut bits, 56, 2, f.hours / 10);
        for (group, nibble) in f.user_bits.iter().enumerate() {
            put(&mut bits, 4 + group * 8, 4, *nibble);
        }
        for (i, b) in [0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1]
            .into_iter()
            .enumerate()
        {
            bits[64 + i] = b;
        }
        bits
    }

    /// Biphase-mark modulator, so tests can drive the decoder with real LTC
    /// audio instead of trusting it against itself.
    struct Encoder {
        samples_per_half: f64,
        amplitude: f32,
        level: f32,
        target: f64,
        out: Vec<f32>,
    }

    impl Encoder {
        fn new(sample_rate: f64, fps: f64, amplitude: f32) -> Encoder {
            Encoder {
                samples_per_half: sample_rate / (fps * FRAME_BITS as f64 * 2.0),
                amplitude,
                level: 1.0,
                target: 0.0,
                out: Vec::new(),
            }
        }

        fn half_cell(&mut self) {
            self.level = -self.level;
            self.target += self.samples_per_half;
            while (self.out.len() as f64) < self.target {
                self.out.push(self.level * self.amplitude);
            }
        }

        /// Emits the leading transition of the cell that would come next,
        /// which is what clocks the final bit of the run out of the decoder.
        fn finish(&mut self) {
            self.half_cell();
        }

        fn push(&mut self, frame: &LtcFrame) {
            for bit in frame_bits(frame) {
                // Every cell starts with a transition; a `1` adds one halfway.
                self.half_cell();
                if bit == 1 {
                    self.half_cell();
                } else {
                    self.target += self.samples_per_half;
                    while (self.out.len() as f64) < self.target {
                        self.out.push(self.level * self.amplitude);
                    }
                }
            }
        }
    }

    fn tc(hours: u8, minutes: u8, seconds: u8, frames: u8) -> LtcFrame {
        LtcFrame {
            hours,
            minutes,
            seconds,
            frames,
            drop_frame: false,
            color_frame: false,
            user_bits: [0; 8],
        }
    }

    /// Advances a non-drop-frame timecode by one frame.
    fn advance(f: &mut LtcFrame, fps: u8) {
        f.frames += 1;
        if f.frames < fps {
            return;
        }
        f.frames = 0;
        f.seconds += 1;
        if f.seconds < 60 {
            return;
        }
        f.seconds = 0;
        f.minutes += 1;
        if f.minutes < 60 {
            return;
        }
        f.minutes = 0;
        f.hours = (f.hours + 1) % 24;
    }

    /// Encodes `count` consecutive frames and returns what the decoder made of
    /// them, alongside the timecodes that went in.
    fn roundtrip(
        sample_rate: f64,
        fps: f64,
        start: LtcFrame,
        count: usize,
    ) -> (Vec<LtcFrame>, Vec<DecodedFrame>) {
        let mut encoder = Encoder::new(sample_rate, fps, 0.5);
        let mut sent = Vec::new();
        let mut current = start;
        for _ in 0..count {
            encoder.push(&current);
            sent.push(current);
            advance(&mut current, fps.round() as u8);
        }
        encoder.finish();

        let mut decoder = LtcDecoder::new(sample_rate);
        let mut got = Vec::new();
        decoder.process(&encoder.out, &mut got);
        (sent, got)
    }

    #[test]
    fn decodes_a_run_of_frames_at_30fps() {
        let (sent, got) = roundtrip(48000.0, 30.0, tc(1, 2, 3, 4), 10);
        // The first sync word only establishes alignment, so we lose frame one.
        let decoded: Vec<LtcFrame> = got.iter().map(|d| d.frame).collect();
        assert_eq!(decoded, sent[1..]);
    }

    #[test]
    fn decodes_at_25fps_and_44100hz() {
        let (sent, got) = roundtrip(44100.0, 25.0, tc(9, 59, 59, 20), 12);
        let decoded: Vec<LtcFrame> = got.iter().map(|d| d.frame).collect();
        assert_eq!(decoded, sent[1..]);
        // ...and that run rolls the minute over.
        assert_eq!(decoded[4].to_string(), "10:00:00:00");
    }

    #[test]
    fn measures_the_frame_rate() {
        for fps in [24.0, 25.0, 30.0] {
            let (_, got) = roundtrip(48000.0, fps, tc(0, 0, 0, 0), 12);
            let last = got.last().expect("no frames decoded");
            assert!(
                (last.measured_fps - fps).abs() < 0.2,
                "{fps} fps measured as {}",
                last.measured_fps
            );
            assert_eq!(last.nominal_fps(), fps);
        }
    }

    #[test]
    fn frames_land_one_frame_period_apart() {
        let (_, got) = roundtrip(48000.0, 30.0, tc(0, 0, 0, 0), 12);
        let period = 48000.0 / 30.0;
        for pair in got.windows(2) {
            let gap = (pair[1].end_sample - pair[0].end_sample) as f64;
            assert!((gap - period).abs() < 2.0, "frame gap was {gap} samples");
        }
    }

    #[test]
    fn carries_drop_frame_and_user_bits() {
        let mut start = tc(2, 0, 0, 0);
        start.drop_frame = true;
        start.user_bits = [1, 2, 3, 4, 0xa, 0xb, 0xc, 0xd];
        let (_, got) = roundtrip(48000.0, 30.0, start, 4);
        let frame = got[0].frame;
        assert!(frame.drop_frame);
        assert_eq!(frame.user_bits_hex(), "1234abcd");
        assert_eq!(frame.to_string(), "02:00:00;01");
        assert_eq!(got[0].nominal_fps(), 29.97);
    }

    #[test]
    fn survives_an_inverted_signal() {
        // Biphase mark carries no absolute polarity, so a swapped tip/ring or
        // an inverting preamp must not matter.
        let mut encoder = Encoder::new(48000.0, 30.0, 0.5);
        let mut current = tc(4, 5, 6, 7);
        for _ in 0..6 {
            encoder.push(&current);
            advance(&mut current, 30);
        }
        encoder.finish();
        let inverted: Vec<f32> = encoder.out.iter().map(|s| -s).collect();

        let mut decoder = LtcDecoder::new(48000.0);
        let mut got = Vec::new();
        decoder.process(&inverted, &mut got);
        assert_eq!(got[0].frame.to_string(), "04:05:06:08");
    }

    #[test]
    fn recovers_after_a_dropout() {
        let mut encoder = Encoder::new(48000.0, 30.0, 0.5);
        let mut current = tc(0, 10, 0, 0);
        for _ in 0..4 {
            encoder.push(&current);
            advance(&mut current, 30);
        }
        encoder.finish();
        let mut audio = encoder.out.clone();
        audio.extend(std::iter::repeat_n(0.0, 24000)); // half a second of nothing

        let mut encoder = Encoder::new(48000.0, 30.0, 0.5);
        let mut current = tc(0, 20, 0, 0);
        for _ in 0..4 {
            encoder.push(&current);
            advance(&mut current, 30);
        }
        encoder.finish();
        audio.extend(encoder.out);

        let mut decoder = LtcDecoder::new(48000.0);
        let mut got = Vec::new();
        decoder.process(&audio, &mut got);
        let decoded: Vec<String> = got.iter().map(|d| d.frame.to_string()).collect();
        assert_eq!(
            decoded,
            ["00:10:00:01", "00:10:00:02", "00:10:00:03", "00:20:00:01", "00:20:00:02", "00:20:00:03"]
        );
    }

    #[test]
    fn ignores_audio_that_is_not_ltc() {
        let mut decoder = LtcDecoder::new(48000.0);
        let mut got = Vec::new();

        let silence = vec![0.0f32; 48000];
        decoder.process(&silence, &mut got);

        let tone: Vec<f32> = (0..48000)
            .map(|n| (n as f32 * 1000.0 * std::f32::consts::TAU / 48000.0).sin() * 0.5)
            .collect();
        decoder.process(&tone, &mut got);

        let mut seed = 0x1234_5678u32;
        let noise: Vec<f32> = (0..48000)
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (seed >> 8) as f32 / 8388608.0 - 1.0
            })
            .collect();
        decoder.process(&noise, &mut got);

        assert!(got.is_empty(), "decoded {} phantom frames", got.len());
    }

    #[test]
    fn splitting_the_audio_into_chunks_changes_nothing() {
        let mut encoder = Encoder::new(48000.0, 30.0, 0.5);
        let mut current = tc(7, 7, 7, 7);
        for _ in 0..8 {
            encoder.push(&current);
            advance(&mut current, 30);
        }
        encoder.finish();

        let mut whole = LtcDecoder::new(48000.0);
        let mut from_whole = Vec::new();
        whole.process(&encoder.out, &mut from_whole);

        let mut chunked = LtcDecoder::new(48000.0);
        let mut from_chunks = Vec::new();
        for chunk in encoder.out.chunks(37) {
            chunked.process(chunk, &mut from_chunks);
        }

        let a: Vec<LtcFrame> = from_whole.iter().map(|d| d.frame).collect();
        let b: Vec<LtcFrame> = from_chunks.iter().map(|d| d.frame).collect();
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }
}

//! Working out why no timecode is arriving.
//!
//! Three failures look identical from outside — nothing in range, something in
//! range whose payload no longer decodes, and a scan delivering no events at
//! all — and only one of them is the user's fault. The middle one is not
//! hypothetical: connecting two boxes to the vendor's phone app moved a byte in
//! the advertisement, every payload was rejected, and the scanner went silently
//! blind. It looked exactly like an empty room.
//!
//! So this counts what the scan is taking in and classifies it. The counting is
//! knowledge; the sentence is not. [`Diagnosis`] deliberately carries no prose —
//! the phrasing that suits a terminal names command-line flags a GUI doesn't
//! have, so the caller writes its own. What's here is which of the failures it
//! is, and the numbers to say it with.

use crate::ble::scan::Device;
use crate::ble::HEADER;

/// One device's contribution to a [`Census`].
///
/// Exists so [`survey`] can be tested: a `PeripheralId` can only be minted by
/// the platform, and none of the counting cares which device is which beyond
/// having a name to blame.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts<'a> {
    pub name: Option<&'a str>,
    /// Payloads received under the Tentacle's service UUID.
    pub payloads: u64,
    /// ...of those, how many didn't decode.
    pub unparsed: u64,
    /// The last one that didn't.
    pub unparsed_sample: Option<&'a [u8]>,
}

impl Device {
    /// This device's counters, for [`census`].
    pub fn counts(&self) -> Counts<'_> {
        Counts {
            name: self.name(),
            payloads: self.payloads(),
            unparsed: self.unparsed(),
            unparsed_sample: self.unparsed_sample(),
        }
    }
}

/// What a scan has taken in, gathered across every device.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct Census {
    /// Peripherals the adapter reported anything about.
    pub devices: usize,
    /// ...of those, the ones that got past the name filter.
    pub matched: usize,
    /// ...of those, the ones that sent service data under the Tentacle's UUID.
    pub advertisers: usize,
    /// Payloads received under it, in total.
    pub payloads: u64,
    /// ...of those, the ones [`parse`](crate::ble::parse) turned down.
    pub unparsed: u64,
    /// The last payload that didn't parse, and who sent it.
    pub sample: Option<(String, Vec<u8>)>,
}

/// What to call a device that never answered a properties lookup.
const UNNAMED: &str = "<unnamed>";

/// Takes a census across every device a scan has seen.
pub fn census<'a>(devices: impl IntoIterator<Item = &'a Device>) -> Census {
    let devices: Vec<&Device> = devices.into_iter().collect();
    // A device's advert count only moves once it's past the name filter, so
    // it's what separates "in range" from "in range and being looked at".
    let matched: Vec<Counts<'_>> = devices
        .iter()
        .filter(|d| d.adverts() > 0)
        .map(|d| d.counts())
        .collect();
    survey(devices.len(), &matched)
}

/// The counting half, split from [`census`] so it can be tested without a
/// platform to mint device ids.
pub fn survey(devices: usize, matched: &[Counts<'_>]) -> Census {
    Census {
        devices,
        matched: matched.len(),
        advertisers: matched.iter().filter(|c| c.payloads > 0).count(),
        payloads: matched.iter().map(|c| c.payloads).sum(),
        unparsed: matched.iter().map(|c| c.unparsed).sum(),
        sample: matched
            .iter()
            // Most failures first, name breaking a tie: devices come back in a
            // different order every time, and a diagnostic that blames a
            // different device each time is one nobody believes. A device with
            // failures always outranks one without, and only a device with
            // failures has a sample to offer.
            .max_by_key(|c| (c.unparsed, c.name))
            .and_then(|c| {
                Some((
                    c.name.unwrap_or(UNNAMED).to_string(),
                    c.unparsed_sample?.to_vec(),
                ))
            }),
    }
}

/// Which failure a scan with nothing to show is in.
///
/// Ordered most specific first by [`diagnose`]. Every case names something the
/// reader can act on, because the alternative — which is what this replaced —
/// is a blank screen that means all of them at once.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Diagnosis {
    /// Not one advertisement of any kind has arrived. Nothing is reaching the
    /// process at all, so the scan is the suspect, not the Tentacles.
    SilentScan,

    /// Devices are in range, but none got past the name filter.
    FilteredOut { devices: usize },

    /// Devices are in range and none of them has advertised anything. Only
    /// reachable with no filter set.
    NothingAdvertising { devices: usize },

    /// A Tentacle is right here and its payloads don't decode — the wire format
    /// has moved, or something else is advertising under the same UUID.
    Unreadable {
        /// The device with the most failures, and the last payload it sent.
        name: String,
        payload: Vec<u8>,
        advertisers: usize,
        payloads: u64,
        unparsed: u64,
    },

    /// Payloads are arriving and decoding, but none has carried timecode —
    /// dates only so far.
    DatesOnly { advertisers: usize, payloads: u64 },

    /// Devices are in range, none advertising the Tentacle's service. No
    /// Tentacle here.
    NoTentacle { matched: usize },
}

impl Diagnosis {
    /// A key for which failure this is, for a caller that logs rather than
    /// redraws.
    ///
    /// The numbers in a diagnosis move on their own — the device count climbs
    /// as the adapter notices more of the room, and an unreadable payload's
    /// timecode bytes change with every advertisement. A terminal rewrites a
    /// line in place and can afford to stay current; anything appending wants
    /// one line per distinct failure, and this is what tells them apart. See
    /// [`shape_of`] for what counts as a different unreadable payload.
    pub fn key(&self) -> String {
        match self {
            Diagnosis::SilentScan => "silent-scan".to_string(),
            Diagnosis::FilteredOut { .. } => "filtered-out".to_string(),
            Diagnosis::NothingAdvertising { .. } => "nothing-advertising".to_string(),
            Diagnosis::Unreadable { payload, .. } => format!("unparsed:{}", shape_of(payload)),
            Diagnosis::DatesOnly { .. } => "dates-only".to_string(),
            Diagnosis::NoTentacle { .. } => "no-tentacle".to_string(),
        }
    }
}

/// Reads a census, most specific case first.
///
/// `filtered` says whether a name filter is in force, which is the only thing
/// that separates "none of them matched" from "none of them said anything".
pub fn diagnose(census: &Census, filtered: bool) -> Diagnosis {
    let Census {
        devices,
        matched,
        advertisers,
        payloads,
        unparsed,
        sample,
    } = census;

    if *devices == 0 {
        return Diagnosis::SilentScan;
    }
    if *matched == 0 {
        // Nothing can fail a filter that isn't there, so the unfiltered arm is
        // a shape a running scan can't produce. Answer something true anyway
        // rather than assert a filter that was never set.
        return match filtered {
            true => Diagnosis::FilteredOut { devices: *devices },
            false => Diagnosis::NothingAdvertising { devices: *devices },
        };
    }
    if let Some((name, payload)) = sample {
        return Diagnosis::Unreadable {
            name: name.clone(),
            payload: payload.clone(),
            advertisers: *advertisers,
            payloads: *payloads,
            unparsed: *unparsed,
        };
    }
    if *advertisers > 0 {
        return Diagnosis::DatesOnly {
            advertisers: *advertisers,
            payloads: *payloads,
        };
    }
    Diagnosis::NoTentacle { matched: *matched }
}

/// What makes one unreadable payload structurally different from another: the
/// flags byte and the length.
///
/// Deliberately not the whole payload, and deliberately not the record type
/// either. A timecode record's data bytes change with every advertisement, so
/// keying on those would put a line in a log two or three times a second; and a
/// device alternates between timecode and date records in perfectly normal
/// operation, so keying on the record type makes a single format failure
/// alternate between two lines forever. What actually moved when this broke was
/// the header — byte 1 — with the size staying put, and that is what a second
/// line is worth reporting for.
pub fn shape_of(payload: &[u8]) -> String {
    let flags = payload.get(HEADER - 1).copied().unwrap_or_default();
    format!("{flags:02x}/{}", payload.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts<'a>(
        name: &'a str,
        payloads: u64,
        unparsed: u64,
        sample: Option<&'a [u8]>,
    ) -> Counts<'a> {
        Counts {
            name: Some(name),
            payloads,
            unparsed,
            unparsed_sample: sample,
        }
    }

    #[test]
    fn a_scan_delivering_nothing_is_its_own_case() {
        assert_eq!(diagnose(&survey(0, &[]), false), Diagnosis::SilentScan);
    }

    #[test]
    fn a_filter_is_what_separates_the_two_empty_cases() {
        let census = survey(40, &[]);
        assert_eq!(
            diagnose(&census, true),
            Diagnosis::FilteredOut { devices: 40 }
        );
        assert_eq!(
            diagnose(&census, false),
            Diagnosis::NothingAdvertising { devices: 40 }
        );
    }

    #[test]
    fn a_room_with_no_tentacle_in_it_says_that() {
        let phone = counts("iPhone", 0, 0, None);
        let watch = counts("Watch", 0, 0, None);
        assert_eq!(
            diagnose(&survey(2, &[phone, watch]), false),
            Diagnosis::NoTentacle { matched: 2 }
        );
    }

    #[test]
    fn dates_arriving_without_timecode_is_its_own_case() {
        let ricki = counts("Ricki", 8, 0, None);
        assert_eq!(
            diagnose(&survey(3, &[ricki]), false),
            Diagnosis::DatesOnly {
                advertisers: 1,
                payloads: 8,
            }
        );
    }

    #[test]
    fn a_payload_that_stopped_decoding_is_reported_with_its_bytes() {
        let bytes: &[u8] = &[0x22, 0x7d, 0x19, 0x0b, 0x25, 0x28, 0x15, 0x5f, 0xc6];
        let ricki = counts("Ricki", 40, 40, Some(bytes));
        let liliana = counts("Liliana", 32, 32, Some(bytes));
        let Diagnosis::Unreadable {
            payloads, unparsed, ..
        } = diagnose(&survey(72, &[ricki, liliana]), false)
        else {
            panic!("expected an unreadable payload");
        };
        assert_eq!((payloads, unparsed), (72, 72));
    }

    #[test]
    fn the_same_device_is_blamed_however_they_are_handed_over() {
        // Devices come back in a different order every time, and a diagnostic
        // that names a different one each time is one nobody believes.
        let bytes: &[u8] = &[0x22, 0x7d, 0x19];
        let quiet = counts("Ricki", 4, 1, Some(bytes));
        let loud = counts("Liliana", 40, 12, Some(bytes));
        let forwards = survey(2, &[quiet, loud]);
        let backwards = survey(2, &[loud, quiet]);
        assert_eq!(forwards.sample, backwards.sample);
        assert_eq!(forwards.sample.unwrap().0, "Liliana");
    }

    #[test]
    fn a_device_with_no_failures_is_never_blamed_for_them() {
        let bytes: &[u8] = &[0x22, 0x7d, 0x19];
        let healthy = counts("Ricki", 90, 0, None);
        let broken = counts("Liliana", 3, 3, Some(bytes));
        let census = survey(2, &[healthy, broken]);
        assert_eq!(census.sample.unwrap().0, "Liliana");
    }

    #[test]
    fn devices_below_the_name_filter_are_counted_but_not_surveyed() {
        let census = survey(9, &[]);
        assert_eq!(census.devices, 9);
        assert_eq!(census.matched, 0);
    }

    #[test]
    fn a_ticking_timecode_is_not_a_new_failure_every_packet() {
        let first = [0x22u8, 0x7d, 0x19, 0x0b, 0x25, 0x28, 0x15, 0x5f, 0xc6];

        // The data bytes move constantly; that's the timecode running, not a
        // new fault.
        let mut later = first;
        later[6] = later[6].wrapping_add(1);
        assert_eq!(shape_of(&first), shape_of(&later));

        // A device alternating between record types is normal operation.
        let mut date = first;
        date[0] = 0x42;
        assert_eq!(shape_of(&first), shape_of(&date));

        // A moved flags byte, or a different size, is the thing worth a second
        // line — it's what actually broke last time.
        let mut reflagged = first;
        reflagged[1] = 0x05;
        assert_ne!(shape_of(&first), shape_of(&reflagged));
        assert_ne!(shape_of(&first), shape_of(&first[..8]));
    }

    #[test]
    fn an_unnamed_device_still_gets_blamed_by_name() {
        let bytes: &[u8] = &[0x22, 0x7d, 0x19];
        let anonymous = Counts {
            name: None,
            payloads: 3,
            unparsed: 3,
            unparsed_sample: Some(bytes),
        };
        let census = survey(1, &[anonymous]);
        assert_eq!(census.sample.unwrap().0, UNNAMED);
    }
}

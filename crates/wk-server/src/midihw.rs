//! Hardware MIDI on the host.
//!
//! A `MidiIn` node opens a physical MIDI input port (a USB keyboard, etc.) and
//! feeds its messages into the same MIDI [`Router`](crate::midi::Router) a
//! guest's `output` uses, so a hardware device wires to app nodes on the canvas
//! exactly like the piano plugin does. A `MidiOut` node is the mirror image: it
//! is a destination on the canvas, and everything wired into it is played out
//! of a physical MIDI port, so wk can drive an external synth or drum machine.
//!
//! Two details separate this from "copy the bytes across":
//!
//! * The bytes from a port are a *stream*, not a message. One packet can hold
//!   several messages, one message can straddle two packets, and devices omit
//!   repeated status bytes. Each open port therefore owns a
//!   [`Decoder`](crate::midi::stream::Decoder) that reassembles whole messages.
//! * CoreMIDI stamps every packet, and takes a stamp on every packet sent, in
//!   mach host-time units. [`hosttime`] relates that to wk's shared MIDI clock
//!   so an instant survives the round trip: a note scheduled by the sequencer
//!   is handed to the driver with the time it should sound, and the driver —
//!   not our polling loop — decides the exact moment it leaves the port.
//!
//! Bound to CoreMIDI on macOS; other platforms stub out (the cross-platform
//! `midir` crate's ALSA version clashes with cpal's at resolve time).

use wk_protocol::NodeId;

use crate::midi::{Router, SharedInbox};

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use crate::midi::stream::Decoder;
    use crate::midi::{now, Event};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// Conversion between CoreMIDI's mach host-time stamps and wk's shared MIDI
    /// clock (microseconds since an arbitrary origin).
    ///
    /// The two clocks tick at the same rate and differ only in origin and unit,
    /// so a single paired reading converts either way. Both readings are taken
    /// microseconds apart, which is far below the precision that matters here.
    pub(super) mod hosttime {
        use std::sync::OnceLock;

        /// The kernel's mach timing calls. Declared here rather than taken
        /// from `libc`, whose bindings for them are deprecated, and rather than
        /// adding a crate for two symbols.
        #[repr(C)]
        struct Timebase {
            numer: u32,
            denom: u32,
        }
        unsafe extern "C" {
            fn mach_timebase_info(info: *mut Timebase) -> i32;
            fn mach_absolute_time() -> u64;
        }

        /// Nanoseconds per mach tick, as the rational `numer / denom` the
        /// kernel reports. On Apple silicon this is not 1/1, so the
        /// multiplication genuinely matters.
        fn timebase() -> (u64, u64) {
            static TB: OnceLock<(u64, u64)> = OnceLock::new();
            *TB.get_or_init(|| {
                let mut info = Timebase { numer: 0, denom: 0 };
                let ok = unsafe { mach_timebase_info(&mut info) } == 0;
                if ok && info.numer != 0 && info.denom != 0 {
                    (info.numer as u64, info.denom as u64)
                } else {
                    (1, 1)
                }
            })
        }

        fn ticks_to_nanos(ticks: u64) -> u64 {
            let (numer, denom) = timebase();
            (ticks as u128 * numer as u128 / denom as u128) as u64
        }

        fn nanos_to_ticks(nanos: u64) -> u64 {
            let (numer, denom) = timebase();
            (nanos as u128 * denom as u128 / numer as u128) as u64
        }

        fn mach_now() -> u64 {
            unsafe { mach_absolute_time() }
        }

        /// The wk instant a packet stamped `stamp` belongs to.
        ///
        /// A stamp of `0` means "now" in CoreMIDI, and a driver may also stamp
        /// slightly in the past; both collapse to the current instant.
        pub fn to_wk(stamp: u64) -> u64 {
            let (wk, mach) = (super::now(), mach_now());
            if stamp == 0 || stamp <= mach {
                wk
            } else {
                wk + ticks_to_nanos(stamp - mach) / 1_000
            }
        }

        /// The CoreMIDI stamp for wk instant `time`.
        ///
        /// Returns `0` — CoreMIDI's "send immediately" — for an instant that is
        /// zero (the "as soon as possible" spelling) or already past, so a late
        /// event goes out at once rather than being pushed further away.
        pub fn from_wk(time: u64) -> u64 {
            let (wk, mach) = (super::now(), mach_now());
            if time <= wk {
                0
            } else {
                mach + nanos_to_ticks((time - wk) * 1_000)
            }
        }
    }

    /// The names of the available MIDI input sources (a USB keyboard's name).
    pub fn input_devices() -> Vec<String> {
        coremidi::Sources
            .into_iter()
            .filter_map(|s| s.display_name())
            .collect()
    }

    /// The names of the available MIDI output destinations (an external synth).
    pub fn output_devices() -> Vec<String> {
        coremidi::Destinations
            .into_iter()
            .filter_map(|d| d.display_name())
            .collect()
    }

    /// An open hardware MIDI input, feeding the router as its node. Holds the
    /// CoreMIDI client + port alive — dropping it closes the connection.
    pub struct MidiDevice {
        _client: coremidi::Client,
        _port: coremidi::InputPort,
        /// The resolved source name (what the node persists so it reconnects).
        pub name: String,
    }

    /// Pick the endpoint whose name contains `want` (case-insensitive), or the
    /// first one when `want` is empty.
    fn pick<T>(
        mut all: impl Iterator<Item = T>,
        want: &str,
        name_of: impl Fn(&T) -> String,
    ) -> Option<T> {
        let wanted = want.trim().to_lowercase();
        all.find(|e| wanted.is_empty() || name_of(e).to_lowercase().contains(&wanted))
    }

    /// Open a MIDI input source and route its messages into `router` tagged as
    /// `node`. When `want` is empty the first source is used; otherwise the
    /// first whose name contains `want` (case-insensitive substring).
    pub fn open(node: NodeId, want: &str, router: Router) -> Result<MidiDevice, String> {
        let source = pick(coremidi::Sources.into_iter(), want, |s| {
            s.display_name().unwrap_or_default()
        })
        .ok_or_else(|| missing("input", want))?;
        let name = source.display_name().unwrap_or_default();

        let client = coremidi::Client::new("wk").map_err(|e| format!("MIDI client: {e}"))?;
        // One decoder per port, living across callbacks: running status and a
        // message split over two packets both depend on what came before.
        let decoder = Mutex::new(Decoder::new());
        let port = client
            .input_port("wk-midi-in", move |packets: &coremidi::PacketList| {
                let mut decoder = decoder.lock().unwrap();
                let mut events = Vec::new();
                for packet in packets.iter() {
                    let time = hosttime::to_wk(packet.timestamp());
                    for data in decoder.feed(packet.data()) {
                        events.push(Event { data, time });
                    }
                }
                if events.is_empty() {
                    return;
                }
                // Take the router lock once for the whole packet list, not once
                // per message, so a dense chord doesn't thrash it.
                let mut routes = router.lock().unwrap();
                for event in &events {
                    routes.send_from(node, event);
                }
            })
            .map_err(|e| format!("MIDI input port: {e}"))?;
        port.connect_source(&source)
            .map_err(|e| format!("connect MIDI source {name:?}: {e}"))?;

        Ok(MidiDevice {
            _client: client,
            _port: port,
            name,
        })
    }

    /// An open hardware MIDI output. Everything wired into the node on the
    /// canvas lands in `inbox`; a small thread drains it and plays it out of the
    /// port, handing each message's instant to the driver.
    pub struct MidiOutDevice {
        /// The router destination for this node. The server connects wires to
        /// this the same way it connects them to a plugin's input port.
        pub inbox: SharedInbox,
        /// The resolved destination name (what the node persists).
        pub name: String,
        stop: Arc<AtomicBool>,
    }

    impl Drop for MidiOutDevice {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    /// How often the pump looks for newly queued messages. Delivery precision
    /// does not depend on this: a message stamped for the future is handed to
    /// CoreMIDI with that stamp and the driver releases it on time. The interval
    /// only bounds how long an *unstamped* message waits.
    const PUMP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1);

    /// Open a MIDI output destination for `node`. When `want` is empty the first
    /// destination is used; otherwise the first whose name contains `want`.
    pub fn open_output(want: &str) -> Result<MidiOutDevice, String> {
        let destination = pick(coremidi::Destinations.into_iter(), want, |d| {
            d.display_name().unwrap_or_default()
        })
        .ok_or_else(|| missing("output", want))?;
        let name = destination.display_name().unwrap_or_default();

        let client = coremidi::Client::new("wk").map_err(|e| format!("MIDI client: {e}"))?;
        let port = client
            .output_port("wk-midi-out")
            .map_err(|e| format!("MIDI output port: {e}"))?;

        let inbox = crate::midi::new_inbox();
        let stop = Arc::new(AtomicBool::new(false));
        let (queue, halt) = (inbox.clone(), stop.clone());
        std::thread::Builder::new()
            .name(format!("wk-midi-out {name}"))
            .spawn(move || {
                // The client and port must outlive the sends, so the thread owns
                // them; dropping them here closes the connection.
                let (_client, port, destination) = (client, port, destination);
                while !halt.load(Ordering::Relaxed) {
                    let events = queue.lock().unwrap().drain();
                    for event in events {
                        let packet =
                            coremidi::PacketBuffer::new(hosttime::from_wk(event.time), &event.data);
                        let _ = port.send(&destination, &packet);
                    }
                    std::thread::sleep(PUMP_INTERVAL);
                }
            })
            .map_err(|e| format!("MIDI output pump: {e}"))?;

        Ok(MidiOutDevice { inbox, name, stop })
    }

    fn missing(kind: &str, want: &str) -> String {
        if want.trim().is_empty() {
            format!("no MIDI {kind} devices are connected")
        } else {
            format!("no MIDI {kind} device matching {want:?}")
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;

    pub fn input_devices() -> Vec<String> {
        Vec::new()
    }

    pub fn output_devices() -> Vec<String> {
        Vec::new()
    }

    pub struct MidiDevice {
        pub name: String,
    }

    pub struct MidiOutDevice {
        pub inbox: SharedInbox,
        pub name: String,
    }

    pub fn open(_node: NodeId, _want: &str, _router: Router) -> Result<MidiDevice, String> {
        Err(unsupported())
    }

    pub fn open_output(_want: &str) -> Result<MidiOutDevice, String> {
        Err(unsupported())
    }

    fn unsupported() -> String {
        "hardware MIDI is only available on macOS in this build".to_string()
    }
}

pub use imp::{input_devices, open, open_output, output_devices, MidiDevice, MidiOutDevice};

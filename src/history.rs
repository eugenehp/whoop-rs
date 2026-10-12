//! The historical-offload state machine, and the rule that makes it safe.
//!
//! # The invariant
//!
//! A strap keeps its records in a ring buffer and hands them over in chunks. At
//! the end of each chunk it sends an eight-byte token and waits to be told the
//! chunk arrived. **A successful acknowledgement can let the strap reclaim that
//! history.** So the acknowledgement must not be sent until the chunk is
//! durably stored on this side, and if storing it fails the acknowledgement
//! must be withheld — the strap will resend.
//!
//! That ordering is the whole point of this module, and it is enforced by the
//! types rather than by a comment:
//!
//! ```text
//! HISTORY_START ──▶ records accumulate
//!   │
//!   ├─ HISTORY_END ──▶ Step::Chunk(PendingChunk)
//!   │     caller persists the chunk's records and its cursor
//!   │     then, and only then, calls PendingChunk::committed()
//!   │     which yields the AckPayload to write back
//!   │
//!   └─ HISTORY_COMPLETE ──▶ Step::Complete
//! ```
//!
//! [`PendingChunk`] holds the token and will not give it up except through
//! [`PendingChunk::committed`], which consumes the chunk. There is no method
//! that returns the acknowledgement bytes alongside the records, so "ack, then
//! store" is not a sequence a caller can write by accident.
//!
//! # What this does not promise
//!
//! Nothing here is exactly-once delivery. The strap resends an unanswered end
//! marker up to four times and ends the attempt on the fifth; a fresh attempt
//! starts again from the acknowledged boundary, so previously received but
//! unacknowledged records recur. Tolerate duplicates, keep arrival order, and
//! treat `HISTORY_COMPLETE` as "this attempt is over" rather than as "every
//! record reached storage" — it can be emitted while a backlog remains.

use std::time::Duration;

use crate::protocol::{is_offload_frame, payload, DeviceFamily, MetadataType};
use crate::types::{HistoryMeta, Record, WhoopEvent};

/// How long a session tolerates silence before giving up.
///
/// Re-armed only by offload traffic — see [`is_offload_frame`]. A live type-43
/// raw flood must not keep a stalled offload looking busy, which is exactly
/// what a watchdog re-armed by "any frame" would do.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The acknowledgement for one committed chunk.
///
/// Only obtainable from [`PendingChunk::committed`], so holding one is evidence
/// that the chunk it belongs to was stored first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckPayload {
    body: Vec<u8>,
    trim_cursor: Option<u32>,
}

impl AckPayload {
    /// The `HISTORICAL_DATA_RESULT` (23) request body: `[0x01] + the verbatim
    /// eight-byte end block`.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// The trim cursor this acknowledgement advances, for a progress log.
    pub fn trim_cursor(&self) -> Option<u32> {
        self.trim_cursor
    }
}

/// One chunk of offloaded history, closed by an end marker and not yet
/// acknowledged.
///
/// Persist [`Self::records`] and [`Self::trim_cursor`] durably, then call
/// [`Self::committed`] for the bytes to write back. Drop this instead if the
/// store failed: the acknowledgement is withheld, the strap keeps the data and
/// resends it.
#[derive(Debug, Clone)]
pub struct PendingChunk {
    end_block: Option<[u8; 8]>,
    /// The chunk's clock reading from its end marker.
    pub unix_seconds: Option<u32>,
    /// The trim cursor — the first word of the end block. Persist it as the
    /// local progress marker, but **never** rebuild the token from it.
    pub trim_cursor: Option<u32>,
    /// Decoded records in arrival order, including preserved unknown layouts.
    ///
    /// Duplicate handling and cursor association both depend on this order, so
    /// it is never sorted on the way through.
    ///
    /// A record this crate could not decode carries its complete frame in
    /// [`Record::Unknown`], so the bytes that could explain a decode failure
    /// are inside the same durability boundary as the decoded rows — there is
    /// no second list of frames to keep in step with this one.
    pub records: Vec<Record>,
}

impl PendingChunk {
    /// Confirm the chunk is durably stored and take the acknowledgement.
    ///
    /// `None` when the end marker arrived without a complete eight-byte block.
    /// There is nothing to echo in that case, and a synthesised token is not the
    /// same token — so the chunk stays unacknowledged and the strap resends.
    pub fn committed(self) -> Option<AckPayload> {
        let block = self.end_block?;
        Some(AckPayload {
            body: payload::history_ack(&block),
            trim_cursor: self.trim_cursor,
        })
    }

    /// How many records this chunk carries.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the chunk is empty.
    ///
    /// An end marker with no records still has to be acknowledged — that is how
    /// the offload advances — so an empty chunk is not a no-op.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// What the caller should do next.
#[derive(Debug)]
pub enum Step {
    /// Nothing to do; keep reading events.
    Idle,
    /// The strap opened the stream.
    ///
    /// Not proof of a restored position: a start marker can appear when the
    /// strap's attempt to resume from the acknowledged boundary did not succeed.
    Started,
    /// A chunk is closed and waiting to be stored. Persist it, then
    /// [`PendingChunk::committed`].
    Chunk(PendingChunk),
    /// The attempt is over.
    ///
    /// Reconcile against persisted progress rather than treating this as
    /// completion: it can be emitted while a backlog remains, and it does not
    /// retroactively commit an open chunk.
    Complete,
}

/// Accumulates records between end markers and hands out chunks to commit.
///
/// Created by [`Self::new`] after the offload request is accepted. Feed it every
/// event; it ignores the ones that are not history.
#[derive(Debug)]
pub struct HistorySession {
    family: DeviceFamily,
    records: Vec<Record>,
    chunk_open: bool,
    started: bool,
    finished: bool,
    chunks_closed: u64,
    records_seen: u64,
    unknown_records: u64,
}

impl HistorySession {
    /// Start a session for `family`.
    ///
    /// High-frequency offload sends **one** start marker and then repeated end
    /// markers — roughly a chunk close every 50 records — so the chunk begins
    /// open: waiting for a second start marker that never comes would drop
    /// every record of the session.
    pub fn new(family: DeviceFamily) -> Self {
        Self {
            family,
            records: Vec::new(),
            chunk_open: true,
            started: false,
            finished: false,
            chunks_closed: 0,
            records_seen: 0,
            unknown_records: 0,
        }
    }

    /// Which family this session is decoding.
    pub fn family(&self) -> DeviceFamily {
        self.family
    }

    /// Whether the strap has signalled the end of the attempt.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Records accumulated in the currently open chunk.
    pub fn buffered(&self) -> usize {
        self.records.len()
    }

    /// Chunks closed so far this session.
    pub fn chunks_closed(&self) -> u64 {
        self.chunks_closed
    }

    /// Records seen this session, across all chunks.
    pub fn records_seen(&self) -> u64 {
        self.records_seen
    }

    /// Records whose layout this crate did not decode, preserved whole.
    ///
    /// Worth reporting at the end of a session: a run where this is non-zero is
    /// a run that found something the decoders do not cover, which is a
    /// different outcome from a clean one and should not look the same.
    pub fn unknown_records(&self) -> u64 {
        self.unknown_records
    }

    /// Whether a frame of this packet type should re-arm the idle watchdog.
    pub fn re_arms_watchdog(packet_type: u8) -> bool {
        is_offload_frame(packet_type)
    }

    /// Whether this event is offload progress, for the same watchdog.
    ///
    /// The event-level counterpart of [`Self::re_arms_watchdog`], and the one a
    /// caller driving the session actually has in hand. It matters that this is
    /// narrower than "any event arrived": the standard Heart Rate Service
    /// notifies about once a second whether or not history is moving, so a
    /// watchdog re-armed by every event would never fire on a strap that had
    /// gone quiet mid-transfer — which is the single thing it exists to catch.
    pub fn is_progress(event: &WhoopEvent) -> bool {
        matches!(
            event,
            // Packet types 47 and 52, 49 and 56, 48, and 50 respectively — the
            // offload list, expressed in the events those frames become.
            WhoopEvent::Record(_)
                | WhoopEvent::HistoryMeta(_)
                | WhoopEvent::Event(_)
                | WhoopEvent::ConsoleLog(_)
        )
    }

    /// Feed one event from the client's stream.
    pub fn feed(&mut self, event: &WhoopEvent) -> Step {
        match event {
            WhoopEvent::Record(record) => {
                self.records_seen += 1;
                if matches!(record, Record::Unknown { .. }) {
                    self.unknown_records += 1;
                }
                self.records.push(record.clone());
                Step::Idle
            }
            WhoopEvent::HistoryMeta(meta) => self.on_meta(meta),
            // A disconnect ends preparation, streaming or an end-marker wait.
            // The session stops here and resumes next time from the durable
            // cursor, which is why the cursor is persisted per chunk rather than
            // per session.
            WhoopEvent::Disconnected => {
                self.finished = true;
                Step::Complete
            }
            _ => Step::Idle,
        }
    }

    fn on_meta(&mut self, meta: &HistoryMeta) -> Step {
        match meta.kind {
            MetadataType::HistoryStart => {
                self.started = true;
                self.chunk_open = true;
                Step::Started
            }
            MetadataType::HistoryEnd => {
                self.chunks_closed += 1;
                // Snapshot and clear, but leave the chunk open: the records that
                // follow form the next chunk of the same stream.
                let chunk = PendingChunk {
                    end_block: meta.end_block,
                    unix_seconds: meta.unix_seconds,
                    trim_cursor: meta.trim_cursor,
                    records: std::mem::take(&mut self.records),
                };
                Step::Chunk(chunk)
            }
            MetadataType::HistoryComplete => {
                // Closes the session, and does not retroactively commit an open
                // chunk: whatever is buffered here was never closed by an end
                // marker, so there is no token for it and it will arrive again.
                self.finished = true;
                self.chunk_open = false;
                self.records.clear();
                Step::Complete
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::packet;
    use crate::types::{BiometricRecord, HeartRate, HrSource};

    fn biometric(unix: u32) -> WhoopEvent {
        WhoopEvent::Record(Record::Biometric(BiometricRecord {
            family: DeviceFamily::Whoop5,
            version: 18,
            record_index: Some(1),
            unix_seconds: unix,
            heart_rate: HeartRate {
                source: HrSource::Historical,
                bpm: 55,
                rr_ms: vec![1000],
                unix_seconds: Some(unix),
            },
            skin_temp_c: None,
            dynamic_accel_g: None,
            gravity_g: None,
            steps: None,
            activity: None,
            quality_flags: None,
            whoop4_sensors: None,
        }))
    }

    fn meta(kind: MetadataType, block: Option<[u8; 8]>) -> WhoopEvent {
        WhoopEvent::HistoryMeta(HistoryMeta {
            kind,
            unix_seconds: Some(1_700_000_000),
            trim_cursor: block.map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            end_block: block,
        })
    }

    #[test]
    fn a_chunk_yields_its_records_and_then_its_token() {
        let mut session = HistorySession::new(DeviceFamily::Whoop5);
        assert!(matches!(
            session.feed(&meta(MetadataType::HistoryStart, None)),
            Step::Started
        ));
        for i in 0..3 {
            assert!(matches!(
                session.feed(&biometric(1_700_000_000 + i)),
                Step::Idle
            ));
        }

        let block = [0x81, 0xB7, 0x01, 0x00, 0x07, 0x00, 0x00, 0x00];
        let Step::Chunk(chunk) = session.feed(&meta(MetadataType::HistoryEnd, Some(block))) else {
            panic!("expected a chunk");
        };
        assert_eq!(chunk.len(), 3);
        assert_eq!(chunk.trim_cursor, Some(112_513));

        let ack = chunk.committed().unwrap();
        // [0x01] followed by the token exactly as it arrived — both words.
        assert_eq!(
            ack.body(),
            &[0x01, 0x81, 0xB7, 0x01, 0x00, 0x07, 0x00, 0x00, 0x00]
        );
        assert_eq!(ack.trim_cursor(), Some(112_513));
    }

    #[test]
    fn the_stream_stays_open_across_end_markers() {
        // High-frequency offload sends one START and repeated ENDs. If the
        // session closed the chunk on an end marker, every record after the
        // first chunk would be dropped.
        let mut session = HistorySession::new(DeviceFamily::Whoop5);
        session.feed(&meta(MetadataType::HistoryStart, None));
        session.feed(&biometric(1));
        let Step::Chunk(first) = session.feed(&meta(MetadataType::HistoryEnd, Some([1; 8]))) else {
            panic!("expected a chunk");
        };
        assert_eq!(first.len(), 1);

        session.feed(&biometric(2));
        session.feed(&biometric(3));
        let Step::Chunk(second) = session.feed(&meta(MetadataType::HistoryEnd, Some([2; 8])))
        else {
            panic!("expected a second chunk");
        };
        assert_eq!(second.len(), 2);
        assert_eq!(session.chunks_closed(), 2);
        assert_eq!(session.records_seen(), 3);
    }

    #[test]
    fn an_empty_end_marker_is_still_acknowledgeable() {
        // That is how the offload progresses: withholding an ack because a
        // chunk happened to be empty stalls the transfer.
        let mut session = HistorySession::new(DeviceFamily::Whoop5);
        let Step::Chunk(chunk) = session.feed(&meta(MetadataType::HistoryEnd, Some([3; 8]))) else {
            panic!("expected a chunk");
        };
        assert!(chunk.is_empty());
        assert!(chunk.committed().is_some());
    }

    #[test]
    fn a_chunk_without_a_token_cannot_be_acknowledged() {
        let mut session = HistorySession::new(DeviceFamily::Whoop5);
        session.feed(&biometric(1));
        let Step::Chunk(chunk) = session.feed(&meta(MetadataType::HistoryEnd, None)) else {
            panic!("expected a chunk");
        };
        // The records are still worth storing; the acknowledgement is not
        // available, so the strap keeps the data and resends it.
        assert_eq!(chunk.len(), 1);
        assert!(chunk.committed().is_none());
    }

    #[test]
    fn complete_does_not_commit_an_open_chunk() {
        let mut session = HistorySession::new(DeviceFamily::Whoop5);
        session.feed(&meta(MetadataType::HistoryStart, None));
        session.feed(&biometric(1));
        session.feed(&biometric(2));
        // No end marker arrived for these two, so there is no token for them.
        assert!(matches!(
            session.feed(&meta(MetadataType::HistoryComplete, None)),
            Step::Complete
        ));
        assert!(session.is_finished());
        assert_eq!(session.buffered(), 0);
    }

    #[test]
    fn a_disconnect_ends_the_session() {
        let mut session = HistorySession::new(DeviceFamily::Whoop4);
        session.feed(&biometric(1));
        assert!(matches!(
            session.feed(&WhoopEvent::Disconnected),
            Step::Complete
        ));
        assert!(session.is_finished());
    }

    #[test]
    fn unknown_records_are_counted_and_keep_their_bytes() {
        let mut session = HistorySession::new(DeviceFamily::Whoop5);
        let unknown = WhoopEvent::Record(Record::Unknown {
            packet_type: packet::HISTORICAL_DATA,
            version: 99,
            length: 4,
            note: "unknown WHOOP 5 layout selector",
            raw: vec![0xAA, 0x01, 0x02, 0x03],
        });
        session.feed(&unknown);
        session.feed(&biometric(1));
        let Step::Chunk(chunk) = session.feed(&meta(MetadataType::HistoryEnd, Some([4; 8]))) else {
            panic!("expected a chunk");
        };
        assert_eq!(chunk.len(), 2);
        // The bytes travel inside the record itself, so there is no second list
        // that could fall out of step with it — or write them to a capture
        // twice, which a parallel list of frames did.
        let Record::Unknown { raw, .. } = &chunk.records[0] else {
            panic!("the undecoded record should be first");
        };
        assert_eq!(raw, &vec![0xAA, 0x01, 0x02, 0x03]);
        assert_eq!(session.unknown_records(), 1);
    }

    #[test]
    fn only_offload_traffic_re_arms_the_watchdog() {
        assert!(HistorySession::re_arms_watchdog(packet::HISTORICAL_DATA));
        assert!(HistorySession::re_arms_watchdog(packet::METADATA));
        // A live raw flood must not make a stalled offload look busy.
        assert!(!HistorySession::re_arms_watchdog(packet::REALTIME_RAW_DATA));
    }

    #[test]
    fn a_standard_service_heartbeat_is_not_offload_progress() {
        assert!(HistorySession::is_progress(&biometric(1)));
        assert!(HistorySession::is_progress(&meta(
            MetadataType::HistoryEnd,
            Some([1; 8])
        )));
        // The standard Heart Rate Service notifies every second regardless of
        // whether history is moving, so counting it as progress would disarm
        // the watchdog permanently.
        assert!(!HistorySession::is_progress(&WhoopEvent::HeartRate(
            HeartRate {
                source: HrSource::StandardService,
                bpm: 60,
                rr_ms: vec![],
                unix_seconds: None,
            }
        )));
    }
}

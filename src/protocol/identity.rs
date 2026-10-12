//! WHOOP 4 identity: the command-35 serial window and its confirmation gate.

// ── WHOOP 4 identity ─────────────────────────────────────────────────────────

/// Offset of the serial field inside a command-35 hello **body**.
pub const WHOOP4_SERIAL_OFFSET: usize = 14;
/// Meaningful bytes of that field. The tenth byte is the NUL terminator.
pub const WHOOP4_SERIAL_LENGTH: usize = 9;

/// Extract the WHOOP 4 serial from a `GET_HELLO_HARVARD` (35) response body.
///
/// A WHOOP 4 exposes no DIS Serial Number String, so this is the only place its
/// stable serial appears. The field is a fixed nine bytes at body offset 14,
/// followed by a NUL.
///
/// Only that window is read, and only when every byte of it is alphanumeric.
/// The adjacent 54 bytes from offset 24 are key and signature material: a
/// decoder that scanned for "the longest alphanumeric run" could walk straight
/// into them and turn sensitive bytes into an identifier. If another firmware
/// does not expose a plausible serial at this offset, identity stays
/// unresolved rather than being found somewhere else.
pub fn whoop4_serial(body: &[u8]) -> Option<String> {
    let window = body.get(WHOOP4_SERIAL_OFFSET..WHOOP4_SERIAL_OFFSET + WHOOP4_SERIAL_LENGTH)?;
    window
        .iter()
        .all(|b| b.is_ascii_alphanumeric())
        .then(|| String::from_utf8_lossy(window).into_owned())
}

/// Withholds a WHOOP 4 serial until the same value has been seen twice.
///
/// A WHOOP 5 adopts its DIS serial on the first read, because `0x2A25` is a
/// spec-defined field that means one thing. The WHOOP 4 offset is not that: it
/// is a strong inference from a capture. If that nine-byte run turned out to be
/// per-session rather than per-strap, adopting it immediately would mint a new
/// identity on every connect — and anything keyed on it would fragment once per
/// connection, which is strictly worse than the duplicate this prevents.
///
/// Two hellos carrying the same run cannot both be a fresh per-session token,
/// which is the only failure mode that does real damage. The cost is that a
/// WHOOP 4 is identified one connect later than a WHOOP 5.
#[derive(Debug, Clone, Default)]
pub struct RepeatedSerialGate {
    candidate: Option<String>,
    confirmed: Option<String>,
}

impl RepeatedSerialGate {
    /// An empty gate.
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer a sighting. Returns the serial **only** on the transition to
    /// confirmed, so a caller can act on it without re-adopting every time.
    pub fn offer(&mut self, serial: &str) -> Option<String> {
        if self.confirmed.as_deref() == Some(serial) {
            return None; // already acted on
        }
        if self.candidate.as_deref() != Some(serial) {
            self.candidate = Some(serial.to_owned()); // first sighting: withhold
            return None;
        }
        self.confirmed = Some(serial.to_owned());
        self.confirmed.clone()
    }

    /// The confirmed serial, if there is one.
    pub fn confirmed(&self) -> Option<&str> {
        self.confirmed.as_deref()
    }
}

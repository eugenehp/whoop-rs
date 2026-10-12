//! BLE scanning, connecting, notification dispatch, and the command API.
//!
//! One type does discovery ([`WhoopClient`]), one does traffic
//! ([`WhoopHandle`]), and in between sits an `mpsc` channel of
//! [`WhoopEvent`]s. The split matters because the two halves have different
//! failure modes: discovery fails loudly and once, while a stream ends quietly
//! whenever the strap walks out of range.
//!
//! # Request and response
//!
//! Unlike a sensor stream, a WHOOP command is a request that gets an answer, and
//! the answer has to be matched to the question. The correlation field is the
//! **originating request sequence** echoed in the reply — not the reply's own
//! sequence byte, which the strap generates fresh. [`WhoopHandle::request`]
//! registers `(command, sequence)` before it writes, so a reply that arrives
//! during the write still finds its waiter, and a `PENDING` result keeps the
//! request open for the final one within a bounded wait.
//!
//! # What a successful write is not
//!
//! A BLE write acknowledgement, a command result, the completion of
//! asynchronous work and the delivery of a measurement are four separate
//! events. Reconnecting does not restore a previous sensor session, and a
//! disconnect is not an acknowledgement that one was torn down — so collection
//! is re-requested explicitly rather than assumed.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use log::{debug, info, warn};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;
use webbluetooth::chooser::{Candidate, FirstMatch};
use webbluetooth::stream::{select_all, BoxStream, StreamExt};
use webbluetooth::uuid::BluetoothUuid;
use webbluetooth::{
    Bluetooth, BluetoothDevice, DeviceFilter, Grant, LeScanOptions, RemoteGattCharacteristic,
    RemoteGattServer, RequestDeviceOptions,
};

use crate::parse;
use crate::platform;
use crate::protocol::{
    admits_send, command_frame, packet, packet_type, packet_type_name, payload, verify_frame,
    whoop5_variant, AlarmRunDetail, AlarmSetDetail, Command, CommandResult, DeviceFamily,
    Reassembler, RebootProbe, SendPolicy, ServiceFamily, Whoop5Variant, Wrist, BATTERY_LEVEL,
    BATTERY_SERVICE, DEVICE_INFORMATION_SERVICE, HARDWARE_REVISION_STRING, HEART_RATE_MEASUREMENT,
    HEART_RATE_SERVICE, MODEL_NUMBER_STRING, SERIAL_NUMBER_STRING,
};
use crate::types::{
    AdvertisingName, AlarmRecord, BatteryPack, BodyLocationStatus, CommandResponse, ConfigEntry,
    ConfigValue, ConfigWalk, ExtendedBattery, Record, RejectedFrame, WalkStop, WhoopEvent,
};

// ── Grants and filters ───────────────────────────────────────────────────────

// The adapter session is `Bluetooth::shared()`: one per process, cloned by
// every caller. It has to be shared, because a `WhoopDevice` is only usable
// through the session that found it, and the TUI scans with one `WhoopClient`
// and connects with another.

/// Services a connection needs access to.
///
/// A Web Bluetooth grant covers exactly the services it names, and
/// `get_primary_service` refuses every other — so both custom services are
/// named even when the family is already known. The standard three are named
/// because they are the fallback sources: Heart Rate works unbonded, and Device
/// Information is the only way to tell a 5.0 from an MG.
fn granted_services(family: Option<DeviceFamily>) -> Vec<Uuid> {
    let mut services = match family {
        Some(f) => vec![f.service_uuid()],
        None => vec![
            DeviceFamily::Whoop4.service_uuid(),
            DeviceFamily::Whoop5.service_uuid(),
        ],
    };
    services.extend_from_slice(&[
        HEART_RATE_SERVICE,
        BATTERY_SERVICE,
        DEVICE_INFORMATION_SERVICE,
    ]);
    services
}

/// What a device request asks for: a strap advertising a WHOOP custom service,
/// plus access to everything [`granted_services`] lists.
fn request_options(config: &WhoopClientConfig) -> Result<RequestDeviceOptions> {
    let mut options = RequestDeviceOptions::new();
    for filter in scan_filters(config)? {
        options = options.filter(filter);
    }
    for service in granted_services(config.family) {
        options = options.optional_service(service)?;
    }
    Ok(options)
}

/// One filter per connectable service, narrowed by a name prefix when the
/// caller gave one.
///
/// Filtering on the service rather than the name is deliberate: a renamed strap
/// still advertises its service, and `GET_ADVERTISING_NAME` exists precisely
/// because the name is not an identity.
fn scan_filters(config: &WhoopClientConfig) -> Result<Vec<DeviceFilter>> {
    let families: Vec<DeviceFamily> = match config.family {
        Some(f) => vec![f],
        None => vec![DeviceFamily::Whoop4, DeviceFamily::Whoop5],
    };
    families
        .into_iter()
        .map(|f| {
            let mut filter = DeviceFilter::new().service(f.service_uuid())?;
            if let Some(prefix) = &config.name_prefix {
                filter = filter.name_prefix(prefix.clone());
            }
            Ok(filter)
        })
        .collect()
}

/// What a scan reports. A scan grants nothing, so there is no service to name
/// here — only what should be reported.
fn scan_options(config: &WhoopClientConfig) -> Result<LeScanOptions> {
    let mut options = LeScanOptions::new();
    for filter in scan_filters(config)? {
        options = options.filter(filter);
    }
    Ok(options)
}

/// What adopting a scanned strap grants.
fn whoop_grant(family: Option<DeviceFamily>) -> Result<Grant> {
    Ok(Grant::new().services(granted_services(family))?)
}

/// Pick one characteristic out of a service's discovered set.
fn find_char(chars: &[RemoteGattCharacteristic], uuid: Uuid) -> Result<RemoteGattCharacteristic> {
    let want = BluetoothUuid::from(uuid);
    chars
        .iter()
        .find(|c| *c.uuid() == want)
        .cloned()
        .ok_or_else(|| anyhow!("characteristic {uuid} not found"))
}

/// Subscribe to a characteristic and tag each notification with its UUID.
///
/// Web Bluetooth subscribes per characteristic and yields bare values, so the
/// UUID is attached here and [`select_all`] merges the tagged streams back into
/// the single stream the dispatch loop reads. The tag is not cosmetic: each
/// notify channel is its own byte stream with its own reassembly state, and
/// merging them without it would splice frames together.
async fn subscribe_tagged(
    chars: &[RemoteGattCharacteristic],
    uuid: Uuid,
) -> Result<BoxStream<'static, (Uuid, Vec<u8>)>> {
    let characteristic = find_char(chars, uuid)?;
    let notifications = characteristic.start_notifications().await?;
    Ok(notifications.map(move |value| (uuid, value)).boxed())
}

/// Read a Device Information string, tolerating its absence.
///
/// Every one of these is optional on both generations — WHOOP 4 exposes no
/// serial at all — so a missing characteristic is an unresolved field rather
/// than a connection failure.
async fn read_dis_string(chars: &[RemoteGattCharacteristic], uuid: Uuid) -> Option<String> {
    let characteristic = find_char(chars, uuid).ok()?;
    let value = characteristic.read_value().await.ok()?;
    let text = String::from_utf8_lossy(&value)
        .trim_matches('\0')
        .trim()
        .to_owned();
    (!text.is_empty()).then_some(text)
}

// ── Pending requests ─────────────────────────────────────────────────────────

/// Outstanding requests, keyed by `(command, originating sequence)`.
///
/// The key is the pair because neither half identifies a request on its own:
/// several commands can be outstanding at once, and an eight-bit sequence wraps.
/// Bounded by how many requests a caller has in flight, and every entry is
/// removed by either a final result or the waiter being dropped.
#[derive(Default)]
struct Pending {
    waiters: Mutex<HashMap<(u8, u8), oneshot::Sender<CommandResponse>>>,
}

impl Pending {
    fn register(&self, key: (u8, u8)) -> oneshot::Receiver<CommandResponse> {
        let (tx, rx) = oneshot::channel();
        if let Ok(mut waiters) = self.waiters.lock() {
            // A re-registered key replaces the old waiter, whose receiver then
            // resolves as cancelled. That is the honest outcome for a caller
            // that reused a sequence while the first request was still open.
            waiters.insert(key, tx);
        }
        rx
    }

    /// Deliver a reply to its waiter, if the result ends the request.
    ///
    /// A `PENDING` result is deliberately not delivered and not removed: the
    /// request stays open for its final result, which is the whole reason a
    /// bounded client wait exists rather than a first-reply-wins rule.
    fn resolve(&self, response: &CommandResponse) {
        if !response.result.is_final() {
            return;
        }
        let key = (response.command, response.origin_sequence);
        let Ok(mut waiters) = self.waiters.lock() else {
            return;
        };
        if let Some(tx) = waiters.remove(&key) {
            // An error here means the caller stopped waiting. Nothing to do: the
            // event copy still reaches the stream.
            let _ = tx.send(response.clone());
        }
    }

    fn forget(&self, key: (u8, u8)) {
        if let Ok(mut waiters) = self.waiters.lock() {
            waiters.remove(&key);
        }
    }
}

// ── WhoopDevice ──────────────────────────────────────────────────────────────

/// Shorten a platform identifier for display, keeping **both** ends.
///
/// Both ends because a truncated prefix does not distinguish two straps paired
/// to the same machine: on macOS and iOS the identifier is a per-host UUID
/// nobody memorises, and its tail is the part that differs.
pub fn short_id(id: &str) -> String {
    if id.len() <= 13 {
        return id.to_owned();
    }
    format!("{}…{}", &id[..6], &id[id.len() - 6..])
}

/// A WHOOP strap discovered during a scan.
///
/// Returned by [`WhoopClient::scan_all`]; pass it to
/// [`WhoopClient::connect_to`].
#[derive(Clone, Debug)]
pub struct WhoopDevice {
    /// Advertised name, e.g. `WHOOP 4B2C`. Renameable, so not an identity.
    pub name: String,
    /// Platform BLE identifier.
    ///
    /// On macOS and iOS a per-host UUID the system assigns — the same strap has
    /// a different one on a different machine, because Apple never exposes the
    /// hardware address. Elsewhere, the Bluetooth address.
    pub id: String,
    /// Which wire profile the advertised service implies.
    pub family: DeviceFamily,
    pub(crate) device: BluetoothDevice,
}

impl WhoopDevice {
    /// A one-line description: name, generation, shortened identifier.
    pub fn label(&self) -> String {
        format!(
            "{:<22} {:<14} {}",
            self.name,
            self.family.label(),
            short_id(&self.id)
        )
    }
}

// ── Config ───────────────────────────────────────────────────────────────────

/// Configuration for [`WhoopClient`].
#[derive(Debug, Clone)]
pub struct WhoopClientConfig {
    /// Restrict discovery to one generation. `None` scans for both.
    pub family: Option<DeviceFamily>,
    /// Match straps whose advertised name starts with this.
    ///
    /// `None` matches any strap advertising a WHOOP service, which is the right
    /// default: the name is user-settable and carries no identity.
    pub name_prefix: Option<String>,
    /// How long [`WhoopClient::scan_all`] collects advertisements. Default 10.
    pub scan_timeout_secs: u64,
    /// Subscribe to the standard Heart Rate Measurement characteristic.
    ///
    /// Independent of the proprietary stream and available unbonded, so it
    /// costs nothing to leave on. Default `true`.
    pub standard_hr: bool,
    /// Send `TOGGLE_REALTIME_HR` after connecting.
    ///
    /// Asks for the proprietary live packet, which carries the strap's own clock
    /// reading in a way the standard service does not. Default `false`, because
    /// it changes strap state and a reader that only wants history should not.
    pub realtime_hr: bool,
    /// What this connection may send beyond the ordinary read surface.
    ///
    /// Reads only by default. Destructive commands are never admitted whatever
    /// this says — see [`crate::protocol::admits_send`].
    pub policy: SendPolicy,
    /// How long [`WhoopHandle::request`] waits for a final result. Default 10.
    ///
    /// A local wait limit closes local waiting; it does not prove the strap
    /// cancelled anything.
    pub request_timeout_secs: u64,
    /// Event channel depth. Default 1024.
    ///
    /// Deeper than a plain sensor stream needs, because an offload delivers
    /// thousands of records in bursts and a consumer that pauses to write a
    /// file should not stall the dispatch loop.
    pub event_channel_capacity: usize,
}

impl Default for WhoopClientConfig {
    fn default() -> Self {
        Self {
            family: None,
            name_prefix: None,
            scan_timeout_secs: 10,
            standard_hr: true,
            realtime_hr: false,
            policy: SendPolicy::READ_ONLY,
            request_timeout_secs: 10,
            event_channel_capacity: 1024,
        }
    }
}

// ── WhoopClient ──────────────────────────────────────────────────────────────

/// BLE client for WHOOP straps.
///
/// Handles scanning, connecting, GATT subscription and notification dispatch
/// for both generations. The family is resolved from the advertised service and
/// then from the discovered services, never from the device name or a default:
/// writing a WHOOP 4 envelope to a WHOOP 5 is not a degraded connection, it is
/// a strap being sent frames it will reject.
#[derive(Debug)]
pub struct WhoopClient {
    config: WhoopClientConfig,
}

impl WhoopClient {
    /// A client with this configuration.
    pub fn new(config: WhoopClientConfig) -> Self {
        Self { config }
    }

    /// Scan for every nearby WHOOP strap and return them.
    ///
    /// Runs for `scan_timeout_secs` so that several straps in range are all
    /// discovered before returning. Every strap returned has already been
    /// granted access, so [`Self::connect_to`] does not scan again.
    ///
    /// Advertisements from the three unmapped WHOOP service families are
    /// reported in the log and then skipped: their framing is unresolved, and
    /// connecting to one to write a frame built for a different family is how a
    /// scan turns into an unexplained strap state.
    pub async fn scan_all(&self) -> Result<Vec<WhoopDevice>> {
        let bluetooth = Bluetooth::shared();
        // Waits for the adapter's first state report, rather than polling for
        // "powered on" by hand.
        bluetooth
            .availability()
            .await
            .context("Bluetooth is not usable")?;

        info!(
            "scan_all: scanning for {} s …",
            self.config.scan_timeout_secs
        );
        let mut scan = bluetooth
            .request_le_scan(scan_options(&self.config)?)
            .await?;

        // A strap advertises repeatedly; keyed by id, each one is kept once.
        let mut seen: BTreeMap<String, Candidate> = BTreeMap::new();
        let window = Duration::from_secs(self.config.scan_timeout_secs);
        let _ = platform::timeout(window, async {
            while let Some(candidate) = scan.next().await {
                if !seen.contains_key(&candidate.id) {
                    info!("scan_all: found {} id={}", candidate.label(), candidate.id);
                }
                seen.insert(candidate.id.clone(), candidate);
            }
        })
        .await;
        scan.stop(); // stop the radio before connecting to anything

        // A scan grants nothing, so each sighting is adopted with the grant it
        // will need once connected.
        let mut found = Vec::new();
        for (id, candidate) in seen {
            let name = candidate.label().to_owned();
            let Some(family) = advertised_family(&candidate) else {
                warn!("scan_all: {name} ({id}) advertises no connectable WHOOP service");
                continue;
            };
            let grant = whoop_grant(Some(family))?;
            match bluetooth.adopt_candidate(&candidate, grant).await {
                Ok(device) => found.push(WhoopDevice {
                    name,
                    id,
                    family,
                    device,
                }),
                Err(e) => warn!("scan_all: {name} ({id}) could not be adopted: {e}"),
            }
        }
        info!("scan_all: {} strap(s) found", found.len());
        Ok(found)
    }

    /// Connect to a strap from [`Self::scan_all`], subscribe, and start the
    /// dispatch task.
    pub async fn connect_to(
        &self,
        device: WhoopDevice,
    ) -> Result<(mpsc::Receiver<WhoopEvent>, WhoopHandle)> {
        self.setup_device(device.device, device.name, Some(device.family))
            .await
    }

    /// Scan for the first WHOOP strap, connect, and start streaming.
    ///
    /// The deadline is a deadline rather than a fixed window: the chooser
    /// returns as soon as something matching advertises.
    pub async fn connect(&self) -> Result<(mpsc::Receiver<WhoopEvent>, WhoopHandle)> {
        let bluetooth = Bluetooth::shared();
        bluetooth
            .availability()
            .await
            .context("Bluetooth is not usable")?;

        info!(
            "scanning for a WHOOP strap (timeout: {} s) …",
            self.config.scan_timeout_secs
        );
        let chooser = FirstMatch::with_timeout(Duration::from_secs(self.config.scan_timeout_secs));
        let device = bluetooth
            .request_device_with(request_options(&self.config)?, &chooser)
            .await
            .map_err(|e| {
                anyhow!(
                    "no WHOOP strap found within {} s: {e}",
                    self.config.scan_timeout_secs
                )
            })?;

        let name = device.name().unwrap_or_else(|| device.id().to_owned());
        info!("found strap: {name}");
        self.setup_device(device, name, self.config.family).await
    }

    // ── Private: setup ───────────────────────────────────────────────────────

    async fn setup_device(
        &self,
        device: BluetoothDevice,
        name: String,
        family_hint: Option<DeviceFamily>,
    ) -> Result<(mpsc::Receiver<WhoopEvent>, WhoopHandle)> {
        let gatt = device.gatt();

        // `connect()` has no deadline of its own — it waits for the peripheral
        // to come into range for as long as that takes — so one is imposed
        // here. Ten seconds is generous for a BLE connection that usually takes
        // under two.
        platform::timeout(Duration::from_secs(10), gatt.connect())
            .await
            .map_err(|_| anyhow!("BLE connect() timed out after 10 s"))??;

        // Resolve the family from what the strap actually exposes. A hint from
        // the advertisement is confirmed rather than trusted, because the
        // envelope every subsequent write uses depends on this answer.
        let (family, service) = resolve_family(&gatt, family_hint).await?;
        let chars = service.get_characteristics(None).await?;
        info!("connected to {name} as {}", family.label());

        let command = find_char(&chars, family.command_characteristic())?;

        // Custom notify channels, each with its own reassembly state.
        let mut streams = Vec::new();
        let mut custom_channels = Vec::new();
        for &uuid in family.notify_characteristics() {
            match subscribe_tagged(&chars, uuid).await {
                Ok(stream) => {
                    streams.push(stream);
                    custom_channels.push(uuid);
                }
                // A missing notify channel is a narrower connection, not a
                // failed one: the command replies and the data stream live on
                // different characteristics and either can still be useful.
                Err(e) => warn!("notify characteristic {uuid}: {e}"),
            }
        }
        if streams.is_empty() {
            return Err(anyhow!(
                "no notify characteristic on the {} service could be subscribed",
                family.label()
            ));
        }

        // Standard sources, both optional. Heart Rate works unbonded, which
        // makes it the one stream available before any handshake.
        if self.config.standard_hr {
            if let Ok(hr) = gatt.get_primary_service(HEART_RATE_SERVICE).await {
                if let Ok(hr_chars) = hr.get_characteristics(None).await {
                    match subscribe_tagged(&hr_chars, HEART_RATE_MEASUREMENT).await {
                        Ok(stream) => streams.push(stream),
                        Err(e) => debug!("standard heart rate unavailable: {e}"),
                    }
                }
            }
        }
        if let Ok(battery) = gatt.get_primary_service(BATTERY_SERVICE).await {
            if let Ok(battery_chars) = battery.get_characteristics(None).await {
                match subscribe_tagged(&battery_chars, BATTERY_LEVEL).await {
                    Ok(stream) => streams.push(stream),
                    Err(e) => debug!("standard battery unavailable: {e}"),
                }
            }
        }

        // Which 5-family hardware this is. Reads are best-effort and the answer
        // is allowed to stay unresolved — an unresolved variant does not
        // establish that the strap lacks MG hardware.
        let variant = match family {
            DeviceFamily::Whoop5 => resolve_variant(&gatt).await,
            DeviceFamily::Whoop4 => Whoop5Variant::Unknown,
        };
        if family == DeviceFamily::Whoop5 {
            info!("{name}: variant resolved as {variant:?}");
        }

        let (tx, rx) = mpsc::channel::<WhoopEvent>(self.config.event_channel_capacity);
        let _ = tx
            .send(WhoopEvent::Connected {
                name: name.clone(),
                family,
            })
            .await;

        // Fires when the link drops — often sooner than the notification
        // streams close.
        let disconnect_tx = tx.clone();
        let device_id = device.id().to_owned();
        let mut disconnects = device.watch_disconnect();
        platform::spawn(async move {
            if disconnects.next().await.is_some() {
                info!("disconnect watcher: {device_id} disconnected");
                let _ = disconnect_tx.send(WhoopEvent::Disconnected).await;
            }
        });

        let pending = Arc::new(Pending::default());

        // The family handshake, written after the subscriptions so the reply
        // cannot arrive before anything is listening for it.
        //
        // WHOOP 5 writes its fixed CLIENT_HELLO. WHOOP 4 has no fixed hello:
        // its link becomes usable once one confirmed write is acknowledged
        // without error, and `GET_BATTERY_LEVEL` is the harmless read that
        // serves as that write.
        match family.client_hello() {
            Some(hello) => {
                command
                    .write_value_with_response(hello)
                    .await
                    .context("writing the WHOOP 5 CLIENT_HELLO")?;
                debug!("{name}: CLIENT_HELLO written");
            }
            None => {
                let frame =
                    command_frame(family, Command::GetBatteryLevel.opcode(), 0, &payload::ZERO);
                command
                    .write_value_with_response(&frame)
                    .await
                    .context("writing the WHOOP 4 bond handshake")?;
                debug!("{name}: bond handshake acknowledged");
            }
        }

        spawn_dispatch(
            select_all(streams),
            custom_channels,
            family,
            tx,
            Arc::clone(&pending),
        );

        let handle = WhoopHandle {
            gatt,
            command,
            family,
            variant,
            pending,
            // The handshake above used sequence 0, so the first request starts
            // at 1 and a reply to the handshake cannot satisfy it.
            seq: Arc::new(AtomicU8::new(1)),
            policy: self.config.policy,
            request_timeout: Duration::from_secs(self.config.request_timeout_secs),
            serial_gate: Arc::new(Mutex::new(crate::protocol::RepeatedSerialGate::new())),
        };

        if self.config.realtime_hr {
            // Asked for, not assumed: this changes strap state, so a failure is
            // reported rather than retried silently.
            if let Err(e) = handle.toggle_realtime_hr(true).await {
                warn!("{name}: could not start the proprietary HR stream: {e}");
            }
        }

        Ok((rx, handle))
    }
}

/// Which family a sighting's advertised services imply.
///
/// An unmapped WHOOP service is named in the log and then refused, rather than
/// being quietly treated as the nearest connectable family.
fn advertised_family(candidate: &Candidate) -> Option<DeviceFamily> {
    let mut unmapped = None;
    for advertised in &candidate.advertisement.service_uuids {
        for family in ServiceFamily::ALL {
            if *advertised == BluetoothUuid::from(family.service_uuid()) {
                match family.connectable_family() {
                    Some(f) => return Some(f),
                    None => unmapped = Some(family),
                }
            }
        }
    }
    if let Some(family) = unmapped {
        warn!(
            "{} detected but unsupported; not connecting or sending commands",
            family.label()
        );
    }
    None
}

/// Confirm the family against the services the strap actually exposes.
///
/// With a hint, only that family's service is fetched — a strap that does not
/// have it is an error rather than a silent fallback to the other envelope.
/// Without one, WHOOP 4 is tried first and WHOOP 5 second; both failing is an
/// error, because there is no third thing this crate knows how to frame.
async fn resolve_family(
    gatt: &RemoteGattServer,
    hint: Option<DeviceFamily>,
) -> Result<(DeviceFamily, webbluetooth::RemoteGattService)> {
    let candidates: Vec<DeviceFamily> = match hint {
        Some(f) => vec![f],
        None => vec![DeviceFamily::Whoop4, DeviceFamily::Whoop5],
    };
    let mut last_error = None;
    for family in candidates {
        match platform::timeout(
            Duration::from_secs(15),
            gatt.get_primary_service(family.service_uuid()),
        )
        .await
        {
            Ok(Ok(service)) => return Ok((family, service)),
            Ok(Err(e)) => last_error = Some(anyhow!("{}: {e}", family.label())),
            Err(_) => last_error = Some(anyhow!("{}: service discovery timed out", family.label())),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("no WHOOP custom service found")))
}

/// Read the Device Information strings and resolve 5.0 against MG.
async fn resolve_variant(gatt: &RemoteGattServer) -> Whoop5Variant {
    let Ok(dis) = gatt.get_primary_service(DEVICE_INFORMATION_SERVICE).await else {
        return Whoop5Variant::Unknown;
    };
    let Ok(chars) = dis.get_characteristics(None).await else {
        return Whoop5Variant::Unknown;
    };
    let serial = read_dis_string(&chars, SERIAL_NUMBER_STRING).await;
    let hardware = read_dis_string(&chars, HARDWARE_REVISION_STRING).await;
    let model = read_dis_string(&chars, MODEL_NUMBER_STRING).await;
    whoop5_variant(serial.as_deref(), hardware.as_deref(), model.as_deref())
}

// ── Dispatch ─────────────────────────────────────────────────────────────────

/// Spawn the loop that turns notifications into events.
///
/// Every byte from a custom notify channel goes through that channel's own
/// [`Reassembler`]; every completed frame is verified before a field is read,
/// and a frame that fails is emitted as [`WhoopEvent::Rejected`] rather than
/// dropped. Both decisions exist for the same reason: a frame nothing else can
/// read is exactly the frame a capture is for.
fn spawn_dispatch(
    mut notifications: webbluetooth::stream::SelectAll<BoxStream<'static, (Uuid, Vec<u8>)>>,
    custom_channels: Vec<Uuid>,
    family: DeviceFamily,
    tx: mpsc::Sender<WhoopEvent>,
    pending: Arc<Pending>,
) {
    platform::spawn(async move {
        let mut reassemblers: Vec<(Uuid, Reassembler)> = custom_channels
            .into_iter()
            .map(|uuid| (uuid, Reassembler::new(family)))
            .collect();
        let mut notification_count: u64 = 0;
        let mut frame_count: u64 = 0;

        while let Some((uuid, data)) = notifications.next().await {
            notification_count += 1;

            if uuid == HEART_RATE_MEASUREMENT {
                if let Some(hr) = parse::hr_measurement(&data) {
                    let _ = tx.send(WhoopEvent::HeartRate(hr)).await;
                }
                continue;
            }
            if uuid == BATTERY_LEVEL {
                if let Some(battery) = parse::battery_level(&data) {
                    let _ = tx.send(WhoopEvent::Battery(battery)).await;
                }
                continue;
            }

            let Some((_, reassembler)) = reassemblers.iter_mut().find(|(u, _)| *u == uuid) else {
                debug!("notification from an unexpected characteristic {uuid}");
                continue;
            };

            for frame in reassembler.feed(&data) {
                frame_count += 1;
                if frame_count <= 3 || frame_count.is_multiple_of(500) {
                    info!(
                        "dispatch: notification #{notification_count} frame #{frame_count} \
                         type={} len={}",
                        packet_type(&frame, family)
                            .map(packet_type_name)
                            .unwrap_or("?"),
                        frame.len(),
                    );
                }
                for event in decode_frame_routed(&frame, family, &pending) {
                    if tx.send(event).await.is_err() {
                        // The consumer is gone; so is the reason to keep
                        // reading. Dropping the streams here also unsubscribes.
                        info!("dispatch: receiver dropped, stopping");
                        return;
                    }
                }
            }
        }

        let dropped: u64 = reassemblers
            .iter()
            .map(|(_, r)| {
                r.header_checksum_drops() + r.below_minimum_length_drops() + r.oversize_drops()
            })
            .sum();
        info!("dispatch: stream ended after {frame_count} frame(s), {dropped} resync drop(s)");
        let _ = tx.send(WhoopEvent::Disconnected).await;
    });
}

/// Turn one complete frame into zero or more events.
///
/// Pure apart from resolving a pending request, which is what lets the whole
/// routing table be exercised from a test without a radio.
pub fn decode_frame(frame: &[u8], family: DeviceFamily) -> Vec<WhoopEvent> {
    decode_frame_inner(frame, family, None)
}

/// [`decode_frame`], additionally resolving whichever request a reply answers.
fn decode_frame_routed(frame: &[u8], family: DeviceFamily, pending: &Pending) -> Vec<WhoopEvent> {
    decode_frame_inner(frame, family, Some(pending))
}

fn decode_frame_inner(
    frame: &[u8],
    family: DeviceFamily,
    pending: Option<&Pending>,
) -> Vec<WhoopEvent> {
    let check = verify_frame(frame, family);
    if !check.ok {
        return vec![WhoopEvent::Rejected(RejectedFrame {
            reason: check.reason,
            packet_type: packet_type(frame, family),
            raw: frame.to_vec(),
        })];
    }

    let Some(ptype) = packet_type(frame, family) else {
        return Vec::new();
    };
    let mut events = Vec::new();

    match ptype {
        packet::COMMAND_RESPONSE | packet::WHOOP5_COMMAND_RESPONSE => {
            if let Some(response) = parse::command_response(frame, family) {
                // Hand the reply to whoever asked for it before decoding its
                // body: a waiter should not be held up by this side's parsing,
                // and a body this crate cannot read is still an answer.
                if let Some(pending) = pending {
                    pending.resolve(&response);
                }
                if let Some(decoded) = parse::response_payload(&response, family) {
                    events.push(match decoded {
                        parse::DecodedResponse::Battery(b) => WhoopEvent::Battery(b),
                        parse::DecodedResponse::Hello(h) => WhoopEvent::Hello(h),
                        parse::DecodedResponse::DataRange(r) => WhoopEvent::DataRange(r),
                    });
                }
                events.push(WhoopEvent::CommandResponse(response));
            }
        }
        packet::REALTIME_DATA => {
            if let Some(hr) = parse::realtime_hr(frame, family) {
                events.push(WhoopEvent::HeartRate(hr));
            }
        }
        // Type 48 only. Types 53 and 54 are a separate event namespace whose
        // layouts are not documented here, and routing them through this
        // decoder would read their payloads at type-48 offsets — the exact
        // mistake the family split exists to prevent. They fall through to the
        // preserved-bytes arm below.
        packet::EVENT => {
            if let Some(event) = parse::strap_event(frame, family) {
                if let Some(battery) = event.battery {
                    events.push(WhoopEvent::Battery(battery));
                }
                events.push(WhoopEvent::Event(event));
            }
        }
        packet::METADATA | packet::WHOOP5_METADATA => {
            if let Some(meta) = parse::history_meta(frame, family) {
                events.push(WhoopEvent::HistoryMeta(meta));
            }
        }
        packet::HISTORICAL_DATA => {
            events.push(WhoopEvent::Record(parse::historical(frame, family)));
        }
        packet::CONSOLE_LOGS => {
            if let Some(text) = parse::console_log(frame, family) {
                events.push(WhoopEvent::ConsoleLog(text));
            }
        }
        // Live raw sensor data. On WHOOP 5 the layout byte can select ECG,
        // which has a decoder; every other shape — including the WHOOP 4
        // 1,917-byte IMU and 1,921-byte optical layouts — is preserved whole
        // rather than guessed at from the packet type.
        // Live raw sensor data, where the selector differs by generation:
        // WHOOP 5 reads a layout byte at 9, WHOOP 4 selects by validated
        // payload length. Neither is guessable from the packet type, which is
        // why each goes through its own decoder and anything unrecognised is
        // preserved whole.
        packet::REALTIME_RAW_DATA => {
            let version = frame.get(9).copied().unwrap_or(0);
            let decoded = match family {
                DeviceFamily::Whoop5 => matches!(
                    version,
                    crate::protocol::layout::R16_ECG_RAW
                        | crate::protocol::layout::R17_ECG_FILTERED
                )
                .then(|| parse::whoop5_record(frame, ptype))
                .filter(|record| matches!(record, Record::Ecg(_))),
                DeviceFamily::Whoop4 => parse::whoop4_raw(frame),
            };
            match decoded {
                Some(record) => events.push(WhoopEvent::Record(record)),
                None => events.push(WhoopEvent::RawSensor {
                    packet_type: ptype,
                    version: match family {
                        DeviceFamily::Whoop4 => frame.get(6).copied().unwrap_or(0),
                        DeviceFamily::Whoop5 => version,
                    },
                    raw: frame.to_vec(),
                }),
            }
        }
        // The dedicated IMU streams. Their array positions are computed from
        // the counts rather than fixed, so the decoder validates the whole
        // calculated span first — and this layout is explicitly **not** R21, so
        // a frame that does not fit it is preserved rather than decoded as one.
        packet::HISTORICAL_IMU_DATA_STREAM | packet::REALTIME_IMU_DATA_STREAM => {
            match parse::imu_stream(frame, family, ptype) {
                Some(record) => events.push(WhoopEvent::Record(Record::ImuStream(record))),
                None => events.push(WhoopEvent::RawSensor {
                    packet_type: ptype,
                    version: frame.get(9).copied().unwrap_or(0),
                    raw: frame.to_vec(),
                }),
            }
        }
        _ => {
            debug!("unrouted packet type {ptype} ({})", packet_type_name(ptype));
            events.push(WhoopEvent::RawSensor {
                packet_type: ptype,
                version: frame.get(family.inner_offset() + 1).copied().unwrap_or(0),
                raw: frame.to_vec(),
            });
        }
    }

    events
}

// ── WhoopHandle ──────────────────────────────────────────────────────────────

/// A handle to a live connection: send commands, await replies, disconnect.
pub struct WhoopHandle {
    gatt: RemoteGattServer,
    command: RemoteGattCharacteristic,
    family: DeviceFamily,
    variant: Whoop5Variant,
    pending: Arc<Pending>,
    seq: Arc<AtomicU8>,
    policy: SendPolicy,
    request_timeout: Duration,
    /// Two-sighting confirmation for the WHOOP 4 serial — see
    /// [`Self::whoop4_serial`]. On the handle rather than inside the function
    /// because confirmation spans two calls by design.
    serial_gate: Arc<Mutex<crate::protocol::RepeatedSerialGate>>,
}

impl std::fmt::Debug for WhoopHandle {
    /// Identity without innards: the GATT handles and the pending-request map
    /// have nothing printable in them, and a handle in someone's
    /// `#[derive(Debug)]` struct should still compile.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhoopHandle")
            .field("family", &self.family)
            .field("variant", &self.variant)
            .field("policy", &self.policy)
            .field("connected", &self.is_connected())
            .finish_non_exhaustive()
    }
}

impl WhoopHandle {
    /// Which wire profile this connection speaks.
    pub fn family(&self) -> DeviceFamily {
        self.family
    }

    /// Which 5-family hardware this is, when it could be resolved.
    ///
    /// [`Whoop5Variant::Unknown`] on WHOOP 4, and on a WHOOP 5 whose Device
    /// Information signals were absent or contradictory. It does **not** mean
    /// the strap lacks MG hardware.
    pub fn variant(&self) -> Whoop5Variant {
        self.variant
    }

    /// What this connection is permitted to send.
    pub fn policy(&self) -> SendPolicy {
        self.policy
    }

    /// The next request sequence.
    ///
    /// Eight bits, so it wraps. Sequence reuse across a disconnect does not
    /// prove a late notification belongs to the new session, which is why
    /// correlation is scoped to one connection's [`Pending`] map rather than to
    /// the number alone.
    fn next_sequence(&self) -> u8 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Write a command and return without waiting for a reply.
    ///
    /// Written with response, so the returned `Ok` means the radio accepted the
    /// write — not that the strap accepted the command, and certainly not that
    /// it did the thing.
    pub async fn send(&self, command: Command, body: &[u8]) -> Result<u8> {
        self.send_raw(command.opcode(), body).await
    }

    /// Write an arbitrary opcode, subject to the same gate.
    ///
    /// The gate is checked before the frame is built, so a refusal costs no
    /// radio time: destructive commands are refused outright, and the ECG family
    /// needs both WHOOP 5 and the caller's opt-in.
    pub async fn send_raw(&self, opcode: u8, body: &[u8]) -> Result<u8> {
        admits_send(opcode, self.family, self.policy)?;
        let seq = self.next_sequence();
        let frame = command_frame(self.family, opcode, seq, body);
        self.command.write_value_with_response(&frame).await?;
        debug!(
            "sent {} ({opcode}) seq={seq} body={} byte(s)",
            crate::protocol::command_name(opcode),
            body.len()
        );
        Ok(seq)
    }

    /// Write a command and wait for its final result.
    ///
    /// The waiter is registered **before** the write, so a reply that arrives
    /// during it is not missed. A `PENDING` result keeps the request open; the
    /// wait ends at `request_timeout_secs`, which closes local waiting and
    /// proves nothing about the strap.
    pub async fn request(&self, command: Command, body: &[u8]) -> Result<CommandResponse> {
        self.request_raw(command.opcode(), body).await
    }

    /// [`Self::request`] for an arbitrary opcode.
    pub async fn request_raw(&self, opcode: u8, body: &[u8]) -> Result<CommandResponse> {
        admits_send(opcode, self.family, self.policy)?;
        let seq = self.next_sequence();
        let key = (opcode, seq);
        let rx = self.pending.register(key);

        let frame = command_frame(self.family, opcode, seq, body);
        if let Err(e) = self.command.write_value_with_response(&frame).await {
            self.pending.forget(key);
            return Err(e.into());
        }

        match platform::timeout(self.request_timeout, rx).await {
            Ok(Ok(response)) => Ok(response),
            // The dispatch loop dropped the sender: the connection ended.
            Ok(Err(_)) => {
                self.pending.forget(key);
                Err(anyhow!(
                    "connection closed while waiting for {}",
                    crate::protocol::command_name(opcode)
                ))
            }
            Err(_) => {
                self.pending.forget(key);
                Err(anyhow!(
                    "no final result for {} within {:?}",
                    crate::protocol::command_name(opcode),
                    self.request_timeout
                ))
            }
        }
    }

    /// Ask for the battery percentage.
    ///
    /// Measured for the request rather than read from a cache. A measurement
    /// timeout is not confirmed to produce the ordinary reply, so this can time
    /// out locally rather than returning a failure body.
    pub async fn battery(&self) -> Result<f32> {
        let response = self
            .request(Command::GetBatteryLevel, &payload::ZERO)
            .await?;
        if response.result != CommandResult::Success {
            return Err(anyhow!("GET_BATTERY_LEVEL returned {:?}", response.result));
        }
        parse::battery_from_command(&response.body, self.family)
            .map(|b| b.percent)
            .ok_or_else(|| anyhow!("battery body was not the documented layout"))
    }

    /// Ask for the identity hello.
    ///
    /// WHOOP 5 only: command 145 has no documented reply on WHOOP 4, and the
    /// legacy command 35 is not an interchangeable request for it.
    pub async fn hello(&self) -> Result<crate::types::Hello> {
        if self.family != DeviceFamily::Whoop5 {
            return Err(anyhow!("GET_HELLO (145) is a WHOOP 5/MG command"));
        }
        let response = self
            .request(Command::GetHello, &payload::REVISION_1)
            .await?;
        parse::hello(&response.body).ok_or_else(|| anyhow!("hello body was not revision 1 or 3"))
    }

    /// Ask for the stored record range and ring cursors.
    pub async fn data_range(&self) -> Result<crate::types::DataRange> {
        let response = self.request(Command::GetDataRange, &payload::ZERO).await?;
        if response.result != CommandResult::Success {
            return Err(anyhow!("GET_DATA_RANGE returned {:?}", response.result));
        }
        parse::data_range(&response.body)
            .ok_or_else(|| anyhow!("data-range body was not the documented 65-byte layout"))
    }

    /// Set the strap's clock.
    ///
    /// Read it back rather than trusting the result: on WHOOP 4 one of two
    /// request forms has been seen to latch, and on WHOOP 5 a clock-read
    /// failure can produce a zero time beside a success. There is no
    /// independent validity flag on either.
    pub async fn set_clock(&self, unix_seconds: u32) -> Result<()> {
        let (command, body) = match self.family {
            DeviceFamily::Whoop4 => (
                Command::SetClockDeprecated,
                payload::set_clock_whoop4(unix_seconds),
            ),
            DeviceFamily::Whoop5 => (
                Command::SetClock,
                payload::set_clock_whoop5(unix_seconds, 0),
            ),
        };
        let response = self.request(command, &body).await?;
        if response.result != CommandResult::Success {
            return Err(anyhow!("SET_CLOCK returned {:?}", response.result));
        }
        Ok(())
    }

    /// Start or stop the proprietary live heart-rate stream.
    ///
    /// The standard Heart Rate Service is unaffected either way.
    pub async fn toggle_realtime_hr(&self, on: bool) -> Result<()> {
        self.send(Command::ToggleRealtimeHr, &payload::toggle(on))
            .await
            .map(|_| ())
    }

    /// Begin the historical offload.
    ///
    /// WHOOP 5 is asked for its range first, as the documented order does;
    /// the result is logged rather than required, because a missing range reply
    /// does not mean history is empty.
    ///
    /// Drive the stream with [`crate::history::HistorySession`], and read its
    /// documentation before acknowledging anything.
    pub async fn begin_history(&self) -> Result<()> {
        if self.family == DeviceFamily::Whoop5 {
            match self.data_range().await {
                Ok(range) => info!(
                    "history preflight: {} page(s) behind, capacity {}",
                    range.pages_behind(),
                    range.capacity
                ),
                Err(e) => info!("history preflight: no usable range ({e}); continuing"),
            }
        }
        let response = self
            .request(Command::SendHistoricalData, &payload::ZERO)
            .await?;
        if response.result == CommandResult::Failure {
            return Err(anyhow!("SEND_HISTORICAL_DATA was refused"));
        }
        Ok(())
    }

    /// Acknowledge one committed chunk.
    ///
    /// Takes an [`crate::history::AckPayload`], which can only be obtained from
    /// [`crate::history::PendingChunk::committed`] — so this cannot be called
    /// for a chunk that was not stored first. That is the one ordering rule the
    /// whole offload depends on: a successful acknowledgement can let the strap
    /// reclaim the history it just sent.
    pub async fn ack_history(&self, ack: &crate::history::AckPayload) -> Result<()> {
        self.send(Command::HistoricalDataResult, ack.body())
            .await
            .map(|_| ())
    }

    /// Ask the strap to stop sending history.
    ///
    /// A stop request, not a trim. Local cleanup must not wait on a reply, so
    /// this does not await one.
    pub async fn abort_history(&self) -> Result<()> {
        self.send(Command::AbortHistoricalTransmits, &payload::ZERO)
            .await
            .map(|_| ())
    }

    /// Buzz the strap.
    ///
    /// `loops` is the number of pulses, 1–8. On WHOOP 5 the wire field counts
    /// the repeats after the first pulse, which [`payload::maverick_buzz`]
    /// handles, so asking for one buzz gives one buzz.
    pub async fn buzz(&self, loops: u8) -> Result<()> {
        match self.family {
            DeviceFamily::Whoop4 => {
                self.send(
                    Command::RunHapticsPattern,
                    &payload::whoop4_haptics(1, loops),
                )
                .await
            }
            DeviceFamily::Whoop5 => {
                self.send(
                    Command::RunHapticPatternMaverick,
                    &payload::maverick_buzz(loops),
                )
                .await
            }
        }
        .map(|_| ())
    }

    /// Start or stop MG ECG data generation.
    ///
    /// Needs `ecg_opt_in` and WHOOP 5. Avoid resending a start blindly:
    /// repeated starts can lose the record a companion collection needs in
    /// order to clean up. Connecting is not an acknowledgement that a previous
    /// ECG session was restored, so check before asking again.
    pub async fn ecg_generation(&self, start: bool) -> Result<()> {
        let signal = if start { 2 } else { 1 };
        self.send(Command::ToggleLabradorDataGeneration, &payload::ecg(signal))
            .await
            .map(|_| ())
    }

    /// Toggle the filtered realtime ECG output.
    pub async fn ecg_filtered_stream(&self, on: bool) -> Result<()> {
        self.send(Command::ToggleLabradorFiltered, &payload::ecg(u8::from(on)))
            .await
            .map(|_| ())
    }

    // ── Battery, identity and service reads ─────────────────────────────────

    /// Read the cached battery-pack record — WHOOP 5 command 151.
    ///
    /// Cached, not live: a success is not a freshness guarantee, and the strap
    /// reports success even with no pack present. The charge field comes back
    /// unscaled, because the wire does not establish a percentage scale for it.
    pub async fn battery_pack(&self) -> Result<BatteryPack> {
        self.whoop5_only(Command::GetBatteryPack)?;
        let response = self
            .request(Command::GetBatteryPack, &payload::REVISION_1)
            .await?;
        parse::battery_pack(&response.body)
            .ok_or_else(|| anyhow!("battery-pack body was not the documented 28-byte record"))
    }

    /// Read the cached extended battery record — WHOOP 4 command 98.
    ///
    /// Announced by event 63. An empty cache returns failure rather than zeros,
    /// so a record here means the strap actually had one stored.
    pub async fn extended_battery(&self) -> Result<ExtendedBattery> {
        if self.family != DeviceFamily::Whoop4 {
            return Err(anyhow!(
                "GET_EXTENDED_BATTERY_INFO (98) has no WHOOP 5/MG fuel-gauge contract; \
                 use battery_pack() instead"
            ));
        }
        let response = self
            .request(Command::GetExtendedBatteryInfo, &payload::ZERO)
            .await?;
        if response.result != CommandResult::Success {
            return Err(anyhow!(
                "the extended battery cache was empty or unreadable"
            ));
        }
        parse::extended_battery(&response.body)
            .ok_or_else(|| anyhow!("extended battery body was not the documented 25-byte record"))
    }

    /// Read the advertised name — WHOOP 5 command 141.
    ///
    /// Status 2 covers fallback, empty storage **and** a storage-read failure
    /// without distinguishing them, so an empty name is not evidence that none
    /// is stored.
    pub async fn advertising_name(&self) -> Result<AdvertisingName> {
        self.whoop5_only(Command::GetAdvertisingName)?;
        let response = self
            .request(Command::GetAdvertisingName, &payload::REVISION_1)
            .await?;
        parse::advertising_name(&response.body)
            .ok_or_else(|| anyhow!("advertising-name body was not the fixed 19-byte reply"))
    }

    /// Write the advertised name.
    ///
    /// A configuration write, so it needs [`SendPolicy::config_write`]. On
    /// WHOOP 4 this is also a **restart**: the rename applies on reboot, which
    /// interrupts anything in flight. The strap stores at most 15 bytes either
    /// way, so a read-back can be shorter than what was sent.
    pub async fn set_advertising_name(&self, name: &str) -> Result<()> {
        let (command, body) = match self.family {
            DeviceFamily::Whoop4 => (
                Command::SetAdvertisingNameHarvard,
                payload::set_advertising_name_whoop4(name),
            ),
            DeviceFamily::Whoop5 => (
                Command::SetAdvertisingName,
                payload::set_advertising_name_whoop5(name),
            ),
        };
        self.send(command, &body).await.map(|_| ())
    }

    /// Read the body-location status — command 84.
    ///
    /// Diagnostic only, on both generations. It never feeds wear detection,
    /// sleep gating or scoring, and on WHOOP 5 only the on-body flag is real:
    /// the two middle bytes of that reply are fixed placeholders.
    pub async fn body_location(&self) -> Result<BodyLocationStatus> {
        let response = self
            .request(Command::GetBodyLocationAndStatus, &payload::revision_only())
            .await?;
        parse::body_location(&response.body, self.family)
            .ok_or_else(|| anyhow!("body-location body was not the documented record"))
    }

    /// The WHOOP 4 strap serial, once two hellos have agreed on it.
    ///
    /// A WHOOP 4 exposes no DIS serial, so this reads command 35 and takes the
    /// nine-byte window at body offset 14 — never scanning for it, because the
    /// adjacent 54 bytes are key and signature material.
    ///
    /// Returns `Ok(None)` on the first sighting **by design**: the offset is an
    /// inference from a capture, and if that run were per-session rather than
    /// per-strap, adopting it immediately would mint a new identity on every
    /// connect. Two hellos carrying the same value cannot both be a fresh
    /// token. Call it twice.
    pub async fn whoop4_serial(&self) -> Result<Option<String>> {
        if self.family != DeviceFamily::Whoop4 {
            return Err(anyhow!(
                "command 35's serial window is a WHOOP 4 contract; a WHOOP 5/MG serial \
                 comes from the Device Information Service"
            ));
        }
        let response = self
            .request(Command::GetHelloHarvard, &payload::ZERO)
            .await?;
        let Some(serial) = crate::protocol::whoop4_serial(&response.body) else {
            return Ok(None);
        };
        let mut gate = self
            .serial_gate
            .lock()
            .map_err(|_| anyhow!("the serial gate was poisoned"))?;
        Ok(gate.offer(&serial))
    }

    // ── Alarms and haptics ──────────────────────────────────────────────────

    /// Arm an alarm for an absolute wake instant.
    ///
    /// WHOOP 5 takes an ID of 1–6 and the full revision-4 record; WHOOP 4 has
    /// one legacy slot and ignores `id`. Both require a correct strap clock,
    /// which is a **separate** prerequisite — read it back rather than assuming
    /// it, because the strap compares against its own seconds.
    ///
    /// On WHOOP 5 the response carries a validation detail distinct from the
    /// outer result, and the two disagree in a documented way: a pattern can be
    /// stored while the time is rejected. Both halves are reported.
    ///
    /// Scheduling is **one-shot**. A due alarm is cleared before the haptic
    /// finishes, so weekday recurrence means arming the next absolute instant
    /// yourself.
    pub async fn arm_alarm(
        &self,
        id: u8,
        wake_epoch_ms: i64,
        pattern: payload::HapticPattern,
    ) -> Result<AlarmSetDetail> {
        let body = match self.family {
            DeviceFamily::Whoop4 => {
                let seconds = (wake_epoch_ms / 1000).clamp(0, u32::MAX as i64) as u32;
                payload::set_alarm_whoop4(seconds)
            }
            DeviceFamily::Whoop5 => {
                if !(1..=6).contains(&id) {
                    return Err(anyhow!("alarm IDs are 1–6; got {id}"));
                }
                // Checked locally first: a rejected request has still cost a
                // round trip and may have stored half of itself.
                pattern
                    .validate()
                    .map_err(|why| anyhow!("haptic pattern rejected before sending: {why}"))?;
                payload::alarm_set_whoop5(id, wake_epoch_ms, pattern)
            }
        };

        let response = self.request(Command::SetAlarmTime, &body).await?;
        let detail = response
            .body
            .get(1)
            .copied()
            .map(AlarmSetDetail::from_byte)
            .unwrap_or(AlarmSetDetail::Unknown(0));

        // Detail 1 can accompany outer failure, so neither half is reported
        // without the other.
        if response.result != CommandResult::Success && detail == AlarmSetDetail::Accepted {
            warn!("alarm: pattern validated but the request failed — the time was likely rejected");
        }
        Ok(detail)
    }

    /// Read a stored alarm — WHOOP 5 command 67.
    ///
    /// An all-zero time means a cleared slot **or** a read fallback: GET can
    /// return zeros after a read failure while still reporting success, and
    /// there is no flag separating the two.
    pub async fn get_alarm(&self, id: u8) -> Result<AlarmRecord> {
        self.whoop5_only(Command::GetAlarmTime)?;
        let response = self
            .request(Command::GetAlarmTime, &payload::alarm_get_whoop5(id))
            .await?;
        parse::alarm_record(&response.body)
            .ok_or_else(|| anyhow!("alarm body was not the documented 21-byte record"))
    }

    /// Fire a stored alarm now.
    ///
    /// Requires an existing non-zero stored time and does not apply the
    /// future-time check. It also **clears** the selected record, so it is not
    /// a non-destructive preview of a schedule.
    ///
    /// The initial reply is pending; the final result is success only when the
    /// detail is `Completed`. The strap may make up to three start attempts
    /// before reporting a timeout, so do not issue another start while waiting.
    pub async fn run_alarm(&self, id: u8) -> Result<AlarmRunDetail> {
        let body = match self.family {
            DeviceFamily::Whoop4 => payload::REVISION_1.to_vec(),
            DeviceFamily::Whoop5 => payload::alarm_run_whoop5(id),
        };
        let response = self.request(Command::RunAlarm, &body).await?;
        Ok(response
            .body
            .get(1)
            .copied()
            .map(AlarmRunDetail::from_byte)
            .unwrap_or(AlarmRunDetail::Unknown(0)))
    }

    /// Disarm one alarm, or every slot.
    ///
    /// `None` disables all six on WHOOP 5. An all-ID disable can clear some
    /// slots and still return failure when at least one was not cleared, so
    /// read individual slots back when their state matters.
    ///
    /// Disabling clears the saved record; it does not stop an already active
    /// haptic — use [`Self::stop_haptics`] for that.
    pub async fn disable_alarm(&self, id: Option<u8>) -> Result<()> {
        let body = match (self.family, id) {
            (DeviceFamily::Whoop4, _) => payload::REVISION_1.to_vec(),
            (DeviceFamily::Whoop5, Some(id)) => payload::alarm_disable_whoop5(id),
            (DeviceFamily::Whoop5, None) => payload::alarm_disable_all_whoop5(),
        };
        let response = self.request(Command::DisableAlarm, &body).await?;
        if response.result != CommandResult::Success {
            return Err(anyhow!(
                "DISABLE_ALARM returned {:?} — some slots may still be armed",
                response.result
            ));
        }
        Ok(())
    }

    /// Stop an in-progress haptic — command 122.
    ///
    /// Asynchronous: the pending reply and the final result have different
    /// success conditions, so the final one is what this waits for. Stopping
    /// the current effect does not disable stored alarms, and it does not
    /// establish that no pending request remains.
    pub async fn stop_haptics(&self) -> Result<()> {
        let body = payload::stop_haptics(self.family);
        self.request(Command::StopHaptics, &body).await.map(|_| ())
    }

    /// Select which wrist the strap is worn on — MG only, command 123.
    ///
    /// In-memory: persistence across reboot is not established. Needs
    /// [`SendPolicy::ecg`], because this is part of the ECG command family.
    pub async fn select_wrist(&self, wrist: Wrist) -> Result<()> {
        self.send(Command::SelectWrist, &payload::ecg(wrist as u8))
            .await
            .map(|_| ())
    }

    // ── Named configuration ─────────────────────────────────────────────────

    /// Read a device-configuration value by name — command 121.
    ///
    /// Checks the outer result and the echoed key before returning: a non-NUL
    /// 32-byte key is normalised by the strap and will not echo exactly, and an
    /// unknown key returns result 0 with a zero value field.
    ///
    /// A zero value can still be a **fallback** rather than a stored zero — an
    /// unreadable storage slot is returned as zeros with result 1 — which
    /// [`ConfigValue::zero_filled`] preserves.
    pub async fn read_device_value(&self, key: &str) -> Result<ConfigValue> {
        self.named_read(Command::ReadDeviceValue, key).await
    }

    /// Read a feature-flag value by name — command 128.
    ///
    /// One hands-on report has this reply's value field contaminated by a stale
    /// shared buffer, so treat a flag read as weaker evidence than a device
    /// value read, and prefer [`Self::enumerate_feature_flags`] when all you
    /// need are names.
    pub async fn read_feature_flag(&self, key: &str) -> Result<ConfigValue> {
        self.named_read(Command::ReadFeatureValue, key).await
    }

    async fn named_read(&self, command: Command, key: &str) -> Result<ConfigValue> {
        self.whoop5_only(command)?;
        let response = self.request(command, &payload::named_read(key)).await?;
        if response.result != CommandResult::Success {
            return Err(anyhow!(
                "{} {key:?} returned {:?} — unknown key, read error or formatting failure",
                command.name(),
                response.result
            ));
        }
        let value = parse::config_value(&response.body)
            .ok_or_else(|| anyhow!("configuration body was not the documented 65-byte record"))?;
        // The normalised echo is the only confirmation that the value belongs
        // to the key that was asked for.
        if !key.starts_with(value.key.as_str()) && value.key != key {
            return Err(anyhow!(
                "configuration reply echoed {:?} for a read of {key:?}",
                value.key
            ));
        }
        Ok(value)
    }

    /// Write a device-configuration value — command 119.
    ///
    /// Needs [`SendPolicy::config_write`]. The value is ASCII matching the
    /// key's declared type; [`crate::protocol::DEVICE_KEYS`] lists them, and
    /// writing a type the key does not take is not a capability test.
    ///
    /// Request acceptance, readback, visible effect and reboot persistence are
    /// four separate states. Read the value back, then observe the behaviour
    /// separately.
    pub async fn write_device_value(&self, key: &str, value: &str) -> Result<()> {
        self.send(Command::SetDeviceValue, &payload::named_write(key, value))
            .await
            .map(|_| ())
    }

    /// Write a feature-flag value — command 120. Needs
    /// [`SendPolicy::config_write`].
    ///
    /// Polarity is not uniform across flags: writing ASCII `2` to the R22
    /// master *removes* its contribution, and `disable_pip_r26_packets` has
    /// inverse permission polarity. Check
    /// [`crate::protocol::FEATURE_FLAGS`] before assuming a direction.
    pub async fn write_feature_flag(&self, key: &str, value: &str) -> Result<()> {
        self.send(Command::SetFeatureValue, &payload::named_write(key, value))
            .await
            .map(|_| ())
    }

    /// Force the worn state, disabling normal wear detection — command 148.
    ///
    /// Needs [`SendPolicy::config_write`]. Restoring detection does not promise
    /// an immediate off-body report, so a reading taken right afterwards is not
    /// evidence either way.
    pub async fn set_wear_detection_override(&self, force_worn: bool) -> Result<()> {
        self.send(
            Command::SetWearDetectionOverride,
            &payload::wear_detection_override(force_worn),
        )
        .await
        .map(|_| ())
    }

    /// Set the persistent LED accessibility option — command 149.
    ///
    /// Needs [`SendPolicy::config_write`]. This updates one option inside a
    /// shared settings record, and the strap starts from defaults if reading
    /// that record fails — so success does not prove the other stored options
    /// survived.
    pub async fn set_led_accessibility(&self, enabled: bool) -> Result<()> {
        self.send(
            Command::SetLedAccessibility,
            &payload::led_accessibility(enabled),
        )
        .await
        .map(|_| ())
    }

    /// Walk the device-configuration key names — commands 115 then 116.
    pub async fn enumerate_device_keys(&self) -> Result<ConfigWalk> {
        self.walk(Command::CountDeviceKeys, Command::NextDeviceKey)
            .await
    }

    /// Walk the feature-flag names — commands 117 then 118.
    ///
    /// Names only: no values are read and nothing is written.
    pub async fn enumerate_feature_flags(&self) -> Result<ConfigWalk> {
        self.walk(
            Command::StartFeatureFlagExchange,
            Command::SendNextFeatureFlag,
        )
        .await
    }

    /// The bounded enumeration walk shared by both namespaces.
    ///
    /// The cursor is **shared strap state**, so each request is sent only after
    /// the previous reply lands — concurrent requests would interleave into one
    /// cursor and produce a list belonging to neither.
    ///
    /// Stopping is the subtle part, and every condition names itself in
    /// [`ConfigWalk::stop`]:
    ///
    /// - `index = 255` is the one terminator a strap has served unambiguously.
    /// - An **invalid** entry on its own does **not** stop the walk. It could
    ///   equally mark an empty or retired slot with the list continuing past
    ///   it, and on the walks on record the two readings have never been
    ///   separated: one run served sixteen entries all valid with no 255 at
    ///   all, another ended on an entry carrying 255 and invalid together. So
    ///   an invalid entry is recorded, stepped over, and the next one asked
    ///   for — what comes back is what separates the readings.
    /// - The rest are this side's bounds: eight consecutive invalid entries, a
    ///   repeated index during such a run (a parked cursor), the announced
    ///   count plus four, or a hard cap of 128.
    async fn walk(&self, count_command: Command, next_command: Command) -> Result<ConfigWalk> {
        const MAX_EMPTY_RUN: usize = 8;
        const HARD_CAP: usize = 128;

        self.whoop5_only(count_command)?;
        let start = self.request(count_command, &payload::REVISION_1).await?;
        let announced = parse::config_count(&start.body).unwrap_or(0);

        let mut entries: Vec<ConfigEntry> = Vec::new();
        let mut empty_run = 0usize;
        let limit = HARD_CAP.min(usize::from(announced).saturating_add(4).max(8));

        let stop = loop {
            if entries.len() >= limit {
                break if limit == HARD_CAP {
                    WalkStop::HardCap
                } else {
                    WalkStop::AnnouncedCount
                };
            }
            let Ok(reply) = self.request(next_command, &payload::REVISION_1).await else {
                break WalkStop::ProtocolError;
            };
            let Some(entry) = parse::config_entry(&reply.body) else {
                break WalkStop::ProtocolError;
            };

            if entry.index == 0xFF {
                entries.push(entry);
                break WalkStop::EndMarker;
            }
            if entry.valid {
                empty_run = 0;
            } else {
                // A parked cursor repeats its index while still reporting
                // invalid; that is a different fault from a run of holes.
                if entries.last().is_some_and(|last| last.index == entry.index) {
                    entries.push(entry);
                    break WalkStop::ParkedCursor;
                }
                empty_run += 1;
                if empty_run >= MAX_EMPTY_RUN {
                    entries.push(entry);
                    break WalkStop::EmptyRun;
                }
            }
            entries.push(entry);
        };

        Ok(ConfigWalk {
            announced,
            entries,
            stop,
        })
    }

    /// Clear every R22 flag, verifying each write by reading it back.
    ///
    /// Needs [`SendPolicy::config_write`]. Writes ASCII `0` to the master and
    /// each version selector in turn, reading each one back before moving on,
    /// because an acknowledgement is not a stored value.
    ///
    /// Returns the keys whose readback did **not** confirm the write. An empty
    /// list is the only success; a non-empty one is not a transport failure,
    /// it is the strap disagreeing with its own acknowledgement.
    pub async fn disable_r22(&self) -> Result<Vec<String>> {
        let mut unconfirmed = Vec::new();
        for key in crate::protocol::R22_FEATURE_KEYS {
            self.write_feature_flag(key, crate::protocol::FEATURE_FLAG_OFF)
                .await?;
            match self.read_feature_flag(key).await {
                Ok(value) if value.value == crate::protocol::FEATURE_FLAG_OFF => {}
                // Includes the contaminated-buffer case: an unreliable read is
                // reported as unconfirmed rather than assumed good.
                _ => unconfirmed.push(key.to_owned()),
            }
        }
        Ok(unconfirmed)
    }

    // ── Diagnostics ─────────────────────────────────────────────────────────

    /// Send one restart-probe candidate — WHOOP 4 only.
    ///
    /// The documented contract says neither restart command evaluates its body,
    /// so all five candidates should be equivalent; one device observation
    /// showed no response, disconnect or reboot at all. The probe exists to
    /// measure that disagreement, which is why it sends exactly **one**
    /// candidate per call and names it in the log.
    ///
    /// This restarts a strap. It interrupts any offload in flight. Never call
    /// it on a connect path or without a user asking.
    pub async fn reboot_probe(&self, variant: RebootProbe) -> Result<()> {
        if self.family != DeviceFamily::Whoop4 {
            return Err(anyhow!("the restart probe is scoped to WHOOP 4"));
        }
        let (opcode, body) = variant.request();
        warn!("restart probe: sending {}", variant.label());
        // Deliberately not awaiting a reply: the interesting outcomes are a
        // dropped link and silence, neither of which produces one.
        self.send_raw(opcode, body).await.map(|_| ())
    }

    /// Reject a command that only exists on WHOOP 5/MG.
    fn whoop5_only(&self, command: Command) -> Result<()> {
        if self.family != DeviceFamily::Whoop5 {
            return Err(anyhow!(
                "{} ({}) is a WHOOP 5/MG command; this connection is a {}",
                command.name(),
                command.opcode(),
                self.family.label()
            ));
        }
        Ok(())
    }

    /// Whether the peripheral is still connected at the adapter level.
    ///
    /// Useful as a watchdog: it notices a drop sooner than waiting for the
    /// notification streams to close.
    pub fn is_connected(&self) -> bool {
        self.gatt.connected()
    }

    /// Disconnect.
    pub async fn disconnect(&self) -> Result<()> {
        self.gatt.disconnect();
        Ok(())
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{crc32, crc8, MetadataType};
    use crate::types::HrSource;

    /// A WHOOP 4 frame with a correct envelope, for the routing table.
    ///
    /// `body` starts at frame offset 6 — after the four envelope bytes and the
    /// type and sequence — so a field documented at frame offset `n` is written
    /// at `body[n - 6]`.
    fn whoop4(ptype: u8, seq: u8, body: &[u8]) -> Vec<u8> {
        let mut inner = vec![ptype, seq];
        inner.extend_from_slice(body);
        let length = (inner.len() + 4) as u16;
        let len_bytes = length.to_le_bytes();
        let mut frame = vec![0xAA];
        frame.extend_from_slice(&len_bytes);
        frame.push(crc8(&len_bytes));
        frame.extend_from_slice(&inner);
        frame.extend_from_slice(&crc32(&inner).to_le_bytes());
        frame
    }

    #[test]
    fn a_broken_frame_is_emitted_rather_than_dropped() {
        let mut frame = whoop4(packet::REALTIME_DATA, 1, &[0u8; 12]);
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;

        let pending = Pending::default();
        let events = decode_frame_routed(&frame, DeviceFamily::Whoop4, &pending);
        let [WhoopEvent::Rejected(rejected)] = &events[..] else {
            panic!("expected one rejected frame, got {events:?}");
        };
        assert_eq!(
            rejected.reason,
            crate::protocol::FrameRejectReason::PayloadChecksum
        );
        // The bytes survive, which is the only reason the event exists.
        assert_eq!(rejected.raw, frame);
        assert_eq!(rejected.packet_type, Some(packet::REALTIME_DATA));
    }

    #[test]
    fn a_command_response_reaches_both_its_waiter_and_the_stream() {
        // command 26, origin sequence 5, success, a WHOOP 4 tenths body.
        let frame = whoop4(packet::COMMAND_RESPONSE, 0x77, &[26, 5, 1, 0xE2, 0x01]);
        let pending = Pending::default();
        let rx = pending.register((26, 5));

        let events = decode_frame_routed(&frame, DeviceFamily::Whoop4, &pending);
        // The decoded battery comes first so a consumer sees the value before
        // the acceptance it arrived in.
        assert!(matches!(events[0], WhoopEvent::Battery(_)));
        assert!(matches!(events[1], WhoopEvent::CommandResponse(_)));

        let response = rx.blocking_recv().expect("the waiter should be resolved");
        assert_eq!(response.command, 26);
        assert_eq!(response.origin_sequence, 5);
        assert_eq!(response.response_sequence, 0x77);
    }

    #[test]
    fn a_pending_result_keeps_the_request_open() {
        let pending = Pending::default();
        let mut rx = pending.register((34, 9));

        // Result 2 is PENDING: the request is not finished, so the waiter must
        // not be resolved with it — otherwise the final body never arrives.
        let frame = whoop4(packet::COMMAND_RESPONSE, 1, &[34, 9, 2]);
        decode_frame_routed(&frame, DeviceFamily::Whoop4, &pending);
        assert!(rx.try_recv().is_err(), "a PENDING reply must not resolve");

        let frame = whoop4(packet::COMMAND_RESPONSE, 2, &[34, 9, 1]);
        decode_frame_routed(&frame, DeviceFamily::Whoop4, &pending);
        let response = rx.try_recv().expect("the final result should resolve");
        assert_eq!(response.result, CommandResult::Success);
    }

    #[test]
    fn a_reply_to_a_different_sequence_does_not_resolve_a_waiter() {
        let pending = Pending::default();
        let mut rx = pending.register((26, 5));
        // Same command, different originating sequence: a different request.
        let frame = whoop4(packet::COMMAND_RESPONSE, 1, &[26, 6, 1, 0x64, 0x00]);
        decode_frame_routed(&frame, DeviceFamily::Whoop4, &pending);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn the_routing_table_sends_each_type_somewhere() {
        let pending = Pending::default();
        let decode = |frame: &[u8]| decode_frame_routed(frame, DeviceFamily::Whoop4, &pending);

        // Live HR.
        let mut body = vec![0u8; 12];
        body[6] = 62; // heart rate at frame offset 12
        let events = decode(&whoop4(packet::REALTIME_DATA, 1, &body));
        let [WhoopEvent::HeartRate(hr)] = &events[..] else {
            panic!("expected a heart rate, got {events:?}");
        };
        assert_eq!(hr.bpm, 62);
        assert_eq!(hr.source, HrSource::RealtimePacket);

        // An event, which also yields its battery reading.
        let mut body = vec![0u8; 24];
        body[0] = 3; // BATTERY_LEVEL at frame offset 6
        body[11] = 0xE2; // percent u16 at frame offset 17
        body[12] = 0x01;
        let events = decode(&whoop4(packet::EVENT, 1, &body));
        assert!(matches!(events[0], WhoopEvent::Battery(_)));
        assert!(matches!(events[1], WhoopEvent::Event(_)));

        // Metadata.
        let mut body = vec![0u8; 24];
        body[0] = 3; // HISTORY_COMPLETE at frame offset 6
        let events = decode(&whoop4(packet::METADATA, 1, &body));
        let [WhoopEvent::HistoryMeta(meta)] = &events[..] else {
            panic!("expected history metadata, got {events:?}");
        };
        assert_eq!(meta.kind, MetadataType::HistoryComplete);

        // A historical record of an unknown version is preserved.
        let events = decode(&whoop4(packet::HISTORICAL_DATA, 99, &[0u8; 40]));
        assert!(matches!(
            events[0],
            WhoopEvent::Record(Record::Unknown { .. })
        ));

        // A console log.
        let mut body = vec![0u8; 20];
        body[5..10].copy_from_slice(b"hello"); // text from frame offset 11
        let events = decode(&whoop4(packet::CONSOLE_LOGS, 1, &body));
        let [WhoopEvent::ConsoleLog(text)] = &events[..] else {
            panic!("expected a console log, got {events:?}");
        };
        assert!(text.starts_with("hello"));

        // A packet type with no route still produces its bytes.
        let events = decode(&whoop4(200, 1, &[0u8; 12]));
        assert!(matches!(
            events[0],
            WhoopEvent::RawSensor {
                packet_type: 200,
                ..
            }
        ));
    }

    #[test]
    fn whoop4_raw_sensor_frames_are_preserved_not_guessed() {
        // The 1,917-byte IMU and 1,921-byte optical layouts are documented but
        // not decoded here, so they must arrive whole rather than as zeros.
        let pending = Pending::default();
        let frame = whoop4(packet::REALTIME_RAW_DATA, 1, &[0u8; 1917]);
        let events = decode_frame_routed(&frame, DeviceFamily::Whoop4, &pending);
        let [WhoopEvent::RawSensor {
            packet_type, raw, ..
        }] = &events[..]
        else {
            panic!("expected a raw sensor frame, got {events:?}");
        };
        assert_eq!(*packet_type, packet::REALTIME_RAW_DATA);
        assert_eq!(raw.len(), frame.len());
    }

    #[test]
    fn granted_services_always_include_the_fallback_sources() {
        // A grant covers exactly what it names, so forgetting one of these
        // turns into `get_primary_service` refusing it at connect time.
        let services = granted_services(Some(DeviceFamily::Whoop5));
        assert!(services.contains(&DeviceFamily::Whoop5.service_uuid()));
        assert!(services.contains(&HEART_RATE_SERVICE));
        assert!(services.contains(&BATTERY_SERVICE));
        assert!(services.contains(&DEVICE_INFORMATION_SERVICE));
        // With no family chosen, both custom services have to be granted:
        // the family is confirmed after connecting, not before.
        let either = granted_services(None);
        assert!(either.contains(&DeviceFamily::Whoop4.service_uuid()));
        assert!(either.contains(&DeviceFamily::Whoop5.service_uuid()));
    }

    #[test]
    fn scan_filters_cover_both_families_by_default() {
        let config = WhoopClientConfig::default();
        assert_eq!(scan_filters(&config).unwrap().len(), 2);
        let one = WhoopClientConfig {
            family: Some(DeviceFamily::Whoop4),
            ..WhoopClientConfig::default()
        };
        assert_eq!(scan_filters(&one).unwrap().len(), 1);
    }
}

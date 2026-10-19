// Bluetooth controller pairing for Kazeta OS, used by the bios BLUETOOTH screen as the session
// user (BlueZ lets any user call it). While the screen is open it discovers, lists the paired
// devices and the gamepads found in pairing mode, pairs those on its own (they can be picked again
// if it failed) and forgets paired ones.
// There is no UI for pairing requests, so it registers an agent that accepts every pairing and
// authorization request, as the default agent: BlueZ's sixaxis plugin asks the default agent
// before pairing a DualShock 3 or 4 plugged in over USB, which is then paired by the cable alone.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

const AGENT_PATH: &str = "/org/kazeta/agent";
const AGENT_CAPABILITY: &str = "NoInputNoOutput";
const PIN_CODE: &str = "0000";
const REGISTER_ATTEMPTS: u32 = 20;
const REGISTER_RETRY_INTERVAL: Duration = Duration::from_millis(500);
/// How often the screen's device list is read from BlueZ
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
/// How long a pairing result stays on the screen
const STATUS_DURATION: Duration = Duration::from_secs(4);
/// Times a gamepad found in pairing mode is paired on its own each time the screen is opened, so
/// one that refuses does not keep the adapter busy
const AUTO_PAIR_ATTEMPTS: u32 = 2;

/// Class of device: peripheral major class, joystick or gamepad minor class
const COD_MAJOR_PERIPHERAL: u32 = 0x05;
const COD_MINOR_JOYSTICK: u32 = 0x01;
const COD_MINOR_GAMEPAD: u32 = 0x02;
/// Bluetooth LE appearance of joysticks and gamepads
const APPEARANCE_JOYSTICK: u16 = 0x03c3;
const APPEARANCE_GAMEPAD: u16 = 0x03c4;

type ManagedObjects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

struct Agent;

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {}

    fn request_pin_code(&self, device: ObjectPath<'_>) -> String {
        eprintln!("Bluetooth: PIN code requested by {}", device);
        PIN_CODE.to_string()
    }

    fn display_pin_code(&self, _device: ObjectPath<'_>, _pincode: &str) {}

    fn request_passkey(&self, device: ObjectPath<'_>) -> u32 {
        eprintln!("Bluetooth: passkey requested by {}", device);
        0
    }

    fn display_passkey(&self, _device: ObjectPath<'_>, _passkey: u32, _entered: u16) {}

    fn request_confirmation(&self, device: ObjectPath<'_>, _passkey: u32) {
        eprintln!("Bluetooth: pairing confirmed for {}", device);
    }

    fn request_authorization(&self, device: ObjectPath<'_>) {
        eprintln!("Bluetooth: pairing authorized for {}", device);
    }

    fn authorize_service(&self, device: ObjectPath<'_>, uuid: &str) {
        eprintln!("Bluetooth: service {} authorized for {}", uuid, device);
    }

    fn cancel(&self) {}
}

/// Register the agent as the default one, which also answers the requests no pairing call started
fn register_agent(conn: &Connection) -> zbus::Result<()> {
    conn.object_server().at(AGENT_PATH, Agent)?;
    let manager = Proxy::new(conn, "org.bluez", "/org/bluez", "org.bluez.AgentManager1")?;
    let path = ObjectPath::try_from(AGENT_PATH)?;
    // bluetoothd may still be claiming its bus name right after it started
    let mut attempt = 1;
    loop {
        match manager.call::<_, _, ()>("RegisterAgent", &(&path, AGENT_CAPABILITY)) {
            Ok(()) => break,
            Err(e) if attempt < REGISTER_ATTEMPTS => {
                eprintln!("Bluetooth: retrying agent registration: {}", e);
                attempt += 1;
                thread::sleep(REGISTER_RETRY_INTERVAL);
            }
            Err(e) => return Err(e),
        }
    }
    manager.call::<_, _, ()>("RequestDefaultAgent", &(&path,))
}

/// A device shown on the BLUETOOTH screen
#[derive(Clone, Debug)]
pub struct Device {
    pub path: OwnedObjectPath,
    pub name: String,
    pub paired: bool,
    pub connected: bool,
}

/// What the BLUETOOTH screen shows, updated by the manager thread
#[derive(Clone, Debug, Default)]
pub struct State {
    /// false when there is no powered adapter
    pub adapter: bool,
    /// paired devices first, then the gamepads found in pairing mode
    pub devices: Vec<Device>,
    /// name of the device being paired
    pub pairing: Option<String>,
    /// result of the last pairing or forget, shown for a while
    pub status: Option<String>,
}

enum Command {
    StartDiscovery,
    StopDiscovery,
    Pair(Device),
    Forget(Device),
}

/// Talks to BlueZ from a background thread, since pairing blocks for many seconds
pub struct Manager {
    commands: Sender<Command>,
    state: Arc<Mutex<State>>,
}

impl Manager {
    pub fn new() -> Self {
        let (commands, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(State::default()));
        let thread_state = state.clone();
        thread::spawn(move || manager_thread(rx, thread_state));
        Manager { commands, state }
    }

    /// Start discovering, while the screen is open
    pub fn open(&self) {
        let _ = self.commands.send(Command::StartDiscovery);
    }

    /// Stop discovering, which slows down the controllers already connected
    pub fn close(&self) {
        let _ = self.commands.send(Command::StopDiscovery);
    }

    pub fn pair(&self, device: &Device) {
        let _ = self.commands.send(Command::Pair(device.clone()));
    }

    pub fn forget(&self, device: &Device) {
        let _ = self.commands.send(Command::Forget(device.clone()));
    }

    pub fn state(&self) -> State {
        self.state.lock().map(|state| state.clone()).unwrap_or_default()
    }
}

fn manager_thread(rx: Receiver<Command>, state: Arc<Mutex<State>>) {
    let conn = match Connection::system() {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("Bluetooth: failed to connect to the system bus: {}", e);
            return;
        }
    };
    // BlueZ drops the agent when the bios exits
    if let Err(e) = register_agent(&conn) {
        eprintln!("Bluetooth: failed to register the pairing agent: {}", e);
    }
    let mut discovering = false;
    let mut status_since = Instant::now();
    let mut pair_attempts: HashMap<OwnedObjectPath, u32> = HashMap::new();
    let mut auto_pair: Option<Device> = None;
    loop {
        let command = match auto_pair.take() {
            Some(device) => Ok(Command::Pair(device)),
            None => rx.recv_timeout(REFRESH_INTERVAL),
        };
        match command {
            Ok(Command::StartDiscovery) => {
                discovering = true;
                pair_attempts.clear();
                set_discovery(&conn, true);
            }
            Ok(Command::StopDiscovery) => {
                discovering = false;
                set_discovery(&conn, false);
            }
            Ok(Command::Pair(device)) => {
                *pair_attempts.entry(device.path.clone()).or_insert(0) += 1;
                update(&state, |s| s.pairing = Some(device.name.clone()));
                // controllers often cancel the pairing while the adapter is still discovering
                set_discovery(&conn, false);
                let status = match pair(&conn, &device.path) {
                    Ok(()) => {
                        eprintln!("Bluetooth: paired {} ({})", device.name, device.path);
                        format!("{} PAIRED", device.name)
                    }
                    Err(e) => {
                        eprintln!("Bluetooth: failed to pair {}: {}", device.path, e);
                        format!("COULD NOT PAIR {}", device.name)
                    }
                };
                // the screen may have been closed while pairing
                discovering = discovering && still_open(&rx);
                if discovering {
                    set_discovery(&conn, true);
                }
                update(&state, |s| {
                    s.pairing = None;
                    s.status = Some(status);
                });
                status_since = Instant::now();
            }
            Ok(Command::Forget(device)) => {
                let status = match forget(&conn, &device.path) {
                    Ok(()) => format!("{} FORGOTTEN", device.name),
                    Err(e) => {
                        eprintln!("Bluetooth: failed to forget {}: {}", device.path, e);
                        format!("COULD NOT FORGET {}", device.name)
                    }
                };
                update(&state, |s| s.status = Some(status));
                status_since = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }

        let (adapter, devices) = match list_devices(&conn) {
            Ok(result) => result,
            Err(e) => {
                eprintln!("Bluetooth: failed to list devices: {}", e);
                (false, Vec::new())
            }
        };
        if discovering {
            auto_pair = devices
                .iter()
                .find(|d| !d.paired && pair_attempts.get(&d.path).copied().unwrap_or(0) < AUTO_PAIR_ATTEMPTS)
                .cloned();
        }
        let clear_status = status_since.elapsed() >= STATUS_DURATION;
        update(&state, |s| {
            s.adapter = adapter;
            s.devices = devices;
            if clear_status {
                s.status = None;
            }
        });
    }
}

/// Whether the screen is still open after the commands sent while pairing
fn still_open(rx: &Receiver<Command>) -> bool {
    let mut open = true;
    while let Ok(command) = rx.try_recv() {
        match command {
            Command::StartDiscovery => open = true,
            Command::StopDiscovery => open = false,
            // the screen offers no pairing or forgetting while pairing
            Command::Pair(_) | Command::Forget(_) => {}
        }
    }
    open
}

fn update(state: &Arc<Mutex<State>>, f: impl FnOnce(&mut State)) {
    if let Ok(mut state) = state.lock() {
        f(&mut state);
    }
}

fn set_discovery(conn: &Connection, on: bool) {
    let Ok(Some(adapter)) = find_adapter(conn) else { return };
    let Ok(adapter) = Proxy::new(conn, "org.bluez", adapter, "org.bluez.Adapter1") else { return };
    let method = if on { "StartDiscovery" } else { "StopDiscovery" };
    // fails harmlessly when discovery is already in that state
    if let Err(e) = adapter.call::<_, _, ()>(method, &()) {
        eprintln!("Bluetooth: {} failed: {}", method, e);
    }
}

fn managed_objects(conn: &Connection) -> zbus::Result<ManagedObjects> {
    let manager = Proxy::new(conn, "org.bluez", "/", "org.freedesktop.DBus.ObjectManager")?;
    manager.call("GetManagedObjects", &())
}

fn find_adapter(conn: &Connection) -> zbus::Result<Option<OwnedObjectPath>> {
    Ok(powered_adapter(&managed_objects(conn)?))
}

fn powered_adapter(objects: &ManagedObjects) -> Option<OwnedObjectPath> {
    let mut adapters: Vec<&OwnedObjectPath> = objects
        .iter()
        .filter(|(_, interfaces)| {
            interfaces
                .get("org.bluez.Adapter1")
                .and_then(|props| props.get("Powered"))
                .and_then(|v| bool::try_from(v).ok())
                .unwrap_or(false)
        })
        .map(|(path, _)| path)
        .collect();
    // hci0 first when there are several adapters
    adapters.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    adapters.first().map(|path| (*path).clone())
}

/// Whether there is a powered adapter, and its paired devices followed by the gamepads in
/// pairing mode
fn list_devices(conn: &Connection) -> zbus::Result<(bool, Vec<Device>)> {
    let objects = managed_objects(conn)?;
    let Some(adapter) = powered_adapter(&objects) else { return Ok((false, Vec::new())) };
    let mut devices: Vec<Device> = objects
        .into_iter()
        .filter_map(|(path, interfaces)| {
            let props = interfaces.get("org.bluez.Device1")?;
            let on_adapter = props
                .get("Adapter")
                .and_then(|v| ObjectPath::try_from(&**v).ok())
                .is_some_and(|p| p.as_str() == adapter.as_str());
            let flag = |name: &str| props.get(name).and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
            let paired = flag("Paired");
            // BlueZ keeps devices from earlier discoveries; only the ones found now have an RSSI
            let in_range = props.contains_key("RSSI");
            let shown = paired || (in_range && is_gamepad(props));
            let name = ["Alias", "Name"]
                .iter()
                .find_map(|key| props.get(*key).and_then(|v| <&str>::try_from(&**v).ok()))
                .unwrap_or("unnamed")
                .to_string();
            (on_adapter && shown).then(|| Device { path, name, paired, connected: flag("Connected") })
        })
        .collect();
    devices.sort_by(|a, b| b.paired.cmp(&a.paired).then_with(|| a.name.cmp(&b.name)));
    Ok((true, devices))
}

fn is_gamepad(props: &HashMap<String, OwnedValue>) -> bool {
    let class = props.get("Class").and_then(|v| u32::try_from(v).ok());
    let appearance = props.get("Appearance").and_then(|v| u16::try_from(v).ok());
    let class_matches = class.is_some_and(|class| {
        let major = (class >> 8) & 0x1f;
        let minor = (class >> 2) & 0x0f;
        major == COD_MAJOR_PERIPHERAL && (minor == COD_MINOR_JOYSTICK || minor == COD_MINOR_GAMEPAD)
    });
    let appearance_matches = appearance.is_some_and(|a| a == APPEARANCE_JOYSTICK || a == APPEARANCE_GAMEPAD);
    class_matches || appearance_matches
}

/// Pair and trust the device so it reconnects on its own, then connect it
fn pair(conn: &Connection, device: &OwnedObjectPath) -> zbus::Result<()> {
    let proxy = Proxy::new(conn, "org.bluez", device, "org.bluez.Device1")?;
    proxy.call::<_, _, ()>("Pair", &())?;
    proxy.set_property("Trusted", true)?;
    proxy.call::<_, _, ()>("Connect", &())
}

/// Remove the pairing; the device has to be put in pairing mode to be paired again
fn forget(conn: &Connection, device: &OwnedObjectPath) -> zbus::Result<()> {
    let adapter = find_adapter(conn)?.ok_or_else(|| zbus::Error::Failure("no powered adapter".into()))?;
    let adapter = Proxy::new(conn, "org.bluez", adapter, "org.bluez.Adapter1")?;
    let device = ObjectPath::try_from(device.as_str())?;
    adapter.call::<_, _, ()>("RemoveDevice", &(&device,))
}

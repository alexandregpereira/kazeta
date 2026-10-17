// Bluetooth controller pairing for Kazeta OS, run as root by kazeta-bluetooth.service.
// There is no UI, so pairing needs no confirmation:
// - an agent accepts every pairing and authorization request. BlueZ's sixaxis plugin asks it
//   before pairing a DualShock 3 or 4 plugged in over USB, which is then paired by the cable alone.
// - plugging a gamepad into USB, or starting with one plugged in, opens a pairing window: a
//   gamepad put into pairing mode (e.g. Create + PS on a DualSense) is paired, trusted and
//   connected on its own.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

/// Argument that runs the pairing service, see run
pub const BLUETOOTH_ARG: &str = "--bluetooth";

const AGENT_PATH: &str = "/org/kazeta/agent";
const AGENT_CAPABILITY: &str = "NoInputNoOutput";
const PIN_CODE: &str = "0000";
const REGISTER_ATTEMPTS: u32 = 20;
const REGISTER_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);
const PAIRING_WINDOW: Duration = Duration::from_secs(60);
const DISCOVERY_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// At boot the adapter may still be powering on when the window opens
const ADAPTER_WAIT: Duration = Duration::from_secs(10);
/// Pairing a controller sometimes fails right after it shows up, so it is tried a few times
const PAIR_ATTEMPTS: u32 = 3;

const BUS_USB: u16 = 0x03;
const BTN_GAMEPAD: usize = 0x130;
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

/// Register the agent and open a pairing window whenever a gamepad is plugged into USB
pub fn run() -> i32 {
    let conn = match Connection::system() {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("Bluetooth: failed to connect to the system bus: {}", e);
            return 1;
        }
    };
    if let Err(e) = register_agent(&conn) {
        eprintln!("Bluetooth: failed to register the pairing agent: {}", e);
        return 1;
    }
    eprintln!("Bluetooth: pairing agent registered");

    // gamepads already plugged in at startup open a window too, so starting with a wired
    // controller is enough to pair another one
    let mut seen = HashSet::new();
    loop {
        thread::sleep(RESCAN_INTERVAL);
        let current = usb_gamepads();
        if let Some(name) = current.difference(&seen).next() {
            eprintln!("Bluetooth: gamepad plugged into USB ({}), opening the pairing window", name);
            if let Err(e) = pairing_window(&conn) {
                eprintln!("Bluetooth: pairing window failed: {}", e);
            }
        }
        // devices plugged in during the window are taken as seen, they had the window open
        seen = usb_gamepads();
    }
}

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

/// Input device names (e.g. event5) of the gamepads connected over USB, read from sysfs so the
/// devices are not opened
fn usb_gamepads() -> HashSet<String> {
    let Ok(entries) = fs::read_dir("/sys/class/input") else { return HashSet::new() };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with("event"))
        .filter(|name| {
            let device = format!("/sys/class/input/{}/device", name);
            let bus = fs::read_to_string(format!("{}/id/bustype", device))
                .ok()
                .and_then(|s| u16::from_str_radix(s.trim(), 16).ok());
            let keys = fs::read_to_string(format!("{}/capabilities/key", device)).unwrap_or_default();
            bus == Some(BUS_USB) && has_key(&keys, BTN_GAMEPAD)
        })
        .collect()
}

/// Whether a sysfs key capability bitmap (hex words, most significant first) has the key code
fn has_key(bitmap: &str, code: usize) -> bool {
    let bits = usize::BITS as usize;
    let words: Vec<&str> = bitmap.split_whitespace().rev().collect();
    words
        .get(code / bits)
        .and_then(|word| usize::from_str_radix(word, 16).ok())
        .is_some_and(|word| word & (1 << (code % bits)) != 0)
}

/// Discover for a while and pair the first gamepad found in pairing mode
fn pairing_window(conn: &Connection) -> zbus::Result<()> {
    let Some(adapter) = wait_for_adapter(conn)? else {
        eprintln!("Bluetooth: no powered adapter, not pairing");
        return Ok(());
    };
    let adapter = Proxy::new(conn, "org.bluez", adapter, "org.bluez.Adapter1")?;
    adapter.call::<_, _, ()>("StartDiscovery", &())?;
    let paired = pair_discovered_gamepad(conn, &adapter);
    // discovery slows down the controllers already connected, so it is always stopped; it may
    // already be stopped by a pairing attempt
    let _ = adapter.call::<_, _, ()>("StopDiscovery", &());
    if !paired? {
        eprintln!("Bluetooth: pairing window closed, no gamepad paired");
    }
    Ok(())
}

/// Pair the first discovered gamepad that accepts it before the window closes
fn pair_discovered_gamepad(conn: &Connection, adapter: &Proxy<'_>) -> zbus::Result<bool> {
    let deadline = Instant::now() + PAIRING_WINDOW;
    let mut attempts: HashMap<OwnedObjectPath, u32> = HashMap::new();
    while Instant::now() < deadline {
        thread::sleep(DISCOVERY_POLL_INTERVAL);
        for (device, name) in unpaired_gamepads(conn, adapter.path())? {
            let attempt = attempts.entry(device.clone()).or_insert(0);
            if *attempt >= PAIR_ATTEMPTS {
                continue;
            }
            *attempt += 1;
            eprintln!("Bluetooth: pairing {} ({}), attempt {}", name, device, attempt);
            // controllers often cancel the pairing while the adapter is still discovering
            adapter.call::<_, _, ()>("StopDiscovery", &())?;
            match pair(conn, &device) {
                Ok(()) => {
                    eprintln!("Bluetooth: paired {} ({})", name, device);
                    return Ok(true);
                }
                Err(e) => eprintln!("Bluetooth: failed to pair {}: {}", device, e),
            }
            adapter.call::<_, _, ()>("StartDiscovery", &())?;
            break;
        }
    }
    Ok(false)
}

fn managed_objects(conn: &Connection) -> zbus::Result<ManagedObjects> {
    let manager = Proxy::new(conn, "org.bluez", "/", "org.freedesktop.DBus.ObjectManager")?;
    manager.call("GetManagedObjects", &())
}

fn wait_for_adapter(conn: &Connection) -> zbus::Result<Option<OwnedObjectPath>> {
    let deadline = Instant::now() + ADAPTER_WAIT;
    loop {
        let adapter = find_adapter(conn)?;
        if adapter.is_some() || Instant::now() >= deadline {
            return Ok(adapter);
        }
        thread::sleep(DISCOVERY_POLL_INTERVAL);
    }
}

fn find_adapter(conn: &Connection) -> zbus::Result<Option<OwnedObjectPath>> {
    let objects = managed_objects(conn)?;
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
    Ok(adapters.first().map(|path| (*path).clone()))
}

/// Gamepads seen by the current discovery that are not paired yet, with their names
fn unpaired_gamepads(conn: &Connection, adapter: &ObjectPath<'_>) -> zbus::Result<Vec<(OwnedObjectPath, String)>> {
    let objects = managed_objects(conn)?;
    Ok(objects
        .into_iter()
        .filter_map(|(path, interfaces)| {
            let props = interfaces.get("org.bluez.Device1")?;
            let on_adapter = props
                .get("Adapter")
                .and_then(|v| ObjectPath::try_from(&**v).ok())
                .is_some_and(|p| p == *adapter);
            let paired = props.get("Paired").and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
            // BlueZ keeps devices from earlier discoveries; only the ones found now have an RSSI
            let in_range = props.contains_key("RSSI");
            let name = props
                .get("Name")
                .and_then(|v| <&str>::try_from(&**v).ok())
                .unwrap_or("unnamed")
                .to_string();
            (on_adapter && !paired && in_range && is_gamepad(props)).then_some((path, name))
        })
        .collect())
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

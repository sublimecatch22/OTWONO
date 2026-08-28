//! Network interface probing, including the radio hardware relevant to the node mesh.

use super::{sysfs, SystemProbe};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterfaceKind {
    Loopback,
    Ethernet,
    Wireless,
    Bluetooth,
    /// 802.15.4 / 6LoWPAN
    Ieee802154,
    /// Point-to-point, tunnels, and the LoRa `lora`/`sx12xx` net devices
    PointToPoint,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkInterface {
    pub name: String,
    pub kind: InterfaceKind,
    /// Kernel `operstate`: `up`, `down`, `unknown`, ...
    pub operstate: String,
    pub speed_mbps: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkInfo {
    pub interfaces: Vec<NetworkInterface>,
    /// A default route exists. Not proof the Internet works, only that a path is claimed.
    pub has_default_route: bool,
    /// At least one non-loopback interface is up.
    pub has_link: bool,
    /// Radio hardware usable by the node mesh beyond ordinary Wi-Fi.
    pub mesh_radio_present: bool,
}

pub(crate) fn probe(p: &SystemProbe, warnings: &mut Vec<String>) -> NetworkInfo {
    let mut info = NetworkInfo::default();
    let base = p.path("/sys/class/net");
    let names = sysfs::list_dir(&base);
    if names.is_empty() {
        warnings.push("network: no interfaces found".to_string());
    }

    for name in names {
        let dev = base.join(&name);
        let arptype = sysfs::read_u64(&dev.join("type")).unwrap_or(0);
        let wireless = sysfs::exists(&dev.join("wireless")) || sysfs::exists(&dev.join("phy80211"));
        let kind = classify_interface(&name, arptype, wireless);
        let operstate = sysfs::read_trimmed(&dev.join("operstate")).unwrap_or_else(|| "unknown".into());
        // `speed` returns -1 or EINVAL on a down interface; read_u64 gives None, which is
        // what we want.
        let speed_mbps = sysfs::read_u64(&dev.join("speed")).and_then(|v| u32::try_from(v).ok());

        if kind != InterfaceKind::Loopback && operstate == "up" {
            info.has_link = true;
        }
        if matches!(kind, InterfaceKind::Ieee802154) || is_lora_name(&name) {
            info.mesh_radio_present = true;
        }
        info.interfaces.push(NetworkInterface {
            name,
            kind,
            operstate,
            speed_mbps,
        });
    }

    info.has_default_route = has_default_route(p);
    info
}

/// ARPHRD constants from `linux/if_arp.h`.
fn classify_interface(name: &str, arptype: u64, wireless: bool) -> InterfaceKind {
    if arptype == 772 {
        return InterfaceKind::Loopback;
    }
    if wireless {
        return InterfaceKind::Wireless;
    }
    match arptype {
        1 => InterfaceKind::Ethernet,     // ARPHRD_ETHER
        804 => InterfaceKind::Ieee802154, // ARPHRD_IEEE802154
        825 => InterfaceKind::Ieee802154, // ARPHRD_6LOWPAN
        512 | 768 => InterfaceKind::PointToPoint,
        _ if is_lora_name(name) => InterfaceKind::PointToPoint,
        _ if name.starts_with("bnep") || name.starts_with("hci") => InterfaceKind::Bluetooth,
        _ => InterfaceKind::Other,
    }
}

fn is_lora_name(name: &str) -> bool {
    name.starts_with("lora") || name.starts_with("sx12") || name.starts_with("rnode")
}

/// A default route is destination `00000000` in `/proc/net/route`.
fn has_default_route(p: &SystemProbe) -> bool {
    let Some(text) = sysfs::read_trimmed(&p.path("/proc/net/route")) else {
        return false;
    };
    text.lines().skip(1).any(|line| {
        let mut cols = line.split_whitespace();
        let _iface = cols.next();
        cols.next() == Some("00000000")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_arphrd() {
        assert_eq!(classify_interface("lo", 772, false), InterfaceKind::Loopback);
        assert_eq!(classify_interface("eth0", 1, false), InterfaceKind::Ethernet);
        assert_eq!(classify_interface("wlan0", 1, true), InterfaceKind::Wireless);
        assert_eq!(classify_interface("wpan0", 804, false), InterfaceKind::Ieee802154);
        assert_eq!(
            classify_interface("lowpan0", 825, false),
            InterfaceKind::Ieee802154
        );
    }

    #[test]
    fn wireless_beats_ether_arptype() {
        // Wi-Fi interfaces report ARPHRD_ETHER; the `wireless` directory is the real signal.
        assert_eq!(classify_interface("wlp3s0", 1, true), InterfaceKind::Wireless);
    }

    #[test]
    fn recognises_lora_device_names() {
        assert!(is_lora_name("lora0"));
        assert!(is_lora_name("sx1262"));
        assert!(is_lora_name("rnode0"));
        assert!(!is_lora_name("eth0"));
    }
}

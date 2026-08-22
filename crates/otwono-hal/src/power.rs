//! Power-supply and thermal probing.
//!
//! Power gates *sustained* workloads. A laptop on battery is not the machine it is on AC,
//! and a passively-cooled SBC running a large model until it throttles is a bad experience
//! rather than a feature.

use super::{sysfs, SystemProbe};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PowerInfo {
    pub has_battery: bool,
    /// A battery exists and no mains supply is online.
    pub on_battery: bool,
    pub mains_online: Option<bool>,
    pub thermal_zones: u32,
}

pub(crate) fn probe(p: &SystemProbe, _warnings: &mut Vec<String>) -> PowerInfo {
    let mut info = PowerInfo::default();
    let base = p.path("/sys/class/power_supply");
    let mut mains_seen = false;
    let mut mains_online = false;

    for name in sysfs::list_dir(&base) {
        let dev = base.join(&name);
        match sysfs::read_trimmed(&dev.join("type"))
            .unwrap_or_default()
            .as_str()
        {
            "Battery" => info.has_battery = true,
            "Mains" | "USB" | "USB_PD" => {
                mains_seen = true;
                if sysfs::read_u64(&dev.join("online")).unwrap_or(0) == 1 {
                    mains_online = true;
                }
            }
            _ => {}
        }
    }

    info.mains_online = mains_seen.then_some(mains_online);
    info.on_battery = info.has_battery && mains_seen && !mains_online;
    info.thermal_zones = sysfs::list_dir(&p.path("/sys/class/thermal"))
        .iter()
        .filter(|n| n.starts_with("thermal_zone"))
        .count() as u32;

    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_desktop_with_no_power_supply_class_is_not_on_battery() {
        // Most desktops and nearly every SBC expose no power_supply devices at all.
        let info = probe(&SystemProbe::from_root("/nonexistent"), &mut Vec::new());
        assert!(!info.has_battery);
        assert!(!info.on_battery);
        assert_eq!(info.mains_online, None);
    }
}

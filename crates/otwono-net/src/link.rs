//! The link abstraction.
//!
//! Every physical medium — Ethernet, Wi-Fi, LoRa, 802.15.4, packet radio, a USB gadget —
//! is wrapped in one interface. This is the boundary that stops LoRa support from
//! metastasizing through the rest of the codebase: the router and the services above never
//! learn what carries their bytes, only what that carrier can bear.
//!
//! The property that makes this more than a naming exercise is [`BandwidthClass`].
//! Attempting a 4 MB transfer over a duty-cycle-limited LoRa link is not slow, it is
//! illegal in most jurisdictions and it jams the channel for every other user. Enforcement
//! therefore lives here and in the router, not in each service's good intentions.

use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    Ethernet,
    Wifi,
    WifiDirect,
    Ble,
    LoRa,
    Ieee802154,
    Ax25,
    UsbGadget,
    /// A TCP/IP connection over whatever the host's normal stack provides.
    Internet,
    /// In-memory, for tests.
    Loopback,
}

/// What a link can carry, ordered from most to least constrained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandwidthClass {
    /// Under 1 KB/s: LoRa, AX.25. Text and tiny signed records only. Never bulk.
    Trickle,
    /// 1 KB/s to 1 MB/s: 802.15.4, BLE, poor Wi-Fi.
    Narrow,
    /// 1 to 100 MB/s: Wi-Fi, fast Ethernet.
    Broad,
    /// Over 100 MB/s.
    Wide,
}

impl BandwidthClass {
    /// Largest payload it is reasonable to push over this link in one go.
    ///
    /// A hard ceiling rather than a hint: a service that wants to send more must chunk it
    /// and accept that the router may refuse, or choose a different link.
    pub fn max_reasonable_payload(&self) -> usize {
        match self {
            BandwidthClass::Trickle => 256,
            BandwidthClass::Narrow => 64 * 1024,
            BandwidthClass::Broad => 8 * 1024 * 1024,
            BandwidthClass::Wide => 64 * 1024 * 1024,
        }
    }

    pub fn permits_bulk_transfer(&self) -> bool {
        *self >= BandwidthClass::Broad
    }
}

/// A regulatory duty-cycle limit, e.g. 1% in EU868.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DutyCycle {
    /// Fraction of wall time this link may transmit, in the range 0.0 to 1.0.
    pub fraction: f32,
    /// Window the fraction is measured over.
    pub window_seconds: u32,
}

impl DutyCycle {
    /// Airtime budget in the window. The router must not exceed this.
    pub fn budget(&self) -> Duration {
        Duration::from_secs_f32(self.window_seconds as f32 * self.fraction)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnergyCost {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkProperties {
    pub kind: LinkKind,
    pub mtu: usize,
    pub bandwidth_class: BandwidthClass,
    /// `None` means unrestricted; `Some` means the router must ration airtime.
    pub duty_cycle: Option<DutyCycle>,
    pub energy_cost: EnergyCost,
    pub broadcast_capable: bool,
    pub typical_latency_ms: u32,
}

impl LinkProperties {
    /// Ethernet or an IP connection: no constraints worth modelling.
    pub fn internet() -> Self {
        LinkProperties {
            kind: LinkKind::Internet,
            mtu: 1500,
            bandwidth_class: BandwidthClass::Broad,
            duty_cycle: None,
            energy_cost: EnergyCost::Low,
            broadcast_capable: false,
            typical_latency_ms: 20,
        }
    }

    /// EU868 LoRa: the constrained case every design decision here exists for.
    pub fn lora_eu868() -> Self {
        LinkProperties {
            kind: LinkKind::LoRa,
            mtu: 222,
            bandwidth_class: BandwidthClass::Trickle,
            duty_cycle: Some(DutyCycle {
                fraction: 0.01,
                window_seconds: 3600,
            }),
            energy_cost: EnergyCost::High,
            broadcast_capable: true,
            typical_latency_ms: 2000,
        }
    }

    /// Would sending this many bytes over this link be acceptable?
    pub fn permits_payload(&self, bytes: usize) -> Result<(), LinkError> {
        let ceiling = self.bandwidth_class.max_reasonable_payload();
        if bytes > ceiling {
            return Err(LinkError::PayloadTooLarge {
                bytes,
                ceiling,
                class: self.bandwidth_class,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    PayloadTooLarge {
        bytes: usize,
        ceiling: usize,
        class: BandwidthClass,
    },
    NotConnected,
    Io(String),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::PayloadTooLarge {
                bytes,
                ceiling,
                class,
            } => write!(
                f,
                "{bytes} bytes exceeds the {ceiling}-byte ceiling for a {class:?} link; \
                 chunk it or choose another link"
            ),
            LinkError::NotConnected => write!(f, "link is not connected"),
            LinkError::Io(e) => write!(f, "link I/O failed: {e}"),
        }
    }
}

impl std::error::Error for LinkError {}

/// A bidirectional, framed channel to one peer over one medium.
///
/// Deliberately message-oriented rather than a byte stream: every medium below this
/// interface except TCP is already packet-based, and pretending otherwise would mean
/// re-implementing framing for each of them.
/// `Send` but deliberately not `Sync`: a link is owned by one task at a time, and both
/// methods take `&mut self`. Requiring `Sync` would rule out perfectly good adapters —
/// anything built on a channel receiver, for one — to permit sharing the router never does.
pub trait LinkAdapter: Send {
    fn properties(&self) -> LinkProperties;
    /// Send one frame. Must reject anything the link cannot reasonably carry.
    fn send(&mut self, frame: &[u8]) -> Result<(), LinkError>;
    /// Receive one frame, blocking until it arrives or the link fails.
    fn recv(&mut self) -> Result<Vec<u8>, LinkError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bandwidth_classes_are_ordered_from_constrained_to_capable() {
        assert!(BandwidthClass::Trickle < BandwidthClass::Narrow);
        assert!(BandwidthClass::Narrow < BandwidthClass::Broad);
        assert!(BandwidthClass::Broad < BandwidthClass::Wide);
    }

    #[test]
    fn only_broad_and_wide_links_permit_bulk() {
        assert!(!BandwidthClass::Trickle.permits_bulk_transfer());
        assert!(!BandwidthClass::Narrow.permits_bulk_transfer());
        assert!(BandwidthClass::Broad.permits_bulk_transfer());
    }

    #[test]
    fn a_lora_link_refuses_an_image() {
        // The regulatory case: this must fail loudly rather than jam the band.
        let lora = LinkProperties::lora_eu868();
        let err = lora.permits_payload(4 * 1024 * 1024).unwrap_err();
        assert!(matches!(err, LinkError::PayloadTooLarge { .. }));
        assert!(err.to_string().contains("Trickle"), "{err}");
    }

    #[test]
    fn a_lora_link_accepts_a_short_text_message() {
        assert!(LinkProperties::lora_eu868().permits_payload(200).is_ok());
    }

    #[test]
    fn an_internet_link_accepts_what_lora_refuses() {
        assert!(LinkProperties::internet()
            .permits_payload(4 * 1024 * 1024)
            .is_ok());
    }

    #[test]
    fn eu868_duty_cycle_is_thirty_six_seconds_an_hour() {
        let dc = LinkProperties::lora_eu868().duty_cycle.unwrap();
        assert_eq!(dc.budget().as_secs(), 36);
    }

    #[test]
    fn link_properties_round_trip_as_json() {
        for p in [LinkProperties::internet(), LinkProperties::lora_eu868()] {
            let json = serde_json::to_string(&p).unwrap();
            assert_eq!(serde_json::from_str::<LinkProperties>(&json).unwrap(), p);
        }
    }
}

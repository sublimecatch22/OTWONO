//! An in-process link.
//!
//! Not a mock: it is a real [`LinkAdapter`] over a channel pair, useful for co-located
//! services and indispensable for testing the handshake without a network. Tests that
//! exercise transport logic against this run in microseconds and are deterministic, which
//! is what makes it practical to test the failure paths.

use crate::link::{BandwidthClass, EnergyCost, LinkAdapter, LinkError, LinkKind, LinkProperties};
use std::sync::mpsc::{Receiver, Sender};

pub struct MemoryLink {
    tx: Sender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
    properties: LinkProperties,
}

impl MemoryLink {
    /// Two ends of one link.
    pub fn pair() -> (MemoryLink, MemoryLink) {
        Self::pair_with(LinkProperties {
            kind: LinkKind::Loopback,
            mtu: 65535,
            bandwidth_class: BandwidthClass::Wide,
            duty_cycle: None,
            energy_cost: EnergyCost::Low,
            broadcast_capable: false,
            typical_latency_ms: 0,
        })
    }

    /// Two ends with borrowed properties, so a test can pretend to be LoRa.
    pub fn pair_with(properties: LinkProperties) -> (MemoryLink, MemoryLink) {
        let (tx_a, rx_b) = std::sync::mpsc::channel();
        let (tx_b, rx_a) = std::sync::mpsc::channel();
        (
            MemoryLink {
                tx: tx_a,
                rx: rx_a,
                properties: properties.clone(),
            },
            MemoryLink {
                tx: tx_b,
                rx: rx_b,
                properties,
            },
        )
    }
}

impl LinkAdapter for MemoryLink {
    fn properties(&self) -> LinkProperties {
        self.properties.clone()
    }

    fn send(&mut self, frame: &[u8]) -> Result<(), LinkError> {
        self.properties.permits_payload(frame.len())?;
        self.tx.send(frame.to_vec()).map_err(|_| LinkError::NotConnected)
    }

    fn recv(&mut self) -> Result<Vec<u8>, LinkError> {
        self.rx.recv().map_err(|_| LinkError::NotConnected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pair_carries_frames_both_ways() {
        let (mut a, mut b) = MemoryLink::pair();
        a.send(b"ping").unwrap();
        assert_eq!(b.recv().unwrap(), b"ping");
        b.send(b"pong").unwrap();
        assert_eq!(a.recv().unwrap(), b"pong");
    }

    #[test]
    fn a_dropped_end_reports_disconnection_rather_than_hanging() {
        let (mut a, b) = MemoryLink::pair();
        drop(b);
        assert_eq!(a.send(b"x"), Err(LinkError::NotConnected));
        assert_eq!(a.recv(), Err(LinkError::NotConnected));
    }

    #[test]
    fn the_adapter_enforces_its_own_bandwidth_class() {
        let (mut a, _b) = MemoryLink::pair_with(crate::link::LinkProperties::lora_eu868());
        assert!(matches!(
            a.send(&[0u8; 4096]),
            Err(LinkError::PayloadTooLarge { .. })
        ));
        assert!(a.send(&[0u8; 100]).is_ok());
    }
}

//! LAN discovery over mDNS/DNS-SD.
//!
//! Each device advertises `_ctrl._udp` with its device ID as the instance
//! name, and only two TXT records: `id` and `fp`, the fingerprint of its static
//! key. Device names, the OS and the user name are not broadcast. A device's
//! name is learned only over an encrypted session, after pairing.

use std::net::{IpAddr, SocketAddr};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::error::{CtrlError, Result};

pub(crate) const SERVICE_TYPE: &str = "_ctrl._udp.local.";

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Sighting {
    pub device_id: String,
    pub fingerprint: String,
    pub addresses: Vec<SocketAddr>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Seen {
    Found(Sighting),
    Lost(String),
}

pub(crate) struct Discovery {
    daemon: ServiceDaemon,
    fullname: String,
}

fn mdns_err(e: mdns_sd::Error) -> CtrlError {
    CtrlError::Network(format!("mDNS: {e}"))
}

impl Discovery {
    pub fn start(
        device_id: &str,
        fingerprint: &str,
        port: u16,
        on_event: impl Fn(Seen) + Send + 'static,
    ) -> Result<Self> {
        let daemon = ServiceDaemon::new().map_err(mdns_err)?;
        let host = format!("ctrl-{}.local.", &device_id[..device_id.len().min(16)]);
        let info =
            ServiceInfo::new(SERVICE_TYPE, device_id, &host, "", port, &[("id", device_id), ("fp", fingerprint)][..])
                .map_err(mdns_err)?
                .enable_addr_auto();
        let fullname = info.get_fullname().to_owned();
        daemon.register(info).map_err(mdns_err)?;

        let events = daemon.browse(SERVICE_TYPE).map_err(mdns_err)?;
        let own_id = device_id.to_owned();
        std::thread::Builder::new().name("ctrl-discovery".into()).spawn(move || {
            while let Ok(event) = events.recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let (Some(id), Some(fp)) = (info.get_property_val_str("id"), info.get_property_val_str("fp"))
                        else {
                            continue;
                        };
                        let instance = info.get_fullname().strip_suffix(&format!(".{SERVICE_TYPE}"));
                        if id == own_id || instance != Some(id) {
                            continue;
                        }
                        let mut addresses: Vec<SocketAddr> = info
                            .get_addresses_v4()
                            .into_iter()
                            .map(|ip| SocketAddr::new(IpAddr::V4(ip), info.get_port()))
                            .collect();
                        addresses.sort();
                        if addresses.is_empty() {
                            continue;
                        }
                        on_event(Seen::Found(Sighting {
                            device_id: id.to_owned(),
                            fingerprint: fp.to_owned(),
                            addresses,
                        }));
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        if let Some(id) = fullname.strip_suffix(&format!(".{SERVICE_TYPE}")) {
                            if id != own_id {
                                on_event(Seen::Lost(id.to_owned()));
                            }
                        }
                    }
                    ServiceEvent::SearchStopped(_) => break,
                    _ => {}
                }
            }
        })?;

        Ok(Self { daemon, fullname })
    }

    pub fn stop(&self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.stop_browse(SERVICE_TYPE);
        let _ = self.daemon.shutdown();
    }
}

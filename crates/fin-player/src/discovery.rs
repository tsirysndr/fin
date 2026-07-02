use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use tokio::time::timeout;

const CAST_SERVICE: &str = "_googlecast._tcp.local.";

#[derive(Debug, Clone)]
pub struct CastDevice {
    pub name: String,
    pub model: String,
    pub address: IpAddr,
    pub port: u16,
    pub uuid: String,
}

impl CastDevice {
    pub fn display_name(&self) -> String {
        if self.name.is_empty() {
            self.model.clone()
        } else {
            self.name.clone()
        }
    }
}

/// Browse the local network for Chromecast-compatible receivers.
pub async fn discover_chromecasts(scan_for: Duration) -> Result<Vec<CastDevice>> {
    let mdns = ServiceDaemon::new()?;
    let receiver = mdns.browse(CAST_SERVICE)?;
    let mut devices: HashMap<String, CastDevice> = HashMap::new();

    let deadline = tokio::time::Instant::now() + scan_for;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .min(Duration::from_millis(500));
        let ev = timeout(remaining, async {
            loop {
                match receiver.recv_async().await {
                    Ok(ev) => return Some(ev),
                    Err(_) => return None,
                }
            }
        })
        .await;
        let ev = match ev {
            Ok(Some(ev)) => ev,
            _ => continue,
        };
        if let ServiceEvent::ServiceResolved(info) = ev {
            let props: HashMap<String, String> = info
                .get_properties()
                .iter()
                .map(|p| (p.key().to_string(), p.val_str().to_string()))
                .collect();
            let friendly = props
                .get("fn")
                .cloned()
                .unwrap_or_else(|| info.get_hostname().to_string());
            let model = props.get("md").cloned().unwrap_or_default();
            let uuid = props
                .get("id")
                .cloned()
                .unwrap_or_else(|| info.get_fullname().to_string());
            let Some(addr) = info.get_addresses().iter().next().copied() else {
                continue;
            };
            let port = info.get_port();
            devices.insert(
                uuid.clone(),
                CastDevice {
                    name: friendly,
                    model,
                    address: addr,
                    port,
                    uuid,
                },
            );
        }
    }
    let _ = mdns.shutdown();
    let mut list: Vec<_> = devices.into_values().collect();
    list.sort_by(|a, b| a.display_name().cmp(&b.display_name()));
    Ok(list)
}

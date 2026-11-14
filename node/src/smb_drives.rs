//! One private SMB3 server per granted Storage route. Only the mesh tunnel
//! reaches it; no OS accounts, Windows shares, firewall rules, or LAN listener.
use allmystuff_session::SmbEndpoint;
use parking_lot::Mutex;
use smb_server::{Access, LocalFsBackend, Share, SmbServer};
use std::{collections::HashMap, path::Path};

pub(crate) const FEATURE: &str = "smb-drives-v1";

struct Host {
    endpoint: SmbEndpoint,
    shutdown: smb_server::ShutdownHandle,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Host {
    fn drop(&mut self) {
        self.shutdown.shutdown();
        self.task.abort();
    }
}

#[derive(Default)]
pub(crate) struct SmbHosts {
    hosts: Mutex<HashMap<String, Host>>,
    startup: tokio::sync::Mutex<()>,
}

impl SmbHosts {
    pub(crate) fn port(&self, route: &str) -> Option<u16> {
        self.hosts.lock().get(route).map(|host| host.endpoint.port)
    }
    pub(crate) fn stop(&self, route: &str) {
        self.hosts.lock().remove(route);
    }

    pub(crate) async fn start(&self, route: &str, root: &Path) -> Result<SmbEndpoint, String> {
        let _guard = self.startup.lock().await;
        if let Some(host) = self.hosts.lock().get(route) {
            return Ok(host.endpoint.clone());
        }
        let mut random = [0u8; 40];
        getrandom::getrandom(&mut random).map_err(|error| error.to_string())?;
        let password = random[..32]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let share = format!(
            "ams{}",
            random[32..]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let username = "ams".to_string();
        // cap-std confines all opens (including symlinks) to this root. Disable
        // SMB leases: applications can also modify the source outside SMB.
        let backend =
            LocalFsBackend::new(root).map_err(|error| format!("open SMB share root: {error}"))?;
        let server = SmbServer::builder()
            .listen("127.0.0.1:0".parse().unwrap())
            .user(&username, &password)
            .require_signing(true)
            .encrypt_data(true)
            .share(
                Share::new(&share, backend)
                    .user(&username, Access::ReadWrite)
                    .leases(false)
                    .encrypt_data(),
            )
            .build()
            .map_err(|error| format!("create SMB share: {error}"))?;
        let address = server
            .bind()
            .await
            .map_err(|error| format!("bind private SMB share: {error}"))?;
        let endpoint = SmbEndpoint {
            port: address.port(),
            share,
            username,
            password,
        };
        let shutdown = server.shutdown_handle();
        let task = tokio::spawn(async move {
            if let Err(error) = server.serve().await {
                tracing::warn!(%error, "private SMB server stopped");
            }
        });
        self.hosts.lock().insert(
            route.to_string(),
            Host {
                endpoint: endpoint.clone(),
                shutdown,
                task,
            },
        );
        Ok(endpoint)
    }
}

/// Separate loopback identities avoid Windows' per-server credential collision
/// between simultaneous mappings. No network adapter or host-file entry needed.
#[cfg(any(windows, test))]
pub(crate) fn client_address(mapping: &str) -> std::net::Ipv4Addr {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(mapping.as_bytes());
    std::net::Ipv4Addr::new(127, 64 + (hash[0] & 63), hash[1], 1 + hash[2] % 254)
}

#[cfg(windows)]
pub(crate) async fn client_listener(mapping: &str) -> Result<tokio::net::TcpListener, String> {
    tokio::net::TcpListener::bind((client_address(mapping), 0))
        .await
        .map_err(|error| format!("bind private SMB tunnel: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn loopback_servers_have_distinct_stable_client_identities() {
        assert_eq!(client_address("one"), client_address("one"));
        assert_ne!(client_address("one"), client_address("two"));
        assert!(client_address("one").is_loopback());
    }

    #[tokio::test]
    async fn private_share_negotiates_smb3_signing_and_releases_its_listener() {
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir(&self.0);
            }
        }
        let root = std::env::temp_dir().join(format!(
            "ams-smb-unit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let _cleanup = Cleanup(root.clone());
        let hosts = SmbHosts::default();
        let endpoint = hosts.start("granted-route", &root).await.unwrap();
        assert_eq!(hosts.start("granted-route", &root).await.unwrap(), endpoint);
        let other = hosts.start("other-route", &root).await.unwrap();
        assert_ne!(other.password, endpoint.password);
        assert_ne!(other.port, endpoint.port);
        assert_eq!(hosts.port("ungranted-route"), None);
        assert!(!format!("{endpoint:?}").contains(&endpoint.password));
        let exchange = async {
            let mut socket =
                tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, endpoint.port))
                    .await
                    .unwrap();
            // SMB2 NEGOTIATE: one SMB 3.0.2 dialect with encryption support.
            let mut request = vec![0u8; 64 + 38];
            request[..4].copy_from_slice(b"\xfeSMB");
            request[4..6].copy_from_slice(&64u16.to_le_bytes());
            request[6..8].copy_from_slice(&1u16.to_le_bytes());
            request[14..16].copy_from_slice(&1u16.to_le_bytes());
            request[24..32].copy_from_slice(&0u64.to_le_bytes());
            request[64..66].copy_from_slice(&36u16.to_le_bytes());
            request[66..68].copy_from_slice(&1u16.to_le_bytes());
            request[68..70].copy_from_slice(&1u16.to_le_bytes());
            request[72..76].copy_from_slice(&0x40u32.to_le_bytes());
            request[76..92].fill(1);
            request[100..102].copy_from_slice(&0x0302u16.to_le_bytes());
            socket
                .write_all(&(request.len() as u32).to_be_bytes())
                .await
                .unwrap();
            socket.write_all(&request).await.unwrap();
            let length = socket.read_u32().await.unwrap() as usize;
            assert!((128..65536).contains(&length));
            let mut reply = vec![0; length];
            socket.read_exact(&mut reply).await.unwrap();
            assert_eq!(&reply[..4], b"\xfeSMB");
            assert_eq!(&reply[8..12], &[0, 0, 0, 0]);
            assert_eq!(
                u16::from_le_bytes(reply[68..70].try_into().unwrap()),
                0x0302
            );
            assert_ne!(
                u16::from_le_bytes(reply[66..68].try_into().unwrap()) & 2,
                0,
                "SMB signing is required"
            );
            assert_ne!(
                u32::from_le_bytes(reply[88..92].try_into().unwrap()) & 0x40,
                0,
                "SMB encryption is supported"
            );
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), exchange)
            .await
            .unwrap();
        hosts.stop("granted-route");
        assert_eq!(hosts.port("granted-route"), None);
        tokio::task::yield_now().await;
        let listener =
            tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, endpoint.port))
                .await
                .unwrap();
        drop(listener);
        hosts.stop("other-route");
        drop(hosts);
        tokio::task::yield_now().await;
        std::fs::remove_dir(&root).unwrap();
    }
}

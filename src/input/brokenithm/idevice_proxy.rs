//! Utility for forwarding a TCP port from an iOS device to localhost using libimobiledevice (idevice crate)

use nix::libc;
use std::process::Stdio;
use tokio::process::Command;

// TODO: Port this natively use `idevice`
use idevice::{
    Idevice,
    usbmuxd::{UsbmuxdConnection, UsbmuxdDevice},
};
use tokio::time::{Duration, timeout};

/// Spawns an `iproxy` process to forward a device port to a local port.
/// Returns the child process handle. The caller is responsible for keeping it alive.
pub async fn spawn_iproxy(
    local_port: u16,
    device_port: u16,
    udid: Option<&str>,
) -> std::io::Result<tokio::process::Child> {
    let mut cmd = Command::new("iproxy");
    cmd.arg(local_port.to_string()).arg(device_port.to_string());
    if let Some(udid) = udid {
        cmd.arg("--udid").arg(udid);
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    match cmd.spawn() {
        Ok(child) => Ok(child),
        Err(e) => {
            if let Some(raw_os_error) = e.raw_os_error() {
                if raw_os_error == libc::ENOENT {
                    tracing::error!(
                        "Failed to spawn 'iproxy': command not found. Is usbmuxd-utils installed?"
                    );
                }
            }
            Err(e)
        }
    }
}

/// Native re-implementation of `iproxy` functionality.
pub struct IproxyManager {
    pub device: UsbmuxdDevice,
    pub idevice: Option<Idevice>,
}

impl IproxyManager {
    /// Creates a new IproxyManager for the given device.
    pub fn new(device: UsbmuxdDevice, idevice: Idevice) -> Self {
        Self {
            device,
            idevice: Some(idevice),
        }
    }

    /// Moves the idevice out of the manager, leaving None in its place
    pub fn take_idevice(&mut self) -> Option<Idevice> {
        self.idevice.take()
    }
    /// Connects to the first available device
    #[tracing::instrument(skip_all)]
    pub async fn get_default_connection(
        port: u16,
        label: &str,
    ) -> Result<Self, String> {
        let mut conn = UsbmuxdConnection::default().await.map_err(|e| e.to_string())?;
        let devices = conn.get_devices().await.map_err(|e| e.to_string())?;

        if devices.is_empty() {
            return Err("No devices found".to_string());
        }

        // Use the first device
        let device = devices.first().cloned().ok_or_else(|| "No devices found".to_string())?;
        let idevice = conn
            .connect_to_device(device.device_id, port, label)
            .await
            .map_err(|e| e.to_string())?;

        Ok(Self {
            device,
            idevice: Some(idevice),
        })
    }

    pub async fn connect_with_usbmuxd_connection(
        port: u16,
        label: &str,
        mut usbmuxd_connection: UsbmuxdConnection,
        device: UsbmuxdDevice,
    ) -> Result<Self, String> {
        let devices = usbmuxd_connection.get_devices().await.map_err(|e| e.to_string())?;

        if devices.is_empty() {
            return Err("No devices found".to_string());
        }
        let idevice = usbmuxd_connection
            .connect_to_device(device.device_id, port, label)
            .await
            .map_err(|e| e.to_string())?;

        Ok(Self {
            device,
            idevice: Some(idevice),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_spawn_iproxy() {
        // This test is just a placeholder to ensure the function compiles.
        // Actual testing would require a device and iproxy setup.
        let mut conn = UsbmuxdConnection::default().await.unwrap();
        let devs = conn.get_devices().await.unwrap();

        for dev in &devs {
            println!("Found device: {:?}", dev);
        }

        // get the first device's UDID
        let dev = devs.first().expect("No devices found");

        println!("Using UDID: {:?}", dev);

        let mut forward = conn
            .connect_to_device(dev.device_id, 24864, "label")
            .await
            .unwrap();

        // Try to read from the device with a timeout, read up to 5 packets for testing
        let mut packet_count = 0;
        let max_packets = 5;

        loop {
            match timeout(Duration::from_secs(5), forward.read_raw(1024)).await {
                Ok(Ok(data)) => {
                    packet_count += 1;
                    eprintln!(
                        "Packet {}: Read {} bytes from device: {:?}",
                        packet_count,
                        data.len(),
                        data
                    );

                    if packet_count >= max_packets {
                        println!("Successfully read {} packets, test complete", max_packets);
                        break;
                    }
                }
                Ok(Err(e)) => {
                    println!("Error reading from device: {:?}", e);
                    break;
                }
                Err(_) => {
                    println!(
                        "Timeout reached while reading from device (read {} packets)",
                        packet_count
                    );
                    break;
                }
            }
        }
    }
}

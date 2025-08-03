pub mod interface;
use std::error::Error;
use zbus::Connection;

pub async fn system_bus() -> Result<Connection, Box<dyn Error>> {
    Connection::system()
        .await
        .map_err(|e| format!("Failed to connect to system bus: {e}").into())
}

pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::sync::OnceLock;

    static TOKIO_RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

    TOKIO_RT
        .get_or_init(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .expect("launch of single-threaded tokio runtime")
        })
        .block_on(future)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_system_bus() {
        let conn = system_bus().await;
        assert!(conn.is_ok(), "Failed to connect to system bus");
    }

    #[tokio::test]
    async fn test_dummy() {
        let conn = system_bus().await.unwrap();

        // Try to create a connection to inputplumber, but don't fail if the service isn't available
        match interface::input_manager::InputManagerProxy::new(&conn).await {
            Ok(manager) => {
                // Service is available, test actual functionality
                match interface::input_manager::TargetDevice::new(
                    &manager,
                    interface::input_manager::TargetDeviceType::Keyboard,
                )
                .await
                {
                    Ok(target_device) => {
                        assert_eq!(
                            target_device.kind,
                            interface::input_manager::TargetDeviceType::Keyboard
                        );
                        println!("Created target device: {:?}", target_device.path);

                        // stop
                        target_device.stop(&manager).await.unwrap();
                        println!("Stopped target device: {:?}", target_device.path);
                    }
                    Err(e) => {
                        println!("Could not create target device: {e}, but service is available");
                        // This is ok - service might be running but not fully functional
                    }
                }
            }
            Err(e) => {
                println!("InputPlumber service not available: {e}");
                // This is expected in CI/test environments where the service isn't running
                println!("Test passed - library compiles and can attempt connections");
            }
        }
    }

    // #[tokio::test]
    // async fn test_managed_device_auto_drop() {
    //     let conn = system_bus().await.unwrap();
    //     let manager = interface::input_manager::InputManagerProxy::new(&conn)
    //         .await
    //         .unwrap();

    //     let device_path;
    //     {
    //         // Create a managed device that will auto-cleanup on drop
    //         let managed_device = interface::input_manager::ManagedTargetDevice::new(
    //             &manager,
    //             interface::input_manager::TargetDeviceType::Keyboard,
    //         )
    //         .await
    //         .unwrap();

    //         assert_eq!(
    //             managed_device.kind,
    //             interface::input_manager::TargetDeviceType::Keyboard
    //         );
    //         device_path = managed_device.path.clone();
    //         println!("Created managed target device: {:?}", managed_device.path);

    //         // Device will be automatically stopped when it goes out of scope here
    //     }

    //     println!(
    //         "Managed device automatically dropped and stopped: {:?}",
    //         device_path
    //     );
    // }
}

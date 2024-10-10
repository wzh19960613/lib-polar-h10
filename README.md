# Lib Polar H10

## Usage

```rust
// lib-polar-h10 = { "version" = "0.1.0", features = ["tokio-rt"] }
// Or
// lib-polar-h10 = "0.1.0"
// tokio = { "version" = "1.40.0", features = ["rt-multi-thread", "macros"] }

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lib_polar_h10::{ PolarH10Error, PolarH10Helper }; 
use tokio::time;

#[tokio::main]
async fn main() -> Result<(), PolarH10Error> {
    let helper = PolarH10Helper::new().await?;

    fn on_hr_update(hr: u8) {
        println!("Heart Rate: {} bpm", hr);
    }

    fn on_battery_update(battery: u8) {
        println!("Battery: {}%", battery);
    }

    helper.on_hr_update(on_hr_update);
    helper.on_battery_update(on_battery_update);

    let scanned_devices = Arc::new(Mutex::new(Vec::new()));
    let scanned_devices_clone = Arc::clone(&scanned_devices);

    helper.on_device_discovered(move |serial| {
        println!("Found Device: {}", serial);
        scanned_devices_clone.lock().unwrap().push(serial);
    });

    helper.start_scan().await?;
    println!("Start Scan...");
    time::sleep(Duration::from_secs(10)).await;

    helper.stop_scan().await?;
    println!("Stop Scan.");

    let devices = scanned_devices.lock().unwrap();
    if let Some(serial) = devices.first() {
        println!("Try to connect: {}", serial);
        helper.connect_device(serial).await?;
        println!("Device Connected.");
        helper.start_hr_measurement().await?;
        println!("Start to measure heart rate...");
        time::sleep(Duration::from_secs(5)).await;

        helper.stop_hr_measurement().await?;
        println!("Stop measuring heart rate.");
    } else {
        println!("No device found.");
    }

    Ok(())
}
```
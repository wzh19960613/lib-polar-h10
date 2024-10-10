use std::collections::{BTreeSet, HashMap};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral as Device, PeripheralId as DeviceId};
use futures::stream::StreamExt;
use tokio::time;
use uuid::Uuid;

const HR_CHARACTERISTIC_UUID: Uuid = Uuid::from_u128(0x00002A3700001000800000805F9B34FB);
const BATTERY_CHARACTERISTIC_UUID: Uuid = Uuid::from_u128(0x00002A1900001000800000805F9B34FB);
const HEART_RATE_SERVICE_UUID: Uuid = Uuid::from_u128(0x0000180D00001000800000805F9B34FB);
const POLAR_MANUFACTURER_ID: u16 = 0x006B;

#[derive(Clone)]
enum ScanedItem {
    PolarH10(String),
    MaybePolarH10,
    Other,
}

type Callbacks<T> = Arc<Mutex<Vec<Box<dyn Fn(T) + Send + 'static>>>>;
type ScanedList = Arc<Mutex<HashMap<DeviceId, ScanedItem>>>;

pub struct PolarH10Helper {
    adapter: Adapter,
    device_callbacks: Callbacks<String>,
    battery_callbacks: Callbacks<u8>,
    hr_callbacks: Callbacks<u8>,
    connected_device: Arc<Mutex<Option<Device>>>,
    polling_thread_exists: Arc<AtomicBool>,
    should_continue_polling: Arc<AtomicBool>,
    scaned: ScanedList,
}

#[derive(Debug)]
pub enum PolarH10Error {
    BLEError(btleplug::Error),
    NoAdapterFound,
    NoDeviceFound,
    CharacteristicNotFound,
    DeviceNotConnected,
    NotPolarH10,
}

impl PolarH10Helper {
    pub async fn new() -> Result<Self, PolarH10Error> {
        let manager = ble(Manager::new()).await?;
        let adapters = ble(manager.adapters()).await?;
        let adapter = adapters
            .into_iter()
            .next()
            .ok_or(PolarH10Error::NoAdapterFound)?;

        Ok(Self {
            adapter,
            device_callbacks: Arc::new(Mutex::new(Vec::new())),
            battery_callbacks: Arc::new(Mutex::new(Vec::new())),
            hr_callbacks: Arc::new(Mutex::new(Vec::new())),
            connected_device: Arc::new(Mutex::new(None)),
            polling_thread_exists: Arc::new(AtomicBool::new(false)),
            should_continue_polling: Arc::new(AtomicBool::new(false)),
            scaned: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn start_scan(&self) -> Result<(), PolarH10Error> {
        ble(self.adapter.start_scan(ScanFilter::default())).await?;
        self.should_continue_polling.store(true, Ordering::SeqCst);

        if !self.polling_thread_exists.load(Ordering::SeqCst) {
            self.polling_thread_exists.store(true, Ordering::SeqCst);
            let adapter = self.adapter.clone();
            let device_callbacks = self.device_callbacks.clone();
            let should_continue_polling = self.should_continue_polling.clone();
            let polling_thread_exists = self.polling_thread_exists.clone();
            {
                self.scaned.clone().lock().unwrap().clear();
            }
            let scaned = self.scaned.clone();

            tokio::spawn(async move {
                while should_continue_polling.load(Ordering::SeqCst) {
                    if let Ok(peripherals) = adapter.peripherals().await {
                        let mut changed_items = HashMap::new();
                        for peripheral in peripherals {
                            if let Some((id, item)) = process_peripheral(
                                &peripheral,
                                &scaned,
                                &device_callbacks,
                                &should_continue_polling,
                            )
                            .await
                            {
                                changed_items.insert(id, item);
                            }
                        }
                        let mut scaned = scaned.lock().unwrap();
                        scaned.extend(changed_items);
                    }
                    time::sleep(Duration::from_secs(2)).await;
                }
                polling_thread_exists.store(false, Ordering::SeqCst);
            });
        }
        Ok(())
    }

    pub async fn stop_scan(&self) -> Result<(), PolarH10Error> {
        self.should_continue_polling.store(false, Ordering::SeqCst);
        {
            self.device_callbacks.lock().unwrap().clear();
        }
        ble(self.adapter.stop_scan()).await
    }

    pub fn on_device_discovered<F: Fn(String) + Send + 'static>(&self, callback: F) {
        self.device_callbacks
            .lock()
            .unwrap()
            .push(Box::new(callback));
    }

    pub async fn connect_device(&self, serial_number: &str) -> Result<(), PolarH10Error> {
        let id = {
            let scaned = self.scaned.lock().unwrap();
            scaned
                .iter()
                .find(|(_, item)| match item {
                    ScanedItem::PolarH10(sn) => sn == serial_number,
                    _ => false,
                })
                .map(|(id, _)| id.clone())
                .ok_or(PolarH10Error::NoDeviceFound)
        }?;
        let devices = ble(self.adapter.peripherals()).await?;
        let device = devices
            .into_iter()
            .find(|p| p.id() == id)
            .ok_or(PolarH10Error::NoDeviceFound)?;
        ble(device.connect()).await?;
        ble(device.discover_services()).await?;
        *self.connected_device.lock().unwrap() = Some(device.clone());
        self.stop_scan().await?;
        self.setup_notifications(device).await?;
        Ok(())
    }

    pub async fn disconnect(&self) -> Result<(), PolarH10Error> {
        let device = { self.connected_device.lock().unwrap().clone() };
        if let Some(device) = device {
            self.hr_callbacks.lock().unwrap().clear();
            self.battery_callbacks.lock().unwrap().clear();
            ble(device.disconnect()).await?
        }
        Ok(())
    }

    pub fn on_battery_update<F: Fn(u8) + Send + 'static>(&self, callback: F) {
        self.battery_callbacks
            .lock()
            .unwrap()
            .push(Box::new(callback));
    }

    pub fn on_hr_update<F: Fn(u8) + Send + 'static>(&self, callback: F) {
        self.hr_callbacks.lock().unwrap().push(Box::new(callback));
    }

    pub async fn start_hr_measurement(&self) -> Result<(), PolarH10Error> {
        self.write_hr_measurement(0x01).await
    }

    pub async fn stop_hr_measurement(&self) -> Result<(), PolarH10Error> {
        self.write_hr_measurement(0x00).await
    }

    async fn write_hr_measurement(&self, value: u8) -> Result<(), PolarH10Error> {
        let device = { self.connected_device.lock().unwrap().clone() };
        if let Some(device) = device {
            let chars = &device.characteristics();
            let hr_char = get_characteristic(chars, HR_CHARACTERISTIC_UUID)?;
            ble(device.write(hr_char, &[value], WriteType::WithoutResponse)).await?;
        } else {
            return Err(PolarH10Error::DeviceNotConnected);
        }
        Ok(())
    }

    async fn setup_notifications(&self, device: Device) -> Result<(), PolarH10Error> {
        let chars = &device.characteristics();
        let hr_char = get_characteristic(chars, HR_CHARACTERISTIC_UUID)?;
        let battery_char = get_characteristic(chars, BATTERY_CHARACTERISTIC_UUID)?;
        ble(device.subscribe(hr_char)).await?;
        ble(device.subscribe(battery_char)).await?;
        let hr_callbacks = self.hr_callbacks.clone();
        let battery_callbacks = self.battery_callbacks.clone();
        let mut notification_stream = ble(device.notifications()).await?;
        tokio::spawn(async move {
            while let Some(notification) = notification_stream.next().await {
                if notification.uuid == HR_CHARACTERISTIC_UUID {
                    let hr_value = notification.value[1];
                    for callback in hr_callbacks.lock().unwrap().iter() {
                        callback(hr_value);
                    }
                } else if notification.uuid == BATTERY_CHARACTERISTIC_UUID {
                    let battery_level = notification.value[0];
                    emit_battery_level(&battery_callbacks, battery_level);
                }
            }
        });
        let battery_level = ble(device.read(battery_char)).await?[0];
        emit_battery_level(&self.battery_callbacks, battery_level);
        Ok(())
    }
}

async fn ble<T>(
    future: impl std::future::Future<Output = Result<T, btleplug::Error>>,
) -> Result<T, PolarH10Error> {
    future.await.map_err(PolarH10Error::BLEError)
}

fn emit_battery_level(callbacks: &Callbacks<u8>, battery_level: u8) {
    for callback in callbacks.lock().unwrap().iter() {
        callback(battery_level);
    }
}

fn may_be_polar_h10(props: &btleplug::api::PeripheralProperties) -> bool {
    return props.manufacturer_data.contains_key(&POLAR_MANUFACTURER_ID)
        && props.services.contains(&HEART_RATE_SERVICE_UUID);
}

fn get_characteristic<'a>(
    chars: &'a BTreeSet<Characteristic>,
    uuid: Uuid,
) -> Result<&'a Characteristic, PolarH10Error> {
    chars
        .iter()
        .find(|c| c.uuid == uuid)
        .ok_or(PolarH10Error::CharacteristicNotFound)
}

async fn process_peripheral(
    device: &Device,
    scaned: &ScanedList,
    device_callbacks: &Callbacks<String>,
    should_continue_polling: &Arc<AtomicBool>,
) -> Option<(DeviceId, ScanedItem)> {
    let id = device.id();
    let new_scaned = scaned.lock().unwrap().clone();

    match new_scaned.get(&id) {
        Some(ScanedItem::MaybePolarH10) => {
            process_maybe_polar_h10(device, id, device_callbacks, should_continue_polling).await
        }
        None => process_new_peripheral(device, id).await,
        _ => None,
    }
}

async fn process_maybe_polar_h10(
    device: &Device,
    id: DeviceId,
    device_callbacks: &Callbacks<String>,
    should_continue_polling: &Arc<AtomicBool>,
) -> Option<(DeviceId, ScanedItem)> {
    let props = device.properties().await.ok()??;
    let name = props.local_name?;
    let sn = name.strip_prefix("Polar H10 ")?;

    if should_continue_polling.load(Ordering::SeqCst) {
        for callback in device_callbacks.lock().unwrap().iter() {
            callback(sn.to_string());
        }
    }

    Some((id, ScanedItem::PolarH10(sn.to_string())))
}

async fn process_new_peripheral(device: &Device, id: DeviceId) -> Option<(DeviceId, ScanedItem)> {
    let props = device.properties().await.ok()??;
    let item = if may_be_polar_h10(&props) {
        ScanedItem::MaybePolarH10
    } else {
        ScanedItem::Other
    };
    Some((id, item))
}

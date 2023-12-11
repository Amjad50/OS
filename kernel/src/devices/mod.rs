use core::fmt;

use alloc::{collections::BTreeMap, string::String, sync::Arc, vec::Vec};

use crate::{
    fs::{self, FileAttributes, FileSystem, FileSystemError, INode},
    sync::{once::OnceLock, spin::mutex::Mutex},
};

use self::pci::{PciDeviceConfig, PciDevicePropeIterator};

pub mod clock;
pub mod ide;
pub mod pci;

// TODO: replace with rwlock
static DEVICES: OnceLock<Arc<Mutex<Devices>>> = OnceLock::new();

const DEVICES_FILESYSTEM_CLUSTER_MAGIC: u32 = 0xdef1ce5;

#[derive(Debug)]
struct Devices {
    devices: BTreeMap<String, Arc<dyn Device>>,
}

pub trait Device: Sync + Send + fmt::Debug {
    fn name(&self) -> &str;
    fn read(&self, offset: u32, buf: &mut [u8]) -> Result<u64, FileSystemError>;
}

impl FileSystem for Mutex<Devices> {
    fn open_dir(&self, path: &str) -> Result<Vec<INode>, FileSystemError> {
        if path == "/" {
            Ok(self
                .lock()
                .devices
                .iter()
                .map(|(name, device)| {
                    INode::new_device(
                        name.clone(),
                        FileAttributes::EMPTY,
                        DEVICES_FILESYSTEM_CLUSTER_MAGIC,
                        0,
                        Some(device.clone()),
                    )
                })
                .collect())
        } else {
            Err(FileSystemError::FileNotFound)
        }
    }

    fn read_dir(&self, inode: &INode) -> Result<Vec<INode>, FileSystemError> {
        assert_eq!(inode.start_cluster(), DEVICES_FILESYSTEM_CLUSTER_MAGIC);
        self.open_dir(inode.name())
    }

    fn read_file(
        &self,
        inode: &INode,
        position: u32,
        buf: &mut [u8],
    ) -> Result<u64, FileSystemError> {
        assert_eq!(inode.start_cluster(), DEVICES_FILESYSTEM_CLUSTER_MAGIC);
        inode
            .device()
            .ok_or(FileSystemError::FileNotFound)?
            .read(position, buf)
    }
}

pub fn init_devices_mapping() {
    DEVICES
        .set(Arc::new(Mutex::new(Devices {
            devices: BTreeMap::new(),
        })))
        .expect("Devices already initialized");

    fs::mount("/devices", DEVICES.get().clone());
}

#[allow(dead_code)]
pub fn register_device(device: Arc<dyn Device>) {
    let mut devices = DEVICES.get().lock();
    devices.devices.insert(String::from(device.name()), device);
}

pub fn prope_pci_devices() {
    let pci_device_iter = PciDevicePropeIterator::new();
    for device in pci_device_iter {
        if probe_driver(&device) {
            println!(
                "Driver found for device: {:04X}:{:04X} - {}",
                device.vendor_id, device.device_id, device.device_type
            );
        } else {
            println!(
                "No driver found for device: {:04X}:{:04X} - {}",
                device.vendor_id, device.device_id, device.device_type
            );
        }
    }
}

pub fn probe_driver(pci_device: &PciDeviceConfig) -> bool {
    ide::try_register_ide_device(pci_device)
    // add more devices here
}

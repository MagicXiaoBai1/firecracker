
use std::cmp;
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::io::{ErrorKind, Write};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use crate::utils::u64_to_usize;



const MSIX_TABLE_BAR_OFFSET: u64 = 0x8000;
// The size is 256KiB because the table can hold up to 2048 entries, with each
// entry being 128 bits (4 DWORDS).
const MSIX_TABLE_SIZE: u64 = 0x40000;
const MSIX_PBA_BAR_OFFSET: u64 = 0x48000;
// The size is 2KiB because the Pending Bit Array has one bit per vector and it
// can support up to 2048 vectors.
const MSIX_PBA_SIZE: u64 = 0x800;



use pci::{
    PciBdf, PciCapabilityId, PciClassCode, PciMassStorageSubclass, PciNetworkControllerSubclass,
    PciSubclass,
};
use crate::pci::{BarReprogrammingParams, DeviceRelocationError, PciDevice};

use crate::pci::configuration::{PciCapability, PciConfiguration, PciConfigurationState};
use crate::pci::msix::{MsixCap, MsixConfig, MsixConfigState};
use crate::vstate::interrupts::{InterruptError, MsixVectorGroup};
use crate::vstate::memory::GuestMemoryMmap;
use crate::vstate::bus::BusDevice;


#[derive(Debug)]
pub struct VfioInterruptMsix {
    msix_config: Arc<Mutex<MsixConfig>>,
    config_vector: Arc<AtomicU16>,
    queues_vectors: Arc<Mutex<Vec<u16>>>,
    vectors: Arc<MsixVectorGroup>,
}

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum VfioPciDeviceError {
    /// Failed creating VfioPciDevice: {0}
    CreateVfioPciDevice(#[from] DeviceRelocationError),
    /// Error creating MSI configuration: {0}
    Msi(#[from] InterruptError),
}


#[derive(Debug)] 
pub struct VfioPciDevice {
    id: String,

    // BDF assigned to the device
    pci_device_bdf: PciBdf,

    // PCI configuration registers.
    configuration: PciConfiguration,

    // PCI interrupts.
    virtio_interrupt: Option<Arc<VfioInterruptMsix>>,

    // Guest memory
    memory: GuestMemoryMmap,

}


impl PciDevice for VfioPciDevice {
    fn write_config_register(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) -> Option<Arc<Barrier>> {
        // Handle the special case where the capability VIRTIO_PCI_CAP_PCI_CFG
        // is accessed. This capability has a special meaning as it allows the
        // guest to access other capabilities without mapping the PCI BAR.
        let base = reg_idx * 4;
        if base + u64_to_usize(offset) >= self.cap_pci_cfg_info.offset
            && base + u64_to_usize(offset) + data.len()
                <= self.cap_pci_cfg_info.offset + self.cap_pci_cfg_info.cap.bytes().len()
        {
            let offset = base + u64_to_usize(offset) - self.cap_pci_cfg_info.offset;
            self.write_cap_pci_cfg(offset, data)
        } else {
            self.configuration
                .write_config_register(reg_idx, offset, data);
            None
        }
    }

    fn read_config_register(&mut self, reg_idx: usize) -> u32 {
        // Handle the special case where the capability VIRTIO_PCI_CAP_PCI_CFG
        // is accessed. This capability has a special meaning as it allows the
        // guest to access other capabilities without mapping the PCI BAR.
        let base = reg_idx * 4;
        if base >= self.cap_pci_cfg_info.offset
            && base + 4 <= self.cap_pci_cfg_info.offset + self.cap_pci_cfg_info.cap.bytes().len()
        {
            let offset = base - self.cap_pci_cfg_info.offset;
            let mut data = [0u8; 4];
            let len = u32::from(self.cap_pci_cfg_info.cap.cap.length) as usize;
            if len <= 4 {
                self.read_cap_pci_cfg(offset, &mut data[..len]);
                u32::from_le_bytes(data)
            } else {
                0
            }
        } else {
            self.configuration.read_reg(reg_idx)
        }
    }

    fn detect_bar_reprogramming(
        &mut self,
        reg_idx: usize,
        data: &[u8],
    ) -> Option<BarReprogrammingParams> {
        self.configuration.detect_bar_reprogramming(reg_idx, data)
    }

    fn move_bar(&mut self, old_base: u64, new_base: u64) -> Result<(), DeviceRelocationError> {
        // We only update our idea of the bar in order to support free_bars() above.
        // The majority of the reallocation is done inside DeviceManager.
        if self.bar_address == old_base {
            self.bar_address = new_base;
        }

        Ok(())
    }

    fn read_bar(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        match offset {
            o if (ISR_CONFIG_BAR_OFFSET..ISR_CONFIG_BAR_OFFSET + ISR_CONFIG_SIZE).contains(&o) => {
                // We don't actually support legacy INT#x interrupts for VirtIO PCI devices
                warn!("pci: read access to unsupported ISR status field");
                data.fill(0);
            }
            o if (DEVICE_CONFIG_BAR_OFFSET..DEVICE_CONFIG_BAR_OFFSET + DEVICE_CONFIG_SIZE)
                .contains(&o) =>
            {
                let device = self.device.lock().unwrap();
                device.read_config(o - DEVICE_CONFIG_BAR_OFFSET, data);
            }
            o if (NOTIFICATION_BAR_OFFSET..NOTIFICATION_BAR_OFFSET + NOTIFICATION_SIZE)
                .contains(&o) =>
            {
                // Handled with ioeventfds.
                warn!("pci: unexpected read to notification BAR. Offset {o:#x}");
            }
            o if (MSIX_TABLE_BAR_OFFSET..MSIX_TABLE_BAR_OFFSET + MSIX_TABLE_SIZE).contains(&o) => {
                if let Some(interrupt) = &self.virtio_interrupt {
                    interrupt
                        .msix_config
                        .lock()
                        .unwrap()
                        .read_table(o - MSIX_TABLE_BAR_OFFSET, data);
                }
            }
            o if (MSIX_PBA_BAR_OFFSET..MSIX_PBA_BAR_OFFSET + MSIX_PBA_SIZE).contains(&o) => {
                if let Some(interrupt) = &self.virtio_interrupt {
                    interrupt
                        .msix_config
                        .lock()
                        .unwrap()
                        .read_pba(o - MSIX_PBA_BAR_OFFSET, data);
                }
            }
            _ => (),
        }
    }

    fn write_bar(&mut self, _base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        match offset {
            o if (ISR_CONFIG_BAR_OFFSET..ISR_CONFIG_BAR_OFFSET + ISR_CONFIG_SIZE).contains(&o) => {
                // We don't actually support legacy INT#x interrupts for VirtIO PCI devices
                warn!("pci: access to unsupported ISR status field");
            }
            o if (DEVICE_CONFIG_BAR_OFFSET..DEVICE_CONFIG_BAR_OFFSET + DEVICE_CONFIG_SIZE)
                .contains(&o) =>
            {
                let mut device = self.device.lock().unwrap();
                device.write_config(o - DEVICE_CONFIG_BAR_OFFSET, data);
            }
            o if (NOTIFICATION_BAR_OFFSET..NOTIFICATION_BAR_OFFSET + NOTIFICATION_SIZE)
                .contains(&o) =>
            {
                // Handled with ioeventfds.
                warn!("pci: unexpected write to notification BAR. Offset {o:#x}");
            }
            o if (MSIX_TABLE_BAR_OFFSET..MSIX_TABLE_BAR_OFFSET + MSIX_TABLE_SIZE).contains(&o) => {
                if let Some(interrupt) = &self.virtio_interrupt {
                    interrupt
                        .msix_config
                        .lock()
                        .unwrap()
                        .write_table(o - MSIX_TABLE_BAR_OFFSET, data);
                }
            }
            o if (MSIX_PBA_BAR_OFFSET..MSIX_PBA_BAR_OFFSET + MSIX_PBA_SIZE).contains(&o) => {
                if let Some(interrupt) = &self.virtio_interrupt {
                    interrupt
                        .msix_config
                        .lock()
                        .unwrap()
                        .write_pba(o - MSIX_PBA_BAR_OFFSET, data);
                }
            }
            _ => (),
        };

        // Try and activate the device if the driver status has changed
        if self.needs_activation() {
            debug!("Activating device");
            let interrupt = Arc::clone(self.virtio_interrupt.as_ref().unwrap());
            match self
                .virtio_device()
                .lock()
                .unwrap()
                .activate(self.memory.clone(), interrupt.clone())
            {
                Ok(()) => self.device_activated.store(true, Ordering::SeqCst),
                Err(err) => {
                    error!("Error activating device: {err:?}");

                    // Section 2.1.2 of the specification states that we need to send a device
                    // configuration change interrupt
                    let _ = interrupt.trigger(VirtioInterruptType::Config);
                }
            }
        }

        // Device has been reset by the driver
        if self.device_activated.load(Ordering::SeqCst) && self.is_driver_init() {
            let mut device = self.device.lock().unwrap();
            let reset_result = device.reset();
            match reset_result {
                Some(_) => {
                    // Upon reset the device returns its interrupt EventFD
                    self.virtio_interrupt = None;
                    self.device_activated.store(false, Ordering::SeqCst);

                    // Reset queue readiness (changes queue_enable), queue sizes
                    // and selected_queue as per spec for reset
                    self.virtio_device()
                        .lock()
                        .unwrap()
                        .queues_mut()
                        .iter_mut()
                        .for_each(Queue::reset);
                    self.common_config.queue_select = 0;
                }
                None => {
                    error!("Attempt to reset device when not implemented in underlying device");
                    // TODO: currently we don't support device resetting, but we still
                    // follow the spec and set the status field to 0.
                    self.common_config.driver_status = DEVICE_INIT;
                }
            }
        }
        None
    }
}

impl BusDevice for VfioPciDevice {
    fn read(&mut self, base: u64, offset: u64, data: &mut [u8]) {
        self.read_bar(base, offset, data)
    }

    fn write(&mut self, base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        self.write_bar(base, offset, data)
    }
}
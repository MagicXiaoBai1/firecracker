
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, RwLock};
use crate::vstate::bus::BusDeviceSync;
use crate::{EventManager, Vm};
use std::os::unix::io::AsRawFd;

use log::{debug, error, info, warn};
use thiserror::Error;

use super::memory_recognizer::{MemoryRecognizer, VfioPcieMemoryRecognizer};
use crate::pci::msix::{MsixCap, MsixConfig, MsixConfigState};
use crate::pci::{BarReprogrammingParams, DeviceRelocationError, PciDevice};
use crate::vstate::interrupts::{InterruptError, MsixVectorGroup};
use crate::vstate::vm::VmCommon;
use pci::{
    PciBdf, PciCapabilityId, PciClassCode, PciMassStorageSubclass, PciNetworkControllerSubclass,
    PciSubclass,
};
use vfio_bindings::bindings::vfio::*;
use vfio_ioctls::{
    VfioContainer, VfioDevice, VfioIrq, VfioRegionInfoCap, VfioRegionSparseMmapArea,
};
use vmm_sys_util::eventfd::EventFd;
use super::msix::VfioInterruptEngine;

#[derive(Debug, Error)]
pub enum VfioError {
    #[error("Kernel VFIO error")]
    KernelVfio(#[source] vfio_ioctls::VfioError),
    #[error("VFIO user error")]
    VfioUser(#[source] vfio_user::Error),
}

pub(crate) trait Vfio: Send + Sync {
    fn read_config_byte(&self, offset: u32) -> u8 {
        let mut data: [u8; 1] = [0];
        self.read_config(offset, &mut data);
        data[0]
    }

    fn read_config_word(&self, offset: u32) -> u16 {
        let mut data: [u8; 2] = [0, 0];
        self.read_config(offset, &mut data);
        u16::from_le_bytes(data)
    }

    fn read_config_dword(&self, offset: u32) -> u32 {
        let mut data: [u8; 4] = [0, 0, 0, 0];
        self.read_config(offset, &mut data);
        u32::from_le_bytes(data)
    }

    fn write_config_dword(&self, offset: u32, buf: u32) {
        let data: [u8; 4] = buf.to_le_bytes();
        self.write_config(offset, &data);
    }

    fn read_config(&self, offset: u32, data: &mut [u8]) {
        self.region_read(VFIO_PCI_CONFIG_REGION_INDEX, offset.into(), data.as_mut());
    }

    fn write_config(&self, offset: u32, data: &[u8]) {
        self.region_write(VFIO_PCI_CONFIG_REGION_INDEX, offset.into(), data);
    }

    fn enable_msi(&self, fds: Vec<&EventFd>) -> Result<(), VfioError> {
        self.enable_irq(VFIO_PCI_MSI_IRQ_INDEX, fds)
    }

    fn disable_msi(&self) -> Result<(), VfioError> {
        self.disable_irq(VFIO_PCI_MSI_IRQ_INDEX)
    }

    fn enable_msix(&self, fds: Vec<&EventFd>) -> Result<(), VfioError> {
        self.enable_irq(VFIO_PCI_MSIX_IRQ_INDEX, fds)
    }

    fn disable_msix(&self) -> Result<(), VfioError> {
        self.disable_irq(VFIO_PCI_MSIX_IRQ_INDEX)
    }

    fn region_read(&self, _index: u32, _offset: u64, _data: &mut [u8]) {
        unimplemented!()
    }

    fn region_write(&self, _index: u32, _offset: u64, _data: &[u8]) {
        unimplemented!()
    }

    fn get_irq_info(&self, _irq_index: u32) -> Option<VfioIrq> {
        unimplemented!()
    }

    fn enable_irq(&self, _irq_index: u32, _event_fds: Vec<&EventFd>) -> Result<(), VfioError> {
        unimplemented!()
    }

    fn disable_irq(&self, _irq_index: u32) -> Result<(), VfioError> {
        unimplemented!()
    }

    fn unmask_irq(&self, _irq_index: u32) -> Result<(), VfioError> {
        unimplemented!()
    }

    fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
        unimplemented!()
    }

    fn get_vfio_device(&self) -> Arc<VfioDevice> {
        unimplemented!()
    }
}


pub(crate) trait VfioMsixOps {
    fn read_table(&self, index: u16, offset: u64, data: &mut [u8]);

    fn write_table(&mut self, index: u16, offset: u64, data: &[u8]);

    fn set_msg_ctl(&mut self, reg: u16);

    fn get_msg_ctl(&self) -> u16 ;

    fn read_pba(&self, offset: u64, data: &mut [u8]);

    fn write_pba(&mut self, _offset: u64, _data: &[u8]);

    fn set_pba_bit(&mut self, vector: u16, reset: bool);

    fn get_pba_bit(&self, vector: u16) -> u8;

    fn inject_msix_and_clear_pba(&mut self, vector: usize);
}

pub(crate) struct VfioDeviceWrapper {
    device: Arc<VfioDevice>,
}

impl VfioDeviceWrapper {
    pub fn new(device: Arc<VfioDevice>) -> Self {
        Self { device }
    }
}

impl Vfio for VfioDeviceWrapper {
    fn region_read(&self, index: u32, offset: u64, data: &mut [u8]) {
        self.device.region_read(index, data, offset);
    }

    fn region_write(&self, index: u32, offset: u64, data: &[u8]) {
        self.device.region_write(index, data, offset);
    }

    fn get_irq_info(&self, irq_index: u32) -> Option<VfioIrq> {
        self.device.get_irq_info(irq_index).copied()
    }

    fn enable_irq(&self, irq_index: u32, event_fds: Vec<&EventFd>) -> Result<(), VfioError> {
        self.device
            .enable_irq(irq_index, event_fds)
            .map_err(VfioError::KernelVfio)
    }

    fn disable_irq(&self, irq_index: u32) -> Result<(), VfioError> {
        self.device
            .disable_irq(irq_index)
            .map_err(VfioError::KernelVfio)
    }

    fn unmask_irq(&self, irq_index: u32) -> Result<(), VfioError> {
        self.device
            .unmask_irq(irq_index)
            .map_err(VfioError::KernelVfio)
    }

    fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
        Some(self.device.as_raw_fd())
    }

    fn get_vfio_device(&self) -> Arc<VfioDevice> {
        Arc::clone(&self.device)
    }

}


const PCI_ROM_EXP_BAR_INDEX: usize = 12;
// PCI config register size (4 bytes).
const PCI_CONFIG_REGISTER_SIZE: usize = 4;

impl fmt::Debug for VfioDeviceWrapper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 直接输出你指定的字符串
        write!(f, "VfioDeviceWrapper ")
    }
}



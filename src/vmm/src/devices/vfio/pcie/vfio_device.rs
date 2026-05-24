
use std::{cmp, fmt};
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::io::{ErrorKind, Write};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use log::debug;
use crate::utils::u64_to_usize;
use crate::{EventManager, Vm};



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
use vfio_ioctls::{VfioContainer, VfioDevice, VfioDeviceFd, VfioOps};
use crate::devices::vfio::pcie::vfio::{Vfio, VfioCommon, VfioDeviceWrapper};



#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum VfioPciDeviceError {
    /// Failed creating VfioPciDevice: {0}
    CreateVfioPciDevice(#[from] DeviceRelocationError),
    /// Error creating MSI configuration: {0}
    Msi(#[from] InterruptError),
}


#[derive(Copy, Clone)]
enum PciVfioSubclass {
    VfioSubclass = 0xff,
}
impl PciSubclass for PciVfioSubclass {
    fn get_register_value(&self) -> u8 {
        *self as u8
    }
}

pub struct VfioPciDevice {
    id: String,

    // BDF assigned to the device
    pci_device_bdf: PciBdf,

    // vfio设备共性部分
    common: VfioCommon,

    // virtual machine
    vm: Arc<Vm>,

    // 资源
    vfio_container: Arc<VfioContainer>,
    vfio_device: Arc<VfioDevice>,
}


impl VfioPciDevice {
    pub fn new(
        pci_device_bdf: PciBdf,
        vfio_device: VfioDevice,
        vfio_container: Arc<VfioContainer>,
        vm: &Arc<Vm>,
        msix_vectors: MsixVectorGroup
    ) -> Self {
        let vfio_device = Arc::new(vfio_device);
        let vfio_wrapper = VfioDeviceWrapper::new(Arc::clone(&vfio_device));
        let common: VfioCommon = VfioCommon::new(
            pci_device_bdf.into(),
         &PciVfioSubclass::VfioSubclass,
           Arc::new(vfio_wrapper) as Arc<dyn Vfio>, 
           msix_vectors,
           vm.clone(),
        );

        Self {
            id: format!("vfio-pci-{}", pci_device_bdf),
            pci_device_bdf: pci_device_bdf,
            common: common,
            vm: Arc::clone(&vm),
            vfio_container: vfio_container,
            vfio_device: vfio_device,
        }
    }
}


impl PciDevice for VfioPciDevice {

    fn write_config_register(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) -> Option<Arc<Barrier>> {
        debug!(
            "vfio-pci {} write_config_register reg_idx={} offset={} len={}",
            self.id,
            reg_idx,
            offset,
            data.len()
        );
        self.common.write_config_register(reg_idx, offset, data)
    }

    fn read_config_register(&mut self, reg_idx: usize) -> u32 {
        debug!(
            "vfio-pci {} read_config_register reg_idx={}",
            self.id,
            reg_idx
        );
        self.common.read_config_register(reg_idx)
    }

    fn detect_bar_reprogramming(
        &mut self,
        reg_idx: usize,
        data: &[u8],
    ) -> Option<BarReprogrammingParams> {
        self.common.detect_bar_reprogramming(reg_idx, data)
    }

    fn move_bar(&mut self, old_base: u64, new_base: u64) -> Result<(), DeviceRelocationError> {
        // TODO 去掉old_base的 vfio fd的映射，创建new_base处的vfio fd
        debug!(
            "vfio-pci {} move_bar old_base={:#x} new_base={:#x} not supported",
            self.id,
            old_base,
            new_base
        );
        self.common.move_bar(old_base, new_base)

    }

    fn read_bar(&mut self, base: u64, offset: u64, data: &mut [u8]) {
        debug!(
            "vfio-pci {} read_bar base={:#x} offset={:#x} len={}",
            self.id,
            base,
            offset,
            data.len()
        );
        self.common.read_bar(base, offset, data);
    }

    fn write_bar(&mut self, base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        debug!(
            "vfio-pci {} write_bar base={:#x} offset={:#x} len={}",
            self.id,
            base,
            offset,
            data.len()
        );
        self.common.write_bar(base, offset, data)
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


impl fmt::Debug for VfioPciDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VfioPciDevice")
            .field("id", &self.id)
            .field("pci_device_bdf", &self.pci_device_bdf)
            .field("common", &self.common)
            // 跳过未实现 Debug 的字段
            .finish()
    }
}


use std::{cmp, fmt};
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
use vfio_ioctls::{VfioContainer, VfioDevice, VfioDeviceFd, VfioOps};
use crate::devices::vfio::pcie::vfio::{Vfio, VfioCommon};



#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum VfioPciDeviceError {
    /// Failed creating VfioPciDevice: {0}
    CreateVfioPciDevice(#[from] DeviceRelocationError),
    /// Error creating MSI configuration: {0}
    Msi(#[from] InterruptError),
}


pub struct VfioPciDevice {
    id: String,

    // BDF assigned to the device
    pci_device_bdf: PciBdf,

    // vfio设备共性部分
    common: VfioCommon,

    // Guest memory
    memory: GuestMemoryMmap,

    // 资源
    vfio_container: Arc<VfioContainer>,
    vfio_device: Arc<VfioDevice>,
}
impl fmt::Debug for VfioPciDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VfioPciDevice")
            .field("id", &self.id)
            .field("pci_device_bdf", &self.pci_device_bdf)
            .field("common", &self.common)
            // 跳过未实现 Debug 的字段
            .field("memory", &self.memory)
            .finish()
    }
}

// impl VfioPciDevice {
//     pub fn new(vfio_device: &VfioDevice, vfio_container: &VfioContainer) -> Self {
//         // 1. 获取设备的BDF
//         // 3. 创建设备
//         Self {
//             id: format!("vfio-pci-{}", pci_device_bdf),
//             pci_device_bdf,
//             configuration,
//             virtio_interrupt: None,
//             memory: GuestMemoryMmap::default(),
//         }
//     }
// }

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
        // let base = reg_idx * 4;
        // if base + u64_to_usize(offset) >= self.cap_pci_cfg_info.offset
        //     && base + u64_to_usize(offset) + data.len()
        //         <= self.cap_pci_cfg_info.offset + self.cap_pci_cfg_info.cap.bytes().len()
        // {
        //     let offset = base + u64_to_usize(offset) - self.cap_pci_cfg_info.offset;
        //     self.write_cap_pci_cfg(offset, data)
        // } else {
        //     self.configuration
        //         .write_config_register(reg_idx, offset, data);
        //     None
        // }
        None
    }

    fn read_config_register(&mut self, reg_idx: usize) -> u32 {
        // Handle the special case where the capability VIRTIO_PCI_CAP_PCI_CFG
        // is accessed. This capability has a special meaning as it allows the
        // guest to access other capabilities without mapping the PCI BAR.
        // let base = reg_idx * 4;
        // if base >= self.cap_pci_cfg_info.offset
        //     && base + 4 <= self.cap_pci_cfg_info.offset + self.cap_pci_cfg_info.cap.bytes().len()
        // {
        //     let offset = base - self.cap_pci_cfg_info.offset;
        //     let mut data = [0u8; 4];
        //     let len = u32::from(self.cap_pci_cfg_info.cap.cap.length) as usize;
        //     if len <= 4 {
        //         self.read_cap_pci_cfg(offset, &mut data[..len]);
        //         u32::from_le_bytes(data)
        //     } else {
        //         0
        //     }
        // } else {
        //     self.configuration.read_reg(reg_idx)
        // }
        0
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
        //  // Remove old region
        //  // SAFETY: MmapRegion invariants guarantee that
        //  // host_addr points to len bytes of
        //  // valid memory that will only be unmapped with munmap().
        //  unsafe {
        //      self.vm.remove_user_memory_region(
        //          user_memory_region.slot,
        //          user_memory_region.start,
        //          len,
        //          host_addr,
        //          false,
        //          false,
        //      )
        //  }
        //  .map_err(io::Error::other)?;

        //  // Update the user memory region with the correct start address.
        //  if new_base > old_base {
        //      user_memory_region.start += new_base - old_base;
        //  } else {
        //      user_memory_region.start -= old_base - new_base;
        //  }

        //  // Insert new region
        //  // SAFETY: MmapRegion invariants guarantee that
        //  // host_addr points to len bytes of
        //  // valid memory that will only be unmapped with munmap().
        //  unsafe {
        //      self.vm.create_user_memory_region(
        //          user_memory_region.slot,
        //          user_memory_region.start,
        //          len,
        //          host_addr,
        //          false,
        //          false,
        //      )
        //  }
        //  .map_err(io::Error::other)?;

        Ok(())
    }

    fn read_bar(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
    }

    fn write_bar(&mut self, _base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
     
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

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use log::{error, info, warn};
use thiserror::Error;

use super::configuration::{VfioBarRegionInfo, VfioPcieConfiguration};
use super::mmio_mgr::VfioMmioEngine;
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
}

pub(crate) trait VfioBarOps {
    fn move_bar(
        &mut self,
        bar_idx: usize,
        old_base: u64,
        new_base: u64,
        len: u64,
    ) -> Result<(), Box<dyn std::error::Error>>;

    fn allocate_bars(
        &mut self,
        vm: VmCommon,
        bar_region_info: &[VfioBarRegionInfo; 6],
    ) -> Result<(), Box<dyn std::error::Error>>;

    fn read_bar(&self, base: u64, offset: u64, data: &mut [u8]);

    fn write_bar(&self, base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>>;
}

pub(crate) trait VfioMsixOps {
    fn read_table(&self, offset: u64, data: &mut [u8]);

    fn write_table(&mut self, offset: u64, data: &[u8]);

    fn set_msg_ctl(&mut self, reg: u16);

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
}


const PCI_ROM_EXP_BAR_INDEX: usize = 12;
// PCI config register size (4 bytes).
const PCI_CONFIG_REGISTER_SIZE: usize = 4;
pub(crate) struct VfioCommon {
    pub(crate) configuration: VfioPcieConfiguration,
    bar_region_info: [VfioBarRegionInfo; 6],
    mmio_mgr: VfioMmioEngine,
    virtio_interrupt_mgr: VfioInterruptEngine,
    pub(crate) vfio_wrapper: Arc<dyn Vfio>,
    // TODO pub(crate) patches: HashMap<usize, ConfigPatch>,

}

impl fmt::Debug for VfioCommon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VfioCommon")
            .field("configuration", &self.configuration)
            .field("virtio_interrupt_mgr", &self.virtio_interrupt_mgr)
            .finish()
    }
}

impl VfioCommon {
    pub(crate) fn new(
        id :u32,
        _subclass: &dyn PciSubclass,
        vfio_wrapper: Arc<dyn Vfio>,
        msix_vectors: MsixVectorGroup
    ) -> Self{
        // Keep a VFIO-local config cache for BAR/MSI-X control paths.
        let mut configuration = VfioPcieConfiguration::new(vfio_wrapper.clone());
        let bar_region_info = configuration.compute_bar_region_info(vfio_wrapper.as_ref());
        let mut mmio_mgr = configuration.create_vfio_mmio_engine();
        mmio_mgr.sync_passthrough_status(&bar_region_info);

        let virtio_interrupt_mgr = configuration.create_vfio_interrupt_engine(id, msix_vectors);

        // TODO FIRST 新建VfioInterruptMsix，研究vfio_wrapper传递
        Self{
            configuration,
            bar_region_info,
            mmio_mgr,
            virtio_interrupt_mgr,
            vfio_wrapper
        }
    }


    pub fn write_config_register(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) -> Option<Arc<Barrier>> {
        // 1. 判断是否在写bar寄存器：如果是就写PciConfiguration对象，然后返回
        // 2. 判断是否在使能misx or msi
        // 3. 读写vfio fd(vfio_wrapper)
        // 4. 根据MSE bit的值处理 bar reprogram（好像什么都不用做）因为bar reprogram（移动bar空间的HPA）不会发生
        let is_bar_write = VfioPcieConfiguration::is_access_bar_register(reg_idx, offset, data);
        if is_bar_write
            || self
                .configuration
                .is_access_msix_capabilities(reg_idx, offset, data, self.vfio_wrapper.as_ref())
        {
            if is_bar_write {
                let _ = self.configuration.detect_bar_reprogramming(reg_idx, data);
            }

            self.configuration.write_config_register(reg_idx, offset, data);
            self.bar_region_info = self.configuration.bar_region_info();
            self.mmio_mgr.sync_passthrough_status(&self.bar_region_info);
            return None;
        }

        let cfg_offset = ((reg_idx * PCI_CONFIG_REGISTER_SIZE) as u64 + offset) as u32;
        self.vfio_wrapper.write_config(cfg_offset, data);
        None
    }

    pub fn read_config_register(&mut self, reg_idx: usize) -> u32 {
        // 1. 判断是否在读bar寄存器：如果是就读PciConfiguration对象，然后返回
        // 2. 判断是否在读misx or msi能力
        // 3. mask multi-function bit
        // 4. 读vfio fd(vfio_wrapper)
        // 5. 处理mask和patch
        let mut value = if VfioPcieConfiguration::is_access_bar_register(reg_idx, 0, &[])
            || self
                .configuration
                .is_access_msix_capabilities(reg_idx, 0, &[], self.vfio_wrapper.as_ref())
        {
            self.configuration.read_reg(reg_idx)
        } else {
            self.vfio_wrapper.read_config_dword((reg_idx * PCI_CONFIG_REGISTER_SIZE) as u32)
        };

        // Header Type register has the multi-function bit as bit 23 in DWORD #3.
        // We currently expose a single function in the virtual topology.
        if reg_idx == 3 {
            value &= !(1u32 << 23);
        }

        value
    }

    pub fn read_bar(&mut self, base: u64, offset: u64, data: &mut [u8]) {
        // 1. 判断是否在读msix table 和 PBA
        // 1. 是：读vmm内存中的msix table -> 调用 virtio_interrupt_mgr
        // 2. 否：读vfio fd(vfio_wrapper)
        if let Some((table_offset, _table_size, pba_offset, pba_size)) =
            self.configuration.msix_layout_for_bar(self.vfio_wrapper.as_ref(), base)
        {
            let access_msix_vector = self
                .configuration
                .is_access_msix_vector_register(self.vfio_wrapper.as_ref(), base, offset);

            if access_msix_vector {
                if let Some(msix_offset) = offset.checked_sub(table_offset) {
                    self.virtio_interrupt_mgr.read_table(msix_offset, data);
                    return;
                }

                if let Some(pba_offset) = offset.checked_sub(pba_offset) {
                    if pba_offset < pba_size {
                        self.virtio_interrupt_mgr.read_pba(pba_offset, data);
                        return;
                    }
                }
            }
        }

        if let Some(region_index) = self.configuration.vfio_region_index_from_base(base) {
            self.vfio_wrapper.region_read(region_index, offset, data);
        } else {
            warn!("Failed to resolve VFIO BAR region for base={:#x}; defaulting to BAR0", base);
            self.vfio_wrapper
                .region_read(VFIO_PCI_BAR0_REGION_INDEX, offset, data);
        }
    }

    pub fn write_bar(&mut self, base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        // 1. 判断是否在写msix table 和 PBA
        // 1. 是：写vmm内存中的msix table -> 调用 virtio_interrupt_mgr
        // 2. 否：写vfio fd(vfio_wrapper)
        if let Some((table_offset, _table_size, pba_offset, pba_size)) =
            self.configuration.msix_layout_for_bar(self.vfio_wrapper.as_ref(), base)
        {
            let access_msix_vector = self
                .configuration
                .is_access_msix_vector_register(self.vfio_wrapper.as_ref(), base, offset);

            if access_msix_vector {
                if let Some(msix_offset) = offset.checked_sub(table_offset) {
                    self.virtio_interrupt_mgr.write_table(msix_offset, data);
                    return None;
                }

                if let Some(pba_offset) = offset.checked_sub(pba_offset) {
                    if pba_offset < pba_size {
                        self.virtio_interrupt_mgr.write_pba(pba_offset, data);
                        return None;
                    }
                }
            }
        }

        if let Some(region_index) = self.configuration.vfio_region_index_from_base(base) {
            self.vfio_wrapper.region_write(region_index, offset, data);
        } else {
            warn!("Failed to resolve VFIO BAR region for base={:#x}; defaulting to BAR0", base);
            self.vfio_wrapper
                .region_write(VFIO_PCI_BAR0_REGION_INDEX, offset, data);
        }
        None
    }

    pub fn detect_bar_reprogramming(
        &mut self,
        reg_idx: usize,
        data: &[u8],
    ) -> Option<BarReprogrammingParams> {
        self.configuration.detect_bar_reprogramming(reg_idx, data)
    }

    pub(crate) fn allocate_bars_in_vfio(
        &mut self,
        _vm: VmCommon,
    ) {
        // 调用vfio_wrapper获得设备可直通的bar空间的mmap
        info!("allocate_bars_in_vfio is not implemented yet");
    }

    pub(crate) fn set_vfio_bar_in_kvm(
        &mut self,
        _vm: VmCommon,
    ) {
        // 将设备可直通的bar空间的mmap的HPA 配置给guest的GPA map HPA
        info!("set_vfio_bar_in_kvm is not implemented yet");
    }

}
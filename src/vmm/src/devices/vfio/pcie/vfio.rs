
use std::fmt;
use super::configuration::VfioPcieConfiguration;
use vfio_bindings::bindings::vfio::*;
use vfio_ioctls::{
    VfioContainer, VfioDevice, VfioIrq, VfioRegionInfoCap, VfioRegionSparseMmapArea,
};
use vmm_sys_util::eventfd::EventFd;
use std::sync::{Arc, Barrier, Mutex};
use log::{error, info, warn};
use thiserror::Error;
use crate::vstate::interrupts::{InterruptError, MsixVectorGroup};
use crate::pci::msix::{MsixCap, MsixConfig, MsixConfigState};
use crate::vstate::vm::VmCommon;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicUsize, Ordering};
use crate::pci::{BarReprogrammingParams, DeviceRelocationError, PciDevice};
use pci::{
    PciBdf, PciCapabilityId, PciClassCode, PciMassStorageSubclass, PciNetworkControllerSubclass,
    PciSubclass,
};

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

#[derive(Debug)]
pub struct VfioInterruptMsix {
    msix_config: Arc<Mutex<MsixConfig>>,
    vectors: Arc<MsixVectorGroup>,
}

const PCI_ROM_EXP_BAR_INDEX: usize = 12;
// PCI config register size (4 bytes).
const PCI_CONFIG_REGISTER_SIZE: usize = 4;
const PCI_CONFIG_CAPABILITY_OFFSET: u32 = 0x34;
const PCI_CONFIG_CAPABILITY_PTR_MASK: u8 = 0xfc;
const MSIX_TABLE_ENTRY_SIZE: u64 = 16;

pub(crate) struct VfioCommon {
    pub(crate) configuration: VfioPcieConfiguration,
    // Reserved for BAR-backed MSI-X emulation path.
    virtio_interrupt: Option<Arc<VfioInterruptMsix>>,
    pub(crate) vfio_wrapper: Arc<dyn Vfio>,
    // TODO pub(crate) patches: HashMap<usize, ConfigPatch>,

}

impl fmt::Debug for VfioCommon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VfioCommon")
            .field("configuration", &self.configuration)
            .field("virtio_interrupt", &self.virtio_interrupt)
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
        let configuration = VfioPcieConfiguration::new();

        let msix_vectors = Arc::new(msix_vectors);
        let msix_config = Arc::new(Mutex::new(MsixConfig::new(
            msix_vectors.clone(),
            id,
        )));

        let virtio_interrupt: Option<Arc<VfioInterruptMsix>> = Option::Some(Arc::new(
            VfioInterruptMsix{
                msix_config: msix_config, 
                vectors: msix_vectors
            }
        ));

        // TODO FIRST 新建VfioInterruptMsix，研究vfio_wrapper传递
        Self{
            configuration,
            virtio_interrupt,
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
        if Self::is_access_bar_register(reg_idx, offset, data)
            || self.is_access_misx_capabilities(reg_idx, offset, data)
        {
            self.configuration.write_config_register(reg_idx, offset, data);
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
        let mut value = if Self::is_access_bar_register(reg_idx, 0, &[])
            || self.is_access_misx_capabilities(reg_idx, 0, &[])
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
        // 1. 判断是否在读msix table
        // 1. 是：读vmm内存中的msix table -> 调用 virtio_interrupt
        // 2. 否：读vfio fd(vfio_wrapper)
        if self.virtio_interrupt.is_some() {
            if let Some((table_offset, _table_size, _pba_offset, _pba_size)) = self.msix_layout_for_bar(base) {
                if let Some(irq) = &self.virtio_interrupt {
                    let msix = irq.msix_config.lock().expect("Poisoned lock");
                    if self.is_access_msix_vector_register(base, offset) {
                        if let Some(msix_offset) = offset.checked_sub(table_offset) {
                            msix.read_table(msix_offset, data);
                            return;
                        }
                        if let Some(pba_offset) = self.msix_pba_relative_offset(base, offset) {
                            msix.read_pba(pba_offset, data);
                            return;
                        }
                    }
                }
            }
        }

        if let Some(region_index) = self.vfio_region_index_from_base(base) {
            self.vfio_wrapper.region_read(region_index, offset, data);
        } else {
            warn!("Failed to resolve VFIO BAR region for base={:#x}; defaulting to BAR0", base);
            self.vfio_wrapper
                .region_read(VFIO_PCI_BAR0_REGION_INDEX, offset, data);
        }
    }

    pub fn write_bar(&mut self, base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        // 1. 判断是否在写msix table
        // 1. 是：写vmm内存中的msix table -> 调用 virtio_interrupt
        // 2. 否：写vfio fd(vfio_wrapper)
        if self.virtio_interrupt.is_some() {
            if let Some((table_offset, _table_size, _pba_offset, _pba_size)) = self.msix_layout_for_bar(base) {
                if let Some(irq) = &self.virtio_interrupt {
                    let mut msix = irq.msix_config.lock().expect("Poisoned lock");
                    if self.is_access_msix_vector_register(base, offset) {
                        if let Some(msix_offset) = offset.checked_sub(table_offset) {
                            msix.write_table(msix_offset, data);
                            return None;
                        }
                        if let Some(pba_offset) = self.msix_pba_relative_offset(base, offset) {
                            msix.write_pba(pba_offset, data);
                            return None;
                        }
                    }
                }
            }
        }

        if let Some(region_index) = self.vfio_region_index_from_base(base) {
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


    fn is_access_bar_register(reg_idx: usize, offset: u64, data: &[u8]) -> bool{
        if !(4..10).contains(&reg_idx) && reg_idx != PCI_ROM_EXP_BAR_INDEX {
            return false;
        }
        

        crate::utils::u64_to_usize(offset) + data.len() <= 4
    }

    fn is_access_misx_capabilities(&self, reg_idx: usize, offset: u64, data: &[u8]) -> bool{
        let Some(msix_cap_offset) = self.find_msix_cap_offset() else {
            return false;
        };

        let access_start = reg_idx * 4 + crate::utils::u64_to_usize(offset);
        let access_end = access_start + core::cmp::max(data.len(), 1);

        // Cloud Hypervisor only treats the first MSI-X capability dword
        // (cap ID + next pointer + message control) as the control register.
        // Table/PBA location dwords are handled separately.
        let cap_start = msix_cap_offset;
        let cap_end = cap_start + 4;

        access_start < cap_end && cap_start < access_end
    }

    fn find_msix_cap_offset(&self) -> Option<usize> {
        let mut cap_ptr = self
            .vfio_wrapper
            .read_config_byte(PCI_CONFIG_CAPABILITY_OFFSET)
            & PCI_CONFIG_CAPABILITY_PTR_MASK;
        let mut guard = 0usize;

        while cap_ptr != 0 && guard < 48 {
            let cap_id = self.vfio_wrapper.read_config_byte(cap_ptr.into());
            if cap_id == PciCapabilityId::MsiX as u8 {
                return Some(usize::from(cap_ptr));
            }

            let next = self
                .vfio_wrapper
                .read_config_byte((cap_ptr + 1).into())
                & PCI_CONFIG_CAPABILITY_PTR_MASK;
            if next == 0 || next == cap_ptr {
                break;
            }
            cap_ptr = next;
            guard += 1;
        }

        None
    }

    fn is_access_msix_vector_register(&self, base: u64, offset: u64) -> bool {
        let Some((table_offset, table_size, pba_offset, pba_size)) = self.msix_layout_for_bar(base) else {
            return false;
        };

        let table_hit = table_offset
            .checked_add(table_size)
            .map(|end| (table_offset..end).contains(&offset))
            .unwrap_or(false);
        let pba_hit = pba_offset
            .checked_add(pba_size)
            .map(|end| (pba_offset..end).contains(&offset))
            .unwrap_or(false);

        table_hit || pba_hit
    }

    fn bar_index_from_base(&self, base: u64) -> Option<u32> {
        self.configuration.find_bar_by_base(base).map(|idx| idx as u32)
    }

    fn vfio_region_index_from_base(&self, base: u64) -> Option<u32> {
        self.bar_index_from_base(base)
            .map(|bar_idx| VFIO_PCI_BAR0_REGION_INDEX + bar_idx)
    }

    fn msix_layout_for_bar(&self, base: u64) -> Option<(u64, u64, u64, u64)> {
        let bar_index = self.bar_index_from_base(base)?;
        let cap_off = self.find_msix_cap_offset()? as u32;

        let msg_ctl = self.vfio_wrapper.read_config_word(cap_off + 2);
        let table = self.vfio_wrapper.read_config_dword(cap_off + 4);
        let pba = self.vfio_wrapper.read_config_dword(cap_off + 8);

        let table_bir = table & 0x7;
        let pba_bir = pba & 0x7;
        if bar_index != table_bir && bar_index != pba_bir {
            return None;
        }

        let table_offset = u64::from(table & 0xffff_fff8);
        let pba_offset = u64::from(pba & 0xffff_fff8);
        let table_entries = u64::from((msg_ctl & 0x07ff) + 1);
        let table_size = table_entries * MSIX_TABLE_ENTRY_SIZE;
        let pba_size = table_entries.div_ceil(64) * 8;

        Some((table_offset, table_size, pba_offset, pba_size))
    }

    fn msix_pba_relative_offset(&self, base: u64, offset: u64) -> Option<u64> {
        let ( _table_offset, _table_size, pba_offset, pba_size) = self.msix_layout_for_bar(base)?;
        let pba_end = pba_offset.checked_add(pba_size)?;
        if !(pba_offset..pba_end).contains(&offset) {
            return None;
        }
        offset.checked_sub(pba_offset)
    }
}
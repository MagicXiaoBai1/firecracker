
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, RwLock};
use crate::vstate::bus::BusDeviceSync;
use crate::{EventManager, Vm};
use std::os::unix::io::AsRawFd;

use log::{debug, error, info, warn};
use thiserror::Error;

use super::configuration::{VfioBarRegionInfo, VfioPcieConfiguration};
use super::mmio_mgr::{BarRegionAccessRequest, BlackStatus, VfioMmioEngine};
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

pub(crate) trait VfioBarOps {
    fn move_bar(
        &mut self,
        old_base: u64,
        new_base: u64,
        len: u64,
    ) -> Result<(), DeviceRelocationError>;

    fn allocate_bars(
        &mut self,
        guest_base: u64,
        bar_region_id: usize,
        offset: u64,
        len: u64,
    ) -> Result<(), Box<dyn std::error::Error>>;

    fn free_bars(&mut self, _guest_base: u64, _len: u64) -> Result<(), Box<dyn std::error::Error>>;

    fn read_bar(&self, base: u64, offset: u64, data: &mut [u8]);

    fn blacklist_filter(
        &self,
        bar_region_id: usize,
        len: u64,
        offset: u64,
    ) -> Vec<BarRegionAccessRequest>;

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

/// VfioCommon: Centralized management of VFIO device state and operations.
///
/// This struct coordinates:
/// - PCI configuration access (through VfioPcieConfiguration)
/// - BAR memory access (through VfioBarOps implementations)
/// - MSI-X interrupt handling (through VfioMsixOps implementations)
/// - Memory region recognition (through MemoryRecognizer)
/// - Thread-safe access to VFIO device FDs
///
/// Important: VFIO device FD is held through the vfio_wrapper Arc, which implements
/// the Vfio trait. This provides a unified, reference-counted interface.
/// Other components must not hold FDs directly; instead they receive them
/// through method parameters to prevent deadlocks.
pub(crate) struct VfioCommon {
    /// PCI configuration space cache and BAR tracking
    pub(crate) configuration: VfioPcieConfiguration,
    
    /// BAR memory access handler (locked separately for BAR operations)
    pub(crate) mmio_mgr: Arc<RwLock<Box<dyn VfioBarOps + Send + Sync>>>,
    
    /// MSI-X interrupt handler (locked separately for interrupt operations)
    pub(crate) msix_mgr: Arc<RwLock<Box<dyn VfioMsixOps + Send + Sync>>>,
    
    /// VFIO device wrapper providing unified interface
    pub(crate) vfio_wrapper: Arc<dyn Vfio>,
    
    /// Memory region recognizer for routing GPA access requests
    pub(crate) memory_recognizer: Arc<RwLock<Box<dyn MemoryRecognizer>>>,
}

impl fmt::Debug for VfioCommon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VfioCommon")
            .field("configuration", &self.configuration)
            .field("has_memory_recognizer", &true)
            .finish()
    }
}

impl VfioCommon {
    pub(crate) fn new(
        id: u32,
        _subclass: &dyn PciSubclass,
        vfio_wrapper: Arc<dyn Vfio>,
        msix_vectors: MsixVectorGroup,
        vm: Arc<Vm>,
    ) -> Self {
        // Keep a VFIO-local config cache for BAR/MSI-X control paths.
        let configuration = VfioPcieConfiguration::new(id, msix_vectors, vfio_wrapper.clone(), vm);
        let mmio_mgr = configuration.get_vfio_mmio_engine();
        let msix_mgr = configuration.get_vfio_msix_engine();

        // Initialize memory recognizer with empty regions.
        // These will be populated during device setup based on BAR layout.
        let memory_recognizer: Box<dyn MemoryRecognizer> =
            Box::new(VfioPcieMemoryRecognizer::new(Vec::new()));

        Self {
            configuration,
            mmio_mgr,
            msix_mgr,
            vfio_wrapper,
            memory_recognizer: Arc::new(RwLock::new(memory_recognizer)),
        }
    }

    /// Update memory recognizer regions after BAR layout changes.
    /// This should be called when BAR addresses are determined or changed.
    pub(crate) fn update_memory_regions(&self, regions: Vec<crate::devices::vfio::pcie::memory_recognizer::RegionInfo>) {
        let new_recognizer = VfioPcieMemoryRecognizer::new(regions);
        if let Ok(mut recognizer) = self.memory_recognizer.write() {
            *recognizer = Box::new(new_recognizer);
        }
    }


    pub fn write_config_register(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) -> Option<Arc<Barrier>> {
        // Check if the access should be handled locally in configuration cache
        if let Ok(recognizer) = self.memory_recognizer.read() {
            if recognizer.should_handle_config_write_locally(reg_idx, offset, data, self.vfio_wrapper.as_ref()) {
                self.configuration.write_config_register(reg_idx, offset, data);
                return None;
            }
        }

        // Otherwise, write directly to VFIO device
        let cfg_offset = ((reg_idx * PCI_CONFIG_REGISTER_SIZE) as u64 + offset) as u32;
        self.vfio_wrapper.write_config(cfg_offset, data);
        None
    }

    pub fn read_config_register(&mut self, reg_idx: usize) -> u32 {
        // Check if the access should be handled locally in configuration cache
        let mut value = if let Ok(recognizer) = self.memory_recognizer.read() {
            if recognizer.should_handle_config_read_locally(reg_idx, self.vfio_wrapper.as_ref()) {
                self.configuration.read_reg(reg_idx)
            } else {
                self.vfio_wrapper.read_config_dword((reg_idx * PCI_CONFIG_REGISTER_SIZE) as u32)
            }
        } else {
            // Fallback to configuration
            self.vfio_wrapper.read_config_dword((reg_idx * PCI_CONFIG_REGISTER_SIZE) as u32)
        };

        // Header Type register has the multi-function bit as bit 23 in DWORD #3.
        // We currently expose a single function in the virtual topology.
        if reg_idx == 3 {
            value &= !(1u32 << 23);
        }

        value
    }

    pub fn move_bar(&mut self,old_base: u64, new_base: u64) -> Result<(), DeviceRelocationError> {
        // Let mmio manager attempt to move mappings if it implements it.
        // We also notify the memory recognizer about the BAR reprogramming
        if let Ok(mut recognizer) = self.memory_recognizer.write() {
            if let Some(bar_idx) = recognizer.bar_index_by_base(old_base) {
                recognizer.on_bar_reprogrammed(bar_idx as u8, old_base, new_base);
            }
        }

        Ok(())
    }


    pub fn read_bar(&mut self, base: u64, offset: u64, data: &mut [u8]) {
        // 1. 判断是否在读msix table 和 PBA
        // 1. 是：读vmm内存中的msix table -> 调用 virtio_interrupt_mgr
        // 2. 否：读vfio fd(vfio_wrapper)
        if let Ok(recognizer) = self.memory_recognizer.read() {
            if let Some((table_offset, _table_size, pba_offset, pba_size)) =
                recognizer.msix_layout_for_bar(self.vfio_wrapper.as_ref(), base)
            {
                let access_msix_vector = recognizer.is_access_msix_vector_register(self.vfio_wrapper.as_ref(), base, offset);

                if access_msix_vector {
                    if let Some(msix_offset) = offset.checked_sub(table_offset) {
                        self.msix_mgr.read().unwrap().read_table(msix_offset, data);
                        return;
                    }

                    if let Some(pba_rel) = offset.checked_sub(pba_offset) {
                        if pba_rel < pba_size {
                            self.msix_mgr.read().unwrap().read_pba(pba_rel, data);
                            return;
                        }
                    }
                }
            }

            if let Some(region_index) = recognizer
                .bar_index_by_base(base)
                .map(|bar_idx| VFIO_PCI_BAR0_REGION_INDEX + bar_idx as u32)
            {
                self.vfio_wrapper.region_read(region_index, offset, data);
                return;
            }
        }

        warn!("Failed to resolve VFIO BAR region for base={:#x}; defaulting to BAR0", base);
        self.vfio_wrapper
            .region_read(VFIO_PCI_BAR0_REGION_INDEX, offset, data);
    }

    pub fn write_bar(&mut self, base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        // 1. 判断是否在写msix table 和 PBA
        // 1. 是：写vmm内存中的msix table -> 调用 virtio_interrupt_mgr
        // 2. 否：继续
        // 3. 调用VfioBarOps的blacklist_filter
        // 4. 仅对BlackStatus::None的访问，写vfio fd(vfio_wrapper)
        if let Ok(recognizer) = self.memory_recognizer.read() {
            if let Some((table_offset, _table_size, pba_offset, pba_size)) =
                recognizer.msix_layout_for_bar(self.vfio_wrapper.as_ref(), base)
            {
                let access_msix_vector = recognizer.is_access_msix_vector_register(self.vfio_wrapper.as_ref(), base, offset);

                if access_msix_vector {
                    if let Some(msix_offset) = offset.checked_sub(table_offset) {
                        self.msix_mgr.write().unwrap().write_table(msix_offset, data);
                        return None;
                    }

                    if let Some(pba_rel) = offset.checked_sub(pba_offset) {
                        if pba_rel < pba_size {
                            self.msix_mgr.write().unwrap().write_pba(pba_rel, data);
                            return None;
                        }
                    }
                }
            }

            if let Some(bar_idx) = recognizer.bar_index_by_base(base) {
                let region_index = VFIO_PCI_BAR0_REGION_INDEX + bar_idx as u32;
                let access_reqs = self
                    .mmio_mgr
                    .read()
                    .unwrap()
                    .blacklist_filter(bar_idx, data.len() as u64, offset);

                for req in access_reqs {
                    if req.block_policy != BlackStatus::None {
                        continue;
                    }

                    let Some(rel_start) = req.bar_offset.checked_sub(offset) else {
                        continue;
                    };
                    let Ok(start) = usize::try_from(rel_start) else {
                        continue;
                    };
                    let Ok(seg_len) = usize::try_from(req.len) else {
                        continue;
                    };
                    if start >= data.len() {
                        continue;
                    }

                    let end = start.saturating_add(seg_len).min(data.len());
                    if start < end {
                        self.vfio_wrapper
                            .region_write(region_index, req.bar_offset, &data[start..end]);
                    }
                }
                return None;
            }
        }

        warn!("Failed to resolve VFIO BAR region for base={:#x}; defaulting to BAR0", base);
        self.vfio_wrapper
            .region_write(VFIO_PCI_BAR0_REGION_INDEX, offset, data);

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
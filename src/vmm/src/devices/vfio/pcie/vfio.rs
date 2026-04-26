
use std::{cmp, fmt};
use vfio_bindings::bindings::vfio::*;
use vfio_ioctls::{
    VfioContainer, VfioDevice, VfioIrq, VfioRegionInfoCap, VfioRegionSparseMmapArea,
};
use vmm_sys_util::eventfd::EventFd;
use std::sync::{Arc, Barrier, Mutex};
use log::{error, info};
use thiserror::Error;
use crate::pci::configuration::{PciCapability, PciConfiguration, PciConfigurationState};
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

const MSIX_TABLE_BAR_OFFSET: u64 = 0x8000;
const MSIX_TABLE_SIZE: u64 = 0x40000;
const MSIX_PBA_BAR_OFFSET: u64 = 0x48000;
const MSIX_PBA_SIZE: u64 = 0x800;


pub(crate) struct VfioCommon {
    pub(crate) configuration: PciConfiguration,
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
        subclass: &dyn PciSubclass,
        vfio_wrapper: Arc<dyn Vfio>,
        msix_vectors: MsixVectorGroup
    ) -> Self{
        // 1. 初始化 PciConfiguration
        let configuration = PciConfiguration::new_type0(0, 0, 0, 
            PciClassCode::Other, subclass, 0, 0, None);  // guest的vendor等id的请求会直通到vfio fd，不用PciConfiguration对象处理

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

        let cfg_offset = (reg_idx * 4 + crate::utils::u64_to_usize(offset)) as u32;
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
            self.vfio_wrapper.read_config_dword((reg_idx * 4) as u32)
        };

        // Header Type register has the multi-function bit as bit 23 in DWORD #3.
        // We currently expose a single function in the virtual topology.
        if reg_idx == 3 {
            value &= !(1u32 << 23);
        }

        value
    }

    pub fn read_bar(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        // 1. 判断是否在读msix table
        // 1. 是：读vmm内存中的msix table -> 调用 virtio_interrupt
        // 2. 否：读vfio fd(vfio_wrapper)
        if self.virtio_interrupt.is_some() && Self::is_access_msix_vector_register(_base, offset) {
            if let Some(irq) = &self.virtio_interrupt {
                let mut msix = irq.msix_config.lock().expect("Poisoned lock");
                if (MSIX_TABLE_BAR_OFFSET..MSIX_TABLE_BAR_OFFSET + MSIX_TABLE_SIZE).contains(&offset)
                {
                    msix.read_table(offset - MSIX_TABLE_BAR_OFFSET, data);
                    return;
                }
                if (MSIX_PBA_BAR_OFFSET..MSIX_PBA_BAR_OFFSET + MSIX_PBA_SIZE).contains(&offset) {
                    msix.read_pba(offset - MSIX_PBA_BAR_OFFSET, data);
                    return;
                }
            }
        }

        self.vfio_wrapper
            .region_read(VFIO_PCI_BAR0_REGION_INDEX, offset, data);
    }

    pub fn write_bar(&mut self, _base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        // 1. 判断是否在写msix table
        // 1. 是：写vmm内存中的msix table -> 调用 virtio_interrupt
        // 2. 否：写vfio fd(vfio_wrapper)
        if self.virtio_interrupt.is_some() && Self::is_access_msix_vector_register(_base, offset) {
            if let Some(irq) = &self.virtio_interrupt {
                let mut msix = irq.msix_config.lock().expect("Poisoned lock");
                if (MSIX_TABLE_BAR_OFFSET..MSIX_TABLE_BAR_OFFSET + MSIX_TABLE_SIZE).contains(&offset)
                {
                    msix.write_table(offset - MSIX_TABLE_BAR_OFFSET, data);
                    return None;
                }
                if (MSIX_PBA_BAR_OFFSET..MSIX_PBA_BAR_OFFSET + MSIX_PBA_SIZE).contains(&offset) {
                    msix.write_pba(offset - MSIX_PBA_BAR_OFFSET, data);
                    return None;
                }
            }
        }

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


    fn is_access_bar_register(reg_idx: usize, offset: u64, data: &[u8]) -> bool{
        if !(4..10).contains(&reg_idx) {
            return false;
        }

        crate::utils::u64_to_usize(offset) + data.len() <= 4
    }

    fn is_access_misx_capabilities(&self, reg_idx: usize, offset: u64, data: &[u8]) -> bool{
        let Some(msix_cap_offset) = self.find_msix_cap_offset() else {
            return false;
        };

        let access_start = reg_idx * 4 + crate::utils::u64_to_usize(offset);
        let access_end = access_start + cmp::max(data.len(), 1);

        let cap_start = msix_cap_offset;
        // Capability header (2) + Message Control (2) + Table (4) + PBA (4)
        let cap_end = cap_start + 12;

        access_start < cap_end && cap_start < access_end
    }

    fn find_msix_cap_offset(&self) -> Option<usize> {
        let read_cfg_byte = |cfg: &PciConfiguration, byte_off: usize| -> u8 {
            let reg = cfg.read_reg(byte_off / 4);
            ((reg >> ((byte_off % 4) * 8)) & 0xff) as u8
        };

        let mut cap_ptr = usize::from(read_cfg_byte(&self.configuration, 0x34));
        let mut guard = 0usize;

        while cap_ptr >= 0x40 && cap_ptr < 0x100 && guard < 48 {
            let cap_id = read_cfg_byte(&self.configuration, cap_ptr);
            if cap_id == PciCapabilityId::MsiX as u8 {
                return Some(cap_ptr);
            }

            let next = usize::from(read_cfg_byte(&self.configuration, cap_ptr + 1));
            if next == 0 || next == cap_ptr {
                break;
            }
            cap_ptr = next;
            guard += 1;
        }

        None
    }

    fn is_access_msix_vector_register(_base: u64, offset: u64) -> bool{
        (MSIX_TABLE_BAR_OFFSET..MSIX_TABLE_BAR_OFFSET + MSIX_TABLE_SIZE).contains(&offset)
            || (MSIX_PBA_BAR_OFFSET..MSIX_PBA_BAR_OFFSET + MSIX_PBA_SIZE).contains(&offset)
    }
}
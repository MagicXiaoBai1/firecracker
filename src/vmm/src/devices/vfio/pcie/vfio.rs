
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


pub(crate) struct VfioCommon {
    pub(crate) configuration: PciConfiguration,
    // TODO pub(crate) mmio_regions: Vec<MmioRegion>,
    // TODO 中断
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
        subclass: &dyn PciSubclass,
        vfio_wrapper: Arc<dyn Vfio>
    ) -> Self{
        // 1. 初始化 PciConfiguration
        let configuration = PciConfiguration::new_type0(0, 0, 0, 
            PciClassCode::Other, subclass, 0, 0, None);  // guest的vendor等id的请求会直通到vfio fd，不用PciConfiguration对象处理
        let virtio_interrupt = Option::None;
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
        // TODO
        // 1. 判断是否在写bar寄存器：如果是就写PciConfiguration对象，然后返回
        // 2. 判断是否在使能misx or msi
        // 3. 读写vfio fd(vfio_wrapper)
        // 4. 根据MSE bit的值处理 bar reprogram（好像什么都不用做）因为bar reprogram（移动bar空间的HPA）不会发生
        None
    }

    pub fn read_config_register(&mut self, reg_idx: usize) -> u32 {
        // TODO 
        // 1. 判断是否在读bar寄存器：如果是就读PciConfiguration对象，然后返回
        // 2. 判断是否在读misx or msi能力
        // 3. mask multi-function bit
        // 4. 读vfio fd(vfio_wrapper)
        // 5. 处理mask和patch
        0
    }

    pub fn read_bar(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        // TODO
        // 1. 判断是否在读msix table
        // 1. 是：读vmm内存中的msix table -> 调用 virtio_interrupt
        // 2. 否：读vfio fd(vfio_wrapper)
    }

    pub fn write_bar(&mut self, _base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        // TODO
        // 1. 判断是否在写msix table
        // 1. 是：写vmm内存中的msix table -> 调用 virtio_interrupt
        // 2. 否：写vfio fd(vfio_wrapper)
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
        vm: VmCommon,
        // TODO
    ) {
        // TODO
        // 调用vfio_wrapper获得设备可直通的bar空间的mmap

    }

    pub(crate) fn set_vfio_bar_in_kvm(
        &mut self,
        vm: VmCommon,
        // TODO
    ) {
        // TODO
        // 将设备可直通的bar空间的mmap的HPA 配置给guest的GPA map HPA

    }


    fn is_access_bar_register(reg_idx: usize, offset: u64, data: &[u8]) -> bool{
        // TODO 判断guest mmio的地址是否为pcie配置空间的bar寄存器
        false
    }

    fn is_access_misx_capabilities(reg_idx: usize, offset: u64, data: &[u8]) -> bool{
        // TODO 判断guest mmio的地址是否为pcie配置空间的misx能力
        false
    }

    fn is_access_msix_vector_register(base: u64, offset: u64) -> bool{
        // TODO 判断guest mmio的地址是否为pcie bar 空间的msix_vector
        false
    }
}
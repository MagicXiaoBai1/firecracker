
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, RwLock};
use crate::devices::vfio::pcie::memory_recognizer::{AccessResolution, IovaAccessData, IovaAccessType};
use crate::devices::vfio::pcie::vfio::{VfioDeviceWrapper, VfioMsixOps};
use crate::vstate::bus::BusDeviceSync;
use crate::{EventManager, Vm};
use std::os::unix::io::AsRawFd;

use log::{debug, error, info, warn};
use thiserror::Error;
use vm_allocator::AddressAllocator;

use super::configuration::VfioPcieConfiguration;
use super::memory_recognizer::{MemoryRecognizer, VfioPcieMemoryRecognizer};
use super::vfio::Vfio;
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

const PCI_CONFIG_REGISTER_SIZE: usize = 4;

trait MemoryRecognizerDebug: MemoryRecognizer + fmt::Debug + Send + Sync {}
impl<T> MemoryRecognizerDebug for T where T: MemoryRecognizer + fmt::Debug + Send + Sync {}
trait VfioMsixOpsDebug: VfioMsixOps + fmt::Debug + Send + Sync {}
impl<T> VfioMsixOpsDebug for T where T: VfioMsixOps + fmt::Debug + Send + Sync {}

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
/// 该对象不需要线程安全（开玩笑怎么可以让两个cpu核并行的修改一个设备的状态）
#[derive(Debug)]
pub(crate) struct VfioCommon {
    // 标识符：
    
    id: u32,
    subclass: u8, // 设备子类，决定了设备的类型和功能，影响配置空间布局和访问行为

    // 业务对象：
    
    pub(crate) configuration: VfioPcieConfiguration,
    /// MSI-X interrupt handler (locked separately for interrupt operations)
    msix_ops: Box<dyn VfioMsixOpsDebug>,
    /// Memory region recognizer for routing GPA access requests
    pub(crate) memory_recognizer: Arc<RwLock<Box<dyn MemoryRecognizerDebug>>>,
    
    // 底层对象：
    vfio_container: VfioContainerWarp,
    vfio_wrapper: VfioDeviceWrapper,
}



impl VfioCommon {

    pub(crate) fn new(
        id: u32,
        subclass: Box<dyn PciSubclass>,
        vfio_wrapper: VfioDeviceWrapper,
        msix_vectors: MsixVectorGroup,
        vfio_container: Arc<VfioContainer>,
    ) -> Self {
        let memory_recognizer =VfioPcieMemoryRecognizer::new(&vfio_wrapper);

        let configuration = VfioPcieConfiguration::new(memory_recognizer.get_bar_region_size());

        let msix_ops = VfioInterruptEngine::new(id, msix_vectors);
        
        Self {
            id,
            subclass: subclass.get_register_value(),
            configuration,
            msix_ops: Box::new(msix_ops),
            memory_recognizer: Arc::new(RwLock::new(Box::new(memory_recognizer))),
            vfio_container: VfioContainerWarp(vfio_container),
            vfio_wrapper
        }
    }


    pub fn write_config_register(  
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) -> Option<Arc<Barrier>> {
        // Check if the access should be handled locally in configuration cache
        let memory_recognizer = self.memory_recognizer.read().unwrap();
        let access_resolutions = memory_recognizer.parse_ecam_write(reg_idx, offset, data);

        let AccessResolution { access_type, .. } = access_resolutions.first().unwrap();
        match access_type {
            IovaAccessType::BarReg { index } => {
                self.configuration.write_config_register(reg_idx, offset, data);
            },
            IovaAccessType::MsixCtrl => { 
                self.msix_ops.set_msg_ctl(u16::from_le_bytes(data[0..2].try_into().unwrap())); // todo 这个转换可能不严谨
            }
            IovaAccessType::EcamCanForwardToVfio{ reg_idx, offset, data_len} => {
                let cfg_offset = (reg_idx * 4) as u64 + offset;
                self.vfio_wrapper.write_config(cfg_offset as u32, data);

            }
            _ => {
                warn!("Unsupported BAR access type for write: {:?}. Ignoring.", access_type);
            }
        }
        None
    }

    pub fn read_config_register(&mut self, reg_idx: usize) -> u32 {
        let mut data = [0 as u8; 4];
        let memory_recognizer = self.memory_recognizer.read().unwrap();
        let access_resolutions = memory_recognizer.parse_ecam_read(reg_idx, &mut data);

        let value;
        let AccessResolution { access_type, data } = access_resolutions.first().unwrap();
        match access_type {
            IovaAccessType::BarReg { index } => {
                value = self.configuration.read_config_register(reg_idx);
            },
            IovaAccessType::MsixCtrl => { // todo 这个分支没做好，guest查询pba位置和msix table位置时需要直接返回其位置，相关内容还需要对cloud hypervisor进行调研
                value = 0;
            }
            IovaAccessType::EcamCanForwardToVfio{ reg_idx, offset, data_len} => {
                value = self.vfio_wrapper.read_config_dword((reg_idx * 4) as u32);
            }
            _ => {
                value = 0;
                warn!("Unsupported BAR access type for read: {:?}. Ignoring.", access_type);
            }
        }


        // Header Type register has the multi-function bit as bit 23 in DWORD #3.
        // We currently expose a single function in the virtual topology.
        if reg_idx == 3 {
            value & !(1u32 << 23)
        } else {
            value
        }
    }

    
    pub fn move_bar(&mut self,old_base: u64, new_base: u64) -> Result<(), DeviceRelocationError> {
        // Let mmio manager attempt to move mappings if it implements it.
        // We also notify the memory recognizer about the BAR reprogramming
        if let Ok(mut recognizer) = self.memory_recognizer.write() {
            recognizer.on_bar_reprogrammed(old_base, new_base);
        }

        Ok(())
    }

    pub fn read_bar(&mut self, base: u64, offset: u64, data: &mut [u8]) {
        // 1. 判断是否在读msix table 和 PBA
        // 1. 是：读vmm内存中的msix table -> 调用 virtio_interrupt_mgr
        // 2. 否：读vfio fd(vfio_wrapper)
        let memory_recognizer = self.memory_recognizer.read().unwrap();
        let access_resolutions = memory_recognizer.parse_bar_read(base, offset, data);


        for AccessResolution { access_type, data } in access_resolutions {

            let mut raw_data;
            if let IovaAccessData::Read { data } = data {
                raw_data = data;
            } else {
                warn!("Unexpected IovaAccessData type for read: {:?}. Ignoring.", data);
                continue;
            }

            match access_type {
                IovaAccessType::MsixTable { index, offset, data_len } => {
                    self.msix_ops.read_table(index, offset, &mut raw_data);
                },
                IovaAccessType::BarRegionCanForwardToVfio{ index, offset, data_len } => {
                    self.vfio_wrapper.region_read(index as u32, offset, &mut raw_data);
                }
                // TODO 黑名单
                _ => {
                    warn!("Unsupported BAR access type for read: {:?}. Ignoring.", access_type);
                }
            }
        }
    }

    pub fn write_bar(&mut self, base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        // 1. 判断是否在写msix table 和 PBA
        // 1. 是：写vmm内存中的msix table -> 调用 virtio_interrupt_mgr
        // 2. 否：继续
        // 3. 调用VfioBarOps的blacklist_filter
        // 4. 仅对BlackStatus::None的访问，写vfio fd(vfio_wrapper)

        let memory_recognizer = self.memory_recognizer.read().unwrap();
        let access_resolutions = memory_recognizer.parse_bar_write(base, offset, data);


        for AccessResolution { access_type, data } in access_resolutions {

            let raw_data;
            if let IovaAccessData::Write { data } = data {
                raw_data = data;
            } else {
                warn!("Unexpected IovaAccessData type for write: {:?}. Ignoring.", data);
                continue;
            }

            match access_type {
                IovaAccessType::MsixTable { index, offset, data_len } => {
                    self.msix_ops.write_table(index, offset, &raw_data);
                },
                IovaAccessType::BarRegionCanForwardToVfio{ index, offset, data_len } => {
                    self.vfio_wrapper.region_write(index as u32, offset, raw_data);
                }
                // TODO 黑名单
                _ => {
                    warn!("Unsupported BAR access type for write: {:?}. Ignoring.", access_type);
                }
            }
        }

        None
    }

    pub fn detect_bar_reprogramming(
        &mut self,
        reg_idx: usize,
        data: &[u8],
    ) -> Option<BarReprogrammingParams> {
        self.memory_recognizer.write().unwrap().detect_bar_reprogramming(reg_idx, data)
    }

    pub(crate) fn init_bar_ragion(
        &mut self,
        mmio64_memory: AddressAllocator
    ) {
        // 1. 调用 memory_recognizer 获取设备的BAR信息（base/size/type）
        // 2. 根据BAR信息在 mmio64_memory 中分配地址资源
        // 3. 调用 memory_recognizer 获取可直通的区域
        // 4. 调用 map_mmio_regions 建立地址映射
        info!("init_bar_region is not implemented yet");
    }


}


#[allow(dead_code)]
pub struct VfioContainerWarp(pub Arc<VfioContainer>);

impl std::fmt::Debug for VfioContainerWarp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VfioContainerWarp").finish()
    }
}
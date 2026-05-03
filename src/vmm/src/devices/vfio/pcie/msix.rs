
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use log::{error, info, warn};
use thiserror::Error;

use super::configuration::{VfioBarRegionInfo, VfioPcieConfiguration};
use super::mmio_mgr::VfioMmioEngine;
use crate::Vm;
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
use super::vfio::{Vfio, VfioMsixOps};

pub struct VfioInterruptEngine {
    pub(crate) vfio_wrapper: Arc<dyn Vfio>,
    msix_config: Arc<Mutex<MsixConfig>>,
    vectors: Arc<MsixVectorGroup>,
    vm: Arc<Vm>
}

impl fmt::Debug for VfioInterruptEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VfioInterruptEngine").finish_non_exhaustive()
    }
}

impl VfioInterruptEngine {
    pub(crate) fn new(
        vfio_wrapper: Arc<dyn Vfio>,
        id: u32,
        msix_vectors: MsixVectorGroup,
        vm: Arc<Vm>
    ) -> Self {
        let msix_vectors = Arc::new(msix_vectors);
        let msix_config = Arc::new(Mutex::new(MsixConfig::new(msix_vectors.clone(), id)));

        Self {
            vfio_wrapper,
            msix_config,
            vectors: msix_vectors,
            vm
        }
    }
}

impl VfioMsixOps for VfioInterruptEngine {
    fn read_table(&self, _offset: u64, _data: &mut [u8]) {
        // unimplemented!()
    }

    fn write_table(&mut self, _offset: u64, _data: &[u8]) {
        // unimplemented!()
    }

    fn set_msg_ctl(&mut self, _reg: u16) {
        // unimplemented!()
    }

    fn read_pba(&self, _offset: u64, _data: &mut [u8]) {
        // unimplemented!()
    }

    fn write_pba(&mut self, _offset: u64, _data: &[u8]) {
        // unimplemented!()
    }

    fn set_pba_bit(&mut self, _vector: u16, _reset: bool) {
        // 从guest armv8cpu角度看一条机器指令不能读写单bit
        unimplemented!()
    }

    fn get_pba_bit(&self, _vector: u16) -> u8 {
        // 从guest armv8cpu角度看一条机器指令不能读写单bit
        unimplemented!()
    }

    fn inject_msix_and_clear_pba(&mut self, _vector: usize) {
        unimplemented!()
    }
}

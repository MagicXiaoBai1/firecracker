use std::ops::DerefMut;
use std::sync::Arc;

use crate::devices::vfio::pcie::vfio::{Vfio, VfioError};
use crate::pci::{BarReprogrammingParams, DeviceRelocationError};
use crate::vstate::bus::BusDeviceSync;
use crate::{EventManager, Vm};

use super::configuration::VfioBarRegionInfo;
use super::vfio::VfioBarOps;

const BAR0_REG: usize = 4;
const NUM_BAR_REGS: usize = 6;
const BAR_MEM_ADDR_MASK: u32 = 0xffff_fff0;

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct VfioMmioBlacklistEntry {
    pub bar_region_id: usize,
    pub offset: u64,
    pub len: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct VfioMmioPassthroughState {
    pub gpa: u64,
    pub len: u64,
    pub bar_region_id: usize,
    pub bar_offset: u64,
}


pub(crate) struct VfioMmioEngine {
    vm: Arc<Vm>,
    vfio_wrapper: Arc<dyn Vfio>,
    blacklist: [Option<VfioMmioBlacklistEntry>; NUM_BAR_REGS],
    passthrough_status: [Option<VfioMmioPassthroughState>; NUM_BAR_REGS],
}

impl VfioMmioEngine {
    pub(crate) fn new(vm: Arc<Vm>, vfio_wrapper: Arc<dyn Vfio>,) -> Self {
        Self {
            vm,
            vfio_wrapper,
            blacklist: [None; NUM_BAR_REGS],
            passthrough_status: [None; NUM_BAR_REGS],
        }
    }

    pub(crate) fn set_blacklist(
        &mut self,
        bar_region_id: usize,
        offset: u64,
        len: u64,
    ) {
        if bar_region_id < NUM_BAR_REGS {
            self.blacklist[bar_region_id] = Some(VfioMmioBlacklistEntry {
                bar_region_id,
                offset,
                len,
            });
        }
    }

    /// Map MMIO regions into the guest, and avoid VM exits when the guest tries
    /// to reach those regions.
    ///
    /// # Arguments
    ///
    /// * `vm` - The VM object. It is used to set the VFIO MMIO regions
    ///   as user memory regions.
    /// * `mem_slot` - The closure to return a memory slot.
    pub fn map_mmio_region(&mut self, bar_region_info: &[VfioBarRegionInfo; NUM_BAR_REGS]) -> Result<(), VfioError> {
        Ok(())
    }

}

impl VfioBarOps for VfioMmioEngine {
    fn move_bar(
        &mut self,
        old_base: u64,
        new_base: u64,
        len: u64,
        host_device_offset: u64,
    ) -> Result<(), DeviceRelocationError>{
        Ok(())
    }

    fn allocate_bars(
        &mut self,
        guest_base: u64,
        len: u64,
        host_device_offset: u64,

    ) -> Result<(), Box<dyn std::error::Error>>{
        let mut resource_allocator_lock = self.vm.resource_allocator();
        let resource_allocator = resource_allocator_lock.deref_mut();

        // for bar in bar_region_info
        //     let virtio_pci_bar_addr = mmio64_allocator
        //         .allocate(
        //             one_BAR_SIZE,
        //             one_BAR_SIZE,
        //             AllocPolicy::FirstMatch,
        //         )
        //         .unwrap()
        //         .start();
        


        //     如下的逻辑不在这里做，在vfio device上层做
        //     vm.common.mmio_bus.insert(
        //         virtio_device.clone(),
        //         virtio_device_locked.bar_address,
        //         CAPABILITY_BAR_SIZE,
        //     )?;

        Ok(())
    }

    fn free_bars(&mut self, _guest_base: u64, _len: u64) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    fn read_bar(&self, base: u64, offset: u64, data: &mut [u8]) {
    }

    fn write_bar(&self, base: u64, offset: u64, data: &[u8]) -> Option<Arc<std::sync::Barrier>> {
        None
    }
}
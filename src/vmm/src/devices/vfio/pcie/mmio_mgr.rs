use std::sync::Arc;

use crate::pci::BarReprogrammingParams;
use crate::vstate::vm::VmCommon;

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

#[derive(Debug, Default)]
pub(crate) struct VfioMmioEngine {
    blacklist: [Option<VfioMmioBlacklistEntry>; NUM_BAR_REGS],
    passthrough_status: [Option<VfioMmioPassthroughState>; NUM_BAR_REGS],
}

impl VfioMmioEngine {
    pub(crate) fn new() -> Self {
        Self::default()
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


    pub(crate) fn move_bar(
        &mut self,
        bar_idx: usize,
        old_base: u64,
        new_base: u64,
        len: u64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if bar_idx >= NUM_BAR_REGS {
            return Err("Invalid BAR index".into());
        }

        Ok(())
    }

    pub(crate) fn allocate_bars(
        &mut self,
        _vm: VmCommon,
        bar_region_info: &[VfioBarRegionInfo; NUM_BAR_REGS],
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Synchronize passthrough status with current BAR region info
        self.sync_passthrough_status(bar_region_info);
        Ok(())
    }

    pub(crate) fn sync_passthrough_status(
        &mut self,
        bar_region_info: &[VfioBarRegionInfo; NUM_BAR_REGS],
    ) {
        for (bar_idx, info) in bar_region_info.iter().copied().enumerate() {
            self.passthrough_status[bar_idx] = if info.used {
                Some(VfioMmioPassthroughState {
                    gpa: u64::from(info.addr & BAR_MEM_ADDR_MASK),
                    len: u64::from((!info.size_mask).wrapping_add(1)),
                    bar_region_id: bar_idx,
                    bar_offset: 0,
                })
            } else {
                None
            };
        }
    }

    pub(crate) fn read_bar(&self, _base: u64, _offset: u64, _data: &mut [u8]) {

    }

    pub(crate) fn write_bar(&self, _base: u64, _offset: u64, _data: &[u8]) -> Option<Arc<std::sync::Barrier>> {
        None
    }

}

impl VfioBarOps for VfioMmioEngine {
    fn move_bar(
        &mut self,
        bar_idx: usize,
        old_base: u64,
        new_base: u64,
        len: u64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        VfioMmioEngine::move_bar(self, bar_idx, old_base, new_base, len)
    }

    fn allocate_bars(
        &mut self,
        vm: VmCommon,
        bar_region_info: &[VfioBarRegionInfo; NUM_BAR_REGS],
    ) -> Result<(), Box<dyn std::error::Error>> {
        VfioMmioEngine::allocate_bars(self, vm, bar_region_info)
    }

    fn read_bar(&self, base: u64, offset: u64, data: &mut [u8]) {
        VfioMmioEngine::read_bar(self, base, offset, data)
    }

    fn write_bar(&self, base: u64, offset: u64, data: &[u8]) -> Option<Arc<std::sync::Barrier>> {
        VfioMmioEngine::write_bar(self, base, offset, data)
    }
}
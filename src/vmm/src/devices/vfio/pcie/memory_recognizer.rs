use std::sync::Arc;
use vfio_ioctls::{VfioDevice, VfioDeviceFd, VfioRegionInfoCap};
use crate::devices::vfio::pcie::vfio::Vfio;
use crate::utils::u64_to_usize;
use pci::PciCapabilityId;
const NUM_BAR_REGS: usize = 6;
const NUM_CONFIGURATION_REGISTERS: usize = 1024;
const BAR_MEM_ADDR_MASK: u32 = 0xffff_fff0;
const PCI_CONFIG_CAPABILITY_OFFSET: u32 = 0x34;
const PCI_CONFIG_CAPABILITY_PTR_MASK: u8 = 0xfc;
const MSIX_TABLE_ENTRY_SIZE: u64 = 16;

struct BarLocation {
    pub index: u8,
    pub offset: u64,
    pub len: u64,
}

/// IOVA types recognized from a VFIO device
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IovaAccessType {
    /// PCIe ECAM space (Configuration space)
    Ecam{ reg_idx: usize, offset: u64, data_len: u8 },
    /// BAR register (PCI configuration register)
    BarReg { index: u8 },
    /// MSI-X control registers 一般就是16bit整体读写
    MsixCtrl,
    /// MSI-X vector table
    MsixTable { index: u16 },
    /// MSI-X Pending Bit Array (PBA)
    Pba { offset: u16, data_len: u8 },
    /// BAR memory region (general MMIO)
    BarMem { index: u8, offset: u64, data_len: u8 },
    /// Memory-only emulation region (no direct device access)
    MmioNeededEmulationInMem,
    /// Discarded region (not accessible)
    MmioNeededDiscard,
}

#[derive(Debug, PartialEq, Eq)]
pub enum IovaAccessData<'a> {
    Read{data: &'a mut [u8]},
    Write{data: &'a [u8]},
}

/// Result of resolving a GPA access request
#[derive(Debug)]
pub struct AccessResolution<'a> {
    /// The region being accessed
    pub access_type: IovaAccessType,
    pub data: IovaAccessData<'a>,
}
#[derive(Debug, Default, Clone, Copy)]
struct VfioBarRegionInfo {
    pub addr: u32,
    pub size: u32,
    pub used: bool,
}

pub trait MemoryRecognizer: Send + Sync {
    fn parse_bar_accesses(base: u64, offset: u64, data: &mut [u8]) -> Vec<AccessResolution>;
    fn parse_ecam_access(reg_idx: usize,offset: u64, data: &[u8]) -> Vec<AccessResolution>;
    fn on_bar_reprogrammed(&mut self, bar_idx: u8, old_gpa: u64, new_gpa: u64);

}
/// Information about VFIO MMIO region.
#[derive(Clone, Debug)]
pub struct VfioRegion {
    pub(crate) flags: u32,
    pub(crate) size: u64,
    pub(crate) offset: u64,
    pub(crate) caps: Vec<VfioRegionInfoCap>,
}
/// VfioPcieMemoryRecognizer: Real implementation for VFIO PCIe devices.
///
/// This implementation parses BAR regions and MSI-X layouts from a VFIO device
/// and provides region lookup for memory access routing.
///
/// The regions are stored sorted by GPA to enable efficient binary search.
pub struct VfioPcieMemoryRecognizer {
    /// Optional cached MSIX capability register index
    bar_region_info: Vec<VfioRegion>,
    bar_reg_indices: [usize; NUM_BAR_REGS],
    msix_cap_reg_idx: Option<usize>,
    msix_pba_location: Option<BarLocation>,
    msix_vector_location: Option<BarLocation>,
}

impl VfioPcieMemoryRecognizer {
    /// Create a new recognizer with the given regions.
    pub fn new(vfio_dev: &VfioDevice) -> Self {
        // 1. Parse BAR regions from VFIO device
        let mut bar_region_info = Vec::new();
        for i in 0..NUM_BAR_REGS {
            let i = i as u32;
            let region_info = VfioRegion{
                flags: vfio_dev.get_region_flags(i),
                size: vfio_dev.get_region_size(i),
                offset: vfio_dev.get_region_offset(i),
                caps: vfio_dev.get_region_caps(i),
            };
            bar_region_info.push(region_info);
        }
        let mut bar_reg_indices = [0x10,0x14,0x18,0x1C,0x20,0x24];
      
        // 2. Parse MSI-X capability and determine vector table and PBA locations
        let msix_cap_reg_idx = Self::find_msix_capability(vfio_dev);
        let (msix_vector_location, msix_pba_location) = if let Some(idx) = msix_cap_reg_idx {
            Self::parse_msix_capability(vfio_dev, idx)
        } else {
            // No MSI-X capability found, use dummy locations
            (None, None)
        };

        Self {
            bar_region_info,
            bar_reg_indices,
            msix_cap_reg_idx,
            msix_vector_location,
            msix_pba_location,
        }
    }




    // msix ======================================================================================

    /// Check if accessing msg_ctl field in MSIX capability
    /// msg_ctl is located at capability_offset + 2 (word, 2 bytes)
    fn is_access_msg_ctl(&mut self, reg_idx: usize, offset: u64, data: &[u8]) -> bool {
    }

    fn is_access_msix_vector(
        &mut self,
        base: u64,
        offset: u64,
    ) -> bool {
    }

    fn is_access_pba(
        &mut self,
        base: u64,
        offset: u64,
    ) -> bool {
    }


}

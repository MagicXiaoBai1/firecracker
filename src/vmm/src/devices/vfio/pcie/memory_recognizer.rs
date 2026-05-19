use std::sync::Arc;
use vfio_ioctls::VfioDeviceFd;

/// IOVA types recognized from a VFIO device
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IovaType {
    Ecam,
    BarReg { index: u8 },
    MsixCtrl,
    MsixTable,
    Pba,
    MmioMem,
    Discard,
}

/// Region info for a device IOVA segment
#[derive(Debug, Clone)]
pub struct RegionInfo {
    pub iova: u64,
    pub len: u64,
    pub typ: IovaType,
}

/// MemoryRecognizer: parse device regions and map GPAs to regions.
///
/// NOTE: existing code in `configuration.rs` / `VfioPcieConfiguration` already
/// provides BAR / MSIX layout detection and some helpers. This trait is a
/// higher-level abstraction planned by the design; the existing helpers can be
/// reused inside an implementation of this trait.
pub trait MemoryRecognizer: Send + Sync {
    /// Parse device regions from a VFIO device FD. Return the list of regions.
    fn parse_device_regions(vfio_dev: &VfioDeviceFd) -> Vec<RegionInfo>
    where
        Self: Sized;

    /// Given a guest physical address (GPA), find the region it belongs to and
    /// return (RegionInfo, offset_into_region).
    fn find_region(&self, gpa: u64) -> Option<(RegionInfo, u64)>;
}

/// A simple placeholder implementation that currently returns no regions.
///
/// TODO: implement actual parsing using `vfio_dev.get_region_info` /
/// configuration helpers; for now this is a scaffold to satisfy the compiler
/// and to be extended later.
pub struct DummyMemoryRecognizer {
    regions: Vec<RegionInfo>,
}

impl DummyMemoryRecognizer {
    pub fn new() -> Self {
        Self { regions: Vec::new() }
    }
}

impl MemoryRecognizer for DummyMemoryRecognizer {
    fn parse_device_regions(_vfio_dev: &VfioDeviceFd) -> Vec<RegionInfo> {
        // TODO: call VFIO ioctls to enumerate regions and translate into RegionInfo
        Vec::new()
    }

    fn find_region(&self, _gpa: u64) -> Option<(RegionInfo, u64)> {
        // TODO: lookup region by gpa and return offset
        None
    }
}

use std::sync::Arc;
use vfio_ioctls::{VfioDevice, VfioDeviceFd, VfioRegionInfoCap};
use crate::devices::vfio::pcie::vfio::{Vfio, VfioDeviceWrapper};
use crate::utils::u64_to_usize;
use pci::PciCapabilityId;
const NUM_BAR_REGS: usize = 6;
const NUM_CONFIGURATION_REGISTERS: usize = 1024;
const BAR_MEM_ADDR_MASK: u32 = 0xffff_fff0;
const PCI_CONFIG_CAPABILITY_OFFSET: u32 = 0x34;
const PCI_CONFIG_CAPABILITY_PTR_MASK: u8 = 0xfc;
const MSIX_TABLE_ENTRY_SIZE: u64 = 16;

struct BarLocation {
    pub bar_index: u8,
    pub offset: u64,
    pub len: u64,
}

/// IOVA types recognized from a VFIO device
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IovaAccessType {
    /// BAR register (PCI configuration register)
    BarReg { index: u8 },
    /// MSI-X control registers 一般就是16bit整体读写
    MsixCtrl,
    /// 除去上两点的其他 PCIe ECAM space (Configuration space)
    Ecam{ reg_idx: usize, offset: u64, data_len: u8 },
    /// MSI-X vector table
    MsixTable { index: u16 },
    /// MSI-X Pending Bit Array (PBA)
    Pba { offset: u16, data_len: u8 },
    /// 除去上两块的其他BAR memory region (general MMIO)
    BarRegion { index: u8, offset: u64, data_len: u8 },
    /// Memory-only emulation region (no direct device access)
    MmioNeededEmulationInMem,
    /// Discarded region (not accessible)
    MmioNeededDiscard,
    /// --------------- ARMv8 中的非法访问判断标准 ---------------
    /// 1. 对齐规则（Device 内存强制）
    ///    读n字节 需满足 offset % n == 0
    ///    n 只能为 1/2/4/8
    /// 2. 绝对禁止跨越的边界（全覆盖）
    ///    - PBA
    ///    - MSI-X Vector
    ///    - BarReg区域（6*32bit）
    ///    - PCIe 能力区域
    ///    - ECAM 4KB 功能边界
    ///    - MSI-X Table 16 字节表项边界
    ///    - PBA 8 字节单元边界
    ///    - ECAM 256 字节兼容配置空间边界
    /// 3. 访问宽度必须匹配硬件寄存器粒度
    ///    - MsixCtrl：只能 2 字节
    ///    - BarReg：写只能 4 字节
    /// 4. 所有访问必须完全落在单一功能/区域内
    ///    不允许一次访问同时覆盖两个不同区域
    InvalidAccess,
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
    bar_reg_indices: [u32; NUM_BAR_REGS],

    msix_clt_reg_offset_start: u32,
    msix_clt_reg_offset_end: u32,

    msix_pba_location: Option<BarLocation>,
    msix_vector_location: Option<BarLocation>,
}

impl VfioPcieMemoryRecognizer {
    /// Create a new recognizer with the given regions.
    pub fn new(vfio_device: VfioDeviceWrapper) -> Self {

        let vfio_dev = vfio_device.get_vfio_device();
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

        
        // 2. 查找并初始化 MSI-X 相关配置
        if let Some(msix_cap_offset) = Self::find_msix_cap_offset(&vfio_device) {
            let msix_cap_offset = msix_cap_offset as u32;
            // MSI-X Control 寄存器的配置空间偏移范围
            let msix_clt_reg_offset_start = msix_cap_offset + 2;
            let msix_clt_reg_offset_end = msix_clt_reg_offset_start + 2;

            // 获取 MSI-X Table / PBA 的 BAR 位置信息
            let (msix_vector_location, msix_pba_location) =
                Self::msix_layout_for_bar(&vfio_device, msix_cap_offset);

            // 找到 MSI-X 能力，初始化完整字段
            Self {
                bar_region_info,
                bar_reg_indices,
                msix_clt_reg_offset_start,
                msix_clt_reg_offset_end,
                msix_pba_location: Some(msix_pba_location),
                msix_vector_location: Some(msix_vector_location),
            }
        } else {
            // 未找到 MSI-X 能力，寄存器偏移置 0，位置为 None
            Self {
                bar_region_info,
                bar_reg_indices,
                msix_clt_reg_offset_start: 0,
                msix_clt_reg_offset_end: 0,
                msix_pba_location: None,
                msix_vector_location: None,
            }
        }
    }


    fn find_msix_cap_offset(vfio: &dyn Vfio) -> Option<usize> {

        let mut cap_ptr = vfio.read_config_byte(PCI_CONFIG_CAPABILITY_OFFSET) & PCI_CONFIG_CAPABILITY_PTR_MASK;
        let mut guard = 0usize;

        while cap_ptr != 0 && guard < 48 {
            let cap_id = vfio.read_config_byte(cap_ptr.into());
            if cap_id == PciCapabilityId::MsiX as u8 {
                let cap_offset = usize::from(cap_ptr);
                return Some(cap_offset);
            }

            let next = vfio.read_config_byte((cap_ptr + 1).into()) & PCI_CONFIG_CAPABILITY_PTR_MASK;
            if next == 0 || next == cap_ptr {
                break;
            }
            cap_ptr = next;
            guard += 1;
        }
        None
    }

    /// 解析 MSI-X 配置空间，返回 Table 和 PBA 的 BAR 位置信息
    pub(crate) fn msix_layout_for_bar(
        vfio: &dyn Vfio,
        msix_cap_offset: u32,
    ) -> (BarLocation, BarLocation) {
        // 读取 MSI-X 能力寄存器
        let msg_ctl = vfio.read_config_word(msix_cap_offset + 2);
        let table = vfio.read_config_dword(msix_cap_offset + 4);
        let pba = vfio.read_config_dword(msix_cap_offset + 8);

        // 解析 Table 信息
        let table_bir = (table & 0x7) as u8; // BAR 编号，转 u8
        let table_offset = u64::from(table & 0xffff_fff8);
        // 计算大小
        let table_entries = u64::from((msg_ctl & 0x07ff) + 1);
        let table_size = table_entries * MSIX_TABLE_ENTRY_SIZE;

        // 解析 PBA 信息
        let pba_bir = (pba & 0x7) as u8; // BAR 编号，转 u8
        let pba_offset = u64::from(pba & 0xffff_fff8);
        let pba_size = table_entries.div_ceil(64) * 8;

        // 封装为 BarLocation 并返回
        let table_location = BarLocation {
            bar_index: table_bir,
            offset: table_offset,
            len: table_size,
        };

        let pba_location = BarLocation {
            bar_index: pba_bir,
            offset: pba_offset,
            len: pba_size,
        };

        (table_location, pba_location)
    }


}


pub trait MemoryRecognizer: Send + Sync {
    fn parse_bar_accesses(base: u64, offset: u64, data: &mut [u8]) -> Vec<AccessResolution>;
    fn parse_ecam_access(reg_idx: usize,offset: u64, data: &[u8]) -> Vec<AccessResolution>;
    fn on_bar_reprogrammed(&mut self, bar_idx: u8, old_gpa: u64, new_gpa: u64);

}

impl MemoryRecognizer for VfioPcieMemoryRecognizer{
    // TODO
}
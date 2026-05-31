use std::sync::Arc;
use log::info;
use vfio_ioctls::{VfioDevice, VfioDeviceFd, VfioRegionInfoCap};
use vm_allocator::AddressAllocator;
use crate::devices::vfio::pcie::vfio::{Vfio, VfioDeviceWrapper};
use crate::pci::BarReprogrammingParams;
use crate::utils::u64_to_usize;
use pci::PciCapabilityId;

const PCI_CONFIG_BAR0_INDEX: usize = 4;
const NUM_BAR_NUMS: usize = 6;
const PCI_ROM_EXP_BAR_INDEX: usize = 12;

const VFIO_REGION_INFO_FLAG_32BIT: u32 = 1 << 8;
const VFIO_REGION_INFO_FLAG_64BIT: u32 = 1 << 9;

// PCI config register size (4 bytes).
const PCI_CONFIG_REGISTER_SIZE: usize = 4;


const NUM_CONFIGURATION_REGISTERS: usize = 1024;
const BAR_MEM_ADDR_MASK: u32 = 0xffff_fff0;
const PCI_CONFIG_CAPABILITY_OFFSET: u32 = 0x34;
const PCI_CONFIG_CAPABILITY_PTR_MASK: u8 = 0xfc;
const MSIX_TABLE_ENTRY_SIZE: u64 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BarLocation {
    pub bar_index: u8,
    pub offset: u64,
    pub len: u64,
}
impl BarLocation {
    fn end(self) -> u64 {
        self.offset.saturating_add(self.len)
    }

    fn contains(self, offset: u64) -> bool {
        (self.offset..self.end()).contains(&offset)
    }
}

pub enum IovaAcccessRawType<'a>{
    WriteConfigRegister {reg_idx: usize, offset: u64, data: &'a [u8]},
    ReadConfigRegister {reg_idx: usize},
    ReadBar {base: u64, offset: u64, data: &'a mut [u8]},
    WriteBar {base: u64, offset: u64, data: &'a [u8]},
}

/// IOVA types recognized from a VFIO device
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IovaAccessType {
    /// BAR register (PCI configuration register)
    BarReg { index: u8 },
    /// MSI-X control registers 一般就是16bit整体读写
    MsixCtrl,
    /// 除去上两点的其他 PCIe ECAM space (Configuration space)
    EcamCanForwardToVfio{ reg_idx: usize, offset: u64, data_len: u8 },
    /// MSI-X vector table
    MsixTable { index: u16, offset: u64, data_len: u8},
    /// MSI-X Pending Bit Array (PBA)
    Pba { offset: u16, data_len: u8 },    // 目前用不上
    /// 除去上两块的其他BAR memory region (general MMIO)
    BarRegionCanForwardToVfio { index: u8, offset: u64, data_len: u8 },
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

/// Information about VFIO MMIO region.
#[derive(Clone, Debug)]
pub struct VfioRegion {
    // 动态的
    pub(crate) index: u32,
    pub(crate) start: u64,
    pub(crate) enable: bool,    // guest 是否可以访问（PS bar reporgram 需要写依次两个dword途中不能访问bar）

    // 静态的
    pub(crate) flags: u32,
    pub(crate) size: u64,
    pub(crate) offset: u64,
    pub(crate) caps: Vec<VfioRegionInfoCap>,
}
/// VfioPcieMemoryRecognizer: 识别内存访问行为
#[derive(Debug)]
pub struct VfioPcieMemoryRecognizer {
    bar_region_reprogramming_in_progress: [Option<u32>; NUM_BAR_NUMS], // 记录正在被重编程的BAR索引，期间该BAR被视为不可访问，值为寄存器中的旧值
    bar_region_info: Vec<VfioRegion>,

    msix_clt_reg_offset_start: usize,
    msix_clt_reg_offset_end: usize,

    msix_pba_location: Option<BarLocation>,
    msix_vector_location: Option<BarLocation>,

    region_need_discard: Vec<BarLocation>,
    region_need_emulation_in_mem: Vec<BarLocation>,
}

impl VfioPcieMemoryRecognizer {
    /// Create a new recognizer with the given regions.
    pub fn new(vfio_device: &VfioDeviceWrapper) -> Self {

        let vfio_dev = vfio_device.get_vfio_device();
        // 1. Parse BAR regions from VFIO device
        let mut bar_region_info = Vec::new();
        for i in 0..NUM_BAR_NUMS {
            let i = i as u32;
            let region_info = VfioRegion{
                index: i,
                start: 0,
                enable: false,
                flags: vfio_dev.get_region_flags(i),
                size: vfio_dev.get_region_size(i),
                offset: vfio_dev.get_region_offset(i),
                caps: vfio_dev.get_region_caps(i),
            };
            bar_region_info.push(region_info);
        }

        
        // 2. 查找并初始化 MSI-X 相关配置
        if let Some(msix_cap_offset) = Self::find_msix_cap_offset(vfio_device) {
            // MSI-X Control 寄存器的配置空间偏移范围
            let msix_clt_reg_offset_start = msix_cap_offset + 2;
            let msix_clt_reg_offset_end = msix_clt_reg_offset_start + 2;

            // 获取 MSI-X Table / PBA 的 BAR 位置信息
            let (msix_vector_location, msix_pba_location) =
                Self::msix_layout_for_bar(vfio_device, msix_cap_offset as u32);

            // 找到 MSI-X 能力，初始化完整字段
            Self {
                bar_region_reprogramming_in_progress: [None; NUM_BAR_NUMS],
                bar_region_info,
                msix_clt_reg_offset_start,
                msix_clt_reg_offset_end,
                msix_pba_location: Some(msix_pba_location),
                msix_vector_location: Some(msix_vector_location),
                region_need_discard:Vec::new(),
                region_need_emulation_in_mem:Vec::new(),
            }
        } else {
            // 未找到 MSI-X 能力，寄存器偏移置 0，位置为 None
            Self {
                bar_region_reprogramming_in_progress: [None; NUM_BAR_NUMS],
                bar_region_info,
                msix_clt_reg_offset_start: 0,
                msix_clt_reg_offset_end: 0,
                msix_pba_location: None,
                msix_vector_location: None,
                region_need_discard:Vec::new(),
                region_need_emulation_in_mem:Vec::new(),
            }
        }
    }

    /// 解析 MSI-X 配置空间，返回 Table 和 PBA 的 BAR 位置信息

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

    /// 根据访问地址查找对应的 VFIO 区域信息
    fn find_region(&self, addr: u64) -> Option<VfioRegion> {
        for region in self.bar_region_info.iter() {
            if addr >= region.start && addr < region.start + region.size && region.enable
            {
                return Some(region.clone());
            }
        }
        None
    }

    // 访问控制 ————————————————————————————————————————————————————————————————
    fn disable_bar_region(&mut self, index: usize) {
        if let Some(region) = self.bar_region_info.get_mut(index) {
            region.enable = false;
        }
    }
    fn enable_bar_region(&mut self, index: usize) {
        if let Some(region) = self.bar_region_info.get_mut(index) {
            region.enable = true;
        }
    }

    fn add_region_need_discard(&mut self, region: BarLocation) {
        self.region_need_discard.push(region);
        self.region_need_discard
            .sort_by_key(|entry| (entry.bar_index, entry.offset));
    }

    fn add_region_need_emulation_in_mem(&mut self, region: BarLocation) {
        self.region_need_emulation_in_mem.push(region);
        self.region_need_emulation_in_mem
            .sort_by_key(|entry| (entry.bar_index, entry.offset));
    }

    fn filter_bar_region_access<'a>(
        &self,
        bar_index: u8,
        access: Vec<AccessResolution<'a>>,
    ) -> Vec<AccessResolution<'a>> {
        let mut filtered = Vec::new();

        for resolution in access {
            let Some((access_start, access_len)) = Self::access_span(&resolution.access_type) else {
                filtered.push(resolution);
                continue;
            };

            if access_len == 0 {
                filtered.push(resolution);
                continue;
            }

            let access_end = access_start.saturating_add(access_len as u64);
            let points = self.split_points_for_bar(bar_index, access_start, access_end);

            match resolution.data {
                IovaAccessData::Read { data } => {
                    let mut remaining = data;
                    for window in points.windows(2) {
                        let segment_start = window[0];
                        let segment_end = window[1];
                        if segment_end <= segment_start {
                            continue;
                        }

                        let segment_len = (segment_end - segment_start) as usize;
                        let (segment_data, rest) = remaining.split_at_mut(segment_len);
                        remaining = rest;

                        let access_type = self
                            .region_kind_at(bar_index, segment_start)
                            .unwrap_or_else(|| Self::remap_segment_type(&resolution.access_type, segment_start, segment_len));

                        filtered.push(AccessResolution {
                            access_type,
                            data: IovaAccessData::Read { data: segment_data },
                        });
                    }
                }
                IovaAccessData::Write { data } => {
                    for window in points.windows(2) {
                        let segment_start = window[0];
                        let segment_end = window[1];
                        if segment_end <= segment_start {
                            continue;
                        }

                        let segment_len = (segment_end - segment_start) as usize;
                        let local_start = (segment_start - access_start) as usize;
                        let local_end = local_start + segment_len;
                        let segment_data = &data[local_start..local_end];

                        let access_type = self
                            .region_kind_at(bar_index, segment_start)
                            .unwrap_or_else(|| Self::remap_segment_type(&resolution.access_type, segment_start, segment_len));

                        filtered.push(AccessResolution {
                            access_type,
                            data: IovaAccessData::Write { data: segment_data },
                        });
                    }
                }
            }
        }

        filtered
    }

    fn get_region_can_passthrough_internal(&self) -> Vec<BarLocation> {
        let mut res = Vec::new();

        for region in &self.bar_region_info {
            if !region.enable {
                continue;
            }

            let mut ranges = vec![(region.start, region.start.saturating_add(region.size))];

            for blocked in self
                .region_need_discard
                .iter()
                .chain(self.region_need_emulation_in_mem.iter())
            {
                if blocked.bar_index != region.index as u8 {
                    continue;
                }

                let blocked_start = blocked.offset;
                let blocked_end = blocked.end();
                let mut next_ranges = Vec::new();

                for (range_start, range_end) in ranges {
                    if range_end <= blocked_start || blocked_end <= range_start {
                        next_ranges.push((range_start, range_end));
                        continue;
                    }

                    if range_start < blocked_start {
                        next_ranges.push((range_start, blocked_start));
                    }

                    if blocked_end < range_end {
                        next_ranges.push((blocked_end, range_end));
                    }
                }

                ranges = next_ranges;
            }

            for (range_start, range_end) in ranges {
                if range_start < range_end {
                    res.push(BarLocation {
                        bar_index: region.index as u8,
                        offset: range_start,
                        len: range_end - range_start,
                    });
                }
            }
        }

        res.sort_by_key(|entry| (entry.bar_index, entry.offset));
        res
    }

    // 辅助函数 ————————————————————————————————————————————————————————————————
    fn access_span(access_type: &IovaAccessType) -> Option<(u64, usize)> {
        match access_type {
            IovaAccessType::BarRegionCanForwardToVfio { offset, data_len, .. } => {
                Some((*offset, *data_len as usize))
            }
            IovaAccessType::MsixTable { offset, data_len, .. } => Some((*offset, *data_len as usize)),
            IovaAccessType::Pba { offset, data_len } => Some((*offset as u64, *data_len as usize)),
            _ => None,
        }
    }

    fn region_kind_at(&self, bar_index: u8, offset: u64) -> Option<IovaAccessType> {
        if self
            .region_need_discard
            .iter()
            .any(|region| region.bar_index == bar_index && region.contains(offset))
        {
            return Some(IovaAccessType::MmioNeededDiscard);
        }

        if self
            .region_need_emulation_in_mem
            .iter()
            .any(|region| region.bar_index == bar_index && region.contains(offset))
        {
            return Some(IovaAccessType::MmioNeededEmulationInMem);
        }

        None
    }

    fn split_points_for_bar(&self, bar_index: u8, start: u64, end: u64) -> Vec<u64> {
        let mut points = vec![start, end];

        for region in self
            .region_need_discard
            .iter()
            .chain(self.region_need_emulation_in_mem.iter())
        {
            if region.bar_index != bar_index {
                continue;
            }

            let region_start = region.offset;
            let region_end = region.end();
            if start < region_end && region_start < end {
                points.push(start.max(region_start));
                points.push(end.min(region_end));
            }
        }

        points.sort_unstable();
        points.dedup();
        points
    }

    fn remap_segment_type(
        access_type: &IovaAccessType,
        segment_start: u64,
        segment_len: usize,
    ) -> IovaAccessType {
        let data_len = segment_len as u8;
        match access_type {
            IovaAccessType::BarRegionCanForwardToVfio { index, .. } => IovaAccessType::BarRegionCanForwardToVfio {
                index: *index,
                offset: segment_start,
                data_len,
            },
            IovaAccessType::MsixTable { index, .. } => IovaAccessType::MsixTable {
                index: *index,
                offset: segment_start,
                data_len,
            },
            IovaAccessType::Pba { .. } => IovaAccessType::Pba {
                offset: segment_start as u16,
                data_len,
            },
            _ => access_type.clone(),
        }
    }


    pub(crate) fn get_bar_region_size(&self) -> [u32; NUM_BAR_NUMS] {
        let mut sizes = [0u32; NUM_BAR_NUMS];
        for region in &self.bar_region_info {
            sizes[region.index as usize] = region.size as u32;
        }
        sizes
    }
}


pub trait MemoryRecognizer: Send + Sync {
    fn parse_ecam_read<'a>(&self, reg_idx: usize, data: &'a mut [u8]) -> Vec<AccessResolution<'a>>;
    fn parse_ecam_write<'a>(&self, reg_idx: usize, offset: u64, data: &'a [u8]) -> Vec<AccessResolution<'a>>;

    fn parse_bar_read<'a>(&self, base: u64, offset: u64, data: &'a mut [u8]) -> Vec<AccessResolution<'a>>;
    fn parse_bar_write<'a>(&self, base: u64, offset: u64, data: &'a [u8]) -> Vec<AccessResolution<'a>>;


    fn find_region(&self, addr: u64) -> Option<VfioRegion>;


    fn on_bar_reprogrammed(&mut self, old_gpa: u64, new_gpa: u64);

    fn detect_bar_reprogramming(
        &mut self,
        reg_idx: usize,
        data: &[u8],
    ) -> Option<BarReprogrammingParams>;

    fn get_region_can_passthrough(&self) -> Vec<BarLocation>;

}

impl MemoryRecognizer for VfioPcieMemoryRecognizer{
    fn parse_ecam_read<'a>(&self, reg_idx: usize, data: &'a mut [u8]) -> Vec<AccessResolution<'a>>{
        let mut res = Vec::new();
        
        if (PCI_CONFIG_BAR0_INDEX..PCI_CONFIG_BAR0_INDEX + NUM_BAR_NUMS).contains(&reg_idx)
            || reg_idx == PCI_ROM_EXP_BAR_INDEX
        {
            let index = reg_idx - PCI_CONFIG_BAR0_INDEX;
            res.push(AccessResolution{
                access_type : IovaAccessType::BarReg { index:reg_idx as u8 },
                data: IovaAccessData::Read { data }
            })
        }
        else{
            res.push(AccessResolution{
                access_type : IovaAccessType::EcamCanForwardToVfio { reg_idx, offset: 0, data_len: 4 },
                data: IovaAccessData::Read { data }
            })
        }
        res
    }

    fn parse_ecam_write<'a>(&self, reg_idx: usize, offset: u64, data: &'a [u8]) -> Vec<AccessResolution<'a>>{
        let mut res = Vec::new();
            

        let access_bytes_index = (reg_idx * PCI_CONFIG_REGISTER_SIZE) + offset as usize;
        let is_msix_ctrl = data.len() == 2 &&
            access_bytes_index >= self.msix_clt_reg_offset_start &&
            access_bytes_index < self.msix_clt_reg_offset_end;
        let is_bar_reg = (PCI_CONFIG_BAR0_INDEX..PCI_CONFIG_BAR0_INDEX + NUM_BAR_NUMS).contains(&reg_idx)
            || reg_idx == PCI_ROM_EXP_BAR_INDEX;


        if is_bar_reg{
            let index = reg_idx - PCI_CONFIG_BAR0_INDEX;
            res.push(AccessResolution{
                access_type : IovaAccessType::BarReg { index:reg_idx as u8 },
                data: IovaAccessData::Write { data }
            })
        } else if is_msix_ctrl {
            res.push(AccessResolution{
                access_type: IovaAccessType::MsixCtrl,
                data: IovaAccessData::Write { data }
            })
        } else {
            res.push(AccessResolution{
                access_type: IovaAccessType::EcamCanForwardToVfio { reg_idx, offset, data_len: data.len() as u8 },
                data: IovaAccessData::Write { data }
            })
        }
        res
    }

    fn parse_bar_read<'a>(&self, base: u64, offset: u64, data: &'a mut [u8]) -> Vec<AccessResolution<'a>>{
        let mut res = Vec::new();

        let access_region = self.find_region(base + offset).unwrap();
        if let Some(msix_vector_location) = self.msix_vector_location{
            let is_access_msix_vector = msix_vector_location.bar_index == access_region.index as u8 &&
                offset >=  msix_vector_location.offset &&
                (offset + data.len() as u64)  < (msix_vector_location.offset + msix_vector_location.len);
            if is_access_msix_vector{
                let index = (msix_vector_location.offset - offset) / 16;

                res.push(AccessResolution{
                    access_type: IovaAccessType::MsixTable { index: index as u16, offset, data_len: data.len() as u8 },
                    data: IovaAccessData::Read { data }
                })
            }
        } else {
            res.push(AccessResolution{
                access_type: IovaAccessType::BarRegionCanForwardToVfio { index:access_region.index as u8, offset, data_len: data.len() as u8 },
                data: IovaAccessData::Read { data }
            })
        }
        
        self.filter_bar_region_access(access_region.index as u8, res)
    }
    
    fn parse_bar_write<'a>(&self, base: u64, offset: u64, data: &'a [u8]) -> Vec<AccessResolution<'a>>{
        let mut res = Vec::new();
        
        let access_region = self.find_region(base + offset).unwrap();
        if let Some(msix_vector_location) = self.msix_vector_location{
            let is_access_msix_vector = msix_vector_location.bar_index == access_region.index as u8 &&
                offset >=  msix_vector_location.offset &&
                (offset + data.len() as u64)  < (msix_vector_location.offset + msix_vector_location.len);
            if is_access_msix_vector{
                let index = (msix_vector_location.offset - offset) / 16;

                res.push(AccessResolution{
                    access_type: IovaAccessType::MsixTable { index: index as u16, offset, data_len: data.len() as u8 },
                    data: IovaAccessData::Write { data }
                })
            }
        } else {
            res.push(AccessResolution{
                access_type: IovaAccessType::BarRegionCanForwardToVfio { index:access_region.index as u8, offset, data_len: data.len() as u8 },
                data: IovaAccessData::Write { data }
            })
        }
        
        self.filter_bar_region_access(access_region.index as u8, res)
    }
  
    // BAR重编程 ))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))

    fn on_bar_reprogrammed(&mut self, old_gpa: u64, new_gpa: u64){
        let bar_index = self.bar_region_info.iter().position(|region| region.start == old_gpa);
        if let Some(bar_index) = bar_index {
            if let Some(region) = self.bar_region_info.iter_mut().find(|region| region.index == bar_index as u32) {
                region.start = new_gpa;
            }
        }
    }

    fn detect_bar_reprogramming(
        &mut self,
        reg_idx: usize,
        data: &[u8],
    ) -> Option<BarReprogrammingParams>{
        // TODO 函数中的分支需要优化

        if (PCI_CONFIG_BAR0_INDEX..PCI_CONFIG_BAR0_INDEX + NUM_BAR_NUMS).contains(&reg_idx)
            || reg_idx == PCI_ROM_EXP_BAR_INDEX && data.len() == 4
        {
            let index = reg_idx - PCI_CONFIG_BAR0_INDEX;
            
            let new_addr = u32::from_le_bytes(data.try_into().unwrap()) & BAR_MEM_ADDR_MASK;
            self.bar_region_info[index].start = new_addr as u64;

            let region = self.bar_region_info.get(index)?;
            let old_addr = region.start as u32;


            let is_64bit_bar = region.flags & VFIO_REGION_INFO_FLAG_64BIT != 0;
            if is_64bit_bar {

                let (pair_bar_rag_index1, pair_bar_rag_index2) = 
                    if index % 2 == 0 { (index, index + 1) } else { (index - 1, index) };

                self.bar_region_reprogramming_in_progress[index] = Some(region.start as u32);  // 记录变化

                // 判断变化是否完整（即两个相邻的 BAR 都被写入了新地址），如果完整则返回重编程参数，否则先返回旧地址并继续等待下一个写入完成重编程流程
                if let Some(old_pair_addr1) = self.bar_region_reprogramming_in_progress[pair_bar_rag_index1] && 
                    let Some(old_pair_addr2) = self.bar_region_reprogramming_in_progress[pair_bar_rag_index2] {

                    let old_bar_addr = (old_pair_addr2 as u64) << 32 | (old_pair_addr1 as u64);
                    let new_bar_addr = (self.bar_region_info[pair_bar_rag_index2].start as u64) << 32 | (self.bar_region_info[pair_bar_rag_index1].start as u64);
                    Some(BarReprogrammingParams {
                            old_base: old_bar_addr,
                            new_base: new_bar_addr,
                            len: region.size
                    })
                } else {
                    // 重编程尚未完成，说明用户正在以 64bit 模式重编程这个 BAR 对，应继续等待下一个写入完成重编程流程，先取消映射
                    let old_bar_addr = if index % 2 == 0 {
                        (self.bar_region_info[index + 1].start as u64) << 32 | (old_addr as u64)
                    } else {
                        (old_addr as u64) << 32 | (self.bar_region_info[index - 1].start as u64)
                    };
                    Some(BarReprogrammingParams {
                            old_base: old_bar_addr,
                            new_base: 0,
                            len: 0
                    })
                }
            } else {
                info!("Detected 32bit BAR{} reprogramming 当前不支持", index);
                None
            }
        } else {
            None
        }
    }


    fn find_region(&self, addr: u64) -> Option<VfioRegion>{
        self.find_region(addr)
    }

    fn get_region_can_passthrough(&self) -> Vec<BarLocation>{
        self.get_region_can_passthrough_internal()

    }

}


impl VfioPcieMemoryRecognizer {
    pub fn preallocated_bar_region_address(&mut self, mmio64_memory: &mut AddressAllocator) {
        for region in self.bar_region_info.iter_mut() {
            if !region.enable && region.size > 0 && region.start == 0 {
                // 预分配一个地址，暂不考虑分配失败的情况
                let addr = mmio64_memory.allocate(region.size, region.size, vm_allocator::AllocPolicy::FirstMatch).unwrap().start();
                region.start = addr;
                region.enable = true;
            }
        }
    }
}
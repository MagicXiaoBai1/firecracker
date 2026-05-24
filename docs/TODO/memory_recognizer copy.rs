use std::collections::HashMap;

use crate::devices::vfio::pcie::vfio::{Vfio, VfioDeviceWrapper};
use crate::utils::u64_to_usize;
use pci::PciCapabilityId;
use vfio_ioctls::{VfioDevice, VfioRegionInfoCap};

const PCI_CONFIG_BAR0_INDEX: usize = 4;
const NUM_BAR_NUMS: usize = 6;
const PCI_ROM_EXP_BAR_INDEX: usize = 12;
const PCI_CONFIG_REGISTER_SIZE: usize = 4;
const PCI_CONFIG_CAPABILITY_OFFSET: u32 = 0x34;
const PCI_CONFIG_CAPABILITY_PTR_MASK: u8 = 0xfc;
const MSIX_TABLE_ENTRY_SIZE: u64 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarLocation {
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

#[derive(Debug, Clone)]
pub struct RegionInfo {
    pub gpa: u64,
    pub len: u64,
    pub typ: IovaType,
}

#[derive(Debug, Clone)]
pub enum IovaType {
    BarMem { index: u8 },
    MsixTable { bar_index: u8 },
    Pba { bar_index: u8 },
    Ecam,
    MmioMem,
    Discard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IovaAccessType {
    BarReg { index: u8 },
    MsixCtrl,
    EcamCanForwardToVfio { reg_idx: usize, offset: u64, data_len: u8 },
    MsixTable { index: u16, offset: u64, data_len: u8 },
    Pba { offset: u16, data_len: u8 },
    BarRegionCanForwardToVfio { index: u8, offset: u64, data_len: u8 },
    MmioNeededEmulationInMem,
    MmioNeededDiscard,
    InvalidAccess,
}

#[derive(Debug, PartialEq, Eq)]
pub enum IovaAccessData<'a> {
    Read { data: &'a mut [u8] },
    Write { data: &'a [u8] },
}

#[derive(Debug)]
pub struct AccessResolution<'a> {
    pub access_type: IovaAccessType,
    pub data: IovaAccessData<'a>,
}

#[derive(Clone, Debug)]
pub struct VfioRegion {
    pub(crate) index: u32,
    pub(crate) start: u64,
    pub(crate) flags: u32,
    pub(crate) size: u64,
    pub(crate) offset: u64,
    pub(crate) caps: Vec<VfioRegionInfoCap>,
}

pub struct VfioPcieMemoryRecognizer {
    bar_region_info: Vec<VfioRegion>,

    msix_clt_reg_offset_start: usize,
    msix_clt_reg_offset_end: usize,
    
    msix_pba_location: Option<BarLocation>,
    msix_vector_location: Option<BarLocation>,
    
    region_need_discard: Vec<BarLocation>,
    region_need_emulation_in_mem: Vec<BarLocation>,
}

impl VfioPcieMemoryRecognizer {
    pub fn new(regions: Vec<RegionInfo>) -> Self {
        Self::from_region_infos(regions)
    }

    pub fn from_vfio_device(vfio_device: VfioDeviceWrapper) -> Self {
        let vfio = vfio_device.get_vfio_device();
        let mut regions = Vec::new();

        for index in 0..NUM_BAR_NUMS {
            let index_u32 = index as u32;
            regions.push(VfioRegion {
                index: index_u32,
                start: 0,
                flags: vfio.get_region_flags(index_u32),
                size: vfio.get_region_size(index_u32),
                offset: vfio.get_region_offset(index_u32),
                caps: vfio.get_region_caps(index_u32),
            });
        }

        let mut recognizer = Self {
            bar_region_info: regions,
            msix_clt_reg_offset_start: 0,
            msix_clt_reg_offset_end: 0,
            msix_pba_location: None,
            msix_vector_location: None,
            region_need_discard: Vec::new(),
            region_need_emulation_in_mem: Vec::new(),
        };

        if let Some(msix_cap_offset) = Self::find_msix_cap_offset(vfio_device.get_vfio_device()) {
            let (table_location, pba_location) = Self::msix_layout_for_bar(
                vfio_device.get_vfio_device().as_ref(),
                msix_cap_offset as u32,
            );
            recognizer.msix_clt_reg_offset_start = msix_cap_offset + 2;
            recognizer.msix_clt_reg_offset_end = recognizer.msix_clt_reg_offset_start + 2;
            recognizer.msix_vector_location = Some(table_location);
            recognizer.msix_pba_location = Some(pba_location);  
        }

        recognizer
    }

    fn from_region_infos(regions: Vec<RegionInfo>) -> Self {
        let mut bar_region_info = Vec::new();
        let mut bar_base_map = HashMap::<u8, u64>::new();

        for region in &regions {
            if let IovaType::BarMem { index } = region.typ {
                bar_base_map.insert(index, region.gpa);
            }
        }

        for region in &regions {
            let index = match region.typ {
                IovaType::BarMem { index }
                | IovaType::MsixTable { bar_index: index }
                | IovaType::Pba { bar_index: index } => index as u32,
                _ => 0,
            };

            bar_region_info.push(VfioRegion {
                index,
                start: region.gpa,
                flags: 0,
                size: region.len,
                offset: 0,
                caps: Vec::new(),
            });
        }

        let mut msix_vector_location = None;
        let mut msix_pba_location = None;

        for region in &regions {
            match region.typ {
                IovaType::MsixTable { bar_index } => {
                    if let Some(bar_base) = bar_base_map.get(&bar_index) {
                        msix_vector_location = Some(BarLocation {
                            bar_index,
                            offset: region.gpa.saturating_sub(*bar_base),
                            len: region.len,
                        });
                    }
                }
                IovaType::Pba { bar_index } => {
                    if let Some(bar_base) = bar_base_map.get(&bar_index) {
                        msix_pba_location = Some(BarLocation {
                            bar_index,
                            offset: region.gpa.saturating_sub(*bar_base),
                            len: region.len,
                        });
                    }
                }
                _ => {}
            }
        }

        Self {
            bar_region_info,
            msix_clt_reg_offset_start: 0,
            msix_clt_reg_offset_end: 0,
            msix_pba_location,
            msix_vector_location,
            region_need_discard: Vec::new(),
            region_need_emulation_in_mem: Vec::new(),
        }
    }

    fn find_msix_cap_offset(vfio: &dyn Vfio) -> Option<usize> {
        let mut cap_ptr = vfio.read_config_byte(PCI_CONFIG_CAPABILITY_OFFSET) & PCI_CONFIG_CAPABILITY_PTR_MASK;
        let mut guard = 0usize;

        while cap_ptr != 0 && guard < 48 {
            let cap_id = vfio.read_config_byte(cap_ptr.into());
            if cap_id == PciCapabilityId::MsiX as u8 {
                return Some(usize::from(cap_ptr));
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
        let msg_ctl = vfio.read_config_word(msix_cap_offset + 2);
        let table = vfio.read_config_dword(msix_cap_offset + 4);
        let pba = vfio.read_config_dword(msix_cap_offset + 8);

        let table_bir = (table & 0x7) as u8;
        let table_offset = u64::from(table & 0xffff_fff8);
        let table_entries = u64::from((msg_ctl & 0x07ff) + 1);
        let table_size = table_entries * MSIX_TABLE_ENTRY_SIZE;

        let pba_bir = (pba & 0x7) as u8;
        let pba_offset = u64::from(pba & 0xffff_fff8);
        let pba_size = table_entries.div_ceil(64) * 8;

        (
            BarLocation {
                bar_index: table_bir,
                offset: table_offset,
                len: table_size,
            },
            BarLocation {
                bar_index: pba_bir,
                offset: pba_offset,
                len: pba_size,
            },
        )
    }

    fn find_region_internal(&self, addr: u64) -> Option<VfioRegion> {
        self.bar_region_info
            .iter()
            .find(|region| addr >= region.start && addr < region.start.saturating_add(region.size))
            .cloned()
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

    fn is_access_bar_register(reg_idx: usize, offset: u64, data_len: usize) -> bool {
        if !(PCI_CONFIG_BAR0_INDEX..PCI_CONFIG_BAR0_INDEX + NUM_BAR_NUMS).contains(&reg_idx)
            && reg_idx != PCI_ROM_EXP_BAR_INDEX
        {
            return false;
        }

        u64_to_usize(offset) + data_len <= PCI_CONFIG_REGISTER_SIZE
    }

    fn is_access_msix_capabilities(
        &self,
        reg_idx: usize,
        offset: u64,
        data_len: usize,
        vfio: &dyn Vfio,
    ) -> bool {
        let Some(msix_cap_offset) = Self::find_msix_cap_offset(vfio) else {
            return false;
        };

        let access_start = reg_idx * PCI_CONFIG_REGISTER_SIZE + u64_to_usize(offset);
        let access_end = access_start + core::cmp::max(data_len, 1);
        let cap_start = msix_cap_offset;
        let cap_end = cap_start + 4;

        access_start < cap_end && cap_start < access_end
    }

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

}

pub trait MemoryRecognizer: Send + Sync {
    fn parse_ecam_read<'a>(&self, reg_idx: usize, data: &'a mut [u8]) -> Vec<AccessResolution<'a>>;
    fn parse_ecam_write<'a>(&self, reg_idx: usize, offset: u64, data: &'a [u8]) -> Vec<AccessResolution<'a>>;

    fn parse_bar_read<'a>(&self, base: u64, offset: u64, data: &'a mut [u8]) -> Vec<AccessResolution<'a>>;
    fn parse_bar_write<'a>(&self, base: u64, offset: u64, data: &'a [u8]) -> Vec<AccessResolution<'a>>;

    fn on_bar_reprogrammed(&mut self, bar_idx: u8, old_gpa: u64, new_gpa: u64);
    fn find_region(&self, addr: u64) -> Option<VfioRegion>;
    fn bar_index_by_base(&self, base: u64) -> Option<usize>;
    fn msix_layout_for_bar(&self, vfio: &dyn Vfio, base: u64) -> Option<(u64, u64, u64, u64)>;
    fn should_handle_config_write_locally(
        &self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
        vfio: &dyn Vfio,
    ) -> bool;
    fn should_handle_config_read_locally(&self, reg_idx: usize, vfio: &dyn Vfio) -> bool;
    fn is_access_msix_vector_register(&self, vfio: &dyn Vfio, base: u64, offset: u64) -> bool;
    fn get_region_can_passthrough(&self) -> Vec<BarLocation>;
}

impl MemoryRecognizer for VfioPcieMemoryRecognizer {
    fn parse_ecam_read<'a>(&self, reg_idx: usize, data: &'a mut [u8]) -> Vec<AccessResolution<'a>> {
        if (PCI_CONFIG_BAR0_INDEX..PCI_CONFIG_BAR0_INDEX + NUM_BAR_NUMS).contains(&reg_idx)
            || reg_idx == PCI_ROM_EXP_BAR_INDEX
        {
            vec![AccessResolution {
                access_type: IovaAccessType::BarReg { index: reg_idx as u8 },
                data: IovaAccessData::Read { data },
            }]
        } else {
            vec![AccessResolution {
                access_type: IovaAccessType::EcamCanForwardToVfio {
                    reg_idx,
                    offset: 0,
                    data_len: 4,
                },
                data: IovaAccessData::Read { data },
            }]
        }
    }

    fn parse_ecam_write<'a>(&self, reg_idx: usize, offset: u64, data: &'a [u8]) -> Vec<AccessResolution<'a>> {
        let access_bytes_index = (reg_idx * PCI_CONFIG_REGISTER_SIZE) + offset as usize;
        let is_msix_ctrl = data.len() == 2
            && access_bytes_index >= self.msix_clt_reg_offset_start
            && access_bytes_index < self.msix_clt_reg_offset_end;
        let is_bar_reg = (PCI_CONFIG_BAR0_INDEX..PCI_CONFIG_BAR0_INDEX + NUM_BAR_NUMS).contains(&reg_idx)
            || reg_idx == PCI_ROM_EXP_BAR_INDEX;

        if is_bar_reg {
            vec![AccessResolution {
                access_type: IovaAccessType::BarReg { index: reg_idx as u8 },
                data: IovaAccessData::Write { data },
            }]
        } else if is_msix_ctrl {
            vec![AccessResolution {
                access_type: IovaAccessType::MsixCtrl,
                data: IovaAccessData::Write { data },
            }]
        } else {
            vec![AccessResolution {
                access_type: IovaAccessType::EcamCanForwardToVfio {
                    reg_idx,
                    offset,
                    data_len: data.len() as u8,
                },
                data: IovaAccessData::Write { data },
            }]
        }
    }

    fn parse_bar_read<'a>(&self, base: u64, offset: u64, data: &'a mut [u8]) -> Vec<AccessResolution<'a>> {
        let Some(access_region) = self.find_region_internal(base + offset) else {
            return vec![AccessResolution {
                access_type: IovaAccessType::InvalidAccess,
                data: IovaAccessData::Read { data },
            }];
        };

        let access_end = offset.saturating_add(data.len() as u64);
        let mut res = Vec::new();

        if let Some(msix_vector_location) = self.msix_vector_location {
            let is_access_msix_vector = msix_vector_location.bar_index == access_region.index as u8
                && offset >= msix_vector_location.offset
                && access_end <= msix_vector_location.end();
            if is_access_msix_vector {
                let index = (offset - msix_vector_location.offset) / MSIX_TABLE_ENTRY_SIZE;
                res.push(AccessResolution {
                    access_type: IovaAccessType::MsixTable {
                        index: index as u16,
                        offset,
                        data_len: data.len() as u8,
                    },
                    data: IovaAccessData::Read { data },
                });
            }
        }

        if res.is_empty() {
            if let Some(msix_pba_location) = self.msix_pba_location {
                let is_access_pba = msix_pba_location.bar_index == access_region.index as u8
                    && offset >= msix_pba_location.offset
                    && access_end <= msix_pba_location.end();
                if is_access_pba {
                    res.push(AccessResolution {
                        access_type: IovaAccessType::Pba {
                            offset: offset as u16,
                            data_len: data.len() as u8,
                        },
                        data: IovaAccessData::Read { data },
                    });
                }
            }
        }

        if res.is_empty() {
            res.push(AccessResolution {
                access_type: IovaAccessType::BarRegionCanForwardToVfio {
                    index: access_region.index as u8,
                    offset,
                    data_len: data.len() as u8,
                },
                data: IovaAccessData::Read { data },
            });
        }

        self.filter_bar_region_access(access_region.index as u8, res)
    }

    fn parse_bar_write<'a>(&self, base: u64, offset: u64, data: &'a [u8]) -> Vec<AccessResolution<'a>> {
        let Some(access_region) = self.find_region_internal(base + offset) else {
            return vec![AccessResolution {
                access_type: IovaAccessType::InvalidAccess,
                data: IovaAccessData::Write { data },
            }];
        };

        let access_end = offset.saturating_add(data.len() as u64);
        let mut res = Vec::new();

        if let Some(msix_vector_location) = self.msix_vector_location {
            let is_access_msix_vector = msix_vector_location.bar_index == access_region.index as u8
                && offset >= msix_vector_location.offset
                && access_end <= msix_vector_location.end();
            if is_access_msix_vector {
                let index = (offset - msix_vector_location.offset) / MSIX_TABLE_ENTRY_SIZE;
                res.push(AccessResolution {
                    access_type: IovaAccessType::MsixTable {
                        index: index as u16,
                        offset,
                        data_len: data.len() as u8,
                    },
                    data: IovaAccessData::Write { data },
                });
            }
        }

        if res.is_empty() {
            if let Some(msix_pba_location) = self.msix_pba_location {
                let is_access_pba = msix_pba_location.bar_index == access_region.index as u8
                    && offset >= msix_pba_location.offset
                    && access_end <= msix_pba_location.end();
                if is_access_pba {
                    res.push(AccessResolution {
                        access_type: IovaAccessType::Pba {
                            offset: offset as u16,
                            data_len: data.len() as u8,
                        },
                        data: IovaAccessData::Write { data },
                    });
                }
            }
        }

        if res.is_empty() {
            res.push(AccessResolution {
                access_type: IovaAccessType::BarRegionCanForwardToVfio {
                    index: access_region.index as u8,
                    offset,
                    data_len: data.len() as u8,
                },
                data: IovaAccessData::Write { data },
            });
        }

        self.filter_bar_region_access(access_region.index as u8, res)
    }

    fn on_bar_reprogrammed(&mut self, bar_idx: u8, _old_gpa: u64, new_gpa: u64) {
        if let Some(region) = self.bar_region_info.iter_mut().find(|region| region.index == bar_idx as u32) {
            region.start = new_gpa;
        }
    }

    fn find_region(&self, addr: u64) -> Option<VfioRegion> {
        self.find_region_internal(addr)
    }

    fn bar_index_by_base(&self, base: u64) -> Option<usize> {
        self.bar_region_info
            .iter()
            .find(|region| region.start == base)
            .map(|region| region.index as usize)
    }

    fn msix_layout_for_bar(&self, vfio: &dyn Vfio, base: u64) -> Option<(u64, u64, u64, u64)> {
        let bar_index = self.bar_index_by_base(base)? as u32;
        let cap_off = Self::find_msix_cap_offset(vfio)? as u32;

        let msg_ctl = vfio.read_config_word(cap_off + 2);
        let table = vfio.read_config_dword(cap_off + 4);
        let pba = vfio.read_config_dword(cap_off + 8);

        let table_bir = table & 0x7;
        let pba_bir = pba & 0x7;
        if bar_index != table_bir && bar_index != pba_bir {
            return None;
        }

        let table_offset = u64::from(table & 0xffff_fff8);
        let pba_offset = u64::from(pba & 0xffff_fff8);
        let table_entries = u64::from((msg_ctl & 0x07ff) + 1);
        let table_size = table_entries * MSIX_TABLE_ENTRY_SIZE;
        let pba_size = table_entries.div_ceil(64) * 8;

        Some((table_offset, table_size, pba_offset, pba_size))
    }

    fn should_handle_config_write_locally(
        &self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
        vfio: &dyn Vfio,
    ) -> bool {
        Self::is_access_bar_register(reg_idx, offset, data.len())
            || self.is_access_msix_capabilities(reg_idx, offset, data.len(), vfio)
    }

    fn should_handle_config_read_locally(&self, reg_idx: usize, vfio: &dyn Vfio) -> bool {
        Self::is_access_bar_register(reg_idx, 0, PCI_CONFIG_REGISTER_SIZE)
            || self.is_access_msix_capabilities(reg_idx, 0, PCI_CONFIG_REGISTER_SIZE, vfio)
    }

    fn is_access_msix_vector_register(&self, vfio: &dyn Vfio, base: u64, offset: u64) -> bool {
        let Some((table_offset, table_size, pba_offset, pba_size)) = self.msix_layout_for_bar(vfio, base) else {
            return false;
        };

        let table_hit = table_offset
            .checked_add(table_size)
            .map(|end| (table_offset..end).contains(&offset))
            .unwrap_or(false);
        let pba_hit = pba_offset
            .checked_add(pba_size)
            .map(|end| (pba_offset..end).contains(&offset))
            .unwrap_or(false);

        table_hit || pba_hit
    }

    fn get_region_can_passthrough(&self) -> Vec<BarLocation> {
        self.get_region_can_passthrough_internal()
    }
}

fn ranges_overlap(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
    a_start < b_end && b_start < a_end
}

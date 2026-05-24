// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::sync::{Arc, RwLock};

use byteorder::{ByteOrder, LittleEndian};

use crate::Vm;
use crate::devices::vfio::pcie::vfio::{VfioBarOps, VfioMsixOps};
use crate::utils::u64_to_usize;
use super::mmio_utils::VfioMmioEngine;
use super::vfio::{Vfio};
use super::msix::VfioInterruptEngine;
use pci::PciCapabilityId;
use crate::vstate::interrupts::MsixVectorGroup;

const NUM_CONFIGURATION_REGISTERS: usize = 1024;
const NUM_BAR_REGS: usize = 6;
const BAR0_REG: usize = 4;
const BAR_MEM_ADDR_MASK: u32 = 0xffff_fff0;
const PCI_CONFIG_CAPABILITY_OFFSET: u32 = 0x34;
const PCI_CONFIG_CAPABILITY_PTR_MASK: u8 = 0xfc;
const MSIX_TABLE_ENTRY_SIZE: u64 = 16;

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct VfioBarRegionInfo {
    pub(crate) addr: u32,
    pub(crate) size_mask: u32,
    pub(crate) used: bool,
}

/// VFIO specific PCIe configuration cache.
///
/// This is intentionally independent from the generic `PciConfiguration` because VFIO
/// passthrough needs a different ownership model over config/BAR/MSI-X fields.
pub(crate) struct VfioPcieConfiguration {
    registers: [u32; NUM_CONFIGURATION_REGISTERS],
    writable_bits: [u32; NUM_CONFIGURATION_REGISTERS],
    bar_region_info: [VfioBarRegionInfo; NUM_BAR_REGS],
    msix_cap_reg_idx: Option<usize>,
    pub(crate) vfio_wrapper: Arc<dyn Vfio>,

    mmio_mgr: Arc<RwLock<Box<dyn VfioBarOps + Send + Sync>>>,
    msix_mgr: Arc<RwLock<Box<dyn VfioMsixOps + Send + Sync>>>,
}

impl fmt::Debug for VfioPcieConfiguration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VfioPcieConfiguration")
            .field("registers_len", &self.registers.len())
            .field("writable_bits_len", &self.writable_bits.len())
            .field("bar_region_info", &self.bar_region_info)
            .field("msix_cap_reg_idx", &self.msix_cap_reg_idx)
            .finish()
    }
}

impl VfioPcieConfiguration {
    pub(crate) fn new(
        id: u32,
        msix_vectors: MsixVectorGroup,
        vfio_wrapper: Arc<dyn Vfio>, 
        vm: Arc<Vm>, 
        ) -> Self {
        let mmio_mgr: Box<dyn VfioBarOps + Send + Sync> =
            Box::new(VfioMmioEngine::new(vm.clone(), vfio_wrapper.clone()));
        let msix_mgr: Box<dyn VfioMsixOps + Send + Sync> = Box::new(VfioInterruptEngine::new(
            vfio_wrapper.clone(),
            id,
            msix_vectors,
            vm.clone(),
        ));

        // compute_bar_region_info
        let mut bar_region_info = [VfioBarRegionInfo::default(); NUM_BAR_REGS];
        let mut bar_idx = 0usize;
        while bar_idx < NUM_BAR_REGS {
            let reg_idx = BAR0_REG + bar_idx;
            let addr = vfio_wrapper.read_config_dword((reg_idx * 4) as u32);
            bar_region_info[bar_idx].addr = addr;
            bar_region_info[bar_idx].used = addr != 0;
            bar_region_info[bar_idx].size_mask = BAR_MEM_ADDR_MASK;
            bar_idx += 1;
        }


        Self {
            registers: [0u32; NUM_CONFIGURATION_REGISTERS],
            writable_bits: [0u32; NUM_CONFIGURATION_REGISTERS],
            bar_region_info,
            msix_cap_reg_idx: None,
            vfio_wrapper,
            mmio_mgr: Arc::new(RwLock::new(mmio_mgr)),
            msix_mgr: Arc::new(RwLock::new(msix_mgr)),
        }
    }

    pub(crate) fn get_vfio_mmio_engine(&self,) -> Arc<RwLock<Box<dyn VfioBarOps + Send + Sync>>> {
        self.mmio_mgr.clone()
    }

    pub(crate) fn get_vfio_msix_engine(&self,) -> Arc<RwLock<Box<dyn VfioMsixOps + Send + Sync>>> {
        self.msix_mgr.clone()
    }

    pub(crate) fn read_reg(&self, reg_idx: usize) -> u32 {
        *self.registers.get(reg_idx).unwrap_or(&0xffff_ffff)
    }

    pub(crate) fn write_reg(&mut self, reg_idx: usize, value: u32) {
        let mut mask = self.writable_bits[reg_idx];

        if (BAR0_REG..BAR0_REG + NUM_BAR_REGS).contains(&reg_idx) {
            let bar_idx = reg_idx - BAR0_REG;
            // When guest writes all 1's to probe BAR size, return the size mask on next read.
            if value == 0xffff_ffff && self.bar_region_info[bar_idx].used {
                mask = self.bar_region_info[bar_idx].size_mask;
            }
            // Track BAR address updates (non-probe writes).
            if value != 0xffff_ffff {
                self.bar_region_info[bar_idx].addr = value;
            }
        }

        if let Some(r) = self.registers.get_mut(reg_idx) {
            *r = (*r & !self.writable_bits[reg_idx]) | (value & mask);
        }
    }

    pub(crate) fn write_config_register(&mut self, reg_idx: usize, offset: u64, data: &[u8]) {
        // 1. 判断是否在写bar寄存器：如果是就写PciConfiguration对象，然后返回
        // 2. 判断是否在使能misx or msi：如果是就写PciConfiguration对象，然后返回
        // 3. 读写vfio fd(vfio_wrapper)
        // PS这里不用处理 bar move
        if reg_idx >= NUM_CONFIGURATION_REGISTERS || u64_to_usize(offset) + data.len() > 4 {
            return;
        }

        // Check if accessing msg_ctl (MSIX capability offset + 2, word length)
        if let Some(msix_cap_offset) = self.find_msix_cap_offset(self.vfio_wrapper.clone().as_ref()) {
            let access_start = reg_idx * 4 + u64_to_usize(offset);
            let access_end = access_start + data.len();
            let msg_ctl_start = msix_cap_offset + 2;
            let msg_ctl_end = msg_ctl_start + 2;

            // If access overlaps with msg_ctl
            if access_start < msg_ctl_end && msg_ctl_start < access_end {
                // Extract msg_ctl value from data
                let msg_ctl_offset_in_data = msg_ctl_start.saturating_sub(access_start);
                let msg_ctl_remaining = msg_ctl_end - msg_ctl_start;
                
                if msg_ctl_offset_in_data < data.len() && msg_ctl_remaining > 0 {
                    // Read msg_ctl from the current register value first
                    let mut msg_ctl_bytes = [0u8; 2];
                    let shift = ((msg_ctl_start % 4) * 8) as u32;
                    let msg_ctl_from_reg = ((self.registers[msix_cap_offset / 4] >> shift) & 0xffff) as u16;
                    msg_ctl_bytes.copy_from_slice(&msg_ctl_from_reg.to_le_bytes());

                    // Overlay the written bytes
                    let bytes_to_copy = std::cmp::min(
                        data.len() - msg_ctl_offset_in_data,
                        msg_ctl_remaining
                    );
                    msg_ctl_bytes[0..bytes_to_copy]
                        .copy_from_slice(&data[msg_ctl_offset_in_data..msg_ctl_offset_in_data + bytes_to_copy]);
                    
                    let msg_ctl_value = u16::from_le_bytes(msg_ctl_bytes);
                    self.msix_mgr.write().unwrap().set_msg_ctl(msg_ctl_value);
                }
            }
        }

        let mut reg = self.registers[reg_idx];
        match data.len() {
            1 => {
                let shift = (u64_to_usize(offset) % 4) * 8;
                let mask = 0xffu32 << shift;
                reg = (reg & !mask) | ((u32::from(data[0])) << shift);
            }
            2 => {
                if u64_to_usize(offset) % 2 != 0 {
                    return;
                }
                let shift = (u64_to_usize(offset) % 4) * 8;
                let mask = 0xffffu32 << shift;
                reg = (reg & !mask) | (u32::from(LittleEndian::read_u16(data)) << shift);
            }
            4 => {
                reg = LittleEndian::read_u32(data);
                self.write_reg(reg_idx, reg);
                return;
            }
            _ => return,
        }

        self.registers[reg_idx] = reg;
    }

    // BAR Region 空间 ======================================================================================
    
    pub(crate) fn detect_bar_reprogramming(
        &mut self,
        reg_idx: usize,
        data: &[u8],
    ) -> Option<crate::pci::BarReprogrammingParams> {
        use crate::pci::BarReprogrammingParams;
        
        if data.len() != 4 || !(BAR0_REG..BAR0_REG + NUM_BAR_REGS).contains(&reg_idx) {
            return None;
        }

        let bar_idx = reg_idx - BAR0_REG;
        let info = self.bar_region_info[bar_idx];
        if !info.used {
            return None;
        }

        let value = u32::from_le_bytes(data.try_into().ok()?);
        if value == 0xffff_ffff {
            return None;
        }

        let reg_mask = {
            let writable_mask = self.writable_bits.get(reg_idx).copied().unwrap_or(0);
            if writable_mask == 0 {
                BAR_MEM_ADDR_MASK
            } else {
                writable_mask
            }
        };

        let old_base = u64::from(info.addr & reg_mask);
        let new_base = u64::from(value & reg_mask);
        if old_base == new_base {
            return None;
        }

        let len = u64::from((!info.size_mask).wrapping_add(1));
        Some(BarReprogrammingParams {
            old_base,
            new_base,
            len,
        })
    }

    pub(crate) fn is_access_bar_register(reg_idx: usize, offset: u64, data: &[u8]) -> bool {
        if !(4..10).contains(&reg_idx) && reg_idx != 12 {
            return false;
        }

        u64_to_usize(offset) + data.len() <= 4
    }

    pub(crate) fn bar_index_by_base(&self, base: u64) -> Option<usize> {
        let mut bar_idx = 0usize;
        while bar_idx < NUM_BAR_REGS {
            if !self.bar_region_info[bar_idx].used {
                bar_idx += 1;
                continue;
            }

            let reg_idx = BAR0_REG + bar_idx;
            let addr_mask = self.writable_bits[reg_idx];
            let mask = if addr_mask == 0 { BAR_MEM_ADDR_MASK } else { addr_mask };
            let bar_base = u64::from(self.bar_region_info[bar_idx].addr & mask);

            if bar_base == base {
                return Some(bar_idx);
            }

            bar_idx += 1;
        }

        None
    }


    // BAR Region 空间 ======================================================================================


    pub(crate) fn is_access_msix_capabilities(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
        vfio: &dyn Vfio,
    ) -> bool {
        let Some(msix_cap_offset) = self.find_msix_cap_offset(vfio) else {
            return false;
        };

        let access_start = reg_idx * 4 + u64_to_usize(offset);
        let access_end = access_start + core::cmp::max(data.len(), 1);

        let cap_start = msix_cap_offset;
        let cap_end = cap_start + 4;

        access_start < cap_end && cap_start < access_end
    }

    pub(crate) fn is_access_msix_vector_register(
        &mut self,
        vfio: &dyn Vfio,
        base: u64,
        offset: u64,
    ) -> bool {
        let Some((table_offset, table_size, pba_offset, pba_size)) =
            self.msix_layout_for_bar(vfio, base)
        else {
            return false;
        };

        let table_hit = table_offset
            .checked_add(table_size)
            .map(|end| (table_offset..end).contains(&offset))
            .unwrap_or(       false);
        let pba_hit = pba_offset
            .checked_add(pba_size)
            .map(|end| (pba_offset..end).contains(&offset))
            .unwrap_or(false);

        table_hit || pba_hit
    }

    pub(crate) fn msix_layout_for_bar(
        &mut self,
        vfio: &dyn Vfio,
        base: u64,
    ) -> Option<(u64, u64, u64, u64)> {
        let bar_index = self.bar_index_by_base(base)? as u32;
        let cap_off = self.find_msix_cap_offset(vfio)? as u32;

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

    fn find_msix_cap_offset(&mut self, vfio: &dyn Vfio) -> Option<usize> {
        if let Some(cached_offset) = self.msix_cap_reg_idx {
            return Some(cached_offset);
        }

        let mut cap_ptr = vfio.read_config_byte(PCI_CONFIG_CAPABILITY_OFFSET) & PCI_CONFIG_CAPABILITY_PTR_MASK;
        let mut guard = 0usize;

        while cap_ptr != 0 && guard < 48 {
            let cap_id = vfio.read_config_byte(cap_ptr.into());
            if cap_id == PciCapabilityId::MsiX as u8 {
                let cap_offset = usize::from(cap_ptr);
                self.msix_cap_reg_idx = Some(cap_offset);
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

    /// Check if accessing msg_ctl field in MSIX capability
    /// msg_ctl is located at capability_offset + 2 (word, 2 bytes)
    pub(crate) fn is_access_msg_ctl(&mut self, reg_idx: usize, offset: u64, data: &[u8]) -> bool {
        let Some(msix_cap_offset) = self.find_msix_cap_offset(self.vfio_wrapper.clone().as_ref()) else {
            return false;
        };

        let access_start = reg_idx * 4 + u64_to_usize(offset);
        let access_end = access_start + data.len();

        let msg_ctl_start = msix_cap_offset + 2;
        let msg_ctl_end = msg_ctl_start + 2;

        access_start < msg_ctl_end && msg_ctl_start < access_end
    }

    /// Check if the configuration register access should be handled locally
    /// Returns true if accessing BAR registers or MSIX capabilities (including msg_ctl)
    pub(crate) fn should_handle_config_write_locally(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) -> bool {
        // Check if accessing BAR register
        if Self::is_access_bar_register(reg_idx, offset, data) {
            return true;
        }
        // Check if accessing MSIX capabilities
        self.is_access_msix_capabilities(reg_idx, offset, data, self.vfio_wrapper.clone().as_ref())
    }

    /// Check if the configuration register read should be handled locally
    /// Returns true if accessing BAR registers or MSIX capabilities (including msg_ctl)
    pub(crate) fn should_handle_config_read_locally(&mut self, reg_idx: usize) -> bool {
        // Check if accessing BAR register
        if Self::is_access_bar_register(reg_idx, 0, &[]) {
            return true;
        }
        // Check if accessing MSIX capabilities
        self.is_access_msix_capabilities(reg_idx, 0, &[], self.vfio_wrapper.clone().as_ref())
    }

    


}

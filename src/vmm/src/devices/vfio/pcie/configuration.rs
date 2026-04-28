// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use byteorder::{ByteOrder, LittleEndian};

use crate::pci::BarReprogrammingParams;
use crate::utils::u64_to_usize;

const NUM_CONFIGURATION_REGISTERS: usize = 1024;
const NUM_BAR_REGS: usize = 6;
const BAR0_REG: usize = 4;
const BAR_MEM_ADDR_MASK: u32 = 0xffff_fff0;

#[derive(Debug, Default, Clone, Copy)]
struct VfioBar {
    addr: u32,
    size_mask: u32,
    used: bool,
}

/// VFIO specific PCIe configuration cache.
///
/// This is intentionally independent from the generic `PciConfiguration` because VFIO
/// passthrough needs a different ownership model over config/BAR/MSI-X fields.
#[derive(Debug)]
pub(crate) struct VfioPcieConfiguration {
    registers: [u32; NUM_CONFIGURATION_REGISTERS],
    writable_bits: [u32; NUM_CONFIGURATION_REGISTERS],
    bars: [VfioBar; NUM_BAR_REGS],
    msix_cap_reg_idx: Option<usize>,
}

impl VfioPcieConfiguration {
    pub(crate) fn new() -> Self {
        Self {
            registers: [0u32; NUM_CONFIGURATION_REGISTERS],
            writable_bits: [0u32; NUM_CONFIGURATION_REGISTERS],
            bars: [VfioBar::default(); NUM_BAR_REGS],
            msix_cap_reg_idx: None,
        }
    }

    pub(crate) fn read_reg(&self, reg_idx: usize) -> u32 {
        *self.registers.get(reg_idx).unwrap_or(&0xffff_ffff)
    }

    pub(crate) fn write_config_register(&mut self, reg_idx: usize, offset: u64, data: &[u8]) {
        if reg_idx >= NUM_CONFIGURATION_REGISTERS || u64_to_usize(offset) + data.len() > 4 {
            return;
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
            }
            _ => return,
        }

        self.registers[reg_idx] = reg;

        if (BAR0_REG..BAR0_REG + NUM_BAR_REGS).contains(&reg_idx) {
            let bar_idx = reg_idx - BAR0_REG;
            self.bars[bar_idx].addr = reg;
            self.bars[bar_idx].used = true;
        }
    }

    pub(crate) fn set_msix_cap_reg_idx(&mut self, reg_idx: usize) {
        self.msix_cap_reg_idx = Some(reg_idx);
    }

    pub(crate) fn msix_cap_reg_idx(&self) -> Option<usize> {
        self.msix_cap_reg_idx
    }

    pub(crate) fn set_bar_size_mask(&mut self, bar_idx: usize, size_mask: u32) {
        if bar_idx < NUM_BAR_REGS {
            self.bars[bar_idx].size_mask = size_mask;
            self.bars[bar_idx].used = true;
        }
    }

    pub(crate) fn set_bar_writable_mask(&mut self, bar_idx: usize, mask: u32) {
        let reg_idx = BAR0_REG + bar_idx;
        if reg_idx < NUM_CONFIGURATION_REGISTERS {
            self.writable_bits[reg_idx] = mask;
        }
    }

    pub(crate) fn find_bar_by_base(&self, base: u64) -> Option<usize> {
        let mut bar_idx = 0usize;
        while bar_idx < NUM_BAR_REGS {
            if !self.bars[bar_idx].used {
                bar_idx += 1;
                continue;
            }

            let reg_idx = BAR0_REG + bar_idx;
            let addr_mask = self.writable_bits[reg_idx];
            let mask = if addr_mask == 0 { BAR_MEM_ADDR_MASK } else { addr_mask };
            let bar_base = u64::from(self.bars[bar_idx].addr & mask);

            if bar_base == base {
                return Some(bar_idx);
            }

            bar_idx += 1;
        }

        None
    }

    pub(crate) fn detect_bar_reprogramming(
        &mut self,
        reg_idx: usize,
        data: &[u8],
    ) -> Option<BarReprogrammingParams> {
        if data.len() != 4 || !(BAR0_REG..BAR0_REG + NUM_BAR_REGS).contains(&reg_idx) {
            return None;
        }

        let bar_idx = reg_idx - BAR0_REG;
        if !self.bars[bar_idx].used {
            return None;
        }

        let value = LittleEndian::read_u32(data);
        if value == 0xffff_ffff {
            return None;
        }

        let reg_mask = if self.writable_bits[reg_idx] == 0 {
            BAR_MEM_ADDR_MASK
        } else {
            self.writable_bits[reg_idx]
        };
        let old_base = u64::from(self.bars[bar_idx].addr & reg_mask);
        let new_base = u64::from(value & reg_mask);

        if old_base == new_base {
            return None;
        }

        self.bars[bar_idx].addr = value;

        Some(BarReprogrammingParams {
            old_base,
            new_base,
            // Placeholder for MVP framework; callers should initialize size masks.
            len: u64::from((!self.bars[bar_idx].size_mask).wrapping_add(1)),
        })
    }
}

// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::sync::{Arc, RwLock};

use byteorder::{ByteOrder, LittleEndian};

use crate::Vm;
use crate::devices::vfio::pcie::vfio::{VfioMsixOps};
use crate::utils::u64_to_usize;
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

/// VFIO specific PCIe configuration
///
/// 负责模拟bar空间读。
/// 负责在内存中模拟PCIe配置空间，并处理配置空间的读写。
#[derive(Debug)]
pub(crate) struct VfioPcieConfiguration {
    registers: [u32; NUM_CONFIGURATION_REGISTERS],
    writable_bits: [u32; NUM_CONFIGURATION_REGISTERS],
    bar_region_size: [u32; NUM_BAR_REGS],

}

impl VfioPcieConfiguration {
    pub(crate) fn new(
        bar_region_size: [u32; NUM_BAR_REGS],
    ) -> Self {


        Self {
            registers: [0u32; NUM_CONFIGURATION_REGISTERS],
            writable_bits: [0u32; NUM_CONFIGURATION_REGISTERS],
            bar_region_size,
        }
    }

    pub fn write_config_register(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) {
        // Check if the access should be handled locally in configuration cache
      
    }

    pub fn read_config_register(&mut self, reg_idx: usize) -> u32 {
        // Check if the access should be handled locally in configuration cache
      

        0
    }

}

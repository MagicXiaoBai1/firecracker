use std::ops::DerefMut;
use std::sync::Arc;
use core::ffi::c_int;
use core::ptr::null_mut;
use std::io::{Error, ErrorKind};
use std::os::fd::{AsRawFd as _, BorrowedFd};
use vfio_bindings::bindings::vfio::*;

use crate::pci::{DeviceRelocation, DeviceRelocationError, PciDevice, BarReprogrammingParams};
use libc::size_t;
use crate::devices::vfio::pcie::vfio::{Vfio, VfioError};
use crate::vstate::bus::BusDeviceSync;
use crate::{EventManager, Vm};

use super::configuration::VfioBarRegionInfo;
use super::vfio::VfioBarOps;
use kvm_bindings::{
kvm_userspace_memory_region,
};
use vm_memory::GuestAddress;
const BAR0_REG: usize = 4;
const NUM_BAR_REGS: usize = 6;
const BAR_MEM_ADDR_MASK: u32 = 0xffff_fff0;

impl Vm {
    fn map_mmio_regions(
        &self,
        pci_dev: & dyn PciDevice,
        vfio_wrapper: Arc<dyn Vfio>,

        guest_base: u64,
        bar_region_id: usize,
        offset: u64,
        len: u64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(slot) = pci_dev.get_bar_region_slot(bar_region_id as u64, offset, len) {
            map_mmio_regions(&self, vfio_wrapper, slot, guest_base, bar_region_id, offset, len)
        } else {
            Err(Box::new(Error::new(ErrorKind::Other, "failed to get MMIO slot")))
        }
    }

    fn unmap_mmio_regions(
        &self,
        pci_dev: & dyn PciDevice,
        
        guest_base: u64,
        bar_region_id: usize,
        offset: u64,
        len: u64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(slot) = pci_dev.get_bar_region_slot(bar_region_id as u64, offset, len) {
            unmap_mmio_regions(&self, slot)
        } else {
            Err(Box::new(Error::new(ErrorKind::Other, "failed to get MMIO slot")))
        }
    }
}

/// Map MMIO regions into the guest, and avoid VM exits when the guest tries
/// to reach those regions.
///
/// # Arguments
fn map_mmio_regions(
    vm: &Vm,
    vfio_wrapper: Arc<dyn Vfio>,

    slot: u32,
    guest_base: u64,
    
    bar_region_id: usize,
    offset: u64,
    len: u64,
) -> Result<(), Box<dyn std::error::Error>>{
    // SAFETY: fd is guaranteed valid
    let fd = vfio_wrapper.as_raw_fd().ok_or_else(|| Error::new(ErrorKind::Other, "vfio wrapper does not support as_raw_fd"))?;
    let Ok(len) = libc::size_t::try_from(len) else {
        return Err(Box::new(Error::new(ErrorKind::InvalidInput, "length too large")));
    };
    let Ok(offset) = libc::off_t::try_from(offset) else {
        return Err(Box::new(Error::new(ErrorKind::InvalidInput, "offset too large")));
    };
    let region_flags = vfio_wrapper.get_vfio_device().get_region_flags(bar_region_id.try_into().unwrap());
    if region_flags & VFIO_REGION_INFO_FLAG_MMAP == 0 {
        return Err(Box::new(Error::new(ErrorKind::Other, "region does not support mmap")));
    }
    let mut prot = 0;
    if region_flags & VFIO_REGION_INFO_FLAG_READ != 0 {
        prot |= libc::PROT_READ;
    }
    if region_flags & VFIO_REGION_INFO_FLAG_WRITE != 0 {
        prot |= libc::PROT_WRITE;
    }
    let addr = unsafe { libc::mmap(null_mut(), len, prot, libc::MAP_SHARED, fd.as_raw_fd(), offset) };
    if addr == libc::MAP_FAILED {
        Error!("mmap failed for region {}: {}", bar_region_id, Error::last_os_error());    // TODO debug ok后删掉
        return Err(Box::new(Error::new(ErrorKind::Other, "mmap failed")));
    }
    let region = kvm_userspace_memory_region{
        slot,
        flags:0,
        guest_phys_addr: guest_base,
        memory_size: len as u64,
        userspace_addr: addr as u64,
    };
    vm.set_user_memory_region(region)?; 
    Ok(())
}


fn unmap_mmio_regions(
    vm: &Vm,
    slot: u32,
) -> Result<(), Box<dyn std::error::Error>>{
    let region = kvm_userspace_memory_region{
        slot,
        flags:0,
        guest_phys_addr: 0,
        memory_size: 0,
        userspace_addr: 0,
    };
    vm.set_user_memory_region(region)?;
    Ok(())
}

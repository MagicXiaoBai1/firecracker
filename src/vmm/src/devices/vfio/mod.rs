// Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! VFIO device passthrough support.
//!
//! This module implements VFIO (Virtual Function I/O) device passthrough,
//! allowing physical PCI devices to be passed directly to the guest VM.
//!
//! ## Overview
//!
//! VFIO provides a framework in the Linux kernel for safely exposing physical
//! devices to userspace processes (and hence to guest VMs). The typical workflow
//! is:
//!
//! 1. Unbind the device from its existing driver and bind it to `vfio-pci`.
//! 2. Open the VFIO container (`/dev/vfio/vfio`).
//! 3. Open the VFIO IOMMU group (`/dev/vfio/<group_id>`).
//! 4. Add the group to the container and configure the IOMMU.
//! 5. Open the device FD and read device/region/IRQ information.
//! 6. Map device BARs into the guest physical address space via KVM.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::os::raw::c_ulong;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::PathBuf;

use displaydoc::Display;
use thiserror::Error;
use vmm_sys_util::ioctl::{ioctl_with_mut_ref, ioctl_with_ref, ioctl_with_val};
use vmm_sys_util::{ioctl_io_nr, ioctl_ior_nr, ioctl_iow_nr, ioctl_iowr_nr};

use crate::vmm_config::vfio::VfioDeviceConfig;

// ---------------------------------------------------------------------------
// VFIO ioctl magic and base
// ---------------------------------------------------------------------------

const VFIO_TYPE: ::std::os::raw::c_uint = b';' as u32;
const VFIO_BASE: ::std::os::raw::c_uint = 100;

// ---------------------------------------------------------------------------
// VFIO container ioctls
// ---------------------------------------------------------------------------

ioctl_io_nr!(VFIO_GET_API_VERSION, VFIO_TYPE, VFIO_BASE);
ioctl_io_nr!(VFIO_CHECK_EXTENSION, VFIO_TYPE, VFIO_BASE + 1);
ioctl_io_nr!(VFIO_SET_IOMMU, VFIO_TYPE, VFIO_BASE + 2);

// ---------------------------------------------------------------------------
// VFIO group ioctls
// ---------------------------------------------------------------------------

ioctl_ior_nr!(VFIO_GROUP_GET_STATUS, VFIO_TYPE, VFIO_BASE + 3, VfioGroupStatus);
ioctl_iow_nr!(VFIO_GROUP_SET_CONTAINER, VFIO_TYPE, VFIO_BASE + 4, u32);
ioctl_io_nr!(VFIO_GROUP_UNSET_CONTAINER, VFIO_TYPE, VFIO_BASE + 5);
ioctl_iow_nr!(VFIO_GROUP_GET_DEVICE_FD, VFIO_TYPE, VFIO_BASE + 6, u64);

// ---------------------------------------------------------------------------
// VFIO device ioctls
// ---------------------------------------------------------------------------

ioctl_ior_nr!(VFIO_DEVICE_GET_INFO, VFIO_TYPE, VFIO_BASE + 7, VfioDeviceInfo);
ioctl_iowr_nr!(
    VFIO_DEVICE_GET_REGION_INFO,
    VFIO_TYPE,
    VFIO_BASE + 8,
    VfioRegionInfo
);
ioctl_iowr_nr!(VFIO_DEVICE_GET_IRQ_INFO, VFIO_TYPE, VFIO_BASE + 9, VfioIrqInfo);
ioctl_io_nr!(VFIO_DEVICE_RESET, VFIO_TYPE, VFIO_BASE + 11);

// ---------------------------------------------------------------------------
// VFIO IOMMU type constants
// ---------------------------------------------------------------------------

/// Type 1 IOMMU (Intel VT-d / AMD-Vi, one-to-one or scatter-gather maps).
pub const VFIO_TYPE1_IOMMU: u64 = 1;
/// Type 1 v2 IOMMU (supports pinning and DMA mapping of user pages).
pub const VFIO_TYPE1v2_IOMMU: u64 = 3;
/// No-IOMMU passthrough mode (dangerous – disables isolation).
pub const VFIO_NOIOMMU_IOMMU: u64 = 8;

// ---------------------------------------------------------------------------
// VFIO group status flags
// ---------------------------------------------------------------------------

/// The group is viable (all devices have been moved to VFIO).
const VFIO_GROUP_FLAGS_VIABLE: u32 = 1 << 0;
/// The group has been added to a container.
const VFIO_GROUP_FLAGS_CONTAINER_SET: u32 = 1 << 1;

// ---------------------------------------------------------------------------
// VFIO region flags
// ---------------------------------------------------------------------------

/// Region is readable.
pub const VFIO_REGION_INFO_FLAG_READ: u32 = 1 << 0;
/// Region is writable.
pub const VFIO_REGION_INFO_FLAG_WRITE: u32 = 1 << 1;
/// Region can be memory-mapped.
pub const VFIO_REGION_INFO_FLAG_MMAP: u32 = 1 << 2;

// ---------------------------------------------------------------------------
// VFIO device info flags
// ---------------------------------------------------------------------------

/// Device is a PCI device.
pub const VFIO_DEVICE_FLAGS_PCI: u32 = 1 << 1;

// ---------------------------------------------------------------------------
// PCI-specific region/IRQ indices
// ---------------------------------------------------------------------------

/// Number of standard PCI regions exposed by the vfio-pci driver.
pub const VFIO_PCI_NUM_REGIONS: u32 = 9;
/// Index of the PCI configuration space region.
pub const VFIO_PCI_CONFIG_REGION_INDEX: u32 = 7;

/// VFIO API version this implementation targets.
const VFIO_API_VERSION: i32 = 0;

/// Path to the VFIO container device.
const VFIO_CONTAINER_PATH: &str = "/dev/vfio/vfio";

// ---------------------------------------------------------------------------
// VFIO C-compatible structures (must match kernel ABI)
// ---------------------------------------------------------------------------

/// Kernel ABI structure returned by `VFIO_GROUP_GET_STATUS`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct VfioGroupStatus {
    /// Size of this structure (must be set before the ioctl).
    pub argsz: u32,
    /// Status flags (see `VFIO_GROUP_FLAGS_*` constants).
    pub flags: u32,
}

/// Kernel ABI structure returned by `VFIO_DEVICE_GET_INFO`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct VfioDeviceInfo {
    /// Size of this structure.
    pub argsz: u32,
    /// Device type flags (see `VFIO_DEVICE_FLAGS_*` constants).
    pub flags: u32,
    /// Number of regions the device exposes.
    pub num_regions: u32,
    /// Number of IRQ sets the device exposes.
    pub num_irqs: u32,
    /// Offset to capabilities chain (0 = no capabilities).
    pub cap_offset: u32,
    /// Reserved padding.
    pub pad: u32,
}

/// Kernel ABI structure returned/used by `VFIO_DEVICE_GET_REGION_INFO`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct VfioRegionInfo {
    /// Size of this structure.
    pub argsz: u32,
    /// Region flags (see `VFIO_REGION_INFO_FLAG_*` constants).
    pub flags: u32,
    /// Region index (set before ioctl).
    pub index: u32,
    /// Capabilities chain offset (0 = none).
    pub cap_offset: u32,
    /// Size of the region in bytes.
    pub size: u64,
    /// Offset from the start of the device FD to the region.
    pub offset: u64,
}

/// Kernel ABI structure returned by `VFIO_DEVICE_GET_IRQ_INFO`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct VfioIrqInfo {
    /// Size of this structure.
    pub argsz: u32,
    /// Flags.
    pub flags: u32,
    /// IRQ set index (set before ioctl).
    pub index: u32,
    /// Number of IRQs in this set.
    pub count: u32,
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur while working with a VFIO device.
#[derive(Debug, Error, Display)]
pub enum VfioError {
    /// Failed to open VFIO container '{0}': {1}
    OpenContainer(String, std::io::Error),
    /// Failed to open VFIO group '{0}': {1}
    OpenGroup(String, std::io::Error),
    /// VFIO API version mismatch: expected {0}, got {1}
    ApiVersionMismatch(i32, i32),
    /// VFIO group is not viable (not all devices bound to vfio-pci driver)
    GroupNotViable,
    /// Failed to set VFIO container for group: {0}
    SetContainer(std::io::Error),
    /// Failed to set IOMMU type on container: {0}
    SetIommu(std::io::Error),
    /// Invalid PCI BDF string (must be in 'domain:bus:device.function' format): {0}
    InvalidPciBdf(String),
    /// Failed to open device FD for '{0}': {1}
    OpenDevice(String, std::io::Error),
    /// Failed to get device info: {0}
    GetDeviceInfo(std::io::Error),
    /// Failed to get region info for index {0}: {1}
    GetRegionInfo(u32, std::io::Error),
    /// Device does not appear to be a PCI device (flags: {0:#x})
    NotPciDevice(u32),
    /// Failed to reset device: {0}
    Reset(std::io::Error),
}

// ---------------------------------------------------------------------------
// VfioDevice
// ---------------------------------------------------------------------------

/// A handle to a single VFIO passthrough PCI device.
///
/// This struct owns the file descriptors for the VFIO container, group, and
/// device, as well as a cached snapshot of the device and region metadata.
#[derive(Debug)]
pub struct VfioDevice {
    /// Configuration supplied by the user.
    pub config: VfioDeviceConfig,
    /// The VFIO container FD (`/dev/vfio/vfio`).
    container: File,
    /// The VFIO IOMMU group FD (`/dev/vfio/<group_id>`).
    _group: File,
    /// The device FD obtained from the group.
    device: File,
    /// Cached device information (capabilities, region/IRQ counts).
    pub device_info: VfioDeviceInfo,
    /// Cached information for each region exposed by the device.
    pub regions: Vec<VfioRegionInfo>,
}

impl VfioDevice {
    /// Open and initialise a VFIO device from a [`VfioDeviceConfig`].
    ///
    /// This function:
    /// 1. Opens `/dev/vfio/vfio` (the container).
    /// 2. Verifies the kernel VFIO API version.
    /// 3. Opens `/dev/vfio/<group_id>` (the IOMMU group).
    /// 4. Checks that the group is viable.
    /// 5. Adds the group to the container.
    /// 6. Sets the IOMMU type (`Type1v2` by default, or `NoIOMMU` if requested).
    /// 7. Opens the device FD using the PCI BDF string.
    /// 8. Fetches device and region information.
    pub fn new(config: VfioDeviceConfig) -> Result<Self, VfioError> {
        // ------------------------------------------------------------------
        // 1. Open the VFIO container
        // ------------------------------------------------------------------
        let container = OpenOptions::new()
            .read(true)
            .write(true)
            .open(VFIO_CONTAINER_PATH)
            .map_err(|e| VfioError::OpenContainer(VFIO_CONTAINER_PATH.to_owned(), e))?;

        // ------------------------------------------------------------------
        // 2. Check the kernel VFIO API version
        // ------------------------------------------------------------------
        // SAFETY: `VFIO_GET_API_VERSION` is a no-argument ioctl that merely
        // returns an integer; it cannot corrupt memory.
        let api_version = unsafe { ioctl_with_val(&container, VFIO_GET_API_VERSION(), 0) };
        if api_version != VFIO_API_VERSION {
            return Err(VfioError::ApiVersionMismatch(VFIO_API_VERSION, api_version));
        }

        // ------------------------------------------------------------------
        // 3. Open the IOMMU group
        // ------------------------------------------------------------------
        let group_path = PathBuf::from(format!("/dev/vfio/{}", config.iommu_group));
        let group = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&group_path)
            .map_err(|e| VfioError::OpenGroup(group_path.display().to_string(), e))?;

        // ------------------------------------------------------------------
        // 4. Verify group viability
        // ------------------------------------------------------------------
        let mut group_status = VfioGroupStatus {
            argsz: std::mem::size_of::<VfioGroupStatus>() as u32,
            flags: 0,
        };
        // SAFETY: `VFIO_GROUP_GET_STATUS` writes into `group_status` which is
        // a valid, correctly-sized structure.
        let ret = unsafe { ioctl_with_mut_ref(&group, VFIO_GROUP_GET_STATUS(), &mut group_status) };
        if ret < 0 || (group_status.flags & VFIO_GROUP_FLAGS_VIABLE) == 0 {
            return Err(VfioError::GroupNotViable);
        }

        // ------------------------------------------------------------------
        // 5. Add group to container
        // ------------------------------------------------------------------
        let container_fd = container.as_raw_fd() as u32;
        // SAFETY: `VFIO_GROUP_SET_CONTAINER` expects a pointer to a `u32`
        // holding the container FD; `container_fd` is valid.
        let ret =
            unsafe { ioctl_with_ref(&group, VFIO_GROUP_SET_CONTAINER(), &container_fd) };
        if ret < 0 {
            return Err(VfioError::SetContainer(std::io::Error::last_os_error()));
        }

        // ------------------------------------------------------------------
        // 6. Set IOMMU type on the container
        // ------------------------------------------------------------------
        let iommu_type = if config.no_iommu {
            VFIO_NOIOMMU_IOMMU
        } else {
            VFIO_TYPE1v2_IOMMU
        };
        // SAFETY: `VFIO_SET_IOMMU` is a simple value ioctl; it cannot corrupt
        // memory.
        let ret = unsafe { ioctl_with_val(&container, VFIO_SET_IOMMU(), iommu_type) };
        if ret < 0 {
            return Err(VfioError::SetIommu(std::io::Error::last_os_error()));
        }

        // ------------------------------------------------------------------
        // 7. Open the device FD
        // ------------------------------------------------------------------
        // The BDF must be a valid NUL-terminated C string (e.g. "0000:01:00.0").
        let bdf_cstr = CString::new(config.host_pci_bdf.as_str())
            .map_err(|_| VfioError::InvalidPciBdf(config.host_pci_bdf.clone()))?;
        // SAFETY: `VFIO_GROUP_GET_DEVICE_FD` accepts a pointer to a C string
        // and returns a new file descriptor on success (>= 0) or -1 on error.
        let device_raw_fd = unsafe {
            ioctl_with_val(
                &group,
                VFIO_GROUP_GET_DEVICE_FD(),
                bdf_cstr.as_ptr() as c_ulong,
            )
        };
        if device_raw_fd < 0 {
            return Err(VfioError::OpenDevice(
                config.host_pci_bdf.clone(),
                std::io::Error::last_os_error(),
            ));
        }
        // SAFETY: `device_raw_fd` is a valid, newly-created file descriptor
        // returned by the kernel; we now take ownership of it.
        let device = unsafe { File::from_raw_fd(device_raw_fd as RawFd) };

        // ------------------------------------------------------------------
        // 8. Fetch device information
        // ------------------------------------------------------------------
        let mut device_info = VfioDeviceInfo {
            argsz: std::mem::size_of::<VfioDeviceInfo>() as u32,
            ..Default::default()
        };
        // SAFETY: `VFIO_DEVICE_GET_INFO` fills in `device_info` which is a
        // valid, correctly-sized structure.
        let ret = unsafe { ioctl_with_mut_ref(&device, VFIO_DEVICE_GET_INFO(), &mut device_info) };
        if ret < 0 {
            return Err(VfioError::GetDeviceInfo(std::io::Error::last_os_error()));
        }
        if (device_info.flags & VFIO_DEVICE_FLAGS_PCI) == 0 {
            return Err(VfioError::NotPciDevice(device_info.flags));
        }

        // ------------------------------------------------------------------
        // 8b. Fetch region information for every region
        // ------------------------------------------------------------------
        let mut regions = Vec::with_capacity(device_info.num_regions as usize);
        for index in 0..device_info.num_regions {
            let mut region_info = VfioRegionInfo {
                argsz: std::mem::size_of::<VfioRegionInfo>() as u32,
                index,
                ..Default::default()
            };
            // SAFETY: `VFIO_DEVICE_GET_REGION_INFO` fills in `region_info`
            // with information about region `index`.  The struct is valid and
            // correctly sized.
            let ret = unsafe {
                ioctl_with_mut_ref(&device, VFIO_DEVICE_GET_REGION_INFO(), &mut region_info)
            };
            if ret < 0 {
                return Err(VfioError::GetRegionInfo(
                    index,
                    std::io::Error::last_os_error(),
                ));
            }
            regions.push(region_info);
        }

        Ok(VfioDevice {
            config,
            container,
            _group: group,
            device,
            device_info,
            regions,
        })
    }

    /// Reset the device via `VFIO_DEVICE_RESET`.
    ///
    /// Not all devices support reset; check `device_info.flags` for the
    /// `VFIO_DEVICE_FLAGS_RESET` flag before calling.
    pub fn reset(&self) -> Result<(), VfioError> {
        // SAFETY: `VFIO_DEVICE_RESET` is a no-argument ioctl with no memory
        // side-effects beyond the kernel-internal device reset path.
        let ret = unsafe { ioctl_with_val(&self.device, VFIO_DEVICE_RESET(), 0) };
        if ret < 0 {
            return Err(VfioError::Reset(std::io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Return the raw file descriptor for the device.
    ///
    /// Used, for example, to `mmap` a region or set up eventfd-based
    /// interrupt forwarding.
    pub fn device_fd(&self) -> RawFd {
        self.device.as_raw_fd()
    }

    /// Return the raw file descriptor for the container.
    ///
    /// Used when setting up DMA mapping with KVM.
    pub fn container_fd(&self) -> RawFd {
        self.container.as_raw_fd()
    }

    /// Return the region info for the PCI configuration space region, if present.
    pub fn config_region(&self) -> Option<&VfioRegionInfo> {
        self.regions
            .get(VFIO_PCI_CONFIG_REGION_INDEX as usize)
            .filter(|r| r.size > 0)
    }

    /// Return the region info for BAR `bar_index` (0–5).
    pub fn bar_region(&self, bar_index: u32) -> Option<&VfioRegionInfo> {
        self.regions
            .get(bar_index as usize)
            .filter(|r| r.size > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Unit tests for VFIO structures (no device required).

    #[test]
    fn test_vfio_group_status_size() {
        assert_eq!(std::mem::size_of::<VfioGroupStatus>(), 8);
    }

    #[test]
    fn test_vfio_device_info_size() {
        assert_eq!(std::mem::size_of::<VfioDeviceInfo>(), 24);
    }

    #[test]
    fn test_vfio_region_info_size() {
        assert_eq!(std::mem::size_of::<VfioRegionInfo>(), 32);
    }

    #[test]
    fn test_vfio_irq_info_size() {
        assert_eq!(std::mem::size_of::<VfioIrqInfo>(), 16);
    }

    #[test]
    fn test_region_flags_constants() {
        // Verify the flag constants don't overlap.
        assert_ne!(VFIO_REGION_INFO_FLAG_READ, VFIO_REGION_INFO_FLAG_WRITE);
        assert_ne!(VFIO_REGION_INFO_FLAG_READ, VFIO_REGION_INFO_FLAG_MMAP);
        assert_ne!(VFIO_REGION_INFO_FLAG_WRITE, VFIO_REGION_INFO_FLAG_MMAP);
    }

    #[test]
    fn test_pci_config_region_index() {
        assert_eq!(VFIO_PCI_CONFIG_REGION_INDEX, 7);
    }
}

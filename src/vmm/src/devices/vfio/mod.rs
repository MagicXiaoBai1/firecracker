// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! VFIO (Virtual Function I/O) device passthrough support.
//!
//! This module provides the ability to pass physical host devices directly
//! through to the guest VM using the Linux VFIO framework. VFIO leverages
//! the host IOMMU to provide safe, isolated device access without requiring
//! privileged userspace drivers.
//!
//! # Usage
//!
//! A device is identified by its sysfs path (e.g.
//! `/sys/bus/pci/devices/0000:00:01.0`). The host kernel must have bound the
//! device to the `vfio-pci` driver before Firecracker is started.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use vm_memory::GuestMemory;
use vfio_ioctls::{VfioContainer, VfioDevice, VfioDeviceFd, VfioError};

use crate::vmm_config::vfio::VfioDeviceConfig;

/// Errors that may occur while working with a VFIO passthrough device.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum VfioPassthroughError {
    /// Failed to create VFIO container: {0}
    CreateContainer(VfioError),
    /// Failed to open VFIO device: {0}
    OpenDevice(VfioError),
    /// Failed to map guest memory into IOMMU: {0}
    MapGuestMemory(VfioError),
    /// Failed to unmap guest memory from IOMMU: {0}
    UnmapGuestMemory(VfioError),
}

/// A VFIO passthrough device.
///
/// Wraps a [`vfio_ioctls::VfioDevice`] together with the shared
/// [`VfioContainer`] that manages the IOMMU group membership.
pub struct VfioPassthroughDevice {
    /// The sysfs path used to create this device.
    pub sysfs_path: PathBuf,
    /// The underlying VFIO device handle.
    pub device: VfioDevice,
    /// The VFIO container shared across all VFIO groups opened from this VMM
    /// instance.
    pub container: Arc<VfioContainer>,
}

// Manual Debug implementation because VfioDevice/VfioContainer do not derive Debug.
impl std::fmt::Debug for VfioPassthroughDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VfioPassthroughDevice")
            .field("sysfs_path", &self.sysfs_path)
            .finish_non_exhaustive()
    }
}

impl VfioPassthroughDevice {
    /// Create a new [`VfioPassthroughDevice`] from a configuration object.
    ///
    /// # Arguments
    ///
    /// * `config` – The device configuration, containing the sysfs path.
    /// * `device_fd` – A KVM VFIO device file descriptor created with
    ///   `KVM_CREATE_DEVICE`.
    pub fn new(
        config: &VfioDeviceConfig,
        device_fd: VfioDeviceFd,
    ) -> Result<Self, VfioPassthroughError> {
        let sysfs_path = PathBuf::from(&config.host_dev_path);
        Self::from_path(&sysfs_path, device_fd)
    }

    /// Create a new [`VfioPassthroughDevice`] from a raw sysfs path.
    ///
    /// # Arguments
    ///
    /// * `sysfs_path` – The sysfs path to the host PCI device.
    /// * `device_fd`  – A KVM VFIO device file descriptor.
    pub fn from_path(
        sysfs_path: &Path,
        device_fd: VfioDeviceFd,
    ) -> Result<Self, VfioPassthroughError> {
        let container = Arc::new(
            VfioContainer::new(Some(Arc::new(device_fd)))
                .map_err(VfioPassthroughError::CreateContainer)?,
        );
        let device = VfioDevice::new(sysfs_path, container.clone())
            .map_err(VfioPassthroughError::OpenDevice)?;

        Ok(VfioPassthroughDevice {
            sysfs_path: sysfs_path.to_path_buf(),
            device,
            container,
        })
    }

    /// Map all regions of the guest's physical address space that are visible
    /// to DMA into the IOMMU page tables owned by this container.
    ///
    /// This must be called after the guest memory has been allocated and
    /// before the device is allowed to perform DMA.
    ///
    /// # Safety
    ///
    /// The caller must ensure the guest memory regions remain valid and pinned
    /// for as long as the IOMMU mapping is in place (i.e. until
    /// [`Self::unmap_guest_memory`] is called or this device is dropped).
    pub unsafe fn map_guest_memory<M: GuestMemory>(
        &self,
        guest_mem: &M,
    ) -> Result<(), VfioPassthroughError> {
        // SAFETY: forwarded to the container which enforces the same contract.
        unsafe {
            self.container
                .vfio_map_guest_memory(guest_mem)
                .map_err(VfioPassthroughError::MapGuestMemory)
        }
    }

    /// Unmap all guest memory regions that were previously registered with the
    /// IOMMU.
    pub fn unmap_guest_memory<M: GuestMemory>(
        &self,
        guest_mem: &M,
    ) -> Result<(), VfioPassthroughError> {
        self.container
            .vfio_unmap_guest_memory(guest_mem)
            .map_err(VfioPassthroughError::UnmapGuestMemory)
    }

    /// Return the sysfs path of the underlying device.
    pub fn sysfs_path(&self) -> &Path {
        &self.sysfs_path
    }
}

/// Container for all VFIO passthrough devices attached to a microVM.
#[derive(Debug, Default)]
pub struct VfioDeviceManager {
    /// The list of attached VFIO devices.
    pub devices: Vec<Arc<Mutex<VfioPassthroughDevice>>>,
}

impl VfioDeviceManager {
    /// Create a new, empty device manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a new VFIO device, returning a counted reference to it.
    pub fn attach(
        &mut self,
        device: VfioPassthroughDevice,
    ) -> Arc<Mutex<VfioPassthroughDevice>> {
        let device = Arc::new(Mutex::new(device));
        self.devices.push(device.clone());
        device
    }

    /// Return the number of attached VFIO devices.
    pub fn count(&self) -> usize {
        self.devices.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmm_config::vfio::VfioDeviceConfig;

    #[test]
    fn test_vfio_device_manager_empty() {
        let mgr = VfioDeviceManager::new();
        assert_eq!(mgr.count(), 0);
        assert!(mgr.devices.is_empty());
    }

    #[test]
    fn test_vfio_device_config_fields() {
        let cfg = VfioDeviceConfig {
            host_dev_path: "/sys/bus/pci/devices/0000:00:01.0".into(),
        };
        assert!(!cfg.host_dev_path.is_empty());
    }
}

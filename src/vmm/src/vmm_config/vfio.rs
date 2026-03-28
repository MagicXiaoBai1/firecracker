// Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Configuration and builder types for VFIO device passthrough.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::devices::vfio::{VfioDevice, VfioError};

/// Configuration for a single VFIO passthrough device.
///
/// Use this structure to describe a physical PCI device that should be
/// passed through to the guest VM.  The device must have been bound to the
/// `vfio-pci` kernel driver before starting Firecracker.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VfioDeviceConfig {
    /// Unique identifier for this device within the VM.
    ///
    /// Must be non-empty and contain only alphanumeric characters and
    /// hyphens (e.g. `"my-nic"` or `"gpu-0"`).
    pub id: String,

    /// IOMMU group number for the host device.
    ///
    /// This corresponds to the directory `/dev/vfio/<iommu_group>` that the
    /// `vfio-pci` driver creates when the device is bound.  Find the group
    /// number with:
    /// ```shell
    /// readlink /sys/bus/pci/devices/<bdf>/iommu_group
    /// ```
    pub iommu_group: u32,

    /// PCI Bus:Device.Function address of the host device (e.g. `"0000:01:00.0"`).
    ///
    /// This is used to open the device FD within the VFIO group.
    pub host_pci_bdf: String,

    /// If `true`, use `VFIO_NOIOMMU_IOMMU` instead of the Type1v2 IOMMU.
    ///
    /// **Warning**: This disables DMA isolation between the device and host
    /// memory.  Only use this in controlled environments where no IOMMU
    /// hardware is available.
    #[serde(default)]
    pub no_iommu: bool,
}

/// Errors that can occur while configuring or building a VFIO device.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum VfioDeviceConfigError {
    /// VFIO device ID must not be empty
    EmptyId,
    /// A VFIO device with id '{0}' already exists
    DeviceAlreadyExists(String),
    /// Failed to create VFIO device '{0}': {1}
    CreateDevice(String, #[source] VfioError),
}

/// Builder that accumulates VFIO device configurations and constructs
/// [`VfioDevice`] instances.
#[derive(Debug, Default)]
pub struct VfioDeviceBuilder {
    /// The list of initialised VFIO devices.
    pub devices: Vec<Arc<Mutex<VfioDevice>>>,
}

impl VfioDeviceBuilder {
    /// Create a new, empty builder.
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
        }
    }

    /// Build and add a VFIO device from the given configuration.
    ///
    /// Returns an error if a device with the same `id` already exists or if
    /// the underlying VFIO initialisation fails.
    pub fn build(&mut self, config: VfioDeviceConfig) -> Result<(), VfioDeviceConfigError> {
        if config.id.is_empty() {
            return Err(VfioDeviceConfigError::EmptyId);
        }

        // Reject duplicate IDs.
        if self
            .devices
            .iter()
            .any(|d| d.lock().unwrap().config.id == config.id)
        {
            return Err(VfioDeviceConfigError::DeviceAlreadyExists(config.id));
        }

        let id = config.id.clone();
        let device = VfioDevice::new(config)
            .map_err(|e| VfioDeviceConfigError::CreateDevice(id, e))?;
        self.devices.push(Arc::new(Mutex::new(device)));
        Ok(())
    }

    /// Add a pre-built [`VfioDevice`] to the builder.
    ///
    /// This is intended for use during snapshot restoration where the device
    /// object has already been constructed.
    pub fn add_device(&mut self, device: Arc<Mutex<VfioDevice>>) {
        self.devices.push(device);
    }

    /// Return the configuration objects for all devices in this builder.
    pub fn configs(&self) -> Vec<VfioDeviceConfig> {
        self.devices
            .iter()
            .map(|d| d.lock().unwrap().config.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vfio_device_config_default() {
        let cfg = VfioDeviceConfig::default();
        assert!(cfg.id.is_empty());
        assert_eq!(cfg.iommu_group, 0);
        assert!(cfg.host_pci_bdf.is_empty());
        assert!(!cfg.no_iommu);
    }

    #[test]
    fn test_vfio_device_config_serialize_deserialize() {
        let cfg = VfioDeviceConfig {
            id: "test-dev".to_string(),
            iommu_group: 42,
            host_pci_bdf: "0000:01:00.0".to_string(),
            no_iommu: false,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let deserialized: VfioDeviceConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, deserialized);
    }

    #[test]
    fn test_vfio_device_config_deny_unknown_fields() {
        let bad_json = r#"{"id": "x", "iommu_group": 1, "host_pci_bdf": "0000:00:01.0", "unknown": true}"#;
        assert!(serde_json::from_str::<VfioDeviceConfig>(bad_json).is_err());
    }

    #[test]
    fn test_vfio_builder_new_is_empty() {
        let builder = VfioDeviceBuilder::new();
        assert!(builder.devices.is_empty());
        assert!(builder.configs().is_empty());
    }
}

// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Configuration types for VFIO passthrough devices.

use serde::{Deserialize, Serialize};

/// Configuration for a single VFIO passthrough device.
///
/// A VFIO device is identified by its sysfs path on the host. The host
/// kernel must have bound the device to the `vfio-pci` driver and the
/// corresponding IOMMU group must be accessible to the Firecracker process
/// (i.e. the calling user must own `/dev/vfio/<group_id>`).
///
/// # Example JSON
///
/// ```json
/// {
///   "host_dev_path": "/sys/bus/pci/devices/0000:00:01.0"
/// }
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VfioDeviceConfig {
    /// Sysfs path to the PCI device on the host.
    ///
    /// Example: `/sys/bus/pci/devices/0000:00:01.0`
    pub host_dev_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialise_round_trip() {
        let cfg = VfioDeviceConfig {
            host_dev_path: "/sys/bus/pci/devices/0000:00:01.0".into(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let decoded: VfioDeviceConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, decoded);
    }

    #[test]
    fn test_default_is_empty() {
        let cfg = VfioDeviceConfig::default();
        assert!(cfg.host_dev_path.is_empty());
    }

    #[test]
    fn test_deny_unknown_fields() {
        let json = r#"{"host_dev_path": "/dev/null", "extra_field": true}"#;
        assert!(serde_json::from_str::<VfioDeviceConfig>(json).is_err());
    }
}

// Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use vmm::rpc_interface::VmmAction;
use vmm::vmm_config::vfio::VfioDeviceConfig;

use super::super::parsed_request::{ParsedRequest, RequestError, checked_id};
use super::{Body, StatusCode};

/// Parse a `PUT /vfio-devices/{id}` request.
///
/// The request body must be a JSON object matching [`VfioDeviceConfig`].
/// The `id` in the path must match the `id` field in the body.
pub(crate) fn parse_put_vfio(
    body: &Body,
    id_from_path: Option<&str>,
) -> Result<ParsedRequest, RequestError> {
    let id = if let Some(id) = id_from_path {
        checked_id(id)?
    } else {
        return Err(RequestError::EmptyID);
    };

    let device_cfg = serde_json::from_slice::<VfioDeviceConfig>(body.raw())?;

    if id != device_cfg.id {
        return Err(RequestError::Generic(
            StatusCode::BadRequest,
            "The id from the path does not match the id from the body!".to_string(),
        ));
    }

    Ok(ParsedRequest::new_sync(VmmAction::InsertVfioDevice(
        device_cfg,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_server::parsed_request::tests::vmm_action_from_request;

    #[test]
    fn test_parse_put_vfio_missing_id() {
        parse_put_vfio(&Body::new("{}"), None).unwrap_err();
    }

    #[test]
    fn test_parse_put_vfio_invalid_payload() {
        parse_put_vfio(&Body::new("invalid json"), Some("dev0")).unwrap_err();
    }

    #[test]
    fn test_parse_put_vfio_unknown_fields() {
        let body = r#"{
            "id": "dev0",
            "iommu_group": 1,
            "host_pci_bdf": "0000:01:00.0",
            "unknown_field": true
        }"#;
        parse_put_vfio(&Body::new(body), Some("dev0")).unwrap_err();
    }

    #[test]
    fn test_parse_put_vfio_id_mismatch() {
        let body = r#"{
            "id": "dev1",
            "iommu_group": 1,
            "host_pci_bdf": "0000:01:00.0"
        }"#;
        parse_put_vfio(&Body::new(body), Some("dev0")).unwrap_err();
    }

    #[test]
    fn test_parse_put_vfio_valid() {
        let body = r#"{
            "id": "gpu0",
            "iommu_group": 42,
            "host_pci_bdf": "0000:01:00.0"
        }"#;
        let req = vmm_action_from_request(parse_put_vfio(&Body::new(body), Some("gpu0")).unwrap());
        let expected = VmmAction::InsertVfioDevice(VfioDeviceConfig {
            id: "gpu0".to_string(),
            iommu_group: 42,
            host_pci_bdf: "0000:01:00.0".to_string(),
            no_iommu: false,
        });
        assert_eq!(req, expected);
    }

    #[test]
    fn test_parse_put_vfio_no_iommu() {
        let body = r#"{
            "id": "fpga0",
            "iommu_group": 7,
            "host_pci_bdf": "0000:00:03.0",
            "no_iommu": true
        }"#;
        let req = vmm_action_from_request(parse_put_vfio(&Body::new(body), Some("fpga0")).unwrap());
        let expected = VmmAction::InsertVfioDevice(VfioDeviceConfig {
            id: "fpga0".to_string(),
            iommu_group: 7,
            host_pci_bdf: "0000:00:03.0".to_string(),
            no_iommu: true,
        });
        assert_eq!(req, expected);
    }
}

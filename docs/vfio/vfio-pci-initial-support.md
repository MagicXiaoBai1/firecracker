# Firecracker Design: Initial VFIO PCI Support (Bring-up First)

## Background

This document updates the previous design to match a bring-up-first target:
1. Introduce rust-vmm VFIO dependencies.
2. Use one fixed host PCIe device path that is pre-unbound from native driver and pre-bound to VFIO.
3. Auto-attach this device at startup.
4. Do not add new Firecracker API endpoints for v1.

This is intentionally a temporary engineering path to unblock end-to-end validation quickly.

## Scope of this revision

### In scope

1. Compile and run with rust-vmm VFIO crates.
2. Create and register one VfioPciDevice.
3. BAR trap plus local PCI BAR cache semantics.
4. MMIO forwarding to VFIO regions.
5. Boot-time DMA mapping and teardown unmapping.

### Out of scope

1. New REST API objects or Swagger changes.
2. Dynamic device hotplug API.
3. Snapshot and restore with VFIO devices.
4. Full interrupt model feature-complete support.

## Dependency plan

Add rust-vmm VFIO dependencies to [src/vmm/Cargo.toml](src/vmm/Cargo.toml):
1. vfio-bindings = { version = "0.6.2", default-features = false }
2. vfio-ioctls = { version = "0.6.0", default-features = false }

Optional follow-up dependency only if needed later:
1. iommufd-ioctls = "0.1.0"

Version choice is aligned with Cloud Hypervisor workspace dependency set you provided.

## Fixed-device startup model

### Assumptions

1. Host side has already unbound target PCIe device from native driver.
2. Host side has already bound that device to vfio-pci.
3. Firecracker process receives access to required VFIO node(s) through jailer setup.
4. A single fixed target is enough for bring-up.

### Configuration source

No API is added. The fixed device selector is provided by one of these implementation choices:
1. Hard-coded constant in code for initial private validation.
2. Internal config file consumed at process startup.
3. Existing startup argument surface if available without API schema changes.

Recommended v1 bring-up: start from a hard-coded constant, then move to internal config file.

### Target identifier

Any one stable host-side identifier can be used:
1. BDF string like 0000:65:00.0.
2. Resolved sysfs path under /sys/bus/pci/devices/.
3. Direct vfio group path when pre-resolved by deployment scripts.

## Architecture changes

### 1) New VFIO module

Add:
1. src/vmm/src/devices/pci/vfio/mod.rs
2. src/vmm/src/devices/pci/vfio/device.rs
3. src/vmm/src/devices/pci/vfio/host_path.rs

Main type:
1. VfioPciDevice

Responsibilities:
1. Open and own VFIO container or group or device handles.
2. Implement PciDevice for config-space behavior.
3. Implement BAR MMIO forwarding.
4. Keep BAR metadata and region mapping tables.
5. Manage DMA map and unmap lifecycle.

### 2) BAR handling model (same core behavior as reference commit)

1. Guest writes to BAR registers are trapped.
2. BAR values are maintained in Firecracker local PCI config cache.
3. Guest BAR reads return cached values.
4. Non-BAR config reads and writes are proxied to VFIO PCI config region.
5. Firecracker never directly trusts guest BAR writes as host BAR programming.

### 3) BAR discovery and guest address allocation

At attach time:
1. Probe VFIO PCI regions and identify BAR type and size.
2. Allocate guest address ranges from existing Firecracker allocators.
3. Register each mapped BAR range on MMIO bus.
4. Record mapping tuple: guest_base, length, vfio_region_index, region_type.
5. Populate local PCI config BAR cache with guest-visible BAR addresses.

### 4) Startup attach flow without API

During VM build path after PCI segment is enabled:
1. Read fixed device selector from internal source.
2. Resolve selector to VFIO endpoint.
3. Create VfioPciDevice.
4. Allocate PCI slot and attach to PciBus.
5. Register BAR ranges into MMIO bus.
6. Map boot-time guest memory into IOMMU.

Any attach failure should be fail-fast in bring-up mode.

## Security and operational constraints

1. Jailer passes only required VFIO resources.
2. Seccomp profile is updated with minimum required VFIO ioctls.
3. Startup fails if fixed device is missing, not VFIO-bound, or inaccessible.
4. v1 is single-device only unless explicitly extended.

## Code change map

### Cargo and build

1. Update [src/vmm/Cargo.toml](src/vmm/Cargo.toml) with VFIO dependencies.

### Device implementation

1. Add new VFIO PCI device module under devices or pci tree.
2. Implement PciDevice trait methods for config and BAR operations.
3. Implement bus read and write forwarding for MMIO regions.

### Device manager path

1. Add internal startup hook to auto-attach fixed VFIO device.
2. Reuse existing PCI slot allocation and bus registration.
3. Keep existing virtio-pci path unchanged.

### Lifecycle

1. Add boot-time DMA map for all guest memory regions.
2. Add teardown unmap best-effort cleanup.
3. Explicitly reject snapshot and restore when VFIO bring-up device is attached.

### No API changes in v1

1. No Swagger updates.
2. No rpc_interface endpoint additions.
3. No device-api matrix changes for now.

## Testing plan (bring-up focused)

### Unit tests

1. BAR size probing and classification.
2. BAR trap plus cache readback behavior.
3. Non-BAR config passthrough behavior.
4. Guest address to VFIO region resolution.

### Integration checks

1. VM boots with fixed VFIO device attached.
2. Guest enumerates PCI function correctly.
3. Guest driver can perform basic BAR MMIO access.
4. Teardown does not leave stale DMA mappings.

### Negative checks

1. Device selector invalid.
2. Device not bound to vfio-pci.
3. Permission denied on VFIO endpoint.
4. Snapshot request while VFIO device is attached.

## Rollout stages

### Stage A: compile and attach

1. Add dependencies.
2. Wire fixed-path attach.
3. Confirm guest enumeration.

Exit:
1. Stable boot and PCI enumeration.

### Stage B: BAR and data path validation

1. Validate BAR probe and allocation.
2. Validate MMIO forwarding and basic driver access.

Exit:
1. Functional device access for target workload.

### Stage C: hardening

1. Tighten seccomp and jailer permissions.
2. Add targeted observability and failure diagnostics.
3. Decide whether to productize into API later.

## Risks and mitigations

1. Risk: fixed device path is environment-specific.
Mitigation: keep selector isolated in one config point and validate early.

2. Risk: DMA map mismatch across future memory lifecycle features.
Mitigation: keep v1 limited to boot-time memory and block unsupported paths.

3. Risk: regressions to existing virtio-pci flows.
Mitigation: isolated module and no changes to virtio transport behavior.

4. Risk: security drift while exposing host VFIO resources.
Mitigation: strict jailer pass-through and minimal ioctl allowlist.

## Later productization path

When bring-up is confirmed, next step can add a formal API model:
1. user-specified VFIO device list
2. lifecycle controls
3. richer interrupt support
4. snapshot compatibility strategy

## Summary

This revised design intentionally prioritizes fast bring-up: rust-vmm VFIO dependency integration, fixed-device startup attach, and BAR-safe virtualization behavior, while postponing API and broader product features.

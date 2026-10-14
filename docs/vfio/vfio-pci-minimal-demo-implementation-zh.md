<!--
Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
SPDX-License-Identifier: Apache-2.0
-->

## 最小 VFIO PCI demo（仅 config 通过 VFIO fd）实现说明

本文档说明本次在 Firecracker `vmm` 中实现的“最小可用 VFIO PCI 直通 demo”做了什么、能验证什么、不能验证什么，以及从启动脚本到 guest `lspci` 的执行链路。

### 1. 目标与边界

本次 demo 的目标是：

- **最少改动**引入 VFIO；
- **只打通 PCI 配置空间（config space）读写通路**：让 guest 在枚举 PCI 设备时，读取到直通设备的真实 `vendor/device/class` 等信息；
- guest 中执行 `lspci` 能看到该直通设备；
- 保持 BAR 写入 **trap**（避免 guest 把 BAR 地址写到真实设备上）。

明确不做的内容：

- **不做 BAR/MMIO region 映射**（不把 VFIO BAR region 挂到 guest MMIO bus）；
- **不做 DMA / IRQ / MSI/MSI-X 绑定**；
- 因此 **不保证设备驱动可正常工作**（仅保证枚举/识别阶段的 config 访问成立）。

### 2. 为什么“不做 BAR/MMIO”也能 `lspci` 看到设备？

`lspci` 的核心依赖是 **PCI 配置空间**：

- `lspci`/guest 内核在枚举时会读取 config space 的寄存器（例如 0x00 vendor/device id、0x08 class code、capability 链等）。
- 这些读取不需要 BAR/MMIO 映射；BAR/MMIO 是“设备功能寄存器/队列”的访问路径，通常在驱动加载并尝试操作设备时才需要。

因此，本 demo 只要让 Firecracker 的 `read_config_register()` 通过 VFIO fd 读取 `VFIO_PCI_CONFIG_REGION_INDEX`，guest 就能枚举并在 `lspci` 中显示该设备。

### 3. 代码改动概览

#### 3.1 新增/修改的模块

- **新增**：`src/vmm/src/devices/pci/vfio/`
  - `mod.rs`
  - `device.rs`
- **修改**：`src/vmm/src/devices/pci/mod.rs`
  - 导出 `vfio` 子模块。
- **修改**：`src/vmm/src/device_manager/pci_mngr.rs`
  - 增加 `PciDevices::attach_fixed_vfio_device()`；
  - 增加 `FC_VFIO_PCI_BDF` 环境变量读取；
  - 在启用 PCI 后尝试挂载固定 VFIO 设备。
- **修改**：`src/vmm/src/device_manager/mod.rs`
  - `enable_pci()` 中调用 `attach_fixed_vfio_device()`。
- **修改**：`start_fc_socket.sh`
  - 增加 `FC_LOG_LEVEL/FC_SHOW_LEVEL/FC_LOG_MODULE`，便于打开 `fixed-vfio:` debug 日志。

#### 3.2 核心实现：`FixedVfioPciDevice`

文件：`src/vmm/src/devices/pci/vfio/device.rs`

关键点：

- **VFIO container + device 初始化（真实 VFIO fd）**
  - `VfioContainer::new(None)` 打开 `/dev/vfio/vfio`
  - `VfioDevice::new("/sys/bus/pci/devices/<BDF>", container)`：
    - 内部会读 `<BDF>/iommu_group` 取 group id
    - 打开 `/dev/vfio/<group>`
    - 绑定 group 到 container、设置 IOMMU
    - `VFIO_GROUP_GET_DEVICE_FD` 拿到 device fd
    - 获取 regions/irqs 信息，准备后续 `region_read/region_write`

- **PCI config 读写通过 VFIO config region**
  - 读取：`VfioDevice.region_read(VFIO_PCI_CONFIG_REGION_INDEX, ...)`
  - 写入：`VfioDevice.region_write(VFIO_PCI_CONFIG_REGION_INDEX, ...)`

- **BAR 写 trap**
  - 当 `reg_idx` 在 BAR0~BAR5 的寄存器范围（`4..=9`）时，不写入 VFIO 设备，避免 guest 地址污染真实设备。

- **调试日志**
  - 初始化阶段会打印 container fd / device fd / sysfs path
  - guest 枚举时会打印 `fixed-vfio: config read ...`

### 4. 执行链路（从脚本到 VFIO fd）

下面描述你使用两个脚本启动时的实际路径。

#### 4.1 启动 Firecracker 进程：`firecracker/start_fc_socket.sh`

关键行为：

- 通过 `--enable-pci` 开启 PCI 支持；
- 通过 `sudo FC_VFIO_PCI_BDF=0000:7a:02.0 ...` 将目标 host BDF 传给 Firecracker 进程环境；
- 可选通过：
  - `FC_LOG_LEVEL=Debug`
  - `FC_LOG_MODULE=vmm::devices::pci::vfio::device`
  打开并过滤 `fixed-vfio:` 日志。

#### 4.2 通过 API 启动 VM：`start_fc_vm.sh`

在你调用 `/actions` 的 `InstanceStart` 后：

1. Firecracker 的设备管理层启用 PCI：
   - `DeviceManager::enable_pci()`
   - `PciDevices::attach_pci_segment()`
2. 同步尝试挂载固定 VFIO 设备：
   - `PciDevices::attach_fixed_vfio_device()`
   - 从环境变量读取 `FC_VFIO_PCI_BDF`
   - 创建 `FixedVfioPciDevice::new(host_bdf)`
   - 将该设备加到 PCI bus（分配一个 guest SBDF）

#### 4.3 guest 枚举与 `lspci`

guest 内核与 `lspci` 会读取 PCI config space。

执行到 Firecracker 侧：

- guest 发起 config 读
- `FixedVfioPciDevice::read_config_register(reg_idx)`
- `VfioDevice.region_read(VFIO_PCI_CONFIG_REGION_INDEX, ...)`
- 内核 VFIO 驱动从真实设备读取并返回
- Firecracker 打印 `fixed-vfio: config read ...`
- `lspci` 输出显示真实设备信息（vendor/device/class 等）

### 5. 验收方式（你当前已验证通过）

满足以下几点即可认为 demo 达成：

- Firecracker 日志中出现：
  - `fixed-vfio: creating VFIO container`
  - `fixed-vfio: device created ... device_fd=...`
  - `fixed-vfio: config read host_bdf=... reg_idx=...`
- guest 内 `lspci` 能看到直通设备条目（非 virtio 的 `1af4:*`）。

### 6. 已知限制与下一步

限制：

- 未做 BAR/MMIO 映射：guest 驱动即使识别到设备，也无法通过 MMIO/PIO 正常驱动设备功能。
- 未做 DMA/IRQ：多数设备无法完成真实 I/O。

下一步（扩展方向）：

- 对齐 `vfio-pci-demo-design-zh.md` 的 13.x 计划：在保持 BAR trap 的前提下，逐步增加 BAR region 转发、guest 地址映射（allocator + mmio_bus）、以及必要的 IRQ/MSI 支持。


# VFIO NPU 直通开发梳理报告（Firecracker）

## 1. 目标与范围

本报告聚焦当前实现入口 `attach_vfio_pcie_device` 的调用链、涉及对象与接口、现状缺口和后续 TODO。

按当前开发目标分阶段：
1. 先完成并验证 MMIO 与 DMA。
2. 再实现 MSI-X 中断。
3. 暂不开展“FC vs CH 的 KVM/VFIO 系统调用差异比对”测试方案。

## 2. VM Start 入口调用链

### 2.1 启动主链

1. `build_microvm_for_boot` 在启动时调用 `attach_vfio_pcie_device`。
   - 位置：src/vmm/src/builder.rs:142, src/vmm/src/builder.rs:227, src/vmm/src/builder.rs:1431
2. `attach_vfio_pcie_device` 当前完成了：
   - 创建 KVM VFIO device fd（`KVM_DEV_TYPE_VFIO`）
   - 创建 `VfioContainer`
   - 创建 `VfioDevice`
   - 遍历 guest memory 做 `vfio_dma_map`
   - 调用 `device_manager.pci_devices.attach_pci_vfio_device(...)`
   - 位置：src/vmm/src/builder.rs:1458, src/vmm/src/builder.rs:1469, src/vmm/src/builder.rs:1473, src/vmm/src/builder.rs:1496, src/vmm/src/builder.rs:1510
3. `attach_pci_vfio_device`：
   - 向 PCI segment 申请 BDF
   - 创建 `VfioPciDevice`
   - 注册到 `pci_bus.add_device(...)`
   - 保存到 `PciDevices.vfio_devices`
   - 位置：src/vmm/src/device_manager/pci_mngr.rs:172

### 2.2 PCI 使能与 ECAM 接入链

1. `device_manager.enable_pci` -> `pci_devices.attach_pci_segment`。
   - 位置：src/vmm/src/device_manager/mod.rs:273, src/vmm/src/device_manager/pci_mngr.rs:84
2. `PciSegment::build` 创建并注册 `PciConfigMmio` 到 `vm.common.mmio_bus` 的 ECAM 区间。
   - 位置：src/vmm/src/devices/pci/pci_segment.rs:72, src/vmm/src/devices/pci/pci_segment.rs:79
3. Guest 访问 PCI 配置空间（ECAM）会经由 `PciConfigMmio` 分发到目标 `PciDevice`。
   - 位置：src/vmm/src/pci/bus.rs:303, src/vmm/src/pci/bus.rs:336, src/vmm/src/pci/bus.rs:379

## 3. 运行时 I/O 调用链（KVM Exit 到设备）

1. vCPU 发生 `VcpuExit::MmioRead/MmioWrite`。
2. `handle_kvm_exit` 调用 `mmio_bus.read/write`。
   - 位置：src/vmm/src/vstate/vcpu.rs:416
3. 若地址命中 ECAM 区域：进入 `PciConfigMmio`，再到 `PciBus.devices[device_id]`，最终调用设备的：
   - `read_config_register`
   - `write_config_register`
   - 位置：src/vmm/src/pci/bus.rs:336
4. 若地址命中 BAR 区域：应命中注册在 `mmio_bus` 上的 `BusDevice`，调用 `read`/`write`，再到设备 `read_bar`/`write_bar`。
   - 目前 VFIO 设备 BAR 尚未注册到 `mmio_bus`，这是 MMIO 数据面尚未打通的关键缺口。

## 4. 关键对象与接口梳理

### 4.1 DeviceManager / PciDevices

1. `DeviceManager` 持有 `pci_devices: PciDevices`。
   - 位置：src/vmm/src/device_manager/mod.rs:94
2. `PciDevices` 维护：
   - `pci_segment`
   - `virtio_devices`
   - `vfio_devices`
   - 位置：src/vmm/src/device_manager/pci_mngr.rs:53
3. `attach_pci_vfio_device` 当前只把 VFIO 设备挂入 PCI bus（配置空间路径），未挂 BAR 到 mmio bus。

### 4.2 PCI 抽象层

1. `PciDevice` trait 统一了配置空间、BAR 和 BAR 重映射接口：
   - `write_config_register`
   - `read_config_register`
   - `detect_bar_reprogramming`
   - `read_bar` / `write_bar`
   - `move_bar`
   - 位置：src/vmm/src/pci/mod.rs:30
2. `PciBus` 内部持有 `HashMap<u32, Arc<Mutex<dyn PciDevice>>>`。
   - 位置：src/vmm/src/pci/bus.rs:78
3. `Vm` 的 `DeviceRelocation` 当前返回 `NotSupported`。
   - 位置：src/vmm/src/vstate/vm.rs:539

### 4.3 VFIO 设备层

1. `VfioPciDevice`：当前是 PCI 设备对接对象。
   - 位置：src/vmm/src/devices/vfio/pcie/vfio_device.rs:59
2. `VfioPciDevice::new` 创建 `VfioDeviceWrapper`，并构建 `VfioCommon`。
   - 位置：src/vmm/src/devices/vfio/pcie/vfio_device.rs:77
3. `impl PciDevice for VfioPciDevice` 中关键方法基本为 TODO：
   - 配置空间读写未转发
   - BAR 读写未转发
   - move_bar 未实现
   - 位置：src/vmm/src/devices/vfio/pcie/vfio_device.rs:101
4. `Vfio` trait 封装了 `region_read/write`、irq enable/disable 等能力，`VfioDeviceWrapper` 已对接底层 `vfio_ioctls::VfioDevice`。
   - 位置：src/vmm/src/devices/vfio/pcie/vfio.rs:29
5. `VfioCommon` 是未来核心路径（配置空间策略、BAR/MMIO、MSI-X 管理），当前主要逻辑仍是 TODO。
   - 位置：src/vmm/src/devices/vfio/pcie/vfio.rs:151

## 5. 当前状态评估（面向 MMIO + DMA）

### 已有基础

1. 启动路径可创建 KVM VFIO device、VfioContainer、VfioDevice。
2. 已有全量 guest memory 的 `vfio_dma_map` 示例流程。
3. PCI ECAM 配置空间总线已存在，VFIO 设备可被挂入 `pci_bus`。

### 关键缺口

1. 缺少设备配置入口（硬编码 BDF 路径和开关）：
   - 设备路径固定为 `/sys/bus/pci/devices/0000:ba:02.0`
   - 没有从配置传入 NPU 设备列表
2. 错误处理不完整：大量 `unwrap/panic`，启动失败不可恢复。
3. VFIO 的配置空间转发未实现：`VfioPciDevice::{read,write}_config_register` 仍返回固定值。
4. BAR MMIO 数据面未接通：
   - 未分配/记录 BAR GPA
   - 未将 VFIO BAR 区间 `insert` 到 `vm.common.mmio_bus`
   - `read_bar/write_bar` 未透传到 `vfio region_read/region_write`
5. BAR 重编程路径缺失：
   - `Vm::move_bar` 目前不支持
   - `VfioPciDevice::move_bar` 仅注释
6. DMA 映射策略仍粗粒度：
   - 缺少生命周期管理（unmap 时机）
   - 缺少热插拔/内存变更联动
7. MSI-X 路径尚未开始实现（符合当前优先级“先不做中断”）。

## 6. 面向当前目标的 TODO List

以下 TODO 按优先级和依赖排序，优先完成 MMIO 与 DMA。

### Phase A: 启动入口与可配置化（先做）

1. 为 VFIO NPU 增加配置项（设备 BDF/path、是否启用、IOMMU group 检查开关）。
2. `attach_vfio_pcie_device` 改为按配置选择设备，而非硬编码路径。
3. 替换 `unwrap/panic` 为结构化错误并挂到 `StartMicrovmError`。
4. 增加关键日志字段：设备标识、IOMMU group、BDF、region 概览、DMA map 结果。

### Phase B: 配置空间通路（先做）(doing now)

1. 在 `VfioPciDevice` 中实现：
   - `read_config_register` -> 调 `VfioCommon.read_config_register`
   - `write_config_register` -> 调 `VfioCommon.write_config_register`
2. 在 `VfioCommon` 中实现最小可用策略：
   - 非 BAR、非 MSI-X 字段：直接透传 VFIO config region
   - BAR 寄存器：先与 `PciConfiguration` 对齐，保证 BAR 尺寸探测和寄存器语义
3. 明确并实现“透传字段白名单/黑名单”规则（例如 command/status/header type 的掩码策略）。

### Phase C: MMIO BAR 数据面（核心）(doing now)

1. 枚举 VFIO region，识别可 mmap/可 MMIO 的 BAR。
2. 为每个 BAR 申请 guest GPA（先固定策略到 mmio64 allocator）。
3. 将 `VfioPciDevice` 按 BAR 区间注册到 `vm.common.mmio_bus`。
4. 实现 `read_bar/write_bar`：
   - 先做直接 `vfio region_read/region_write` 透传
   - 后续再处理特殊窗口（MSI-X table/PBA）
5. `attach_pci_vfio_device` 对齐 virtio PCI 路径，补齐 BAR 注册逻辑。

### Phase D: DMA 完整化（核心）

1. 抽象 DMA map 管理器：记录 `(iova, size, host_va)`，支持回滚和清理。
2. 处理失败回滚：任一 region 映射失败时执行已映射区域 unmap。
3. 明确与 hotplug memory/virtio-mem 的策略：
   - 当前注释已提到不注册 virtio-mem 区域，需要实现可验证逻辑。
4. 增加 VM 关闭或设备移除时的 DMA unmap。
5. 补充 IOMMU/权限前置检查，给出可读错误。

### Phase E: BAR 重定位与重编程（MMIO 稳定后）

1. 设计 Firecracker 内部 BAR relocation 支持路径：
   - 当前 `Vm::move_bar = NotSupported`，需要先决定是否支持 VFIO 设备 BAR move。
2. 若短期不支持 move：
   - 在配置空间写 BAR 时明确拒绝策略和日志，避免 silent failure。
3. 若支持 move：
   - 实现 `VfioPciDevice::move_bar`，联动 mmio bus remove/insert 与 KVM user memory region。

### Phase F: MSI-X 中断（按你的目标放到后续）

1. 在 MMIO + DMA 稳定后，补齐 `VfioCommon` 中 MSI-X 相关流程：
   - capability 读写
   - vector table/PBA
   - irqfd 绑定与 mask/unmask
2. 结合 `MsixVectorGroup` 建立与 VFIO IRQ index 的映射关系。
3. 明确 INTx/MSI 的禁用策略，只保留 MSI-X（如符合设备需求）。

## 7. 建议的近期里程碑

1. M1: 配置化 + 错误处理改造 + 配置空间可读写（不含中断）
2. M2: BAR 注册到 mmio bus + BAR 读写透传可跑通
3. M3: DMA map/unmap 生命周期闭环 + 稳定性验证
4. M4: 再进入 MSI-X 实现与中断验证

## 8. 与当前测试策略的对齐

你提到的正式测试路径是“比对 FC 与 CH 在 KVM/VFIO 系统调用上的差异”。
当前阶段建议先聚焦功能闭环：
1. 功能性检查：guest 侧可枚举设备、可访问 BAR、DMA 收发可用。
2. 稳定性检查：反复启动/关闭、异常路径回滚、资源无泄漏。
3. 等 MMIO + DMA 稳定后，再接 syscall diff 测试与 MSI-X 中断测试。

---

如需下一步直接落地，我建议先从 Phase B + Phase C 的最小实现开始：
1. 打通 `VfioPciDevice` 到 `VfioCommon` 的配置空间与 BAR 透传。
2. 在 `attach_pci_vfio_device` 里补 BAR 注册到 `mmio_bus` 的路径。
3. 先不碰 MSI-X，仅保留占位与日志。


已经完成框架切换的第一步，接下来我会跑一次错误检查，修掉这次改动引入的编译问题，确保你可以在这个基础上继续填功能。

Checked configuration.rs, vfio.rs, mod.rs, no problems found

我还会做一次最小范围编译验证（只检查 vmm crate），确认新框架没有破坏当前工程构建；如果有错误我会继续修到可编译。

结论已经明确并且代码框架已落好：你的判断在 MVP 阶段是正确的。下面我会给出简要架构判断依据、这次实际改动点、以及当前框架边界（哪些还需要你下一步补齐）。你的判断是正确的，而且对当前阶段是更稳妥的路线。

核心原因是：现有 PciConfiguration 带有明显 virtio 假设，不适合作为 VFIO 的配置基座，尤其是 BAR 与 MSI-X 语义。
比如在 configuration.rs:259 的 add_pci_bar 逻辑里，默认 BAR 使用模型是围绕 virtio 设计的；而 VFIO 需要更“透传设备真实配置 + 选择性拦截”的模型。

已按“正确”路径落了可编译的代码框架：

新增独立配置结构
新文件：configuration.rs
新结构定义：configuration.rs:26
已包含 MVP 需要的基础能力：
配置空间寄存器缓存与按 byte/word/dword 写入：configuration.rs:47
BAR base 查找：configuration.rs:104
BAR 重编程检测接口骨架：configuration.rs:127
MSI-X capability 索引预留接口（set/get）
切换 VfioCommon 到新配置结构
VfioCommon.configuration 已从 PciConfiguration 切换到 VfioPcieConfiguration：
vfio.rs:157
vfio.rs:158
构造流程已改为创建新结构：
vfio.rs:183
模块导出
在 VFIO PCIe 模块里新增导出：
mod.rs:3
验证结果：

已执行 cargo check -p vmm，编译通过。
当前仅有项目里已有的若干 unused import 警告，无新增编译错误阻塞。
当前框架边界（你下一步要填的功能）：

用 VFIO config region 初始化并持续同步本地 config cache（尤其 BAR 寄存器与 mask）。
完整补齐 64-bit BAR 成对语义与 size-probe 行为（现在是 MVP 占位）。
把 MSI-X table/PBA 的 capability 解析结果回填到新结构（替代对旧配置对象的隐式依赖）。
建立 base -> region index 显式映射，逐步去掉 BAR0 fallback。
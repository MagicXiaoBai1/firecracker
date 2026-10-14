# Firecracker VFIO PCI 打通方案设计（中文详版）

## 1. 文档目的

本文档描述本次在 Firecracker 中实现的 VFIO PCI 最小可用打通方案，目标是：

1. 在不引入新 API 的前提下，将一个宿主机 PCI 设备映射为来宾可枚举的 PCI 设备。
2. 在 microVM 内通过 `lspci` 可以看到该设备。
3. 以最小源码改动验证端到端链路，后续再逐步产品化。

本文档同时记录当前实现边界、已完成内容、验证方法、技术债与下一阶段任务。

## 2. 背景与目标

### 2.1 背景

参考 cloud-hypervisor 的早期 VFIO PCI 思路，本次优先完成“枚举可见”目标，而不是一次性做完整透传数据面。

你当前最关心的验收标准是：

1. 进入 microVM。
2. 执行 `lspci`。
3. 看到新增设备条目。

### 2.2 目标（本阶段）

1. 引入 rust-vmm 的 VFIO 相关依赖，完成编译链路。
2. 在 Firecracker 启用 PCI 模式时，自动尝试附加一个固定宿主 BDF 对应的设备。
3. 来宾可通过 PCI 配置空间读取到该设备基础信息（vendor/device/class），可被 `lspci` 枚举。

### 2.3 非目标（本阶段明确不做）

1. 不新增 Firecracker REST API 或 Swagger 字段。
2. 不做完整 BAR 数据面转发与 DMA 映射。
3. 不做中断路径（MSI/MSI-X/INTx）透传。
4. 不做快照/恢复兼容。
5. 不做多设备管理与动态热插拔控制。

## 3. 现状与核心设计取舍

### 3.1 取舍原则

本阶段采用“最小改动 + 最快可验证”策略：

1. 先保证来宾 PCI 枚举可见。
2. 先不强依赖完整 VFIO 内核接口调用路径。
3. 先用宿主 sysfs 的配置空间文件读取作为最小数据源。

### 3.2 固定设备接入方式

当前实现通过环境变量传入宿主 BDF：

1. 环境变量名：`FC_VFIO_PCI_BDF`
2. 典型值：`0000:7a:02.0`
3. 运行时读取路径：`/sys/bus/pci/devices/<BDF>/config`

如果变量为空，则不附加该 demo 设备。

## 4. 已完成的实现内容

### 4.1 依赖引入

在 `src/vmm/Cargo.toml` 中加入：

1. `vfio-bindings = { version = "0.6.2", default-features = false }`
2. `vfio-ioctls = { version = "0.6.0", default-features = false }`

说明：当前最小方案虽未深入使用完整 ioctls 数据面，但依赖已接入，后续扩展可直接基于此演进。

### 4.2 新增最小 VFIO PCI 设备模块

新增模块目录：

1. `src/vmm/src/devices/pci/vfio/mod.rs`
2. `src/vmm/src/devices/pci/vfio/device.rs`

核心类型：`FixedVfioPciDevice`

核心行为：

1. 校验 BDF 格式（`dddd:bb:dd.f`）。
2. 从宿主读取 256B PCI 配置空间。
3. 实现 `PciDevice` 的配置空间读取接口。
4. 配置空间写入暂不处理（只读，满足枚举目标）。

### 4.3 挂接到 PCI 设备树

在 `src/vmm/src/devices/pci/mod.rs` 中导出新模块：

1. 增加 `pub mod vfio;`

### 4.4 Device Manager 自动附加逻辑

在 `src/vmm/src/device_manager/pci_mngr.rs` 增加：

1. `PciDevices` 新字段：`fixed_vfio_device`
2. 新错误类型分支：`FixedVfio...`
3. 新方法：`attach_fixed_vfio_device()`

附加流程：

1. 判断 PCI segment 已启用。
2. 读取 `FC_VFIO_PCI_BDF`。
3. 申请 guest SBDF 槽位。
4. 创建 `FixedVfioPciDevice`。
5. 注册到 `PciBus`。

### 4.5 在启用 PCI 时触发自动附加

在 `src/vmm/src/device_manager/mod.rs` 的 `enable_pci()` 中：

1. 先 `attach_pci_segment(vm)`
2. 再 `attach_fixed_vfio_device()`

由此保证当 Firecracker 以 PCI 模式启动时，demo 设备自动尝试挂入。

### 4.6 启动脚本增强

更新 `start_fc_socket.sh`：

1. 增加 `FC_VFIO_PCI_BDF` 默认值（当前默认 `0000:7a:02.0`，可覆盖）。
2. 增加 `FC_ENABLE_PCI`（默认 1），自动带 `--enable-pci`。
3. 通过 `sudo` 透传 `FC_VFIO_PCI_BDF` 到 Firecracker 进程。

### 4.7 源码调用链（从启动参数到设备可枚举）

这一节给出本次改动真正生效的关键执行路径，帮助定位“为什么 VM 在 boot 后可以读到这个 PCI 设备”。

#### 步骤 A：命令行打开 PCI 总线

入口在 `src/firecracker/src/main.rs`：

1. 解析 `--enable-pci`，得到 `pci_enabled`。
2. 将 `pci_enabled` 写入 `VmResources.pci_enabled`。

对应构建逻辑在 `src/vmm/src/builder.rs`：

1. `if vm_resources.pci_enabled { device_manager.enable_pci(&vm)?; }`
2. 如果没开 PCI，会走 `boot_cmdline.insert("pci", "off")`。

这说明：

1. VM 是否存在 PCI 总线，由 `--enable-pci` 决定。
2. 你的脚本里默认打开 `FC_ENABLE_PCI=1` 并传 `--enable-pci`，所以这条路径被触发。

#### 步骤 B：创建 PCI segment 与配置空间访问通道

在 `src/vmm/src/device_manager/mod.rs` 的 `enable_pci()`：

1. `attach_pci_segment(vm)`
2. `attach_fixed_vfio_device()`

其中 `attach_pci_segment` 在 `src/vmm/src/device_manager/pci_mngr.rs`。

再进入 `src/vmm/src/devices/pci/pci_segment.rs`：

1. 创建 `PciBus`，默认注册 `PciRoot`（设备 0）。
2. 创建 `PciConfigMmio`，并把 ECAM 配置空间窗口挂到 `vm.common.mmio_bus`。
3. 在 x86_64 上额外创建 `PciConfigIo`，挂到 0xcf8/0xcfc 端口。

这一步的意义：

1. 让 guest 在 boot 与枚举期间有地方读 PCI config space。
2. 后续所有 `lspci` 读取最终都会走到这些配置空间处理器。

#### 步骤 C：把固定设备注册到 PciBus

在 `src/vmm/src/device_manager/pci_mngr.rs` 的 `attach_fixed_vfio_device()`：

1. 读环境变量 `FC_VFIO_PCI_BDF`。
2. 调用 `next_device_sbdf()` 分配来宾槽位（所以常见为 00:01.0）。
3. 创建 `FixedVfioPciDevice::new(&host_bdf)`。
4. 调用 `pci_bus.add_device(sbdf.device(), vfio_device)`。

这一步是“能被枚举”的决定性动作：

1. 如果没 add_device，guest 读该 slot 会得到 0xffff_ffff（等价不存在）。
2. add_device 后，该 slot 的 config 读取会命中我们的设备对象。

#### 步骤 D：设备对象如何响应 config 读取

在 `src/vmm/src/devices/pci/vfio/device.rs`：

1. `new()` 读取 `/sys/bus/pci/devices/<BDF>/config` 的前 256B 到本地缓存。
2. `read_config_register(reg_idx)` 从缓存按 DWORD 返回数据。
3. `write_config_register` 目前是 no-op（只读 demo）。

对应总线路径在 `src/vmm/src/pci/bus.rs`：

1. `PciConfigMmio::config_space_read` 或 `PciConfigIo::config_space_read`
2. 通过 `pci_bus.devices.get(&device)` 找到设备
3. 调用 `read_config_register(register)`

因此 guest 的 `lspci` 最终拿到的是这份配置空间缓存中的 vendor/device/class 信息。

### 4.8 回答核心问题：是不是因为写了 FDT 才能读到设备

结论先说：

1. 在 aarch64 上，FDT 是“让内核知道有 PCI host bridge 与 ECAM 窗口”的必要条件之一。
2. 但“具体某个函数设备能被枚举到”，不是靠为该函数单独写 FDT 节点，而是靠 PciBus 对应 slot 的 config read 返回了有效 vendor/device。
3. 在 x86_64 上，不走 FDT，而是 ACPI（DSDT + MCFG）加 PCI config 通道。

#### aarch64 路径

`src/vmm/src/arch/aarch64/fdt.rs` 会在 FDT 生成时调用 `create_pci_nodes()`：

1. 仅当 `pci_segment` 存在才创建 `pci@...` 节点。
2. 节点里提供 `pci-host-ecam-generic`、`reg`（ECAM 基址）、`ranges`、`bus-range`、`msi-map` 等。

这告诉 guest 内核：

1. PCI 配置空间在哪个 MMIO 区域。
2. PCI 地址如何映射到 CPU 地址。

但它并不会为 `00:01.0` 单独写一个 USB 节点。

#### x86_64 路径

`src/vmm/src/acpi/mod.rs` 在 `create_acpi_tables()` 中构建：

1. DSDT（包含 PCI segment AML）
2. MCFG（给出 PCI MMCONFIG/ECAM 地址）

再结合 `PciConfigIo` 与 `PciConfigMmio`，guest 扫描时同样能读到 slot 配置空间。

### 4.9 为什么本次没有改 FDT 也能看到设备（你的实测现象）

你这次看到设备，核心是两件事同时成立：

1. PCI 总线已启用（`--enable-pci` 生效，segment 已创建）。
2. 固定设备已经 add 到 `PciBus`，且 config 读取能返回有效 vendor/device。

其中：

1. 若是 aarch64，FDT 中已有通用 PCI host 节点生成逻辑，只要 `pci_segment` 存在它就会出现。
2. 若是 x86_64，主要依赖 ACPI 与 PCI config 访问机制。

所以本次“设备可见”的直接原因，不是新增了某个设备级 FDT 条目，而是总线注册 + 配置空间读取链路打通。

### 4.10 aarch64 视角下的最终因果链（重点）

你关心的核心问题是：在 aarch64 上，是否写了 FDT，为什么不改 FDT 设备节点也能枚举到新设备。

结论分三层：

1. 写了 FDT，但写的是 PCI Host 节点，不是某个具体 PCI function 节点。
2. 能否枚举到具体设备，取决于该 BDF 槽位的配置空间读请求是否返回有效 vendor/device。
3. 我们本次修改的是“把设备对象挂到 PciBus 并让 config read 有值”，不是“给该设备写独立 FDT 子节点”。

按 boot 时序拆解如下。

第一步：启用 PCI（否则 FDT 里不会有 PCI host）

1. 启动参数带上 --enable-pci。
2. 代码在 [src/firecracker/src/main.rs](src/firecracker/src/main.rs#L396) 解析出 pci_enabled。
3. 代码在 [src/vmm/src/builder.rs](src/vmm/src/builder.rs#L208) 根据 vm_resources.pci_enabled 决定是否调用 enable_pci。

第二步：创建 PCI segment（使 FDT 能写入 PCI host 描述）

1. 代码在 [src/vmm/src/device_manager/mod.rs](src/vmm/src/device_manager/mod.rs#L346) 的 enable_pci 里调用 attach_pci_segment。
2. attach_pci_segment 在 [src/vmm/src/device_manager/pci_mngr.rs](src/vmm/src/device_manager/pci_mngr.rs#L82)。
3. 真正创建 segment 在 [src/vmm/src/devices/pci/pci_segment.rs](src/vmm/src/devices/pci/pci_segment.rs#L73)。
4. 这里会创建 PciBus、PciConfigMmio，并把 ECAM 窗口挂到 vm.common.mmio_bus。

第三步：FDT 写入 PCI host 节点（总线层描述）

1. FDT 入口在 [src/vmm/src/arch/aarch64/fdt.rs](src/vmm/src/arch/aarch64/fdt.rs#L67)。
2. 调用 create_pci_nodes 在 [src/vmm/src/arch/aarch64/fdt.rs](src/vmm/src/arch/aarch64/fdt.rs#L102)。
3. create_pci_nodes 函数在 [src/vmm/src/arch/aarch64/fdt.rs](src/vmm/src/arch/aarch64/fdt.rs#L474)。
4. 仅当 pci_segment 存在时才生成 pci@... 节点（见 [src/vmm/src/arch/aarch64/fdt.rs](src/vmm/src/arch/aarch64/fdt.rs#L475)）。

这一步告诉 guest 内核：

1. 有一个 PCI host。
2. ECAM 配置空间在哪。
3. 地址转换 ranges 如何做。

但这一步不会逐个声明 00:01.0、00:02.0 这种 function。

第四步：把固定宿主设备映射成一个 guest slot（设备层可见）

1. enable_pci 在 attach_pci_segment 后调用 attach_fixed_vfio_device，见 [src/vmm/src/device_manager/mod.rs](src/vmm/src/device_manager/mod.rs#L348)。
2. attach_fixed_vfio_device 在 [src/vmm/src/device_manager/pci_mngr.rs](src/vmm/src/device_manager/pci_mngr.rs#L95)。
3. 读取环境变量 FC_VFIO_PCI_BDF，见 [src/vmm/src/device_manager/pci_mngr.rs](src/vmm/src/device_manager/pci_mngr.rs#L100)。
4. 分配 SBDF 并 add_device 到 PciBus。

第五步：guest 枚举时读取配置空间，命中我们设备对象

1. guest 扫描 PCI 时，会读 ECAM 配置空间。
2. 读请求进入 PciConfigMmio::config_space_read，定义在 [src/vmm/src/pci/bus.rs](src/vmm/src/pci/bus.rs#L320)。
3. 通过 devices.get(device) 找到目标设备，再调 read_config_register。
4. read_config_register 实现在 [src/vmm/src/devices/pci/vfio/device.rs](src/vmm/src/devices/pci/vfio/device.rs#L68)。

因为我们返回了有效 vendor/device/class，内核就会把它识别为真实存在的 PCI function，于是 lspci 可见。

一句话总结：

1. FDT 负责让内核知道“有 PCI 总线并知道去哪里读配置空间”。
2. PciBus 设备注册和 read_config_register 负责让内核读到“这个具体槽位有设备”。
3. 本次让设备出现的关键改动在第 2 部分，而不是新增 FDT 设备子节点。

## 5. 为什么来宾 BDF 是 00:01.0

来宾看到的是虚拟 PCI 拓扑地址，不是宿主原始 BDF。

分配规则：

1. `00:00.0` 是根桥（Host bridge）。
2. 新附加设备按 `next_device_sbdf()` 从下一个 slot 分配。
3. 因此第一个附加设备通常显示为 `00:01.0`。

所以“宿主 0000:7a:02.0”映射为“来宾 00:01.0”是正常行为。

## 6. 编译与验证状态

### 6.1 编译状态

已验证通过：

1. `cargo check -p vmm`
2. `cargo build -p firecracker --release`

### 6.2 验收结果

在来宾中 `lspci` 可见：

1. `00:00.0 Host bridge`
2. `00:01.0 USB controller: Huawei ...`
3. 其他 virtio 设备

这说明当前阶段目标“新增设备可枚举”已经达成。

## 7. 使用说明（当前版本）

### 7.1 宿主侧准备

1. 目标设备解绑原驱动。
2. 绑定到 `vfio-pci`（建议通过 `lspci -k -s <BDF>` 确认）。
3. 确认设备节点与权限满足 Firecracker 运行条件。

### 7.2 启动 Firecracker

示例：

1. 使用默认 BDF：直接执行脚本。
2. 覆盖 BDF：设置 `FC_VFIO_PCI_BDF` 后执行脚本。

### 7.3 来宾侧验证

1. 若有 `lspci`：执行 `lspci -nn`。
2. 若无 `lspci`：检查 `/sys/bus/pci/devices` 下 `vendor/device/class`。

## 8. 当前方案的边界与技术债

### 8.1 功能边界

1. 仅保证配置空间可读与枚举可见。
2. 不保证设备驱动在来宾中可正常工作。
3. 不提供 BAR 访问功能。
4. 不提供 DMA/中断功能。

### 8.2 技术债

1. `FixedVfioPciDevice` 名称与行为偏 demo，后续需抽象为通用 `VfioPciDevice`。
2. 配置空间为初始化时一次性读取快照，未做动态同步。
3. 无设备生命周期治理（reset、错误恢复、资源回收细粒度处理）。
4. 与 snapshot/persist 语义尚未建立完整契约。

## 9. 下一阶段任务清单（建议）

下面按优先级给出可执行路线。

### P0：从“可见”到“可访问”

1. 实现 BAR 发现与分配。
2. 将 BAR 区间注册到 mmio bus。
3. 实现 BAR read/write 到 VFIO region 的转发。
4. 增加最小错误日志与统计。

验收：

1. 来宾驱动可完成最小寄存器访问。
2. 非法偏移访问可被明确拦截并记录。

### P1：从“可访问”到“可用”

1. 实现 DMA map/unmap（至少覆盖启动时内存）。
2. 实现中断最小闭环（优先 MSI-X）。
3. 完善设备 teardown 清理。

验收：

1. 指定目标设备可运行基础 I/O。
2. 设备重启/销毁后无残留映射。

### P2：工程化与产品化

1. 从环境变量迁移到正式 API 配置模型。
2. 增加多设备支持与冲突校验。
3. 补齐 seccomp/jailer 生产策略。
4. 明确 snapshot 禁止或支持矩阵。

验收：

1. API 可声明多个 VFIO 设备。
2. 启动失败可给出可操作错误提示。

## 10. 风险清单与规避建议

1. 风险：宿主 IOMMU group 不可隔离，导致绑定或运行异常。
   建议：将 group 检查纳入启动前脚本。

2. 风险：来宾可见但驱动不可用，误判为“透传完成”。
   建议：验收分层，明确“枚举成功”与“功能可用”是两级目标。

3. 风险：默认参数误指向错误设备。
   建议：脚本启动前打印 BDF、vendor/device 并二次确认。

4. 风险：后续扩展引入回归。
   建议：为 P0/P1 分别建立单元测试与最小集成用例。

## 11. 对你当前结果的结论

结合当前日志与来宾 `lspci` 结果，可以确认：

1. Firecracker PCI 总线已启用。
2. 固定宿主设备已成功映射为来宾可枚举设备。
3. 本阶段 demo 目标已经达成。

后续如果你继续推进，我建议立即进入 P0（BAR 与 MMIO 转发），这是从“看到设备”走向“设备可用”的关键分水岭。

## 12. 本次改动清单（文件 + 函数 + 目的）

下面是本次为实现可枚举打通而修改的核心文件与函数。

### 12.1 依赖与构建

1. 文件 [src/vmm/Cargo.toml](src/vmm/Cargo.toml)
2. 修改点：新增 vfio-bindings 与 vfio-ioctls 依赖
3. 目的：为后续 VFIO 演进预置依赖并保证本分支可编译

### 12.2 新增固定设备模型

1. 文件 [src/vmm/src/devices/pci/vfio/mod.rs](src/vmm/src/devices/pci/vfio/mod.rs)
2. 修改点：导出 FixedVfioPciDevice 与错误类型
3. 目的：形成独立模块边界

1. 文件 [src/vmm/src/devices/pci/vfio/device.rs](src/vmm/src/devices/pci/vfio/device.rs)
2. 关键函数：
3. FixedVfioPciDevice::new
4. PciDevice::read_config_register
5. PciDevice::write_config_register
6. is_valid_bdf
7. 目的：
8. 从 /sys/bus/pci/devices/<BDF>/config 读取配置空间快照
9. 对 guest 提供可读的 PCI config DWORD
10. 写路径保持 no-op，满足 demo 可见性目标

### 12.3 挂入 PCI 设备体系

1. 文件 [src/vmm/src/devices/pci/mod.rs](src/vmm/src/devices/pci/mod.rs)
2. 修改点：增加 vfio 模块导出
3. 目的：让 device manager 可引用新设备类型

### 12.4 设备管理器自动附加逻辑

1. 文件 [src/vmm/src/device_manager/pci_mngr.rs](src/vmm/src/device_manager/pci_mngr.rs)
2. 关键函数与字段：
3. FIXED_VFIO_BDF_ENV
4. attach_fixed_vfio_device
5. PciDevices.fixed_vfio_device
6. 目的：
7. 读取 FC_VFIO_PCI_BDF
8. 分配 guest SBDF
9. 创建 FixedVfioPciDevice
10. add_device 到 PciBus

1. 文件 [src/vmm/src/device_manager/mod.rs](src/vmm/src/device_manager/mod.rs)
2. 关键函数：enable_pci
3. 修改点：在 attach_pci_segment 后调用 attach_fixed_vfio_device
4. 目的：只要开启 PCI，就自动尝试附加 demo 设备

### 12.5 启动脚本

1. 文件 [start_fc_socket.sh](start_fc_socket.sh)
2. 修改点：
3. 新增 FC_VFIO_PCI_BDF 默认值与覆盖能力
4. 新增 FC_ENABLE_PCI 默认值，自动加 --enable-pci
5. sudo 启动时透传 FC_VFIO_PCI_BDF
6. 目的：降低验证门槛，避免每次手工拼启动参数

### 12.6 本次没有改动但与原理强相关的文件

1. [src/vmm/src/arch/aarch64/fdt.rs](src/vmm/src/arch/aarch64/fdt.rs)
2. 相关函数：create_fdt、create_pci_nodes
3. 作用：当 pci_segment 存在时，生成 PCI host 的 FDT 描述

1. [src/vmm/src/pci/bus.rs](src/vmm/src/pci/bus.rs)
2. 相关函数：PciConfigMmio::config_space_read
3. 作用：把 guest 的配置空间读请求分发到注册设备的 read_config_register

## 13. 下一阶段：使用 VFIO fd 读配置空间（快速 demo 版本）

### 13.1 目标

升级当前实现，从“读 sysfs config 快照”改为“通过 VFIO device fd 读 VFIO config region”，让设备枚举时真正通过 VFIO 通道访问直通设备。

### 13.2 设计取舍

1. 仍不做 BAR/DMA/IRQ，只改 config 读写路径。
2. 仍用固定 BDF + 环境变量。
3. 仍不新增 API。
4. 验收标准：guest `lspci` 仍可见设备，但现在是通过 VFIO fd 读的。

### 13.3 核心改动

#### 13.3.1 修改 FixedVfioPciDevice::new

1. 打开 `/dev/vfio/vfio` 获取 container fd。
2. 读取 `/sys/bus/pci/devices/<BDF>/iommu_group` 获取 group id。
3. 打开 `/dev/vfio/<group_id>` 获取 group fd。
4. 调用 VFIO_GROUP_SET_CONTAINER 把 group 加入 container。
5. 调用 VFIO_SET_IOMMU 设置 IOMMU 类型（e.g., VFIO_TYPE1_IOMMU）。
6. 调用 VFIO_GROUP_GET_DEVICE_FD 获取 device fd。
7. 保存 container fd, group fd, device fd 到结构体。

#### 13.3.2 修改 read_config_register/write_config_register

1. 使用 device fd 调用 VFIO_DEVICE_GET_REGION_INFO 获取 config region 信息。
2. 使用 region_read/region_write（或 pread/pwrite）访问 VFIO_PCI_CONFIG_REGION_INDEX。
3. 对于 BAR 寄存器，仍做本地 cache trap（避免 guest BAR 地址写进设备）。

#### 13.3.3 错误处理与清理

1. 在 Drop impl 中关闭所有 fd，并调用 VFIO_GROUP_UNSET_CONTAINER 等清理。
2. 添加 VFIO 相关错误类型。

### 13.4 验收方法

1. 编译通过 `cargo build -p firecracker --release`。
2. 启动 VM，guest `lspci` 可见设备。
3. 日志中打印 VFIO fd 打开成功信息。
4. 对比前后：现在 config 读是动态从 VFIO fd 来的，不是静态快照。

### 13.5 风险

1. VFIO 绑定失败：确保设备已绑定 vfio-pci，且 group 可访问。
2. 权限问题：Firecracker 需有 /dev/vfio/* 访问权。
3. 性能：VFIO ioctl 比 sysfs 读慢，但 demo 阶段可接受。

### 13.6 后续

这个版本完成后，可作为“真 VFIO config 访问”的起点，再逐步加 BAR 转发等。

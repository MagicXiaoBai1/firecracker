<!--
Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Ascend VFIO BAR Quirks — Firecracker 移植设计

本文是 **实现设计文档**，描述如何将昇腾官方 QEMU patch（`hw/vfio/pci-quirks.c`）中的 **Ascend BAR quirk** 移植到 Firecracker `fixed-vfio` 路径。

**相关文档**

| 文档 | 内容 |
|------|------|
| [ascend-vfio-quirks-qemu-summary-zh.md](./ascend-vfio-quirks-qemu-summary-zh.md) | QEMU 原版 quirk 行为总结 |
| [vfio-msix-implementation-zh.md](./vfio-msix-implementation-zh.md) | MSI-X table 拦截（与本 quirk 正交） |
| [pci-bar-move-implementation-zh.md](./pci-bar-move-implementation-zh.md) | NPU BAR 布局与 reprogram |

**参考来源**：[昇腾 configureg_0010 — 修改 QEMU](https://www.hiascend.com/document/detail/zh/Atlas%20200I%20A2/260RC1/re/virtualmachineconfiguration/configureg_0010.html)

---

## 1. 目标与非目标

### 1.1 目标

1. 在 guest 访问 Ascend 设备 BAR 内 **xloader-updating 寄存器** 时，行为与 QEMU patch **一致**：
   - **读** → 转发 VFIO region read；
   - **写** → **丢弃**（可选 debug log），**不**写入 host VFIO。
2. 首版覆盖当前环境：**Atlas 200I A2 / Ascend310P（19e5:d500）1P**。
3. 架构上可扩展至 910 / 910A2 / 310B / 310 及 310P 2P。

### 1.2 非目标

| 非目标 | 原因 |
|--------|------|
| 移植 QEMU `linux-user/syscall.c`、`virt.c`、`common.c` | 与 Firecracker 无关（见 summary 文档） |
| 替代 MSI-X 拦截 | MSIX table 仍走现有 `VfioMsixManager` |
| 解决 ENOSYS / memslot 崩溃 | 独立问题；quirk 仅对齐 init/MMIO 语义 |
| Guest davinci 驱动修改 | 保持 guest 与 QEMU 场景一致 |

---

## 2. QEMU 行为摘要（实现基准）

### 2.1 机制

QEMU 在 VFIO BAR 的 MemoryRegion 树中，于指定 `(bar, offset)` 叠加 **高优先级** 4 字节 IO 区域：

```text
Guest MMIO @ BAR[n] + offset
        │
        ▼
  落在 Ascend quirk 窗口？
        │
   是  ├─ read  → vfio_region_read(bar, offset)
        └─ write → log + no-op（不写设备）
        │
   否  └─ 正常 BAR 透传
```

### 2.2 Ascend310P（本环境）

| 字段 | 值 |
|------|-----|
| Vendor / Device | `0x19e5` / `0xd500` |
| BAR | **2**（VFIO region index = 2） |
| Quirk offset | **`0x100430`** |
| Quirk size | **4** 字节 |
| 1P / 2P | Subsystem Device ID（config `0x2E`，16-bit） |

| Subsystem ID | 类型 | Quirk 个数 | 第二窗口 |
|--------------|------|------------|----------|
| `0x0100` – `0x010f` | 1P | 1 | — |
| `0x0110` – `0x011f` | 2P | 2 | `0x100430 + 128MiB` |
| 其它 | QEMU 不装 quirk | 0 | — |

**当前设备（200I A2）预期为 1P**；日志中 config 曾出现 `0x0100` 量级，实现时仍应在 attach 时读取并记录 subsystem ID。

---

## 3. 现有 Firecracker 代码与插入点

### 3.1 相关模块

| 文件 | 现状 |
|------|------|
| `devices/pci/vfio/device.rs` | `read_bar` / `write_bar`：MSIX 拦截 → VFIO region 透传 |
| `devices/pci/vfio/msix_table.rs` | MSI-X table GPA 区间拦截（策略 A） |
| `devices/pci/vfio/mod.rs` | 子模块导出 |

`FixedVfioPciDevice` **当前未缓存** vendor/device/subsystem；config 读通过 `vfio_read_config_dword` + `read_config_register` 按需访问 VFIO config region。

### 3.2 MMIO 分发顺序（设计约定）

与 MSI-X 类似，quirk 作为 **BAR 内子区间拦截**，在 `read_bar` / `write_bar` 中按以下顺序处理：

```text
read_bar(base, offset, data):
  1. MSI-X table 区间？     → msix.read_table（已有）
  2. Ascend quirk 区间？    → 【新增】quirk.read
  3. 命中 mmio_maps         → device.region_read

write_bar(base, offset, data):
  1. MSI-X table 区间？     → msix.write_table + 可选 VFIO 写（已有）
  2. Ascend quirk 区间？    → 【新增】quirk.write（no-op，不写 VFIO）
  3. 命中 mmio_maps         → device.region_write
```

**理由**

- Quirk 窗口（`0x100430`）与 MSI-X table 不重叠（310P table 在 BAR0 内，见现有日志）。
- Quirk **优先于** 裸 VFIO 透传，等价 QEMU `memory_region_add_subregion_overlap(..., priority=1)`。

### 3.3 与 guest GPA 的关系

`read_bar` / `write_bar` 的 `offset` 是 **BAR 内字节偏移**（相对 guest BAR base），与 QEMU quirk 的 `addr + quirk->offset` 一致；**无需**再减 guest base。

---

## 4. 模块设计

### 4.1 新文件 `ascend_quirks.rs`

建议新增独立模块，避免 `device.rs` 继续膨胀：

```text
devices/pci/vfio/
  ascend_quirks.rs   ← 新增
  device.rs          ← 调用 quirk 分发
  mod.rs             ← mod ascend_quirks;
```

### 4.2 核心类型

```rust
/// PCI config offset 0x2E: Subsystem Device ID (16-bit, LE in dword @ 0x2C).
const PCI_SUBSYSTEM_ID_OFFSET: u64 = 0x2e;

/// 单个 intercept 窗口（BAR 内 offset + 长度）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AscendQuirkWindow {
    pub vfio_region_index: u32,  // BAR / VFIO region 号，310P = 2
    pub offset: u64,
    pub size: u64,               // 恒为 4
}

/// Attach 时解析出的 quirk 集合；无 quirk 则为空 Vec。
#[derive(Debug, Default)]
pub struct AscendQuirkSet {
    pub windows: Vec<AscendQuirkWindow>,
}

impl AscendQuirkSet {
    pub fn probe(vfio: &VfioDevice) -> Self { ... }

    pub fn find(&self, vfio_region: u32, offset: u64, len: usize)
        -> Option<&AscendQuirkWindow> { ... }
}
```

**不在 `FixedVfioPciDevice` 存 vendor/device 字符串**；probe 一次后将结果固化为 `AscendQuirkSet`（仅 windows 列表），attach 后只读。

### 4.3 设备表（常量 + probe 逻辑）

```rust
const PCI_VENDOR_HUAWEI: u16 = 0x19e5;
const PCI_DEVICE_ASCEND310P: u16 = 0xd500;

const ASCEND310P_XLOADER_OFFSET: u64 = 0x100_430;
const ASCEND310P_XLOADER_SIZE: u64 = 4;
const ASCEND310P_2P_STRIDE: u64 = 128 * 1024 * 1024;

const SUBSYS_310P_1P_MIN: u16 = 0x0100;
const SUBSYS_310P_1P_MAX: u16 = 0x010f;
const SUBSYS_310P_2P_MIN: u16 = 0x0110;
const SUBSYS_310P_2P_MAX: u16 = 0x011f;
```

**Phase 1** 仅实现 `probe_ascend310p`；其它型号留 `// Phase 2` stub 或表格驱动扩展。

#### `probe()` 伪代码

```rust
fn probe(vfio: &VfioDevice) -> AscendQuirkSet {
    let vendor = read_config_u16(vfio, 0x00);      // 0x19e5
    let device = read_config_u16(vfio, 0x02);      // 0xd500
    if vendor != PCI_VENDOR_HUAWEI {
        return Default::default();
    }
    match device {
        PCI_DEVICE_ASCEND310P => probe_ascend310p(vfio),
        // Phase 2: d801, d802, d105, d100 ...
        _ => Default::default(),
    }
}

fn probe_ascend310p(vfio: &VfioDevice) -> AscendQuirkSet {
    let subsys = read_config_u16(vfio, PCI_SUBSYSTEM_ID_OFFSET);
    let mut windows = Vec::new();
    if (SUBSYS_310P_1P_MIN..=SUBSYS_310P_1P_MAX).contains(&subsys) {
        windows.push(window(2, ASCEND310P_XLOADER_OFFSET));
    } else if (SUBSYS_310P_2P_MIN..=SUBSYS_310P_2P_MAX).contains(&subsys) {
        windows.push(window(2, ASCEND310P_XLOADER_OFFSET));
        windows.push(window(2, ASCEND310P_2P_STRIDE + ASCEND310P_XLOADER_OFFSET));
    }
    // 与 QEMU 一致：subsys 不在范围内 → 不安装 quirk
    AscendQuirkSet { windows }
}
```

**配置项（可选，Phase 1 可省略）**

- 环境变量 / boot 参数：`FC_ASCEND_QUIRK_FORCE_1P=1` — 当 subsystem 读取异常但已知为 200I A2 时强制 1P quirk（**默认关闭**，仅调试）。

### 4.4 Quirk 读写实现

```rust
pub fn quirk_read(
    vfio: &VfioDevice,
    window: &AscendQuirkWindow,
    data: &mut [u8],
) {
    vfio.region_read(window.vfio_region_index, data, window.offset);
    // debug log（与现有 bar_mmio_data_hex_le 一致）
}

pub fn quirk_write(
    host_bdf: &str,
    window: &AscendQuirkWindow,
    data: &[u8],
) {
    // 与 QEMU 一致：不写 VFIO
    debug!(
        "fixed-vfio: ascend quirk write dropped host_bdf={} region={} offset={:#x} len={} data={}",
        host_bdf, window.vfio_region_index, window.offset, data.len(), ...
    );
}
```

**对齐要求**

- 仅当 `[offset, offset + len)` 完全落在 `[window.offset, window.offset + window.size)` 内才命中；部分重叠走正常透传或 warn（与 MSIX 一致，建议 **完全包含** 才拦截）。
- `len` 为 1/2/4 均允许（guest 可能 byte/hword 访问）；QEMU size 参数为 unsigned size。

### 4.5 `FixedVfioPciDevice` 变更

```rust
pub struct FixedVfioPciDevice {
    // ... existing fields ...
    ascend_quirks: AscendQuirkSet,
}
```

在 `FixedVfioPciDevice::new` / attach 完成 `parse_bars` 之后：

```rust
let ascend_quirks = AscendQuirkSet::probe(&device);
if !ascend_quirks.windows.is_empty() {
    info!(
        "fixed-vfio: Ascend BAR quirks installed host_bdf={} windows={:?}",
        bdf, ascend_quirks.windows
    );
}
```

`read_bar` / `write_bar` 在 MSIX 分支之后、`mmio_maps` 循环之前：

```rust
for mmap in &self.mmio_maps {
    if let Some(w) = self.ascend_quirks.find(
        mmap.vfio_region_index, offset, data.len(),
    ) {
        if is_write {
            ascend_quirks::quirk_write(&self.bdf, w, data);
        } else {
            ascend_quirks::quirk_read(&self.device, w, data);
        }
        return ...;
    }
}
```

**注意**：`find` 需用 **vfio_region_index** 匹配，不是 PCI BAR slot 号；310P BAR2 对应 `vfio_region_index == 2`（与现有 `mmio_maps` 一致）。

---

## 5. 分阶段交付

| 阶段 | 范围 | 验收 |
|------|------|------|
| **P0** | 310P 1P：`0x100430` read 透传 / write drop | 日志见 `ascend quirk write dropped`；read `0x100430` 与 QEMU 一致 |
| **P1** | 310P 2P：+ `0x8100430` | 2P 卡或 mock subsystem |
| **P2** | 910 / 910A2 / 310B / 310 | 按 summary 表 offsets |
| **P3** | 单元测试 + 可选 metrics | 见 §7 |

---

## 6. 日志与可观测性

**调试阶段：quirk 路径的 read / write 都打 log**（与现有 `BAR MMIO read` 同级，用独立前缀便于 grep）。

| 级别 | 消息前缀 | 时机 |
|------|----------|------|
| `info` | `fixed-vfio: Ascend BAR quirks installed` | attach 时 probe 成功 |
| `debug` | `fixed-vfio: ascend quirk read` | quirk 读（转发 VFIO 后，含 val） |
| `debug` | `fixed-vfio: ascend quirk write dropped` | quirk 写被丢弃 |

建议 log 字段（读/写一致，便于确认 BAR）：

```text
fixed-vfio: ascend quirk read  host_bdf=0000:01:00.0 vfio_region=2 bar_slot=2 guest_base=0x4200000000 offset=0x100430 len=4 val=0x.... 
fixed-vfio: ascend quirk write dropped host_bdf=... vfio_region=2 bar_slot=2 guest_base=... offset=0x100430 len=4 data=0x....
```

- `vfio_region` / `bar_slot`：VFIO region index，**310P 上等于 PCI BAR2 号**
- `offset`：**BAR 内字节偏移**（非 config、非 guest 绝对 GPA）
- 上线稳定后可把 read 改为采样或去掉；**write dropped 建议保留**

Attach 时额外 log PCI ID：

```text
fixed-vfio: PCI id host_bdf=0000:01:00.0 vendor=19e5 device=d500 subsystem=0100 quirks=1
```

---

## 7. 测试计划

### 7.1 单元测试（`ascend_quirks.rs`）

| 测试 | 内容 |
|------|------|
| `find_window_exact` | offset/size 命中 |
| `find_window_miss` | 相邻 offset 不命中 |
| `probe_310p_1p_subsys` | mock config bytes → 1 窗口 |
| `probe_310p_2p_subsys` | → 2 窗口 |
| `probe_unknown_subsys` | → 空集 |

Config mock：构造 `[u8; 64]` config header，写入 vendor/device/subsystem LE。

### 7.2 集成 / 手工

1. Host bind VFIO，`FC_VFIO_PCI_BDF=0000:01:00.0` 启动 FC。
2. Guest 加载驱动或最小 MMIO 探测，确认：
   - 读 `BAR2+0x100430` 有返回值；
   - 写该地址 host log 出现 `write dropped`，且 **不** 再出现对应 `region_write`。
3. 对比 QEMU/virsh 同 guest 下该寄存器 read 值（可选）。
4. ENOSYS 是否仍出现 — **单独记录**，不作为 quirk PR 阻塞条件。

---

## 8. 风险与开放问题

| 项 | 说明 | 处理 |
|----|------|------|
| Subsystem 不在 1P/2P 范围 | QEMU 不装 quirk | 与 QEMU 一致；200I A2 实测后再决定是否 force |
| Quirk 与 ENOSYS | irq 后 memslot 问题可能仍在 | quirk PR 独立合并 |
| 多设备 | 当前仅 fixed single VFIO | `AscendQuirkSet`  per device，无全局状态 |
| 64K host page | 非本设计范围 | 不修改 DMA listener |
| 写丢弃对驱动的影响 | 官方 QEMU 如此设计 | 若 init 依赖写成功，需昇腾文档确认（预期：故意禁止 xloader update） |

### 8.1 开放问题（实现前确认）

1. **Subsystem ID**：在 host 上对 `01:00.0` 执行 `setpci -s 01:00.0 0x2e.w`，写入设计文档实测表。
2. **Guest 是否写 `0x100430`**：若仅 read，quirk write 路径仅防御性；仍应实现。
3. **Phase 2 型号**：是否同一 FC 镜像需支持 910 等 — 决定 P2 优先级。

---

## 9. 文件变更清单（预估）

| 文件 | 变更 |
|------|------|
| `devices/pci/vfio/ascend_quirks.rs` | **新增** probe + find + read/write |
| `devices/pci/vfio/mod.rs` | `mod ascend_quirks;` |
| `devices/pci/vfio/device.rs` | 字段 `ascend_quirks`；`read_bar`/`write_bar` 分发；attach 时 probe |
| `docs/ascend-vfio-quirks-qemu-summary-zh.md` | 链接本设计文档 |

**预估 diff 规模**：~250–400 行（含测试），P0 不含 910 等型号。

---

## 10. 实现检查清单

- [ ] `AscendQuirkSet::probe` 读取 vendor/device/subsystem
- [ ] 310P 1P：`region=2`, `offset=0x100430`, `size=4`
- [ ] `read_bar`：quirk read → `region_read`
- [ ] `write_bar`：quirk write → **不**调用 `region_write`
- [ ] MSIX 拦截顺序优先于 quirk（table 与 quirk 不重叠）
- [ ] attach `info` 日志含 subsystem + window 列表
- [ ] 单元测试覆盖 probe/find
- [ ] 更新 summary 文档链接

---

## 11. 附录：与 QEMU 代码映射

| QEMU | Firecracker |
|------|-------------|
| `vfio_probe_ascend310p_bar2_quirk` | `AscendQuirkSet::probe_ascend310p` |
| `vfio_ascend_quirk_read` | `ascend_quirks::quirk_read` |
| `vfio_ascend_quirk_write` | `ascend_quirks::quirk_write` |
| `memory_region_add_subregion_overlap` | `read_bar`/`write_bar` 优先分发 |
| `vfio_bar_quirk_setup()` 注册 | attach 时 `probe()` 一次 |

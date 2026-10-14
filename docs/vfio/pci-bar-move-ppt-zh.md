# Firecracker VFIO + Virtio PCI BAR Move 实践汇报

> 场景：aarch64 Firecracker + VFIO NPU 透传（`0000:01:00.0`，`19e5:d500`）+ virtio-pci（blk + net）  
> 目标：guest 正常完成 `pci_bus_assign_resources`，virtio 可用，NPU 可被 guest 枚举

---

## 第 1 页：背景与目标

**问题**

- 固定 VFIO NPU 与 virtio-pci 共存时，guest 内核在 `pci_bus_assign_resources` 会重编程 BAR。
- Firecracker 原实现不支持 BAR 搬迁，`mmio_bus` 与 ECAM 不一致，导致启动 hang 或 I/O 失败。

**目标**

- 参考 Cloud Hypervisor / 设计文档 `pci-bar-move-implementation-zh.md`，实现 **detect → pending → MSE=1 → move_bar** 完整链路。
- VFIO NPU 与 virtio 在 guest 终态布局下共存、可启动。

**Guest 终态 MMIO 布局（设计 §1.1）**

```
NPU  BAR4:  0x4000000000 – 0x41ffffffff  (8GB)
NPU  BAR2:  0x4200000000 – 0x420fffffff  (256MB)
NPU  BAR0:  0x4210000000 – 0x4210ffffff  (16MB)
blk  BAR0:  0x4211000000 – 0x421107ffff  (512KB)
net  BAR0:  0x4211080000 – 0x42110fffff  (512KB)
```

---

## 第 2 页：move_bar 实现关键步骤（1/2）

### 整体时序（对齐 Cloud Hypervisor）

```
Guest 写 ECAM BAR 寄存器
  → PciConfiguration：detect + push pending（MSE=0 时只记账）
  → Guest 写 Command 使 MSE=1
  → write_config_register drain pending，返回 Vec<BarReprogrammingParams>
  → pci/bus.rs 调用 vm.relocate_bars()
  → Vm::move_bar：mmio_bus.update_range + pci_dev.move_bar + on_bar_moved
```

### 步骤 1：BAR 重编程检测与 MSE 门控

**文件**：`src/vmm/src/pci/configuration.rs`

- Guest 写 4 字节 BAR 寄存器时调用 `detect_bar_reprogramming`，结果 push 到 `pending_bar_reprogram`。
- **仅当 MSE=1**（Memory Space Enable）时 drain pending 并返回给 bus 层执行搬迁。

```rust
// configuration.rs — write_config_register 尾部
if data.len() == 4 && offset == 0 {
    if let Some(params) = self.detect_bar_reprogramming(reg_idx, data) {
        self.pending_bar_reprogram.push(params);
    }
}
if self.mse_enabled() {
    std::mem::take(&mut self.pending_bar_reprogram)
} else {
    Vec::new()
}
```

### 步骤 2：ECAM 写入口触发 relocate

**文件**：`src/vmm/src/pci/bus.rs`

```rust
let (pending, barrier) = device.write_config_register(register, offset, data);
if let Err(e) = pci_bus.vm.relocate_bars(&pending, device.deref_mut()) {
    error!("Failed relocating device BARs: {e}");
}
```

---

## 第 3 页：move_bar 实现关键步骤（2/2）

### 步骤 3：VMM 侧统一搬迁

**调用链**（`move_bar` 不是顶层入口，外层有 retry 包装）：

```
pci/bus.rs  ECAM 写
  → device.write_config_register()  返回 pending
  → vm.relocate_bars(&pending, mover)
       ├─ mover 是 VFIO → relocate_vfio_with_retry → commit_all_bars  （见步骤 3b）
       └─ mover 是 virtio → for each pending:
              relocate_one_bar_with_retry(params, mover)   ← 步骤 4 在这里
                 └─ move_bar(...)                        ← 步骤 3a
```

### 步骤 3a：virtio — 单 BAR `move_bar`

**何时调用**：`relocate_bars` 里 `mover.uses_commit_all_bars() == false`（virtio-pci），对 pending 中**每条** BAR 调 `relocate_one_bar_with_retry` → `move_bar`。

**文件**：`src/vmm/src/vstate/vm.rs`

```rust
// relocate_bars：virtio 走逐条 retry
for params in pending {
    self.relocate_one_bar_with_retry(mover, params)?;
}

// relocate_one_bar_with_retry：先调 move_bar，Overlap 再 peel VFIO 重试
fn relocate_one_bar_with_retry(...) {
    match self.move_bar(...) {
        Ok(()) => { self.retry_all_pending_devices(); Ok(()) }
        Err(MmioBus(Overlap)) => {
            self.peel_vfio_for_overlap()?;
            self.move_bar(...)?;  // 重试
            self.retry_all_pending_devices();
            Ok(())
        }
        Err(e) => Err(e),
    }
}

// move_bar：mmio_bus 搬迁 + 设备记账 + ioevent
impl DeviceRelocation for Vm {
    fn move_bar(&self, old_base, new_base, len, pci_dev, _region_type) -> Result<...> {
        self.common.mmio_bus.update_range(old_base, len, new_base, len)?;
        pci_dev.move_bar(old_base, new_base)?;
        pci_dev.on_bar_moved(self, old_base, new_base)?;
        Ok(())
    }
}
```

**文件**：`src/vmm/src/vstate/bus.rs` — `update_range` 搬迁单段映射。

---

### 步骤 3b：VFIO — 批量 `commit_all_bars`（与 virtio 不同）

**何时调用**：guest 写 **NPU（VFIO 设备）** 的 ECAM（BAR 寄存器或 Command/MSE），且 `mse_enabled()==true` 时，`write_config_register` 通过 `drain_pending_if_mse()` 返回非空 pending（或任意 BAR 写后 MSE=1）；`pci/bus.rs` 调 `relocate_bars`，因 VFIO 的 `uses_commit_all_bars() == true`，走 **`relocate_vfio_with_retry`**，**不按 pending 逐条 `move_bar`**。

**VFIO 侧 pending 如何产生**

**文件**：`src/vmm/src/devices/pci/vfio/device.rs`

```rust
// Guest 写 BAR dword → 更新 bar_reg shadow → push pending
fn write_bar_dword(&mut self, reg_idx, value) {
    let old_regs = self.bar_reg;
    self.bar_reg[idx] = ...;
    self.push_pending_from_bar_write(reg_idx, &old_regs);
}

// MSE=1 时才把 pending 交给 bus 层
fn drain_pending_if_mse(&mut self) -> Vec<BarReprogrammingParams> {
    if self.mse_enabled() { take(pending) } else { vec![] }
}
```

**relocate 入口**（`vm.rs`）：

```rust
fn relocate_bars(&self, pending, mover) {
    if mover.uses_commit_all_bars() {
        return self.relocate_vfio_with_retry(mover);  // 忽略 pending 逐条内容
    }
    // virtio 才走 for + relocate_one_bar_with_retry
}

fn relocate_vfio_with_retry(&self, mover) {
    match mover.commit_all_bars() {
        Ok(()) => { self.retry_all_pending_devices(); Ok(()) }
        Err(MmioBus(Overlap)) => {
            mover.clear_mmio_bus_registrations();
            mover.set_pending_mmio_remap(true);
            self.retry_all_pending_devices();  // 等 virtio 让路后再 commit
            Ok(())
        }
        Err(e) => Err(e),
    }
}
```

**`commit_all_bars` 做什么**（VFIO 多 BAR **原子**提交）：

```rust
fn commit_all_bars(&mut self) -> Result<(), DeviceRelocationError> {
    bus.remove_by_device(&vfio_handler);     // 摘掉 VFIO 全部 mmio 映射
    for mmap in mmio_maps { mmap.registered = None; }

    let desired = mmio_maps.iter()
        .map(|m| (bar_guest_base_from_regs(&bar_reg, m), m.pci_size));  // 从 shadow 读新址

    for (base, len) in desired {
        bus.insert(vfio_handler.clone(), base, len)?;  // 按新地址整体插回
    }
    refresh_msix_table_region();  // 见下文：按新 BAR 基址更新 MSI-X 表 guest 地址
    Ok(())
}
```

**`refresh_msix_table_region` 做什么**

MSI-X 表不在固定 MMIO 地址，而是由 capability 指定：**落在哪个 BAR（BIR）+ BAR 内偏移**。BAR 搬迁后，表的 guest 物理地址会变，VMM 必须同步更新，否则 guest 写表项会走错路径。

计算公式（`msix_table.rs`）：

```rust
guest_base = bar_reg[table_bir] 的当前 guest 地址 + cap.table_offset_in_bar
len        = num_vectors × 16
```

`refresh_msix_table_region` 在 `commit_all_bars` 末尾：

```rust
fn refresh_msix_table_region(&mut self) {
    if let Some(VfioInterruptManager::Msix(msix)) = self.irq.as_mut() {
        if let Some(region) = msix_table_region(msix.cap(), &self.bar_reg) {
            msix.update_table_region(region);  // 只更新 VMM 侧的表地址范围
        }
    }
}
```

**作用**：更新 `VfioMsixManager.table_region`，供 `read_bar` / `write_bar` 判断某次 MMIO 是否命中 MSI-X 表（`table_index_for_offset` → `contains`），并正确模拟表项读写、同步 KVM 中断路由。

**不做什么**：不重新 `enable_msix`、不重绑 VFIO eventfd；仅修正 **BAR 搬家后的表地址解码窗口**。

**NPU 实例**：attach 日志常见 `MSIX table guest=[0x4210010000..0x4210011fff]`（BAR0@`0x4210…` + 表偏移）。`commit_all_bars` 后若 BAR0 基址变了，必须 refresh，否则 NPU 中断配置写不进去。
```

**与 virtio 路径对比**

| | virtio | VFIO (NPU) |
|--|--------|------------|
| 搬迁 API | `move_bar` + `update_range`（单 BAR） | `commit_all_bars`（多 BAR 一次摘掉再插回） |
| pending 用法 | 每条 pending 各搬一次 | pending 仅作「MSE=1 触发信号」；实际地址以 `bar_reg` shadow 为准 |
| Overlap 处理 | `peel_vfio_for_overlap` 后重试 `move_bar` | `clear_mmio_bus_registrations`，等 virtio peel 后 `retry_all_pending_devices` → 再 `commit_all_bars` |
| 副作用 | `on_bar_moved` 迁 ioevent | 无 ioevent；刷新 MSIX table 映射 |

**NPU guest-final attach 后的典型情况**

- Attach 时 BAR 已在 `0x4000/0x4200/0x4210…`，guest assign 若不再改 NPU BAR，`old_base == new_base`，pending 为空或 `commit_all_bars` 等价于按同址重建映射。
- 若 guest 仍写 NPU BAR（例如 resize 探测），MSE=1 时会触发 `commit_all_bars`，保证 **BAR0/2/4 同时** 与 ECAM shadow 对齐，避免逐 BAR `update_range` 产生中间态 overlap。

---

### 步骤 4：virtio Overlap 时摘掉 VFIO 再重试（何时触发）

**何时调用**：guest 写 ECAM 使 MSE=1 后，`pci/bus.rs` → `relocate_bars` → **`relocate_one_bar_with_retry`**（仅 virtio，每条 pending BAR 一次）。

**触发条件**：`move_bar` 里 `mmio_bus.update_range` 返回 **`BusError::Overlap`**——virtio 要搬到的新地址仍被 VFIO attach 态 MMIO 占用。

**典型场景**：VFIO 仍用旧 attach 布局（如 BAR4 盖住 `0x4200…–0x43ff…`），guest assign 要把 virtio 改到 `0x4211…`，第一次 `move_bar` 撞上 VFIO 映射 → `peel_vfio_for_overlap()` 摘掉 VFIO → 再 `move_bar` → `retry_all_pending_devices()` 让 VFIO `commit_all_bars`。

**当前 NPU guest-final attach 后**：VFIO 已在 `0x4000/0x4200/0x4210…`，virtio 搬到 `0x4211…` 往往 **不再 Overlap**，此路径可能不走 peel；属于早期调试阶段的兜底逻辑。

### 步骤 5：基础设施（PR-1a）

| 模块 | 文件 | 职责 |
|------|------|------|
| Bus | `vstate/bus.rs` | `update_range`、`remove_by_device` |
| PCI trait | `pci/mod.rs` | `BarReprogrammingParams`、`DeviceRelocation`、`on_bar_moved` |
| Virtio | `virtio/transport/pci/device.rs` | `mmio_bus_device()`、`move_bar` 更新 `bar_address` |
| VFIO | `pci/vfio/device.rs` | `pending_bar_reprogram`、`commit_all_bars`、去掉 eager `sync_mmio_maps` |

---

## 第 4 页：`pci_bus_assign_resources` 内核 Stall

### 现象（`rcu.txt`）

Guest dmesg 在 NPU BAR4 assign 后停止推进：

```
[    0.227869][    T1] pci 0000:00:01.0: BAR 4: assigned [mem 0x4000000000-0x41ffffffff 64bit pref]
（此后长时间无新日志，或伴随 RCU / hung_task）
```

此前枚举：

```
pci 0000:00:01.0: reg 0x20: [mem 0x4000000000-0x41ffffffff 64bit pref]   # NPU BAR4 规划
pci 0000:00:02.0: reg 0x10: [mem 0x4400000000-0x440007ffff 64bit]         # virtio-blk
```

### 原因简述

1. **VFIO attach 布局与 guest 规划不一致（`rcu.txt` 实测）**  
   早期 attach 时 VMM 日志为：
   - BAR0（region 0）：`0x4000000000` – `0x4000ffffff`（16MB）
   - BAR4（region 4）：`0x4200000000` – `0x43ffffffff`（8GB）  
   Guest 枚举后规划为 BAR4@`0x4000…`+8GB、virtio@`0x4400…`。  
   VFIO BAR0 占住 `0x4000…` 起 16MB，与 guest 拟分配的 8GB BAR4 **同起点冲突**；同时 ECAM shadow 与 guest 终态（BAR4@`0x4000`、BAR2@`0x4200`、BAR0@`0x4210`）不一致，assign 阶段长时间卡住。

2. **Guest 视角的资源冲突**  
   `pci_bus_assign_resources` 按 BAR size 从低到高分配；8GB pref BAR + 邻近 virtio BAR 在 **同一 MMIO 窗口** 内产生冲突，内核 PCI 核心在 assign / resize 路径上长时间持锁或重试，表现为 **stall**（日志停在 `BAR 4: assigned` 之后）。

3. **与 VMM `mmio_bus` 无关**  
   此阶段 stall 发生在 **guest 内核** PCI 子系统，尚未到 virtio 驱动 I/O；根因是 **ECAM 中各设备 BAR 规划区间在 assign 时互相重叠**。

---

## 第 5 页：解决办法 — 避免 assign 阶段看到重叠

### 设计思路

**让 guest 在 `pci_bus_assign_resources` 时，不要把 virtio 和 NPU 8GB 规划区放在同一重叠窗口。**

**Guest 终态目标**（assign 完成后）：

```
virtio-blk:  0x4211000000
virtio-net:  0x4211080000
```

**VMM 当前实现（两段式，非直接 attach 到 0x4211）**：

- **Attach**：blk@`0x4400000000`、net@`0x4400080000`（避开 VFIO 旧 attach BAR4 窗口，保证 `mmio_bus.insert` 不 Overlap）
- **Assign 后**：guest 将 virtio 改到 `0x4211…`，VMM 通过 `move_bar` + ioevent 迁移对齐终态

### 关键源码：virtio 固定 BAR 地址

**文件**：`src/vmm/src/device_manager/pci_mngr.rs`

存在 fixed VFIO 时，为 virtio 分配固定高地址（当前实现为 attach 基址 `0x4400000000`，guest assign 后再 `move_bar` 到 `0x4211…`）：

```rust
let fixed_bar_addr = self.fixed_vfio_device.as_ref().map(|_| {
    VirtioPciDevice::vfio_coexist_bar_address(
        u32::try_from(self.virtio_devices.len()).unwrap_or(u32::MAX),
    )
});
virtio_device.allocate_bars(&mut resource_allocator.mmio64_memory, fixed_bar_addr);
```

**文件**：`src/vmm/src/devices/virtio/transport/pci/device.rs`

```rust
/// Virtio BAR attach base when coexisting with fixed VFIO.
pub const VFIO_COEXIST_VIRTIO_BAR_BASE: u64 = 0x4400_0000_00;

pub fn vfio_coexist_bar_address(index: u32) -> u64 {
    VFIO_COEXIST_VIRTIO_BAR_BASE + u64::from(index) * CAPABILITY_BAR_SIZE
}
// index=0 → blk @ 0x4400000000
// index=1 → net @ 0x4400080000
```

**说明**：若 attach 直接使用 `0x4211000000`，在 VFIO 仍使用旧 attach BAR4（`0x4200000000` + 8GB）时，VMM `mmio_bus.insert` 会 **Overlap** 导致实例无法启动；因此采用 **attach@0x44… + assign 后 move_bar@0x4211…** 的两段式策略。

---

## 第 6 页：改后仍遇 Stall — VFIO Guest-Final 布局

### 现象

virtio 抬高到 `0x4400000000` 后，VMM attach 成功，但 guest 仍可能在 NPU probe / assign 附近长时间卡住；或 NPU ECAM BAR 与规划地址不一致。

### 根因

VFIO **attach 态** BAR 布局必须一次性对齐 **guest 终态**，否则 guest assign NPU BAR4@`0x4000…` 时，VMM 侧 MMIO 仍映射在错误区间，内核 NPU 驱动 probe 极慢或异常。

### 最终 VFIO 分配方案

**文件**：`src/vmm/src/devices/pci/vfio/device.rs` — `allocate_bars_and_register_mmio`（**VMM attach，只执行一次**）

**地址常量**

```rust
const NPU_GUEST_FINAL_BAR4_BASE: u64 = 0x4000_0000_00;  // 8GB  @ slot 4
const NPU_GUEST_FINAL_BAR2_BASE: u64 = 0x4200_0000_00;  // 256MB @ slot 2
const NPU_GUEST_FINAL_BAR0_BASE: u64 = 0x4210_0000_00;  // 16MB  @ slot 0

fn npu_guest_final_region_size() -> u64 {
    NPU_GUEST_FINAL_BAR0_BASE + NPU_BAR0_SIZE - NPU_GUEST_FINAL_BAR4_BASE
    // 0x4210ffffff - 0x4000000000 + 1 = 0x211000000
}
```

#### 阶段 A：attach（`allocate_bars_and_register_mmio`）

| 步骤 | 要点 | 源码行 |
|------|------|--------|
| A1 | `is_npu_bar_topology()` | L579 |
| A2 | 一次性 `mmio64.allocate(region_size, ExactMatch(BAR4_BASE))` | L580–594 |
| A3 | `npu_guest_final_bar_base(slot, size)` 取固定地址（**不再** per-BAR allocate） | L613–614 |
| A4 | `bar_plans.sort_by_key(base)` | L637–638 |
| A5 | `program_bar_shadow` + `mmio_bus.insert` + `mmap.registered = …` | L640–666 |
| A6 | `refresh_msix_table_region()` | L668 |

```rust
let npu_topology = is_npu_bar_topology(&self.parsed_bars);

if npu_topology {
    mmio64.allocate(npu_guest_final_region_size(), 4096,
        AllocPolicy::ExactMatch(NPU_GUEST_FINAL_BAR4_BASE))?;
}

let mut bar_plans = Vec::new();
for pb in self.parsed_bars.iter() {
    let guest_base = if npu_topology {
        npu_guest_final_bar_base(first_slot, pci_size).expect(...)
        // slot4→0x4000…, slot2→0x4200…, slot0→0x4210…
    } else { /* FirstMatch / 单 BAR ExactMatch */ };
    bar_plans.push((first_slot, guest_base, pci_size, vfio_region));
}
bar_plans.sort_by_key(|(_, base, _, _)| *base);

for (first_slot, guest_base, pci_size, vfio_region) in bar_plans {
    program_bar_shadow(&mut self.bar_reg, first_slot, guest_base);
    vm.common.mmio_bus.insert(self_arc.clone(), guest_base, pci_size)?;
    mmap.registered = Some((guest_base, pci_size));
}
self.refresh_msix_table_region();
```

**踩坑**：常量误写 `0x4210_0000_0000` → `0x421000000000`，`ExactMatch` 失败；正确为 `0x4210_0000_00`。

#### 阶段 B：运行时搬迁（guest 写 ECAM 之后，**不是 attach**）

| 步骤 | 要点 |
|------|------|
| B1 | 去掉 eager `sync_mmio_maps`（旧代码已删） |
| B2 | Guest 写 VFIO BAR + MSE=1 → `drain_pending_if_mse()` |
| B3 | `relocate_vfio_with_retry` → **`commit_all_bars`**（摘掉全部 VFIO 映射，按 `bar_reg` 整体插回） |

NPU guest-final attach 后，若 guest **不再改** NPU BAR 地址，阶段 B 可能不触发；阶段 A 已把 ECAM 与 mmio_bus 对齐。

### 改后 guest 枚举（正常）

```
pci 0000:00:01.0: reg 0x10: [mem 0x4210000000-0x4210ffffff 64bit]
pci 0000:00:01.0: reg 0x18: [mem 0x4200000000-0x420fffffff 64bit pref]
pci 0000:00:01.0: reg 0x20: [mem 0x4000000000-0x41ffffffff 64bit pref]
pci 0000:00:01.0: BAR 4/2/0: assigned（与规划一致）
pci 0000:00:02.0: BAR 0: assigned [mem 0x4211000000-...]
```

`pci_bus_assign_resources` **通过**（NPU probe 仍可能耗时数分钟，属驱动行为）。

---

## 第 7 页：`unexpected write to notification BAR` 与 ioevent 迁移

### 现象

```
pci: unexpected write to notification BAR. Offset 0x6000
task swapper/0 blocked ... virtblk_probe → read_part_sector → wait_on_page_read
```

Guest 已将 virtio BAR 从 `0x4400000000` assign 到 `0x4211000000`，`virtio_blk` 能枚举容量，但读 GPT 时 hung_task。

### 原因

| 机制 | BAR 迁移后状态 |
|------|----------------|
| `mmio_bus.update_range` | ✅ 已搬到 `0x4211…` |
| KVM **ioeventfd** | ❌ 仍监听 attach 时地址 `0x4400060000` |

**ioeventfd 作用**：guest 写 virtio notify 区（BAR + `0x6000`）时，KVM 在硬件路径直接唤醒 VMM 的 `EventFd`，触发 virtqueue 处理；**不经过** `write_bar`。

BAR 搬迁后 guest 写 `0x4211060000`，KVM 未拦截 → 落入 `write_bar` 仅打 warn → 队列永不推进。

### 解决方案：move_bar 后迁移 ioevent

**文件**：`src/vmm/src/devices/virtio/transport/pci/device.rs`

```rust
fn update_notification_ioevents(&self, vm, new_bar_addr, old_bar_addr: Option<u64>) {
    for (i, queue_evt) in self.device.lock().unwrap().queue_events().iter().enumerate() {
        if let Some(old_bar) = old_bar_addr {
            let old_io = IoEventAddress::Mmio(Self::notification_io_addr(old_bar, i));
            vm.fd().unregister_ioevent(queue_evt, &old_io, NoDatamatch)?;
        }
        let new_io = IoEventAddress::Mmio(Self::notification_io_addr(new_bar_addr, i));
        vm.fd().register_ioevent(queue_evt, &new_io, NoDatamatch)?;
    }
}
```

**文件**：`src/vmm/src/vstate/vm.rs` — `move_bar` 末尾调用 `pci_dev.on_bar_moved()`，virtio 实现中调用 `reregister_notification_ioevent`。

### 修复后预期

- VMM 日志：`Reregistered virtio notification ioevent after BAR move 0x4400000000->0x4211000000`
- 不再出现 `unexpected write to notification BAR`
- `VFS: Mounted root`，可登录 shell

---

## 第 8 页：目前结果与后续跟踪

### 已达成

| 项目 | 状态 |
|------|------|
| VMM attach（VFIO NPU guest-final + virtio@0x44…） | ✅ |
| Guest `pci_bus_assign_resources` | ✅ 通过 |
| virtio BAR `move_bar`（0x44… → 0x4211…） | ✅ |
| ioeventfd 随 BAR 迁移 | ✅（已实现，修复 hung_task） |
| Guest 启动、rootfs 挂载、`lspci` 可见设备 | ✅（ioevent 修复后可登录；NPU 用户态工具仍待驱动） |

**`lspci` 示例**

```
00:01.0 Processing accelerators [1200]: Huawei Technologies Co., Ltd. Device [19e5:d500]
00:02.0 Mass storage controller [0108]: Red Hat, Inc. Virtio block device [1af4:1042]
00:03.0 Ethernet controller [0200]: Red Hat, Inc. Virtio network device [1af4:1041]
```

NPU BAR 地址与规划一致（`0x4000… / 0x4200… / 0x4210…`）。

### 待解决问题：NPU 驱动未加载

Guest 内：

```
npu-smi info
DrvMngGetConsoleLogLevel failed. (ret=4)
dcmi module initialize failed. ret is -8005

lsmod | grep -E 'drv|ascend|davinci'   # 空
```

**原因（与 Firecracker BAR 无关）**

- Firecracker 使用自编译内核 `vmlinux.bin`（`5.10.0+`）。
- Ascend 驱动 DKMS 编译目标为 `5.10.0-60.18.0.50.oe2203.aarch64`。
- 模块装在 `/lib/modules/5.10.0-60.../`，运行内核从 `/lib/modules/5.10.0+/` 加载 → **modprobe 找不到模块**。
- 安装日志亦记录：`The kernel version of uname -a and uname -r is inconsistent.`

### 后续跟踪方向

1. **内核对齐**：为 `vmlinux.bin`（`5.10.0+`）重装 Ascend driver，或改用与 DKMS 一致的 openEuler 内核启动 VM。
2. **驱动 probe**：`dmesg | grep -iE ascend|davinci` 确认 VFIO 透传后 probe 是否成功。
3. **Host 释放**：确认 host 上 `0000:01:00.0` 由 `vfio-pci` 绑定，非 host NPU 驱动占用。

---

## 附录：关键文件索引

| 路径 | 内容 |
|------|------|
| `docs/pci-bar-move-implementation-zh.md` | 设计文档 v2.3 |
| `src/vmm/src/pci/configuration.rs` | BAR detect + MSE gating |
| `src/vmm/src/pci/bus.rs` | ECAM → relocate_bars |
| `src/vmm/src/vstate/vm.rs` | move_bar、overlap peel、retry |
| `src/vmm/src/vstate/bus.rs` | mmio_bus update_range |
| `src/vmm/src/devices/pci/vfio/device.rs` | NPU guest-final attach |
| `src/vmm/src/devices/virtio/transport/pci/device.rs` | virtio BAR、ioevent 迁移 |
| `src/vmm/src/device_manager/pci_mngr.rs` | virtio 固定 BAR 分配 |

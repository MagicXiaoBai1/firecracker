<!--
Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Ascend VFIO：Quirk 生效后 ENOSYS/NISV 问题排查与修改总结

> **用途**：制作 PPT / 技术汇报。  
> **环境**：Firecracker aarch64 + Ascend 310P VFIO passthrough（`19e5:d500`，host `0000:01:00.0`）+ virtio net/blk。  
> **对照**：同一 rootfs 在 QEMU/virsh 下可正常 probe NPU。

**相关文档**

| 文档 | 内容 |
|------|------|
| [ascend-vfio-quirks-design-zh.md](./ascend-vfio-quirks-design-zh.md) | Ascend BAR quirk 设计 |
| [ascend-vfio-quirks-qemu-summary-zh.md](./ascend-vfio-quirks-qemu-summary-zh.md) | QEMU 原版 quirk 行为 |
| [pci-bar-move-implementation-zh.md](./pci-bar-move-implementation-zh.md) | NPU BAR 固定布局 |
| [vfio-msix-implementation-zh.md](./vfio-msix-implementation-zh.md) | MSI-X 拦截 |

---

## 1. 背景：我们在做什么

### 1.1 目标

在 **Firecracker（FC）** 上实现 **Ascend NPU VFIO 直通**，guest 使用与 QEMU 相同的 rootfs 和 davinci 驱动，完成设备 probe 与加载。

### 1.2 两条独立工作线

| 工作线 | 目的 | 能否解决 ENOSYS/NISV |
|--------|------|----------------------|
| **Ascend BAR Quirk** | 对齐 QEMU `pci-quirks.c`：`BAR2+0x100430` 读走 VFIO、写丢弃 | **不能**（设计文档已声明为独立问题） |
| **KVM ENOSYS / NISV** | guest 用 KVM 无法 decode 的访存指令访问设备 MMIO | **需要** 单独处理 |

Quirk 解决的是 **特定寄存器语义**；ENOSYS/NISV 解决的是 **KVM 与 VMM 的 MMIO 交付路径**。

### 1.3 NPU Guest BAR 布局（fixed-vfio）

| PCI BAR | VFIO region | Guest GPA | 大小 |
|---------|-------------|-----------|------|
| BAR4 | 4 | `0x4000000000` | 8 GiB |
| BAR2 | 2 | `0x4200000000` | 256 MiB |
| BAR0 | 0 | `0x4210000000` | 16 MiB |

常量定义：`src/vmm/src/devices/pci/vfio/device.rs`（`NPU_GUEST_FINAL_BAR*`）。

---

## 2. 关键转折点：「Quirk 日志已出现，但 ENOSYS/NISV 仍在发生」

### 2.1 这说明什么

- **Quirk 已生效**：日志出现 `ascend quirk write dropped` / `ascend quirk read`，说明 `0x100430` 窗口行为与 QEMU 一致。
- **崩溃原因不是 quirk 没装上**，而是 **quirk 之后** guest 继续 probe 时触发了 **KVM 无法交给 userspace 做 MMIO 的访存**。

### 2.2 关键时间线（日志证据）

#### 阶段 A — Quirk 正常 + 首次 ENOSYS（`quirk_log.txt`，2026-07-04 19:26:41）

```text
… MSIX vec=2 配置、BAR2 大量 4-byte read …
→ ascend quirk write dropped @ BAR2+0x100430
→ BAR2 read @ 0x13c004/008/00c …
→ ascend quirk read @ BAR2+0x100430 val=0
→ [约 11ms 无 MMIO 日志]
→ ENOSYS: KVM failed to emulate an instruction
→ VM exit
```

**结论**：quirk read 成功后，下一条 fault **未走** `KVM_EXIT_MMIO`（无 `BAR MMIO read/write` 日志），FC 收到 **`-ENOSYS`**（当时尚未 enable NISV cap，无 `fault_ipa`）。

#### 阶段 B — 启用 NISV 诊断后（2026-07-04 20:00 / 20:18）

```text
… 同样 quirk read @ 0x100430 …
→ KVM_EXIT_ARM_NISV: fault_ipa=0x4200140008 esr_iss=0x45
→ fault_ipa hint: VFIO NPU BAR2 GPA   （hint 修正后）
→ ARM_NISV: KVM could not decode the faulting instruction
→ VM exit（修改 NISV handler 前）
```

**字段说明：`fault_ipa` 与 `esr_iss` 是什么？**

| 字段 | 来源 | 含义 |
|------|------|------|
| **`fault_ipa`** | `kvm_run.arm_nisv.fault_ipa`（KVM 在 NISV exit 时填入） | **Fault 的 IPA（Intermediate Physical Address）** |
| **`esr_iss`** | `kvm_run.arm_nisv.esr_iss` | ESR 中 **少量保留位**（FSC + WNR + CM），**不是**完整 instruction syndrome |

**`fault_ipa` 是 guest 虚拟地址吗？—— 不是。**

在 KVM 两阶段地址翻译模型里，有三层地址，不要和 ENOSYS 日志里的 `FAR_EL1` 混淆：

```text
Guest 虚拟地址 (VA)          ← 例如 FAR_EL1=0xffffbae6cec0（常误导，勿用来对 BAR）
        │  Guest 页表 (EL1)
        ▼
Guest 物理地址 (GPA)         ← 即 IPA / fault_ipa=0x4200140008  ★ 看这里
        │  Stage-2 页表 (KVM)
        ▼
Host 物理地址 (HPA)          ← VFIO/MMIO 模拟在 FC 里处理，一般不直接暴露给诊断
```

- **`fault_ipa` = Guest 物理地址（GPA）**，也就是 guest 眼里 PCI BAR 映射到的 **物理/MMIO 地址**。
- Ascend 驱动 `ioremap` 设备寄存器后，CPU 访存最终落到 **GPA**；KVM 在 stage-2 fault 时报告的就是这个 GPA。
- **`FAR_EL1`** 是 fault 瞬间 EL1 的 **虚拟地址**，与 stage-2 fault 的 IPA **经常不一致**，因此 ENOSYS 早期日志里用 FAR 对 BAR 会判错。

本例换算：

```text
fault_ipa   = 0x4200140008
BAR2 base   = 0x4200000000   （guest 物理地址，mmio_bus 注册基址）
BAR 内偏移  = 0x140008       （约 1.25 MiB 处）
```

与 FC 日志 `base=0x4200000000 offset=0x100488` 是同一套坐标系（都是 **GPA = base + offset**）。

**`esr_iss=0x45` 能读出什么？**

NISV 时 KVM **故意不**提供完整 ESR（ISV=0），只通过 `kvm_vcpu_dabt_iss_nisv_sanitized()` 保留：

```text
esr_iss = ESR 的 (CM | WNR | FSC) 位
        = 0x45
        = 0b0100_0101
```

| 位域 | 本例 | 含义 |
|------|------|------|
| **FSC**（bits 0–5） | `0x05` | Fault status（stage-2 翻译/外部 abort 类编码） |
| **WNR**（bit 6） | `1` | **Write** Not Read → 这是一次 **写** 访问 |
| **CM**（bit 8） | `0` | 非 cache maintenance 类访问 |

**读不出的信息**（因此 FC 无法像 `KVM_EXIT_MMIO` 那样直接 emulate）：

- 访问宽度（1/2/4/8 字节？）
- 目标/源寄存器编号
- 是否为 `STP`/`LDP` 等复合指令

**结论**：

- `fault_ipa = 0x4200140008` = **BAR2 GPA + offset `0x140008`**，**在已注册 mmio_bus 范围内**。
- `esr_iss=0x45` → 已知是 **write**，但 **宽度与寄存器未知** → KVM 只能给 NISV，不能给 MMIO exit。
- 问题 **不是 BAR 未注册**，而是 **KVM NISV 路径 FC 当时未处理**（与 guest VA 无关）。

> **与阶段 A 的关系**：阶段 A 的 ENOSYS 与阶段 B 的 NISV 通常是 **同一类 ISV=0 fault**；enable `KVM_CAP_ARM_NISV_TO_USER` 后，KVM **改报** NISV 并带上 `fault_ipa`，并非 guest 突然换了故障类型。见 §3.2。

#### 阶段 C — 方案 1 实现后（预期行为）

```text
→ KVM_EXIT_ARM_NISV: fault_ipa=0x4200140008 …
→ KVM_EXIT_ARM_NISV: injected ext_dabt at fault_ipa=…; continuing guest
→ VM **不退出**，由 guest 内核/驱动处理 external data abort
```

（需重新编译并跑完整 probe 以确认驱动是否最终加载成功。）

---

## 3. 背景知识（PPT 可单独成页）

### 3.1 Guest 访问设备 MMIO 的两条 KVM 路径

```text
Guest 访问设备 GPA（不在 RAM memslot）
        │
        ▼
   stage-2 Data Abort
        │
        ├─ ESR.ISV=1（syndrome 完整）
        │     → KVM_EXIT_MMIO
        │     → FC mmio_bus → read_bar / write_bar → VFIO region_read/write
        │
        └─ ESR.ISV=0（syndrome 不完整，NISV）
              → KVM_EXIT_ARM_NISV（enable cap 177 后）
              或 -ENOSYS（未 enable cap 时）
              → FC 必须自行处理，否则 VM 退出
```

**常见触发 NISV 的指令**：`STP`/`LDP`、post-index `LDR`、部分 SIMD/atomic 等。

### 3.2 ENOSYS vs NISV：不是两种故障，是两种上报

同一次 **ISV=0** 的 stage-2 data abort（KVM 无法 syndromize 的 MMIO 访问）：

| FC / KVM 配置 | `KVM_RUN` 返回 | 能否看到 `fault_ipa` |
|---------------|----------------|----------------------|
| 未 enable `KVM_CAP_ARM_NISV_TO_USER` | **`-ENOSYS`** | 否（阶段 A，`quirk_log.txt` 19:26） |
| 已 enable 该 cap | **`KVM_EXIT_ARM_NISV`** | 是（阶段 B，20:18） |

因此时间线上从 ENOSYS「跳到」NISV，是因为 **FC 启用了 NISV cap 与诊断**，不是 guest 或 BAR 又出现了新类型错误。

### 3.3 FAR_EL1 不能当 fault GPA

`FAR_EL1` 是 guest **虚拟地址**，与 stage-2 fault 的 **IPA** 常不一致。诊断应优先用 **NISV 的 `fault_ipa`** 或 EL2 **HPFAR/FAR**。

### 3.4 mmio_bus 注册 ≠ KVM memslot

VFIO BAR 在 FC 中注册在 **userspace `mmio_bus`**，**不在** guest RAM memslot。  
每次访问依赖 **KVM exit → mmio_bus**；若 KVM 给出 NISV 而非 MMIO exit，**read_bar 不会被调用**。

### 3.5 QEMU 对 NISV 的做法（方案 1 对齐对象）

QEMU `target/arm/kvm.c` 中 **`kvm_arm_handle_dabt_nisv`**：

- **不做** 指令 decode + MMIO emulate；
- 有 `KVM_CAP_ARM_INJECT_EXT_DABT` 时：`KVM_SET_VCPU_EVENTS` 设 `ext_dabt_pending=1`，让 KVM 向 guest **注入 external data abort**，然后继续运行。

---

## 4. Quirk 生效后 guest 在做什么（崩溃前最后几步）

| 顺序 | 日志 | 含义 |
|------|------|------|
| 1 | MSIX vec=2 / vec=16 配置 | 中断路由正常 |
| 2 | BAR2 read `0x100488` 等 | 常规 MMIO 经 KVM_EXIT_MMIO 正常 |
| 3 | `ascend quirk write dropped @ 0x100430` | Quirk 写拦截正常 |
| 4 | BAR2 read `0x13c004`…`0x13c00c` | probe 继续读其他寄存器 |
| 5 | `ascend quirk read @ 0x100430` | Quirk 读正常 |
| 6 | **无 MMIO 日志** → NISV/ENOSYS | guest 用 **复杂 store** 访问 **BAR2+0x140008**，KVM 无法 decode |

**关键结论**：Quirk 与 MSIX 均已工作；**新 crash 点是 BAR2 内另一偏移 `0x140008` 的 NISV write**。

---

## 5. 修改清单（源码路径 + 作用）

### 5.1 已实现：Ascend BAR Quirk（转折点之前）

| 路径 | 修改内容 | 作用 |
|------|----------|------|
| `src/vmm/src/devices/pci/vfio/ascend_quirks.rs` | **新增** Quirk 探测与 read/write 逻辑 | 310P：`region=2, offset=0x100430`；2P 加第二窗口 +128MiB |
| `src/vmm/src/devices/pci/vfio/device.rs` | `read_bar` / `write_bar` 中调用 quirk | MSIX → quirk → `region_read/write` |
| `src/vmm/src/devices/pci/vfio/mod.rs` | 导出 `ascend_quirks` 模块 | 模块接线 |

**验证日志**：

```text
fixed-vfio: Ascend BAR quirks installed vendor=0x19e5 device=0xd500 subsystem=0x0110 …
fixed-vfio: ascend quirk write dropped … offset=0x100430 …
fixed-vfio: ascend quirk read … offset=0x100430 …
```

### 5.2 转折点之后：诊断 ENOSYS / NISV

| 路径 | 修改内容 | 作用 |
|------|----------|------|
| `src/vmm/src/arch/aarch64/enosys_log.rs` | **新增** ENOSYS/NISV 诊断 | 读 EL2 寄存器；按 `fault_ipa` 分类 BAR2/BAR4 等；修正 BAR 边界常量算法 |
| `src/vmm/src/arch/aarch64/kvm.rs` | `try_enable_arm_nisv_caps()` | VM 创建时 enable `KVM_CAP_ARM_NISV_TO_USER`；探测 `KVM_CAP_ARM_INJECT_EXT_DABT` |
| `src/vmm/src/vstate/vm.rs` | 调用 `try_enable_arm_nisv_caps`；保存 `arm_inject_ext_dabt` | VM 级 cap 状态 |
| `src/vmm/src/arch/aarch64/vcpu.rs` | `log_enosys_guest_state`、`handle_arm_nisv_exit` 等 | NISV 日志与 ext_dabt 注入 |
| `src/vmm/src/vstate/vcpu.rs` | NISV exit 分支；ENOSYS 时打诊断 | 不再把 NISV 一律当 Unknown；连接 handler |
| `src/vmm/src/lib.rs` | `set_arm_inject_ext_dabt` 传入 vCPU | 启动 vCPU 时传递 cap 能力 |

**诊断带来的结果**：

| 之前 | 之后 |
|------|------|
| `-ENOSYS`，只有 `FAR_EL1`（guest VA，易误判） | `KVM_EXIT_ARM_NISV` + **`fault_ipa=0x4200140008`** |
| 怀疑 BAR 未注册 | 确认 **BAR2 内 offset `0x140008`**，注册无误 |
| hint 误报 BAR4（hex 常量 typo） | 修正为 **BAR2 GPA** hint |

### 5.3 转折点之后：方案 1 — QEMU 对齐 ext_dabt 注入

| 路径 | 修改内容 | 作用 |
|------|----------|------|
| `src/vmm/src/arch/aarch64/vcpu.rs` | `handle_arm_nisv_exit()` | NISV 时 `ext_dabt_pending=1`，**继续 guest**，不 fatal exit |
| `src/vmm/src/arch/aarch64/kvm.rs` | `ArmNisvCaps` | 记录 host 是否支持 inject |
| `src/vmm/src/vstate/vcpu.rs` | NISV → `handle_arm_nisv_exit` | 接线 |
| `src/vmm/src/lib.rs` | vCPU 设置 `arm_inject_ext_dabt` | 运行时启用 |

**预期日志**：

```text
KVM_CAP_ARM_INJECT_EXT_DABT available for NISV external abort injection
KVM_EXIT_ARM_NISV: injected ext_dabt at fault_ipa=0x4200140008; continuing guest
```

**方案 1 不做什么**：不 decode 指令、不替 guest 完成 MMIO write；只把 abort **交还 guest**（与 QEMU 相同）。

---

## 6. 问题根因总结（一页 PPT）

```text
┌─────────────────────────────────────────────────────────────┐
│  Quirk / MSIX / BAR mmio_bus 注册  →  均已验证正常           │
├─────────────────────────────────────────────────────────────┤
│  崩溃根因：KVM_EXIT_ARM_NISV（非 quirk 失败）                │
│  • guest 在 BAR2+0x140008 发起 KVM 无法 syndromize 的 write  │
│  • FC 未处理 NISV → VM 退出（方案 1 前）                     │
├─────────────────────────────────────────────────────────────┤
│  方案 1（已实现）：inject ext_dabt，VM 继续，对齐 QEMU       │
│  方案 3（已实现）：VFIO BAR mmap + quirk/MSIX 页打洞         │
│  方案 2（待定）：指令 decode + mmio_bus 真正完成 MMIO        │
│          （mmap 后若空洞外仍 NISV 再做）                      │
└─────────────────────────────────────────────────────────────┘
```

---

## 7. 各阶段结果对照表

| 阶段 | Quirk | 崩溃类型 | fault 位置 | FC 行为 | VM 结果 |
|------|-------|----------|------------|---------|---------|
| Quirk 前 | 无 | ENOSYS/NISV | 不明确 | 直接退出 | 失败 |
| Quirk 后（19:26） | ✅ | ENOSYS | 无 fault_ipa | 直接退出 | 失败 |
| +NISV cap（20:18） | ✅ | NISV | BAR2+`0x140008` | 打日志后退出 | 失败 |
| +方案 1 ext_dabt | ✅ | NISV | 同上 | inject 后继续 | guest SEA panic |
| +方案 3 BAR mmap | ✅ | （期望消失） | — | memslot 直达 | **已验收**（probe + `npu-smi`） |

---

## 8. 尚未完成 / 后续工作

| 项 | 说明 | 优先级 |
|----|------|--------|
| ~~验收方案 3 / BAR4 mmap~~ | `MAX_MMAP_BAR_SIZE=8GiB`，BAR0/2/4 mmap 已落地；`npu-smi` OK | 已完成 |
| **SMBIOS Type 9** | FC 无 DMI 时 DCMI 卡枚举靠 guest `board_cfg.ini`；对齐 QEMU card=2048 需 VMM 补 Type 9 | P2 |
| **方案 2** | 仅当 mmap 后仍有空洞外 NISV | P3 |

---

## 9. PPT 建议结构（10–12 页）

1. **背景**：FC + Ascend VFIO 直通目标；QEMU 可跑、FC 不可跑  
2. **架构**：Guest BAR 布局 + mmio_bus + KVM exit 路径  
3. **工作线 1**：Ascend Quirk 是什么、改了哪些文件、日志如何证明生效  
4. **转折点**：Quirk 有了仍崩溃 — 时间线最后 20 行日志 walkthrough  
5. **背景知识**：MMIO exit vs NISV；ISV=0 的含义  
6. **诊断修改**：enosys_log + NISV cap；`fault_ipa` 定位到 BAR2+0x140008  
7. **根因**：不是 BAR 未注册，是 NISV 未处理  
8. **方案 1**：ext_dabt 注入原理、QEMU 对齐、改了哪些文件  
9. **结果对照表**（§7）  
10. **下一步**：方案 2/3、验证计划  

---

## 10. 关键日志摘录（可直接贴 PPT）

**Quirk 生效：**

```text
fixed-vfio: ascend quirk write dropped … offset=0x100430 …
fixed-vfio: ascend quirk read … offset=0x100430 len=4 val=0x00000000
```

**NISV 根因（20:18）：**

```text
KVM_EXIT_ARM_NISV: fault_ipa=0x4200140008 esr_iss=0x45
fault_ipa hint: VFIO NPU BAR2 GPA
ARM_NISV: KVM could not decode the faulting instruction; userspace must emulate or abort the guest
```

**方案 1 预期：**

```text
KVM_EXIT_ARM_NISV: injected ext_dabt at fault_ipa=0x4200140008; continuing guest
```

---

## 11. 构建与测试

```bash
cd firecracker
cargo build --release -p firecracker
cargo test -p vmm ascend_quirks enosys_log npu_bar4_end --target aarch64-unknown-linux-gnu
```

启动脚本：`firecracker/start_fc_socket.sh`、`/home/zjj/modify_firecracker/start_fc_vm.sh`  
参考日志：`/home/zjj/modify_firecracker/quirk_log.txt`

---

*文档版本：2026-07-05，基于 2026-07-04 实验日志与 FC 源码状态整理。*

<!--
Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Ascend VFIO：QEMU NISV 处理与 Firecracker 对齐实现

> **用途**：技术汇报 / 与 QEMU 行为对照。  
> **环境**：aarch64 KVM + Ascend 310P VFIO passthrough。  
> **QEMU 源码路径**：仓库根目录 `qemu/`（本文引用 `qemu/target/arm/kvm.c`、`qemu/hw/vfio/region.c` 等）。

**相关文档**

| 文档 | 内容 |
|------|------|
| [ascend-vfio-quirk-enosys-nisv-journey-zh.md](./ascend-vfio-quirk-enosys-nisv-journey-zh.md) | 问题排查时间线与日志解读 |
| [ascend-vfio-quirks-design-zh.md](./ascend-vfio-quirks-design-zh.md) | Ascend BAR quirk（与 NISV 独立） |

---

## 1. 背景知识（读实现前先理解）

本节解释 **NISV**、**external data abort（外部数据中止）**、**inject ext_dabt** 分别是什么，以及它们与「正常 MMIO emulate」的区别。无 ARM/KVM 背景的读者建议先读本节。

### 1.1 Guest 访问设备 MMIO 时发生了什么

Guest 驱动通过 **GPA（Guest Physical Address）** 访问 PCI BAR，例如 Ascend BAR2 基址 `0x4200000000`。在 KVM 里：

| 地址类型 | 登记方式 | Guest 访问时 |
|----------|----------|--------------|
| Guest RAM | **KVM memslot**（`KVM_SET_USER_MEMORY_REGION`） | 通常直接读写，不 exit |
| 设备 BAR | **不在 memslot**（FC 用 `mmio_bus`，QEMU 用 MemoryRegion） | Stage-2 **缺页/无映射** → **Data Abort** → 回到 host |

因此 host dmesg 里 **「outside memslots」** 对 BAR 访问是 **正常现象**，不代表 BAR 配错了；真正要区分的是 KVM **能不能把这次 abort 变成可 emulate 的 MMIO exit**。

### 1.2 NISV 是什么

**NISV** = **Non-ISV** = **Non Instruction Syndrome Valid**（无有效 Instruction Syndrome 的 Data Abort）。

ARM Data Abort 时，硬件在 **ESR（Exception Syndrome Register）** 里报告 fault 细节。其中 **ISV 位（ISS bit 24）**：

| ISV | 含义 | KVM 能做什么 |
|-----|------|--------------|
| **1** | Syndrome 完整：含 **SAS**（访问宽度）、**SRT**（寄存器 Rt）、**WNR**（读/写）等 | 走 **`KVM_EXIT_MMIO`**，userspace 用 `mmio_bus` / VFIO 完成访问 |
| **0** | **NISV**：宽度、寄存器等信息 **无效** | **不能** 走 MMIO exit；只能 **NISV exit** 或 **-ENOSYS** |

常见触发 NISV 的 guest 指令：`STP`/`LDP`、带 post-index 的 `LDR`/`STR`、部分 SIMD/atomic 等——硬件/report 的 syndrome 不足以让 KVM 在内核里完成 MMIO decode。

内核判定与截断（`kernel_op/arch/arm64/include/asm/kvm_emulate.h`）：

```c
kvm_vcpu_dabt_isvalid(vcpu)  // ESR.ISV == 1 ?
// NISV 时仅保留：
esr_iss & (ESR_ELx_CM | ESR_ELx_WNR | ESR_ELx_FSC)
```

### 1.3 正常 MMIO exit vs NISV：VMM 两种完全不同的职责

```text
【路径 A】ISV=1  —  KVM_EXIT_MMIO
  VMM 替 guest「完成」这次访存：
    read  → 从设备读数据，写回 guest 寄存器，KVM 跳过 faulting 指令
    write → 从 guest 寄存器取数据，写到设备，KVM 跳过 faulting 指令

【路径 B】ISV=0  —  KVM_EXIT_ARM_NISV
  VMM **无法** 替 guest 完成访存（不知道写几字节、哪个寄存器）
  可选：
    (1) inject ext_dabt  — 把 fault **报告给 guest**（QEMU / FC 方案 1）
    (2) 自己 decode 指令 + emulate MMIO  — 方案 2，QEMU 默认不做
    (3) 杀 VM  — FC 在方案 1 之前的行为
```

内核文档明确（`Documentation/virt/kvm/api.rst`）：

> KVM does **not** skip the faulting instruction for NISV, unlike KVM_EXIT_MMIO.

即：**inject ext_dabt 之后，faulting 指令仍停在原处**，由 guest 内核/驱动的 Data Abort handler 决定下一步（重试、报错、释放等）。

### 1.4 External Data Abort（外部数据中止）是什么

在 ARM 架构里，Data Abort 的 **FSC（Fault Status Code）** 区分 fault 种类。与 MMIO 场景相关的两类：

| FSC 类 | 典型含义 | 谁产生 |
|--------|----------|--------|
| **Translation fault**（如 `0x04`–`0x07`） | Stage-2 无有效 PTE，IPA 未映射 | 访问 BAR 时 KVM **第一次** trap 常见 |
| **External abort**（`ESR_ELx_FSC_EXTABT = 0x10`） | **外部**同步 abort：如同步的 bus error / 设备侧 abort | **inject ext_dabt** 时 KVM **合成** 给 guest |

**External abort** 的语义是：「这次对某 **物理地址** 的访问在 memory system / 设备侧失败了」，guest OS 会走 **Data Abort 异常向量**，在 **EL1** 看到：

- **FAR_EL1**（Fault Address Register）≈ fault 时的 IPA/GPA  
- **ESR_EL1**：EC = Data Abort，FSC = **External abort**  
- **ELR_EL1**：faulting 指令 PC（**未自动前进**）

对驱动而言，这类似「读写了 MMIO 窗口，但硬件返回了 abort」——驱动可以按平台惯例处理（打印、retry、probe 失败等），而不是 host VMM 直接崩溃。

### 1.5 inject ext_dabt 到底是什么意思

**名称拆解**：

| 词 | 含义 |
|----|------|
| **inject** | VMM 请求 **KVM 向虚拟 CPU 注入** 一条异常（不是 guest 自己触发的真实 bus fault，是 **虚拟化层合成** 的） |
| **ext** | **External** — 注入的是 **External synchronous Data Abort**（FSC=0x10） |
| **dabt** | **Data Abort** — 数据中止，非 Prefetch Abort |

**API 路径**（需要 `KVM_CAP_ARM_INJECT_EXT_DABT`，cap 178）：

```text
NISV exit 返回 userspace
  → VMM: KVM_SET_VCPU_EVENTS { exception.ext_dabt_pending = 1 }
  → KVM: kvm_inject_dabt(vcpu, kvm_vcpu_get_hfar(vcpu))
  → 下一次 KVM_RUN 进入 guest 时，guest 在 EL1 收到 Data Abort
```

内核实现（`kernel_op/arch/arm64/kvm/guest.c` + `inject_fault.c`）：

```c
// guest.c — SET_VCPU_EVENTS
if (ext_dabt_pending)
    kvm_inject_dabt(vcpu, kvm_vcpu_get_hfar(vcpu));

// inject_fault.c — AArch64
vcpu_write_sys_reg(vcpu, addr, FAR_EL1);           // fault 地址
vcpu_write_sys_reg(vcpu, esr | ESR_ELx_FSC_EXTABT, ESR_EL1);  // External abort
enter_exception64(...);                            // 切到 EL1 handler
```

**HFAR** 保存 NISV exit 时的 fault IPA，因此 guest 看到的 **FAR_EL1** 与 `fault_ipa`（如 `0x4200140008`）一致。

**inject ext_dabt 明确表示**：

1. VMM **承认**：「这次 MMIO 我（host）没法在内核/userspace 里替你做完。」  
2. VMM **不** 替 guest 写 BAR、**不** 跳过指令。  
3. VMM 把失败 **转交 guest**：「请你的内核/驱动按 External Data Abort 处理。」  
4. VM **继续运行**（若驱动能容忍或 recover），而不是 VMM `exit(1)`。

**inject ext_dabt 不表示**：

- ❌ 已成功完成对该 MMIO 地址的 read/write  
- ❌ 等价于 `KVM_EXIT_MMIO` + `read_bar`/`write_bar`  
- ❌ 保证 Ascend 驱动 probe 一定成功（驱动可能对 ext abort 仍报错；需实测）

### 1.6 为什么 QEMU 选 inject，而不是 decode 指令

历史上 KVM 对 NISV 直接 **杀 VM**（`-ENOSYS`）。cap 177/178 出现后，KVM 文档与 QEMU 的共识是：

- 对 **真 MMIO** 且 VMM **不想/不能 decode** 时：inject ext_dabt，行为与「访问了没有正确映射的设备窗口」一致；  
- 若 VMM **愿意** 从 guest 内存 fetch 并 decode faulting 指令，可以 **自己 emulate**（内核称 *if you're very brave*），但 QEMU **未做**，FC 方案 1 亦未做。

对 Ascend VFIO：quirk 与常规 `len=4` read 仍走 **路径 A**；probe 中 occasional **STP write** 走 **路径 B + inject**，与 virsh/QEMU 对齐。

### 1.7 三个 cap / exit 的关系（速查）

| 名称 | 编号 | 作用 |
|------|------|------|
| `KVM_CAP_ARM_NISV_TO_USER` | 177 | VM 创建时 enable；NISV 时 **exit 28 + fault_ipa**，而非 `-ENOSYS` |
| `KVM_EXIT_ARM_NISV` | exit 28 | userspace 收到 NISV 的结构化 exit |
| `KVM_CAP_ARM_INJECT_EXT_DABT` | 178 | 允许 NISV/MMIO 之后 `ext_dabt_pending=1` |
| `KVM_SET_VCPU_EVENTS` | ioctl | 设置 `ext_dabt_pending`，触发 §1.5 注入 |

---

## 2. 从 host dmesg 一行日志说起

典型内核日志（enable `KVM_CAP_ARM_NISV_TO_USER` 后）：

```text
[301.764350] kvm [1024]: nisv on 0x3f01b000 (esr 0x92000044)
```

Firecracker 侧对应日志：

```text
KVM_EXIT_ARM_NISV: fault_ipa=0x4200140008 esr_iss=0x45
fault_ipa hint: VFIO NPU BAR2 GPA
```

### 2.1 这行 dmesg 体现了什么

| 字段 | 含义 |
|------|------|
| **`nisv`** | **Non-ISV** data abort：ESR 中 **ISV=0**，KVM 无法从 syndrome 解码出 MMIO 所需的 width / 寄存器 |
| **`0x3f01b000` / `fault_ipa`** | Stage-2 fault 的 **IPA（= Guest GPA）**；应与客户机 PCI BAR 映射的 MMIO 物理地址对照 |
| **`esr 0x92000044`** | 完整 **ESR_EL2** 快照，用于人工解码 EC / ISS |

以 `esr = 0x92000044` 为例（位域定义见 `kernel_op/arch/arm64/include/asm/esr.h`）：

| 位域 | 值 | 解读 |
|------|-----|------|
| **EC** [31:26] | `0x24` | `ESR_ELx_EC_DABT_LOW` — 来自较低 EL 的 Data Abort |
| **IL** [25] | `1` | 32-bit 指令 |
| **ISV** [ISS 24] | `0` | **NISV** — SAS/SRT 等无效 |
| **ISS 低位** | `0x44` | FSC=`0x04`（translation fault L0），WNR=`1`（**写**），CM=`0` |

**要点**：dmesg 的 `nisv on <ipa>` 与 FC 的 `KVM_EXIT_ARM_NISV: fault_ipa=…` 描述 **同一类 fault**——guest 访问 **非 RAM memslot** 的 MMIO GPA，且硬件未给出完整 instruction syndrome。  
`fault_ipa` 落在 Guest BAR 区间内（如 BAR2 `0x4200000000` + offset）说明 **设备映射正确**，问题在 **KVM 交付路径**，不是「BAR 未注册 memslot」。

---

## 3. 内核：从 fault 到 userspace 的分叉

源码：`kernel_op/arch/arm64/kvm/mmio.c`

```text
Guest 访问 IPA（不在 guest RAM memslot）
        │
        ▼
   io_mem_abort(fault_ipa)
        │
   kvm_vcpu_dabt_isvalid()  ← ESR.ISV == 1 ?
        │
   ├─ YES → dabt_get_as / dabt_get_rd / dabt_iswrite
   │         → kvm_io_bus 或 KVM_EXIT_MMIO（GPA + len + data）
   │
   └─ NO  → NISV
             ├─ cap 177 未 enable → -ENOSYS + dmesg "outside memslots…"
             └─ cap 177 已 enable → KVM_EXIT_ARM_NISV
                                   arm_nisv.{fault_ipa, esr_iss}
                                   （esr_iss 仅 CM|WNR|FSC）
```

NISV 时 **不会** 调用 userspace 的 `read_bar` / `write_bar`——因为根本没有 `KVM_EXIT_MMIO`。

---

## 4. QEMU 如何处理 NISV

### 4.1 VM 创建：enable cap（与 FC 对齐）

**文件**：`qemu/target/arm/kvm.c`，函数 `kvm_arm_init()`（约 618–626 行）

```c
if (kvm_check_extension(s, KVM_CAP_ARM_NISV_TO_USER)) {
    if (kvm_vm_enable_cap(s, KVM_CAP_ARM_NISV_TO_USER, 0)) {
        error_report("Failed to enable KVM_CAP_ARM_NISV_TO_USER cap");
    } else {
        cap_has_inject_ext_dabt = kvm_check_extension(s,
                                KVM_CAP_ARM_INJECT_EXT_DABT);
    }
}
```

| 步骤 | QEMU | 说明 |
|------|------|------|
| ① | `KVM_CHECK_EXTENSION(NISV_TO_USER)` | 探测 host 支持 |
| ② | `kvm_vm_enable_cap(NISV_TO_USER)` | **按 VM** 打开 NISV→userspace |
| ③ | `KVM_CHECK_EXTENSION(INJECT_EXT_DABT)` | 仅探测，记录到 `cap_has_inject_ext_dabt` |

**时机**：KVM 初始化、VM 创建阶段（早于 vCPU `KVM_RUN`）。

### 4.2 vCPU exit：识别 `KVM_EXIT_ARM_NISV`

**文件**：`qemu/target/arm/kvm.c`，`kvm_arch_handle_exit()`（约 1556–1560 行）

```c
case KVM_EXIT_ARM_NISV:
    ret = kvm_arm_handle_dabt_nisv(cpu, run->arm_nisv.esr_iss,
                                   run->arm_nisv.fault_ipa);
    break;
```

QEMU 在 **arch-specific exit handler** 里专门处理 reason `28`；通用 KVM 主循环会把未识别的 exit 交给 `kvm_arch_handle_exit`。

### 4.3 核心处理：`kvm_arm_handle_dabt_nisv`（方案 1 — inject ext_dabt）

**文件**：`qemu/target/arm/kvm.c`（约 1429–1466 行）

```c
static int kvm_arm_handle_dabt_nisv(ARMCPU *cpu, uint64_t esr_iss,
                                    uint64_t fault_ipa)
{
    if (cap_has_inject_ext_dabt) {
        struct kvm_vcpu_events events = { };
        events.exception.ext_dabt_pending = 1;
        if (!kvm_vcpu_ioctl(CPU(cpu), KVM_SET_VCPU_EVENTS, &events)) {
            env->ext_dabt_raised = 1;
            return 0;   /*  handled — 继续 KVM_RUN  */
        }
    } else {
        error_report("Data abort … at physical address: 0x%lx", fault_ipa);
        error_printf("KVM unable to emulate faulting instruction.\n");
    }
    return -1;
}
```

**QEMU 明确不做的事**（注释与实现一致）：

- **不** 从 guest 内存 decode  faulting 指令；
- **不** 替 guest 完成 MMIO read/write；
- **不** 把 NISV 转成 `KVM_EXIT_MMIO` 再走 VFIO `region_read/write`。

**QEMU 做的事**：

1. 读 `run->arm_nisv.fault_ipa` / `esr_iss`；
2. `KVM_SET_VCPU_EVENTS` 设 `ext_dabt_pending = 1`；
3. 返回 `0` → 主循环 **继续** `KVM_RUN`；
4. KVM 向 guest **注入 external data abort**（用 fault 时的 IPA）；
5. Guest OS / 驱动在 EL1 收到 Data Abort，自行处理（retry、报错、释放资源等）。

内核文档（`kernel_op/Documentation/virt/kvm/api.rst` §4.32）说明：`ext_dabt_pending` 只允许在 **`KVM_EXIT_MMIO` 或 `KVM_EXIT_ARM_NISV`** 之后设置，目的是让不同 VMM 以统一方式把「无法 emulate 的 I/O 访问」报告给 guest。

### 4.4 可选验证：`kvm_arch_pre_run`（32-bit 边缘情况）

**文件**：`qemu/target/arm/kvm.c`（约 1332–1357 行）

QEMU 在 **AArch32 guest** 上额外维护 `env->ext_dabt_raised`：下次 `KVM_RUN` 前检查 ESR_EL1 的 DFSC 是否为 external abort，防止旧内核误注入导致 **无限重跑同一条指令**。  
Ascend 310P passthrough 场景为 **AArch64 guest**，此路径通常不触发；Firecracker **未实现** 对等验证（见 §6.3 差异表）。

---

## 5. Firecracker 对齐实现

### 5.1 能力 enable（对齐 QEMU §4.1）

| 组件 | 路径 | 行为 |
|------|------|------|
| Cap enable | `src/vmm/src/arch/aarch64/kvm.rs` → `try_enable_arm_nisv_caps()` | `CHECK` + `ENABLE` cap 177；`CHECK` cap 178 |
| 调用时机 | `src/vmm/src/vstate/vm.rs` → `create_common()` | `KVM_CREATE_VM` 成功后立即调用 |
| 状态保存 | `VmCommon.arm_inject_ext_dabt` | 供各 vCPU 使用 |

### 5.2 vCPU exit 分发（对齐 QEMU §4.2）

**文件**：`src/vmm/src/vstate/vcpu.rs` → `run_emulation()`

```rust
Ok(VcpuExit::Unsupported(reason)) if reason == KVM_EXIT_ARM_NISV => {
    handle_arm_nisv_exit(&mut self.kvm_vcpu.fd, arm_inject_ext_dabt)
}
```

因 `kvm-ioctls 0.24` 无 `VcpuExit::ArmNisv` variant，reason `28` 落在 `Unsupported(28)`；语义等同 QEMU 的 `case KVM_EXIT_ARM_NISV`。

另有 `Ok(VcpuExit::Unknown)` + `is_arm_nisv_kvm_run()` 兜底分支。

### 5.3 NISV handler（对齐 QEMU §4.3）

**文件**：`src/vmm/src/arch/aarch64/vcpu.rs` → `handle_arm_nisv_exit()`

```rust
events.exception.ext_dabt_pending = 1;
fd.set_vcpu_events(&events)?;
Ok(VcpuEmulation::Handled)   // 继续 KVM_RUN，不 fatal exit
```

与 QEMU `kvm_arm_handle_dabt_nisv` **行为一致**：inject ext_dabt → continue guest。

### 5.4 vCPU 接线

**文件**：`src/vmm/src/lib.rs`

```rust
vcpu.set_arm_inject_ext_dabt(self.vm.common.arm_inject_ext_dabt);
```

### 5.5 诊断（FC 额外，QEMU 无同等日志）

**文件**：`src/vmm/src/arch/aarch64/enosys_log.rs`

- ENOSYS 路径：读 EL1/EL2 寄存器，提示 host dmesg 常见 `"Data abort outside memslots"`；
- NISV 路径：打印 `fault_ipa`、`esr_iss`，并对照 NPU BAR2/BAR4/BAR0 GPA 区间给出 hint。

---

## 6. QEMU vs Firecracker 对照

### 6.1 NISV 处理路径（方案 1）

| 步骤 | QEMU | Firecracker | 对齐 |
|------|------|-------------|------|
| VM 创建 enable cap 177 | `kvm_arm_init` | `try_enable_arm_nisv_caps` | ✅ |
| 探测 cap 178 | `cap_has_inject_ext_dabt` | `ArmNisvCaps.inject_ext_dabt` | ✅ |
| 识别 exit 28 | `kvm_arch_handle_exit` | `run_emulation` + `Unsupported(28)` | ✅ |
| inject ext_dabt | `KVM_SET_VCPU_EVENTS` | `set_vcpu_events` | ✅ |
| 继续 guest | `return 0` | `Ok(VcpuEmulation::Handled)` | ✅ |
| 无 cap 178 时 | `error_report` + `return -1` | `FaultyKvmExit` | ✅ |
| 指令 decode + MMIO | **不做** | **不做**（方案 2 待做） | ✅ |
| AArch32 ext_dabt 验证 | `kvm_arch_pre_run` | **未实现** | ⚠️ 不影响 Ascend AArch64 |

### 6.2 设备 MMIO 交付（方案 3 已落地）

| | QEMU（VFIO PCI） | Firecracker（fixed-vfio，方案 3 后） |
|--|------------------|--------------------------------------|
| BAR 暴露方式 | `memory_region_init_io` + **VFIO region mmap**（`hw/vfio/region.c`） | 整 BAR 仍 `mmio_bus.insert`；可 mmap 子区间另挂 **KVM memslot** |
| Guest 访问可 mmap 区 | stage-2 → host `mmap` 页，无 VMM exit | 同左（`KVM_SET_USER_MEMORY_REGION`） |
| 必须 trap 的页 | MSI-X table / quirk 以更高优先级 IO subregion 覆盖 | **页级打洞**（不注册 memslot），继续走 `read_bar`/`write_bar` |
| Quirk | `hw/vfio/pci-quirks.c` | `ascend_quirks.rs`（空洞页内） |
| 首版范围 | 全 BAR（含 sparse） | **BAR0 + BAR2**（≤256MiB）；**BAR4 8GiB 暂不 mmap** |

实现要点（对齐 QEMU / Dragonball）：

1. Attach 时 `probe_region`：读 `VFIO_REGION_INFO_FLAG_MMAP` / sparse caps，打日志。
2. `mmap(MAP_SHARED)` VFIO device fd → `Vm::set_user_memory_region`（GPA = BAR base + offset）。
3. 打洞：MSI-X table 所在页（BAR0）、Ascend quirk `0x100430` 所在页（BAR2）。
4. `commit_all_bars` / peel：先删旧 KVM slot 再按新 GPA 重建。
5. 方案 1（inject ext_dabt）保留，作空洞上 NISV 的兜底。

源码：`src/vmm/src/devices/pci/vfio/bar_mmap.rs`，由 `FixedVfioPciDevice::sync_bar_kvm_mmaps` 调用。

### 6.3 方案 2（未实现）：指令 decode + 真正完成 MMIO

QEMU 与 FC **当前均未** 在 NISV 路径做 instruction decode。mmap 之后若空洞外仍出现 NISV，再考虑方案 2。

---

## 7. 端到端时序（PPT 用）

```text
┌────────── Guest ──────────┐     ┌────────── KVM ──────────┐     ┌──── VMM ────┐
│ STP/LDP/… store @ BAR GPA │────►│ ISV=0 stage-2 fault       │────►│             │
└───────────────────────────┘     │ cap177? NISV exit       │     │             │
                                  │ fault_ipa, esr_iss      │     │             │
                                  └─────────────────────────┘     │             │
                                         │                        │ QEMU / FC   │
                                         │  dmesg: nisv on IPA    │             │
                                         └───────────────────────►│             │
                                                                │ SET_VCPU_   │
                                                                │ EVENTS      │
                                                                │ ext_dabt=1  │
                                                                └──────┬──────┘
                                                                       │
                                  ┌─────────────────────────┐        │
                                  │ KVM 注入 ext data abort │◄───────┘
                                  │ 继续 KVM_RUN            │
                                  └───────────┬─────────────┘
                                              ▼
                                  ┌─────────────────────────┐
                                  │ Guest EL1 Data Abort    │
                                  │ 驱动 / 内核处理         │
                                  └─────────────────────────┘
```

**与正常 MMIO / mmap 对比**：

```text
【方案 3】STP @ BAR2+0x140008（已 mmap）
  → stage-2 命中 KVM memslot → host VFIO mmap 页，无 exit

【空洞】quirk / MSI-X table 页
  ISV=1  LDR/STR  →  KVM_EXIT_MMIO  →  mmio_bus  →  read_bar/write_bar
  ISV=0  STP/…    →  KVM_EXIT_ARM_NISV  →  ext_dabt（方案 1 兜底）
```

---

## 8. Ascend 场景下的日志对照表

| 观测点 | 典型内容 | 说明 |
|--------|----------|------|
| FC attach | `BAR mmap probe vfio_region=2 … mappable=true` | Phase 0：BAR2 可 mmap |
| FC attach | `BAR mmap active … gpa=0x4200…` | Phase 1：KVM slot 已挂 |
| FC attach | `BAR mmap hole …` / quirk / MSIX | 打洞页仍走 trap |
| FC（方案 3 后，期望） | **无** `fault_ipa=0x4200140008` NISV | `STP` 不再 trap |
| FC（方案 1 兜底） | `injected ext_dabt at fault_ipa=…` | 仅空洞或未 mmap 区 |
| FC（方案 1 前） | `Firecracker exiting with error` | NISV 当 fatal（已修） |

`esr_iss=0x45` 解码（若仍出现）：`FSC=0x05`，`WNR=1`（写），`CM=0`；无 SAS/SRT。

---

## 9. 源码索引

### QEMU（`qemu/`）

| 文件 | 符号 / 区域 | 作用 |
|------|-------------|------|
| `target/arm/kvm.c` | `kvm_arm_init` | enable `KVM_CAP_ARM_NISV_TO_USER` |
| `target/arm/kvm.c` | `kvm_arch_handle_exit` | `KVM_EXIT_ARM_NISV` 分发 |
| `target/arm/kvm.c` | `kvm_arm_handle_dabt_nisv` | inject ext_dabt |
| `target/arm/kvm.c` | `kvm_arch_pre_run` | AArch32 ext_dabt 验证 |
| `hw/vfio/region.c` | `vfio_region_setup` / mmap | BAR mmap，减少 trap |

### Firecracker（`firecracker/src/vmm/`）

| 文件 | 符号 | 作用 |
|------|------|------|
| `arch/aarch64/kvm.rs` | `try_enable_arm_nisv_caps` | cap 177/178 |
| `vstate/vm.rs` | `create_common` | VM 创建时 enable |
| `vstate/vcpu.rs` | `run_emulation` | NISV exit 分发 |
| `arch/aarch64/vcpu.rs` | `handle_arm_nisv_exit` | inject ext_dabt |
| `arch/aarch64/enosys_log.rs` | `log_arm_nisv_exit` | 诊断与 BAR GPA hint |
| `lib.rs` | `set_arm_inject_ext_dabt` | vCPU 接线 |
| `devices/pci/vfio/bar_mmap.rs` | `probe_region` / `map_bar` / `unmap_bar` | 方案 3：VFIO mmap + KVM slot |
| `devices/pci/vfio/device.rs` | `sync_bar_kvm_mmaps` / `commit_all_bars` | 打洞、BAR 搬迁时重建 slot |

### 内核（`kernel_op/`）

| 文件 | 作用 |
|------|------|
| `arch/arm64/kvm/mmio.c` | ISV 分支 / NISV / ENOSYS |
| `arch/arm64/kvm/arm.c` | `KVM_ENABLE_CAP` NISV |
| `arch/arm64/include/asm/esr.h` | ESR 位域定义 |
| `arch/arm64/include/asm/kvm_emulate.h` | `dabt_isvalid` / `dabt_iss_nisv_sanitized` |
| `Documentation/virt/kvm/api.rst` | cap 177 / 178 / ext_dabt 语义 |

---

## 10. 验证清单（方案 3）

| # | 检查项 | 期望 |
|---|--------|------|
| 1 | FC 日志 | `BAR mmap probe vfio_region=2 … mappable=true` |
| 2 | FC 日志 | `BAR mmap active` 覆盖 BAR2（含 `0x140008` 所在页） |
| 3 | FC 日志 | quirk / MSIX 页有 hole；`ascend quirk …`、`MSI route updated` 仍出现 |
| 4 | Guest probe | **无** `fault_ipa=0x4200140008` NISV / SEA panic |
| 5 | Guest | `lsmod` 含 davinci 类模块；可选 `npu-smi info` |
| 6 | 方案 1 | cap 177/178 仍 enable（空洞兜底） |

---

## 11. 后续工作

| 方案 | 内容 | 状态 |
|------|------|------|
| **方案 1** | inject ext_dabt，继续 guest | ✅ 已实现（空洞兜底） |
| **方案 3** | VFIO BAR mmap + 页级打洞 | ✅ 已实现（BAR0/BAR2；BAR4 暂缓） |
| **方案 2** | 指令 decode + 真正完成 MMIO | 待定（mmap 后若仍 NISV） |
| BAR4 mmap | 8GiB region 的 slot / pfnmap 预算 | 待定 |

---

## 12. 一句话总结

**NISV** 表示 guest 访问设备 MMIO 时 **ISV=0**，KVM 无法给出 `KVM_EXIT_MMIO`。Firecracker 已与 QEMU 对齐两条路径：**方案 3** 对可 mmap 的 BAR 子区间挂 KVM memslot，使 `STP` 直达设备；**方案 1** 对打洞页（quirk / MSI-X）仍 inject ext_dabt 作兜底。FC 用「整 BAR `mmio_bus` + memslot 覆盖可 mmap 区」等价 QEMU 的 MemoryRegion overlap。

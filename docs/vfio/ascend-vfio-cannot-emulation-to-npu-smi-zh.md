<!--
Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Ascend VFIO：从 “cannot emulate / ENOSYS” 到 `npu-smi info` 可用

> **环境**：Firecracker aarch64 + Ascend 310P（`19e5:d500`，host BDF `0000:01:00.0`）VFIO 直通  
> **Guest**：openEuler rootfs，驱动 **25.3.rc1**（与 QEMU 对照机的 24.1.rc2 **不同**）  
> **终态验收**：guest 内 `npu-smi info` 有表、Health OK；`/dev/davinci0/1` 存在；DSMI health=0；ACL H2D/D2H `ACL_SMOKE_OK`  
> **代码基线（可存档）**：`main` 上 `d34f58557`（功能）+ `93cd2570e`（清理）；建议打 tag 后再做 SMBIOS

**相关文档**

| 文档 | 内容 |
|------|------|
| [ascend-vfio-quirk-enosys-nisv-journey-zh.md](./ascend-vfio-quirk-enosys-nisv-journey-zh.md) | Quirk / NISV 早期排查（PPT 向） |
| [ascend-vfio-nisv-qemu-fc-alignment-zh.md](./ascend-vfio-nisv-qemu-fc-alignment-zh.md) | 与 QEMU NISV/ext_dabt 对齐 |
| [ascend-vfio-quirks-design-zh.md](./ascend-vfio-quirks-design-zh.md) | Ascend BAR quirk |
| [pci-bar-move-ppt-zh.md](./pci-bar-move-ppt-zh.md) | BAR 固定布局与 move_bar |

**现场日志归档（workspace 根目录，便于复盘）**

| 文件 | 阶段 |
|------|------|
| `quirk_log.txt` | Quirk 已命中仍 ENOSYS |
| `fc_nisv_guest_panic_20260915.log` | NISV + ext_dabt → guest SEA panic |
| `fc_bar_mmap_verify.log` / `fc_bar_mmap_guest2.txt` | BAR0/2 mmap，BAR4 仍 skip → `uda_wait` |
| `fc_msix512.log` / `fc_msix512_guest.txt` | MSI-X 512 + BAR4 mmap 后驱动起，仍 `-9005` |
| `fc_dcmi_deep.txt` | DCMI 卡列表空 / `-9005` 根因 |
| `fc_regress.log` / `fc_regress_guest.txt` | 终态：无 NISV，`npu-smi` OK |
| `fc_acl_smoke_final.txt` | ACL `ACL_SMOKE_OK` |

---

## 1. 问题是什么（含现场日志）

Guest 驱动在 probe Ascend BAR 时，会用 **KVM 无法完整 decode 的访存指令**（典型为 `STP` 等）访问设备 MMIO。

表现演进：

| 阶段 | 日志/现象 | 含义 |
|------|-----------|------|
| 最初 | `KVM failed to emulate an instruction` / `-ENOSYS` | KVM 不能 emulate，且当时未把 fault 结构化交给 VMM |
| 打开 NISV cap 后 | `KVM_EXIT_ARM_NISV`，`fault_ipa=0x4200140008` | 故障 GPA 落在 **NPU BAR2**（`0x4200000000 + 0x140008`） |
| 只 inject ext_dabt | guest 收到 synchronous external abort → panic | 对齐 QEMU 兜底，但 **没有真正完成那次 MMIO** |
| BAR mmap 不全 / MSI-X 不足 | `uda_wait` timeout；`npu-smi` 卡住 | 双 die / HDC 通道起不齐 |
| 驱动已起 | `npu-smi` → `-9005` | DCMI 卡枚举缺 SMBIOS / 无 `board_cfg` |
| 终态 | NISV 基本消失；`npu-smi info` OK | BAR mmap + MSI-X + quirk + guest `board_cfg` |

一句话：**不是 quirk 没装上，而是 MMIO 交付路径（trap vs mmap）和中断向量数量不够，再加上 DCMI 卡枚举缺 SMBIOS。**

### 1.1 现场 A：Quirk 已工作，立刻 ENOSYS（VM 退出）

摘自 `quirk_log.txt`（2026-07-04）：

```text
fixed-vfio: ascend quirk read ... offset=0x100430 len=4 val=0x00000000
ENOSYS guest state: PC=0xffff800008f410d8 FAR_EL1=0xffffbae6cec0 ESR_EL1=0x56000000
ENOSYS hint: FAR outside known ITS/BAR ranges
Received ENOSYS error because KVM failed to emulate an instruction.
Vmm is stopping.
```

要点：上一条还是 quirk / BAR MMIO，下一条直接 ENOSYS —— **未走完整 `KVM_EXIT_MMIO` 交付**。

### 1.2 现场 B：NISV 定位到 BAR2，ext_dabt 后 guest panic

摘自 `fc_nisv_guest_panic_20260915.log`：

```text
fixed-vfio: ascend quirk read ... offset=0x100430 ...
KVM_EXIT_ARM_NISV: fault_ipa=0x4200140008 esr_iss=0x45
fault_ipa hint: VFIO NPU BAR2 GPA
ARM_NISV: KVM could not decode the faulting instruction; userspace must emulate or abort the guest
KVM_EXIT_ARM_NISV: injected ext_dabt at fault_ipa=0x4200140008; continuing guest

[guest] Internal error: synchronous external abort: ffffffff96000010 [#1] SMP
[guest] Workqueue: events devdrv_load_finish [drv_pcie_host]
[guest] pc : devdrv_sync_non_trans_msg_send+0xc8/0x620 [drv_pcie_host]
```

要点：`fault_ipa = BAR2_base + 0x140008`；inject 只保 VMM 不死，**guest 驱动仍认为写失败**。

### 1.3 现场 C：BAR4 未 mmap → `uda_wait`；驱动半起

早期 BAR mmap 日志（`fc_bar_mmap_verify.log`）里 BAR4 被跳过：

```text
BAR mmap probe vfio_region=4 ... skip=Some("BAR larger than 256MiB (deferred)")
BAR mmap skipped vfio_region=4 guest_base=0x4000000000 mappable=false areas=0
MSIX cap ... vectors=512 ...
```

Guest（`fc_bar_mmap_guest2.txt`）：

```text
[ascend] [uda] [ERROR] [uda_wait_all_phy_startup 1342] <npu-smi:...>
  Wait timeout. (dev_num=1; uda_detected_dev_num=2)
```

要点：已能看到双 die 探测不一致；随后把 **`MAX_MMAP_BAR_SIZE` 提到 8GiB** 并对 MSI-X 扩到 512。

### 1.4 现场 D：驱动节点在，仍 `npu-smi -9005`

摘自 `fc_msix512_guest.txt` / `fc_dcmi_deep.txt`：

```text
/dev/davinci0  /dev/davinci1  均存在
msg_chan_* ITS-MSI 有计数

==== npu-smi ====
npu get board type failed. ret is -9005
npu_get_first_ready_device failed. err is -9005
can not get ok device.

num_list 0 0 [0, 0, 0, 0]    # DCMI 卡列表空
dev_in_card2048 -8001
```

同时对照：FC guest **无** `/sys/class/dmi` / SMBIOS Type 9；QEMU 有 Type 9 slot↔BDF，`card_id=2048`。

---

## 2. 排查方式（怎么定位的）

### 2.1 对照实验

- **QEMU/libvirt** 同一块卡 VFIO：guest `npu-smi` 正常（card_id=2048，双 die）。
- **Firecracker**：逐步加日志，确认 quirk 已命中后仍然 ENOSYS/NISV。

### 2.2 关键证据链

1. Quirk 日志出现 `ascend quirk read/write @ BAR2+0x100430` → quirk 工作线 OK。  
2. 下一条 fault **没有** `BAR MMIO read/write` 日志 → 未走 `KVM_EXIT_MMIO`。  
3. Enable `KVM_CAP_ARM_NISV_TO_USER` 后得到 `fault_ipa` → 精确定位 BAR2+`0x140008`。  
4. 与 QEMU 对齐：QEMU 对 NISV inject ext_dabt；但对 **mappable** BAR 优先 **mmap + KVM memslot**，STP 直接 stage-2，根本不进 NISV。  
5. 驱动半起：看 `uda_wait` + MSI-X 向量数 + BAR4 mmap 是否 skip。  
6. `-9005`：DSMI health=0 但 `dcmi_get_card_num_list` 空 → 查 DMI / `board_cfg`，不是 VFIO 没通。

### 2.3 驱动起来之后仍 `npu-smi -9005`

| 检查 | FC | QEMU |
|------|----|------|
| `/dev/davinci0/1` | 有 | 有 |
| DSMI device_count / health | 2 / 0 | 2 / 0 |
| `dcmi_get_card_num_list` | 空 | `[2048]` |
| `/sys/class/dmi` / Type 9 | **无** | **有** |
| `dmidecode -t9` | DCMI 会调，但无真实 SMBIOS | 有 slot↔BDF |

结论：驱动 probe 已成功；`-9005` 是 **DCMI 用户态卡枚举**失败，不是 VFIO 没直通。

---

## 3. 改了哪些东西（Firecracker 代码）

下列均相对「fixed-vfio Stage A/B/C 基线」之上的关键增量（工作区终态）。

### 3.1 Ascend BAR Quirk

| 文件 | 作用 |
|------|------|
| `src/vmm/src/devices/pci/vfio/ascend_quirks.rs` | 对齐 QEMU：`BAR2+0x100430` 读走 VFIO、写丢弃 |
| `device.rs` `read_bar` / `write_bar` | 命中 quirk 窗口时走 quirk，不误写设备 |

### 3.2 NISV / ENOSYS 诊断与兜底

| 文件 | 作用 |
|------|------|
| `src/vmm/src/arch/aarch64/kvm.rs` | enable `KVM_CAP_ARM_NISV_TO_USER` |
| `src/vmm/src/arch/aarch64/enosys_log.rs` | ENOSYS/NISV 时打印 regs + `fault_ipa` hint |
| `src/vmm/src/arch/aarch64/vcpu.rs` | `handle_arm_nisv_exit`：可选 inject ext_dabt |
| `src/vmm/src/vstate/vcpu.rs` | `run_emulation` 识别 exit 28 / ENOSYS |

方案 1（ext_dabt）保证 VM 不因未处理 exit 直接死掉；**真正消掉 STP NISV 靠方案 3**。

### 3.3 方案 3：VFIO BAR mmap（核心）

| 文件 | 作用 |
|------|------|
| `src/vmm/src/devices/pci/vfio/bar_mmap.rs` | 将 VFIO mappable 子区间 mmap 进 guest GPA；`MAX_MMAP_BAR_SIZE=8GiB`（含 BAR4） |
| `device.rs` | attach 后建立 mmap；对 MSI-X table / quirk 窗口 **打洞**，继续 trap |

效果：guest `STP` 等指令直接打到设备映射，不再依赖 KVM instruction emulate。

### 3.4 MSI-X

| 改动 | 原因 |
|------|------|
| `MAX_VFIO_MSIX_VECTORS` **64 → 512**（`irq.rs`） | 310P 上报 512 向量；64 时 HDC/msg_chan 绑不全，`uda_wait` / 双 die 起不来 |
| MSI-X table 写 **只更新 KVM 路由，不 `region_write` 穿透到 BAR** | 避免与 VFIO `SET_IRQS` 竞态导致健康/HDC 卡住 |

### 3.5 PCI BAR 布局 / virtio 共存（既有能力，联调中加固）

- NPU guest-final：`BAR4=0x4000…` / `BAR2=0x4200…` / `BAR0=0x4210…`
- virtio BAR move 后 ioeventfd 重注册（避免 hung_task）

### 3.6 启动脚本

- `firecracker/start_fc_socket.sh`：`FC_VFIO_PCI_BDF`（默认 `0000:01:00.0`）、`--enable-pci`、日志级别（默认 `Info`）
- 仓库外 `start_fc_vm.sh`：KERNEL/ROOTFS/TAP/IP 默认值；`KERNEL_BOOT_ARGS` 已含 `net.ifnames=0` + 静态 IP

---

## 4. Guest / 运维侧改动（非 FC 代码）

| 项 | 说明 |
|------|------|
| `/run/board_cfg.ini` 内容 `board_type=0` | 绕过「无 SMBIOS Type 9 → 卡列表为空」 |
| `fc-ascend-board-cfg.service` + `fc_ascend_board_cfg.sh` | 开机写入上述文件（已写入当前 rootfs） |
| 驱动版本 | 保持 rootfs 自带 **25.3.rc1**；曾临时 scp QEMU 24.1 用户态对比，**已还原并删除残留** |

FC **没有**实现 SMBIOS Type 9；QEMU 有。正规对齐需 VMM 向 guest 暴露 DMI；当前用 `board_cfg` 是用户态 workaround。

副作用（已知）：`card_id=0`（QEMU 为 2048），`npu-smi` 常只显示 1 行 chip；`/dev/davinci0/1` 与 DSMI 双 die 仍在。

---

## 5. 解决办法对照（问题 → 手段）

```text
cannot emulate / ENOSYS
  → enable NISV + 日志（fault_ipa）
  → ext_dabt 兜底（不停 VM）
  → VFIO BAR mmap（消 STP NISV）★

驱动起不来 / uda_wait
  → MSI-X 512
  → BAR4 一并 mmap
  → MSI-X 表不写穿 BAR

npu-smi -9005 / 卡列表空
  → DSMI 已健康，查 DCMI + DMI
  → /run/board_cfg.ini board_type=0（guest，临时）
  → 正规：VMM 补 SMBIOS Type 9（见 §9）
```

---

## 6. 解决后日志（终态对照）

### 6.1 Host：BAR0/2/4 均 mmap，无 NISV 风暴

摘自 `fc_regress.log`（2026-09-27）：

```text
BAR mmap probe vfio_region=0 ... mappable=true
BAR mmap probe vfio_region=2 ... mappable=true
BAR mmap probe vfio_region=4 ... mappable=true   # 不再 skip
BAR mmap active vfio_region=0 ... gpa=0x4210000000
BAR mmap active vfio_region=2 ... gpa=0x4200000000
BAR mmap active vfio_region=4 ... gpa=0x4000000000 size=0x200000000
```

同次回归日志中 **无** `fault_ipa` / `KVM_EXIT_ARM_NISV` / `ENOSYS`。

Guest 侧 board_cfg 服务：

```text
Starting Ascend board_cfg.… Firecracker (DCMI card enum)...
[  OK  ] Finished Ascend board_cfg.…
```

### 6.2 Guest：`npu-smi info` Health OK

摘自 `fc_regress_guest.txt`：

```text
| npu-smi 25.3.rc1                                 Version: 25.3.rc1                                     |
| NPU     Name                  | Health          | Power(W)     Temp(C)           Hugepages-Usage(page) |
| 0       310P3                 | OK              | 0.0          51                0     / 0             |
| 0       0                     | 0000:00:01.0    | 0            1723 / 44278                            |
```

（`card_id`/`Chip` 显示为 workaround 形态；Bus-Id 为扁平 PCI `0000:00:01.0`。）

### 6.3 Guest：ACL 冒烟

摘自 `fc_acl_smoke_final.txt`（驱动完全就绪后）：

```text
aclrtGetDeviceCount=2
aclrtSetDevice(0) OK
H2D/D2H mismatch=0 (sample h_out[1]=0.500000)
ACL_SMOKE_OK
rc=0
```

说明：刚 boot 立刻跑 ACL 可能出现 `aclrtSetDevice → 507033`；等 `/dev/davinci*` 与驱动 settle 后再跑即可。

---

## 7. 终态怎么验收

Guest（密码见运维约定，示例 `abc123!`，IP `172.16.88.2`）：

```bash
cat /run/board_cfg.ini          # 期望 board_type=0
ls -l /dev/davinci0 /dev/davinci1
npu-smi info                    # 有表，Health OK
```

Host 日志期望：

- 有 `BAR mmap active`（BAR0/2/4）
- 无明显重复的 `fault_ipa=0x4200140008` NISV 风暴
- quirk / MSIX 路径仍可用

---

## 8. 手动启动与验收命令

脚本已带默认值；一般 **无需** 手动 `export`（`BDF` / socket / KERNEL / 静态 IP 均有默认）。

### 8.1 Host：绑定 VFIO

```bash
BDF=0000:01:00.0
# 若被 libvirt/QEMU 占用：virsh destroy <domain>
echo "$BDF" | sudo tee /sys/bus/pci/devices/$BDF/driver/unbind 2>/dev/null || true
echo vfio-pci | sudo tee /sys/bus/pci/devices/$BDF/driver_override
echo "$BDF" | sudo tee /sys/bus/pci/drivers/vfio-pci/bind
basename $(readlink -f /sys/bus/pci/devices/$BDF/driver)   # 期望 vfio-pci
ls /dev/vfio/
```

### 8.2 终端 A：启动 Firecracker API

```bash
cd /home/zjj/modify_firecracker/firecracker
bash ./start_fc_socket.sh
# 排查 BAR/NISV 时可：FC_LOG_LEVEL=Debug bash ./start_fc_socket.sh
```

### 8.3 终端 B：配置并启动 VM

```bash
cd /home/zjj/modify_firecracker
bash ./start_fc_vm.sh
```

### 8.4 Guest：验收直通 / `npu-smi`

```bash
sshpass -p 'abc123!' ssh -o StrictHostKeyChecking=no root@172.16.88.2
cat /run/board_cfg.ini          # board_type=0
ls -l /dev/davinci0 /dev/davinci1
npu-smi info                    # Health OK
```

### 8.5 Guest：CANN/ACL 计算冒烟（可选）

Guest 需已有 toolkit runtime（本环境已装到
`/usr/local/Ascend/ascend-toolkit/8.3.RC2/aarch64-linux`）。host 上编译的
`cann_guest_smoke/acl_device_smoke`（H2D/D2H）已验证 `ACL_SMOKE_OK`、`aclrtGetDeviceCount=2`。

```bash
export ASCEND_HOME_PATH=/usr/local/Ascend/ascend-toolkit/8.3.RC2/aarch64-linux
export LD_LIBRARY_PATH=$ASCEND_HOME_PATH/lib64:/usr/local/Ascend/driver/lib64/driver
/root/acl_device_smoke    # 期望打印 ACL_SMOKE_OK
```

---

## 9. 存档与下一步：补「缺失的 BIOS/SMBIOS」

### 9.1 可以存档吗？

**可以，而且建议先存档再改。** 当前 `npu-smi` / ACL 终态已经闭环；下一步与 VFIO MMIO 路径正交，适合从干净基线开新分支。

建议：

```bash
cd /home/zjj/modify_firecracker/firecracker
git tag -a ascend-vfio-npu-smi-ok -m "Ascend 310P VFIO: BAR mmap + MSI-X512 + board_cfg workaround"
# 可选：git push origin main --tags
git checkout -b feat/smbios-type9
```

### 9.2 缺的到底是什么？

常说的「FC 缺 BIOS」在这里更准确是：

| 说法 | 实际缺口 |
|------|----------|
| 全套 PC BIOS / SeaBIOS | **不是**本问题范围；aarch64 FC 走 DT，不做传统 BIOS 引导 |
| **SMBIOS / DMI Type 9（System Slots）** | **是**；QEMU 有，FC guest 无 `/sys/class/dmi`，DCMI 无法做 slot↔BDF → 卡列表空 |
| 其它 Type 0/1/3… | 按 DCMI/`dmidecode` 实际读取再补；优先对齐 Type 9 |

正规目标：VMM（或极简 firmware blob）向 guest 提供 DMI 表，使：

- `dmidecode -t 9` 可见 slot，且能关联到 guest BDF（当前 `0000:00:01.0`）
- 去掉或弱化 `board_cfg.ini` workaround 后，`dcmi_get_card_num_list` 仍非空
- 理想对齐 QEMU：`card_id=2048`、双 die 两行显示（若仍差，再查 DCMI 对 BDF/桥拓扑的假设）

实现时建议对照 QEMU guest 的 `dmidecode -t 9` 字段，而不是盲目搬整份 SeaBIOS。

### 9.3 与 QEMU 仍存差异（非阻塞）

| 项 | QEMU | FC 终态 |
|----|------|---------|
| SMBIOS Type 9 | 有 | 无（`board_cfg` 绕过） |
| DCMI card_id | 2048 | 0 |
| npu-smi 双 die 两行 | 有 | 常 1 行（workaround 副作用） |
| Guest BDF | 如 `08:00.0`（桥后） | `00:01.0`（扁平 PCI） |
| 驱动版本（本环境） | 24.1.rc2 | 25.3.rc1 |

---

*文档版本：2026-09-27，含问题现场 / 排查 / 解决后日志；覆盖 cannot-emulation → BAR mmap / MSI-X512 → npu-smi + board_cfg；下一步 SMBIOS Type 9。*

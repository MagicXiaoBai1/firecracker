# NPU 直通部分

> 组会汇报 · 2026-09-30 · 内容：Firecracker microVM 沙箱 + 昇腾 310P VFIO 整卡直通

---

## 1. 下阶段交付定位

- **交付物**：设计文档 + 原型代码
- **验证对象**：Firecracker microVM 沙箱 + 昇腾 310P VFIO 整卡直通（Guest 能枚举、驱动能加载）

### 已完成部分

- 上游 Firecracker + fixed-vfio：`FC_VFIO_PCI_BDF` 启动挂载（首版无热插拔）
- 已打通：Host `vfio-pci` 绑定 → Guest `lspci` 可见 `19e5:d500` → 驱动开始 probe（但失败）

### VFIO 直通总体设计

| 方向 | 内容 |
| --- | --- |
| VFIO 直通总体设计 | 设备挂载、BAR/MMIO、DMA（IOVA=GPA）、MSI/MSI-X |
| PCI move BAR | NPU 与 virtio-blk/net 共存、MSE / pending 队列 |
| Ascend BAR Quirk | 对齐官方 QEMU patch（BAR2+0x100430 读透传、写丢弃） |
| aarch64 NISV | ENOSYS → `KVM_EXIT_ARM_NISV` → 注入 ext_dabt |

---

## 2. KVM_CAP_ARM_NISV_TO_USER：NISV 现象

日志说明：guest 以 KVM 无法 syndromize 的 store 访问 BAR2 内偏移 `0x140008`，Firecracker 收到 `KVM_EXIT_ARM_NISV` 后未处理，VM 退出。

- `fault_ipa` 的故障地址为 guest 视角下 PCI BAR 映射的 MMIO GPA：`0x4200000000`（BAR2 GPA）+ `0x140008`（BAR 内偏移）
- `esr_iss` 为 `0x45`（`0b0100_0101`），其中第 6 个比特位为 WNR，值表明这是一次**写访问**

> 备注：无法获取的信息如访问宽度（1/2/4/8 字节？）

![NISV 触发日志](pptx_assets/p2_b2e9106690.png)

日志关键行：

```text
fixed-vfio: MSIX table entry addr=0x3ffa0040:0x0 data=0x10 masked=false
fixed-vfio: BAR MMIO write (MSIX table) host_bdf=0000:01:00.0 base=0x4210000000 offset=0x1010c len=4 data=0x00000000
fixed-vfio: BAR MMIO read host_bdf=0000:01:00.0 region=2 base=0x4200000000 offset=0x100488 len=4 val=0x69727173
fixed-vfio: ascend quirk read host_bdf=0000:01:00.0 vfio_region=2 bar_slot=2 guest_base=0x4200000000 offset=0x100430 len=4 val=0x00000000
KVM_EXIT_ARM_NISV: fault_ipa=0x4200140008 esr_iss=0x45
fault_ipa hint: VFIO NPU BAR2 GPA
ARM_NISV: KVM could not decode the faulting instruction; userspace must emulate or abort the guest
Vmm is stopping. / Killing vCPU threads
RunWithApi error: MicroVM stopped with an error: GenericError
Firecracker exiting with error. exit_code=1
```

---

## 3. KVM_CAP_ARM_INJECT_EXT_DABT：对齐 QEMU 注入异常

当 reason 为 `KVM_EXIT_ARM_NISV` 时，QEMU 通过 inject ext dabt（`qemu/target/arm/kvm.c: kvm_arm_handle_dabt_nisv`）将 fault 报告给 guest。inject ext_dabt 之后，faulting 指令仍停在原处，由 guest 内核/驱动的 Data Abort handler 决定下一步（重试、报错、释放等）。

inject ext_dabt 本质上 VMM 请求 KVM 向虚拟 CPU 注入一条虚拟化层合成异常，对应 kernel 层代码为 `arch/arm64/kvm/guest.c`。

对齐 QEMU，可对 firecracker 做类似改动：`src/vmm/src/arch/aarch64/vcpu.rs`。

> 备注：`handle_arm_nisv_exit`：打日志 + 注入 ext DABT；避免 VMM 直接退出；Guest handler 决定重试/报错

### QEMU 侧处理（kvm.c）

![QEMU kvm_arm_handle_dabt_nisv](pptx_assets/p3_af3e9e143f.png)

### 内核侧实现（guest.c）

![__kvm_arm_vcpu_set_events](pptx_assets/p3_96501df7a3.png)

### Firecracker 侧对齐改动（vcpu.rs）

![firecracker vcpu.rs set_vcpu_events](pptx_assets/p3_71aa755449.png)

---

## 4. KVM_CAP_ARM_INJECT_EXT_DABT：注入后依旧 panic

注入后，guest 内核依旧 panic。

堆栈信息显示故障指令为 **BAR2 上的 STP**。

![注入后 guest 日志](pptx_assets/p4_1f1e013082.png)

![panic 堆栈](pptx_assets/p4_34edd3b0ae.png)

日志关键点：

```text
KVM_EXIT_ARM_NISV: injected ext_dabt at fault_ipa=0x4200140008; continuing guest
Internal error: synchronous external abort: ffffffff96000010 [#1] SMP
pc : devdrv_sync_non_trans_msg_send+0xc8/0x620 [drv_pcie_host]
Call trace: devdrv_sync_non_trans_msg_send -> devdrv_common_msg_send -> hdc_drv_ctrl_msg_send ...
Kernel panic - not syncing: synchronous external abort: Fatal exception
SMP: stopping secondary CPUs / Rebooting in 1 seconds..
```

---

## 5. KVM_CAP_ARM_INJECT_EXT_DABT：故障定位与修复方案

AI 分析日志后，给出相关解决方案，后续尝试**方案3**，跟 QEMU 对齐。

> 备注：
>
> - 驱动用 STP（一次写两个 32 位）打 BAR2 doorbell
> - ARM 对该指令 ISV=0，KVM 给不出普通 MMIO exit，只能 NISV
> - Firecracker 当前每笔 BAR 都 trap；注入 abort 发生在内核态，Linux 当 Fatal → panic
> - QEMU 能过，是因为 BAR 常被 mmap，STP 不进这条路径

### 故障指令分析：BAR2 上的 STP

![STP 故障分析](pptx_assets/p5_fc62edcae7.png)

| 项目 | 内容 |
| --- | --- |
| 崩溃指令 | `STP w20, w21, [x6, #8]`（括号内 `0x290154d4`） |
| x6 | `0xffff800015840000`（ioremap 后的 BAR2 窗口 VA） |
| w20/w21 | `0x40 / 0x40`（要写入的两个 32 位数） |
| 地址对应 | VA = x6 + 8 = `0xffff800015840008`；IPA = BAR2 + 0x140008 = `0x4200140008` |

STP 会写 8 字节：`0x140008` 和 `0x14000c` 各 4 字节。KVM 的 MMIO exit 只能处理单次 LDR/STR 这类能给出宽度/寄存器的指令，STP 的 ISV=0，所以只能走 NISV。

### 修复方案

Firecracker 用 mmio_bus trap 每一次 BAR 访问。驱动用 STP 写 BAR2 doorbell 时 KVM 交不出 `KVM_EXIT_MMIO`。方案1把 fault 丢回 guest，但发生在内核 workqueue 里，Linux 对内核态 external abort 的策略就是 panic。写本身没有真正落到设备上。

QEMU 对 VFIO BAR 做 mmap（`qemu/hw/vfio/region.c`），同样的 STP 会变成对映射页的普通 store，不经过这条 NISV 路径。

![修复方案](pptx_assets/p5_44c83ebae4.png)

| 方案 | 做什么 | 和这次日志的关系 |
| --- | --- | --- |
| 方案2 | 在 NISV 里 decode STP，对 `0x4200140008`/`0x14000c` 各做一次 4B VFIO write（数据都是 `0x40`），再 skip 这条指令 | 直接补上这次没完成的写 |
| 方案3 | VFIO BAR mmap，对齐 QEMU | 让 STP 不再 trap，一劳永逸 |

---

## 6. 任务排期

| 阶段 | 任务 | 主责 |
| --- | --- | --- |
| G1 | NISV：STP emulate 或 BAR mmap | VMM / 直通 |
| G2 | davinci 完整 probe，`npu-smi info` 出卡 | 直通 + 镜像 |
| G3 | Guest 内 CANN / 运行时，跑通一个小模型 | 推理 + 镜像 |
| G4 | 直通接入沙箱生命周期（创建时挂卡、销毁时还卡） | 沙箱编排 |

难点在于直通 NPU 后，能在 GuestOS 内拉起 NPU 驱动。

项目的风险在于未知的问题太多，只能遇到一个解决一个。

---

## 7. 从 ENOSYS 到 npu-smi info 可用

Guest 驱动在 probe Ascend BAR 时，会用 KVM 无法完整 decode 的访存指令（典型为 STP 等）访问设备 MMIO。问题逐个排查的阶段如下：

| 阶段 | 问题描述 | 说明 |
| --- | --- | --- |
| Checkpoint-0 | `KVM failed to emulate an instruction / -ENOSYS` | VM_EXIT 后，KVM 不能 emulate 特定指令 |
| KVM 打开 NISV | err 从 `-ENOSYS` 变成 `KVM_EXIT_ARM_NISV`，并带上 `fault_ipa = 0x4200140008` | 故障 GPA 落在 NPU BAR2（`0x4200000000` + `0x140008`） |
| 对齐 QEMU，对 NISV 异常兜底，将异常 ext_dabt 注入到 guest kernel | guest 收到 synchronous external abort → panic | 内核依旧 panic；QEMU 对 mappable BAR 优先 mmap + KVM memslot，不进 NISV |
| 将设备 BAR 空间全部 mmap 进 guest | 驱动已起，但 npu-smi 显示错误码 `-9005`（DCMI 不知道板型/卡列表） | DCMI 卡枚举缺 SMBIOS（Firecracker 相比于 QEMU 缺失） |
| 直接在 rootfs 中写入 board_cfg，指定卡类型，暂时跳过枚举 | `npu-smi info` 正常显示 | 正规做法仍是 FC 构造并暴露 SMBIOS 表，而不是长期靠这个文件 |

---

## 8. 现状

- Guest OS 版本：openeuler 22.03
- NPU 驱动版本：25.3.rc1
- Guest 内用 ACL（Ascend Computing Language）库编写数据搬运代码，验证运行时能用直通卡

### lspci：设备枚举

![lspci 输出](pptx_assets/p8_e789a5b1ff.png)

```text
[root@localhost ~]# lspci
00:00.0 Host bridge: Intel Corporation Device 0d57
00:01.0 Processing accelerators: Huawei Technologies Co., Ltd. Device d500 (rev 23)
00:02.0 Mass storage controller: Virtio: Virtio block device (rev 01)
00:03.0 Ethernet controller: Virtio: Virtio network device (rev 01)
```

### npu-smi info：驱动出卡

![npu-smi info 输出](pptx_assets/p8_f388f836ec.png)

```text
[root@localhost ~]# npu-smi info
npu-smi 25.3.rc1   Version: 25.3.rc1
NPU Chip Name    Health   Power(W)  Temp(C)  Hugepages-Usage(page)
0    310P3      OK       0.0       52       0 / 0
     Device     Bus-Id       AICore(%)  Memory-Usage(MB)
     0          0000:00:01.0  0          1647 / 44278
No running processes found in NPU 0
```

---

## 9. 后续安排

### 环境与访问

- 源码路径：`/home/zjj/modify_firecracker/firecracker`
- 启动方式：在 firecracker 目录下执行 `./start_fc_socket.sh`，在 modify_firecracker 目录下执行 `./start_fc_vm.sh`
- guest OS 账号：root，密码：`abc123!`
- ACL 测试代码在 guestos 的 `/root/acl_test` 目录下

### 后续计划（待补充）

- **给 firecracker 适配 SMBIOS**，之后不依赖于 board_cfg；安全性分析（zoujiajun）
- **规范代码**：目前仅支持单 PCIe 设备直通，在启动 firecracker 前通过参数传入需要直通设备的 BDF 码；代码中存在较多硬编码逻辑（例如指定了 NPU 设备各个 BAR 空间的基地址 `BAR4=0x4000…` / `BAR2=0x4200…` / `BAR0=0x4210…`），跟 kata 对接（yunfei）
- **在 GuestOS 内**（扩容内存/磁盘大小？）尝试安装 torch-npu，利用 NPU 运行简单 CNN 网络（chenke）
- 等等

> 备注：买台服务器；非 310P 卡

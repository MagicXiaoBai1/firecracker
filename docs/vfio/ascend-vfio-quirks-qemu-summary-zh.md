# Ascend NPU QEMU VFIO Quirks 修改总结（Firecracker 移植参考）

本文档总结昇腾官方文档 [修改 QEMU（configureg_0010）](https://www.hiascend.com/document/detail/zh/Atlas%20200I%20A2/260RC1/re/virtualmachineconfiguration/configureg_0010.html) 中 **`hw/vfio/pci-quirks.c`** 的 Ascend 专用修改，并说明在 **Firecracker VFIO 直通** 场景下需要移植什么、不需要什么。

适用产品：**Atlas 200I A2**（文档版本 26.0.RC1+）。  
本文环境示例：**19e5:d500（Ascend310P）** VFIO 直通到 aarch64 guest。

---

## 1. 背景：为什么 QEMU 要改 pci-quirks.c

Ascend 设备通过 **VFIO 直通** 给 VM 后，guest 驱动会访问 BAR 内的 **xloader-updating 寄存器**。若允许 guest **写入** 该寄存器，可能触发不期望的固件/xloader 更新行为。

QEMU 在 `hw/vfio/pci-quirks.c` 中为各 Ascend 型号增加 **BAR quirk**：

- 在指定 BAR 的指定 **offset** 上叠加一层 **intercept 区域**（4 字节）；
- **读**：转发到真实 VFIO BAR（`vfio_region_read`）；
- **写**：**拒绝**（只打 log，不写入设备），防止 guest 打开 xloader-updating。

这与 NVIDIA/ATI 等 VFIO quirk 是同一机制，属于 **hypervisor 对 passthrough MMIO 的特殊处理**，不是 guest 内核或 davinci 驱动 patch。

---

## 2. 修改位置与注册方式

### 2.1 新增代码块（`// ascend-patch begin` … `end`）

文件：`qemu-4.1.0/hw/vfio/pci-quirks.c`

主要内容：

| 类别 | 内容 |
|------|------|
| PCI ID 宏 | Huawei vendor `0x19e5`，各 Ascend device ID |
| 结构体 | `VFIOAscendBarQuirk`（vdev、bar、offset、mem） |
| 回调 | `vfio_ascend_quirk_read` / `vfio_ascend_quirk_write` |
| probe 函数 | 按型号在对应 BAR 上安装 quirk |

### 2.2 在 `vfio_bar_quirk_setup()` 中注册

```c
void vfio_bar_quirk_setup(VFIOPCIDevice *vdev, int nr)
{
    // ... 原有 ATI/NVIDIA quirk ...

    // ascend-patch begin
    vfio_probe_ascend910_a2_bar2_quirk(vdev, nr);
    vfio_probe_ascend910_bar0_quirk(vdev, nr);
    vfio_probe_ascend310p_bar2_quirk(vdev, nr);
    vfio_probe_ascend310b_bar2_quirk(vdev, nr);
    vfio_probe_ascend310_bar4_quirk(vdev, nr);
    // ascend-patch end

    // ... 其余 quirk ...
}
```

每个 BAR 初始化时调用；probe 内部若 vendor/device/BAR 号不匹配则直接 return。

---

## 3. 各 Ascend 型号 quirk 一览

| 型号 | Device ID | BAR | Offset | 大小 | 备注 |
|------|-----------|-----|--------|------|------|
| Ascend910 | `0xd801` | BAR0 | `0x80400` | 4 B | |
| Ascend910 A2 | `0xd802` | BAR2 | `0x18208430` | 4 B | |
| **Ascend310P** | **`0xd500`** | **BAR2** | **`0x100430`** | **4 B** | **200I A2 常用** |
| Ascend310P（2P） | `0xd500` | BAR2 | `0x100430 + 128MiB` | 4 B | 按 subsystem ID 区分 1P/2P |
| Ascend310B | `0xd105` | BAR2 | `0x4430` | 4 B | |
| Ascend310 | `0xd100` | BAR4 | `0x400` | 4 B | |

### 3.1 Ascend310P（19e5:d500）细节

```
PCI_VENDOR_ID_HUAWEI           0x19e5
PCI_DEVICE_ID_ASCEND310P        0xd500
ASCEND310P_XLOADER_OFFSET       0x100430
ASCEND310P_XLOADER_SIZE         4
ASCEND310P_2P_BASE              128 * 1024 * 1024   // 128 MiB
```

**1P / 2P 区分**（读 PCI subsystem ID）：

| Subsystem ID 范围 | 含义 | quirk 数量 |
|-------------------|------|------------|
| `0x100` – `0x10f` | 1P | 1 个（offset `0x100430`） |
| `0x110` – `0x11f` | 2P | 2 个（`0x100430` 与 `0x100430 + 128MiB`） |

**read 行为：**

```c
return vfio_region_read(&vdev->bars[quirk->bar].region,
                        addr + quirk->offset, size);
```

**write 行为：**

```c
qemu_log("modifying RO region is not allowed! ...");
// 不写入设备
```

注释目的：*intercept w/r to the xloader-updating register, so the vm can't enable xloader-updating*。

### 3.2 与当前 Firecracker 日志的对应

fixed-vfio 日志中已出现：

```text
BAR MMIO read ... region=2 ... offset=0x100430 ... val=0x5a6b7c8d
```

说明 guest 会访问文档所述 **BAR2 + 0x100430**。  
当前 Firecracker **直接透传** 该 MMIO；QEMU **read 透传、write 拦截**。

---

## 4. read/write 语义（移植时要一致）

```
Guest MMIO 访问 BAR[n] + offset
        │
        ▼
  offset 落在 quirk 4 字节窗口内？
        │
   是  ├─ read  → vfio_region_read（真实设备）
        └─ write → 丢弃 / 仅 log（不允许修改）
        │
   否  └─ 正常 BAR 透传
```

Quirk 区域通过 `memory_region_add_subregion_overlap(..., priority=1)` 覆盖 BAR 内子区域。

---

## 5. 同文档其它修改 — Firecracker 是否需要

| QEMU 修改 | Firecracker 是否需要 | 原因 |
|-------------|---------------------|------|
| **`pci-quirks.c` Ascend quirk** | **需要（等价实现）** | 直通 MMIO 行为应与 QEMU 一致 |
| `linux-user/syscall.c` stime | 不需要 | QEMU linux-user 模式专用 |
| `hw/vfio/common.c` 64K 页对齐 | 一般不照搬 | QEMU VFIO listener 路径；FC 走 KVM/自有 VFIO |
| `hw/arm/virt.c` extended_memmap 2048G | 不需要 | FC 自有 PCI BAR 拓扑（见 `device.rs`） |
| `configure` spice/trace | 不需要 | QEMU 构建选项 |
| libvirt / AppArmor 等 | 不需要 | host 运维步骤 |

**结论：Firecracker 场景下，文档里值得移植的核心就是 Ascend BAR quirk。**

---

## 6. Firecracker 移植建议

### 6.1 建议改动位置

- `firecracker/src/vmm/src/devices/pci/vfio/`（fixed-vfio MMIO read/write 路径）
- 在 BAR dispatch 前增加：**vendor/device/BAR/offset** 判断

### 6.2 310P 最小实现（P0）

1. 匹配 `vendor=0x19e5`、`device=0xd500`、**BAR2**
2. offset 命中 **`0x100430`**（及 2P 时 **`0x100430 + 128MiB`**）且 len≤4：
   - **read**：走现有 VFIO region read
   - **write**：log + **no-op**（不写 host VFIO）
3. 可选：读 subsystem ID 区分 1P/2P

### 6.3 与 ENOSYS 崩溃的关系

| 现象 | quirk 能否解决 |
|------|----------------|
| BAR2 `0x100430` 读写、MSIX 配置 | 行为与 QEMU 对齐，减少 init 差异 |
| irq vec=2 后 FAR=`0xaaa…` + KVM memslot ENOSYS | **多半不能单独解决**；需继续查 uio/中断/KVM |

Quirk 是 **QEMU 兼容性 / 官方要求**；memslot 问题是 **另一条线**，两者都要做。

### 6.4 当前状态

截至文档编写时，Firecracker 代码中 **尚未实现** 上述 Ascend quirk（无 `0x100430` / `xloader` 相关逻辑）。

---

## 7. 参考资料

- **Firecracker 实现设计**：[ascend-vfio-quirks-design-zh.md](./ascend-vfio-quirks-design-zh.md)
- 昇腾文档：[修改 QEMU - configureg_0010](https://www.hiascend.com/document/detail/zh/Atlas%200I%20A2/260RC1/re/virtualmachineconfiguration/configureg_0010.html)
- 上游参考文件：`qemu-4.1.0/hw/vfio/pci-quirks.c`（`// ascend-patch begin` 段）
- 本项目相关：`firecracker/docs/vfio-msix-implementation-zh.md`、`firecracker/docs/pci-bar-move-implementation-zh.md`

---

## 8. 快速检查清单

- [ ] guest 读 `BAR2+0x100430` 返回值与 QEMU 一致
- [ ] guest **写** `BAR2+0x100430` 被拦截（host 侧可见 log）
- [ ] 2P 卡第二窗口 `0x100430+128MiB`（如适用）
- [ ] MSIX / 中断 / 驱动加载仍正常
- [ ] ENOSYS 是否仍存在（单独跟踪 KVM fault_ipa）

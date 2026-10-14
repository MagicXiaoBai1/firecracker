# VFIO NPU 直通：文档索引与仓库策略

> 本目录是 Firecracker + 昇腾 310P VFIO 直通产品化项目的**文档权威归宿**（canonical）。
> M0（基线整合，2026-10-14）建立。上游文档见 `docs/`（英文），本目录仅收录本项目相关文档。

## 1. 文档索引

### 1.1 项目级设计（活跃，后续在此演进）

| 文档 | 内容 | 状态 |
|---|---|---|
| [npu-vfio-productization-design-zh.md](npu-vfio-productization-design-zh.md) | **产品化概要设计 HLD v1.0**：三大目标（API 化 / 模块内聚 / Kata 对接）、总体架构、里程碑 M0–M6、ADR D1–D11、9 张设计图 | **canonical**（顶层 `vm_vfio/docs/` 的副本为工作稿） |
| [kata-fc-vfio-integration-design-zh.md](kata-fc-vfio-integration-design-zh.md) | **Kata 对接专题详设 v1.0**：三处断点 D-1/D-2/D-3、KD 决策、KP-1..7 PR 组织、M5.0–M5.4 子计划 | **canonical** |
| [vfio-npu-passthrough-report.md](vfio-npu-passthrough-report.md) | fork（本仓库）模块化重构现状报告：VfioCommon / MemoryRecognizer / DmaEngine / Vfio trait 骨架与 stub 清单 | fork 侧现状快照 |

另见 fork 自身的模块化设计演进系列：`docs/TODO/`（中文，含 MEMORY_RECOGNIZER_GUIDE.md、VFIO_COMMON_INTEGRATION.md 等）。

### 1.2 黄金参考（golden reference，**已冻结**，来源 `firecracker_vfio_worked`）

worked 仓库端到端调通（lspci → npu-smi → ACL）过程中的全部设计/排障文档，M1 移植时的语义对照基准：

| 文档 | 内容 |
|---|---|
| [firecracker基于vfio的npu_pcie直通调通记录.md](firecracker基于vfio的npu_pcie直通调通记录.md) | **调通全过程记录**：Checkpoint-0（NISV）→ ext_dabt 注入 → STP/mmap 方案 → board_cfg 出卡 → BAR 稳定性 |
| [vfio-pci-demo-design-zh.md](vfio-pci-demo-design-zh.md) | PCI 枚举打通（lspci 可见）设计 |
| [vfio-pci-initial-support.md](vfio-pci-initial-support.md) | initial support bring-up 设计 |
| [vfio-pci-minimal-demo-implementation-zh.md](vfio-pci-minimal-demo-implementation-zh.md) | 最小 demo 实现记录 |
| [vfio-msix-implementation-zh.md](vfio-msix-implementation-zh.md) | MSI-X 闭环设计（512 向量、eventfd/irqfd） |
| [pci-bar-move-implementation-zh.md](pci-bar-move-implementation-zh.md) | BAR 重编程 v2.3（MSE 门控 + pending + commit_all_bars + Overlap 摘除/重试）——M3 的直接蓝本 |
| [pci-bar-move-ppt-zh.md](pci-bar-move-ppt-zh.md) | BAR 重编程汇报版 |
| [cloud-hypervisor-pci-bar-reprogramming-zh.md](cloud-hypervisor-pci-bar-reprogramming-zh.md) | cloud-hypervisor BAR 迁移机制调研 |
| [ascend-vfio-quirks-design-zh.md](ascend-vfio-quirks-design-zh.md) | Ascend 310P quirk 设计（xloader 寄存器读透传/写丢弃） |
| [ascend-vfio-quirks-qemu-summary-zh.md](ascend-vfio-quirks-qemu-summary-zh.md) | 昇腾官方 QEMU patch 对照总结 |
| [ascend-vfio-quirk-enosys-nisv-journey-zh.md](ascend-vfio-quirk-enosys-nisv-journey-zh.md) | quirk / ENOSYS / NISV 排障历程 |
| [ascend-vfio-nisv-qemu-fc-alignment-zh.md](ascend-vfio-nisv-qemu-fc-alignment-zh.md) | NISV 处理与 BAR mmap 对齐 QEMU 的论证（方案 3 的依据） |
| [ascend-vfio-cannot-emulation-to-npu-smi-zh.md](ascend-vfio-cannot-emulation-to-npu-smi-zh.md) | 从"不可仿真"到 npu-smi 出卡的完整链路 |

> 迁移说明（2026-10-14，M0）：以上文档自 `firecracker_vfio_worked/docs/` 与顶层 `vm_vfio/`、`vm_vfio/docs/` 拷入。
> worked/docs 中与上游同名的英文文档属于其**旧基线**（未随迁）；`fuzzing.md` 为上游旧文档、新基线已删除，亦未随迁。

## 2. 仓库与分支策略

### 2.1 两库定位（HLD §2.2 决策 D1）

| 仓库 | 角色 | 状态 |
|---|---|---|
| `firecracker`（本仓库） | **开发基线**：新基线（v1.15/1.16 era）+ 模块化骨架（`devices/vfio/pcie/` 9 文件、多设备、`Vfio` trait 可 mock） | 活跃开发 |
| `firecracker_vfio_worked` | **黄金参考**：已验证的数据面/中断/quirk/NISV/BAR 迁移实现（lspci/npu-smi/ACL 全通） | **冻结**（git 仓库 + tag `golden/v1.0`，不再演进） |

worked 的价值 = M1–M3 移植期间的语义对照（日志关键字、quirk 行为、MSE 时序）与验收基准，**不承接任何新功能**。

### 2.2 本仓库分支模型

```text
main          跟踪上游 firecracker-microvm/firecracker，仅同步、不开发
 └── vfio/series   项目开发主线（= 原 feat-vfio 末端 683c9a3a6，模块化重构 backup 提交）
      ├── PR-0    基线：文档迁入 + CI            ← M0（本提交）
      ├── PR-1a..1e 数据面移植                    ← M1
      ├── PR-2    API 全链路 + 去硬编码           ← M2
      ├── PR-3    BAR 重编程 + 多设备             ← M3
      └── PR-4    jailer + seccomp                ← M4
```

- **PR 粒度**：每个 PR 独立可编译、带单测、不动无关文件（低侵入的可执行证明，HLD §7.2）。
- **远端**：`origin` = MagicXiaoBai1/firecracker（推送）；`upstream` = firecracker-microvm/firecracker（只读，同步 main 用）。
- **rebase 策略**（风险 R6 对冲）：`vfio/series` 与 main 已分叉（基线落后上游约 98 提交）；M1–M3 期间**不主动 rebase**，保持移植对照稳定；M3 结束后评估一次性 rebase（PR 粒度小、侵入面清单明确，冲突面可控）。
- **历史分支处置**：远端 `feat-vfio`（= vfio/series 前身）、`my`、`copilot/*` 保留为历史归档，不再推进。
- **kata-containers**（顶层 `vm_vfio/kata-containers/`）：M5 才动，届时按 KD 决策另立分支策略。

### 2.3 归档与同步纪律

- worked：冻结后**只读**；若发现 worked 代码与本文档描述不符，以代码为准并回改文档。
- 本目录为文档 canonical；顶层 `vm_vfio/docs/` 不再演进（避免双源）。
- 移植期间每完成一个 PR，在对应 worked 设计文档头部追加一行"已移植至 PR-x"标记，保持对照关系可追溯。

## 3. CI（Linux build + test）

`.github/workflows/ci.yml`：push `main`/`vfio/series` 及任意 PR 触发，两个 job：

| Job | 内容 | 环境 |
|---|---|---|
| `unit-tests`（x86_64，**root**） | `cargo test --workspace --locked --no-fail-fast` | apt: `libseccomp-dev libclang-dev cmake`；tap 创建（CAP_NET_ADMIN）、jailer（mknod/chroot）、KVM 测试都需要 root（对齐上游 CI 容器行为） |
| `check-aarch64`（交叉） | `cargo check --workspace --target aarch64-unknown-linux-gnu --locked` | apt: `libclang-dev cmake gcc/g++-aarch64-linux-gnu` + `CC/CXX/AR_aarch64_unknown_linux_gnu` 环境变量 |

**回归门禁**（HLD §6.3）：不配置任何 vfio 设备时既有测试必须 100% 通过——CI 即该门禁的执行点。

### 3.1 构建环境备忘（2026-10-14 实测，WSL2 Ubuntu-22.04 验证）

本基线（v1.15 era）的依赖树**不再是纯 Rust**，构建环境要求（CI 与 NPU 服务器同理）：

- `libseccomp-dev`：`seccompiler/build.rs` 链接 `-lseccomp`（上游 dev-machine-setup 本就要求）；
- `libclang-dev`：`userfaultfd-sys`（vmm 的 uffd 快照支持）build script 走 bindgen；
- `cmake`：`aws-lc-sys`（vmm 的 `aws-lc-rs`，用于 MMDS token AES-GCM / virtio-rng / vmgenid —— **上游真实依赖，不是 fork 引入**，勿删）构建 aws-lc C 代码；仓库 `.cargo/config.toml` 已内置 `AWS_LC_SYS_NO_JITTER_ENTROPY=1` 等必要配置；
- musl 目标额外需要 musl C 工具链（`userfaultfd-sys`/`aws-lc-sys` 的 cc 检测）与 musl 版 libseccomp —— **musl 静态产物按上游 devtool 容器流程在 NPU 服务器上构建**，不进 CI；
- aarch64 gnu 交叉**完整 build**（非 check）额外需要：`CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc` 与 arm64 版 libseccomp（multiarch：`dpkg --add-architecture arm64` + ports.ubuntu.com 源 + `libseccomp-dev:arm64`）——已本地验证可行，因步骤较繁，CI 用 check 替代；
- **直通驱动（M1 过渡态）**：`FC_VFIO_PCI_BDF` 环境变量（如 `0000:ba:02.0`）——设置时 builder 执行 attach（对齐 worked 的驱动方式，仅 NPU 服务器上使用）；未设置时跳过 attach，常规启动与全部测试走此路径。M2 由 `PUT /vfio-devices/{id}` API 取代（HLD §3.2）。
- **rustc 1.93.0 诊断渲染 bug（重要）**：当 lint 警告的 span 覆盖含多字节字符（中文注释/文档）的源码行时，渲染下划线标注直接 panic（`StyledBuffer::replace: slice index starts at X but ends at Y`，unicode 字节偏移类已知 bug，1.99 已不复现）。后果是**任何警告都可能变成 ICE 并遮住真实错误**（基线中 vmm 的 E0277 编译错误正是被它藏住的）。对策：保持 vmm **零警告**（M0 已清零并进 CI 门禁）；开发中若遇 ICE，先 `cargo +stable check -p vmm`（新版渲染安全）看真实诊断，修掉警告后回到 pinned 1.93 重编。
- **测试须以 root 运行**（对齐上游 CI 容器行为）：tap 创建需要 CAP_NET_ADMIN、jailer 测试需要 mknod/chroot、部分测试需要 /dev/kvm。WSL 本地另有两个已知环境限制：无 `/sys/devices/virtual/dmi/id`（cpu-template-helper 的 2 个 DMI 测试挂）、嵌套 KVM 下测试内核不能干净退出（`test_build_microvm`/`test_build_and_boot_microvm` 挂 —— **merge-base 纯上游对照复现同样失败**，非 fork 回归；CI 的 GitHub runner 上需首轮验证，必要时再处理）。

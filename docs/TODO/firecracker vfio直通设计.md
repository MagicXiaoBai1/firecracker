# firecracker vfio直通设计

## keyPoints

1. Guest的设备IO相关内存
    1. MMIO：Guest如何访问设备的配置空间和bar空间
        1. 权限判断：设备IOVA有些可以直通给Guest、有些要内存模拟，有些要丢弃Guest访问
        2. GPA分配：设备IO相关的空间需要在设备启动前预分配好，当Guest写入Bar 寄存器时还要动态调整
        3. 直通配置：尽量在Guest访问可直通的MMIO的GPA前调用KVM和VFIO配置好GPA的直通
        4. GPA读写VM Exit配置：像firecracker中的PciBus注册VfioPcieDevice，当Guest因读写MMIO GPA发生VM Exit时要能跳到该VfioPcieDevice的成员函数上
    2. DMA：设备如何访问Guest内存
        1. 对于每一块Guest内存都需要调用vfio_container.vfio_dma_map
2. Guest的设备msix中断
    1. 相关GPA读写拦截：当Guest读写：msix控制寄存器、msix向量表和PBA时拦截
    2. msix中断链接GSI：当Guest写入msix向量表时将Guest的Msix中断与对应GSI对应起来
    3. 设备中断连接GSI：在Guest启动前，先调VFIO将设备中断和eventFd链接，再调KVM将eventFd和GSI链接
3. VFIO设备框架：要能承载上面的keyPoints、外部会init和free

## 时序梳理keyPoints

1. Guest启动前
    1. 入参检查时：
    2. 常规cpu，内存资源分配完，开始分配vfio设备资源时：
        1. 权限判断
        2. GPA分配
        3. 直通配置
        4. GPA读写VM Exit配置
        5. 对于每一块Guest内存都需要调用vfio_container.vfio_dma_map
2. Guest启动后
    1. VM Exit发生时
        1. 权限判断
        2. GPA分配
        3. 直通配置
        4. GPA读写VM Exit配置
        5. 对于每一块Guest内存都需要调用vfio_container.vfio_dma_map
3. Guest关机时：
    1. 释放资源

## 资源持有与责任梳理keyPoints

全部资源与相关keyPoints：
- VFIO
  - VfioDeviceFd：MMIO、DMA、Msix
  - VfioGroupFd
  - VfioContainerFd：DMA
- Vm（Arc<Vm>可在各种层级的对象中持有）
  - PciBus（由Vm持有，在Guest启动前可随意操作）：MMIO、VFIO设备框架、DMA
  - DeviceMgr（由Vm持有，在Guest启动前可随意操作）：VFIO设备框架
  - DeviceMgr.PciDevices（由Vm持有，在Guest启动前可随意操作）：持有VfioPciDevice
- MemoryAllocator（通过Arc<Vm>加锁获得无需持有）：MMIO、DMA
资源持有与利用分析：
- VfioContainerFd由PciDevices持有，统一管理各种设备的DMA
- VfioGroupFd和VfioDeviceFd由该设备对象持有并与该对象的各个子对象共享
- Vm下资源全局共享
PciBus、DeviceMgr、PciDevices这三者都需要VfioPciDevice实现某些接口如PciDevice等。

## VFIO设备与virtIO设备的不同（Firecracker中已有virtIO设备的支持）

virtIO设备只需响应Guest对设备请求就可以了，几乎所有keyPoints可以在响应中完成
VFIO设备不一样，除了响应Guest对设备请求，还要监控Guest对设备资源（mmio、msix）状态的修改，当状态被修改时要调用VFIO、KVM配置相关资源的直通访问。

## MVP

- 先不考虑Guest DRAM内存热插拔
- 先不考虑快照

## keyPoints2structure

### Guest的设备IO相关内存    
  1. MMIO：Guest如何访问设备的配置空间和bar空间
    1. 权限判断：设备IOVA有些可以直通给Guest、有些要内存模拟，有些要丢弃Guest访问
    2. GPA分配：设备IO相关的空间需要在设备启动前预分配好，当Guest写入Bar 寄存器时还要动态调整（这一点和前一点放在一起：）
    3. 直通配置：尽量在Guest访问可直通的MMIO的GPA前调用KVM和VFIO配置好GPA的直通（在`src/vmm/src/devices/vfio`中创建一个mmio_passthrough_utils，工具接收参数`vfioContainerFd`、`iova`、`len`、 `vfioDeviceMemoryFd`）
    4. GPA读写VM Exit配置：像firecracker中的PciBus注册VfioPcieDevice，当Guest因读写MMIO GPA发生VM Exit时要能跳到该VfioPcieDevice的成员函数上
  2. DMA：设备如何访问Guest内存
    1. 对于每一块Guest内存都需要调用vfio_container.vfio_dma_map：用一个钩子，尝试挂载到所有Guest内存变化点（目前来说有virtio-memory、MMIO空间挂载到PciBus，难点：如何监控所有Guest内存），在@src/vmm/src/device_manager/pci_mngr.rs:52-60中添加dma_engine对象，所有vfio直通设备讲其`vfioContainerFd`注册到dma_engine中，需要DMA时统一调用dma_engine的vfio_dma_map方法
其中只有GPA读写VM Exit配置依赖上次对象如PciBus
### 需要模块or类（这块不太考虑GPA读写VM Exit配置）：
- 对象内存识别模块
  - 枚举体指明所有iova类型：一般 ECAM、BAR 寄存器、Msix控制寄存器、丢弃、仅内存模拟读写、MsixVector、PBA
  - 输入vfioDeviceFd输出所有iova段和段类型
  - 输入GPA访问请求（外部请求对ECAM是reg_idx: usize,offset: u64,data: &[u8]，对bar是base: u64, offset: u64, data: &mut [u8]）：输出这个GPA属于那个段，段的类型是啥，对BAR空间还要输出属于那个BAR Region，offest，拆开跨段的访问（或直接报错）
  - 钩子用于设备监控BAR空间变化
- VfioCommon模块
  - 调用对象内存识别模块对不同的iova类型挂载不同的“处理函数”
  - 创建并持有包含“处理函数”的对象
  - 持有VFIO相关的文件描述符Arc<Mutex<>>防止并发（其他子对象禁止持有，需要用的话就用入参传递,要是传入Mutex对象的话就只能传入一个，要想传入多个最好在VfioCommon中获取锁，避免死锁）
- VfioCommon的子模块
  - 处理仅内存模拟读写的
  - 处理Msix的（这个对象负责所有Msix直通）
  - 处理BAR寄存器读写的
- MmioMapEngine（VfioCommon持有）
  - Guest启动后任何配置kvm-vfio直通的地方只能用该对象，需要提供iova(GPA)、bar index、offest、len
  - 在这个对象中可以在做一次安全检查（目前先不做）
- VfioPciDevice（顶级对象）
  - 持有VfioCommon
  - 实现各种接口，向上对接
  - 做一些设备特异性的东西（如Ascend310P BAR2被限制写入的 XLoader 地址（偏移）有0x100430和0x8100430）
#### GPA读写VM Exit配置需要模块or类
- Vm实现DeviceRelocation接口：主要是修改PciBus对象的状态
- VfioPciDevice实现PciDevice接口：
  - detect_bar_reprogramming
  - move_bar：主要是修改直通状态
- 修改vm的 DeviceRelocation 借口实现：
  - move_bar时判断是否为vfio设备的bar空间，如果是就调用self的VmCommon的resource_allocator来free或申请mmio64地址。去调用VfioPciDevice的move_bar
#### Guest的设备msix中断
  1. 相关GPA读写拦截：当Guest读写：msix控制寄存器、msix向量表和PBA时拦截
  2. msix中断链接GSI：当Guest写入msix向量表时将Guest的Msix中断与对应GSI对应起来
  3. 设备中断连接GSI：在Guest启动前，先调VFIO将设备中断和eventFd链接，再调KVM将eventFd和GSI链接
### VFIO设备框架：要能承载上面的keyPoints、外部会init和free
初始化分为两部分：
1. 创建各种对象
2. 配置一些资源的直通

# 备注
目前guest的iova和GPA是一对一对应关系，不用任何地址转换

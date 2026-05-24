use std::sync::Arc;
use crate::vstate::memory::GuestMemoryMmap;
use vfio_ioctls::VfioContainer;
use vm_memory::GuestAddress;

/// DmaEngine负责将guest memory挂载到vfio container
///
/// TODO: 目前为框架 stub；实际实现应调用 `vfio_container.vfio_dma_map` 并在失败时回滚。
pub struct DmaEngine {
    pub container: Option<Arc<VfioContainer>>,
    // mapped regions: (gpa_start, len)
    pub mapped_guest_regions: Vec<(u64, u64)>,
}

impl DmaEngine {
    pub fn new() -> Self {
        Self {
            container: None,
            mapped_guest_regions: Vec::new(),
        }
    }

    /// 注册一个 vfio container，用于后续的 dma_map 操作
    pub fn register_container(&mut self, container: Arc<VfioContainer>) {
        // TODO: check if already registered and handle accordingly
        self.container = Some(container);
    }

    /// 对当前 guest memory 的所有 region 执行 vfio_dma_map
    pub fn map_guest_memory(&mut self, gpa: GuestAddress, len: u64) -> Result<(), ()> {
        // TODO: 遍历 guest memory region，并对每一段调用 container.vfio_dma_map
        // 如果某段映射失败，应回滚之前的映射并返回错误。
        Err(())
    }
    pub fn unmap_guest_memory(&mut self, gpa: GuestAddress, len: u64) -> Result<(), ()> {
        Err(())
    }

    /// 取消所有已映射的 DMA 区域
    pub fn unmap_all(&mut self) -> Result<(), ()> {
        // TODO: 调用 vfio_dma_unmap，并清空 mapped_guest_regions
        self.mapped_guest_regions.clear();
        Ok(())
    }
}

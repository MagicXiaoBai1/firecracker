use std::sync::Arc;
use crate::vstate::memory::GuestMemoryMmap;
use log::debug;
use vfio_ioctls::VfioContainer;
use vm_memory::GuestAddress;

/// DmaEngine负责将guest memory挂载到vfio container
///
/// TODO: 目前为框架 stub；实际实现应调用 `vfio_container.vfio_dma_map` 并在失败时回滚。
pub struct DmaEngine {
    pub containers: Vec<Arc<VfioContainer>>,
    // mapped regions: (gpa_start, len)
    pub mapped_guest_regions: Vec<(u64, u64)>,  // TODO 这个对象要加锁，所有成员函数要从 &mut self 改为 &self
}

impl DmaEngine {
    pub fn new() -> Self {
        Self {
            containers: Vec::new(),
            mapped_guest_regions: Vec::new(),
        }
    }

    /// 注册一个 vfio container，用于后续的 dma_map 操作
    pub fn register_container(&mut self, container: Arc<VfioContainer>) {
        // TODO: check if already registered and handle accordingly
        self.containers.push(container);
    }

    /// 对当前 guest memory 的所有 region 执行 vfio_dma_map
    pub fn map_guest_memory(&mut self, gpa: GuestAddress, len: u64, usr_addr: u64) -> Result<(), ()> {
        // 1. 检查是否重复映射
        for &(start, l) in &self.mapped_guest_regions {
            if start == gpa.0 && l == len {
                return Err(());
            }
        }

        self.mapped_guest_regions.push((gpa.0, len));
        
        for container in &self.containers {
            // vfio_dma_map is unsound and ought to be marked as unsafe
            #[allow(unused_unsafe)]
            // SAFETY: GuestMemoryMmap guarantees that region points
            // to len bytes of valid memory starting at as_ptr()
            // that will only be freed with munmap().
            unsafe {
                    container.vfio_dma_map(
                        gpa.0,
                        len,
                        usr_addr as u64,
                    )
                }.unwrap_or_else(|e| {
                debug!("VFIO 设备DMA 映射失败: {:?}", e);
                panic!("初始化终止")
                });
        }
        Ok(())
    }
    pub fn unmap_guest_memory(&mut self, gpa: GuestAddress, len: u64, usr_addr: u64) -> Result<(), ()> {
        self.mapped_guest_regions.retain(|&(start, region_len)| {
            !(start == gpa.0 && region_len == len)
        });
        for container in &self.containers {
            // vfio_dma_map is unsound and ought to be marked as unsafe
            #[allow(unused_unsafe)]
            // SAFETY: GuestMemoryMmap guarantees that region points
            // to len bytes of valid memory starting at as_ptr()
            // that will only be freed with munmap().
            unsafe {
                    container.vfio_dma_unmap(
                        gpa.0,
                        len,
                    )
                }.unwrap_or_else(|e| {
                debug!("VFIO 设备DMA 映射失败: {:?}", e);
                panic!("初始化终止")
                });
        }
        Ok(())
    }

    /// 取消所有已映射的 DMA 区域
    pub fn unmap_all(&mut self) -> Result<(), ()> {
        for &(gpa, len) in &self.mapped_guest_regions {
            for container in &self.containers {
                // vfio_dma_map is unsound and ought to be marked as unsafe
                #[allow(unused_unsafe)]
                // SAFETY: GuestMemoryMmap guarantees that region points
                // to len bytes of valid memory starting at as_ptr()
                // that will only be freed with munmap().
                unsafe {
                        container.vfio_dma_unmap(
                            gpa,
                            len,
                        )
                    }.unwrap_or_else(|e| {
                    debug!("VFIO 设备DMA 取消映射失败: {:?}", e);
                    panic!("初始化终止")
                    });
            }
        }
        self.mapped_guest_regions.clear();
        Ok(())
    }
}

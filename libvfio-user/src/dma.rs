use std::fmt::{Debug, Formatter};
use std::io::Error;
use std::mem::size_of;
use std::ptr::null_mut;
use std::slice::{from_raw_parts, from_raw_parts_mut};

use anyhow::{anyhow, ensure, Result};

use libvfio_user_sys::*;

use crate::DeviceContext;

/// Mapping to a certain guest range, may span multiple mapped regions
// Debug implemented manually to inspect sgl entries
pub struct DmaMapping {
    // Vfu context and sgls are needed for vfu_sgl_put call when mapping is dropped
    ctx: *mut vfu_ctx_t,
    sgl_buffer: Vec<u8>,
    mapped_regions: Vec<iovec>,
}

impl DmaMapping {
    pub fn dma(&self, region_index: usize) -> &[u8] {
        let region = self.mapped_regions[region_index];
        unsafe { from_raw_parts(region.iov_base as *const u8, region.iov_len) }
    }

    pub fn dma_mut(&mut self, region_index: usize) -> &mut [u8] {
        let region = self.mapped_regions[region_index];
        unsafe { from_raw_parts_mut(region.iov_base as *mut u8, region.iov_len) }
        // We do not need to call vfu_sgl_mark_dirty since we call vfu_sgl_put on drop
    }

    pub fn region_length(&self, region_index: usize) -> usize {
        self.mapped_regions[region_index].iov_len
    }

    pub fn total_length(&self) -> usize {
        self.mapped_regions.iter().map(|x| x.iov_len).sum()
    }

    pub fn base_addresses(&self) -> Vec<usize> {
        self.mapped_regions
            .iter()
            .map(|iov| iov.iov_base as usize)
            .collect()
    }

    pub fn lengths(&self) -> Vec<usize> {
        self.mapped_regions.iter().map(|iov| iov.iov_len).collect()
    }
}

impl Drop for DmaMapping {
    fn drop(&mut self) {
        unsafe {
            vfu_sgl_put(
                self.ctx,
                self.sgl_buffer.as_mut_ptr() as *mut dma_sg_t,
                self.mapped_regions.as_mut_ptr(), // Parameter unused inside vfu_sgl_put
                self.mapped_regions.len(),
            );
        }
    }
}

impl DeviceContext {
    pub fn map_range(
        &mut self, dma_addr: usize, len: usize, max_regions: usize, read: bool, write: bool,
    ) -> Result<DmaMapping> {
        ensure!(
            len > 0,
            "Mapping should not be empty. Skip calling if len == 0."
        );
        ensure!(max_regions > 0, "At least 1 region is required.");
        ensure!(
            !self.dma_regions.is_empty(),
            "No mappable regions registered, have you called .setup_dma(true) during configuration?"
        );

        let mut prot = 0;
        if read {
            prot |= 0x1;
        }
        if write {
            prot |= 0x2;
        }

        unsafe {
            // 1. Gather SGL

            // dma_sg_t size is only indirectly available, allocate a buffer and do casts instead
            let mut sgl_buffer = vec![0u8; dma_sg_size() * max_regions];

            let ret = vfu_addr_to_sgl(
                self.vfu_ctx,
                dma_addr as vfu_dma_addr_t,
                len,
                sgl_buffer.as_mut_ptr() as *mut dma_sg_t,
                max_regions,
                prot,
            );

            match ret {
                0 => {
                    return Err(anyhow!(
                        "Failed to populate sgl entries: no entries created"
                    ));
                }
                -1 => {
                    let err = Error::last_os_error();
                    return Err(anyhow!("Failed to populate sgl entries: {}", err));
                }
                x if x < -1 => {
                    return Err(anyhow!(
                        "Failed to populate sgl entries, not enough sg entries available, \
                        required={}, available={}",
                        -ret - 1,
                        max_regions
                    ));
                }
                _ => {}
            }
            let region_count = ret as usize;

            // Ensure all sgl entries are mappable
            for (i, sg) in sgl_buffer.chunks_exact_mut(dma_sg_size()).enumerate() {
                ensure!(
                    vfu_sg_is_mappable(self.vfu_ctx, sg.as_mut_ptr() as *mut dma_sg_t),
                    "Sg entry {} is not mappable",
                    i
                );
            }

            // 2. Collect iovec to each region

            let mut iovs: Vec<iovec> = vec![
                iovec {
                    iov_base: null_mut(),
                    iov_len: 0
                };
                region_count
            ];
            let ret = vfu_sgl_get(
                self.vfu_ctx,
                sgl_buffer.as_mut_ptr() as *mut dma_sg_t,
                iovs.as_mut_ptr(),
                region_count,
                0,
            );
            if ret != 0 {
                let err = Error::last_os_error();
                return Err(anyhow!("Failed to populate iovec array: {}", err));
            }

            Ok(DmaMapping {
                ctx: self.vfu_ctx,
                sgl_buffer,
                mapped_regions: iovs,
            })
        }
    }
}

// Replica struct of dma_sg in libvfio-user/lib/dma.h
// dma_sg is not directly exposed, only its size via dma_sg_size()
// I assume this is because it may change in the future.
// Therefore this replica should only be used for debugging purposes.
#[repr(C)]
#[derive(Debug)]
struct DmaSgDebug {
    dma_addr: vfu_dma_addr_t,
    region: i32,
    length: u64,
    offset: u64,
    writeable: bool,
}

impl DmaSgDebug {
    unsafe fn try_from_ptr(sg: *const dma_sg_t) -> Option<DmaSgDebug> {
        if sg.is_null() {
            return None;
        }

        // If sizes don't match the struct probably changed
        if dma_sg_size() != size_of::<DmaSgDebug>() {
            return None;
        }

        let sg_debug = (sg as *const DmaSgDebug).read();
        Some(sg_debug)
    }
}

impl Debug for DmaMapping {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        unsafe {
            // Try to use list of DmaSgDebug instead of just printing the sgl_buffer byte vec
            let mut sgl = vec![];
            let mut sgl_formatted = true;

            for sg_chunk in self.sgl_buffer.chunks_exact(dma_sg_size()) {
                match DmaSgDebug::try_from_ptr(sg_chunk.as_ptr() as *const dma_sg_t) {
                    Some(sg_debug) => {
                        sgl.push(sg_debug);
                    }
                    None => {
                        sgl_formatted = false;
                        break;
                    }
                }
            }

            let mut format = f.debug_struct("DmaMapping");
            if sgl_formatted {
                format.field("sgl", &sgl)
            } else {
                format.field("sgl_buffer", &self.sgl_buffer)
            }
            .field("mapped_regions", &self.mapped_regions)
            .finish()
        }
    }
}

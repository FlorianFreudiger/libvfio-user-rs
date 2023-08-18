use std::fmt::{Debug, Formatter};
use std::io::Error;
use std::mem::size_of;
use std::os::raw::c_void;
use std::ptr::null_mut;
use std::slice::{from_raw_parts, from_raw_parts_mut};

use anyhow::{anyhow, ensure, Result};

use libvfio_user_sys::*;

use crate::DeviceContext;

// Debug implemented manually to inspect sgl entries
pub struct DmaRange {
    // Vfu context and sgl_buffer is needed for vfu_sg_is_mappable and vfu_sgl_put call
    // when DmaMapping is dropped
    ctx: *mut vfu_ctx_t,
    sgl_buffer: Vec<u8>,

    size: usize,
    region_count: usize,
}

impl DmaRange {
    pub fn size(&self) -> usize {
        self.size
    }

    pub fn region_count(&self) -> usize {
        self.region_count
    }

    pub fn read(&mut self) -> Result<Vec<u8>> {
        let mut buffer = vec![0u8; self.size];

        let ret = unsafe {
            vfu_sgl_read(
                self.ctx,
                self.sgl_buffer.as_mut_ptr() as *mut dma_sg_t,
                1,
                buffer.as_mut_ptr() as *mut c_void,
            )
        };

        if ret != 0 {
            let err = Error::last_os_error();
            return Err(anyhow!("Failed to read from dma range: {}", err));
        }

        Ok(buffer)
    }

    pub fn write(&mut self, buffer: &[u8]) -> Result<()> {
        ensure!(
            buffer.len() == self.size,
            "Must write exact size of dma range"
        );

        let ret = unsafe {
            vfu_sgl_write(
                self.ctx,
                self.sgl_buffer.as_mut_ptr() as *mut dma_sg_t,
                1,
                // Intentional cast from const ptr to mut ptr, contents should not change
                buffer.as_ptr() as *mut c_void,
            )
        };

        if ret != 0 {
            let err = Error::last_os_error();
            return Err(anyhow!("Failed to write to dma range: {}", err));
        }

        Ok(())
    }

    pub fn is_mappable(&self) -> bool {
        // Ensure all sgl entries are mappable
        unsafe {
            self.sgl_buffer
                .chunks_exact(dma_sg_size())
                // Cast from const ptr to mut ptr, should be fine since vfu_sg_is_mappable does not
                // affect contents (parameter mut because of bindings)
                .map(|sg| vfu_sg_is_mappable(self.ctx, sg.as_ptr() as *mut dma_sg_t))
                .all(|b| b)
        }
    }

    pub fn into_mapping(mut self) -> Result<DmaMapping> {
        ensure!(self.is_mappable(), "Dma range is not mappable.");

        let mut iovs: Vec<iovec> = vec![
            iovec {
                iov_base: null_mut(),
                iov_len: 0
            };
            self.region_count
        ];

        let ret = unsafe {
            vfu_sgl_get(
                self.ctx,
                self.sgl_buffer.as_mut_ptr() as *mut dma_sg_t,
                iovs.as_mut_ptr(),
                self.region_count,
                0,
            )
        };

        if ret != 0 {
            let err = Error::last_os_error();
            return Err(anyhow!("Failed to populate iovec array: {}", err));
        }

        Ok(DmaMapping {
            range: self,
            mapped_regions: iovs,
        })
    }
}

/// Mapping to a certain guest range, may span multiple mapped regions
#[derive(Debug)]
pub struct DmaMapping {
    range: DmaRange,
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
                self.range.ctx,
                self.range.sgl_buffer.as_mut_ptr() as *mut dma_sg_t,
                self.mapped_regions.as_mut_ptr(), // Parameter unused inside vfu_sgl_put
                self.mapped_regions.len(),
            );
        }
    }
}

impl DeviceContext {
    pub fn dma_range(
        &mut self, dma_addr: usize, len: usize, max_regions: usize, read: bool, write: bool,
    ) -> Result<DmaRange> {
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

            Ok(DmaRange {
                ctx: self.vfu_ctx,
                sgl_buffer,
                size: len,
                region_count,
            })
        }
    }

    pub fn dma_map(
        &mut self, dma_addr: usize, len: usize, max_regions: usize, read: bool, write: bool,
    ) -> Result<DmaMapping> {
        self.dma_range(dma_addr, len, max_regions, read, write)?
            .into_mapping()
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

impl Debug for DmaRange {
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
            .field("size", &self.size)
            .field("region_count", &self.region_count)
            .finish()
        }
    }
}

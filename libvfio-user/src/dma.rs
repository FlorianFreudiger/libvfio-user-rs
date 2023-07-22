use std::io::Error;
use std::os::raw::c_int;
use std::ptr::null_mut;
use std::slice::{from_raw_parts, from_raw_parts_mut};

use anyhow::{anyhow, ensure, Context, Result};

use libvfio_user_sys::*;

use crate::DeviceContext;

#[derive(Debug)]
pub struct DmaProtection {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

impl DmaProtection {
    fn int(&self) -> c_int {
        let mut prot = 0;

        if self.read {
            prot |= 0x1;
        }
        if self.write {
            prot |= 0x2;
        }
        if self.execute {
            prot |= 0x4;
        }

        prot
    }
}

#[derive(Debug)]
pub struct DmaMapping {
    ctx: *mut vfu_ctx_t,
    sgl_buffer: Vec<u8>,
    iovs: Vec<*mut iovec>,
    count: usize,
}

impl DmaMapping {
    pub fn iov(&self, index: usize) -> &[u8] {
        unsafe {
            let iov = *self.iovs[index];
            from_raw_parts(iov.iov_base as *const u8, iov.iov_len)
        }
    }

    pub fn iov_mut(&mut self, index: usize) -> &mut [u8] {
        unsafe {
            let iov = *self.iovs[index];
            from_raw_parts_mut(iov.iov_base as *mut u8, iov.iov_len)
        }
    }

    pub fn iov_len(&self, index: usize) -> usize {
        unsafe {
            let iov = *self.iovs[index];
            iov.iov_len
        }
    }

    pub fn total_length(&self) -> usize {
        let mut total = 0;
        unsafe {
            for iov_p in self.iovs.iter() {
                let iov = **iov_p;
                total += iov.iov_len;
            }
        }
        total
    }
}

impl Drop for DmaMapping {
    fn drop(&mut self) {
        unsafe {
            vfu_sgl_put(
                self.ctx,
                self.sgl_buffer.as_mut_ptr() as *mut dma_sg_t,
                self.iovs.as_mut_ptr() as *mut iovec,
                self.count,
            );
        }
    }
}

impl DeviceContext {
    pub fn create_dma_mapping(
        &mut self, dma_addr: usize, protection: &DmaProtection, max_sgl_entries: usize,
    ) -> Result<DmaMapping> {
        ensure!(max_sgl_entries > 0, "At least 1 entry is required.");

        let len = self
            .dma_regions
            .get(&dma_addr)
            .context("Dma range not registered")?;

        let prot = protection.int();

        unsafe {
            // dma_sg_t size is only indirectly available, allocate a buffer and do casts instead
            let mut sgl_buffer = vec![0u8; dma_sg_size() * max_sgl_entries];

            let count = vfu_addr_to_sgl(
                self.vfu_ctx,
                dma_addr as vfu_dma_addr_t,
                *len,
                sgl_buffer.as_mut_ptr() as *mut dma_sg_t,
                max_sgl_entries,
                prot,
            );

            match count {
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
                        -count - 1,
                        max_sgl_entries
                    ));
                }
                _ => {}
            }
            let count = count as usize;

            // Ensure all sgl entries are mappable
            for (i, sg) in sgl_buffer.chunks_exact_mut(dma_sg_size()).enumerate() {
                ensure!(
                    vfu_sg_is_mappable(self.vfu_ctx, sg.as_mut_ptr() as *mut dma_sg_t),
                    "Sg entry {} is not mappable",
                    i
                );
            }

            let mut iovs: Vec<*mut iovec> = vec![null_mut(); count];
            let ret = vfu_sgl_get(
                self.vfu_ctx,
                sgl_buffer.as_mut_ptr() as *mut dma_sg_t,
                iovs.as_mut_ptr() as *mut iovec,
                count,
                0,
            );
            if ret != 0 {
                let err = Error::last_os_error();
                return Err(anyhow!("Failed to populate iovec array: {}", err));
            }
            ensure!(
                !iovs.contains(&null_mut()),
                "vfu_sgl_get did not fill iovs properly"
            );

            Ok(DmaMapping {
                ctx: self.vfu_ctx,
                sgl_buffer,
                iovs,
                count,
            })
        }
    }
}

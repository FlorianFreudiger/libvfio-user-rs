use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::slice::from_raw_parts_mut;

use errno::{set_errno, Errno};

use libvfio_user_sys::*;

use crate::{Device, DeviceRegionKind, DeviceResetReason};

// Use macro to avoid having to specify a lifetime
macro_rules! device_from_vfu_ctx {
    ($vfu_ctx:ident) => {{
        let private = vfu_get_private($vfu_ctx);
        &mut *(private as *mut T)
    }};
}

pub(crate) unsafe extern "C" fn log_callback<T: Device>(
    vfu_ctx: *mut vfu_ctx_t, level: c_int, msg: *const c_char,
) {
    let device = device_from_vfu_ctx!(vfu_ctx);
    let msg = unsafe { CStr::from_ptr(msg) };

    device.log(level, msg.to_str().unwrap());
}

impl DeviceRegionKind {
    pub(crate) fn get_region_access_callback_fn<T: Device>(
        &self,
    ) -> unsafe extern "C" fn(*mut vfu_ctx_t, *mut c_char, usize, loff_t, bool) -> isize {
        match self.to_vfu_region_type() {
            0 => region_access_callback::<T, 0>,
            1 => region_access_callback::<T, 1>,
            2 => region_access_callback::<T, 2>,
            3 => region_access_callback::<T, 3>,
            4 => region_access_callback::<T, 4>,
            5 => region_access_callback::<T, 5>,
            6 => region_access_callback::<T, 6>,
            7 => region_access_callback::<T, 7>,
            8 => region_access_callback::<T, 8>,
            9 => region_access_callback::<T, 9>,
            _ => {
                unreachable!("Invalid region type")
            }
        }
    }
}

// Use R const generic to create an unique callback for each region type index
// since we can't differentiate between regions in the callback otherwise
pub(crate) unsafe extern "C" fn region_access_callback<T: Device, const R: u8>(
    vfu_ctx: *mut vfu_ctx_t, buf: *mut c_char, count: usize, offset: loff_t, is_write: bool,
) -> isize {
    let device = device_from_vfu_ctx!(vfu_ctx);

    let buf = from_raw_parts_mut(buf as *mut u8, count);
    let offset = offset as usize;

    // Not very pretty but compiler should at least optimize the match away
    let result = match R {
        0 => device.region_access_bar0(offset, buf, is_write),
        1 => device.region_access_bar1(offset, buf, is_write),
        2 => device.region_access_bar2(offset, buf, is_write),
        3 => device.region_access_bar3(offset, buf, is_write),
        4 => device.region_access_bar4(offset, buf, is_write),
        5 => device.region_access_bar5(offset, buf, is_write),
        6 => device.region_access_rom(offset, buf, is_write),
        7 => device.region_access_config(offset, buf, is_write),
        8 => device.region_access_vga(offset, buf, is_write),
        9 => device.region_access_migration(offset, buf, is_write),
        _ => {
            unreachable!("Invalid region type")
        }
    };

    match result {
        Ok(bytes_processed) => bytes_processed as isize,
        Err(error) => {
            set_errno(Errno(error));
            -1
        }
    }
}

pub(crate) unsafe extern "C" fn reset_callback<T: Device>(
    vfu_ctx: *mut vfu_ctx_t, reset_type: vfu_reset_type_t,
) -> c_int {
    let device = device_from_vfu_ctx!(vfu_ctx);

    let reason = match reset_type {
        x if x == vfu_reset_type_VFU_RESET_DEVICE => DeviceResetReason::ClientRequest,
        x if x == vfu_reset_type_VFU_RESET_LOST_CONN => DeviceResetReason::LostConnection,
        x if x == vfu_reset_type_VFU_RESET_PCI_FLR => DeviceResetReason::PciReset,
        _ => {
            unreachable!("Invalid reset type")
        }
    };

    device.reset(reason).err().unwrap_or(0)
}

pub(crate) unsafe extern "C" fn dma_register_callback<T: Device>(
    vfu_ctx: *mut vfu_ctx_t, info: *mut vfu_dma_info_t,
) {
    let device = device_from_vfu_ctx!(vfu_ctx);

    let info = &mut *info;
    device
        .ctx_mut()
        .dma_regions
        .insert(info.iova.iov_base as usize, info.iova.iov_len);
}

pub(crate) unsafe extern "C" fn dma_unregister_callback<T: Device>(
    vfu_ctx: *mut vfu_ctx_t, info: *mut vfu_dma_info_t,
) {
    let device = device_from_vfu_ctx!(vfu_ctx);

    let info = &mut *info;
    device
        .ctx_mut()
        .dma_regions
        .remove(&(info.iova.iov_base as usize));
}

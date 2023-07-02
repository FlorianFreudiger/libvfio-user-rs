use std::ffi::CStr;
use std::os::raw::{c_char, c_int};

use libvfio_user_sys::*;

use crate::{Device, DeviceContext};

// Use macro to avoid having to specify a lifetime
macro_rules! device_context_from_vfu_ctx {
    ($vfu_ctx:ident) => {{
        let private = vfu_get_private($vfu_ctx);
        &mut *(private as *mut DeviceContext<T>)
    }};
}

pub(crate) unsafe extern "C" fn log_callback<T: Device>(
    vfu_ctx: *mut vfu_ctx_t,
    level: c_int,
    msg: *const c_char,
) {
    let device_context = device_context_from_vfu_ctx!(vfu_ctx);
    let msg = unsafe { CStr::from_ptr(msg) };

    device_context.device.log(level, msg.to_str().unwrap());
}

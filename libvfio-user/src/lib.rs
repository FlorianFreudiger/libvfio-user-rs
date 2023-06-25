#[macro_use]
extern crate derive_builder;

use std::ffi::{CStr, CString};
use std::io::Error;
use std::os::raw::{c_char, c_int};
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};

use libvfio_user_sys::*;

#[derive(Clone, Debug)]
pub struct PciConfig {
    pub vendor_id: u16,
    pub device_id: u16,
    pub subsystem_vendor_id: u16,
    pub subsystem_id: u16,
}

#[derive(Debug, Builder)]
pub struct VfuSetup {
    // Path to the socket to be used for communication with the client (e.g. qemu)
    socket_path: PathBuf,

    // Exposed PCI information
    pci_config: PciConfig,
}

impl VfuSetup {
    unsafe fn setup_create_ctx(&self) -> Result<*mut vfu_ctx_t> {
        let socket_path = CString::new(
            self.socket_path
                .to_str()
                .context("Path is not valid unicode")?,
        )?;
        let ctx = vfu_create_ctx(
            vfu_trans_t_VFU_TRANS_SOCK,
            socket_path.as_ptr(),
            0,
            std::ptr::null_mut(),
            vfu_dev_type_t_VFU_DEV_TYPE_PCI,
        );

        if ctx.is_null() {
            let err = Error::last_os_error();
            return Err(anyhow!("Failed to create VFIO context: {}", err));
        }

        Ok(ctx)
    }

    unsafe fn setup_log(&self, ctx: *mut vfu_ctx_t) -> Result<()> {
        let ret = vfu_setup_log(ctx, Some(vfu_log), 0);

        if ret < 0 {
            let err = Error::last_os_error();
            return Err(anyhow!("Failed to setup logging: {}", err));
        }

        // Test log
        //let msg = CString::new("test").unwrap();
        //vfu_log(ctx, 0, msg.as_ptr());

        Ok(())
    }

    unsafe fn setup_pci(&self, ctx: *mut vfu_ctx_t) -> Result<()> {
        let ret = vfu_pci_init(ctx, vfu_pci_type_t_VFU_PCI_TYPE_EXPRESS, 0, 0);

        if ret < 0 {
            let err = Error::last_os_error();
            return Err(anyhow!("Failed to setup PCI: {}", err));
        }

        vfu_pci_set_id(
            ctx,
            self.pci_config.vendor_id,
            self.pci_config.device_id,
            self.pci_config.subsystem_vendor_id,
            self.pci_config.subsystem_id,
        );

        Ok(())
    }

    pub fn setup(&self) -> Result<VfuContext> {
        unsafe {
            let ctx = self.setup_create_ctx()?;
            self.setup_log(ctx)?;
            self.setup_pci(ctx)?;

            Ok(VfuContext { vfu_ctx: ctx })
        }
    }
}

extern "C" fn vfu_log(vfu_ctx: *mut vfu_ctx_t, level: c_int, msg: *const c_char) {
    let msg = unsafe { CStr::from_ptr(msg) };
    println!("log: {:?} - level {:?}: {:?}", vfu_ctx, level, msg);
}

pub struct VfuContext {
    vfu_ctx: *mut vfu_ctx_t,
}

impl VfuContext {
    pub fn raw_ctx(&self) -> *mut vfu_ctx_t {
        self.vfu_ctx
    }
}

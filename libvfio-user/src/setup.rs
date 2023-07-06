use std::ffi::CString;
use std::io::Error;
use std::os::raw::{c_int, c_void};

use anyhow::{anyhow, Context, Result};

use libvfio_user_sys::*;

use crate::callbacks::*;
use crate::{Device, DeviceConfiguration, DeviceContext, DeviceRegionKind};

impl DeviceConfiguration {
    unsafe fn setup_create_ctx<T: Device>(&self) -> Result<Box<DeviceContext<T>>> {
        let mut ctx = Box::new(DeviceContext::default());

        let socket_path = CString::new(
            self.socket_path
                .to_str()
                .context("Path is not valid unicode")?,
        )?;
        let flags = if self.non_blocking {
            LIBVFIO_USER_FLAG_ATTACH_NB
        } else {
            0
        } as c_int;

        // Get raw pointer to box contents
        let ctx_pointer = (&mut *ctx) as *mut DeviceContext<T>;

        let raw_ctx = vfu_create_ctx(
            vfu_trans_t_VFU_TRANS_SOCK,
            socket_path.as_ptr(),
            flags,
            ctx_pointer as *mut c_void,
            vfu_dev_type_t_VFU_DEV_TYPE_PCI,
        );

        if raw_ctx.is_null() {
            let err = Error::last_os_error();
            return Err(anyhow!("Failed to create VFIO context: {}", err));
        }

        ctx.vfu_ctx = Some(raw_ctx);
        Ok(ctx)
    }

    unsafe fn setup_log<T: Device>(&self, ctx: &DeviceContext<T>) -> Result<()> {
        let raw_ctx = ctx.raw_ctx();
        let ret = vfu_setup_log(raw_ctx, Some(log_callback::<T>), 7);

        if ret < 0 {
            let err = Error::last_os_error();
            return Err(anyhow!("Failed to setup logging: {}", err));
        }

        // Test log
        //let msg = CString::new("test").unwrap();
        //vfu_log(raw_ctx, 0, msg.as_ptr());

        Ok(())
    }

    unsafe fn setup_pci<T: Device>(&self, ctx: &DeviceContext<T>) -> Result<()> {
        let raw_ctx = ctx.raw_ctx();
        let ret = vfu_pci_init(raw_ctx, self.pci_type.to_vfu_type(), 0, 0);

        if ret < 0 {
            let err = Error::last_os_error();
            return Err(anyhow!("Failed to setup PCI: {}", err));
        }

        vfu_pci_set_id(
            raw_ctx,
            self.pci_config.vendor_id,
            self.pci_config.device_id,
            self.pci_config.subsystem_vendor_id,
            self.pci_config.subsystem_id,
        );

        vfu_pci_set_class(
            raw_ctx,
            self.pci_config.class_code_base,
            self.pci_config.class_code_subclass,
            self.pci_config.class_code_programming_interface,
        );

        // Set other pci fields directly since libvfio-user does not provide functions for them
        let config_space = vfu_pci_get_config_space(raw_ctx).as_mut().unwrap();
        config_space.__bindgen_anon_1.hdr.__bindgen_anon_1.rid = self.pci_config.revision_id;

        Ok(())
    }

    unsafe fn setup_device_regions<T: Device>(&self, ctx: &DeviceContext<T>) -> Result<()> {
        let raw_ctx = ctx.raw_ctx();
        for region in &self.device_regions {
            let region_idx = region.region_type.to_vfu_region_type();

            let mut flags = 0;
            if region.read {
                flags |= VFU_REGION_FLAG_READ;
            }
            if region.write {
                flags |= VFU_REGION_FLAG_WRITE;
            }
            if region.memory {
                flags |= VFU_REGION_FLAG_MEM;
            }
            if let DeviceRegionKind::Config { always_callback } = region.region_type {
                if always_callback {
                    flags |= VFU_REGION_FLAG_ALWAYS_CB;
                }
            }

            let callback = region.region_type.get_region_access_callback_fn::<T>();

            let ret = vfu_setup_region(
                raw_ctx,
                region_idx,
                region.size,
                Some(callback),
                flags as c_int,
                std::ptr::null_mut(), // TODO: Allow mappings
                0,
                region.file_descriptor,
                region.offset,
            );

            if ret != 0 {
                let err = Error::last_os_error();
                return Err(anyhow!("Failed to setup region {:?}: {}", region, err));
            }
        }

        Ok(())
    }

    unsafe fn setup_other_callbacks<T: Device>(&self, ctx: &DeviceContext<T>) -> Result<()> {
        let raw_ctx = ctx.raw_ctx();

        let ret = vfu_setup_device_reset_cb(raw_ctx, Some(reset_callback::<T>));
        if ret != 0 {
            let err = Error::last_os_error();
            return Err(anyhow!("Failed to setup device reset callback: {}", err));
        }

        // TODO: Other callbacks

        Ok(())
    }

    unsafe fn setup_realize<T: Device>(&self, ctx: &DeviceContext<T>) -> Result<()> {
        let raw_ctx = ctx.raw_ctx();
        let ret = vfu_realize_ctx(raw_ctx);

        if ret != 0 {
            let err = Error::last_os_error();
            return Err(anyhow!("Failed to finalize device: {}", err));
        }

        Ok(())
    }

    pub fn setup<T: Device>(&self) -> Result<Box<DeviceContext<T>>> {
        unsafe {
            let ctx = self.setup_create_ctx()?;
            self.setup_log(&ctx)?;
            self.setup_pci(&ctx)?;
            self.setup_device_regions(&ctx)?;
            // TODO: Interrupts
            // TODO: Capabilities
            self.setup_other_callbacks(&ctx)?;
            self.setup_realize(&ctx)?;

            Ok(ctx)
        }
    }
}

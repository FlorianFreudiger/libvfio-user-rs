use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::io::Error;
use std::os::raw::{c_int, c_void};
use std::os::unix::fs::FileTypeExt;
use std::ptr::null_mut;

use anyhow::{anyhow, Context, Result};

use libvfio_user_sys::*;

use crate::callbacks::*;
use crate::{Device, DeviceConfiguration, DeviceConfigurator, DeviceContext, DeviceRegionKind};

impl DeviceRegionKind {
    pub(crate) fn to_vfu_region_type(&self) -> c_int {
        let region_idx = match self {
            DeviceRegionKind::Bar0 => VFU_PCI_DEV_BAR0_REGION_IDX,
            DeviceRegionKind::Bar1 => VFU_PCI_DEV_BAR1_REGION_IDX,
            DeviceRegionKind::Bar2 => VFU_PCI_DEV_BAR2_REGION_IDX,
            DeviceRegionKind::Bar3 => VFU_PCI_DEV_BAR3_REGION_IDX,
            DeviceRegionKind::Bar4 => VFU_PCI_DEV_BAR4_REGION_IDX,
            DeviceRegionKind::Bar5 => VFU_PCI_DEV_BAR5_REGION_IDX,
            DeviceRegionKind::Rom => VFU_PCI_DEV_ROM_REGION_IDX,
            DeviceRegionKind::Config { .. } => VFU_PCI_DEV_CFG_REGION_IDX,
            DeviceRegionKind::Vga => VFU_PCI_DEV_VGA_REGION_IDX,
            DeviceRegionKind::Migration => VFU_PCI_DEV_MIGR_REGION_IDX,
        };
        region_idx as c_int
    }
}

impl DeviceConfigurator {
    pub(crate) fn validate(&self) -> Result<(), String> {
        // Check if the regions are valid and unique
        if let Some(regions) = &self.device_regions {
            let mut region_vfu_types = HashSet::new();
            for region in regions {
                let vfu_region_type = region.region_type.to_vfu_region_type();

                if region_vfu_types.contains(&vfu_region_type) {
                    return Err(format!("Duplicate device region, idx={}", vfu_region_type));
                }

                region_vfu_types.insert(vfu_region_type);
            }
        }

        Ok(())
    }
}

impl DeviceConfiguration {
    unsafe fn setup_create_device<T: Device>(&self) -> Result<Box<T>> {
        if self.overwrite_socket {
            if let Ok(metadata) = fs::metadata(&self.socket_path) {
                if metadata.file_type().is_socket() {
                    fs::remove_file(&self.socket_path)?;
                }
            }
        }

        let mut device = Box::new(T::new(DeviceContext {
            vfu_ctx: null_mut(),
            dma_regions: Default::default(),
        }));

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
        // TODO: Use some kind of Pin (instead?) of Box to ensure user cannot move out of box
        // which would drop the box and make the pointer invalid, definitely causing segfaults
        let device_pointer = (&mut *device) as *mut T;

        let raw_ctx = vfu_create_ctx(
            vfu_trans_t_VFU_TRANS_SOCK,
            socket_path.as_ptr(),
            flags,
            device_pointer as *mut c_void,
            vfu_dev_type_t_VFU_DEV_TYPE_PCI,
        );

        if raw_ctx.is_null() {
            let err = Error::last_os_error();
            return Err(anyhow!("Failed to create VFIO context: {}", err));
        }

        device.ctx_mut().vfu_ctx = raw_ctx;
        Ok(device)
    }

    unsafe fn setup_log<T: Device>(&self, ctx: &DeviceContext) -> Result<()> {
        let ret = vfu_setup_log(ctx.vfu_ctx, Some(log_callback::<T>), 7);

        if ret < 0 {
            let err = Error::last_os_error();
            return Err(anyhow!("Failed to setup logging: {}", err));
        }

        // Test log
        //let msg = CString::new("test").unwrap();
        //vfu_log(ctx.vfu_ctx, 0, msg.as_ptr());

        Ok(())
    }

    unsafe fn setup_pci<T: Device>(&self, ctx: &DeviceContext) -> Result<()> {
        let ret = vfu_pci_init(ctx.vfu_ctx, self.pci_type.to_vfu_type(), 0, 0);

        if ret < 0 {
            let err = Error::last_os_error();
            return Err(anyhow!("Failed to setup PCI: {}", err));
        }

        vfu_pci_set_id(
            ctx.vfu_ctx,
            self.pci_config.vendor_id,
            self.pci_config.device_id,
            self.pci_config.subsystem_vendor_id,
            self.pci_config.subsystem_id,
        );

        vfu_pci_set_class(
            ctx.vfu_ctx,
            self.pci_config.class_code_base,
            self.pci_config.class_code_subclass,
            self.pci_config.class_code_programming_interface,
        );

        // Set other pci fields directly since libvfio-user does not provide functions for them
        let config_space = vfu_pci_get_config_space(ctx.vfu_ctx).as_mut().unwrap();
        config_space.__bindgen_anon_1.hdr.__bindgen_anon_1.rid = self.pci_config.revision_id;

        Ok(())
    }

    unsafe fn setup_device_regions<T: Device>(&self, ctx: &DeviceContext) -> Result<()> {
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
                ctx.vfu_ctx,
                region_idx,
                region.size,
                Some(callback),
                flags as c_int,
                null_mut(), // TODO: Allow mappings
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

    unsafe fn setup_other_callbacks<T: Device>(&self, ctx: &DeviceContext) -> Result<()> {
        let ret = vfu_setup_device_reset_cb(ctx.vfu_ctx, Some(reset_callback::<T>));
        if ret != 0 {
            let err = Error::last_os_error();
            return Err(anyhow!("Failed to setup device reset callback: {}", err));
        }

        // Only setup dma if requested since this requires additional operations by both
        // libvfio-user and this wrapper for tracking and mapping dma regions,
        // which the device may not use at all
        if self.setup_dma {
            let ret = vfu_setup_device_dma(
                ctx.vfu_ctx,
                Some(dma_register_callback::<T>),
                Some(dma_unregister_callback::<T>),
            );

            if ret != 0 {
                let err = Error::last_os_error();
                return Err(anyhow!("Failed to setup DMA: {}", err));
            }
        }

        // TODO: Other callbacks

        Ok(())
    }

    unsafe fn setup_realize<T: Device>(&self, ctx: &DeviceContext) -> Result<()> {
        let ret = vfu_realize_ctx(ctx.vfu_ctx);

        if ret != 0 {
            let err = Error::last_os_error();
            return Err(anyhow!("Failed to finalize device: {}", err));
        }

        Ok(())
    }

    pub(crate) unsafe fn setup_all<T: Device>(&self) -> Result<Box<T>> {
        let device: Box<T> = self.setup_create_device()?;
        let ctx = device.ctx();

        self.setup_log::<T>(ctx)?;
        self.setup_pci::<T>(ctx)?;
        self.setup_device_regions::<T>(ctx)?;
        // TODO: Interrupts
        // TODO: Capabilities
        self.setup_other_callbacks::<T>(ctx)?;
        self.setup_realize::<T>(ctx)?;

        Ok(device)
    }
}

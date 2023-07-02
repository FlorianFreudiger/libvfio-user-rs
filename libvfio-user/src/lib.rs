#[macro_use]
extern crate derive_builder;

use std::collections::HashSet;
use std::ffi::CString;
use std::io::{Error, ErrorKind};
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};

use libvfio_user_sys::*;

use crate::callbacks::log_callback;

mod callbacks;

#[derive(Clone, Debug)]
pub enum PciType {
    Pci,
    PciX1,
    PciX2,
    PciExpress,
}

impl PciType {
    fn to_vfu_type(&self) -> vfu_dev_type_t {
        match self {
            PciType::Pci => vfu_pci_type_t_VFU_PCI_TYPE_CONVENTIONAL,
            PciType::PciX1 => vfu_pci_type_t_VFU_PCI_TYPE_PCI_X_1,
            PciType::PciX2 => vfu_pci_type_t_VFU_PCI_TYPE_PCI_X_2,
            PciType::PciExpress => vfu_pci_type_t_VFU_PCI_TYPE_EXPRESS,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PciConfig {
    pub vendor_id: u16,
    pub device_id: u16,
    pub subsystem_vendor_id: u16,
    pub subsystem_id: u16,
    pub class_code_base: u8,
    pub class_code_subclass: u8,
    pub class_code_programming_interface: u8,
    pub revision_id: u8,
}

#[derive(Clone, Debug)]
pub struct DeviceRegion {
    pub region_type: DeviceRegionKind,
    pub size: usize,
    pub file_descriptor: i32,
    pub offset: u64,
    pub read: bool,
    pub write: bool,
    pub memory: bool,
}

#[derive(Clone, Debug)]
pub enum DeviceRegionKind {
    Bar { bar: u8 },
    Rom,
    Config { always_callback: bool },
    Vga,
    Migration,
}

impl DeviceRegionKind {
    fn to_vfu_region_type(&self) -> Result<c_int> {
        let region_idx = match self {
            DeviceRegionKind::Bar { bar } => {
                if *bar > 5 {
                    return Err(anyhow!("Invalid BAR number: {}", bar));
                }

                *bar as c_uint
            }
            DeviceRegionKind::Rom => VFU_PCI_DEV_ROM_REGION_IDX,
            DeviceRegionKind::Config { .. } => VFU_PCI_DEV_CFG_REGION_IDX,
            DeviceRegionKind::Vga => VFU_PCI_DEV_VGA_REGION_IDX,
            DeviceRegionKind::Migration => VFU_PCI_DEV_MIGR_REGION_IDX,
        };
        Ok(region_idx as c_int)
    }
}

#[derive(Builder, Debug)]
#[builder(name = "DeviceConfigurator", build_fn(validate = "Self::validate"))]
pub struct DeviceConfiguration {
    // Path to the socket to be used for communication with the client (e.g. qemu)
    socket_path: PathBuf,

    // Run non-blocking, caller must handle waiting/polling for requests itself
    #[builder(default = "false")]
    non_blocking: bool,

    // Type of PCI connector the vfio-user client should expose
    #[builder(default = "PciType::PciExpress")]
    pci_type: PciType,

    // Exposed PCI information
    pci_config: PciConfig,

    #[builder(setter(custom))]
    device_regions: Vec<DeviceRegion>,
}

impl DeviceConfigurator {
    pub fn add_device_region(&mut self, region: DeviceRegion) -> &mut Self {
        self.device_regions.get_or_insert(Vec::new()).push(region);
        self
    }

    fn validate(&self) -> Result<(), String> {
        // Check if the regions are valid and unique
        if let Some(regions) = &self.device_regions {
            let mut region_vfu_types = HashSet::new();
            for region in regions {
                let vfu_region_type = region
                    .region_type
                    .to_vfu_region_type()
                    .map_err(|e| e.to_string())?;

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
        let ret = vfu_setup_log(raw_ctx, Some(log_callback::<T>), 0);

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
            let region_idx = region.region_type.to_vfu_region_type()?;

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

            let ret = vfu_setup_region(
                raw_ctx,
                region_idx,
                region.size,
                Some(vfu_region_access_callback), // TODO: Allow custom callbacks
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
            // TODO: Callbacks
            self.setup_realize(&ctx)?;

            Ok(ctx)
        }
    }
}

unsafe extern "C" fn vfu_region_access_callback(
    vfu_ctx: *mut vfu_ctx_t,
    buf: *mut c_char,
    count: usize,
    offset: loff_t,
    is_write: bool,
) -> isize {
    println!(
        "vfu_region_access_callback: {:?} - buf:{:?} - count:{:?} - offset:{:?} - write:{:?}",
        vfu_ctx, buf, count, offset, is_write
    );

    0
}

#[derive(Default)]
pub struct DeviceContext<T: Device> {
    vfu_ctx: Option<*mut vfu_ctx_t>,
    device: T,
}

impl<T: Device> DeviceContext<T> {
    pub fn raw_ctx(&self) -> *mut vfu_ctx_t {
        self.vfu_ctx.unwrap()
    }

    // Attach to the transport, if non-blocking it may return None and needs to be called again
    pub fn attach(&self) -> Result<Option<()>> {
        unsafe {
            let ret = vfu_attach_ctx(self.vfu_ctx.unwrap());

            if ret != 0 {
                let err = Error::last_os_error();

                return if err.kind() == ErrorKind::WouldBlock {
                    Ok(None)
                } else {
                    Err(anyhow!("Failed to attach device: {}", err))
                };
            }

            Ok(Some(()))
        }
    }

    pub fn run(&self) -> Result<u32> {
        unsafe {
            let ret = vfu_run_ctx(self.vfu_ctx.unwrap());

            if ret < 0 {
                let err = Error::last_os_error();
                return Err(anyhow!("Failed to run device: {}", err));
            }

            Ok(ret as u32)
        }
    }
}

impl<T: Device> Drop for DeviceContext<T> {
    fn drop(&mut self) {
        unsafe {
            match self.vfu_ctx {
                Some(ctx) => vfu_destroy_ctx(ctx),
                None => (),
            }
        }
    }
}

pub trait Device: Default {
    fn log(&self, level: i32, msg: &str);
}

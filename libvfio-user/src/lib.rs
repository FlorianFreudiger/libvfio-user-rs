#[macro_use]
extern crate derive_builder;

use std::ffi::{CStr, CString};
use std::io::Error;
use std::os::raw::{c_char, c_int, c_uint};
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};

use libvfio_user_sys::*;

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

#[derive(Debug, Clone)]
pub struct DeviceRegion {
    pub region_type: DeviceRegionKind,
    pub size: usize,
    pub file_descriptor: i32,
    pub offset: u64,
    pub read: bool,
    pub write: bool,
    pub memory: bool,
}

#[derive(Debug, Clone)]
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

#[derive(Debug, Builder)]
#[builder(build_fn(validate = "Self::validate"))]
pub struct VfuSetup {
    // Path to the socket to be used for communication with the client (e.g. qemu)
    socket_path: PathBuf,

    // Exposed PCI information
    pci_config: PciConfig,

    #[builder(setter(custom))]
    device_regions: Vec<DeviceRegion>,
}

impl VfuSetupBuilder {
    pub fn add_device_region(&mut self, region: DeviceRegion) -> &mut Self {
        self.device_regions.get_or_insert(Vec::new()).push(region);
        self
    }

    fn validate(&self) -> Result<(), String> {
        // Check there regions are valid
        if let Some(regions) = &self.device_regions {
            for region in regions {
                region
                    .region_type
                    .to_vfu_region_type()
                    .map_err(|e| e.to_string())?;
            }
        }

        Ok(())
    }
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

        vfu_pci_set_class(
            ctx,
            self.pci_config.class_code_base,
            self.pci_config.class_code_subclass,
            self.pci_config.class_code_programming_interface,
        );

        // Set other pci fields directly since libvfio-user does not provide functions for them
        let config_space = vfu_pci_get_config_space(ctx).as_mut().unwrap();
        config_space.__bindgen_anon_1.hdr.__bindgen_anon_1.rid = self.pci_config.revision_id;

        Ok(())
    }

    unsafe fn setup_device_regions(&self, ctx: *mut vfu_ctx_t) -> Result<()> {
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
                ctx,
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

    unsafe fn setup_realize(&self, ctx: *mut vfu_ctx_t) -> Result<()> {
        let ret = vfu_realize_ctx(ctx);

        if ret != 0 {
            let err = Error::last_os_error();
            return Err(anyhow!("Failed to finalize device: {}", err));
        }

        Ok(())
    }

    pub fn setup(&self) -> Result<VfuContext> {
        unsafe {
            let ctx = self.setup_create_ctx()?;
            self.setup_log(ctx)?;
            self.setup_pci(ctx)?;
            self.setup_device_regions(ctx)?;
            // TODO: Interrupts
            // TODO: Capabilities
            // TODO: Callbacks
            self.setup_realize(ctx)?;

            Ok(VfuContext { vfu_ctx: ctx })
        }
    }
}

// TODO: Allow custom logging
extern "C" fn vfu_log(vfu_ctx: *mut vfu_ctx_t, level: c_int, msg: *const c_char) {
    let msg = unsafe { CStr::from_ptr(msg) };
    println!("log: {:?} - level {:?}: {:?}", vfu_ctx, level, msg);
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

pub struct VfuContext {
    vfu_ctx: *mut vfu_ctx_t,
}

impl VfuContext {
    pub fn raw_ctx(&self) -> *mut vfu_ctx_t {
        self.vfu_ctx
    }
}

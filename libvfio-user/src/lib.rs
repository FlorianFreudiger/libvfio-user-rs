#[macro_use]
extern crate derive_builder;

use std::collections::HashSet;
use std::io::{Error, ErrorKind};
use std::os::raw::c_int;
use std::path::PathBuf;

use anyhow::anyhow;

use libvfio_user_sys::*;

mod callbacks;
mod setup;

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
    Bar0,
    Bar1,
    Bar2,
    Bar3,
    Bar4,
    Bar5,
    Rom,
    Config { always_callback: bool },
    Vga,
    Migration,
}

impl DeviceRegionKind {
    fn to_vfu_region_type(&self) -> c_int {
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

#[derive(Clone, Debug)]
pub enum DeviceResetReason {
    ClientRequest,
    LostConnection,
    PciReset,
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
    #[builder(default = "PciType::Pci")]
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
    pub fn attach(&self) -> anyhow::Result<Option<()>> {
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

    pub fn run(&self) -> anyhow::Result<u32> {
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

#[allow(unused_variables)]
pub trait Device: Default {
    fn log(&self, level: i32, msg: &str);

    fn reset(&mut self, reason: DeviceResetReason) -> Result<(), i32>;

    fn region_access_bar0(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }

    fn region_access_bar1(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }

    fn region_access_bar2(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }

    fn region_access_bar3(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }

    fn region_access_bar4(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }

    fn region_access_bar5(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }

    fn region_access_rom(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }

    fn region_access_config(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }

    fn region_access_vga(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }

    fn region_access_migration(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }
}

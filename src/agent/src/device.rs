// Copyright (c) 2019 Ant Financial
//
// SPDX-License-Identifier: Apache-2.0
//

use nix::sys::stat;
use regex::Regex;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::prelude::FileTypeExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::linux_abi::*;
use crate::pci;
use crate::sandbox::Sandbox;
use crate::uevent::{wait_for_uevent, Uevent, UeventMatcher};
use anyhow::{anyhow, Context, Result};
use cfg_if::cfg_if;
use oci::{LinuxDeviceCgroup, Spec};
use oci_spec::runtime as oci;
use protocols::agent::Device;
use tracing::instrument;

use kata_types::device::{DRIVER_VFIO_AP_TYPE, DRIVER_VFIO_PCI_GK_TYPE, DRIVER_VFIO_PCI_TYPE};

// Convenience function to obtain the scope logger.
fn sl() -> slog::Logger {
    slog_scope::logger().new(o!("subsystem" => "device"))
}

const BLOCK: &str = "block";

cfg_if! {
    if #[cfg(target_arch = "s390x")] {
        use crate::ap;
    }
}

#[instrument]
pub fn online_device(path: &str) -> Result<()> {
    fs::write(path, "1")?;
    Ok(())
}

// Force a given PCI device to bind to the given driver, does
// basically the same thing as
//    driverctl set-override <PCI address> <driver>
#[instrument]
pub fn pci_driver_override<T, U>(syspci: T, dev: pci::Address, drv: U) -> Result<()>
where
    T: AsRef<OsStr> + std::fmt::Debug,
    U: AsRef<OsStr> + std::fmt::Debug,
{
    let syspci = Path::new(&syspci);
    let drv = drv.as_ref();
    info!(sl(), "rebind_pci_driver: {} => {:?}", dev, drv);

    let devpath = syspci.join("devices").join(dev.to_string());
    let overridepath = &devpath.join("driver_override");

    fs::write(overridepath, drv.as_bytes())?;

    let drvpath = &devpath.join("driver");
    let need_unbind = match fs::read_link(drvpath) {
        Ok(d) if d.file_name() == Some(drv) => return Ok(()), // Nothing to do
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false, // No current driver
        Err(e) => return Err(anyhow!("Error checking driver on {}: {}", dev, e)),
        Ok(_) => true, // Current driver needs unbinding
    };
    if need_unbind {
        let unbindpath = &drvpath.join("unbind");
        fs::write(unbindpath, dev.to_string())?;
    }
    let probepath = syspci.join("drivers_probe");
    fs::write(probepath, dev.to_string())?;
    Ok(())
}

// Represents an IOMMU group
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IommuGroup(u32);

impl fmt::Display for IommuGroup {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "{}", self.0)
    }
}

// Determine the IOMMU group of a PCI device
#[instrument]
fn pci_iommu_group<T>(syspci: T, dev: pci::Address) -> Result<Option<IommuGroup>>
where
    T: AsRef<OsStr> + std::fmt::Debug,
{
    let syspci = Path::new(&syspci);
    let grouppath = syspci
        .join("devices")
        .join(dev.to_string())
        .join("iommu_group");

    match fs::read_link(&grouppath) {
        // Device has no group
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow!("Error reading link {:?}: {}", &grouppath, e)),
        Ok(group) => {
            if let Some(group) = group.file_name() {
                if let Some(group) = group.to_str() {
                    if let Ok(group) = group.parse::<u32>() {
                        return Ok(Some(IommuGroup(group)));
                    }
                }
            }
            Err(anyhow!(
                "Unexpected IOMMU group link {:?} => {:?}",
                grouppath,
                group
            ))
        }
    }
}

// pcipath_to_sysfs fetches the sysfs path for a PCI path, relative to
// the sysfs path for the PCI host bridge, based on the PCI path
// provided.
#[instrument]
pub fn pcipath_to_sysfs(root_bus_sysfs: &str, pcipath: &pci::Path) -> Result<String> {
    let mut bus = "0000:00".to_string();
    let mut relpath = String::new();

    for i in 0..pcipath.len() {
        let bdf = format!("{}:{}", bus, pcipath[i]);

        relpath = format!("{}/{}", relpath, bdf);

        if i == pcipath.len() - 1 {
            // Final device need not be a bridge
            break;
        }

        // Find out the bus exposed by bridge
        let bridgebuspath = format!("{}{}/pci_bus", root_bus_sysfs, relpath);
        let mut files: Vec<_> = fs::read_dir(&bridgebuspath)?.collect();

        match files.pop() {
            Some(busfile) if files.is_empty() => {
                bus = busfile?
                    .file_name()
                    .into_string()
                    .map_err(|e| anyhow!("Bad filename under {}: {:?}", &bridgebuspath, e))?;
            }
            _ => {
                return Err(anyhow!(
                    "Expected exactly one PCI bus in {}, got {} instead",
                    bridgebuspath,
                    // Adjust to original value as we've already popped
                    files.len() + 1
                ));
            }
        };
    }

    Ok(relpath)
}

#[derive(Debug)]
struct PciMatcher {
    devpath: String,
}

impl PciMatcher {
    fn new(relpath: &str) -> Result<PciMatcher> {
        let root_bus = create_pci_root_bus_path();
        Ok(PciMatcher {
            devpath: format!("{}{}", root_bus, relpath),
        })
    }
}

impl UeventMatcher for PciMatcher {
    fn is_match(&self, uev: &Uevent) -> bool {
        uev.devpath == self.devpath
    }
}

pub async fn wait_for_pci_device(
    sandbox: &Arc<Mutex<Sandbox>>,
    pcipath: &pci::Path,
) -> Result<pci::Address> {
    let root_bus_sysfs = format!("{}{}", SYSFS_DIR, create_pci_root_bus_path());
    let sysfs_rel_path = pcipath_to_sysfs(&root_bus_sysfs, pcipath)?;
    let matcher = PciMatcher::new(&sysfs_rel_path)?;

    let uev = wait_for_uevent(sandbox, matcher).await?;

    let addr = uev
        .devpath
        .rsplit('/')
        .next()
        .ok_or_else(|| anyhow!("Bad device path {:?} in uevent", &uev.devpath))?;
    let addr = pci::Address::from_str(addr)?;
    Ok(addr)
}

#[derive(Debug)]
struct NetPciMatcher {
    devpath: String,
}

impl NetPciMatcher {
    fn new(relpath: &str) -> NetPciMatcher {
        let root_bus = create_pci_root_bus_path();

        NetPciMatcher {
            devpath: format!("{}{}", root_bus, relpath),
        }
    }
}

impl UeventMatcher for NetPciMatcher {
    fn is_match(&self, uev: &Uevent) -> bool {
        uev.devpath.starts_with(self.devpath.as_str())
            && uev.subsystem == "net"
            && !uev.interface.is_empty()
            && uev.action == "add"
    }
}

pub async fn wait_for_net_interface(
    sandbox: &Arc<Mutex<Sandbox>>,
    pcipath: &pci::Path,
) -> Result<()> {
    let root_bus_sysfs = format!("{}{}", SYSFS_DIR, create_pci_root_bus_path());
    let sysfs_rel_path = pcipath_to_sysfs(&root_bus_sysfs, pcipath)?;

    let matcher = NetPciMatcher::new(&sysfs_rel_path);

    // Check if the interface is already added in case network is cold-plugged
    // or the uevent loop is started before network is added.
    // We check for the pci deive in the sysfs directory for network devices.
    let pattern = format!(
        r"[./]+{}/[a-z0-9/]*net/[a-z0-9/]*",
        matcher.devpath.as_str()
    );
    let re = Regex::new(&pattern).expect("BUG: Failed to compile regex for NetPciMatcher");

    for entry in fs::read_dir(SYSFS_NET_PATH)? {
        let entry = entry?;
        let path = entry.path();
        let target_path = fs::read_link(path)?;
        let target_path_str = target_path
            .to_str()
            .ok_or_else(|| anyhow!("Expected symlink in dir {}", SYSFS_NET_PATH))?;

        if re.is_match(target_path_str) {
            return Ok(());
        }
    }
    let _uev = wait_for_uevent(sandbox, matcher).await?;

    Ok(())
}

#[derive(Debug)]
struct VfioMatcher {
    syspath: String,
}

impl VfioMatcher {
    fn new(grp: IommuGroup) -> VfioMatcher {
        VfioMatcher {
            syspath: format!("/devices/virtual/vfio/{}", grp),
        }
    }
}

impl UeventMatcher for VfioMatcher {
    fn is_match(&self, uev: &Uevent) -> bool {
        uev.devpath == self.syspath
    }
}

#[instrument]
async fn get_vfio_device_name(sandbox: &Arc<Mutex<Sandbox>>, grp: IommuGroup) -> Result<String> {
    let matcher = VfioMatcher::new(grp);

    let uev = wait_for_uevent(sandbox, matcher).await?;
    Ok(format!("{}/{}", SYSTEM_DEV_PATH, &uev.devname))
}

#[cfg(target_arch = "s390x")]
#[derive(Debug)]
struct ApMatcher {
    syspath: String,
}

#[cfg(target_arch = "s390x")]
impl ApMatcher {
    fn new(address: ap::Address) -> ApMatcher {
        ApMatcher {
            syspath: format!(
                "{}/card{:02x}/{}",
                AP_ROOT_BUS_PATH, address.adapter_id, address
            ),
        }
    }
}

#[cfg(target_arch = "s390x")]
impl UeventMatcher for ApMatcher {
    fn is_match(&self, uev: &Uevent) -> bool {
        uev.action == "add" && uev.devpath == self.syspath
    }
}

#[cfg(target_arch = "s390x")]
#[instrument]
async fn wait_for_ap_device(sandbox: &Arc<Mutex<Sandbox>>, address: ap::Address) -> Result<()> {
    let matcher = ApMatcher::new(address);
    wait_for_uevent(sandbox, matcher).await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    // Device type, "b" for block device and "c" for character device
    cgroup_type: String,
    // The major and minor numbers for the device within the guest
    guest_major: i64,
    guest_minor: i64,
}

impl DeviceInfo {
    /// Create a device info.
    ///
    /// # Arguments
    ///
    /// * `vm_path` - Device's vm path.
    /// * `is_rdev` - If the vm_path is a device, set to true. If the
    ///   vm_path is a file in a device, set to false.
    pub fn new(vm_path: &str, is_rdev: bool) -> Result<Self> {
        let cgroup_type;
        let devid;

        let vm_path = PathBuf::from(vm_path);
        if !vm_path.exists() {
            return Err(anyhow!("VM device path {:?} doesn't exist", vm_path));
        }

        let metadata = fs::metadata(&vm_path)?;

        if is_rdev {
            devid = metadata.rdev();
            let file_type = metadata.file_type();
            if file_type.is_block_device() {
                cgroup_type = String::from("b");
            } else if file_type.is_char_device() {
                cgroup_type = String::from("c");
            } else {
                return Err(anyhow!("Unknown device {:?}'s cgroup type", vm_path));
            }
        } else {
            devid = metadata.dev();
            cgroup_type = String::from("b");
        }

        let guest_major = stat::major(devid) as i64;
        let guest_minor = stat::minor(devid) as i64;

        Ok(DeviceInfo {
            cgroup_type,
            guest_major,
            guest_minor,
        })
    }
}

// Represents the device-node and resource related updates to the OCI
// spec needed for a particular device
#[derive(Debug, Clone)]
struct DevUpdate {
    info: DeviceInfo,
    // an optional new path to update the device to in the "inner" container
    // specification
    final_path: Option<String>,
}

impl DevUpdate {
    fn new(vm_path: &str, final_path: &str) -> Result<Self> {
        Ok(DevUpdate {
            final_path: Some(final_path.to_owned()),
            ..DeviceInfo::new(vm_path, true)?.into()
        })
    }
}

impl From<DeviceInfo> for DevUpdate {
    fn from(info: DeviceInfo) -> Self {
        DevUpdate {
            info,
            final_path: None,
        }
    }
}

// Represents the updates to the OCI spec needed for a particular device
#[derive(Debug, Clone, Default)]
struct SpecUpdate {
    dev: Option<DevUpdate>,
    // optional corrections for PCI addresses
    pci: Vec<(pci::Address, pci::Address)>,
}

impl<T: Into<DevUpdate>> From<T> for SpecUpdate {
    fn from(dev: T) -> Self {
        SpecUpdate {
            dev: Some(dev.into()),
            pci: Vec::new(),
        }
    }
}

// update_spec_devices updates the device list in the OCI spec to make
// it include details appropriate for the VM, instead of the host.  It
// is given a map of (container_path => update) where:
//     container_path: the path to the device in the original OCI spec
//     update: information on changes to make to the device
#[instrument]
fn update_spec_devices(spec: &mut Spec, mut updates: HashMap<&str, DevUpdate>) -> Result<()> {
    let linux = spec
        .linux_mut()
        .as_mut()
        .ok_or_else(|| anyhow!("Spec didn't contain linux field"))?;
    let mut res_updates = HashMap::<(String, i64, i64), DeviceInfo>::with_capacity(updates.len());

    let mut default_devices = Vec::new();
    let linux_devices = linux.devices_mut().as_mut().unwrap_or(&mut default_devices);
    for specdev in linux_devices.iter_mut() {
        let devtype = specdev.typ().as_str().to_string();
        if let Some(update) = updates.remove(specdev.path().clone().display().to_string().as_str())
        {
            let host_major = specdev.major();
            let host_minor = specdev.minor();

            info!(
                sl(),
                "update_spec_devices() updating device";
                "container_path" => &specdev.path().display().to_string(),
                "type" => &devtype,
                "host_major" => host_major,
                "host_minor" => host_minor,
                "guest_major" => update.info.guest_major,
                "guest_minor" => update.info.guest_minor,
                "final_path" => update.final_path.as_ref(),
            );

            specdev.set_major(update.info.guest_major);
            specdev.set_minor(update.info.guest_minor);
            if let Some(final_path) = update.final_path {
                specdev.set_path(PathBuf::from(&final_path));
            }

            if res_updates
                .insert((devtype, host_major, host_minor), update.info)
                .is_some()
            {
                return Err(anyhow!(
                    "Conflicting resource updates for host_major={} host_minor={}",
                    host_major,
                    host_minor
                ));
            }
        }
    }

    // Make sure we applied all of our updates
    if !updates.is_empty() {
        return Err(anyhow!(
            "Missing devices in OCI spec: {:?}",
            updates
                .keys()
                .map(|d| format!("{:?}", d))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }

    if let Some(resources) = linux.resources_mut().as_mut() {
        if let Some(resources_devices) = resources.devices_mut().as_mut() {
            for d in resources_devices.iter_mut() {
                let dev_type = d.typ().unwrap_or_default().as_str().to_string();
                if let (Some(host_major), Some(host_minor)) = (d.major(), d.minor()) {
                    if let Some(update) =
                        res_updates.get(&(dev_type.clone(), host_major, host_minor))
                    {
                        info!(
                            sl(),
                            "update_spec_devices() updating resource";
                            "type" => &dev_type,
                            "host_major" => host_major,
                            "host_minor" => host_minor,
                            "guest_major" => update.guest_major,
                            "guest_minor" => update.guest_minor,
                        );

                        d.set_major(Some(update.guest_major));
                        d.set_minor(Some(update.guest_minor));
                    }
                }
            }
        }
    }

    Ok(())
}

// update_env_pci alters PCI addresses in a set of environment
// variables to be correct for the VM instead of the host.  It is
// given a map of (host address => guest address)
#[instrument]
pub fn update_env_pci(
    env: &mut [String],
    pcimap: &HashMap<pci::Address, pci::Address>,
) -> Result<()> {
    // SR-IOV device plugin may add two environment variables for one resource:
    // - PCIDEVICE_<prefix>_<resource-name>: a list of PCI device ids separated by comma
    // - PCIDEVICE_<prefix>_<resource-name>_INFO: detailed info in JSON for above PCI devices
    // Both environment variables hold information about the same set of PCI devices.
    // Below code updates both of them in two passes:
    // - 1st pass updates PCIDEVICE_<prefix>_<resource-name> and collects host to guest PCI address mapping
    let mut pci_dev_map: HashMap<String, HashMap<String, String>> = HashMap::new();
    for envvar in env.iter_mut() {
        let eqpos = envvar
            .find('=')
            .ok_or_else(|| anyhow!("Malformed OCI env entry {:?}", envvar))?;

        let (name, eqval) = envvar.split_at(eqpos);
        let val = &eqval[1..];

        if !name.starts_with("PCIDEVICE_") || name.ends_with("_INFO") {
            continue;
        }

        let mut addr_map: HashMap<String, String> = HashMap::new();
        let mut guest_addrs = Vec::<String>::new();
        for host_addr_str in val.split(',') {
            let host_addr = pci::Address::from_str(host_addr_str)
                .with_context(|| format!("Can't parse {} environment variable", name))?;
            let guest_addr = pcimap
                .get(&host_addr)
                .ok_or_else(|| anyhow!("Unable to translate host PCI address {}", host_addr))?;

            guest_addrs.push(format!("{}", guest_addr));
            addr_map.insert(host_addr_str.to_string(), format!("{}", guest_addr));
        }

        pci_dev_map.insert(format!("{}_INFO", name), addr_map);

        envvar.replace_range(eqpos + 1.., guest_addrs.join(",").as_str());
    }

    // - 2nd pass update PCIDEVICE_<prefix>_<resource-name>_INFO if it exists
    for envvar in env.iter_mut() {
        let eqpos = envvar
            .find('=')
            .ok_or_else(|| anyhow!("Malformed OCI env entry {:?}", envvar))?;

        let (name, _) = envvar.split_at(eqpos);
        if !(name.starts_with("PCIDEVICE_") && name.ends_with("_INFO")) {
            continue;
        }

        if let Some(addr_map) = pci_dev_map.get(name) {
            for (host_addr, guest_addr) in addr_map {
                *envvar = envvar.replace(host_addr, guest_addr);
            }
        }
    }

    Ok(())
}

fn split_vfio_pci_option(opt: &str) -> Option<(&str, &str)> {
    let mut tokens = opt.split('=');
    let hostbdf = tokens.next()?;
    let path = tokens.next()?;
    if tokens.next().is_some() {
        None
    } else {
        Some((hostbdf, path))
    }
}

// device.options should have one entry for each PCI device in the VFIO group
// Each option should have the form "DDDD:BB:DD.F=<pcipath>"
//     DDDD:BB:DD.F is the device's PCI address in the host
//     <pcipath> is a PCI path to the device in the guest (see pci.rs)
#[instrument]
async fn vfio_pci_device_handler(
    device: &Device,
    sandbox: &Arc<Mutex<Sandbox>>,
) -> Result<SpecUpdate> {
    let vfio_in_guest = device.type_ != DRIVER_VFIO_PCI_GK_TYPE;
    let mut pci_fixups = Vec::<(pci::Address, pci::Address)>::new();
    let mut group = None;

    for opt in device.options.iter() {
        let (host, pcipath) = split_vfio_pci_option(opt)
            .ok_or_else(|| anyhow!("Malformed VFIO PCI option {:?}", opt))?;
        let host =
            pci::Address::from_str(host).context("Bad host PCI address in VFIO option {:?}")?;
        let pcipath = pci::Path::from_str(pcipath)?;

        let guestdev = wait_for_pci_device(sandbox, &pcipath).await?;
        if vfio_in_guest {
            pci_driver_override(SYSFS_BUS_PCI_PATH, guestdev, "vfio-pci")?;

            // Devices must have an IOMMU group to be usable via VFIO
            let devgroup = pci_iommu_group(SYSFS_BUS_PCI_PATH, guestdev)?
                .ok_or_else(|| anyhow!("{} has no IOMMU group", guestdev))?;

            if let Some(g) = group {
                if g != devgroup {
                    return Err(anyhow!("{} is not in guest IOMMU group {}", guestdev, g));
                }
            }

            group = Some(devgroup);
        }

        // collect PCI address mapping for both vfio-pci-gk and vfio-pci device
        pci_fixups.push((host, guestdev));
    }

    let dev_update = if vfio_in_guest {
        // If there are any devices at all, logic above ensures that group is not None
        let group = group.ok_or_else(|| anyhow!("failed to get VFIO group"))?;

        let vm_path = get_vfio_device_name(sandbox, group).await?;

        Some(DevUpdate::new(&vm_path, &vm_path)?)
    } else {
        None
    };

    Ok(SpecUpdate {
        dev: dev_update,
        pci: pci_fixups,
    })
}

// The VFIO AP (Adjunct Processor) device handler takes all the APQNs provided as device options
// and awaits them. It sets the minimum AP rescan time of 5 seconds and temporarily adds that
// amount to the hotplug timeout.
#[cfg(target_arch = "s390x")]
#[instrument]
async fn vfio_ap_device_handler(
    device: &Device,
    sandbox: &Arc<Mutex<Sandbox>>,
) -> Result<SpecUpdate> {
    // Force AP bus rescan
    fs::write(AP_SCANS_PATH, "1")?;
    for apqn in device.options.iter() {
        wait_for_ap_device(sandbox, ap::Address::from_str(apqn)?).await?;
    }
    let dev_update = Some(DevUpdate::new(Z9_CRYPT_DEV_PATH, Z9_CRYPT_DEV_PATH)?);
    Ok(SpecUpdate {
        dev: dev_update,
        pci: Vec::new(),
    })
}

#[cfg(not(target_arch = "s390x"))]
async fn vfio_ap_device_handler(_: &Device, _: &Arc<Mutex<Sandbox>>) -> Result<SpecUpdate> {
    Err(anyhow!("AP is only supported on s390x"))
}

#[instrument]
pub async fn add_devices(
    devices: &[Device],
    spec: &mut Spec,
    sandbox: &Arc<Mutex<Sandbox>>,
) -> Result<()> {
    let mut dev_updates = HashMap::<&str, DevUpdate>::with_capacity(devices.len());

    for device in devices.iter() {
        let update = add_device(device, sandbox).await?;
        if let Some(dev_update) = update.dev {
            if dev_updates
                .insert(&device.container_path, dev_update.clone())
                .is_some()
            {
                return Err(anyhow!(
                    "Conflicting device updates for {}",
                    &device.container_path
                ));
            }

            // Update cgroup to allow all devices added to guest.
            insert_devices_cgroup_rule(spec, &dev_update.info, true, "rwm")
                .context("Update device cgroup")?;
        }

        let mut sb = sandbox.lock().await;
        for (host, guest) in update.pci {
            if let Some(other_guest) = sb.pcimap.insert(host, guest) {
                return Err(anyhow!(
                    "Conflicting guest address for host device {} ({} versus {})",
                    host,
                    guest,
                    other_guest
                ));
            }
        }
    }

    if let Some(process) = spec.process_mut() {
        let env_vec: &mut Vec<String> =
            &mut process.env_mut().get_or_insert_with(Vec::new).to_vec();
        update_env_pci(env_vec, &sandbox.lock().await.pcimap)?
    }
    update_spec_devices(spec, dev_updates)
}

#[instrument]
async fn add_device(device: &Device, sandbox: &Arc<Mutex<Sandbox>>) -> Result<SpecUpdate> {
    // log before validation to help with debugging gRPC protocol version differences.
    info!(sl(), "device-id: {}, device-type: {}, device-vm-path: {}, device-container-path: {}, device-options: {:?}",
          device.id, device.type_, device.vm_path, device.container_path, device.options);

    if device.type_.is_empty() {
        return Err(anyhow!("invalid type for device {:?}", device));
    }

    if device.id.is_empty() && device.vm_path.is_empty() {
        return Err(anyhow!("invalid ID and VM path for device {:?}", device));
    }

    if device.container_path.is_empty() {
        return Err(anyhow!("invalid container path for device {:?}", device));
    }

    match device.type_.as_str() {
        DRIVER_BLK_PCI_TYPE => virtio_blk_device_handler(device, sandbox).await,
        DRIVER_BLK_CCW_TYPE => virtio_blk_ccw_device_handler(device, sandbox).await,
        DRIVER_BLK_MMIO_TYPE => virtiommio_blk_device_handler(device, sandbox).await,
        DRIVER_NVDIMM_TYPE => virtio_nvdimm_device_handler(device, sandbox).await,
        DRIVER_SCSI_TYPE => virtio_scsi_device_handler(device, sandbox).await,
        DRIVER_VFIO_PCI_GK_TYPE | DRIVER_VFIO_PCI_TYPE => {
            vfio_pci_device_handler(device, sandbox).await
        }
        DRIVER_VFIO_AP_TYPE => vfio_ap_device_handler(device, sandbox).await,
        _ => Err(anyhow!("Unknown device type {}", device.type_)),
    }
}

// Insert a devices cgroup rule to control access to device.
#[instrument]
pub fn insert_devices_cgroup_rule(
    spec: &mut Spec,
    dev_info: &DeviceInfo,
    allow: bool,
    access: &str,
) -> Result<()> {
    let linux = spec
        .linux_mut()
        .as_mut()
        .ok_or_else(|| anyhow!("Spec didn't container linux field"))?;
    let devcgrp_type = dev_info
        .cgroup_type
        .parse::<oci::LinuxDeviceType>()
        .context(format!(
            "Failed to parse {:?} to Enum LinuxDeviceType",
            dev_info.cgroup_type
        ))?;
    let linux_resource = &mut oci::LinuxResources::default();
    let resource = linux.resources_mut().as_mut().unwrap_or(linux_resource);
    let mut device_cgrp = LinuxDeviceCgroup::default();
    device_cgrp.set_allow(allow);
    device_cgrp.set_major(Some(dev_info.guest_major));
    device_cgrp.set_minor(Some(dev_info.guest_minor));
    device_cgrp.set_typ(Some(devcgrp_type));
    device_cgrp.set_access(Some(access.to_owned()));

    debug!(
        sl(),
        "Insert a devices cgroup rule";
        "linux_device_cgroup" => device_cgrp.allow(),
        "guest_major" => device_cgrp.major(),
        "guest_minor" => device_cgrp.minor(),
        "type" => device_cgrp.typ().unwrap().as_str(),
        "access" => device_cgrp.access().as_ref().unwrap().as_str(),
    );

    if let Some(devices) = resource.devices_mut() {
        devices.push(device_cgrp);
    } else {
        resource.set_devices(Some(vec![device_cgrp]));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uevent::spawn_test_watcher;
    use oci::{
        Linux, LinuxBuilder, LinuxDeviceBuilder, LinuxDeviceCgroupBuilder, LinuxDeviceType,
        LinuxResources, LinuxResourcesBuilder, SpecBuilder,
    };
    use oci_spec::runtime as oci;
    use std::iter::FromIterator;
    use tempfile::tempdir;

    const VM_ROOTFS: &str = "/";

    #[test]
    fn test_update_device_cgroup() {
        let mut linux = Linux::default();
        linux.set_resources(Some(LinuxResources::default()));
        let mut spec = SpecBuilder::default().linux(linux).build().unwrap();

        let dev_info = DeviceInfo::new(VM_ROOTFS, false).unwrap();
        insert_devices_cgroup_rule(&mut spec, &dev_info, false, "rw").unwrap();

        let devices = spec
            .linux()
            .as_ref()
            .unwrap()
            .resources()
            .as_ref()
            .unwrap()
            .devices()
            .clone()
            .unwrap();
        assert_eq!(devices.len(), 1);

        let meta = fs::metadata(VM_ROOTFS).unwrap();
        let rdev = meta.dev();
        let major = stat::major(rdev) as i64;
        let minor = stat::minor(rdev) as i64;

        assert_eq!(devices[0].major(), Some(major));
        assert_eq!(devices[0].minor(), Some(minor));
    }

    #[test]
    fn test_update_spec_devices() {
        let (major, minor) = (7, 2);
        let mut spec = Spec::default();

        // vm_path empty
        let update = DeviceInfo::new("", true);
        assert!(update.is_err());

        // linux is empty
        let container_path = "/dev/null";
        let vm_path = "/dev/null";
        let res = update_spec_devices(
            &mut spec,
            HashMap::from_iter(vec![(
                container_path,
                DeviceInfo::new(vm_path, true).unwrap().into(),
            )]),
        );
        assert!(res.is_err());

        spec.set_linux(Some(Linux::default()));

        // linux.devices doesn't contain the updated device
        let res = update_spec_devices(
            &mut spec,
            HashMap::from_iter(vec![(
                container_path,
                DeviceInfo::new(vm_path, true).unwrap().into(),
            )]),
        );
        assert!(res.is_err());

        spec.linux_mut()
            .as_mut()
            .unwrap()
            .set_devices(Some(vec![LinuxDeviceBuilder::default()
                .path(PathBuf::from("/dev/null2"))
                .major(major)
                .minor(minor)
                .build()
                .unwrap()]));

        // guest and host path are not the same
        let res = update_spec_devices(
            &mut spec,
            HashMap::from_iter(vec![(
                container_path,
                DeviceInfo::new(vm_path, true).unwrap().into(),
            )]),
        );
        assert!(
            res.is_err(),
            "container_path={:?} vm_path={:?} spec={:?}",
            container_path,
            vm_path,
            spec
        );

        spec.linux_mut()
            .as_mut()
            .unwrap()
            .devices_mut()
            .as_mut()
            .unwrap()[0]
            .set_path(PathBuf::from(container_path));

        // spec.linux.resources is empty
        let res = update_spec_devices(
            &mut spec,
            HashMap::from_iter(vec![(
                container_path,
                DeviceInfo::new(vm_path, true).unwrap().into(),
            )]),
        );
        assert!(res.is_ok());

        // update both devices and cgroup lists
        spec.linux_mut()
            .as_mut()
            .unwrap()
            .set_devices(Some(vec![LinuxDeviceBuilder::default()
                .path(PathBuf::from(container_path))
                .major(major)
                .minor(minor)
                .build()
                .unwrap()]));

        spec.linux_mut().as_mut().unwrap().set_resources(Some(
            oci::LinuxResourcesBuilder::default()
                .devices(vec![LinuxDeviceCgroupBuilder::default()
                    .major(major)
                    .minor(minor)
                    .build()
                    .unwrap()])
                .build()
                .unwrap(),
        ));

        let res = update_spec_devices(
            &mut spec,
            HashMap::from_iter(vec![(
                container_path,
                DeviceInfo::new(vm_path, true).unwrap().into(),
            )]),
        );
        assert!(res.is_ok());
    }

    #[test]
    fn test_update_spec_devices_guest_host_conflict() {
        let null_rdev = fs::metadata("/dev/null").unwrap().rdev();
        let zero_rdev = fs::metadata("/dev/zero").unwrap().rdev();
        let full_rdev = fs::metadata("/dev/full").unwrap().rdev();

        let host_major_a = stat::major(null_rdev) as i64;
        let host_minor_a = stat::minor(null_rdev) as i64;
        let host_major_b = stat::major(zero_rdev) as i64;
        let host_minor_b = stat::minor(zero_rdev) as i64;

        let mut spec = SpecBuilder::default()
            .linux(
                LinuxBuilder::default()
                    .devices(vec![
                        LinuxDeviceBuilder::default()
                            .path(PathBuf::from("/dev/a"))
                            .typ(LinuxDeviceType::C)
                            .major(host_major_a)
                            .minor(host_minor_a)
                            .build()
                            .unwrap(),
                        LinuxDeviceBuilder::default()
                            .path(PathBuf::from("/dev/b"))
                            .typ(LinuxDeviceType::C)
                            .major(host_major_b)
                            .minor(host_minor_b)
                            .build()
                            .unwrap(),
                    ])
                    .resources(
                        LinuxResourcesBuilder::default()
                            .devices(vec![
                                LinuxDeviceCgroupBuilder::default()
                                    .typ(LinuxDeviceType::C)
                                    .major(host_major_a)
                                    .minor(host_minor_a)
                                    .build()
                                    .unwrap(),
                                LinuxDeviceCgroupBuilder::default()
                                    .typ(LinuxDeviceType::C)
                                    .major(host_major_b)
                                    .minor(host_minor_b)
                                    .build()
                                    .unwrap(),
                            ])
                            .build()
                            .unwrap(),
                    )
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();

        let container_path_a = "/dev/a";
        let vm_path_a = "/dev/zero";

        let guest_major_a = stat::major(zero_rdev) as i64;
        let guest_minor_a = stat::minor(zero_rdev) as i64;

        let container_path_b = "/dev/b";
        let vm_path_b = "/dev/full";

        let guest_major_b = stat::major(full_rdev) as i64;
        let guest_minor_b = stat::minor(full_rdev) as i64;

        let specdevices = &spec.linux().as_ref().unwrap().devices().clone().unwrap();
        assert_eq!(host_major_a, specdevices[0].major());
        assert_eq!(host_minor_a, specdevices[0].minor());
        assert_eq!(host_major_b, specdevices[1].major());
        assert_eq!(host_minor_b, specdevices[1].minor());

        let specresources_devices = spec
            .linux()
            .as_ref()
            .unwrap()
            .resources()
            .as_ref()
            .unwrap()
            .devices()
            .clone()
            .unwrap();
        assert_eq!(Some(host_major_a), specresources_devices[0].major());
        assert_eq!(Some(host_minor_a), specresources_devices[0].minor());
        assert_eq!(Some(host_major_b), specresources_devices[1].major());
        assert_eq!(Some(host_minor_b), specresources_devices[1].minor());

        let updates = HashMap::from_iter(vec![
            (
                container_path_a,
                DeviceInfo::new(vm_path_a, true).unwrap().into(),
            ),
            (
                container_path_b,
                DeviceInfo::new(vm_path_b, true).unwrap().into(),
            ),
        ]);
        let res = update_spec_devices(&mut spec, updates);
        assert!(res.is_ok());

        let specdevices = &spec.linux().as_ref().unwrap().devices().clone().unwrap();
        assert_eq!(guest_major_a, specdevices[0].major());
        assert_eq!(guest_minor_a, specdevices[0].minor());
        assert_eq!(guest_major_b, specdevices[1].major());
        assert_eq!(guest_minor_b, specdevices[1].minor());

        let specresources_devices = spec
            .linux()
            .as_ref()
            .unwrap()
            .resources()
            .as_ref()
            .unwrap()
            .devices()
            .clone()
            .unwrap();
        assert_eq!(Some(guest_major_a), specresources_devices[0].major());
        assert_eq!(Some(guest_minor_a), specresources_devices[0].minor());
        assert_eq!(Some(guest_major_b), specresources_devices[1].major());
        assert_eq!(Some(guest_minor_b), specresources_devices[1].minor());
    }

    #[test]
    fn test_update_spec_devices_char_block_conflict() {
        let null_rdev = fs::metadata("/dev/null").unwrap().rdev();

        let guest_major = stat::major(null_rdev) as i64;
        let guest_minor = stat::minor(null_rdev) as i64;
        let host_major: i64 = 99;
        let host_minor: i64 = 99;

        let mut spec = SpecBuilder::default()
            .linux(
                LinuxBuilder::default()
                    .devices(vec![
                        LinuxDeviceBuilder::default()
                            .path(PathBuf::from("/dev/char"))
                            .typ(LinuxDeviceType::C)
                            .major(host_major)
                            .minor(host_minor)
                            .build()
                            .unwrap(),
                        LinuxDeviceBuilder::default()
                            .path(PathBuf::from("/dev/block"))
                            .typ(LinuxDeviceType::B)
                            .major(host_major)
                            .minor(host_minor)
                            .build()
                            .unwrap(),
                    ])
                    .resources(
                        LinuxResourcesBuilder::default()
                            .devices(vec![
                                LinuxDeviceCgroupBuilder::default()
                                    .typ(LinuxDeviceType::C)
                                    .major(host_major)
                                    .minor(host_minor)
                                    .build()
                                    .unwrap(),
                                LinuxDeviceCgroupBuilder::default()
                                    .typ(LinuxDeviceType::B)
                                    .major(host_major)
                                    .minor(host_minor)
                                    .build()
                                    .unwrap(),
                            ])
                            .build()
                            .unwrap(),
                    )
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();

        let container_path = "/dev/char";
        let vm_path = "/dev/null";

        let specresources_devices = spec
            .linux()
            .as_ref()
            .unwrap()
            .resources()
            .as_ref()
            .unwrap()
            .devices()
            .clone()
            .unwrap();
        assert_eq!(Some(host_major), specresources_devices[0].major());
        assert_eq!(Some(host_minor), specresources_devices[0].minor());
        assert_eq!(Some(host_major), specresources_devices[1].major());
        assert_eq!(Some(host_minor), specresources_devices[1].minor());

        let res = update_spec_devices(
            &mut spec,
            HashMap::from_iter(vec![(
                container_path,
                DeviceInfo::new(vm_path, true).unwrap().into(),
            )]),
        );
        assert!(res.is_ok());

        // Only the char device, not the block device should be updated
        let specresources_devices = spec
            .linux()
            .as_ref()
            .unwrap()
            .resources()
            .as_ref()
            .unwrap()
            .devices()
            .clone()
            .unwrap();
        assert_eq!(Some(guest_major), specresources_devices[0].major());
        assert_eq!(Some(guest_minor), specresources_devices[0].minor());
        assert_eq!(Some(host_major), specresources_devices[1].major());
        assert_eq!(Some(host_minor), specresources_devices[1].minor());
    }

    #[test]
    fn test_update_spec_devices_final_path() {
        let null_rdev = fs::metadata("/dev/null").unwrap().rdev();
        let guest_major = stat::major(null_rdev) as i64;
        let guest_minor = stat::minor(null_rdev) as i64;

        let container_path = "/dev/original";
        let host_major: i64 = 99;
        let host_minor: i64 = 99;

        let mut spec = SpecBuilder::default()
            .linux(
                LinuxBuilder::default()
                    .devices(vec![LinuxDeviceBuilder::default()
                        .path(PathBuf::from(container_path))
                        .typ(LinuxDeviceType::C)
                        .major(host_major)
                        .minor(host_minor)
                        .build()
                        .unwrap()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();

        let vm_path = "/dev/null";
        let final_path = "/dev/new";

        let res = update_spec_devices(
            &mut spec,
            HashMap::from_iter(vec![(
                container_path,
                DevUpdate::new(vm_path, final_path).unwrap(),
            )]),
        );
        assert!(res.is_ok());

        let specdevices = &spec.linux().as_ref().unwrap().devices().clone().unwrap();
        assert_eq!(guest_major, specdevices[0].major());
        assert_eq!(guest_minor, specdevices[0].minor());
        assert_eq!(&PathBuf::from(final_path), specdevices[0].path());
    }

    #[test]
    fn test_update_env_pci() {
        let example_map = [
            // Each is a host,guest pair of pci addresses
            ("0000:1a:01.0", "0000:01:01.0"),
            ("0000:1b:02.0", "0000:01:02.0"),
            // This one has the same host address as guest address
            // above, to test that we're not double-translating
            ("0000:01:01.0", "ffff:02:1f.7"),
        ];

        let pci_dev_info_original = r#"PCIDEVICE_x_INFO={"0000:1a:01.0":{"generic":{"deviceID":"0000:1a:01.0"}},"0000:1b:02.0":{"generic":{"deviceID":"0000:1b:02.0"}}}"#;
        let pci_dev_info_expected = r#"PCIDEVICE_x_INFO={"0000:01:01.0":{"generic":{"deviceID":"0000:01:01.0"}},"0000:01:02.0":{"generic":{"deviceID":"0000:01:02.0"}}}"#;
        let mut env = vec![
            "PCIDEVICE_x=0000:1a:01.0,0000:1b:02.0".to_string(),
            pci_dev_info_original.to_string(),
            "PCIDEVICE_y=0000:01:01.0".to_string(),
            "NOTAPCIDEVICE_blah=abcd:ef:01.0".to_string(),
        ];

        let pci_fixups = example_map
            .iter()
            .map(|(h, g)| {
                (
                    pci::Address::from_str(h).unwrap(),
                    pci::Address::from_str(g).unwrap(),
                )
            })
            .collect();

        let res = update_env_pci(&mut env, &pci_fixups);
        assert!(res.is_ok(), "error: {}", res.err().unwrap());

        assert_eq!(env[0], "PCIDEVICE_x=0000:01:01.0,0000:01:02.0");
        assert_eq!(env[1], pci_dev_info_expected);
        assert_eq!(env[2], "PCIDEVICE_y=ffff:02:1f.7");
        assert_eq!(env[3], "NOTAPCIDEVICE_blah=abcd:ef:01.0");
    }

    #[test]
    fn test_pcipath_to_sysfs() {
        let testdir = tempdir().expect("failed to create tmpdir");
        let rootbuspath = testdir.path().to_str().unwrap();

        let path2 = pci::Path::from_str("02").unwrap();
        let path23 = pci::Path::from_str("02/03").unwrap();
        let path234 = pci::Path::from_str("02/03/04").unwrap();

        let relpath = pcipath_to_sysfs(rootbuspath, &path2);
        assert_eq!(relpath.unwrap(), "/0000:00:02.0");

        let relpath = pcipath_to_sysfs(rootbuspath, &path23);
        assert!(relpath.is_err());

        let relpath = pcipath_to_sysfs(rootbuspath, &path234);
        assert!(relpath.is_err());

        // Create mock sysfs files for the device at 0000:00:02.0
        let bridge2path = format!("{}{}", rootbuspath, "/0000:00:02.0");

        fs::create_dir_all(&bridge2path).unwrap();

        let relpath = pcipath_to_sysfs(rootbuspath, &path2);
        assert_eq!(relpath.unwrap(), "/0000:00:02.0");

        let relpath = pcipath_to_sysfs(rootbuspath, &path23);
        assert!(relpath.is_err());

        let relpath = pcipath_to_sysfs(rootbuspath, &path234);
        assert!(relpath.is_err());

        // Create mock sysfs files to indicate that 0000:00:02.0 is a bridge to bus 01
        let bridge2bus = "0000:01";
        let bus2path = format!("{}/pci_bus/{}", bridge2path, bridge2bus);

        fs::create_dir_all(bus2path).unwrap();

        let relpath = pcipath_to_sysfs(rootbuspath, &path2);
        assert_eq!(relpath.unwrap(), "/0000:00:02.0");

        let relpath = pcipath_to_sysfs(rootbuspath, &path23);
        assert_eq!(relpath.unwrap(), "/0000:00:02.0/0000:01:03.0");

        let relpath = pcipath_to_sysfs(rootbuspath, &path234);
        assert!(relpath.is_err());

        // Create mock sysfs files for a bridge at 0000:01:03.0 to bus 02
        let bridge3path = format!("{}/0000:01:03.0", bridge2path);
        let bridge3bus = "0000:02";
        let bus3path = format!("{}/pci_bus/{}", bridge3path, bridge3bus);

        fs::create_dir_all(bus3path).unwrap();

        let relpath = pcipath_to_sysfs(rootbuspath, &path2);
        assert_eq!(relpath.unwrap(), "/0000:00:02.0");

        let relpath = pcipath_to_sysfs(rootbuspath, &path23);
        assert_eq!(relpath.unwrap(), "/0000:00:02.0/0000:01:03.0");

        let relpath = pcipath_to_sysfs(rootbuspath, &path234);
        assert_eq!(relpath.unwrap(), "/0000:00:02.0/0000:01:03.0/0000:02:04.0");
    }

    // We use device specific variants of this for real cases, but
    // they have some complications that make them troublesome to unit
    // test
    async fn example_get_device_name(
        sandbox: &Arc<Mutex<Sandbox>>,
        relpath: &str,
    ) -> Result<String> {
        let matcher = VirtioBlkPciMatcher::new(relpath);

        let uev = wait_for_uevent(sandbox, matcher).await?;

        Ok(uev.devname)
    }

    #[tokio::test]
    async fn test_get_device_name() {
        let devname = "vda";
        let root_bus = create_pci_root_bus_path();
        let relpath = "/0000:00:0a.0/0000:03:0b.0";
        let devpath = format!("{}{}/virtio4/block/{}", root_bus, relpath, devname);

        let mut uev = crate::uevent::Uevent::default();
        uev.action = crate::linux_abi::U_EVENT_ACTION_ADD.to_string();
        uev.subsystem = BLOCK.to_string();
        uev.devpath = devpath.clone();
        uev.devname = devname.to_string();

        let logger = slog::Logger::root(slog::Discard, o!());
        let sandbox = Arc::new(Mutex::new(Sandbox::new(&logger).unwrap()));

        let mut sb = sandbox.lock().await;
        sb.uevent_map.insert(devpath.clone(), uev);
        drop(sb); // unlock

        let name = example_get_device_name(&sandbox, relpath).await;
        assert!(name.is_ok(), "{}", name.unwrap_err());
        assert_eq!(name.unwrap(), devname);

        let mut sb = sandbox.lock().await;
        let uev = sb.uevent_map.remove(&devpath).unwrap();
        drop(sb); // unlock

        spawn_test_watcher(sandbox.clone(), uev);

        let name = example_get_device_name(&sandbox, relpath).await;
        assert!(name.is_ok(), "{}", name.unwrap_err());
        assert_eq!(name.unwrap(), devname);
    }

    #[tokio::test]
    #[allow(clippy::redundant_clone)]
    async fn test_virtio_blk_matcher() {
        let root_bus = create_pci_root_bus_path();
        let devname = "vda";

        let mut uev_a = crate::uevent::Uevent::default();
        let relpath_a = "/0000:00:0a.0";
        uev_a.action = crate::linux_abi::U_EVENT_ACTION_ADD.to_string();
        uev_a.subsystem = BLOCK.to_string();
        uev_a.devname = devname.to_string();
        uev_a.devpath = format!("{}{}/virtio4/block/{}", root_bus, relpath_a, devname);
        let matcher_a = VirtioBlkPciMatcher::new(relpath_a);

        let mut uev_b = uev_a.clone();
        let relpath_b = "/0000:00:0a.0/0000:00:0b.0";
        uev_b.devpath = format!("{}{}/virtio0/block/{}", root_bus, relpath_b, devname);
        let matcher_b = VirtioBlkPciMatcher::new(relpath_b);

        assert!(matcher_a.is_match(&uev_a));
        assert!(matcher_b.is_match(&uev_b));
        assert!(!matcher_b.is_match(&uev_a));
        assert!(!matcher_a.is_match(&uev_b));
    }

    #[cfg(target_arch = "s390x")]
    #[tokio::test]
    async fn test_virtio_blk_ccw_matcher() {
        let root_bus = CCW_ROOT_BUS_PATH;
        let subsystem = "block";
        let devname = "vda";
        let relpath = "0.0.0002";

        let mut uev = crate::uevent::Uevent::default();
        uev.action = crate::linux_abi::U_EVENT_ACTION_ADD.to_string();
        uev.subsystem = subsystem.to_string();
        uev.devname = devname.to_string();
        uev.devpath = format!(
            "{}/0.0.0001/{}/virtio1/{}/{}",
            root_bus, relpath, subsystem, devname
        );

        // Valid path
        let device = ccw::Device::from_str(relpath).unwrap();
        let matcher = VirtioBlkCCWMatcher::new(root_bus, &device);
        assert!(matcher.is_match(&uev));

        // Invalid paths
        uev.devpath = format!(
            "{}/0.0.0001/0.0.0003/virtio1/{}/{}",
            root_bus, subsystem, devname
        );
        assert!(!matcher.is_match(&uev));

        uev.devpath = format!("0.0.0001/{}/virtio1/{}/{}", relpath, subsystem, devname);
        assert!(!matcher.is_match(&uev));

        uev.devpath = format!(
            "{}/0.0.0001/{}/virtio/{}/{}",
            root_bus, relpath, subsystem, devname
        );
        assert!(!matcher.is_match(&uev));

        uev.devpath = format!("{}/0.0.0001/{}/virtio1", root_bus, relpath);
        assert!(!matcher.is_match(&uev));

        uev.devpath = format!(
            "{}/1.0.0001/{}/virtio1/{}/{}",
            root_bus, relpath, subsystem, devname
        );
        assert!(!matcher.is_match(&uev));

        uev.devpath = format!(
            "{}/0.4.0001/{}/virtio1/{}/{}",
            root_bus, relpath, subsystem, devname
        );
        assert!(!matcher.is_match(&uev));

        uev.devpath = format!(
            "{}/0.0.10000/{}/virtio1/{}/{}",
            root_bus, relpath, subsystem, devname
        );
        assert!(!matcher.is_match(&uev));
    }

    #[tokio::test]
    #[allow(clippy::redundant_clone)]
    async fn test_scsi_block_matcher() {
        let root_bus = create_pci_root_bus_path();
        let devname = "sda";

        let mut uev_a = crate::uevent::Uevent::default();
        let addr_a = "0:0";
        uev_a.action = crate::linux_abi::U_EVENT_ACTION_ADD.to_string();
        uev_a.subsystem = BLOCK.to_string();
        uev_a.devname = devname.to_string();
        uev_a.devpath = format!(
            "{}/0000:00:00.0/virtio0/host0/target0:0:0/0:0:{}/block/sda",
            root_bus, addr_a
        );
        let matcher_a = ScsiBlockMatcher::new(addr_a);

        let mut uev_b = uev_a.clone();
        let addr_b = "2:0";
        uev_b.devpath = format!(
            "{}/0000:00:00.0/virtio0/host0/target0:0:2/0:0:{}/block/sdb",
            root_bus, addr_b
        );
        let matcher_b = ScsiBlockMatcher::new(addr_b);

        assert!(matcher_a.is_match(&uev_a));
        assert!(matcher_b.is_match(&uev_b));
        assert!(!matcher_b.is_match(&uev_a));
        assert!(!matcher_a.is_match(&uev_b));
    }

    #[tokio::test]
    #[allow(clippy::redundant_clone)]
    async fn test_vfio_matcher() {
        let grpa = IommuGroup(1);
        let grpb = IommuGroup(22);

        let mut uev_a = crate::uevent::Uevent::default();
        uev_a.action = crate::linux_abi::U_EVENT_ACTION_ADD.to_string();
        uev_a.devname = format!("vfio/{}", grpa);
        uev_a.devpath = format!("/devices/virtual/vfio/{}", grpa);
        let matcher_a = VfioMatcher::new(grpa);

        let mut uev_b = uev_a.clone();
        uev_b.devpath = format!("/devices/virtual/vfio/{}", grpb);
        let matcher_b = VfioMatcher::new(grpb);

        assert!(matcher_a.is_match(&uev_a));
        assert!(matcher_b.is_match(&uev_b));
        assert!(!matcher_b.is_match(&uev_a));
        assert!(!matcher_a.is_match(&uev_b));
    }

    #[tokio::test]
    #[allow(clippy::redundant_clone)]
    async fn test_net_pci_matcher() {
        let root_bus = create_pci_root_bus_path();
        let relpath_a = "/0000:00:02.0/0000:01:01.0";

        let mut uev_a = crate::uevent::Uevent::default();
        uev_a.action = crate::linux_abi::U_EVENT_ACTION_ADD.to_string();
        uev_a.devpath = format!("{}{}", root_bus, relpath_a);
        uev_a.subsystem = String::from("net");
        uev_a.interface = String::from("eth0");
        let matcher_a = NetPciMatcher::new(relpath_a);
        println!("Matcher a : {}", matcher_a.devpath);

        let relpath_b = "/0000:00:02.0/0000:01:02.0";
        let mut uev_b = uev_a.clone();
        uev_b.devpath = format!("{}{}", root_bus, relpath_b);
        let matcher_b = NetPciMatcher::new(relpath_b);

        assert!(matcher_a.is_match(&uev_a));
        assert!(matcher_b.is_match(&uev_b));
        assert!(!matcher_b.is_match(&uev_a));
        assert!(!matcher_a.is_match(&uev_b));

        let relpath_c = "/0000:00:02.0/0000:01:03.0";
        let net_substr = "/net/eth0";
        let mut uev_c = uev_a.clone();
        uev_c.devpath = format!("{}{}{}", root_bus, relpath_c, net_substr);
        let matcher_c = NetPciMatcher::new(relpath_c);

        assert!(matcher_c.is_match(&uev_c));
        assert!(!matcher_a.is_match(&uev_c));
        assert!(!matcher_b.is_match(&uev_c));
    }

    #[tokio::test]
    #[allow(clippy::redundant_clone)]
    async fn test_mmio_block_matcher() {
        let devname_a = "vda";
        let devname_b = "vdb";
        let mut uev_a = crate::uevent::Uevent::default();
        uev_a.action = crate::linux_abi::U_EVENT_ACTION_ADD.to_string();
        uev_a.subsystem = BLOCK.to_string();
        uev_a.devname = devname_a.to_string();
        uev_a.devpath = format!(
            "/sys/devices/virtio-mmio-cmdline/virtio-mmio.0/virtio0/block/{}",
            devname_a
        );
        let matcher_a = MmioBlockMatcher::new(devname_a);

        let mut uev_b = uev_a.clone();
        uev_b.devpath = format!(
            "/sys/devices/virtio-mmio-cmdline/virtio-mmio.4/virtio4/block/{}",
            devname_b
        );
        let matcher_b = MmioBlockMatcher::new(devname_b);

        assert!(matcher_a.is_match(&uev_a));
        assert!(matcher_b.is_match(&uev_b));
        assert!(!matcher_b.is_match(&uev_a));
        assert!(!matcher_a.is_match(&uev_b));
    }

    #[test]
    fn test_split_vfio_pci_option() {
        assert_eq!(
            split_vfio_pci_option("0000:01:00.0=02/01"),
            Some(("0000:01:00.0", "02/01"))
        );
        assert_eq!(split_vfio_pci_option("0000:01:00.0=02/01=rubbish"), None);
        assert_eq!(split_vfio_pci_option("0000:01:00.0"), None);
    }

    #[test]
    fn test_pci_driver_override() {
        let testdir = tempdir().expect("failed to create tmpdir");
        let syspci = testdir.path(); // Path to mock /sys/bus/pci

        let dev0 = pci::Address::new(0, 0, pci::SlotFn::new(0, 0).unwrap());
        let dev0path = syspci.join("devices").join(dev0.to_string());
        let dev0drv = dev0path.join("driver");
        let dev0override = dev0path.join("driver_override");

        let drvapath = syspci.join("drivers").join("drv_a");
        let drvaunbind = drvapath.join("unbind");

        let probepath = syspci.join("drivers_probe");

        // Start mocking dev0 as being unbound
        fs::create_dir_all(&dev0path).unwrap();

        pci_driver_override(syspci, dev0, "drv_a").unwrap();
        assert_eq!(fs::read_to_string(&dev0override).unwrap(), "drv_a");
        assert_eq!(fs::read_to_string(&probepath).unwrap(), dev0.to_string());

        // Now mock dev0 already being attached to drv_a
        fs::create_dir_all(&drvapath).unwrap();
        std::os::unix::fs::symlink(&drvapath, dev0drv).unwrap();
        std::fs::remove_file(&probepath).unwrap();

        pci_driver_override(syspci, dev0, "drv_a").unwrap(); // no-op
        assert_eq!(fs::read_to_string(&dev0override).unwrap(), "drv_a");
        assert!(!probepath.exists());

        // Now try binding to a different driver
        pci_driver_override(syspci, dev0, "drv_b").unwrap();
        assert_eq!(fs::read_to_string(&dev0override).unwrap(), "drv_b");
        assert_eq!(fs::read_to_string(&probepath).unwrap(), dev0.to_string());
        assert_eq!(fs::read_to_string(drvaunbind).unwrap(), dev0.to_string());
    }

    #[test]
    fn test_pci_iommu_group() {
        let testdir = tempdir().expect("failed to create tmpdir"); // mock /sys
        let syspci = testdir.path().join("bus").join("pci");

        // Mock dev0, which has no group
        let dev0 = pci::Address::new(0, 0, pci::SlotFn::new(0, 0).unwrap());
        let dev0path = syspci.join("devices").join(dev0.to_string());

        fs::create_dir_all(dev0path).unwrap();

        // Test dev0
        assert!(pci_iommu_group(&syspci, dev0).unwrap().is_none());

        // Mock dev1, which is in group 12
        let dev1 = pci::Address::new(0, 1, pci::SlotFn::new(0, 0).unwrap());
        let dev1path = syspci.join("devices").join(dev1.to_string());
        let dev1group = dev1path.join("iommu_group");

        fs::create_dir_all(&dev1path).unwrap();
        std::os::unix::fs::symlink("../../../kernel/iommu_groups/12", dev1group).unwrap();

        // Test dev1
        assert_eq!(
            pci_iommu_group(&syspci, dev1).unwrap(),
            Some(IommuGroup(12))
        );

        // Mock dev2, which has a bogus group (dir instead of symlink)
        let dev2 = pci::Address::new(0, 2, pci::SlotFn::new(0, 0).unwrap());
        let dev2path = syspci.join("devices").join(dev2.to_string());
        let dev2group = dev2path.join("iommu_group");

        fs::create_dir_all(dev2group).unwrap();

        // Test dev2
        assert!(pci_iommu_group(&syspci, dev2).is_err());
    }

    #[cfg(target_arch = "s390x")]
    #[tokio::test]
    async fn test_vfio_ap_matcher() {
        let subsystem = "ap";
        let card = "0a";
        let relpath = format!("{}.0001", card);

        let mut uev = Uevent::default();
        uev.action = U_EVENT_ACTION_ADD.to_string();
        uev.subsystem = subsystem.to_string();
        uev.devpath = format!("{}/card{}/{}", AP_ROOT_BUS_PATH, card, relpath);

        let ap_address = ap::Address::from_str(&relpath).unwrap();
        let matcher = ApMatcher::new(ap_address);

        assert!(matcher.is_match(&uev));

        let mut uev_remove = uev.clone();
        uev_remove.action = U_EVENT_ACTION_REMOVE.to_string();
        assert!(!matcher.is_match(&uev_remove));

        let mut uev_other_device = uev.clone();
        uev_other_device.devpath = format!("{}/card{}/{}.0002", AP_ROOT_BUS_PATH, card, card);
        assert!(!matcher.is_match(&uev_other_device));
    }
}

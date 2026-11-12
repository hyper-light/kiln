//! Read-only Configuration Manager calls for the exact present PCI devnode.
#![allow(unsafe_code)]

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_PropertyW, CM_Get_Device_IDW, CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW,
    CR_NO_SUCH_DEVNODE, CR_SUCCESS, MAX_DEVICE_ID_LEN,
};
use windows_sys::Win32::Devices::Properties::{
    DEVPKEY_Device_Address, DEVPKEY_Device_BusNumber, DEVPKEY_Device_LocationInfo,
    DEVPROP_TYPE_INT32, DEVPROP_TYPE_STRING, DEVPROP_TYPE_UINT32,
};
use windows_sys::Win32::Foundation::DEVPROPKEY;

use super::{LOCATION_UNITS, corroborate, location};
use crate::{PciAddress, Unread};

pub(super) fn pci_for_instance(instance: &str) -> Result<PciAddress, Unread> {
    // MAX_DEVICE_ID_LEN includes the terminator. ASCII PCI instances fit this fixed frame;
    // unrecognized/non-PCI inputs never select the root devnode or another device.
    // cfgmgr32.h MAX_DEVICE_ID_LEN is 200 UTF16 units on both supported Windows targets.
    const INSTANCE_UNITS: usize = 200;
    let mut input = [0u16; INSTANCE_UNITS];
    if !instance
        .get(.."PCI\\VEN_".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("PCI\\VEN_"))
        || instance.len() >= input.len()
        || !instance.bytes().all(|b| (0x21..=0x7e).contains(&b))
    {
        return Err(Unread::Malformed);
    }
    for (byte, unit) in instance.bytes().zip(&mut input) {
        *unit = u16::from(byte);
    }
    let mut node = 0;
    // SAFETY: nonempty exact PCI instance in an initialized NUL-terminated fixed buffer;
    // output is writable. NORMAL only observes the current tree, never phantom/cancel-remove.
    status(unsafe { CM_Locate_DevNodeW(&mut node, input.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) })?;

    let mut returned = [0u16; INSTANCE_UNITS];
    // SAFETY: node came from the successful local lookup; buffer is initialized and has exactly
    // the advertised u16 capacity. This call only reads the actual devnode's instance identifier.
    status(unsafe { CM_Get_Device_IDW(node, returned.as_mut_ptr(), MAX_DEVICE_ID_LEN, 0) })?;
    let end = returned
        .iter()
        .position(|u| *u == 0)
        .ok_or(Unread::Malformed)?;
    let actual = returned.get(..end).ok_or(Unread::Malformed)?;
    if actual.len() != instance.len()
        || !actual
            .iter()
            .zip(instance.bytes())
            .all(|(a, b)| u8::try_from(*a).is_ok_and(|a| a.eq_ignore_ascii_case(&b)))
    {
        return Err(Unread::Ambiguous);
    }

    let mut text = [0u16; LOCATION_UNITS];
    let mut kind = 0;
    let mut bytes = u32::try_from(std::mem::size_of_val(&text)).map_err(|_| Unread::Malformed)?;
    // SAFETY: fixed property key and local devnode, initialized aligned UTF16 output of precisely
    // the advertised byte capacity, independent writable type/length outputs, reserved flags zero.
    status(unsafe {
        CM_Get_DevNode_PropertyW(
            node,
            &DEVPKEY_Device_LocationInfo,
            &mut kind,
            text.as_mut_ptr().cast(),
            &mut bytes,
            0,
        )
    })?;
    if kind != DEVPROP_TYPE_STRING {
        return Err(Unread::Malformed);
    }
    let pci = location(
        &text,
        usize::try_from(bytes).map_err(|_| Unread::Malformed)?,
    )?;
    corroborate(
        pci,
        integer(node, &DEVPKEY_Device_BusNumber)?,
        integer(node, &DEVPKEY_Device_Address)?,
    )
}

fn integer(node: u32, key: &DEVPROPKEY) -> Result<u32, Unread> {
    let mut value = 0u32;
    let mut kind = 0;
    let mut bytes = u32::try_from(std::mem::size_of_val(&value)).map_err(|_| Unread::Malformed)?;
    // SAFETY: same successful local devnode, borrowed static key, one initialized aligned u32
    // output, and initialized type/byte-count outputs. No native pointer escapes this call.
    status(unsafe {
        CM_Get_DevNode_PropertyW(
            node,
            key,
            &mut kind,
            (&mut value as *mut u32).cast(),
            &mut bytes,
            0,
        )
    })?;
    // Microsoft's SDK specifies UINT32; the property reference says INT32. Both losslessly encode
    // the accepted physical range; corroborate rejects negative/high-bit/out-of-range values.
    if !matches!(kind, DEVPROP_TYPE_UINT32 | DEVPROP_TYPE_INT32)
        || usize::try_from(bytes).ok() != Some(std::mem::size_of::<u32>())
    {
        return Err(Unread::Malformed);
    }
    Ok(value)
}

fn status(code: u32) -> Result<(), Unread> {
    match code {
        CR_SUCCESS => Ok(()),
        CR_NO_SUCH_DEVNODE => Err(Unread::Gone),
        code => Err(Unread::WindowsCm { code }),
    }
}

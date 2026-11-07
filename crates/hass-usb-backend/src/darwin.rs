//! macOS IOKit serial-port enumeration.
//!
//! The IOKit traversal and property lookup are adapted from `serialx 1.15.1`
//! (`src/darwin.rs`, Apache-2.0). The PyO3 module boundary was removed; the
//! native result is mapped into the local provider record.

#![allow(clippy::as_ptr_cast_mut)]

use std::ffi::CStr;

use core_foundation::{
    base::{CFType, TCFType, kCFAllocatorDefault},
    number::CFNumber,
    string::CFString,
};
use io_kit_sys::ret::kIOReturnSuccess;
use io_kit_sys::types::{io_iterator_t, io_object_t};
use io_kit_sys::{
    IOIteratorNext, IOObjectRelease, IOObjectRetain, IORegistryEntryCreateCFProperty,
    IORegistryEntrySearchCFProperty, IOServiceGetMatchingServices, IOServiceMatching,
    kIOMasterPortDefault, kIORegistryIterateParents, kIORegistryIterateRecursively,
};

use crate::{DarwinSerialPortRecord, UsbBackendError, map_darwin_serial_records};

const KIO_SERVICE_PLANE: &CStr = c"IOService";

struct IoObject(io_object_t);

impl IoObject {
    const fn from_raw(object: io_object_t) -> Self {
        Self(object)
    }

    const fn raw(&self) -> io_object_t {
        self.0
    }

    fn string_property(&self, key: &str) -> Option<String> {
        let cf_key = CFString::new(key);
        let value = unsafe {
            IORegistryEntryCreateCFProperty(
                self.0,
                cf_key.as_concrete_TypeRef() as _,
                kCFAllocatorDefault,
                0,
            )
        };
        if value.is_null() {
            return None;
        }
        let cf_type: CFType = unsafe { TCFType::wrap_under_create_rule(value) };
        cf_type
            .downcast::<CFString>()
            .map(|string| string.to_string())
    }

    fn search_parent_property(&self, key: &str) -> Option<CFType> {
        let cf_key = CFString::new(key);
        let value = unsafe {
            IORegistryEntrySearchCFProperty(
                self.0,
                KIO_SERVICE_PLANE.as_ptr() as *mut _,
                cf_key.as_concrete_TypeRef() as _,
                kCFAllocatorDefault,
                kIORegistryIterateRecursively | kIORegistryIterateParents,
            )
        };
        if value.is_null() {
            return None;
        }
        Some(unsafe { TCFType::wrap_under_create_rule(value) })
    }

    fn search_parent_string_property(&self, key: &str) -> Option<String> {
        self.search_parent_property(key)?
            .downcast::<CFString>()
            .map(|string| string.to_string())
    }

    fn search_parent_u16_property(&self, key: &str) -> Option<u16> {
        self.search_parent_property(key)?
            .downcast::<CFNumber>()
            .and_then(|number| number.to_i64().map(|value| value as u16))
    }
}

impl Clone for IoObject {
    fn clone(&self) -> Self {
        if self.0 != 0 {
            unsafe { IOObjectRetain(self.0) };
        }
        Self(self.0)
    }
}

impl Drop for IoObject {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe { IOObjectRelease(self.0) };
        }
    }
}

struct IoIterator(IoObject);

impl IoIterator {
    const fn from_raw(iterator: io_iterator_t) -> Self {
        Self(IoObject::from_raw(iterator))
    }
}

impl Iterator for IoIterator {
    type Item = IoObject;

    fn next(&mut self) -> Option<Self::Item> {
        let object = unsafe { IOIteratorNext(self.0.raw()) };
        (object != 0).then(|| IoObject::from_raw(object))
    }
}

pub(super) fn enumerate_iokit() -> Result<Vec<hass_usb::SerialPortInfo>, UsbBackendError> {
    let matching = unsafe { IOServiceMatching(c"IOSerialBSDClient".as_ptr()) };
    if matching.is_null() {
        return Err(UsbBackendError::Native(
            "IOServiceMatching returned null".to_owned(),
        ));
    }

    let mut iterator_raw: io_iterator_t = 0;
    let result =
        unsafe { IOServiceGetMatchingServices(kIOMasterPortDefault, matching, &mut iterator_raw) };
    if result != kIOReturnSuccess {
        return Err(UsbBackendError::Native(format!(
            "IOServiceGetMatchingServices failed: {result}"
        )));
    }

    let iterator = IoIterator::from_raw(iterator_raw);
    let records = iterator.filter_map(|service| serial_port_record(&service));
    Ok(map_darwin_serial_records(records))
}

fn serial_port_record(service: &IoObject) -> Option<DarwinSerialPortRecord> {
    let device = service.string_property("IOCalloutDevice")?;
    if let Some(vid) = service.search_parent_u16_property("idVendor") {
        Some(DarwinSerialPortRecord {
            device,
            vid: Some(vid),
            pid: service.search_parent_u16_property("idProduct"),
            serial_number: service.search_parent_string_property("kUSBSerialNumberString"),
            manufacturer: service.search_parent_string_property("kUSBVendorString"),
            product: service.search_parent_string_property("kUSBProductString"),
            bcd_device: service.search_parent_u16_property("bcdDevice"),
            interface_description: service.search_parent_string_property("kUSBString"),
            interface_num: service
                .search_parent_u16_property("bInterfaceNumber")
                .map(|value| value as u8),
        })
    } else {
        Some(DarwinSerialPortRecord {
            device,
            ..DarwinSerialPortRecord::default()
        })
    }
}

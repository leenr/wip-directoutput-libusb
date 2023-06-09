use std::{
    cell::OnceCell,
    io::Read,
    mem,
    sync::{Arc, Mutex, RwLock, Weak},
    time::Duration, thread::sleep,
};

use bitmask_enum::bitmask;
use num_enum::{IntoPrimitive, TryFromPrimitive, TryFromPrimitiveError};
use uuid::{self, Uuid};
use zerocopy::{AsBytes, FromBytes, Unaligned};

use crate::devices::ManagedDisplay;

struct DeviceHandlerWrapper<T: rusb::UsbContext> {
    libusb_handle: rusb::DeviceHandle<T>,
    hid_endpoint_address: u8,
    read_endpoint_address: u8,
    write_endpoint_address: u8,
}

#[allow(clippy::enum_variant_names)]
#[derive(IntoPrimitive, TryFromPrimitive)]
#[repr(u32)]
enum Request {
    FolderRemoved = 0x02, // ??WHAT??
    SaveFile = 0x03,
    SetImageFile = 0x04, // + DisplayFile
    SetImage = 0x06,
    DeleteFile = 0x07,
    StartServer = 0x09,
    SomeFactoryModeRequest = 0x0a, // ? i'm not sure
    ClearImage = 0x13,
    SetLed = 0x18,
}

impl<T: rusb::UsbContext> DeviceHandlerWrapper<T> {
    fn read_hid(&self, buf: &mut [u8], timeout: Duration) -> Result<usize, rusb::Error> {
        log::trace!("reading hid");
        self.libusb_handle
            .read_bulk(self.hid_endpoint_address, buf, timeout)
    }

    fn read_bulk(&self, buf: &mut [u8], timeout: Duration) -> Result<usize, rusb::Error> {
        log::trace!("reading bulk");
        self.libusb_handle
            .read_bulk(self.read_endpoint_address, buf, timeout)
    }

    fn write_bulk(&self, buf: &[u8], timeout: Duration) -> Result<usize, rusb::Error> {
        log::trace!("writing bulk");
        self.libusb_handle
            .write_bulk(self.write_endpoint_address, buf, timeout)
    }
}

struct UsbSaitekFipLcdInt<T: rusb::UsbContext> {
    handle: DeviceHandlerWrapper<T>,
    serial_number: String,
    device_type_uuid: Uuid,
    vendor_if_mutex: Mutex<()>,
}
struct UsbSaitekFipLcd<T: rusb::UsbContext> {
    libusb_device: rusb::Device<T>,
    int: Arc<RwLock<Option<UsbSaitekFipLcdInt<T>>>>,
}

impl<T: rusb::UsbContext> UsbSaitekFipLcdInt<T> {
    fn new(dev: &UsbSaitekFipLcd<T>) -> Result<UsbSaitekFipLcdInt<T>, rusb::Error> {
        let mut libusb_handle = dev.libusb_device.open()?;
        let device_descriptor = dev.libusb_device.device_descriptor()?;
        let config_descriptor = dev.libusb_device.active_config_descriptor()?;

        let mut interfaces = config_descriptor.interfaces();

        let hid_interface = interfaces
            .find(|interface| match interface.descriptors().next() {
                Some(desc) => desc.class_code() == rusb::constants::LIBUSB_CLASS_HID,
                None => false,
            })
            .expect("Cannot find HID interface of the device");
        let vendor_interface = interfaces
            .find(|interface| match interface.descriptors().next() {
                Some(desc) => desc.class_code() == rusb::constants::LIBUSB_CLASS_VENDOR_SPEC,
                None => false,
            })
            .expect("Cannot find vendor's interface of the device");

        _ = libusb_handle.detach_kernel_driver(hid_interface.number());
        libusb_handle.claim_interface(hid_interface.number())?;

        _ = libusb_handle.detach_kernel_driver(vendor_interface.number());
        libusb_handle.claim_interface(vendor_interface.number())?;

        let serial_number = {
            let langs = libusb_handle.read_languages(std::time::Duration::from_secs(5))?;
            libusb_handle
                .read_serial_number_string(
                    langs[0],
                    &device_descriptor,
                    std::time::Duration::from_secs(1),
                )?
        };

        // seems like that is just a harcoded uuid
        // with no way of retreiving it from device itself, but I may be wrong
        let device_type_uuid = uuid::uuid!("3E083CD8-6A37-4A58-80A8-3D6A2C07513E");

        let hid_endpoint_address: OnceCell<u8> = OnceCell::new();
        hid_interface
            .descriptors()
            .next()
            .expect("Cannot read device HID interface descriptors")
            .endpoint_descriptors()
            .for_each(|endpoint| match endpoint.direction() {
                rusb::Direction::In => hid_endpoint_address
                    .set(endpoint.address())
                    .expect("Found multiple IN endpoints"),
                rusb::Direction::Out => (),
            });

        let read_endpoint_address: OnceCell<u8> = OnceCell::new();
        let write_endpoint_address: OnceCell<u8> = OnceCell::new();
        vendor_interface
            .descriptors()
            .next()
            .expect("Cannot read device vendor's interface descriptors")
            .endpoint_descriptors()
            .for_each(|endpoint| match endpoint.direction() {
                rusb::Direction::In => read_endpoint_address
                    .set(endpoint.address())
                    .expect("Found multiple IN endpoints"),
                rusb::Direction::Out => write_endpoint_address
                    .set(endpoint.address())
                    .expect("Found multiple OUT endpoints"),
            });

        log::info!(
            "Saitek FIP device initialized (serial number: {:?}, type uuid: {:?})",
            serial_number,
            device_type_uuid
        );

        Ok(UsbSaitekFipLcdInt {
            handle: DeviceHandlerWrapper {
                libusb_handle,
                hid_endpoint_address: *hid_endpoint_address
                    .get()
                    .expect("Could not find HID endpoint"),
                read_endpoint_address: *read_endpoint_address
                    .get()
                    .expect("Could not find IN endpoint"),
                write_endpoint_address: *write_endpoint_address
                    .get()
                    .expect("Could not find OUT endpoint"),
            },
            serial_number,
            device_type_uuid,
            vendor_if_mutex: Mutex::default(),
        })
    }
}

type BEU32 = zerocopy::byteorder::U32<zerocopy::byteorder::BigEndian>;

#[derive(AsBytes, Debug, FromBytes, Unaligned)]
#[repr(C)]
struct ControlPacket {
    server_id: BEU32,
    page: BEU32,
    data_size: BEU32,
    header_error: BEU32,
    header_info: BEU32,
    request: BEU32,
    param_1: BEU32, // led page? / ???????
    param_2: BEU32, // led index / ???????
    param_3: BEU32, // led value / file id
    request_error: BEU32,
    request_info: BEU32,
}
impl ControlPacket {
    #[inline(always)]
    fn server_id(&self) -> u32 {
        self.server_id.get()
    }
    #[inline(always)]
    fn set_server_id(&mut self, value: u32) {
        self.server_id = value.into()
    }

    #[inline(always)]
    fn page(&self) -> u8 {
        self.page.get().try_into().expect("Got invalid `page`")
    }
    #[inline(always)]
    fn set_page(&mut self, value: u8) {
        self.page = <u32>::into(value.into())
    }

    #[inline(always)]
    fn data_size(&self) -> usize {
        self.data_size.get() as usize
    }
    #[inline(always)]
    fn set_data_size(&mut self, value: usize) {
        self.data_size = (value as u32).into()
    }

    #[inline(always)]
    fn header_error(&self) -> u32 {
        self.header_error.get()
    }
    #[inline(always)]
    fn set_header_error(&mut self, value: u32) {
        self.header_error = value.into()
    }

    #[inline(always)]
    fn header_info(&self) -> u32 {
        self.header_info.get()
    }
    #[inline(always)]
    fn set_header_info(&mut self, value: u32) {
        self.header_info = value.into()
    }

    #[inline(always)]
    fn request(&self) -> Result<Request, TryFromPrimitiveError<Request>> {
        Request::try_from(self.request.get())
    }
    #[inline(always)]
    fn set_request(&mut self, value: Request) {
        self.request = <u32>::into(value.into())
    }

    #[inline(always)]
    fn param_1(&self) -> u32 {
        self.param_1.get()
    }
    #[inline(always)]
    fn set_param_1(&mut self, value: u32) {
        self.param_1 = value.into()
    }

    #[inline(always)]
    fn param_2(&self) -> u32 {
        self.param_2.get()
    }
    #[inline(always)]
    fn set_param_2(&mut self, value: u32) {
        self.param_2 = value.into()
    }

    #[inline(always)]
    fn param_3(&self) -> u32 {
        self.param_3.get()
    }
    #[inline(always)]
    fn set_param_3(&mut self, value: u32) {
        self.param_3 = value.into()
    }

    #[inline(always)]
    fn request_error(&self) -> u32 {
        self.request_error.get()
    }
    #[inline(always)]
    fn set_request_error(&mut self, value: u32) {
        self.request_error = value.into()
    }

    #[inline(always)]
    fn request_info(&self) -> u32 {
        self.request_info.get()
    }
    #[inline(always)]
    fn set_request_info(&mut self, value: u32) {
        self.request_info = value.into()
    }

    fn has_error(&self) -> bool {
        self.header_error() > 0 || self.request_error() > 0
    }

    fn new(request: Request) -> ControlPacket {
        ControlPacket {
            server_id: 0.into(),
            page: 0.into(),
            data_size: 0.into(),
            header_error: 0.into(),
            header_info: 0.into(),
            request: <u32>::into(request.into()),
            param_1: 0.into(),
            param_2: 0.into(),
            param_3: 0.into(),
            request_error: 0.into(),
            request_info: 0.into(),
        }
    }
}

impl<T: rusb::UsbContext> UsbSaitekFipLcdInt<T> {
    fn _read(&self) -> Result<(ControlPacket, Option<Vec<u8>>), rusb::Error> {
        let control_packet_bytes = {
            // FIXME(leenr): get rid of initializing a slice somehow
            let mut buffer = [0_u8; mem::size_of::<ControlPacket>()];
            if self
                .handle
                .read_bulk(buffer.as_mut_slice(), Duration::from_secs(5))?
                == mem::size_of::<ControlPacket>()
            {
                Ok(buffer)
            } else {
                Err(rusb::Error::Other)
            }
        }?;
        let control_packet =
            ControlPacket::read_from(&control_packet_bytes as &[u8]).expect("Something strange");
        log::debug!("Read control packet from device: {:?}", control_packet);

        if control_packet.data_size() == 0 {
            Ok((control_packet, None))
        } else {
            if control_packet.data_size() >= 512 * 1024 {
                panic!("Too big data size");
            }
            let mut vec = Vec::with_capacity(control_packet.data_size());
            if self.handle.read_bulk(&mut vec, Duration::from_secs(5))?
                == control_packet.data_size()
            {
                Ok((control_packet, Some(vec)))
            } else {
                Err(rusb::Error::Other)
            }
        }
    }

    fn _write(
        &self,
        control_packet: ControlPacket,
        data: Option<&[u8]>,
    ) -> Result<(), rusb::Error> {
        if data.unwrap_or(&[]).len() != control_packet.data_size() {
            panic!("Data size is not the same as the data size in the packet");
        }

        let buffer = control_packet.as_bytes();
        log::debug!("Write control packet to device: {:?}", control_packet);
        if self.handle.write_bulk(buffer, Duration::from_secs(5))? != buffer.len() {
            return Err(rusb::Error::Other);
        }

        if let Some(data) = data && !data.is_empty() {
            log::debug!("Write data of len {:?} to device", data.len());
            if self.handle.write_bulk(data, Duration::from_secs(5))? != data.len() {
                return Err(rusb::Error::Other);
            }
        };
        Ok(())
    }

    fn transcieve(
        &self,
        control_packet: ControlPacket,
        data: Option<&[u8]>,
    ) -> Result<(ControlPacket, Option<Vec<u8>>), rusb::Error> {
        let mutex = self.vendor_if_mutex.lock();
        self._write(control_packet, data)?;
        self._read()
    }
}

#[bitmask(u16)]
enum Buttons {
    S1 = 0b_00000001_00000000,
    S2 = 0b_00000010_00000000,
    S3 = 0b_00000100_00000000,
    S4 = 0b_00001000_00000000,
    S5 = 0b_00010000_00000000,
    S6 = 0b_00100000_00000000,
    LEFT_ANTICLOCKWISE = 0b_01000000_00000000,
    LEFT_CLOCKWISE = 0b_10000000_00000000,
    UP = 0b_00000000_00000001,
    DOWN = 0b_00000000_00000010,
    RIGHT_ANTICLOCKWISE = 0b_00000000_00000100,
    RIGHT_CLOCKWISE = 0b_00000000_00001000,
}

impl<T: rusb::UsbContext> UsbSaitekFipLcd<T> {
    fn transmit(
        &self,
        control_packet: ControlPacket,
        data: Option<&[u8]>,
    ) -> Result<(ControlPacket, Option<Vec<u8>>), rusb::Error> {
        let int_guard = self.int.read().expect("Device is poisoned");
        let int = int_guard
            .as_ref()
            .expect("Device is gone or not initialized yet");
        int.transcieve(control_packet, data)
    }

    fn _thread_target(device_weak: Weak<UsbSaitekFipLcd<T>>) {
        let Some(device) = device_weak.upgrade() else { return };
        let device_int = match UsbSaitekFipLcdInt::new(&device) {
            Ok(device_int) => device_int,
            Err(rusb::Error::Access) => {
                sleep(Duration::from_secs(1));
                UsbSaitekFipLcdInt::new(&device).expect("Cannot open device")
            }
            Err(_) => panic!("Cannot open device")
        };

        let (response, _) = device_int
            .transcieve(ControlPacket::new(Request::SomeFactoryModeRequest), None)
            .expect("Could not transcieve with the device");
        if !response.has_error() {
            log::warn!("Device is set to 'Factory Mode', whatever that means - skipping it");
            return;
        }

        _ = device
            .int
            .write()
            .expect("Device is poisoned")
            .replace(device_int);

        let mut hid_buffer: [u8; 2] = [0, 0];

        loop {
            let device = match device_weak.upgrade() {
                Some(device) => device,
                None => return, // device is dropped
            };
            match device
                .int
                .read()
                .expect("Device is poisoned")
                .as_ref()
                .unwrap()
                .handle
                .read_hid(&mut hid_buffer, Duration::from_secs(5))
            {
                Ok(_) => {
                    let buttons = Buttons::from(
                        <zerocopy::U16<zerocopy::BigEndian>>::from_bytes(hid_buffer).get(),
                    );
                    log::debug!("Got HID buttons: {:#?}", buttons);
                    // TODO
                }
                Err(rusb::Error::Timeout) => {
                    continue;
                }
                Err(rusb::Error::NoDevice) => {
                    log::info!("Device is disconnected, invalidating it");
                    if let Ok(mut guard) = device.int.write() {
                        drop(guard.take()); // invalidate the device
                    }
                }
                Err(err) => {
                    log::error!("Could not read from device ({}), invalidating it", err);
                    if let Ok(mut guard) = device.int.write() {
                        drop(guard.take()); // invalidate the device
                    }
                }
            };
            drop(device);
        }
    }
}

pub fn new_from_libusb<T: rusb::UsbContext + 'static>(
    libusb_device: rusb::Device<T>,
) -> Arc<dyn ManagedDisplay> {
    let device = Arc::new(UsbSaitekFipLcd {
        libusb_device: libusb_device.clone(),
        int: Arc::default(),
    });

    let device_ref = Arc::downgrade(&device);
    std::thread::Builder::new()
        .name(format!(
            "Saitek FIP @ {:03}-{:03}",
            libusb_device.bus_number(),
            libusb_device.address()
        ))
        .spawn(|| UsbSaitekFipLcd::_thread_target(device_ref))
        .expect("Could not start device thread");

    device
}

impl<T: rusb::UsbContext> ManagedDisplay for UsbSaitekFipLcd<T> {
    fn ready(&self) -> bool {
        self.int.read().is_ok_and(|int| int.is_some())
    }

    fn serial_number(&self) -> String {
        let int_guard = self.int.read().expect("Device is poisoned");
        let int = int_guard
            .as_ref()
            .expect("Device is gone or not initialized yet");
        int.serial_number.clone()
    }

    fn device_type_uuid(&self) -> Uuid {
        let int_guard = self.int.read().expect("Device is poisoned");
        let int = int_guard
            .as_ref()
            .expect("Device is gone or not initialized yet");
        int.device_type_uuid
    }

    fn set_image_data(&self, page: u8, data: &[u8; 0x38400]) -> Result<(), ()> {
        let mut packet = ControlPacket::new(Request::SetImage);
        packet.set_page(page);
        packet.set_data_size(data.len());
        let (packet, _) = self.transmit(packet, Some(data)).map_err(|_| ())?; // TODO: error
        match packet.has_error() {
            false => Ok(()),
            true => Err(()), // TODO
        }
    }

    fn set_led(&self, page: u8, index: u8, value: bool) -> Result<(), ()> {
        let mut packet = ControlPacket::new(Request::SetLed);
        packet.set_param_1(page.into());
        packet.set_param_2(index.into());
        packet.set_param_3(value.into());
        let (packet, _) = self.transmit(packet, None).map_err(|_| ())?; // TODO: error
        match packet.has_error() {
            false => Ok(()),
            true => Err(()), // TODO
        }
    }

    fn clear_image(&self, page: u8) -> Result<(), ()> {
        let mut packet = ControlPacket::new(Request::ClearImage);
        packet.set_page(page);
        let (packet, _) = self.transmit(packet, None).map_err(|_| ())?; // TODO: error
        match packet.has_error() {
            false => Ok(()),
            true => Err(()), // TODO
        }
    }

    fn save_file(&self, page: u8, file: u8, data: &mut dyn Read) -> Result<(), ()> {
        let mut packet = ControlPacket::new(Request::SaveFile);
        packet.set_param_1(page.into());
        packet.set_param_3(file.into());

        let mut buffer = Vec::new();
        if let Err(err) = data.read_to_end(&mut buffer) {
            log::error!("Cannot read data: {:?}", err);
            return Err(());
        }
        packet.set_data_size(buffer.len());

        let (packet, _) = self
            .transmit(packet, Some(buffer.as_slice()))
            .map_err(|_| ())?; // TODO: error
        match packet.has_error() {
            false => Ok(()),
            true => Err(()), // TODO
        }
    }

    fn display_file(&self, page: u8, index: u8, file: u8) -> Result<(), ()> {
        let mut packet = ControlPacket::new(Request::SaveFile);
        packet.set_param_1(page.into());
        packet.set_param_2(index.into());
        packet.set_param_3(file.into());
        let (packet, _) = self.transmit(packet, None).map_err(|_| ())?; // TODO: error
        match packet.has_error() {
            false => Ok(()),
            true => Err(()), // TODO
        }
    }

    fn delete_file(&self, page: u8, file: u8) -> Result<(), ()> {
        let mut packet = ControlPacket::new(Request::SaveFile);
        packet.set_param_1(page.into());
        packet.set_param_3(file.into());
        let (packet, _) = self.transmit(packet, None).map_err(|_| ())?; // TODO: error
        match packet.has_error() {
            false => Ok(()),
            true => Err(()), // TODO
        }
    }
}

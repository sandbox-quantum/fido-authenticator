mod pipe;

use std::{
    borrow::Cow,
    cell::RefCell,
    fmt::{self, Debug, Formatter},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Once,
    },
    thread,
    time::{Duration, SystemTime},
};

use ciborium::Value;
use ctap_types::ctap2::Operation;
use ctaphid::{
    error::{RequestError, ResponseError},
    HidDevice, HidDeviceInfo,
};
use ctaphid_dispatch::{
    dispatch::Dispatch,
    types::{Channel, Requester},
};
use fido_authenticator::{Authenticator, Config, Conforming};
use trussed_staging::virt;

use crate::webauthn::Request;

use pipe::Pipe;

static INIT_LOGGER: Once = Once::new();

pub fn run_ctaphid<F, T>(f: F) -> T
where
    F: FnOnce(ctaphid::Device<Device>) -> T + Send,
    T: Send,
{
    INIT_LOGGER.call_once(|| {
        env_logger::init();
    });
    virt::with_ram_client("fido", |client| {
        // TODO: setup attestation cert

        let mut authenticator = Authenticator::new(
            client,
            Conforming {},
            Config {
                max_msg_size: 0,
                skip_up_timeout: None,
                max_resident_credential_count: None,
                large_blobs: None,
                nfc_transport: false,
            },
        );

        let channel = Channel::new();
        let (rq, rp) = channel.split().unwrap();

        thread::scope(|s| {
            let stop = Arc::new(AtomicBool::new(false));
            let poller_stop = stop.clone();
            let poller = s.spawn(move || {
                let mut dispatch = Dispatch::new(rp);
                while !poller_stop.load(Ordering::Relaxed) {
                    dispatch.poll(&mut [&mut authenticator]);
                    thread::sleep(Duration::from_millis(10));
                }
            });

            let runner = s.spawn(move || {
                let device = Device::new(rq);
                let device = ctaphid::Device::new(device, DeviceInfo).unwrap();
                f(device)
            });

            let result = runner.join();
            stop.store(true, Ordering::Relaxed);
            poller.join().unwrap();
            result.unwrap()
        })
    })
}

pub fn run_ctap2<F, T>(f: F) -> T
where
    F: FnOnce(Ctap2) -> T + Send,
    T: Send,
{
    run_ctaphid(|device| f(Ctap2(device)))
}

pub struct Ctap2<'a>(ctaphid::Device<Device<'a>>);

impl Ctap2<'_> {
    pub fn exec<R: Request>(&self, request: R) -> Result<R::Reply, Ctap2Error> {
        let operation = Operation::try_from(R::COMMAND)
            .map(|op| format!("{op:?}"))
            .unwrap_or_else(|_| "?".to_owned());
        log::info!("Executing command {:#x} ({})", R::COMMAND, operation);
        let request = request.into();
        log::debug!("Sending request {request:?}");
        let mut serialized = Vec::new();
        ciborium::into_writer(&request, &mut serialized).unwrap();
        let reply = self
            .0
            .ctap2(R::COMMAND, &serialized)
            .map_err(|err| match err {
                ctaphid::error::Error::CommandError(ctaphid::error::CommandError::CborError(
                    value,
                )) => {
                    log::warn!("Received CTAP2 error {value:#x}");
                    Ctap2Error(value)
                }
                err => panic!("failed to execute CTAP2 command: {err:?}"),
            })?;
        let value: Value = if reply.is_empty() {
            Value::Map(Vec::new())
        } else {
            ciborium::from_reader(reply.as_slice()).unwrap()
        };
        log::debug!("Received reply {value:?}");
        Ok(value.into())
    }
}

#[derive(PartialEq)]
pub struct Ctap2Error(pub u8);

impl Debug for Ctap2Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Ctap2Error")
            .field(&format_args!("{:#x}", self.0))
            .finish()
    }
}

#[derive(Debug)]
pub struct DeviceInfo;

impl HidDeviceInfo for DeviceInfo {
    fn vendor_id(&self) -> u16 {
        0x20a0
    }

    fn product_id(&self) -> u16 {
        0x42b2
    }

    fn path(&self) -> Cow<'_, str> {
        "test".into()
    }
}

pub struct Device<'a>(RefCell<Pipe<'a>>);

impl<'a> Device<'a> {
    fn new(requester: Requester<'a>) -> Self {
        Self(RefCell::new(Pipe::new(requester)))
    }
}

impl HidDevice for Device<'_> {
    type Info = DeviceInfo;

    fn send(&self, data: &[u8]) -> Result<(), RequestError> {
        self.0.borrow_mut().push(data);
        Ok(())
    }

    fn receive<'a>(
        &self,
        buffer: &'a mut [u8],
        timeout: Option<Duration>,
    ) -> Result<&'a [u8], ResponseError> {
        let start = SystemTime::now();

        loop {
            if let Some(timeout) = timeout {
                let elapsed = start.elapsed().unwrap();
                if elapsed >= timeout {
                    return Err(ResponseError::Timeout);
                }
            }

            if let Some(response) = self.0.borrow_mut().pop() {
                return if buffer.len() >= response.len() {
                    log::info!("received response: {} bytes", response.len());
                    buffer[..response.len()].copy_from_slice(&response);
                    Ok(&buffer[..response.len()])
                } else {
                    Err(ResponseError::PacketReceivingFailed(
                        "invalid buffer size".into(),
                    ))
                };
            }

            thread::sleep(Duration::from_millis(10));
        }
    }
}

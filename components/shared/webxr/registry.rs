/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use embedder_traits::EventLoopWaker;
use log::warn;
#[cfg(feature = "ipc")]
use serde::{Deserialize, Serialize};

use crate::{
    DiscoveryAPI, Error, Frame, GLTypes, LayerGrandManager, MainThreadSession, MockDeviceInit,
    MockDeviceMsg, MockDiscoveryAPI, Session, SessionBuilder, SessionId, SessionInit, SessionMode,
    WebXrReceiver, WebXrSender,
};

#[derive(Clone)]
#[cfg_attr(feature = "ipc", derive(Serialize, Deserialize))]
pub struct Registry {
    sender: WebXrSender<RegistryMsg>,
    waker: MainThreadWakerImpl,
}

pub struct MainThreadRegistry<GL> {
    discoveries: Vec<Box<dyn DiscoveryAPI<GL>>>,
    sessions: Vec<Box<dyn MainThreadSession>>,
    mocks: Vec<Box<dyn MockDiscoveryAPI<GL>>>,
    sender: WebXrSender<RegistryMsg>,
    receiver: WebXrReceiver<RegistryMsg>,
    waker: MainThreadWakerImpl,
    grand_manager: LayerGrandManager<GL>,
    next_session_id: u32,
}

#[derive(Clone)]
#[cfg_attr(feature = "ipc", derive(Serialize, Deserialize))]
struct MainThreadWakerImpl {
    #[cfg(feature = "ipc")]
    sender: WebXrSender<()>,
    #[cfg(not(feature = "ipc"))]
    waker: Box<dyn EventLoopWaker>,
}

#[cfg(feature = "ipc")]
impl MainThreadWakerImpl {
    fn new(waker: Box<dyn EventLoopWaker>) -> Result<MainThreadWakerImpl, Error> {
        let (sender, receiver) = crate::webxr_channel().or(Err(Error::CommunicationError))?;
        ipc_channel::router::ROUTER.add_typed_route(receiver, Box::new(move |_| waker.wake()));
        Ok(MainThreadWakerImpl { sender })
    }

    fn wake(&self) {
        let _ = self.sender.send(());
    }
}

#[cfg(not(feature = "ipc"))]
impl MainThreadWakerImpl {
    fn new(waker: Box<dyn EventLoopWaker>) -> Result<MainThreadWakerImpl, Error> {
        Ok(MainThreadWakerImpl { waker })
    }

    pub fn wake(&self) {
        self.waker.wake()
    }
}

impl Registry {
    pub fn supports_session(&mut self, mode: SessionMode, dest: WebXrSender<Result<(), Error>>) {
        let _ = self.sender.send(RegistryMsg::SupportsSession(mode, dest));
        self.waker.wake();
    }

    pub fn request_session(
        &mut self,
        mode: SessionMode,
        init: SessionInit,
        dest: WebXrSender<Result<Session, Error>>,
        animation_frame_handler: WebXrSender<Frame>,
    ) {
        let _ = self.sender.send(RegistryMsg::RequestSession(
            mode,
            init,
            dest,
            animation_frame_handler,
        ));
        self.waker.wake();
    }

    pub fn simulate_device_connection(
        &mut self,
        init: MockDeviceInit,
        dest: WebXrSender<Result<WebXrSender<MockDeviceMsg>, Error>>,
    ) {
        let _ = self
            .sender
            .send(RegistryMsg::SimulateDeviceConnection(init, dest));
        self.waker.wake();
    }
}

impl<GL: 'static + GLTypes> MainThreadRegistry<GL> {
    pub fn new(
        waker: Box<dyn EventLoopWaker>,
        grand_manager: LayerGrandManager<GL>,
    ) -> Result<Self, Error> {
        let (sender, receiver) = crate::webxr_channel().or(Err(Error::CommunicationError))?;
        let discoveries = Vec::new();
        let sessions = Vec::new();
        let mocks = Vec::new();
        let waker = MainThreadWakerImpl::new(waker)?;
        Ok(MainThreadRegistry {
            discoveries,
            sessions,
            mocks,
            sender,
            receiver,
            waker,
            grand_manager,
            next_session_id: 0,
        })
    }

    pub fn registry(&self) -> Registry {
        Registry {
            sender: self.sender.clone(),
            waker: self.waker.clone(),
        }
    }

    pub fn register<D>(&mut self, discovery: D)
    where
        D: DiscoveryAPI<GL>,
    {
        self.discoveries.push(Box::new(discovery));
    }

    pub fn register_mock<D>(&mut self, discovery: D)
    where
        D: MockDiscoveryAPI<GL>,
    {
        self.mocks.push(Box::new(discovery));
    }

    pub fn run_on_main_thread<S>(&mut self, session: S)
    where
        S: MainThreadSession,
    {
        self.sessions.push(Box::new(session));
    }

    pub fn run_one_frame(&mut self) {
        while let Ok(msg) = self.receiver.try_recv() {
            self.handle_msg(msg);
        }
        for session in &mut self.sessions {
            session.run_one_frame();
        }
        self.sessions.retain(|session| session.running());
    }

    pub fn running(&self) -> bool {
        self.sessions.iter().any(|session| session.running())
    }

    fn handle_msg(&mut self, msg: RegistryMsg) {
        match msg {
            RegistryMsg::SupportsSession(mode, dest) => {
                let _ = dest.send(self.supports_session(mode));
            },
            RegistryMsg::RequestSession(mode, init, dest, raf_sender) => {
                let _ = dest.send(self.request_session(mode, init, raf_sender));
            },
            RegistryMsg::SimulateDeviceConnection(init, dest) => {
                let _ = dest.send(self.simulate_device_connection(init));
            },
        }
    }

    fn supports_session(&mut self, mode: SessionMode) -> Result<(), Error> {
        for discovery in &self.discoveries {
            if discovery.supports_session(mode) {
                return Ok(());
            }
        }
        Err(Error::NoMatchingDevice)
    }

    fn request_session(
        &mut self,
        mode: SessionMode,
        init: SessionInit,
        raf_sender: WebXrSender<Frame>,
    ) -> Result<Session, Error> {
        for discovery in &mut self.discoveries {
            if discovery.supports_session(mode) {
                let raf_sender = raf_sender.clone();
                let id = SessionId(self.next_session_id);
                self.next_session_id += 1;
                let xr = SessionBuilder::new(
                    &mut self.sessions,
                    raf_sender,
                    self.grand_manager.clone(),
                    id,
                );
                match discovery.request_session(mode, &init, xr) {
                    Ok(session) => return Ok(session),
                    Err(err) => warn!("XR device error {:?}", err),
                }
            }
        }
        warn!("no device could support the session");
        Err(Error::NoMatchingDevice)
    }

    fn simulate_device_connection(
        &mut self,
        init: MockDeviceInit,
    ) -> Result<WebXrSender<MockDeviceMsg>, Error> {
        for mock in &mut self.mocks {
            let (sender, receiver) = crate::webxr_channel().or(Err(Error::CommunicationError))?;
            if let Ok(discovery) = mock.simulate_device_connection(init.clone(), receiver) {
                self.discoveries.insert(0, discovery);
                return Ok(sender);
            }
        }
        Err(Error::NoMatchingDevice)
    }
}

#[cfg_attr(feature = "ipc", derive(Serialize, Deserialize))]
#[allow(clippy::large_enum_variant)]
enum RegistryMsg {
    RequestSession(
        SessionMode,
        SessionInit,
        WebXrSender<Result<Session, Error>>,
        WebXrSender<Frame>,
    ),
    SupportsSession(SessionMode, WebXrSender<Result<(), Error>>),
    SimulateDeviceConnection(
        MockDeviceInit,
        WebXrSender<Result<WebXrSender<MockDeviceMsg>, Error>>,
    ),
}

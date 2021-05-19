use crate::rpc::{
    request::{self, Msg, RuntimeContext},
    Request, ServiceBus,
};
use crate::walletd::NodeSecrets;
use crate::Senders;
use crate::{Config, CtlServer, Error, Service, ServiceId};
use bitcoin::secp256k1;
use internet2::{LocalNode, TypedEnum};
use microservices::esb::{self, Handler};
use request::{NodeId, Secret};

use crate::LogStyle;

pub fn run(
    config: Config,
    walletd_token: String,
    node_secrets: NodeSecrets,
    node_id: bitcoin::secp256k1::PublicKey,
) -> Result<(), Error> {
    let runtime = Runtime {
        identity: ServiceId::Wallet,
        walletd_token,
        node_secrets,
        node_id,
    };

    Service::run(config, runtime, false)
}

pub struct Runtime {
    identity: ServiceId,
    walletd_token: String,
    node_secrets: NodeSecrets,
    node_id: bitcoin::secp256k1::PublicKey,
}

impl CtlServer for Runtime {}

impl esb::Handler<ServiceBus> for Runtime {
    type Request = Request;
    type Address = ServiceId;
    type Error = Error;

    fn identity(&self) -> ServiceId {
        self.identity.clone()
    }

    fn handle(
        &mut self,
        senders: &mut esb::SenderList<ServiceBus, ServiceId>,
        bus: ServiceBus,
        source: ServiceId,
        request: Request,
    ) -> Result<(), Self::Error> {
        match bus {
            ServiceBus::Msg => self.handle_rpc_msg(senders, source, request),
            ServiceBus::Ctl => self.handle_rpc_ctl(senders, source, request),
            _ => Err(Error::NotSupported(ServiceBus::Bridge, request.get_type())),
        }
    }

    fn handle_err(&mut self, _: esb::Error) -> Result<(), esb::Error> {
        // We do nothing and do not propagate error; it's already being reported
        // with `error!` macro by the controller. If we propagate error here
        // this will make whole daemon panic
        Ok(())
    }
}

impl Runtime {
    fn send_farcasterd(
        &self,
        senders: &mut Senders,
        message: request::Request,
    ) -> Result<(), Error> {
        senders.send_to(
            ServiceBus::Ctl,
            self.identity(),
            ServiceId::Farcasterd,
            message,
        )?;
        Ok(())
    }

    fn handle_rpc_msg(
        &mut self,
        _senders: &mut Senders,
        _source: ServiceId,
        request: Request,
    ) -> Result<(), Error> {
        match request {
            Request::Hello => {
                // Ignoring; this is used to set remote identity at ZMQ level
            }
            _ => {
                error!("MSG RPC can only be used for farwarding LNPBP messages")
            }
        }
        Ok(())
    }

    fn handle_rpc_ctl(
        &mut self,
        senders: &mut Senders,
        source: ServiceId,
        request: Request,
    ) -> Result<(), Error> {
        match request {
            Request::GetSecret(request) => {
                if request.0 != self.walletd_token {
                    Err(Error::InvalidToken)?
                }
                let secrets = Secret(self.node_secrets.clone(), request.1);
                info!("sent Secret request to farcasterd");
                self.send_farcasterd(senders, Request::Secret(secrets))?
            }
            Request::GetNodeId => {
                let node_id = NodeId(self.node_id.clone());
                self.send_farcasterd(senders, Request::NodeId(node_id))?
            }

            Request::Loopback(request) => match request {
                RuntimeContext::GetInfo => self.send_farcasterd(senders, Request::GetInfo)?,
                RuntimeContext::MakeOffer(offer) => {
                    self.send_farcasterd(senders, Request::MakeOffer(offer))?
                }
                RuntimeContext::TakeOffer(offer) => {
                    self.send_farcasterd(senders, Request::TakeOffer(offer))?
                }
                RuntimeContext::Listen(addr) => {
                    self.send_farcasterd(senders, Request::Listen(addr))?
                }
                RuntimeContext::ConnectPeer(addr) => {
                    self.send_farcasterd(senders, Request::ConnectPeer(addr))?
                }
            },

            _ => {
                error!(
                    "Request {:?} is not supported by the CTL interface",
                    request
                );
            }
        }
        Ok(())
    }
}

// LNP Node: node running lightning network protocol and generalized lightning
// channels.
// Written in 2020 by
//     Dr. Maxim Orlovsky <orlovsky@pandoracore.com>
//
// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to

// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the MIT License
// along with this software.
// If not, see <https://opensource.org/licenses/MIT>.

use crate::service::Endpoints;
use crate::{
    rpc::request::Outcome,
    rpc::request::{BitcoinFundingInfo, FundingInfo, MoneroFundingInfo},
    syncerd::{
        opts::Coin, Abort, GetTx, HeightChanged, SweepAddress, SweepAddressAddendum, SweepSuccess,
        SweepXmrAddress, TaskId, TaskTarget, TransactionRetrieved, WatchHeight, XmrAddressAddendum,
    },
};
use std::{
    any::Any,
    collections::{BTreeMap, HashMap, HashSet},
    convert::TryInto,
};
use std::{convert::TryFrom, str::FromStr};
use std::{
    io::Cursor,
    time::{Duration, SystemTime},
};

use super::{
    storage::{self, Driver},
    swap_state::{AliceState, BobState, State},
    syncer_client::{log_tx_received, log_tx_seen, SyncerState, SyncerTasks},
    temporal_safety::TemporalSafety,
};
use crate::rpc::{
    request::{self, Msg},
    Request, ServiceBus,
};
use crate::{CtlServer, Error, LogStyle, Service, ServiceConfig, ServiceId};
use bitcoin::{consensus::Encodable, secp256k1};
use bitcoin::{
    hashes::{hex::FromHex, ripemd160, sha256, Hash, HashEngine},
    Txid,
};
use bitcoin::{
    util::psbt::{serialize::Deserialize, PartiallySignedTransaction},
    Script,
};

use crate::syncerd::types::{
    AddressAddendum, AddressTransaction, Boolean, BroadcastTransaction, BtcAddressAddendum, Event,
    Task, TransactionConfirmations, WatchAddress, WatchTransaction,
};
use farcaster_core::{
    bitcoin::{
        fee::SatPerVByte, segwitv0::LockTx, segwitv0::SegwitV0, timelock::CSVTimelock, Bitcoin,
        BitcoinSegwitV0,
    },
    blockchain::{self, FeeStrategy},
    bundle::{AliceParameters, BobParameters, Proof},
    consensus::{self, Encodable as FarEncodable},
    crypto::{CommitmentEngine, SharedKeyId, TaggedElement},
    monero::{Monero, SHARED_VIEW_KEY_ID},
    negotiation::{Offer, PublicOffer},
    protocol_message::{
        BuyProcedureSignature, CommitAliceParameters, CommitBobParameters, CoreArbitratingSetup,
        RefundProcedureSignatures,
    },
    role::{Arbitrating, SwapRole, TradeRole},
    swap::btcxmr::BtcXmr,
    swap::SwapId,
    transaction::{Broadcastable, Transaction, TxLabel, Witnessable},
};
use internet2::zmqsocket::{self, ZmqSocketAddr, ZmqType};
use internet2::{
    session, CreateUnmarshaller, NodeAddr, Session, TypedEnum, Unmarshall, Unmarshaller,
};
use lnpbp::chain::Chain;
use microservices::esb::{self, Handler};
use monero::{cryptonote::hash::keccak_256, PrivateKey, ViewPair};
use request::{
    Checkpoint, CheckpointChunk, CheckpointMultipartChunk, CheckpointState, Commit, InitSwap,
    Params, Reveal, TakeCommit, Tx,
};
use strict_encoding::{StrictDecode, StrictEncode};

pub fn run(
    config: ServiceConfig,
    swap_id: SwapId,
    public_offer: PublicOffer<BtcXmr>,
    local_trade_role: TradeRole,
) -> Result<(), Error> {
    let Offer {
        cancel_timelock,
        punish_timelock,
        maker_role, // SwapRole of maker (Alice or Bob)
        network,
        accordant_amount: monero_amount,
        arbitrating_amount: bitcoin_amount,
        ..
    } = public_offer.offer;
    // alice or bob
    let local_swap_role = match local_trade_role {
        TradeRole::Maker => maker_role,
        TradeRole::Taker => maker_role.other(),
    };

    let init_state = match local_swap_role {
        SwapRole::Alice => State::Alice(AliceState::StartA {
            local_trade_role,
            public_offer,
        }),
        SwapRole::Bob => State::Bob(BobState::StartB {
            local_trade_role,
            public_offer,
        }),
    };
    let sweep_monero_thr = 10;
    info!(
        "{}: {}",
        "Starting swap".to_string().bright_green_bold(),
        format!("{:#x}", swap_id).addr()
    );
    info!(
        "{} | Initial state: {}",
        swap_id.bright_blue_italic(),
        init_state.bright_white_bold()
    );

    let temporal_safety = TemporalSafety {
        cancel_timelock: cancel_timelock.as_u32(),
        punish_timelock: punish_timelock.as_u32(),
        btc_finality_thr: 1,
        race_thr: 3,
        xmr_finality_thr: 1,
        sweep_monero_thr,
    };

    temporal_safety.valid_params()?;
    let tasks = SyncerTasks {
        counter: 0,
        watched_addrs: none!(),
        watched_txs: none!(),
        retrieving_txs: none!(),
        sweeping_addr: none!(),
        txids: none!(),
        final_txs: none!(),
    };
    let syncer_state = SyncerState {
        swap_id,
        tasks,
        monero_height: 0,
        bitcoin_height: 0,
        confirmation_bound: 50000,
        lock_tx_confs: None,
        cancel_tx_confs: None,
        network,
        bitcoin_syncer: ServiceId::Syncer(Coin::Bitcoin, network),
        monero_syncer: ServiceId::Syncer(Coin::Monero, network),
        monero_amount,
        bitcoin_amount,
        awaiting_funding: false,
    };

    let runtime = Runtime {
        swap_id,
        identity: ServiceId::Swap(swap_id),
        peer_service: ServiceId::Loopback,
        state: init_state,
        maker_peer: None,
        started: SystemTime::now(),
        syncer_state,
        temporal_safety,
        enquirer: None,
        storage: Box::new(storage::DiskDriver::init(
            swap_id,
            Box::new(storage::DiskConfig {
                path: Default::default(),
            }),
        )?),
        pending_requests: none!(),
        pending_peer_request: none!(),
        pending_checkpoint_chunks: map![],
        txs: none!(),
    };
    let broker = false;
    Service::run(config, runtime, broker)
}

// FIXME: State enum should carry over the data that is accumulated over time,
// and corresponding lines should be removed from Runtime
pub struct Runtime {
    swap_id: SwapId,
    identity: ServiceId,
    peer_service: ServiceId,
    state: State,
    maker_peer: Option<NodeAddr>,
    started: SystemTime,
    enquirer: Option<ServiceId>,
    syncer_state: SyncerState,
    temporal_safety: TemporalSafety,
    pending_requests: HashMap<ServiceId, Vec<PendingRequest>>, // FIXME Something more meaningful than ServiceId to index
    pending_peer_request: Vec<request::Msg>, // Peer requests that failed and are waiting for reconnection
    pending_checkpoint_chunks: HashMap<[u8; 20], HashSet<CheckpointChunk>>,
    txs: HashMap<TxLabel, bitcoin::Transaction>,
    #[allow(dead_code)]
    storage: Box<dyn storage::Driver>,
}

#[derive(Debug, Clone)]
pub struct PendingRequest {
    dest: ServiceId,
    bus_id: ServiceBus,
    request: Request,
}

impl StrictEncode for PendingRequest {
    fn strict_encode<E: std::io::Write>(&self, mut e: E) -> Result<usize, strict_encoding::Error> {
        let mut len = self.dest.strict_encode(&mut e)?;
        len += self.bus_id.strict_encode(&mut e)?;
        len += self.request.serialize().strict_encode(&mut e)?;
        Ok(len)
    }
}

impl StrictDecode for PendingRequest {
    fn strict_decode<D: std::io::Read>(mut d: D) -> Result<Self, strict_encoding::Error> {
        let unmarshaller: Unmarshaller<Request> = Request::create_unmarshaller();
        let dest = ServiceId::strict_decode(&mut d)?;
        let bus_id = ServiceBus::strict_decode(&mut d)?;
        let request: Request = (&*unmarshaller
            .unmarshall(Cursor::new(Vec::<u8>::strict_decode(&mut d)?))
            .unwrap())
            .clone();
        Ok(PendingRequest {
            dest,
            bus_id,
            request,
        })
    }
}

#[derive(Debug, Clone, Display)]
#[display("checkpoint-swapd")]
pub struct CheckpointSwapd {
    pub state: State,
    pub last_msg: Msg,
    pub enquirer: Option<ServiceId>,
    pub temporal_safety: TemporalSafety,
    pub txs: HashMap<TxLabel, bitcoin::Transaction>,
    pub txids: HashMap<TxLabel, Txid>,
    pub pending_requests: HashMap<ServiceId, Vec<PendingRequest>>,
}

impl StrictEncode for CheckpointSwapd {
    fn strict_encode<E: std::io::Write>(&self, mut e: E) -> Result<usize, strict_encoding::Error> {
        let mut len = self.state.strict_encode(&mut e)?;
        len += self.last_msg.strict_encode(&mut e)?;
        len += self.enquirer.strict_encode(&mut e)?;
        len += self.temporal_safety.strict_encode(&mut e)?;

        len += self.txs.len().strict_encode(&mut e)?;
        let res: Result<usize, strict_encoding::Error> =
            self.txs.iter().try_fold(len, |mut acc, (key, val)| {
                acc += key.strict_encode(&mut e).map_err(|err| {
                    strict_encoding::Error::DataIntegrityError(format!("{}", err))
                })?;
                acc += val.strict_encode(&mut e).map_err(|err| {
                    strict_encoding::Error::DataIntegrityError(format!("{}", err))
                })?;
                Ok(acc)
            });
        len = match res {
            Ok(val) => Ok(val),
            Err(err) => Err(strict_encoding::Error::DataIntegrityError(format!(
                "{}",
                err
            ))),
        }?;

        len += self.txids.len().strict_encode(&mut e)?;
        let res: Result<usize, strict_encoding::Error> =
            self.txids.iter().try_fold(len, |mut acc, (key, val)| {
                acc += key.strict_encode(&mut e).map_err(|err| {
                    strict_encoding::Error::DataIntegrityError(format!("{}", err))
                })?;
                acc += val.strict_encode(&mut e).map_err(|err| {
                    strict_encoding::Error::DataIntegrityError(format!("{}", err))
                })?;
                Ok(acc)
            });
        len = match res {
            Ok(val) => Ok(val),
            Err(err) => Err(strict_encoding::Error::DataIntegrityError(format!(
                "{}",
                err
            ))),
        }?;

        len += self.pending_requests.len().strict_encode(&mut e)?;
        self.pending_requests
            .iter()
            .try_fold(len, |mut acc, (key, val)| {
                acc += key.strict_encode(&mut e)?;
                acc += val.strict_encode(&mut e)?;
                Ok(acc)
            })
    }
}

impl StrictDecode for CheckpointSwapd {
    fn strict_decode<D: std::io::Read>(mut d: D) -> Result<Self, strict_encoding::Error> {
        let state = State::strict_decode(&mut d)?;
        let last_msg = Msg::strict_decode(&mut d)?;
        let enquirer = Option::<ServiceId>::strict_decode(&mut d)?;
        let temporal_safety = TemporalSafety::strict_decode(&mut d)?;

        let len = usize::strict_decode(&mut d)?;
        let mut txs = HashMap::<TxLabel, bitcoin::Transaction>::new();
        for _ in 0..len {
            let key = TxLabel::strict_decode(&mut d)?;
            let val = bitcoin::Transaction::strict_decode(&mut d)?;
            if txs.contains_key(&key) {
                return Err(strict_encoding::Error::RepeatedValue(format!("{:?}", key)));
            }
            txs.insert(key, val);
        }

        let len = usize::strict_decode(&mut d)?;
        let mut txids = HashMap::<TxLabel, Txid>::new();
        for _ in 0..len {
            let key = TxLabel::strict_decode(&mut d)?;
            let val = Txid::strict_decode(&mut d)?;
            if txids.contains_key(&key) {
                return Err(strict_encoding::Error::RepeatedValue(format!("{:?}", key)));
            }
            txids.insert(key, val);
        }

        let len = usize::strict_decode(&mut d)?;
        let mut pending_requests = HashMap::<ServiceId, Vec<PendingRequest>>::new();
        for _ in 0..len {
            let key = ServiceId::strict_decode(&mut d)?;
            let val = Vec::<PendingRequest>::strict_decode(&mut d)?;
            if pending_requests.contains_key(&key) {
                return Err(strict_encoding::Error::RepeatedValue(format!("{:?}", key)));
            }
            pending_requests.insert(key, val);
        }
        Ok(CheckpointSwapd {
            state,
            last_msg,
            enquirer,
            temporal_safety,
            txs,
            txids,
            pending_requests,
        })
    }
}

impl CtlServer for Runtime {}

impl esb::Handler<ServiceBus> for Runtime {
    type Request = Request;
    type Error = Error;

    fn identity(&self) -> ServiceId {
        self.identity.clone()
    }

    fn handle(
        &mut self,
        endpoints: &mut Endpoints,
        bus: ServiceBus,
        source: ServiceId,
        request: Request,
    ) -> Result<(), Self::Error> {
        match bus {
            ServiceBus::Msg => self.handle_rpc_msg(endpoints, source, request),
            ServiceBus::Ctl => self.handle_rpc_ctl(endpoints, source, request),
            _ => Err(Error::NotSupported(ServiceBus::Bridge, request.get_type())),
        }
    }

    fn handle_err(&mut self, _: &mut Endpoints, _: esb::Error<ServiceId>) -> Result<(), Error> {
        // We do nothing and do not propagate error; it's already being reported
        // with `error!` macro by the controller. If we propagate error here
        // this will make whole daemon panic
        Ok(())
    }
}

impl Runtime {
    fn send_peer(&mut self, endpoints: &mut Endpoints, msg: request::Msg) -> Result<(), Error> {
        trace!("sending peer message {}", msg.bright_yellow_bold());
        if let Err(error) = endpoints.send_to(
            ServiceBus::Msg,
            self.identity(),
            self.peer_service.clone(), // = ServiceId::Loopback
            Request::Protocol(msg.clone()),
        ) {
            error!(
                "could not send message {} to {} due to {}",
                msg, self.peer_service, error
            );
            warn!("notifying farcasterd of peer error, farcasterd will attempt to reconnect");
            endpoints.send_to(
                ServiceBus::Ctl,
                self.identity(),
                ServiceId::Farcasterd,
                Request::PeerdUnreachable(self.peer_service.clone()),
            )?;
            self.pending_peer_request.push(msg);
        }
        Ok(())
    }

    fn swap_id(&self) -> SwapId {
        match self.identity {
            ServiceId::Swap(swap_id) => swap_id,
            _ => {
                unreachable!("not ServiceId::Swap")
            }
        }
    }

    fn state_update(&mut self, endpoints: &mut Endpoints, next_state: State) -> Result<(), Error> {
        info!(
            "{} | State transition: {} -> {}",
            self.swap_id.bright_blue_italic(),
            self.state.bright_white_bold(),
            next_state.bright_white_bold(),
        );
        let msg = format!("{} -> {}", self.state, next_state,);
        self.state = next_state;
        self.report_state_transition_progress_message_to(endpoints, self.enquirer.clone(), msg)?;
        Ok(())
    }

    fn broadcast(
        &mut self,
        tx: bitcoin::Transaction,
        tx_label: TxLabel,
        endpoints: &mut Endpoints,
    ) -> Result<(), Error> {
        let req = Request::SyncerTask(Task::BroadcastTransaction(BroadcastTransaction {
            id: self.syncer_state.tasks.new_taskid(),
            tx: bitcoin::consensus::serialize(&tx),
        }));

        info!(
            "{} | Broadcasting {} tx ({})",
            self.swap_id.bright_blue_italic(),
            tx_label.bright_white_bold(),
            tx.txid().bright_yellow_italic()
        );
        Ok(endpoints.send_to(
            ServiceBus::Ctl,
            self.identity(),
            self.syncer_state.bitcoin_syncer(),
            req,
        )?)
    }

    fn handle_rpc_msg(
        &mut self,
        endpoints: &mut Endpoints,
        source: ServiceId,
        request: Request,
    ) -> Result<(), Error> {
        if self.peer_service != source {
            return Err(Error::Farcaster(format!(
                "{}: expected {}, found {}",
                "Incorrect peer connection", self.peer_service, source
            )));
        }
        let msg_bus = ServiceBus::Msg;
        match &request {
            Request::Protocol(msg) => {
                if msg.swap_id() != self.swap_id() {
                    return Err(Error::Farcaster(format!(
                        "{}: expected {}, found {}",
                        "Incorrect swap_id ",
                        self.swap_id(),
                        msg.swap_id(),
                    )));
                }
                match &msg {
                    // we are taker and the maker committed, now we reveal after checking
                    // whether we're Bob or Alice and that we're on a compatible state
                    Msg::MakerCommit(remote_commit)
                        if self.state.commit()
                            && self.state.trade_role() == Some(TradeRole::Taker)
                            && self.state.remote_commit().is_none() =>
                    {
                        trace!("received remote commitment");
                        self.state.t_sup_remote_commit(remote_commit.clone());

                        if self.state.swap_role() == SwapRole::Bob {
                            let addr = self
                                .state
                                .b_address()
                                .cloned()
                                .expect("address available at CommitB");
                            let txlabel = TxLabel::Funding;
                            if !self.syncer_state.is_watched_addr(&txlabel) {
                                let task = self
                                    .syncer_state
                                    .watch_addr_btc(addr.script_pubkey(), txlabel);
                                self.send_ctl(
                                    endpoints,
                                    self.syncer_state.bitcoin_syncer(),
                                    Request::SyncerTask(task),
                                )?;
                            }
                        }

                        trace!("Watch height bitcoin");
                        let watch_height_bitcoin = Task::WatchHeight(WatchHeight {
                            id: self.syncer_state.tasks.new_taskid(),
                            lifetime: self.syncer_state.task_lifetime(Coin::Bitcoin),
                        });
                        endpoints.send_to(
                            ServiceBus::Ctl,
                            self.identity(),
                            self.syncer_state.bitcoin_syncer(),
                            Request::SyncerTask(watch_height_bitcoin),
                        )?;

                        trace!("Watch height monero");
                        let watch_height_monero = Task::WatchHeight(WatchHeight {
                            id: self.syncer_state.tasks.new_taskid(),
                            lifetime: self.syncer_state.task_lifetime(Coin::Monero),
                        });
                        endpoints.send_to(
                            ServiceBus::Ctl,
                            self.identity(),
                            self.syncer_state.monero_syncer(),
                            Request::SyncerTask(watch_height_monero),
                        )?;
                        self.send_wallet(msg_bus, endpoints, request)?;
                    }
                    Msg::TakerCommit(_) => {
                        unreachable!(
                            "msg handled by farcasterd/walletd, and indirectly here by \
                             Ctl Request::MakeSwap"
                        )
                    }
                    Msg::Reveal(Reveal::Proof(_)) => {
                        // These messages are saved as pending if Bob and then forwarded once the
                        // parameter reveal forward is triggered. If Alice, send immediately.
                        match self.state.swap_role() {
                            SwapRole::Bob => {
                                let pending_request = PendingRequest {
                                    request,
                                    dest: ServiceId::Wallet,
                                    bus_id: ServiceBus::Msg,
                                };
                                trace!("Added pending request to be forwarded later",);
                                if self
                                    .pending_requests
                                    .insert(ServiceId::Wallet, vec![pending_request])
                                    .is_some()
                                {
                                    error!(
                                        "Pending requests already existed prior to Reveal::Proof!"
                                    )
                                }
                            }
                            SwapRole::Alice => {
                                debug!("Alice: forwarding reveal");
                                trace!(
                                    "sending request {} to {} on bus {}",
                                    &request,
                                    &ServiceId::Wallet,
                                    &ServiceBus::Msg
                                );
                                self.send_wallet(msg_bus, endpoints, request)?
                            }
                        }
                    }
                    // bob and alice
                    // store parameters from counterparty if we have not received them yet.
                    // if we're maker, also reveal to taker if their commitment is valid.
                    Msg::Reveal(reveal)
                        if self.state.remote_commit().is_some()
                            && (self.state.commit() || self.state.reveal()) =>
                    {
                        // TODO: since we're not actually revealing, find other name for
                        // intermediary state

                        let remote_commit = self.state.remote_commit().cloned().unwrap();

                        if let Ok(remote_params_candidate) =
                            remote_params_candidate(reveal, remote_commit)
                        {
                            debug!("{:?} sets remote_params", self.state.swap_role());
                            self.state.sup_remote_params(remote_params_candidate);
                        } else {
                            error!("Revealed remote params not preimage of commitment");
                        }

                        // Specific to swap roles
                        // pass request on to wallet daemon so that it can set remote params
                        match self.state.swap_role() {
                            // validated state above, no need to check again
                            SwapRole::Alice => {
                                // Alice already sends RevealProof immediately, so only have to
                                // forward Reveal now
                                trace!(
                                    "sending request {} to {} on bus {}",
                                    &request,
                                    &ServiceId::Wallet,
                                    &ServiceBus::Msg
                                );
                                self.send_wallet(msg_bus, endpoints, request)?
                            }
                            SwapRole::Bob => {
                                // sending this request will initialize the
                                // arbitrating setup, that can be only performed
                                // after the funding tx was seen
                                let pending_request = PendingRequest {
                                    request,
                                    dest: ServiceId::Wallet,
                                    bus_id: ServiceBus::Msg,
                                };

                                trace!(
                                    "This pending request will be called later: {:?}",
                                    &pending_request
                                );
                                let pending_requests = self.pending_requests.get_mut(&ServiceId::Wallet)
                                    .expect("should already have received Reveal::Proof, so this key should exist.");
                                if pending_requests.len() != 1 {
                                    error!("should have a single pending Reveal::Proof only FIXME")
                                }
                                pending_requests.push(pending_request);

                                if let Some(address) = self.state.b_address().cloned() {
                                    let swap_id = self.swap_id();
                                    let fees = bitcoin::Amount::from_sat(200); // FIXME
                                    let amount = self.syncer_state.bitcoin_amount + fees;
                                    info!(
                                        "{} | Send {} to {}",
                                        swap_id.bright_blue_italic(),
                                        amount.bright_green_bold(),
                                        address.addr(),
                                    );
                                    let req = Request::FundingInfo(FundingInfo::Bitcoin(
                                        BitcoinFundingInfo {
                                            swap_id,
                                            address,
                                            amount,
                                        },
                                    ));
                                    self.syncer_state.awaiting_funding = true;

                                    if let Some(enquirer) = self.enquirer.clone() {
                                        endpoints.send_to(
                                            ServiceBus::Ctl,
                                            self.identity(),
                                            enquirer,
                                            req,
                                        )?
                                    }
                                }
                            }
                        }

                        // up to here for both maker and taker, following only Maker

                        // if did not yet reveal, maker only. on the msg flow as
                        // of 2021-07-13 taker reveals first
                        if self.state.commit() && self.state.trade_role() == Some(TradeRole::Maker)
                        {
                            if let Some(addr) = self.state.b_address().cloned() {
                                let txlabel = TxLabel::Funding;
                                if !self.syncer_state.is_watched_addr(&txlabel) {
                                    let watch_addr_task = self
                                        .syncer_state
                                        .watch_addr_btc(addr.script_pubkey(), txlabel);
                                    self.send_ctl(
                                        endpoints,
                                        self.syncer_state.bitcoin_syncer(),
                                        Request::SyncerTask(watch_addr_task),
                                    )?;
                                }
                            }
                            trace!("Watch height bitcoin");
                            let watch_height_bitcoin = Task::WatchHeight(WatchHeight {
                                id: self.syncer_state.tasks.new_taskid(),
                                lifetime: self.syncer_state.task_lifetime(Coin::Bitcoin),
                            });
                            endpoints.send_to(
                                ServiceBus::Ctl,
                                self.identity(),
                                self.syncer_state.bitcoin_syncer(),
                                Request::SyncerTask(watch_height_bitcoin),
                            )?;

                            trace!("Watch height monero");
                            let watch_height_monero = Task::WatchHeight(WatchHeight {
                                id: self.syncer_state.tasks.new_taskid(),
                                lifetime: self.syncer_state.task_lifetime(Coin::Monero),
                            });
                            endpoints.send_to(
                                ServiceBus::Ctl,
                                self.identity(),
                                self.syncer_state.monero_syncer(),
                                Request::SyncerTask(watch_height_monero),
                            )?;
                        }
                    }
                    // alice receives, bob sends
                    Msg::CoreArbitratingSetup(CoreArbitratingSetup {
                        swap_id,
                        lock,
                        cancel,
                        refund,
                        ..
                    }) if self.state.swap_role() == SwapRole::Alice
                        && self.state.reveal()
                        && swap_id == &self.swap_id() =>
                    {
                        for (&tx, tx_label) in [lock, cancel, refund].iter().zip([
                            TxLabel::Lock,
                            TxLabel::Cancel,
                            TxLabel::Refund,
                        ]) {
                            let tx = tx.clone().extract_tx();
                            let txid = tx.txid();
                            if !self.syncer_state.is_watched_tx(&tx_label) {
                                let task = self.syncer_state.watch_tx_btc(txid, tx_label);
                                endpoints.send_to(
                                    ServiceBus::Ctl,
                                    self.identity(),
                                    self.syncer_state.bitcoin_syncer(),
                                    Request::SyncerTask(task),
                                )?;
                            }
                            if tx_label == TxLabel::Refund {
                                self.syncer_state.tasks.txids.insert(TxLabel::Refund, txid);
                            }
                        }
                        self.send_wallet(msg_bus, endpoints, request)?;
                    }
                    // bob receives, alice sends
                    Msg::RefundProcedureSignatures(_) if self.state.b_core_arb() => {
                        self.send_wallet(msg_bus, endpoints, request)?;
                    }
                    // alice receives, bob sends
                    Msg::BuyProcedureSignature(buy_proc_sig) if self.state.a_refundsig() => {
                        // checkpoint swap alice pre buy
                        debug!(
                            "{} | checkpointing alice swapd state",
                            self.swap_id.bright_blue_italic()
                        );
                        checkpoint_state(
                            endpoints,
                            self.swap_id,
                            request::CheckpointState::CheckpointSwapd(CheckpointSwapd {
                                state: self.state.clone(),
                                last_msg: Msg::BuyProcedureSignature(buy_proc_sig.clone()),
                                enquirer: self.enquirer.clone(),
                                temporal_safety: self.temporal_safety.clone(),
                                txs: self.txs.clone(),
                                txids: self.syncer_state.tasks.txids.clone(),
                                pending_requests: self.pending_requests.clone(),
                            }),
                        )?;

                        // Alice verifies that she has sent refund procedure signatures before
                        // processing the buy signatures from Bob
                        let tx_label = TxLabel::Buy;
                        if !self.syncer_state.is_watched_tx(&tx_label) {
                            let txid = buy_proc_sig.buy.clone().extract_tx().txid();
                            let task = self.syncer_state.watch_tx_btc(txid, tx_label);
                            endpoints.send_to(
                                ServiceBus::Ctl,
                                self.identity(),
                                self.syncer_state.bitcoin_syncer(),
                                Request::SyncerTask(task),
                            )?;
                        }
                        self.send_wallet(msg_bus, endpoints, request)?
                    }

                    // bob and alice
                    Msg::Abort(_) => {
                        return Err(Error::Farcaster("Abort not yet supported".to_string()))
                    }
                    Msg::Ping(_) | Msg::Pong(_) | Msg::PingPeer => {
                        unreachable!("ping/pong must remain in peerd, and unreachable in swapd")
                    }
                    request => error!("request not supported {}", request),
                }
            }
            _ => {
                error!("MSG RPC can be only used for forwarding farcaster protocol messages");
                return Err(Error::NotSupported(ServiceBus::Msg, request.get_type()));
            }
        }
        Ok(())
    }

    fn handle_rpc_ctl(
        &mut self,
        endpoints: &mut Endpoints,
        source: ServiceId,
        request: Request,
    ) -> Result<(), Error> {
        match (&request, &source) {

            (Request::Hello, _) => {
                info!(
                    "{} | Service {} daemon is now {}",
                    self.swap_id.bright_blue_italic(),
                    source.bright_green_bold(),
                    "connected"
                );
            }
            (_, ServiceId::Syncer(..)) if source == self.syncer_state.bitcoin_syncer || source == self.syncer_state.monero_syncer => {
            }
            (
                _,
                ServiceId::Farcasterd
                | ServiceId::Wallet
                | ServiceId::Checkpoint
            ) => {}
            (Request::GetInfo(_), ServiceId::Client(_)) => {}
            _ => return Err(Error::Farcaster(
                "Permission Error: only Farcasterd, Wallet, Client and Syncer can can control swapd"
                    .to_string(),
            )),
        };

        match request {
            Request::Terminate if source == ServiceId::Farcasterd => {
                info!(
                    "{} | {}",
                    self.swap_id.bright_blue_italic(),
                    format!("Terminating {}", self.identity()).bright_white_bold()
                );
                std::process::exit(0);
            }
            Request::SweepXmrAddress(SweepXmrAddress {
                view_key,
                spend_key,
                address,
                minimum_balance,
                ..
            }) if source == ServiceId::Wallet => {
                let from_height = None; // will be set when sending out the request
                let task = self.syncer_state.sweep_xmr(
                    view_key,
                    spend_key,
                    address,
                    from_height,
                    minimum_balance,
                );
                let acc_confs_needs =
                    self.temporal_safety.sweep_monero_thr - self.temporal_safety.xmr_finality_thr;
                let sweep_block = self.syncer_state.height(Coin::Monero) + acc_confs_needs as u64;
                info!(
                    "{} | Tx {} needs {}, and has {} {}",
                    self.swap_id.bright_blue_italic(),
                    TxLabel::AccLock.bright_white_bold(),
                    "10 confirmations".bright_green_bold(),
                    (10 - acc_confs_needs).bright_green_bold(),
                    "confirmations".bright_green_bold(),
                );
                info!(
                    "{} | {} reaches your address {} around block {}",
                    self.swap_id.bright_blue_italic(),
                    Coin::Monero.bright_white_bold(),
                    address.bright_yellow_bold(),
                    sweep_block.bright_blue_bold(),
                );
                warn!(
                    "Peerd might crash, just ignore it, counterparty closed\
                       connection but you don't need it anymore!"
                );
                let request = Request::SyncerTask(task);
                let dest = self.syncer_state.monero_syncer();
                let pending_request = PendingRequest {
                    request,
                    dest: dest.clone(),
                    bus_id: ServiceBus::Ctl,
                };
                if self
                    .pending_requests
                    .insert(dest, vec![pending_request])
                    .is_some()
                {
                    error!("pending request for syncer already there")
                }
            }
            Request::TakeSwap(InitSwap {
                peerd,
                report_to,
                local_params,
                swap_id,
                remote_commit: None,
                funding_address, // Some(_) for Bob, None for Alice
            }) if self.state.start() => {
                if ServiceId::Swap(swap_id) != self.identity {
                    error!(
                        "{}: {}",
                        "This swapd instance is not reponsible for swap_id", swap_id
                    );
                    return Ok(());
                };
                self.peer_service = peerd.clone();
                self.enquirer = report_to.clone();

                if let ServiceId::Peer(ref addr) = peerd {
                    self.maker_peer = Some(addr.clone());
                }
                let local_commit =
                    self.taker_commit(endpoints, local_params.clone())
                        .map_err(|err| {
                            self.report_failure_to(
                                endpoints,
                                &report_to,
                                microservices::rpc::Failure {
                                    code: 0, // TODO: Create error type system
                                    info: err.to_string(),
                                },
                            )
                        })?;
                let next_state = self.state.clone().sup_start_to_commit(
                    local_commit.clone(),
                    local_params,
                    funding_address,
                    None,
                );
                let public_offer = self
                    .state
                    .public_offer()
                    .map(|offer| offer.to_string())
                    .expect("state Start has puboffer");
                let take_swap = TakeCommit {
                    commit: local_commit,
                    public_offer,
                    swap_id,
                };
                self.send_peer(endpoints, Msg::TakerCommit(take_swap))?;
                self.state_update(endpoints, next_state)?;
            }
            Request::Protocol(Msg::Reveal(reveal))
                if self.state.commit() && self.state.remote_commit().is_some() =>
            {
                let reveal_proof = Msg::Reveal(reveal);
                let swap_id = reveal_proof.swap_id();
                self.send_peer(endpoints, reveal_proof)?;
                trace!("sent reveal_proof to peerd");
                let local_params = self
                    .state
                    .local_params()
                    .expect("commit state has local_params");
                let reveal_params: Reveal = (swap_id, local_params.clone()).into();
                self.send_peer(endpoints, Msg::Reveal(reveal_params))?;
                trace!("sent reveal_proof to peerd");
                let next_state = self.state.clone().sup_commit_to_reveal();
                self.state_update(endpoints, next_state)?;
            }

            Request::MakeSwap(InitSwap {
                peerd,
                report_to,
                local_params,
                swap_id,
                remote_commit: Some(remote_commit),
                funding_address, // Some(_) for Bob, None for Alice
            }) if self.state.start() => {
                self.peer_service = peerd.clone();
                if let ServiceId::Peer(ref addr) = peerd {
                    self.maker_peer = Some(addr.clone());
                }
                self.enquirer = report_to.clone();
                let local_commit = self
                    .maker_commit(endpoints, &peerd, swap_id, &local_params)
                    .map_err(|err| {
                        self.report_failure_to(
                            endpoints,
                            &report_to,
                            microservices::rpc::Failure {
                                code: 0, // TODO: Create error type system
                                info: err.to_string(),
                            },
                        )
                    })?;
                let next_state = self.state.clone().sup_start_to_commit(
                    local_commit.clone(),
                    local_params,
                    funding_address,
                    Some(remote_commit),
                );

                trace!("sending peer MakerCommit msg {}", &local_commit);
                self.send_peer(endpoints, Msg::MakerCommit(local_commit))?;
                self.state_update(endpoints, next_state)?;
            }
            Request::FundingUpdated
                if source == ServiceId::Wallet
                    && ((self.state.trade_role() == Some(TradeRole::Taker)
                        && self.state.reveal())
                        || (self.state.trade_role() == Some(TradeRole::Maker)
                            && self.state.commit()))
                    && self.pending_requests.contains_key(&source)
                    && self
                        .pending_requests
                        .get(&source)
                        .map(|reqs| reqs.len() == 2)
                        .unwrap() =>
            {
                trace!("funding updated received from wallet");
                let mut pending_requests = self
                    .pending_requests
                    .remove(&source)
                    .expect("checked above, should have pending Reveal{Proof} requests");
                let PendingRequest {
                    request: request_parameters,
                    dest: dest_parameters,
                    bus_id: bus_id_parameters,
                } = pending_requests.pop().expect("checked .len() == 2");
                let PendingRequest {
                    request: request_proof,
                    dest: dest_proof,
                    bus_id: bus_id_proof,
                } = pending_requests.pop().expect("checked .len() == 2");
                // continue RevealProof
                // continuing request by sending it to wallet
                if let (
                    Request::Protocol(Msg::Reveal(Reveal::Proof(_))),
                    ServiceId::Wallet,
                    ServiceBus::Msg,
                ) = (&request_proof, &dest_proof, &bus_id_proof)
                {
                    trace!(
                        "sending request {} to {} on bus {}",
                        &request_proof,
                        &dest_proof,
                        &bus_id_proof
                    );
                    endpoints.send_to(bus_id_proof, self.identity(), dest_proof, request_proof)?
                } else {
                    error!("Not the expected request: found {:?}", request);
                }

                // continue Reveal
                // continuing request by sending it to wallet
                if let (
                    Request::Protocol(Msg::Reveal(Reveal::AliceParameters(_))),
                    ServiceId::Wallet,
                    ServiceBus::Msg,
                ) = (&request_parameters, &dest_parameters, &bus_id_parameters)
                {
                    trace!(
                        "sending request {} to {} on bus {}",
                        &request_parameters,
                        &dest_parameters,
                        &bus_id_parameters
                    );
                    endpoints.send_to(
                        bus_id_parameters,
                        self.identity(),
                        dest_parameters,
                        request_parameters,
                    )?
                } else {
                    error!("Not the expected request: found {:?}", request);
                }
            }
            // Request::SyncerEvent(ref event) => match (&event, source) {
            // handle monero events here
            // }
            Request::SyncerEvent(ref event) if source == self.syncer_state.monero_syncer => {
                match &event {
                    Event::HeightChanged(HeightChanged { height, .. }) => {
                        self.syncer_state
                            .handle_height_change(*height, Coin::Monero);
                    }
                    Event::AddressTransaction(AddressTransaction {
                        id,
                        hash,
                        amount,
                        block,
                        tx,
                    }) if self.state.swap_role() == SwapRole::Alice
                        && self.syncer_state.tasks.watched_addrs.contains_key(id)
                        && !self.state.a_xmr_locked() =>
                    {
                        debug!(
                            "Event details: {} {:?} {} {:?} {:?}",
                            id, hash, amount, block, tx
                        );
                        self.state.a_sup_refundsig_xmrlocked();
                        let txlabel = TxLabel::AccLock;
                        if !self.syncer_state.is_watched_tx(&txlabel) {
                            if self.syncer_state.awaiting_funding {
                                endpoints.send_to(
                                    ServiceBus::Ctl,
                                    self.identity(),
                                    ServiceId::Farcasterd,
                                    Request::FundingCompleted(Coin::Monero),
                                )?;
                                self.syncer_state.awaiting_funding = false;
                            }
                            let task = self.syncer_state.watch_tx_xmr(hash.clone(), txlabel);
                            endpoints.send_to(
                                ServiceBus::Ctl,
                                self.identity(),
                                self.syncer_state.monero_syncer(),
                                Request::SyncerTask(task),
                            )?;
                        }
                        if self.syncer_state.tasks.watched_addrs.remove(id).is_some() {
                            let abort_task = Task::Abort(Abort {
                                task_target: TaskTarget::TaskId(*id),
                                respond: Boolean::False,
                            });
                            endpoints.send_to(
                                ServiceBus::Ctl,
                                self.identity(),
                                self.syncer_state.monero_syncer(),
                                Request::SyncerTask(abort_task),
                            )?;
                        }
                    }
                    Event::AddressTransaction(AddressTransaction {
                        id,
                        hash,
                        amount,
                        block: _,
                        tx: _,
                    }) if self.state.swap_role() == SwapRole::Bob
                        && self.syncer_state.tasks.watched_addrs.contains_key(id)
                        && self.syncer_state.is_watched_addr(&TxLabel::AccLock) =>
                    {
                        let amount = monero::Amount::from_pico(*amount);
                        if amount < self.syncer_state.monero_amount {
                            warn!(
                                "Not enough monero locked: expected {}, found {}",
                                self.syncer_state.monero_amount, amount
                            );
                            return Ok(());
                        }
                        if let Some(tx_label) = self.syncer_state.tasks.watched_addrs.remove(id) {
                            if !self.syncer_state.is_watched_tx(&tx_label) {
                                let watch_tx =
                                    self.syncer_state.watch_tx_xmr(hash.clone(), tx_label);
                                endpoints.send_to(
                                    ServiceBus::Ctl,
                                    self.identity(),
                                    self.syncer_state.monero_syncer(),
                                    Request::SyncerTask(watch_tx),
                                )?;
                            }

                            let abort_task = Task::Abort(Abort {
                                task_target: TaskTarget::TaskId(*id),
                                respond: Boolean::False,
                            });
                            endpoints.send_to(
                                ServiceBus::Ctl,
                                self.identity(),
                                self.syncer_state.monero_syncer(),
                                Request::SyncerTask(abort_task),
                            )?;
                        }
                    }
                    Event::TransactionConfirmations(TransactionConfirmations {
                        confirmations: Some(confirmations),
                        ..
                    }) if self.state.b_buy_sig()
                        | (self.state.a_refundsig() && self.state.a_xmr_locked())
                        && *confirmations >= self.temporal_safety.sweep_monero_thr
                        && self.pending_requests.contains_key(&source) =>
                    {
                        let PendingRequest {
                            request,
                            dest,
                            bus_id,
                        } = self
                            .pending_requests
                            .remove(&source)
                            .expect("Checked above")
                            .pop()
                            .unwrap();
                        if let (
                            Request::SyncerTask(Task::SweepAddress(mut task)),
                            ServiceBus::Ctl,
                        ) = (request.clone(), bus_id)
                        {
                            // safe cast
                            task.from_height =
                                Some(self.syncer_state.monero_height - *confirmations as u64);
                            let request = Request::SyncerTask(Task::SweepAddress(task));

                            info!(
                                "{} | Monero are spendable now (height {}), sweeping ephemeral wallet",
                                self.swap_id.bright_blue_italic(),
                                self.syncer_state.monero_height.bright_white_bold()
                            );
                            endpoints.send_to(bus_id, self.identity(), dest, request)?;
                        } else {
                            error!(
                                "Not the sweep task {} or not Ctl bus found {}",
                                request, bus_id
                            );
                        }
                    }
                    Event::TransactionConfirmations(TransactionConfirmations {
                        confirmations: Some(confirmations),
                        ..
                    }) if self.temporal_safety.final_tx(*confirmations, Coin::Monero)
                        && self.state.b_core_arb()
                        && !self.state.cancel_seen()
                        && self.pending_requests.contains_key(&source)
                        && self
                            .pending_requests
                            .get(&source)
                            .map(|reqs| reqs.len() == 1)
                            .unwrap() =>
                    {
                        // error!("not checking tx rcvd is accordant lock");
                        let PendingRequest {
                            request,
                            dest,
                            bus_id,
                        } = self
                            .pending_requests
                            .remove(&source)
                            .expect("Checked above")
                            .pop()
                            .unwrap();
                        if let (Request::Protocol(Msg::BuyProcedureSignature(_)), ServiceBus::Msg) =
                            (&request, &bus_id)
                        {
                            endpoints.send_to(bus_id, self.identity(), dest, request)?;
                            debug!("sent buyproceduresignature at state {}", &self.state);
                            let next_state = State::Bob(BobState::BuySigB { buy_tx_seen: false });
                            self.state_update(endpoints, next_state)?;
                        } else {
                            error!(
                                "Not buyproceduresignatures {} or not Msg bus found {}",
                                request, bus_id
                            );
                        }
                    }
                    Event::TransactionConfirmations(TransactionConfirmations {
                        id,
                        confirmations,
                        ..
                    }) if self.syncer_state.tasks.watched_txs.contains_key(id)
                        && !self
                            .temporal_safety
                            .final_tx(confirmations.unwrap_or(0), Coin::Monero) =>
                    {
                        self.syncer_state.handle_tx_confs(
                            id,
                            confirmations,
                            self.swap_id(),
                            self.temporal_safety.xmr_finality_thr,
                        );
                    }
                    Event::TransactionConfirmations(TransactionConfirmations {
                        id,
                        confirmations,
                        ..
                    }) => {
                        self.syncer_state.handle_tx_confs(
                            id,
                            confirmations,
                            self.swap_id(),
                            self.temporal_safety.xmr_finality_thr,
                        );
                    }

                    Event::TaskAborted(_) => {}
                    Event::SweepSuccess(SweepSuccess { id, .. })
                        if (self.state.b_buy_sig() || self.state.a_xmr_locked())
                            && self.syncer_state.tasks.sweeping_addr.is_some()
                            && &self.syncer_state.tasks.sweeping_addr.unwrap() == id =>
                    {
                        if self.syncer_state.awaiting_funding {
                            warn!(
                                "FundingCompleted never emitted, but not possible to sweep\
                                   monero without passing through funding completed:\
                                   emitting it now to clean up farcasterd"
                            );
                            self.syncer_state.awaiting_funding = false;
                            match self.state.swap_role() {
                                SwapRole::Alice => {
                                    endpoints.send_to(
                                        ServiceBus::Ctl,
                                        self.identity(),
                                        ServiceId::Farcasterd,
                                        Request::FundingCompleted(Coin::Monero),
                                    )?;
                                }
                                SwapRole::Bob => {
                                    endpoints.send_to(
                                        ServiceBus::Ctl,
                                        self.identity(),
                                        ServiceId::Farcasterd,
                                        Request::FundingCompleted(Coin::Bitcoin),
                                    )?;
                                }
                            }
                        }
                        let abort_all = Task::Abort(Abort {
                            task_target: TaskTarget::AllTasks,
                            respond: Boolean::False,
                        });
                        endpoints.send_to(
                            ServiceBus::Ctl,
                            self.identity(),
                            self.syncer_state.monero_syncer(),
                            Request::SyncerTask(abort_all.clone()),
                        )?;
                        endpoints.send_to(
                            ServiceBus::Ctl,
                            self.identity(),
                            self.syncer_state.bitcoin_syncer(),
                            Request::SyncerTask(abort_all),
                        )?;
                        let success = if self.state.b_buy_sig() {
                            self.state_update(
                                endpoints,
                                State::Bob(BobState::FinishB(Outcome::Buy)),
                            )?;
                            Some(Outcome::Buy)
                        } else if self.state.a_refund_seen() {
                            self.state_update(
                                endpoints,
                                State::Alice(AliceState::FinishA(Outcome::Refund)),
                            )?;
                            Some(Outcome::Refund)
                        } else {
                            error!("Unexpected sweeping state, not sending finalization commands to wallet and farcasterd");
                            None
                        };
                        if let Some(success) = success {
                            let swap_success_req = Request::SwapOutcome(success);
                            self.send_ctl(endpoints, ServiceId::Wallet, swap_success_req.clone())?;
                            self.send_ctl(endpoints, ServiceId::Farcasterd, swap_success_req)?;
                            // remove txs from outdated states
                            self.txs.remove(&TxLabel::Lock);
                            self.txs.remove(&TxLabel::Cancel);
                            self.txs.remove(&TxLabel::Refund);
                            self.txs.remove(&TxLabel::Punish);
                        }
                    }
                    event => {
                        error!("event not handled {}", event)
                    }
                }
            }
            Request::SyncerEvent(ref event) if source == self.syncer_state.bitcoin_syncer => {
                match &event {
                    Event::HeightChanged(HeightChanged { height, .. }) => {
                        self.syncer_state
                            .handle_height_change(*height, Coin::Bitcoin);
                    }
                    Event::AddressTransaction(AddressTransaction {
                        id,
                        hash: _,
                        amount: _,
                        block: _,
                        tx,
                    }) if self.syncer_state.tasks.watched_addrs.get(id).is_some() => {
                        let tx = bitcoin::Transaction::deserialize(tx)?;
                        info!(
                            "Received AddressTransaction, processing tx {}",
                            &tx.txid().addr()
                        );
                        let txlabel = self.syncer_state.tasks.watched_addrs.get(id).unwrap();
                        match txlabel {
                            TxLabel::Funding if self.syncer_state.awaiting_funding => {
                                log_tx_seen(self.swap_id, txlabel, &tx.txid());
                                if self.syncer_state.awaiting_funding {
                                    self.syncer_state.awaiting_funding = false;
                                    endpoints.send_to(
                                        ServiceBus::Ctl,
                                        self.identity(),
                                        ServiceId::Farcasterd,
                                        Request::FundingCompleted(Coin::Bitcoin),
                                    )?;
                                }
                                let req = Request::Tx(Tx::Funding(tx));
                                self.send_wallet(ServiceBus::Ctl, endpoints, req)?;
                            }

                            txlabel => {
                                error!(
                                    "address transaction event not supported for tx {} at state {}",
                                    txlabel, &self.state
                                )
                            }
                        }
                    }
                    Event::AddressTransaction(AddressTransaction { tx, .. }) => {
                        let tx = bitcoin::Transaction::deserialize(tx)?;
                        warn!(
                            "unknown address transaction with txid {}",
                            &tx.txid().addr()
                        )
                    }
                    Event::TransactionRetrieved(TransactionRetrieved { id, tx: Some(tx) })
                        if self.syncer_state.tasks.retrieving_txs.contains_key(id) =>
                    {
                        let (txlabel, _) =
                            self.syncer_state.tasks.retrieving_txs.remove(id).unwrap();
                        match txlabel {
                            TxLabel::Buy if self.state.b_buy_sig() => {
                                log_tx_seen(self.swap_id, &txlabel, &tx.txid());
                                self.state.b_sup_buysig_buy_tx_seen();
                                let req = Request::Tx(Tx::Buy(tx.clone()));
                                self.send_wallet(ServiceBus::Ctl, endpoints, req)?
                            }
                            TxLabel::Buy => {
                                warn!(
                                    "expected BobState(BuySigB), found {}. Any chance you reused the \
                                     destination/refund address in the cli command? For your own privacy, \
                                     do not reuse bitcoin addresses. Txid {}",
                                    self.state,
                                    tx.txid().addr(),
                                )
                            }
                            TxLabel::Refund
                                if self.state.a_refundsig()
                                    && self.state.a_xmr_locked()
                                // && !self.state.a_buy_published()
                                =>
                            {
                                log_tx_seen(self.swap_id, &txlabel, &tx.txid());
                                let req = Request::Tx(Tx::Refund(tx.clone()));
                                self.send_wallet(ServiceBus::Ctl, endpoints, req)?
                            }
                            txlabel => {
                                error!(
                                    "Transaction retrieved event not supported for tx {} at state {}",
                                    txlabel, &self.state
                                )
                            }
                        }
                    }

                    Event::TransactionRetrieved(TransactionRetrieved { id, tx: None })
                        if self.syncer_state.tasks.retrieving_txs.contains_key(id) =>
                    {
                        let (_tx_label, task) =
                            self.syncer_state.tasks.retrieving_txs.get(id).unwrap();
                        std::thread::sleep(core::time::Duration::from_millis(500));
                        endpoints.send_to(
                            ServiceBus::Ctl,
                            self.identity(),
                            self.syncer_state.bitcoin_syncer(),
                            Request::SyncerTask(task.clone()),
                        )?;
                    }
                    Event::TransactionConfirmations(TransactionConfirmations {
                        id,
                        confirmations: Some(confirmations),
                        ..
                    }) if self.temporal_safety.final_tx(*confirmations, Coin::Bitcoin)
                        && self.syncer_state.tasks.watched_txs.get(id).is_some() =>
                    {
                        self.syncer_state.handle_tx_confs(
                            id,
                            &Some(*confirmations),
                            self.swap_id(),
                            self.temporal_safety.btc_finality_thr,
                        );
                        let txlabel = self.syncer_state.tasks.watched_txs.get(id).unwrap();
                        // saving requests of interest for later replaying latest event
                        // TODO MAYBE: refactor this block into following TxLabel match as an outer block with inner matching again
                        match &txlabel {
                            TxLabel::Lock => {
                                self.syncer_state.lock_tx_confs = Some(request.clone());
                            }
                            TxLabel::Cancel => {
                                self.syncer_state.cancel_tx_confs = Some(request.clone());
                                self.state.sup_cancel_seen();
                            }

                            _ => {}
                        }
                        match txlabel {
                            TxLabel::Funding => {}
                            TxLabel::Lock
                                if self.state.a_refundsig()
                                    && !self.state.a_xmr_locked()
                                    && !self.state.a_buy_published()
                                    && self.state.local_params().is_some()
                                    && self.state.remote_params().is_some()
                                    && !self.syncer_state.acc_lock_watched() =>
                            {
                                // TODO: implement state management here?
                                if let (
                                    Some(Params::Alice(alice_params)),
                                    Some(Params::Bob(bob_params)),
                                ) = (&self.state.local_params(), &self.state.remote_params())
                                {
                                    let (spend, view) =
                                        aggregate_xmr_spend_view(alice_params, bob_params);
                                    let viewpair = monero::ViewPair { spend, view };
                                    let address = monero::Address::from_viewpair(
                                        self.syncer_state.network.into(),
                                        &viewpair,
                                    );
                                    let swap_id = self.swap_id();
                                    let amount = self.syncer_state.monero_amount
                                        + monero::Amount::from_xmr(0.02).unwrap();
                                    info!(
                                        "{} | Send {} to {}",
                                        swap_id.bright_blue_italic(),
                                        amount.bright_green_bold(),
                                        address.addr(),
                                    );
                                    let funding_request = Request::FundingInfo(
                                        FundingInfo::Monero(MoneroFundingInfo {
                                            swap_id,
                                            address,
                                            amount,
                                        }),
                                    );
                                    self.syncer_state.awaiting_funding = true;
                                    if let Some(enquirer) = self.enquirer.clone() {
                                        endpoints.send_to(
                                            ServiceBus::Ctl,
                                            self.identity(),
                                            enquirer,
                                            funding_request,
                                        )?
                                    }
                                    let txlabel = TxLabel::AccLock;
                                    if !self.syncer_state.is_watched_addr(&txlabel) {
                                        let watch_addr_task =
                                            self.syncer_state.watch_addr_xmr(spend, view, txlabel);
                                        endpoints.send_to(
                                            ServiceBus::Ctl,
                                            self.identity(),
                                            self.syncer_state.monero_syncer(),
                                            Request::SyncerTask(watch_addr_task),
                                        )?;
                                    }
                                } else {
                                    error!(
                                        "local_params or remote_params not set for Alice, state {}",
                                        self.state
                                    )
                                }
                            }
                            TxLabel::Lock
                                if self.temporal_safety.valid_cancel(*confirmations)
                                    && self.state.safe_cancel()
                                    && self.txs.contains_key(&TxLabel::Cancel) =>
                            {
                                let cancel_tx = self.txs.get(&TxLabel::Cancel).unwrap().clone();
                                self.broadcast(cancel_tx, TxLabel::Cancel, endpoints)?
                            }
                            TxLabel::Lock
                                if self.temporal_safety.safe_buy(*confirmations)
                                    && self.state.swap_role() == SwapRole::Alice
                                    && self.state.a_refundsig()
                                    && !self.state.a_buy_published()
                                    && !self.state.cancel_seen()
                                    && self.txs.contains_key(&TxLabel::Buy)
                                    && self.state.remote_params().is_some()
                                    && self.state.local_params().is_some() =>
                            {
                                let xmr_locked = self.state.a_xmr_locked();
                                if let Some(buy_tx) = self.txs.get(&TxLabel::Buy) {
                                    let buy_tx = buy_tx.clone();
                                    self.broadcast(buy_tx, TxLabel::Buy, endpoints)?;
                                    self.state = State::Alice(AliceState::RefundSigA {
                                        local_params: self.state.local_params().cloned().unwrap(),
                                        buy_published: true,
                                        xmr_locked,
                                        cancel_seen: false,
                                        refund_seen: false,
                                        remote_params: self.state.remote_params().unwrap(),
                                    });
                                } else {
                                    warn!(
                                        "Alice doesn't have the buy tx, probably didnt receive \
                                             the buy signature yet: {}",
                                        self.state
                                    );
                                }
                            }
                            TxLabel::Lock
                                if self
                                    .temporal_safety
                                    .stop_funding_before_cancel(*confirmations)
                                    && self.state.safe_cancel()
                                    && self.state.swap_role() == SwapRole::Alice
                                    && self.syncer_state.awaiting_funding =>
                            {
                                warn!(
                                    "{} | Alice, the swap may be cancelled soon. Do not fund anymore",
                                    self.swap_id.bright_blue_italic()
                                );
                                self.syncer_state.awaiting_funding = false;
                                endpoints.send_to(
                                    ServiceBus::Ctl,
                                    self.identity(),
                                    ServiceId::Farcasterd,
                                    Request::FundingCanceled(Coin::Monero),
                                )?
                            }

                            TxLabel::Cancel
                                if self.temporal_safety.valid_punish(*confirmations)
                                    && self.state.a_refundsig()
                                    && self.state.a_xmr_locked()
                                    && self.txs.contains_key(&TxLabel::Punish)
                                    && !self.state.a_refund_seen() =>
                            {
                                trace!("Alice publishes punish tx");

                                let punish_tx = self.txs.get(&TxLabel::Punish).unwrap().clone();
                                // syncer's watch punish tx task
                                if !self.syncer_state.is_watched_tx(&TxLabel::Punish) {
                                    let txid = punish_tx.clone().txid();
                                    let task =
                                        self.syncer_state.watch_tx_btc(txid, TxLabel::Punish);
                                    endpoints.send_to(
                                        ServiceBus::Ctl,
                                        self.identity(),
                                        self.syncer_state.bitcoin_syncer(),
                                        Request::SyncerTask(task),
                                    )?;
                                }

                                self.broadcast(punish_tx, TxLabel::Punish, endpoints)?;
                            }

                            TxLabel::Cancel
                                if self.temporal_safety.safe_refund(*confirmations)
                                    && (self.state.b_buy_sig() || self.state.b_core_arb())
                                    && self.txs.contains_key(&TxLabel::Refund) =>
                            {
                                trace!("here Bob publishes refund tx");
                                let refund_tx = self.txs.get(&TxLabel::Refund).unwrap().clone();
                                self.broadcast(refund_tx, TxLabel::Refund, endpoints)?;
                            }
                            TxLabel::Cancel
                                if (self.state.swap_role() == SwapRole::Alice
                                    && !self.state.a_xmr_locked()) =>
                            {
                                warn!(
                                    "{} | Alice, this swap was canceled. Do not fund anymore.",
                                    self.swap_id.bright_blue_italic()
                                );
                                if self.syncer_state.awaiting_funding {
                                    endpoints.send_to(
                                        ServiceBus::Ctl,
                                        self.identity(),
                                        ServiceId::Farcasterd,
                                        Request::FundingCanceled(Coin::Monero),
                                    )?;
                                    self.syncer_state.awaiting_funding = false;
                                }
                                self.state_update(
                                    endpoints,
                                    State::Alice(AliceState::FinishA(Outcome::Refund)),
                                )?;
                                let abort_all = Task::Abort(Abort {
                                    task_target: TaskTarget::AllTasks,
                                    respond: Boolean::False,
                                });
                                endpoints.send_to(
                                    ServiceBus::Ctl,
                                    self.identity(),
                                    self.syncer_state.monero_syncer(),
                                    Request::SyncerTask(abort_all.clone()),
                                )?;
                                endpoints.send_to(
                                    ServiceBus::Ctl,
                                    self.identity(),
                                    self.syncer_state.bitcoin_syncer(),
                                    Request::SyncerTask(abort_all),
                                )?;
                                let swap_success_req = Request::SwapOutcome(Outcome::Refund);
                                self.send_wallet(
                                    ServiceBus::Ctl,
                                    endpoints,
                                    swap_success_req.clone(),
                                )?;
                                self.send_ctl(endpoints, ServiceId::Farcasterd, swap_success_req)?;
                                // remove txs from outdated states
                                self.txs.remove(&TxLabel::Lock);
                                self.txs.remove(&TxLabel::Buy);
                                self.txs.remove(&TxLabel::Cancel);
                                self.txs.remove(&TxLabel::Punish);
                            }
                            TxLabel::Buy
                                if self.temporal_safety.final_tx(*confirmations, Coin::Bitcoin)
                                    && self.state.a_refundsig()
                                    && self.state.a_buy_published() =>
                            {
                                // FIXME: swap ends here for alice
                                // wallet + farcaster
                                self.state_update(
                                    endpoints,
                                    State::Alice(AliceState::FinishA(Outcome::Buy)),
                                )?;
                                let abort_all = Task::Abort(Abort {
                                    task_target: TaskTarget::AllTasks,
                                    respond: Boolean::False,
                                });
                                endpoints.send_to(
                                    ServiceBus::Ctl,
                                    self.identity(),
                                    self.syncer_state.monero_syncer(),
                                    Request::SyncerTask(abort_all.clone()),
                                )?;
                                endpoints.send_to(
                                    ServiceBus::Ctl,
                                    self.identity(),
                                    self.syncer_state.bitcoin_syncer(),
                                    Request::SyncerTask(abort_all),
                                )?;
                                let swap_success_req = Request::SwapOutcome(Outcome::Buy);
                                self.send_wallet(
                                    ServiceBus::Ctl,
                                    endpoints,
                                    swap_success_req.clone(),
                                )?;
                                self.send_ctl(endpoints, ServiceId::Farcasterd, swap_success_req)?;
                                self.txs.remove(&TxLabel::Cancel);
                                self.txs.remove(&TxLabel::Punish);
                            }
                            TxLabel::Buy
                                if self.state.swap_role() == SwapRole::Bob
                                    && self.syncer_state.tasks.txids.contains_key(txlabel) =>
                            {
                                debug!("request Buy tx task");
                                let (txlabel, txid) =
                                    self.syncer_state.tasks.txids.remove_entry(txlabel).unwrap();
                                let task = self.syncer_state.retrieve_tx_btc(txid, txlabel);
                                endpoints.send_to(
                                    ServiceBus::Ctl,
                                    self.identity(),
                                    self.syncer_state.bitcoin_syncer(),
                                    Request::SyncerTask(task),
                                )?;
                            }
                            TxLabel::Refund
                                if self.state.swap_role() == SwapRole::Alice
                                    && !self.state.a_refund_seen()
                                    && self.syncer_state.tasks.txids.contains_key(txlabel) =>
                            {
                                debug!("subscribe Refund address task");
                                self.state.a_sup_refundsig_refund_seen();
                                let (txlabel, txid) =
                                    self.syncer_state.tasks.txids.remove_entry(txlabel).unwrap();
                                let task = self.syncer_state.retrieve_tx_btc(txid, txlabel);
                                endpoints.send_to(
                                    ServiceBus::Ctl,
                                    self.identity(),
                                    self.syncer_state.bitcoin_syncer(),
                                    Request::SyncerTask(task),
                                )?;
                            }

                            TxLabel::Refund if self.state.swap_role() == SwapRole::Bob => {
                                let abort_all = Task::Abort(Abort {
                                    task_target: TaskTarget::AllTasks,
                                    respond: Boolean::False,
                                });
                                endpoints.send_to(
                                    ServiceBus::Ctl,
                                    self.identity(),
                                    self.syncer_state.monero_syncer(),
                                    Request::SyncerTask(abort_all.clone()),
                                )?;
                                endpoints.send_to(
                                    ServiceBus::Ctl,
                                    self.identity(),
                                    self.syncer_state.bitcoin_syncer(),
                                    Request::SyncerTask(abort_all),
                                )?;
                                self.state_update(
                                    endpoints,
                                    State::Bob(BobState::FinishB(Outcome::Refund)),
                                )?;
                                let swap_success_req = Request::SwapOutcome(Outcome::Refund);
                                self.send_ctl(
                                    endpoints,
                                    ServiceId::Wallet,
                                    swap_success_req.clone(),
                                )?;
                                self.send_ctl(endpoints, ServiceId::Farcasterd, swap_success_req)?;
                                // remove txs from outdated states
                                self.txs.remove(&TxLabel::Lock);
                                self.txs.remove(&TxLabel::Cancel);
                                self.txs.remove(&TxLabel::Refund);
                                self.txs.remove(&TxLabel::Buy);
                                self.txs.remove(&TxLabel::Punish);
                            }
                            TxLabel::Punish => {
                                let abort_all = Task::Abort(Abort {
                                    task_target: TaskTarget::AllTasks,
                                    respond: Boolean::False,
                                });
                                endpoints.send_to(
                                    ServiceBus::Ctl,
                                    self.identity(),
                                    self.syncer_state.monero_syncer(),
                                    Request::SyncerTask(abort_all.clone()),
                                )?;
                                endpoints.send_to(
                                    ServiceBus::Ctl,
                                    self.identity(),
                                    self.syncer_state.bitcoin_syncer(),
                                    Request::SyncerTask(abort_all),
                                )?;
                                match self.state.swap_role() {
                                    SwapRole::Alice => self.state_update(
                                        endpoints,
                                        State::Alice(AliceState::FinishA(Outcome::Punish)),
                                    )?,
                                    SwapRole::Bob => {
                                        warn!("{}", "You were punished!".err());
                                        self.state_update(
                                            endpoints,
                                            State::Bob(BobState::FinishB(Outcome::Punish)),
                                        )?
                                    }
                                }
                                let swap_success_req = Request::SwapOutcome(Outcome::Punish);
                                self.send_ctl(
                                    endpoints,
                                    ServiceId::Wallet,
                                    swap_success_req.clone(),
                                )?;
                                self.send_ctl(endpoints, ServiceId::Farcasterd, swap_success_req)?;
                                // remove txs from outdated states
                                self.txs.remove(&TxLabel::Lock);
                                self.txs.remove(&TxLabel::Cancel);
                                self.txs.remove(&TxLabel::Refund);
                                self.txs.remove(&TxLabel::Buy);
                                self.txs.remove(&TxLabel::Punish);
                            }
                            tx_label => trace!(
                                "{} | Tx {} with {} confirmations evokes no response in state {}",
                                self.swap_id.bright_blue_italic(),
                                tx_label.bright_white_bold(),
                                confirmations,
                                &self.state
                            ),
                        }
                    }
                    Event::TransactionConfirmations(TransactionConfirmations {
                        id,
                        confirmations,
                        ..
                    }) => {
                        self.syncer_state.handle_tx_confs(
                            id,
                            confirmations,
                            self.swap_id(),
                            self.temporal_safety.btc_finality_thr,
                        );
                    }
                    Event::TransactionBroadcasted(event) => {
                        debug!("{}", event)
                    }
                    Event::TaskAborted(event) => {
                        debug!("{}", event)
                    }
                    Event::SweepSuccess(event) => {
                        debug!("{}", event)
                    }
                    Event::TransactionRetrieved(event) => {
                        debug!("{}", event)
                    }
                    Event::FeeEstimation(event) => {
                        debug!("{}", event)
                    }
                }
            }
            Request::Protocol(Msg::CoreArbitratingSetup(core_arb_setup))
                if self.state.reveal()
                    && self.state.remote_params().is_some()
                    && self.state.local_params().is_some() =>
            {
                // checkpoint swap pre lock bob
                debug!(
                    "{} | checkpointing bob pre lock swapd state",
                    self.swap_id.bright_blue_italic()
                );
                checkpoint_state(
                    endpoints,
                    self.swap_id,
                    request::CheckpointState::CheckpointSwapd(CheckpointSwapd {
                        state: self.state.clone(),
                        last_msg: Msg::CoreArbitratingSetup(core_arb_setup.clone()),
                        enquirer: self.enquirer.clone(),
                        temporal_safety: self.temporal_safety.clone(),
                        txs: self.txs.clone(),
                        txids: self.syncer_state.tasks.txids.clone(),
                        pending_requests: self.pending_requests.clone(),
                    }),
                )?;
                let CoreArbitratingSetup {
                    swap_id: _,
                    lock,
                    cancel,
                    refund,
                    cancel_sig: _,
                } = core_arb_setup.clone();
                for (tx, tx_label) in [lock, cancel, refund].iter().zip([
                    TxLabel::Lock,
                    TxLabel::Cancel,
                    TxLabel::Refund,
                ]) {
                    if !self.syncer_state.is_watched_tx(&tx_label) {
                        let txid = tx.clone().extract_tx().txid();
                        let task = self.syncer_state.watch_tx_btc(txid, tx_label);
                        endpoints.send_to(
                            ServiceBus::Ctl,
                            self.identity(),
                            self.syncer_state.bitcoin_syncer(),
                            Request::SyncerTask(task),
                        )?;
                    }
                }
                trace!("sending peer CoreArbitratingSetup msg: {}", &core_arb_setup);
                self.send_peer(endpoints, Msg::CoreArbitratingSetup(core_arb_setup))?;
                let next_state = State::Bob(BobState::CorearbB {
                    local_params: self.state.local_params().cloned().unwrap(),
                    cancel_seen: false,
                    remote_params: self.state.remote_params().unwrap(),
                });
                self.state_update(endpoints, next_state)?;
            }

            // TODO: checkpoint here or in caller of this
            Request::Tx(Tx::Lock(btc_lock)) if self.state.b_core_arb() => {
                log_tx_received(self.swap_id, TxLabel::Lock);
                self.broadcast(btc_lock, TxLabel::Lock, endpoints)?;
                if let (Some(Params::Bob(bob_params)), Some(Params::Alice(alice_params))) =
                    (&self.state.local_params(), &self.state.remote_params())
                {
                    let (spend, view) = aggregate_xmr_spend_view(alice_params, bob_params);

                    let txlabel = TxLabel::AccLock;
                    if !self.syncer_state.is_watched_addr(&txlabel) {
                        let task = self.syncer_state.watch_addr_xmr(spend, view, txlabel);
                        endpoints.send_to(
                            ServiceBus::Ctl,
                            self.identity(),
                            self.syncer_state.monero_syncer(),
                            Request::SyncerTask(task),
                        )?
                    }
                } else {
                    error!(
                        "local_params or remote_params not set, state {}",
                        self.state
                    )
                }
            }
            Request::Tx(transaction) => {
                // update state
                match transaction.clone() {
                    Tx::Cancel(tx) => {
                        log_tx_received(self.swap_id, TxLabel::Cancel);
                        self.txs.insert(TxLabel::Cancel, tx);
                    }
                    Tx::Refund(tx) => {
                        log_tx_received(self.swap_id, TxLabel::Refund);
                        self.txs.insert(TxLabel::Refund, tx);
                    }
                    Tx::Punish(tx) => {
                        log_tx_received(self.swap_id, TxLabel::Punish);
                        self.txs.insert(TxLabel::Punish, tx);
                    }
                    Tx::Buy(tx) => {
                        log_tx_received(self.swap_id, TxLabel::Buy);
                        self.txs.insert(TxLabel::Buy, tx);
                    }
                    Tx::Funding(_) => unreachable!("not handled in swapd"),
                    Tx::Lock(_) => unreachable!("handled above"),
                }
                // replay last tx confirmation event received from syncer, recursing
                let source = self.syncer_state.bitcoin_syncer();
                match transaction {
                    Tx::Cancel(_) | Tx::Buy(_) => {
                        if let Some(lock_tx_confs_req) = self.syncer_state.lock_tx_confs.clone() {
                            self.handle_rpc_ctl(endpoints, source, lock_tx_confs_req)?;
                        }
                    }
                    Tx::Refund(_) | Tx::Punish(_) => {
                        if let Some(cancel_tx_confs_req) = self.syncer_state.cancel_tx_confs.clone()
                        {
                            self.handle_rpc_ctl(endpoints, source, cancel_tx_confs_req)?;
                        }
                    }
                    _ => {}
                }
            }

            Request::Protocol(Msg::RefundProcedureSignatures(refund_proc_sigs))
                if self.state.reveal()
                    && self.state.remote_params().is_some()
                    && self.state.local_params().is_some() =>
            {
                // checkpoint alice pre lock bob
                debug!(
                    "{} | checkpointing alice pre lock swapd state",
                    self.swap_id.bright_blue_italic()
                );
                checkpoint_state(
                    endpoints,
                    self.swap_id,
                    request::CheckpointState::CheckpointSwapd(CheckpointSwapd {
                        state: self.state.clone(),
                        last_msg: Msg::RefundProcedureSignatures(refund_proc_sigs.clone()),
                        enquirer: self.enquirer.clone(),
                        temporal_safety: self.temporal_safety.clone(),
                        txs: self.txs.clone(),
                        txids: self.syncer_state.tasks.txids.clone(),
                        pending_requests: self.pending_requests.clone(),
                    }),
                )?;

                self.send_peer(endpoints, Msg::RefundProcedureSignatures(refund_proc_sigs))?;
                trace!("sent peer RefundProcedureSignatures msg");
                let next_state = State::Alice(AliceState::RefundSigA {
                    local_params: self.state.local_params().cloned().unwrap(),
                    xmr_locked: false,
                    buy_published: false,
                    cancel_seen: false,
                    refund_seen: false,
                    remote_params: self.state.remote_params().unwrap(),
                });
                self.state_update(endpoints, next_state)?;
            }

            Request::Protocol(Msg::BuyProcedureSignature(ref buy_proc_sig))
                if self.state.b_core_arb()
                    && !self.syncer_state.tasks.txids.contains_key(&TxLabel::Buy) =>
            {
                // checkpoint bob pre buy
                debug!(
                    "{} | checkpointing bob pre buy swapd state",
                    self.swap_id.bright_blue_italic()
                );
                checkpoint_state(
                    endpoints,
                    self.swap_id,
                    request::CheckpointState::CheckpointSwapd(CheckpointSwapd {
                        state: self.state.clone(),
                        last_msg: Msg::BuyProcedureSignature(buy_proc_sig.clone()),
                        enquirer: self.enquirer.clone(),
                        temporal_safety: self.temporal_safety.clone(),
                        txs: self.txs.clone(),
                        txids: self.syncer_state.tasks.txids.clone(),
                        pending_requests: self.pending_requests.clone(),
                    }),
                )?;

                debug!("subscribing with syncer for receiving raw buy tx ");

                let buy_tx = buy_proc_sig.buy.clone().extract_tx();
                let txid = buy_tx.txid();
                // register Buy tx task
                let tx_label = TxLabel::Buy;
                if !self.syncer_state.is_watched_tx(&tx_label) {
                    let task = self.syncer_state.watch_tx_btc(txid, tx_label);
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        self.identity(),
                        self.syncer_state.bitcoin_syncer(),
                        Request::SyncerTask(task),
                    )?;
                }
                // set external eddress: needed to subscribe for buy tx (bob) or refund (alice)
                self.syncer_state.tasks.txids.insert(TxLabel::Buy, txid);

                let pending_request = PendingRequest {
                    request,
                    dest: self.peer_service.clone(),
                    bus_id: ServiceBus::Msg,
                };
                if self
                    .pending_requests
                    .insert(self.syncer_state.monero_syncer(), vec![pending_request])
                    .is_none()
                {
                    debug!("deferring BuyProcedureSignature msg");
                } else {
                    error!("removed a pending request by mistake")
                };
            }

            Request::GetInfo(_) => {
                fn bmap<T>(remote_peer: &Option<NodeAddr>, v: &T) -> BTreeMap<NodeAddr, T>
                where
                    T: Clone,
                {
                    remote_peer
                        .as_ref()
                        .map(|p| bmap! { p.clone() => v.clone() })
                        .unwrap_or_default()
                }

                let swap_id = if self.swap_id() == zero!() {
                    None
                } else {
                    Some(self.swap_id())
                };
                let info = request::SwapInfo {
                    swap_id,
                    // state: self.state, // FIXME serde missing
                    maker_peer: self.maker_peer.clone().map(|p| vec![p]).unwrap_or_default(),
                    uptime: SystemTime::now()
                        .duration_since(self.started)
                        .unwrap_or_else(|_| Duration::from_secs(0)),
                    since: self
                        .started
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_else(|_| Duration::from_secs(0))
                        .as_secs(),
                    // params: self.params, // FIXME
                    // serde::Serialize/Deserialize missing
                    local_keys: dumb!(),
                    remote_keys: bmap(&self.maker_peer, &dumb!()),
                };
                self.send_ctl(endpoints, source, Request::SwapInfo(info))?;
            }

            Request::PeerdReconnected => {
                for msg in self.pending_peer_request.clone().iter() {
                    self.send_peer(endpoints, msg.clone())?;
                }
                self.pending_peer_request.clear();
            }

            Request::CheckpointMultipartChunk(request::CheckpointMultipartChunk {
                checksum,
                msg_index,
                msgs_total,
                serialized_state_chunk,
                swap_id,
            }) => {
                debug!("received checkpoint multipart message");
                if self.pending_checkpoint_chunks.contains_key(&checksum) {
                    let chunks = self
                        .pending_checkpoint_chunks
                        .get_mut(&checksum)
                        .expect("checked with contains_key");
                    chunks.insert(CheckpointChunk {
                        msg_index,
                        serialized_state_chunk,
                    });
                } else {
                    let mut chunk = HashSet::new();
                    chunk.insert(CheckpointChunk {
                        msg_index,
                        serialized_state_chunk,
                    });
                    self.pending_checkpoint_chunks.insert(checksum, chunk);
                }
                let mut chunks = self
                    .pending_checkpoint_chunks
                    .get(&checksum)
                    .unwrap_or(&HashSet::new())
                    .clone();
                if chunks.len() >= msgs_total {
                    let mut chunk_tup_vec = chunks
                        .drain()
                        .map(|chunk| (chunk.msg_index, chunk.serialized_state_chunk))
                        .collect::<Vec<(usize, Vec<u8>)>>(); // map the hashset to a vec for sorting
                    chunk_tup_vec.sort_by(|(msg_number_a, _), (msg_number_b, _)| {
                        msg_number_a.cmp(&msg_number_b)
                    }); // sort in ascending order
                    let chunk_vec = chunk_tup_vec
                        .drain(..)
                        .map(|(_, chunk)| chunk)
                        .collect::<Vec<Vec<u8>>>(); // drop the extra integer index
                    let serialized_checkpoint =
                        chunk_vec.into_iter().flatten().collect::<Vec<u8>>(); // collect the chunked messages into a single serialized message
                    if ripemd160::Hash::hash(&serialized_checkpoint).into_inner() != checksum {
                        // this should never happen
                        error!("Unable to checkpoint the message, checksum did not match");
                        return Ok(());
                    }
                    // serialize request and recurse to handle the actual request
                    let request = Request::Checkpoint(request::Checkpoint {
                        swap_id,
                        state: CheckpointState::strict_decode(std::io::Cursor::new(
                            serialized_checkpoint,
                        ))
                        .map_err(|err| Error::Farcaster(err.to_string()))?,
                    });
                    self.handle_rpc_ctl(endpoints, source, request)?;
                }
            }

            Request::Checkpoint(request::Checkpoint { swap_id, state }) => match state {
                CheckpointState::CheckpointSwapd(CheckpointSwapd {
                    state,
                    last_msg,
                    enquirer,
                    temporal_safety,
                    txs,
                    txids,
                    pending_requests,
                }) => {
                    info!("{} | Restoring swap", swap_id);
                    self.state = state;
                    self.enquirer = enquirer;
                    self.temporal_safety = temporal_safety;
                    self.pending_requests = pending_requests;
                    self.txs = txs.clone();
                    trace!("Watch height bitcoin");
                    let watch_height_bitcoin = Task::WatchHeight(WatchHeight {
                        id: self.syncer_state.tasks.new_taskid(),
                        lifetime: self.syncer_state.task_lifetime(Coin::Bitcoin),
                    });
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        self.identity(),
                        self.syncer_state.bitcoin_syncer(),
                        Request::SyncerTask(watch_height_bitcoin),
                    )?;

                    trace!("Watch height monero");
                    let watch_height_monero = Task::WatchHeight(WatchHeight {
                        id: self.syncer_state.tasks.new_taskid(),
                        lifetime: self.syncer_state.task_lifetime(Coin::Monero),
                    });
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        self.identity(),
                        self.syncer_state.monero_syncer(),
                        Request::SyncerTask(watch_height_monero),
                    )?;

                    trace!("Watching transactions");
                    for (tx_label, tx) in txs.iter() {
                        let task = self.syncer_state.watch_tx_btc(tx.txid(), tx_label.clone());
                        endpoints.send_to(
                            ServiceBus::Ctl,
                            self.identity(),
                            self.syncer_state.bitcoin_syncer(),
                            Request::SyncerTask(task),
                        )?;
                    }
                    for (tx_label, txid) in txids.iter() {
                        let task = self
                            .syncer_state
                            .watch_tx_btc(txid.clone(), tx_label.clone());
                        endpoints.send_to(
                            ServiceBus::Ctl,
                            self.identity(),
                            self.syncer_state.bitcoin_syncer(),
                            Request::SyncerTask(task),
                        )?;
                    }
                    let msg = format!("Restored swap at state {}", self.state);
                    let _ = self.report_progress_message_to(endpoints, ServiceId::Farcasterd, msg);

                    self.handle_rpc_ctl(
                        endpoints,
                        ServiceId::Database,
                        Request::Protocol(last_msg),
                    )?;
                }
                s => {
                    error!("Checkpoint {} not supported in swapd", s);
                }
            },

            _ => {
                error!("Request is not supported by the CTL interface {}", request);
                return Err(Error::NotSupported(ServiceBus::Ctl, request.get_type()));
            }
        }
        Ok(())
    }
}

impl Runtime {
    pub fn taker_commit(
        &mut self,
        endpoints: &mut Endpoints,
        params: Params,
    ) -> Result<request::Commit, Error> {
        info!(
            "{} | {} to Maker remote peer",
            self.swap_id().bright_blue_italic(),
            "Proposing to take swap".bright_white_bold(),
        );

        let msg = format!(
            "Proposing to take swap {} to Maker remote peer",
            self.swap_id()
        );
        let enquirer = self.enquirer.clone();
        // Ignoring possible reporting errors here and after: do not want to
        // halt the swap just because the client disconnected
        let _ = self.report_progress_message_to(endpoints, &enquirer, msg);

        let engine = CommitmentEngine;
        let commitment = match params {
            Params::Bob(params) => request::Commit::BobParameters(
                CommitBobParameters::commit_to_bundle(self.swap_id(), &engine, params),
            ),
            Params::Alice(params) => request::Commit::AliceParameters(
                CommitAliceParameters::commit_to_bundle(self.swap_id(), &engine, params),
            ),
        };

        Ok(commitment)
    }

    pub fn maker_commit(
        &mut self,
        endpoints: &mut Endpoints,
        peerd: &ServiceId,
        swap_id: SwapId,
        params: &Params,
    ) -> Result<request::Commit, Error> {
        info!(
            "{} | {} as Maker from Taker through peerd {}",
            swap_id.bright_blue_italic(),
            "Accepting swap".bright_white_bold(),
            peerd.bright_blue_italic()
        );

        let msg = format!(
            "Accepting swap {} as Maker from Taker through peerd {}",
            swap_id, peerd
        );
        let enquirer = self.enquirer.clone();
        // Ignoring possible reporting errors here and after: do not want to
        // halt the swap just because the client disconnected
        let _ = self.report_progress_message_to(endpoints, &enquirer, msg);

        let engine = CommitmentEngine;
        let commitment = match params.clone() {
            Params::Bob(params) => request::Commit::BobParameters(
                CommitBobParameters::commit_to_bundle(self.swap_id(), &engine, params),
            ),
            Params::Alice(params) => request::Commit::AliceParameters(
                CommitAliceParameters::commit_to_bundle(self.swap_id(), &engine, params),
            ),
        };

        Ok(commitment)
    }
}

pub fn get_swap_id(source: &ServiceId) -> Result<SwapId, Error> {
    if let ServiceId::Swap(swap_id) = source {
        Ok(*swap_id)
    } else {
        Err(Error::Farcaster("Not swapd".to_string()))
    }
}

fn aggregate_xmr_spend_view(
    alice_params: &AliceParameters<BtcXmr>,
    bob_params: &BobParameters<BtcXmr>,
) -> (monero::PublicKey, monero::PrivateKey) {
    let alice_view = *alice_params
        .accordant_shared_keys
        .clone()
        .into_iter()
        .find(|vk| vk.tag() == &SharedKeyId::new(SHARED_VIEW_KEY_ID))
        .expect("accordant shared keys should always have a view key")
        .elem();
    let bob_view = *bob_params
        .accordant_shared_keys
        .clone()
        .into_iter()
        .find(|vk| vk.tag() == &SharedKeyId::new(SHARED_VIEW_KEY_ID))
        .expect("accordant shared keys should always have a view key")
        .elem();
    (alice_params.spend + bob_params.spend, alice_view + bob_view)
}

fn remote_params_candidate(reveal: &Reveal, remote_commit: Commit) -> Result<Params, Error> {
    // parameter processing irrespective of maker & taker role
    let core_wallet = CommitmentEngine;
    match reveal {
        Reveal::AliceParameters(reveal) => match &remote_commit {
            Commit::AliceParameters(commit) => {
                commit.verify_with_reveal(&core_wallet, reveal.clone())?;
                Ok(Params::Alice(reveal.clone().into()))
            }
            _ => {
                let err_msg = "expected Some(Commit::Alice(commit))";
                error!("{}", err_msg);
                Err(Error::Farcaster(err_msg.to_string()))
            }
        },
        Reveal::BobParameters(reveal) => match &remote_commit {
            Commit::BobParameters(commit) => {
                commit.verify_with_reveal(&core_wallet, reveal.clone())?;
                Ok(Params::Bob(reveal.clone().into()))
            }
            _ => {
                let err_msg = "expected Some(Commit::Bob(commit))";
                error!("{}", err_msg);
                Err(Error::Farcaster(err_msg.to_string()))
            }
        },
        Reveal::Proof(_) => Err(Error::Farcaster(s!(
            "this should have been caught by another pattern!"
        ))),
    }
}

pub fn checkpoint_state(
    endpoints: &mut Endpoints,
    swap_id: SwapId,
    state: request::CheckpointState,
) -> Result<(), Error> {
    if let request::CheckpointState::CheckpointSwapd(swapd_state) = state.clone() {
        debug!("transactions: {:?}", swapd_state.txs);
    }
    let mut serialized_state = vec![];
    let size = state.strict_encode(&mut serialized_state).unwrap();

    // if the size exceeds a boundary, send a multi-part message
    let max_chunk_size = internet2::transport::MAX_FRAME_SIZE - 1024;
    if size > max_chunk_size {
        let checksum: [u8; 20] = ripemd160::Hash::hash(&serialized_state).into_inner();
        debug!(
            "{} | need to chunk the checkpoint message",
            swap_id.bright_blue_italic()
        );
        let chunks: Vec<(usize, Vec<u8>)> = serialized_state
            .chunks_mut(max_chunk_size)
            .enumerate()
            .map(|(n, chunk)| (n, chunk.to_vec()))
            .collect();
        let chunks_total = chunks.len();
        for (n, chunk) in chunks {
            debug!(
                "{} | sending chunked checkpoint message {} of a total {}",
                swap_id.bright_blue_italic(),
                n + 1,
                chunks_total
            );
            endpoints.send_to(
                ServiceBus::Ctl,
                ServiceId::Swap(swap_id),
                ServiceId::Database,
                Request::CheckpointMultipartChunk(CheckpointMultipartChunk {
                    checksum,
                    msg_index: n,
                    msgs_total: chunks_total,
                    serialized_state_chunk: chunk,
                    swap_id,
                }),
            )?;
        }
    } else {
        endpoints.send_to(
            ServiceBus::Ctl,
            ServiceId::Swap(swap_id),
            ServiceId::Database,
            Request::Checkpoint(Checkpoint { swap_id, state }),
        )?;
    }
    Ok(())
}

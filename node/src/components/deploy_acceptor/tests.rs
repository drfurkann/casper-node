#![cfg(test)]

use std::{
    collections::{BTreeMap, VecDeque},
    fmt::{self, Debug, Display, Formatter},
    time::Duration,
};

use derive_more::From;
use futures::channel::oneshot;
use prometheus::Registry;
use reactor::ReactorEvent;
use serde::Serialize;
use tempfile::TempDir;
use thiserror::Error;
use tokio::time;

use casper_execution_engine::{
    core::engine_state::{BalanceResult, QueryResult, MAX_PAYMENT_AMOUNT},
    storage::trie::merkle_proof::TrieMerkleProof,
};
use casper_types::{
    account::{Account, ActionThresholds, Weight},
    CLValue, ProtocolVersion, StoredValue, URef, U512,
};

use super::*;
use crate::{
    components::storage::{self, Storage},
    effect::{
        announcements::{ControlAnnouncement, DeployAcceptorAnnouncement},
        requests::ContractRuntimeRequest,
        Responder,
    },
    reactor::{self, EventQueueHandle, QueueKind, Runner},
    testing::ConditionCheckReactor,
    types::{Block, Chainspec, Deploy, NodeId},
    utils::{Loadable, WithDir},
    NodeRng,
};

const VERIFY_ACCOUNTS: bool = true;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const TIMEOUT: Duration = Duration::from_secs(10);

/// Top-level event for the reactor.
#[derive(Debug, From, Serialize)]
#[allow(clippy::large_enum_variant)]
#[must_use]
enum Event {
    #[from]
    Storage(#[serde(skip_serializing)] storage::Event),
    #[from]
    DeployAcceptor(#[serde(skip_serializing)] super::Event),
    #[from]
    ControlAnnouncement(ControlAnnouncement),
    #[from]
    DeployAcceptorAnnouncement(#[serde(skip_serializing)] DeployAcceptorAnnouncement<NodeId>),
    #[from]
    ContractRuntime(#[serde(skip_serializing)] ContractRuntimeRequest),
}

impl ReactorEvent for Event {
    fn as_control(&self) -> Option<&ControlAnnouncement> {
        if let Self::ControlAnnouncement(ref ctrl_ann) = self {
            Some(ctrl_ann)
        } else {
            None
        }
    }
}

impl From<StorageRequest> for Event {
    fn from(request: StorageRequest) -> Self {
        Event::Storage(storage::Event::from(request))
    }
}

impl Display for Event {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Event::Storage(event) => write!(formatter, "storage: {}", event),
            Event::DeployAcceptor(event) => write!(formatter, "deploy acceptor: {}", event),
            Event::ControlAnnouncement(ctrl_ann) => write!(formatter, "control: {}", ctrl_ann),
            Event::DeployAcceptorAnnouncement(ann) => {
                write!(formatter, "deploy-acceptor announcement: {}", ann)
            }

            Event::ContractRuntime(event) => {
                write!(formatter, "contract-runtime event: {:?}", event)
            }
        }
    }
}

/// Error type returned by the test reactor.
#[derive(Debug, Error)]
enum Error {
    #[error("prometheus (metrics) error: {0}")]
    Metrics(#[from] prometheus::Error),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ContractScenario {
    Valid,
    MissingContractAtHash,
    MissingContractAtName,
    MissingEntryPoint,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ContractPackageScenario {
    Valid,
    MissingPackageAtHash,
    MissingPackageAtName,
    MissingContractVersion,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TestScenario {
    FromPeerInvalidDeploy,
    FromPeerValidDeploy,
    FromPeerRepeatedValidDeploy,
    FromClientInvalidDeploy,
    FromClientMissingAccount,
    FromClientInsufficientBalance,
    FromClientValidDeploy,
    FromClientRepeatedValidDeploy,
    AccountWithInsufficientWeight,
    AccountWithInvalidAssociatedKeys,
    AccountWithUnknownBalance,
    DeployWithCustomPaymentContract(ContractScenario),
    DeployWithCustomPaymentContractPackage(ContractPackageScenario),
    DeployWithSessionContract(ContractScenario),
    DeployWithSessionContractPackage(ContractPackageScenario),
    DeployWithNativeTransferInPayment,
    DeployWithEmptySessionModuleBytes,
    BalanceCheckForDeploySentByPeer,
}

impl TestScenario {
    fn source(&self, rng: &mut NodeRng) -> Source<NodeId> {
        match self {
            TestScenario::FromPeerInvalidDeploy
            | TestScenario::FromPeerValidDeploy
            | TestScenario::FromPeerRepeatedValidDeploy
            | TestScenario::BalanceCheckForDeploySentByPeer => Source::Peer(NodeId::random(rng)),
            TestScenario::FromClientInvalidDeploy
            | TestScenario::FromClientMissingAccount
            | TestScenario::FromClientInsufficientBalance
            | TestScenario::FromClientValidDeploy
            | TestScenario::FromClientRepeatedValidDeploy
            | TestScenario::AccountWithInsufficientWeight
            | TestScenario::AccountWithInvalidAssociatedKeys
            | TestScenario::AccountWithUnknownBalance
            | TestScenario::DeployWithCustomPaymentContract(_)
            | TestScenario::DeployWithCustomPaymentContractPackage(_)
            | TestScenario::DeployWithSessionContract(_)
            | TestScenario::DeployWithSessionContractPackage(_)
            | TestScenario::DeployWithEmptySessionModuleBytes
            | TestScenario::DeployWithNativeTransferInPayment => Source::Client,
        }
    }

    fn deploy(&self, rng: &mut NodeRng) -> Deploy {
        let mut deploy = Deploy::random_valid_native_transfer(rng);
        match self {
            TestScenario::FromPeerInvalidDeploy | TestScenario::FromClientInvalidDeploy => {
                deploy.invalidate();
                deploy
            }
            TestScenario::FromPeerValidDeploy
            | TestScenario::FromPeerRepeatedValidDeploy
            | TestScenario::FromClientMissingAccount
            | TestScenario::FromClientInsufficientBalance
            | TestScenario::FromClientValidDeploy
            | TestScenario::FromClientRepeatedValidDeploy
            | TestScenario::AccountWithInvalidAssociatedKeys
            | TestScenario::AccountWithInsufficientWeight
            | TestScenario::AccountWithUnknownBalance
            | TestScenario::BalanceCheckForDeploySentByPeer => deploy,
            TestScenario::DeployWithCustomPaymentContract(contract_scenario) => {
                match contract_scenario {
                    ContractScenario::Valid | ContractScenario::MissingContractAtName => {
                        deploy.random_with_valid_custom_payment_contract_by_name(rng)
                    }
                    ContractScenario::MissingEntryPoint => {
                        deploy.random_with_missing_entry_point_in_payment_contract(rng)
                    }
                    ContractScenario::MissingContractAtHash => {
                        deploy.random_with_missing_payment_contract_by_hash(rng)
                    }
                }
            }
            TestScenario::DeployWithCustomPaymentContractPackage(contract_package_scenario) => {
                match contract_package_scenario {
                    ContractPackageScenario::Valid
                    | ContractPackageScenario::MissingPackageAtName => {
                        deploy.random_with_valid_custom_payment_package_by_name(rng)
                    }
                    ContractPackageScenario::MissingPackageAtHash => {
                        deploy.random_with_missing_payment_package_by_hash(rng)
                    }
                    ContractPackageScenario::MissingContractVersion => {
                        deploy.random_with_nonexistent_contract_version_in_payment_package(rng)
                    }
                }
            }
            TestScenario::DeployWithSessionContract(contract_scenario) => match contract_scenario {
                ContractScenario::Valid | ContractScenario::MissingContractAtName => {
                    deploy.random_with_valid_session_contract_by_name(rng)
                }
                ContractScenario::MissingContractAtHash => {
                    deploy.random_with_missing_session_contract_by_hash(rng)
                }
                ContractScenario::MissingEntryPoint => {
                    deploy.random_with_missing_entry_point_in_session_contract(rng)
                }
            },
            TestScenario::DeployWithSessionContractPackage(contract_package_scenario) => {
                match contract_package_scenario {
                    ContractPackageScenario::Valid
                    | ContractPackageScenario::MissingPackageAtName => {
                        deploy.random_with_valid_session_package_by_name(rng)
                    }
                    ContractPackageScenario::MissingPackageAtHash => {
                        deploy.random_with_missing_session_package_by_hash(rng)
                    }
                    ContractPackageScenario::MissingContractVersion => {
                        deploy.random_with_nonexistent_contract_version_in_session_package(rng)
                    }
                }
            }
            TestScenario::DeployWithEmptySessionModuleBytes => {
                deploy.random_with_empty_session_module_bytes(rng)
            }
            TestScenario::DeployWithNativeTransferInPayment => {
                deploy.random_with_native_transfer_in_payment_logic(rng)
            }
        }
    }

    fn is_valid_deploy_case(&self) -> bool {
        match self {
            TestScenario::FromPeerRepeatedValidDeploy
            | TestScenario::FromPeerValidDeploy
            | TestScenario::FromClientRepeatedValidDeploy
            | TestScenario::FromClientValidDeploy => true,
            TestScenario::FromPeerInvalidDeploy
            | TestScenario::FromClientInsufficientBalance
            | TestScenario::FromClientMissingAccount
            | TestScenario::FromClientInvalidDeploy
            | TestScenario::AccountWithInsufficientWeight
            | TestScenario::AccountWithInvalidAssociatedKeys
            | TestScenario::AccountWithUnknownBalance
            | TestScenario::DeployWithEmptySessionModuleBytes
            | TestScenario::DeployWithNativeTransferInPayment
            | TestScenario::BalanceCheckForDeploySentByPeer => false,
            TestScenario::DeployWithCustomPaymentContract(contract_scenario)
            | TestScenario::DeployWithSessionContract(contract_scenario) => match contract_scenario
            {
                ContractScenario::Valid => true,
                ContractScenario::MissingContractAtName
                | ContractScenario::MissingContractAtHash
                | ContractScenario::MissingEntryPoint => false,
            },
            TestScenario::DeployWithCustomPaymentContractPackage(contract_package_scenario)
            | TestScenario::DeployWithSessionContractPackage(contract_package_scenario) => {
                match contract_package_scenario {
                    ContractPackageScenario::Valid => true,
                    ContractPackageScenario::MissingPackageAtName
                    | ContractPackageScenario::MissingPackageAtHash
                    | ContractPackageScenario::MissingContractVersion => false,
                }
            }
        }
    }

    fn is_repeated_deploy_case(&self) -> bool {
        matches!(
            self,
            TestScenario::FromClientRepeatedValidDeploy | TestScenario::FromPeerRepeatedValidDeploy
        )
    }
}

struct Reactor {
    storage: Storage,
    deploy_acceptor: DeployAcceptor,
    _storage_tempdir: TempDir,
    test_scenario: TestScenario,
}

impl reactor::Reactor for Reactor {
    type Event = Event;
    type Config = TestScenario;
    type Error = Error;

    fn new(
        config: Self::Config,
        registry: &Registry,
        _event_queue: EventQueueHandle<Self::Event>,
        _rng: &mut NodeRng,
    ) -> Result<(Self, Effects<Self::Event>), Self::Error> {
        let (storage_config, storage_tempdir) = storage::Config::default_for_tests();
        let storage_withdir = WithDir::new(storage_tempdir.path(), storage_config);
        let storage = Storage::new(
            &storage_withdir,
            None,
            ProtocolVersion::from_parts(1, 0, 0),
            false,
            "test",
        )
        .unwrap();

        let deploy_acceptor = DeployAcceptor::new(
            super::Config::new(VERIFY_ACCOUNTS),
            &Chainspec::from_resources("local"),
            registry,
        )
        .unwrap();

        let reactor = Reactor {
            storage,
            deploy_acceptor,
            _storage_tempdir: storage_tempdir,
            test_scenario: config,
        };

        let effects = Effects::new();

        Ok((reactor, effects))
    }

    fn dispatch_event(
        &mut self,
        effect_builder: EffectBuilder<Self::Event>,
        rng: &mut NodeRng,
        event: Event,
    ) -> Effects<Self::Event> {
        match event {
            Event::Storage(event) => reactor::wrap_effects(
                Event::Storage,
                self.storage.handle_event(effect_builder, rng, event),
            ),
            Event::DeployAcceptor(event) => reactor::wrap_effects(
                Event::DeployAcceptor,
                self.deploy_acceptor
                    .handle_event(effect_builder, rng, event),
            ),
            Event::ControlAnnouncement(ctrl_ann) => {
                panic!("unhandled control announcement: {}", ctrl_ann)
            }
            Event::DeployAcceptorAnnouncement(_) => {
                // We do not care about deploy acceptor announcements in the acceptor tests.
                Effects::new()
            }
            Event::ContractRuntime(event) => match event {
                ContractRuntimeRequest::Query {
                    query_request,
                    responder,
                } => {
                    let query_result = if self.test_scenario
                        == TestScenario::FromClientMissingAccount
                    {
                        QueryResult::ValueNotFound(String::new())
                    } else if let Key::Account(account_hash) = query_request.key() {
                        if query_request.path().is_empty() {
                            let account = if let TestScenario::AccountWithInvalidAssociatedKeys =
                                self.test_scenario
                            {
                                Account::create(
                                    AccountHash::default(),
                                    BTreeMap::new(),
                                    URef::default(),
                                )
                            } else if let TestScenario::AccountWithInsufficientWeight =
                                self.test_scenario
                            {
                                let preset =
                                    Account::create(account_hash, BTreeMap::new(), URef::default());
                                let invalid_action_threshold =
                                    ActionThresholds::new(Weight::new(100u8), Weight::new(100u8))
                                        .expect("should create action threshold");
                                Account::new(
                                    preset.account_hash(),
                                    preset.named_keys().clone(),
                                    preset.main_purse(),
                                    preset.associated_keys().clone(),
                                    invalid_action_threshold,
                                )
                            } else {
                                Account::create(account_hash, BTreeMap::new(), URef::default())
                            };

                            QueryResult::Success {
                                value: Box::new(StoredValue::Account(account)),
                                proofs: vec![],
                            }
                        } else {
                            match self.test_scenario {
                                TestScenario::DeployWithCustomPaymentContractPackage(
                                    contract_package_scenario,
                                )
                                | TestScenario::DeployWithSessionContractPackage(
                                    contract_package_scenario,
                                ) => match contract_package_scenario {
                                    ContractPackageScenario::Valid
                                    | ContractPackageScenario::MissingContractVersion => {
                                        QueryResult::Success {
                                            value: Box::new(StoredValue::ContractPackage(
                                                ContractPackage::default(),
                                            )),
                                            proofs: vec![],
                                        }
                                    }
                                    _ => QueryResult::ValueNotFound(String::new()),
                                },
                                TestScenario::DeployWithSessionContract(contract_scenario)
                                | TestScenario::DeployWithCustomPaymentContract(
                                    contract_scenario,
                                ) => match contract_scenario {
                                    ContractScenario::Valid
                                    | ContractScenario::MissingEntryPoint => QueryResult::Success {
                                        value: Box::new(StoredValue::Contract(Contract::default())),
                                        proofs: vec![],
                                    },
                                    _ => QueryResult::ValueNotFound(String::new()),
                                },
                                _ => QueryResult::ValueNotFound(String::new()),
                            }
                        }
                    } else if let Key::Hash(_) = query_request.key() {
                        match self.test_scenario {
                            TestScenario::DeployWithSessionContract(contract_scenario)
                            | TestScenario::DeployWithCustomPaymentContract(contract_scenario) => {
                                match contract_scenario {
                                    ContractScenario::Valid
                                    | ContractScenario::MissingEntryPoint => QueryResult::Success {
                                        value: Box::new(StoredValue::Contract(Contract::default())),
                                        proofs: vec![],
                                    },
                                    ContractScenario::MissingContractAtHash
                                    | ContractScenario::MissingContractAtName => {
                                        QueryResult::ValueNotFound(String::new())
                                    }
                                }
                            }
                            TestScenario::DeployWithSessionContractPackage(
                                contract_package_scenario,
                            )
                            | TestScenario::DeployWithCustomPaymentContractPackage(
                                contract_package_scenario,
                            ) => match contract_package_scenario {
                                ContractPackageScenario::Valid
                                | ContractPackageScenario::MissingContractVersion => {
                                    QueryResult::Success {
                                        value: Box::new(StoredValue::ContractPackage(
                                            ContractPackage::default(),
                                        )),
                                        proofs: vec![],
                                    }
                                }
                                ContractPackageScenario::MissingPackageAtHash
                                | ContractPackageScenario::MissingPackageAtName => {
                                    QueryResult::ValueNotFound(String::new())
                                }
                            },
                            _ => QueryResult::ValueNotFound(String::new()),
                        }
                    } else {
                        panic!("expect only queries using Key::Account or Key::Hash variant");
                    };
                    responder.respond(Ok(query_result)).ignore()
                }
                ContractRuntimeRequest::GetBalance {
                    balance_request,
                    responder,
                } => {
                    let proof = TrieMerkleProof::new(
                        balance_request.purse_uref().into(),
                        StoredValue::CLValue(CLValue::from_t(()).expect("should get CLValue")),
                        VecDeque::new(),
                    );
                    let motes = if self.test_scenario == TestScenario::FromClientInsufficientBalance
                    {
                        MAX_PAYMENT_AMOUNT - 1
                    } else {
                        MAX_PAYMENT_AMOUNT
                    };
                    let balance_result =
                        if self.test_scenario == TestScenario::AccountWithUnknownBalance {
                            BalanceResult::RootNotFound
                        } else {
                            BalanceResult::Success {
                                motes: U512::from(motes),
                                proof: Box::new(proof),
                            }
                        };
                    responder.respond(Ok(balance_result)).ignore()
                }
                _ => panic!("should not receive {:?}", event),
            },
        }
    }

    fn maybe_exit(&self) -> Option<crate::reactor::ReactorExit> {
        panic!()
    }
}

fn put_block_to_storage(
    block: Box<Block>,
    responder: Responder<bool>,
) -> impl FnOnce(EffectBuilder<Event>) -> Effects<Event> {
    |effect_builder: EffectBuilder<Event>| {
        effect_builder
            .into_inner()
            .schedule(
                StorageRequest::PutBlock { block, responder },
                QueueKind::Regular,
            )
            .ignore()
    }
}

fn put_deploy_to_storage(
    deploy: Box<Deploy>,
    responder: Responder<bool>,
) -> impl FnOnce(EffectBuilder<Event>) -> Effects<Event> {
    |effect_builder: EffectBuilder<Event>| {
        effect_builder
            .into_inner()
            .schedule(
                StorageRequest::PutDeploy { deploy, responder },
                QueueKind::Regular,
            )
            .ignore()
    }
}

fn schedule_accept_deploy(
    deploy: Box<Deploy>,
    source: Source<NodeId>,
    responder: Responder<Result<(), super::Error>>,
) -> impl FnOnce(EffectBuilder<Event>) -> Effects<Event> {
    |effect_builder: EffectBuilder<Event>| {
        effect_builder
            .into_inner()
            .schedule(
                super::Event::Accept {
                    deploy,
                    source,
                    maybe_responder: Some(responder),
                },
                QueueKind::Regular,
            )
            .ignore()
    }
}

fn inject_balance_check_for_peer(
    deploy: Box<Deploy>,
    source: Source<NodeId>,
    responder: Responder<Result<(), super::Error>>,
) -> impl FnOnce(EffectBuilder<Event>) -> Effects<Event> {
    |effect_builder: EffectBuilder<Event>| {
        let event_metadata = EventMetadata::new(deploy, source, Some(responder));
        effect_builder
            .into_inner()
            .schedule(
                super::Event::GetBalanceResult {
                    event_metadata,
                    prestate_hash: Default::default(),
                    maybe_balance_value: None,
                    account_hash: Default::default(),
                    verification_start_timestamp: Timestamp::now(),
                },
                QueueKind::Regular,
            )
            .ignore()
    }
}

async fn run_deploy_acceptor_without_timeout(
    test_scenario: TestScenario,
) -> Result<(), super::Error> {
    let mut rng = crate::new_rng();

    let mut runner: Runner<ConditionCheckReactor<Reactor>> =
        Runner::new(test_scenario, &mut rng).await.unwrap();

    let block = Box::new(Block::random(&mut rng));
    // Create a responder to assert that the block was successfully injected into storage.
    let (block_sender, block_receiver) = oneshot::channel();
    let block_responder = Responder::create(block_sender);

    runner
        .process_injected_effects(put_block_to_storage(block, block_responder))
        .await;

    // There's only one scheduled event, so we only need to try cranking until the first time it
    // returns `Some`.
    while runner.try_crank(&mut rng).await.is_none() {
        time::sleep(POLL_INTERVAL).await;
    }
    assert!(block_receiver.await.unwrap());

    // Create a responder to assert the validity of the deploy
    let (deploy_sender, deploy_receiver) = oneshot::channel();
    let deploy_responder = Responder::create(deploy_sender);

    // Create a deploy specific to the test scenario
    let deploy = test_scenario.deploy(&mut rng);
    // Mark the source as either a peer or a client depending on the scenario.
    let source = test_scenario.source(&mut rng);

    {
        // Inject the deploy artificially into storage to simulate a previously seen deploy.
        if test_scenario.is_repeated_deploy_case() {
            let injected_deploy = Box::new(deploy.clone());
            let (injected_sender, injected_receiver) = oneshot::channel();
            let injected_responder = Responder::create(injected_sender);
            runner
                .process_injected_effects(put_deploy_to_storage(
                    injected_deploy,
                    injected_responder,
                ))
                .await;
            while runner.try_crank(&mut rng).await.is_none() {
                time::sleep(POLL_INTERVAL).await;
            }
            // Check that the "previously seen" deploy is present in storage.
            assert!(injected_receiver.await.unwrap());
        }

        if test_scenario == TestScenario::BalanceCheckForDeploySentByPeer {
            let fatal_deploy = Box::new(deploy.clone());
            let (deploy_sender, _) = oneshot::channel();
            let deploy_responder = Responder::create(deploy_sender);
            runner
                .process_injected_effects(inject_balance_check_for_peer(
                    fatal_deploy,
                    source.clone(),
                    deploy_responder,
                ))
                .await;
            while runner.try_crank(&mut rng).await.is_none() {
                time::sleep(POLL_INTERVAL).await;
            }
        }
    }

    runner
        .process_injected_effects(schedule_accept_deploy(
            Box::new(deploy.clone()),
            source,
            deploy_responder,
        ))
        .await;

    // Tests where the deploy is already in storage will not trigger any deploy acceptor
    // announcement, so use the deploy acceptor `PutToStorage` event as the condition.
    let stopping_condition = move |event: &Event| -> bool {
        match test_scenario {
            // Check that invalid deploys sent by a client raise the `InvalidDeploy` announcement
            // with the appropriate source.
            TestScenario::FromClientInvalidDeploy
            | TestScenario::FromClientMissingAccount
            | TestScenario::FromClientInsufficientBalance
            | TestScenario::DeployWithEmptySessionModuleBytes
            | TestScenario::AccountWithInvalidAssociatedKeys
            | TestScenario::AccountWithInsufficientWeight
            | TestScenario::AccountWithUnknownBalance
            | TestScenario::DeployWithNativeTransferInPayment => {
                matches!(
                    event,
                    Event::DeployAcceptorAnnouncement(DeployAcceptorAnnouncement::InvalidDeploy {
                        source: Source::Client,
                        ..
                    })
                )
            }
            // Check that executable items with valid contracts are successfully stored.
            // Conversely, ensure that invalid contracts will raise the invalid deploy
            // announcement.
            TestScenario::DeployWithCustomPaymentContract(contract_scenario)
            | TestScenario::DeployWithSessionContract(contract_scenario) => match contract_scenario
            {
                ContractScenario::Valid => matches!(
                    event,
                    Event::DeployAcceptorAnnouncement(
                        DeployAcceptorAnnouncement::AcceptedNewDeploy { .. }
                    )
                ),
                ContractScenario::MissingContractAtHash
                | ContractScenario::MissingContractAtName
                | ContractScenario::MissingEntryPoint => matches!(
                    event,
                    Event::DeployAcceptorAnnouncement(
                        DeployAcceptorAnnouncement::InvalidDeploy { .. }
                    )
                ),
            },
            // Check that executable items with valid contract packages are successfully stored.
            // Conversely, ensure that invalid contract packages will raise the invalid deploy
            // announcement.
            TestScenario::DeployWithCustomPaymentContractPackage(contract_package_scenario)
            | TestScenario::DeployWithSessionContractPackage(contract_package_scenario) => {
                match contract_package_scenario {
                    ContractPackageScenario::Valid => matches!(
                        event,
                        Event::DeployAcceptorAnnouncement(
                            DeployAcceptorAnnouncement::AcceptedNewDeploy { .. }
                        )
                    ),
                    ContractPackageScenario::MissingContractVersion
                    | ContractPackageScenario::MissingPackageAtHash
                    | ContractPackageScenario::MissingPackageAtName => matches!(
                        event,
                        Event::DeployAcceptorAnnouncement(
                            DeployAcceptorAnnouncement::InvalidDeploy { .. }
                        )
                    ),
                }
            }
            // Check that invalid deploys sent by a peer raise the `InvalidDeploy` announcement
            // with the appropriate source.
            TestScenario::FromPeerInvalidDeploy | TestScenario::BalanceCheckForDeploySentByPeer => {
                matches!(
                    event,
                    Event::DeployAcceptorAnnouncement(DeployAcceptorAnnouncement::InvalidDeploy {
                        source: Source::Peer(_),
                        ..
                    })
                )
            }
            // Check that a, new and valid, deploy sent by a peer raises an `AcceptedNewDeploy`
            // announcement with the appropriate source.
            TestScenario::FromPeerValidDeploy => {
                matches!(
                    event,
                    Event::DeployAcceptorAnnouncement(
                        DeployAcceptorAnnouncement::AcceptedNewDeploy {
                            source: Source::Peer(_),
                            ..
                        }
                    )
                )
            }
            // Check that a, new and valid, deploy sent by a client raises an `AcceptedNewDeploy`
            // announcement with the appropriate source.
            TestScenario::FromClientValidDeploy => {
                matches!(
                    event,
                    Event::DeployAcceptorAnnouncement(
                        DeployAcceptorAnnouncement::AcceptedNewDeploy {
                            source: Source::Client,
                            ..
                        }
                    )
                )
            }
            // Check that repeated valid deploys raise the `PutToStorageResult` with the
            // `is_new` flag as false.
            TestScenario::FromClientRepeatedValidDeploy
            | TestScenario::FromPeerRepeatedValidDeploy => {
                matches!(
                    event,
                    Event::DeployAcceptor(super::Event::PutToStorageResult { is_new: false, .. })
                )
            }
        }
    };
    runner
        .reactor_mut()
        .set_condition_checker(Box::new(stopping_condition));

    loop {
        if runner.try_crank(&mut rng).await.is_some() {
            if runner.reactor().condition_result() {
                break;
            }
        } else {
            time::sleep(POLL_INTERVAL).await;
        }
    }

    {
        // Assert that the deploy is present in the case of a valid deploy.
        // Conversely, assert its absence in the invalid case.
        let is_in_storage = runner
            .reactor()
            .inner()
            .storage
            .get_deploy_by_hash(*deploy.id())
            .is_some();

        if test_scenario.is_valid_deploy_case() {
            assert!(is_in_storage)
        } else {
            assert!(!is_in_storage)
        }
    }

    deploy_receiver.await.unwrap()
}

async fn run_deploy_acceptor(test_scenario: TestScenario) -> Result<(), super::Error> {
    time::timeout(TIMEOUT, run_deploy_acceptor_without_timeout(test_scenario))
        .await
        .unwrap()
}

#[tokio::test]
async fn should_accept_valid_deploy_from_peer() {
    let result = run_deploy_acceptor(TestScenario::FromPeerValidDeploy).await;
    assert!(result.is_ok())
}

#[tokio::test]
async fn should_reject_invalid_deploy_from_peer() {
    let result = run_deploy_acceptor(TestScenario::FromPeerInvalidDeploy).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployConfiguration(_))
    ))
}

#[tokio::test]
async fn should_accept_valid_deploy_from_client() {
    let result = run_deploy_acceptor(TestScenario::FromClientValidDeploy).await;
    assert!(result.is_ok())
}

#[tokio::test]
async fn should_reject_invalid_deploy_from_client() {
    let result = run_deploy_acceptor(TestScenario::FromClientInvalidDeploy).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployConfiguration(_))
    ))
}

#[tokio::test]
async fn should_reject_valid_deploy_from_client_for_missing_account() {
    let result = run_deploy_acceptor(TestScenario::FromClientMissingAccount).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployParameters {
            failure: DeployParameterFailure::NonexistentAccount { .. },
            ..
        })
    ))
}

#[tokio::test]
async fn should_reject_valid_deploy_for_account_with_invalid_associated_keys() {
    let result = run_deploy_acceptor(TestScenario::AccountWithInvalidAssociatedKeys).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployParameters {
            failure: DeployParameterFailure::InvalidAssociatedKeys,
            ..
        })
    ))
}

#[tokio::test]
async fn should_reject_valid_deploy_for_account_with_insufficient_weight() {
    let result = run_deploy_acceptor(TestScenario::AccountWithInsufficientWeight).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployParameters {
            failure: DeployParameterFailure::InsufficientDeploySignatureWeight,
            ..
        })
    ))
}

#[tokio::test]
async fn should_reject_valid_deploy_from_client_for_insufficient_balance() {
    let result = run_deploy_acceptor(TestScenario::FromClientInsufficientBalance).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployParameters {
            failure: DeployParameterFailure::InsufficientBalance { .. },
            ..
        })
    ))
}

#[tokio::test]
async fn should_reject_valid_deploy_from_client_for_unknown_balance() {
    let result = run_deploy_acceptor(TestScenario::AccountWithUnknownBalance).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployParameters {
            failure: DeployParameterFailure::UnknownBalance { .. },
            ..
        })
    ))
}

#[tokio::test]
async fn should_accept_repeated_valid_deploy_from_peer() {
    let result = run_deploy_acceptor(TestScenario::FromPeerRepeatedValidDeploy).await;
    assert!(result.is_ok())
}

#[tokio::test]
async fn should_accept_repeated_valid_deploy_from_client() {
    let result = run_deploy_acceptor(TestScenario::FromClientRepeatedValidDeploy).await;
    assert!(result.is_ok())
}

#[tokio::test]
async fn should_accept_deploy_with_valid_custom_payment() {
    let test_scenario = TestScenario::DeployWithCustomPaymentContract(ContractScenario::Valid);
    let result = run_deploy_acceptor(test_scenario).await;
    assert!(result.is_ok())
}

#[tokio::test]
async fn should_reject_deploy_with_missing_custom_payment_contract_by_name() {
    let test_scenario =
        TestScenario::DeployWithCustomPaymentContract(ContractScenario::MissingContractAtName);
    let result = run_deploy_acceptor(test_scenario).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployParameters {
            failure: DeployParameterFailure::NonexistentContractAtName { .. },
            ..
        })
    ))
}

#[tokio::test]
async fn should_reject_deploy_with_missing_custom_payment_contract_by_hash() {
    let test_scenario =
        TestScenario::DeployWithCustomPaymentContract(ContractScenario::MissingContractAtHash);
    let result = run_deploy_acceptor(test_scenario).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployParameters {
            failure: DeployParameterFailure::NonexistentContractAtHash { .. },
            ..
        })
    ))
}

#[tokio::test]
async fn should_reject_deploy_with_missing_entry_point_custom_payment() {
    let test_scenario =
        TestScenario::DeployWithCustomPaymentContract(ContractScenario::MissingEntryPoint);
    let result = run_deploy_acceptor(test_scenario).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployParameters {
            failure: DeployParameterFailure::NonexistentContractEntryPoint { .. },
            ..
        })
    ))
}

#[tokio::test]
async fn should_accept_deploy_with_valid_payment_contract_package_by_name() {
    let test_scenario =
        TestScenario::DeployWithCustomPaymentContractPackage(ContractPackageScenario::Valid);
    let result = run_deploy_acceptor(test_scenario).await;
    assert!(result.is_ok())
}

#[tokio::test]
async fn should_reject_deploy_with_missing_payment_contract_package_at_name() {
    let test_scenario = TestScenario::DeployWithCustomPaymentContractPackage(
        ContractPackageScenario::MissingPackageAtName,
    );
    let result = run_deploy_acceptor(test_scenario).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployParameters {
            failure: DeployParameterFailure::NonexistentContractPackageAtName { .. },
            ..
        })
    ))
}

#[tokio::test]
async fn should_reject_deploy_with_missing_payment_contract_package_at_hash() {
    let test_scenario = TestScenario::DeployWithCustomPaymentContractPackage(
        ContractPackageScenario::MissingPackageAtHash,
    );
    let result = run_deploy_acceptor(test_scenario).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployParameters {
            failure: DeployParameterFailure::NonexistentContractPackageAtHash { .. },
            ..
        })
    ))
}

#[tokio::test]
async fn should_reject_deploy_with_missing_version_in_payment_contract_package() {
    let test_scenario = TestScenario::DeployWithCustomPaymentContractPackage(
        ContractPackageScenario::MissingContractVersion,
    );
    let result = run_deploy_acceptor(test_scenario).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployParameters {
            failure: DeployParameterFailure::InvalidContractAtVersion { .. },
            ..
        })
    ))
}

#[tokio::test]
async fn should_accept_deploy_with_valid_session_contract() {
    let test_scenario = TestScenario::DeployWithSessionContract(ContractScenario::Valid);
    let result = run_deploy_acceptor(test_scenario).await;
    assert!(result.is_ok())
}

#[tokio::test]
async fn should_reject_deploy_with_missing_session_contract_by_hash() {
    let test_scenario =
        TestScenario::DeployWithSessionContract(ContractScenario::MissingContractAtHash);
    let result = run_deploy_acceptor(test_scenario).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployParameters {
            failure: DeployParameterFailure::NonexistentContractAtHash { .. },
            ..
        })
    ))
}

#[tokio::test]
async fn should_reject_deploy_with_missing_session_contract_by_name() {
    let test_scenario =
        TestScenario::DeployWithSessionContract(ContractScenario::MissingContractAtName);
    let result = run_deploy_acceptor(test_scenario).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployParameters {
            failure: DeployParameterFailure::NonexistentContractAtName { .. },
            ..
        })
    ))
}

#[tokio::test]
async fn should_reject_deploy_with_missing_entry_point_in_session_contract() {
    let test_scenario =
        TestScenario::DeployWithSessionContract(ContractScenario::MissingEntryPoint);
    let result = run_deploy_acceptor(test_scenario).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployParameters {
            failure: DeployParameterFailure::NonexistentContractEntryPoint { .. },
            ..
        })
    ))
}

#[tokio::test]
async fn should_accept_deploy_with_valid_session_contract_package() {
    let test_scenario =
        TestScenario::DeployWithSessionContractPackage(ContractPackageScenario::Valid);
    let result = run_deploy_acceptor(test_scenario).await;
    assert!(result.is_ok())
}

#[tokio::test]
async fn should_reject_deploy_with_missing_session_contract_package_at_name() {
    let test_scenario = TestScenario::DeployWithSessionContractPackage(
        ContractPackageScenario::MissingPackageAtName,
    );
    let result = run_deploy_acceptor(test_scenario).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployParameters {
            failure: DeployParameterFailure::NonexistentContractPackageAtName { .. },
            ..
        })
    ))
}

#[tokio::test]
async fn should_reject_deploy_with_missing_session_contract_package_at_hash() {
    let test_scenario = TestScenario::DeployWithSessionContractPackage(
        ContractPackageScenario::MissingPackageAtHash,
    );
    let result = run_deploy_acceptor(test_scenario).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployParameters {
            failure: DeployParameterFailure::NonexistentContractPackageAtHash { .. },
            ..
        })
    ))
}

#[tokio::test]
async fn should_reject_deploy_with_missing_version_in_session_contract_package() {
    let test_scenario = TestScenario::DeployWithCustomPaymentContractPackage(
        ContractPackageScenario::MissingContractVersion,
    );
    let result = run_deploy_acceptor(test_scenario).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployParameters {
            failure: DeployParameterFailure::InvalidContractAtVersion { .. },
            ..
        })
    ))
}

#[tokio::test]
async fn should_reject_deploy_with_empty_module_bytes_in_session() {
    let test_scenario = TestScenario::DeployWithEmptySessionModuleBytes;
    let result = run_deploy_acceptor(test_scenario).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployParameters {
            failure: DeployParameterFailure::MissingModuleBytes,
            ..
        })
    ))
}

#[tokio::test]
async fn should_reject_deploy_with_transfer_in_payment() {
    let test_scenario = TestScenario::DeployWithNativeTransferInPayment;
    let result = run_deploy_acceptor(test_scenario).await;
    assert!(matches!(
        result,
        Err(super::Error::InvalidDeployParameters {
            failure: DeployParameterFailure::InvalidPaymentVariant,
            ..
        })
    ))
}

#[tokio::test]
#[should_panic]
async fn should_panic_when_balance_checking_for_deploy_sent_by_peer() {
    let test_scenario = TestScenario::BalanceCheckForDeploySentByPeer;
    let result = run_deploy_acceptor(test_scenario).await;
    assert!(result.is_ok())
}
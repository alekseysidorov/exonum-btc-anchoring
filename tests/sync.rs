// Copyright 2019 The Exonum Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSEccccc//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use async_trait::async_trait;
use exonum::{
    blockchain::ApiSender,
    crypto::{Hash, KeyPair},
    helpers::Height,
    merkledb::ObjectHash,
    messages::{AnyTx, Verified},
};
use exonum_btc_anchoring::{
    api::{AnchoringChainLength, AnchoringProposalState, PrivateApi},
    blockchain::{AddFunds, BtcAnchoringInterface, SignInput},
    btc,
    config::Config,
    sync::{
        AnchoringChainUpdateTask, BitcoinRelay, ChainUpdateError, SyncWithBitcoinError,
        SyncWithBitcoinTask, TransactionStatus,
    },
    test_helpers::{get_anchoring_schema, AnchoringTestKit, ANCHORING_INSTANCE_ID},
};
use exonum_rust_runtime::api;
use exonum_testkit::TestKitApiClient;

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone)]
enum FakeRelayRequest {
    SendTransaction {
        request: btc::Transaction,
        response: btc::Sha256d,
    },
    TransactionStatus {
        request: btc::Sha256d,
        response: TransactionStatus,
    },
}

impl FakeRelayRequest {
    fn into_send_transaction(self) -> (btc::Transaction, btc::Sha256d) {
        if let FakeRelayRequest::SendTransaction { request, response } = self {
            (request, response)
        } else {
            panic!(
                "Expected response for the `send_transaction` request. But got {:?}",
                self
            )
        }
    }

    fn into_transaction_status(self) -> (btc::Sha256d, TransactionStatus) {
        if let FakeRelayRequest::TransactionStatus { request, response } = self {
            (request, response)
        } else {
            panic!(
                "Expected response for the `transaction_confirmations` request. But got {:?}",
                self
            )
        }
    }
}

#[derive(Debug, Clone, Default)]
struct FakeBitcoinRelay {
    requests: Arc<Mutex<VecDeque<FakeRelayRequest>>>,
}

impl FakeBitcoinRelay {
    fn enqueue_requests(&self, requests: impl IntoIterator<Item = FakeRelayRequest>) {
        self.requests.lock().unwrap().extend(requests)
    }

    fn dequeue_request(&self) -> FakeRelayRequest {
        self.requests
            .lock()
            .unwrap()
            .pop_front()
            .expect("Expected relay request")
    }
}

impl Drop for FakeBitcoinRelay {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            assert!(
                self.requests.lock().unwrap().is_empty(),
                "Unhandled requests remained. {:?}",
                self
            );
        }
    }
}

#[async_trait]
impl BitcoinRelay for FakeBitcoinRelay {
    type Error = anyhow::Error;

    async fn send_transaction(
        &self,
        transaction: &btc::Transaction,
    ) -> Result<btc::Sha256d, Self::Error> {
        let (expected_request, response) = self.dequeue_request().into_send_transaction();
        assert_eq!(&expected_request, transaction, "Unexpected data in request");
        Ok(response)
    }

    async fn transaction_status(&self, id: btc::Sha256d) -> Result<TransactionStatus, Self::Error> {
        let (expected_request, response) = self.dequeue_request().into_transaction_status();
        assert_eq!(expected_request, id, "Unexpected data in request");
        Ok(response)
    }
}

/// TODO Implement creating TestkitApi for an arbitrary TestNode. [ECR-3222]
#[derive(Debug)]
struct FakePrivateApi {
    service_keypair: KeyPair,
    client: TestKitApiClient,
    broadcaster: ApiSender,
}

impl FakePrivateApi {
    fn for_anchoring_node(
        testkit: &AnchoringTestKit,
        client: TestKitApiClient,
        bitcoin_key: &btc::PublicKey,
    ) -> Self {
        let service_keypair = testkit
            .find_anchoring_node(bitcoin_key)
            .unwrap()
            .service_keypair();

        Self {
            service_keypair,
            client,
            broadcaster: testkit.inner.blockchain().sender().clone(),
        }
    }

    async fn send<T>(&self, transaction: T)
    where
        T: Into<Verified<AnyTx>>,
    {
        self.broadcaster
            .broadcast_transaction(transaction.into())
            .await
            .expect("Cannot broadcast transaction");
    }
}

#[async_trait]
impl PrivateApi for FakePrivateApi {
    type Error = api::Error;

    async fn sign_input(&self, sign_input: SignInput) -> Result<Hash, Self::Error> {
        let signed_tx = self
            .service_keypair
            .sign_input(ANCHORING_INSTANCE_ID, sign_input);
        let hash = signed_tx.object_hash();
        self.send(signed_tx).await;
        Ok(hash)
    }

    async fn add_funds(&self, transaction: btc::Transaction) -> Result<Hash, Self::Error> {
        let signed_tx = self
            .service_keypair
            .add_funds(ANCHORING_INSTANCE_ID, AddFunds { transaction });
        let hash = signed_tx.object_hash();
        self.send(signed_tx).await;
        Ok(hash)
    }

    async fn anchoring_proposal(&self) -> Result<AnchoringProposalState, Self::Error> {
        self.client.anchoring_proposal().await
    }

    async fn config(&self) -> Result<Config, Self::Error> {
        self.client.config().await
    }

    async fn transaction_with_index(
        &self,
        index: u64,
    ) -> Result<Option<btc::Transaction>, Self::Error> {
        self.client.transaction_with_index(index).await
    }

    async fn transactions_count(&self) -> Result<AnchoringChainLength, Self::Error> {
        self.client.transactions_count().await
    }
}

fn anchoring_transaction_payload(testkit: &AnchoringTestKit, index: u64) -> Option<btc::Payload> {
    get_anchoring_schema(&testkit.inner.snapshot())
        .transactions_chain
        .get(index)
        .map(|tx| tx.anchoring_payload().unwrap())
}

#[tokio::test]
async fn chain_updater_normal() {
    let mut testkit = AnchoringTestKit::default();
    let api = testkit.inner.api();

    let anchoring_interval = testkit.actual_anchoring_config().anchoring_interval;
    // Commit several blocks.
    testkit
        .inner
        .create_blocks_until(Height(anchoring_interval));
    // Perform a several anchoring chain updates.
    for i in 0..2 {
        for keypair in testkit.anchoring_keypairs() {
            let private_api =
                FakePrivateApi::for_anchoring_node(&testkit, api.client().clone(), &keypair.0);

            AnchoringChainUpdateTask::new(vec![keypair], private_api)
                .process()
                .await
                .unwrap();
        }
        testkit.inner.create_block();
        // Make sure the anchoring proposal has been finalized.
        assert_eq!(
            anchoring_transaction_payload(&testkit, i)
                .unwrap()
                .block_height,
            Height(i * anchoring_interval)
        );
    }
}

#[tokio::test]
async fn chain_updater_no_initial_funds() {
    let anchoring_interval = 5;
    let mut testkit = AnchoringTestKit::new(1, anchoring_interval);
    // Commit several blocks.
    testkit
        .inner
        .create_blocks_until(Height(anchoring_interval));
    // Try to perform anchoring chain update.
    let api = testkit.inner.api();
    let e = AnchoringChainUpdateTask::new(testkit.anchoring_keypairs(), api.client().clone())
        .process()
        .await
        .unwrap_err();

    match e {
        ChainUpdateError::NoInitialFunds => {}
        e => panic!("Unexpected error occurred: {:?}", e),
    }
}

#[tokio::test]
async fn chain_updater_insufficient_funds() {
    let anchoring_interval = 5;
    let mut testkit = AnchoringTestKit::new(1, anchoring_interval);

    // Add an initial funding transaction to enable anchoring.
    testkit
        .inner
        .create_block_with_transactions(testkit.create_funding_confirmation_txs(200).0);

    // Commit several blocks.
    testkit
        .inner
        .create_blocks_until(Height(anchoring_interval));
    // Try to perform anchoring chain update.
    let api = testkit.inner.api();
    let e = AnchoringChainUpdateTask::new(testkit.anchoring_keypairs(), api.client().clone())
        .process()
        .await
        .unwrap_err();

    match e {
        ChainUpdateError::InsufficientFunds { balance, total_fee } => {
            assert_eq!(balance, 200);
            assert_eq!(total_fee, 1530);
        }
        e => panic!("Unexpected error occurred: {:?}", e),
    }
}

#[tokio::test]
async fn sync_with_bitcoin_normal() {
    let mut testkit = AnchoringTestKit::default();
    let anchoring_interval = testkit.actual_anchoring_config().anchoring_interval;
    // Create a several anchoring transactions
    for i in 0..2 {
        testkit
            .inner
            .create_blocks_until(Height(anchoring_interval * i));

        testkit
            .inner
            .create_block_with_transactions(testkit.create_signature_txs().into_iter().flatten());
    }

    // Check that sync with bitcoin works as expected.
    let snapshot = testkit.inner.snapshot();
    let anchoring_schema = get_anchoring_schema(&snapshot);
    let tx_chain = anchoring_schema.transactions_chain;

    let fake_relay = FakeBitcoinRelay::default();
    let api = testkit.inner.api();
    let sync = SyncWithBitcoinTask::new(fake_relay.clone(), api.client().clone());
    // Send first anchoring transaction.
    fake_relay.enqueue_requests(vec![
        // Relay should see that we have only a funding transaction confirmed.
        FakeRelayRequest::TransactionStatus {
            request: tx_chain.get(1).unwrap().id(),
            response: TransactionStatus::Unknown,
        },
        FakeRelayRequest::TransactionStatus {
            request: tx_chain.get(0).unwrap().id(),
            response: TransactionStatus::Unknown,
        },
        FakeRelayRequest::TransactionStatus {
            request: tx_chain.get(0).unwrap().prev_tx_id(),
            response: TransactionStatus::Committed(10),
        },
        // Ensure that relay sends first anchoring transaction to the Bitcoin network.
        FakeRelayRequest::SendTransaction {
            request: tx_chain.get(0).unwrap(),
            response: tx_chain.get(0).unwrap().id(),
        },
    ]);
    let latest_committed_tx_index = sync
        .process(None)
        .await
        .unwrap()
        .expect("Transaction should be committed");
    assert_eq!(latest_committed_tx_index, 0);
    // Send second anchoring transaction.
    fake_relay.enqueue_requests(vec![
        FakeRelayRequest::TransactionStatus {
            request: tx_chain.get(1).unwrap().id(),
            response: TransactionStatus::Unknown,
        },
        FakeRelayRequest::SendTransaction {
            request: tx_chain.get(1).unwrap(),
            response: tx_chain.get(1).unwrap().id(),
        },
    ]);
    let latest_committed_tx_index = sync
        .process(Some(1))
        .await
        .unwrap()
        .expect("Transaction should be committed");
    assert_eq!(latest_committed_tx_index, 1);
    // Check second anchoring transaction.
    fake_relay.enqueue_requests(vec![FakeRelayRequest::TransactionStatus {
        request: tx_chain.get(1).unwrap().id(),
        response: TransactionStatus::Mempool,
    }]);
    let latest_committed_tx_index = sync
        .process(Some(1))
        .await
        .unwrap()
        .expect("Transaction should be committed");
    assert_eq!(latest_committed_tx_index, 1);
}

#[tokio::test]
async fn sync_with_bitcoin_empty_chain() {
    let mut testkit = AnchoringTestKit::default();
    let api = testkit.inner.api();
    assert!(
        SyncWithBitcoinTask::new(FakeBitcoinRelay::default(), api.client().clone())
            .process(None)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn sync_with_bitcoin_err_unconfirmed_funding_tx() {
    let mut testkit = AnchoringTestKit::default();
    // Establish anchoring transactions chain.
    testkit
        .inner
        .create_block_with_transactions(testkit.create_signature_txs().into_iter().flatten());
    // Check that synchronization will cause an error if the funding transaction was not confirmed.
    let snapshot = testkit.inner.snapshot();
    let anchoring_schema = get_anchoring_schema(&snapshot);
    let tx_chain = anchoring_schema.transactions_chain;

    let fake_relay = FakeBitcoinRelay::default();
    let api = testkit.inner.api();
    let sync = SyncWithBitcoinTask::new(fake_relay.clone(), api.client().clone());
    fake_relay.enqueue_requests(vec![
        FakeRelayRequest::TransactionStatus {
            request: tx_chain.get(0).unwrap().id(),
            response: TransactionStatus::Unknown,
        },
        FakeRelayRequest::TransactionStatus {
            request: tx_chain.get(0).unwrap().prev_tx_id(),
            response: TransactionStatus::Unknown,
        },
    ]);

    let e = sync.process(None).await.unwrap_err();
    match e {
        SyncWithBitcoinError::UnconfirmedFundingTransaction(hash) => {
            assert_eq!(hash, tx_chain.get(0).unwrap().prev_tx_id())
        }
        e => panic!("Unexpected error occurred: {:?}", e),
    }
}

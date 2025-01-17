// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use super::{AptosDataClient, AptosNetDataClient, DataSummaryPoller, Error};
use crate::aptosnet::state::calculate_optimal_chunk_sizes;
use aptos_config::{
    config::{AptosDataClientConfig, StorageServiceConfig},
    network_id::{NetworkId, PeerNetworkId},
};
use aptos_crypto::HashValue;
use aptos_time_service::{MockTimeService, TimeService};
use aptos_types::{
    block_info::BlockInfo,
    ledger_info::{LedgerInfo, LedgerInfoWithSignatures},
    transaction::{TransactionListWithProof, Version},
    PeerId,
};
use channel::{aptos_channel, message_queues::QueueStyle};
use claim::{assert_err, assert_matches};
use futures::StreamExt;
use maplit::hashmap;
use network::{
    application::{interface::MultiNetworkSender, storage::PeerMetadataStorage},
    peer_manager::{ConnectionRequestSender, PeerManagerRequest, PeerManagerRequestSender},
    protocols::{network::NewNetworkSender, wire::handshake::v1::ProtocolId},
    transport::ConnectionMetadata,
};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use storage_service_client::{StorageServiceClient, StorageServiceNetworkSender};
use storage_service_server::network::{NetworkRequest, ResponseSender};
use storage_service_types::{
    CompleteDataRange, DataSummary, ProtocolMetadata, StorageServerSummary, StorageServiceError,
    StorageServiceMessage, StorageServiceRequest, StorageServiceResponse,
    TransactionsWithProofRequest,
};

fn mock_ledger_info(version: Version) -> LedgerInfoWithSignatures {
    LedgerInfoWithSignatures::new(
        LedgerInfo::new(
            BlockInfo::new(0, 0, HashValue::zero(), HashValue::zero(), version, 0, None),
            HashValue::zero(),
        ),
        BTreeMap::new(),
    )
}

fn mock_storage_summary(version: Version) -> StorageServerSummary {
    StorageServerSummary {
        protocol_metadata: ProtocolMetadata {
            max_epoch_chunk_size: 1000,
            max_transaction_chunk_size: 1000,
            max_transaction_output_chunk_size: 1000,
            max_account_states_chunk_size: 1000,
        },
        data_summary: DataSummary {
            synced_ledger_info: Some(mock_ledger_info(version)),
            epoch_ending_ledger_infos: None,
            transactions: Some(CompleteDataRange::new(0, version).unwrap()),
            transaction_outputs: None,
            account_states: None,
        },
    }
}

struct MockNetwork {
    peer_mgr_reqs_rx: aptos_channel::Receiver<(PeerId, ProtocolId), PeerManagerRequest>,
    peer_infos: Arc<PeerMetadataStorage>,
}

impl MockNetwork {
    fn new() -> (Self, MockTimeService, AptosNetDataClient, DataSummaryPoller) {
        let queue_cfg = aptos_channel::Config::new(10).queue_style(QueueStyle::FIFO);
        let (peer_mgr_reqs_tx, peer_mgr_reqs_rx) = queue_cfg.build();
        let (connection_reqs_tx, _connection_reqs_rx) = queue_cfg.build();

        let network_sender = MultiNetworkSender::new(hashmap! {
            NetworkId::Validator => StorageServiceNetworkSender::new(
                PeerManagerRequestSender::new(peer_mgr_reqs_tx),
                ConnectionRequestSender::new(connection_reqs_tx),
            )
        });

        let peer_infos = PeerMetadataStorage::new(&[NetworkId::Validator, NetworkId::Vfn]);
        let network_client = StorageServiceClient::new(network_sender, peer_infos.clone());

        let mock_time = TimeService::mock();
        let (client, poller) = AptosNetDataClient::new(
            AptosDataClientConfig::default(),
            StorageServiceConfig::default(),
            mock_time.clone(),
            network_client,
        );

        let mock_network = Self {
            peer_mgr_reqs_rx,
            peer_infos,
        };
        (mock_network, mock_time.into_mock(), client, poller)
    }

    /// Add a new priority peer to the network peer DB
    fn add_priority_peer(&mut self) -> PeerNetworkId {
        let network_id = NetworkId::Validator;
        self.add_peer(network_id)
    }

    /// Add a new regular peer to the network peer DB
    fn add_regular_peer(&mut self) -> PeerNetworkId {
        let network_id = NetworkId::Vfn;
        self.add_peer(network_id)
    }

    fn add_peer(&mut self, network_id: NetworkId) -> PeerNetworkId {
        let peer_id = PeerId::random();
        let mut connection_metadata = ConnectionMetadata::mock(peer_id);
        connection_metadata
            .application_protocols
            .insert(ProtocolId::StorageServiceRpc);

        self.peer_infos
            .insert_connection(network_id, connection_metadata);
        PeerNetworkId::new(network_id, peer_id)
    }

    /// Get the next request sent from the client.
    async fn next_request(&mut self) -> Option<NetworkRequest> {
        match self.peer_mgr_reqs_rx.next().await {
            Some(PeerManagerRequest::SendRpc(peer_id, network_request)) => {
                let protocol = network_request.protocol_id;
                let data = network_request.data;
                let res_tx = network_request.res_tx;

                let message: StorageServiceMessage = bcs::from_bytes(data.as_ref()).unwrap();
                let request = match message {
                    StorageServiceMessage::Request(request) => request,
                    _ => panic!("unexpected: {:?}", message),
                };
                let response_sender = ResponseSender::new(res_tx);

                Some((peer_id, protocol, request, response_sender))
            }
            Some(PeerManagerRequest::SendDirectSend(_, _)) => panic!("Unexpected direct send msg"),
            None => None,
        }
    }
}

#[tokio::test]
async fn request_works_only_when_data_available() {
    ::aptos_logger::Logger::init_for_testing();
    let (mut mock_network, mock_time, client, poller) = MockNetwork::new();

    tokio::spawn(poller.start_poller());

    // This request should fail because no peers are currently connected
    let error = client
        .get_transactions_with_proof(100, 50, 100, false)
        .await
        .unwrap_err();
    assert_matches!(error, Error::DataIsUnavailable(_));

    // Add a connected peer
    let expected_peer = mock_network.add_priority_peer();

    // Requesting some txns now will still fail since no peers are advertising
    // availability for the desired range.
    let error = client
        .get_transactions_with_proof(100, 50, 100, false)
        .await
        .unwrap_err();
    assert_matches!(error, Error::DataIsUnavailable(_));

    // Advance time so the poller sends a data summary request
    tokio::task::yield_now().await;
    mock_time.advance_async(Duration::from_millis(1_000)).await;

    // Receive their request and fulfill it
    let (peer, protocol, request, response_sender) = mock_network.next_request().await.unwrap();
    assert_eq!(peer, expected_peer.peer_id());
    assert_eq!(protocol, ProtocolId::StorageServiceRpc);
    assert_matches!(request, StorageServiceRequest::GetStorageServerSummary);

    let summary = mock_storage_summary(200);
    response_sender.send(Ok(StorageServiceResponse::StorageServerSummary(summary)));

    // Let the poller finish processing the response
    tokio::task::yield_now().await;

    // Handle the client's transactions request
    tokio::spawn(async move {
        let (peer, protocol, request, response_sender) = mock_network.next_request().await.unwrap();

        assert_eq!(peer, expected_peer.peer_id());
        assert_eq!(protocol, ProtocolId::StorageServiceRpc);
        assert_matches!(
            request,
            StorageServiceRequest::GetTransactionsWithProof(TransactionsWithProofRequest {
                start_version: 50,
                end_version: 100,
                proof_version: 100,
                include_events: false,
            })
        );

        response_sender.send(Ok(StorageServiceResponse::TransactionsWithProof(
            TransactionListWithProof::new_empty(),
        )));
    });

    // The client's request should succeed since a peer finally has advertised
    // data for this range.
    let response = client
        .get_transactions_with_proof(100, 50, 100, false)
        .await
        .unwrap();
    assert_eq!(response.payload, TransactionListWithProof::new_empty());
}

#[tokio::test]
async fn fetch_priority_peers_to_poll() {
    ::aptos_logger::Logger::init_for_testing();
    let (mut mock_network, _, client, _) = MockNetwork::new();

    // Request the next set of peers to poll and verify we have no peers
    assert_matches!(
        client.fetch_peers_to_poll(),
        Err(Error::DataIsUnavailable(_))
    );

    // Add priority peer 1
    let priority_peer_1 = mock_network.add_priority_peer();

    // Request the next set of peers and verify the set contains priority peer 1
    for _ in 0..2 {
        let peers_to_poll = client.fetch_peers_to_poll().unwrap();
        assert_eq!(peers_to_poll, vec![priority_peer_1]);
    }

    // Add priority peer 2
    let priority_peer_2 = mock_network.add_priority_peer();

    // Request the next set of peers and verify the set contains both peers
    let peers_to_poll = client.fetch_peers_to_poll().unwrap();
    assert_eq!(2, peers_to_poll.len());
    assert!(peers_to_poll.contains(&priority_peer_1));
    assert!(peers_to_poll.contains(&priority_peer_2));

    // Request the next set of peers and verify the set returns only one
    let peers_to_poll = client.fetch_peers_to_poll().unwrap();
    assert_eq!(1, peers_to_poll.len());

    // Request the next set of peers and verify the set returns the oldest
    let peers_to_poll = client.fetch_peers_to_poll().unwrap();
    assert_eq!(1, peers_to_poll.len());
    let polled_peer = peers_to_poll.first().unwrap();

    // Add priority peer 3
    let priority_peer_3 = mock_network.add_priority_peer();

    // Request the next set of peers and verify the set contains two priority peers
    let peers_to_poll = client.fetch_peers_to_poll().unwrap();
    assert_eq!(2, peers_to_poll.len());
    assert!(peers_to_poll.contains(&priority_peer_3));
    assert!(!peers_to_poll.contains(polled_peer));

    // Add priority peer 4 and 5
    let priority_peer_4 = mock_network.add_priority_peer();
    let priority_peer_5 = mock_network.add_priority_peer();

    // Request the next set of peers and verify the set contains three priority peers
    let peers_to_poll = client.fetch_peers_to_poll().unwrap();
    assert_eq!(3, peers_to_poll.len());
    assert!(peers_to_poll.contains(&priority_peer_4));
    assert!(peers_to_poll.contains(&priority_peer_5));
    assert!(peers_to_poll.contains(polled_peer));

    // Request the next set of peers and verify the oldest peer is chosen
    let peers_to_poll = client.fetch_peers_to_poll().unwrap();
    assert_eq!(1, peers_to_poll.len());
    assert!(!peers_to_poll.contains(&priority_peer_4));
    assert!(!peers_to_poll.contains(&priority_peer_5));
    assert!(!peers_to_poll.contains(polled_peer));
}

#[tokio::test]
async fn fetch_regular_peers_to_poll() {
    ::aptos_logger::Logger::init_for_testing();
    let (mut mock_network, _, client, _) = MockNetwork::new();

    // Request the next set of peers and verify we have no peers
    assert_matches!(
        client.fetch_peers_to_poll(),
        Err(Error::DataIsUnavailable(_))
    );

    // Add regular peer 1
    let regular_peer_1 = mock_network.add_regular_peer();

    // Request the next set of peers and verify the set contains regular peer 1
    for _ in 0..3 {
        let peers_to_poll = client.fetch_peers_to_poll().unwrap();
        assert_eq!(peers_to_poll, vec![regular_peer_1]);
    }

    // Add priority peer 1
    let priority_peer_1 = mock_network.add_priority_peer();

    // Request the next set of peers and verify the regular peer is polled only a few times
    let num_fetch_polls = 20;
    let mut regular_poll_count = 0;
    for _ in 0..10 {
        let peers_to_poll = client.fetch_peers_to_poll().unwrap();
        let num_peers_to_poll = peers_to_poll.len();
        assert!(num_peers_to_poll == 1 || num_peers_to_poll == 2);
        assert!(peers_to_poll.contains(&priority_peer_1));
        if peers_to_poll.len() == 2 {
            regular_poll_count += 1;
        }
    }
    assert!(regular_poll_count > 0 && regular_poll_count < num_fetch_polls);

    // Add regular peer 2
    let regular_peer_2 = mock_network.add_regular_peer();

    // Request the next set of peers and verify the set returns the priority and new peer
    let peers_to_poll = client.fetch_peers_to_poll().unwrap();
    assert!(peers_to_poll.contains(&regular_peer_2));
    assert!(peers_to_poll.contains(&priority_peer_1));

    // Add priority peer 2
    let priority_peer_2 = mock_network.add_priority_peer();

    // Request the next set of peers to poll and verify the set contains both priority peers
    let peers_to_poll = client.fetch_peers_to_poll().unwrap();
    assert!(peers_to_poll.contains(&priority_peer_1));
    assert!(peers_to_poll.contains(&priority_peer_2));

    // Request the next set of peers to poll and verify the set contains only one priority peer
    // and potentially a regular peer (depending on the sampling).
    let peers_to_poll = client.fetch_peers_to_poll().unwrap();
    let num_peers_to_poll = peers_to_poll.len();
    assert!(num_peers_to_poll == 1 || num_peers_to_poll == 2);
    if num_peers_to_poll == 1 {
        assert!(
            peers_to_poll.contains(&priority_peer_1) || peers_to_poll.contains(&priority_peer_2)
        );
    } else {
        assert!(peers_to_poll.contains(&regular_peer_1) || peers_to_poll.contains(&regular_peer_2));
    }
}

// 1. 2 peers
// 2. one advertises bad range, one advertises honest range
// 3. sending a bunch of requests to the bad range (which will always go to the
//    bad peer) should lower bad peer's score
// 4. eventually bad peer score should hit threshold and we err with no available
#[tokio::test]
async fn bad_peer_is_eventually_banned_internal() {
    ::aptos_logger::Logger::init_for_testing();
    let (mut mock_network, _, client, _) = MockNetwork::new();

    let good_peer = mock_network.add_priority_peer();
    let bad_peer = mock_network.add_priority_peer();

    // Bypass poller and just add the storage summaries directly.

    // Good peer advertises txns 0 -> 100.
    client.update_summary(good_peer, mock_storage_summary(100));
    // Bad peer advertises txns 0 -> 200 (but can't actually service).
    client.update_summary(bad_peer, mock_storage_summary(200));
    client.update_global_summary_cache();

    // The global summary should contain the bad peer's advertisement.
    let global_summary = client.get_global_data_summary();
    assert!(global_summary
        .advertised_data
        .transactions
        .contains(&CompleteDataRange::new(0, 200).unwrap()));

    // Spawn a handler for both peers.
    tokio::spawn(async move {
        while let Some((peer, _, _, response_sender)) = mock_network.next_request().await {
            if peer == good_peer.peer_id() {
                // Good peer responds with good response.
                response_sender.send(Ok(StorageServiceResponse::TransactionsWithProof(
                    TransactionListWithProof::new_empty(),
                )));
            } else if peer == bad_peer.peer_id() {
                // Bad peer responds with error.
                response_sender.send(Err(StorageServiceError::InternalError("".to_string())));
            }
        }
    });

    let mut seen_data_unavailable_err = false;

    // Sending a bunch of requests to the bad peer's upper range will fail.
    for _ in 0..20 {
        let result = client
            .get_transactions_with_proof(200, 200, 200, false)
            .await;

        // While the score is still decreasing, we should see a bunch of
        // InternalError's. Once we see a `DataIsUnavailable` error, we should
        // only see that error.
        if !seen_data_unavailable_err {
            assert_err!(&result);
            if let Err(Error::DataIsUnavailable(_)) = result {
                seen_data_unavailable_err = true;
            }
        } else {
            assert_matches!(result, Err(Error::DataIsUnavailable(_)));
        }
    }

    // Peer should eventually get ignored and we should consider this request
    // range unserviceable.
    assert!(seen_data_unavailable_err);

    // The global summary should no longer contain the bad peer's advertisement.
    client.update_global_summary_cache();
    let global_summary = client.get_global_data_summary();
    assert!(!global_summary
        .advertised_data
        .transactions
        .contains(&CompleteDataRange::new(0, 200).unwrap()));

    // We should still be able to send the good peer a request.
    let response = client
        .get_transactions_with_proof(100, 50, 100, false)
        .await
        .unwrap();
    assert_eq!(response.payload, TransactionListWithProof::new_empty());
}

#[tokio::test]
async fn bad_peer_is_eventually_banned_callback() {
    ::aptos_logger::Logger::init_for_testing();
    let (mut mock_network, _, client, _) = MockNetwork::new();

    let bad_peer = mock_network.add_priority_peer();

    // Bypass poller and just add the storage summaries directly.
    // Bad peer advertises txns 0 -> 200 (but can't actually service).
    client.update_summary(bad_peer, mock_storage_summary(200));
    client.update_global_summary_cache();

    // Spawn a handler for both peers.
    tokio::spawn(async move {
        while let Some((_, _, _, response_sender)) = mock_network.next_request().await {
            response_sender.send(Ok(StorageServiceResponse::TransactionsWithProof(
                TransactionListWithProof::new_empty(),
            )));
        }
    });

    let mut seen_data_unavailable_err = false;

    // Sending a bunch of requests to the bad peer (that we later decide are bad).
    for _ in 0..20 {
        let result = client
            .get_transactions_with_proof(200, 200, 200, false)
            .await;

        // While the score is still decreasing, we should see a bunch of
        // InternalError's. Once we see a `DataIsUnavailable` error, we should
        // only see that error.
        if !seen_data_unavailable_err {
            match result {
                Ok(response) => {
                    response
                        .context
                        .response_callback
                        .notify_bad_response(crate::ResponseError::ProofVerificationError);
                }
                Err(Error::DataIsUnavailable(_)) => {
                    seen_data_unavailable_err = true;
                }
                Err(_) => panic!("unexpected result: {:?}", result),
            }
        } else {
            assert_matches!(result, Err(Error::DataIsUnavailable(_)));
        }
    }

    // Peer should eventually get ignored and we should consider this request
    // range unserviceable.
    assert!(seen_data_unavailable_err);

    // The global summary should no longer contain the bad peer's advertisement.
    client.update_global_summary_cache();
    let global_summary = client.get_global_data_summary();
    assert!(!global_summary
        .advertised_data
        .transactions
        .contains(&CompleteDataRange::new(0, 200).unwrap()));
}

#[tokio::test]
async fn bad_peer_is_eventually_added_back() {
    ::aptos_logger::Logger::init_for_testing();
    let (mut mock_network, mock_time, client, poller) = MockNetwork::new();

    // Add a connected peer.
    mock_network.add_priority_peer();

    tokio::spawn(poller.start_poller());
    tokio::spawn(async move {
        while let Some((_, _, request, response_sender)) = mock_network.next_request().await {
            match request {
                StorageServiceRequest::GetTransactionsWithProof(_) => {
                    response_sender.send(Ok(StorageServiceResponse::TransactionsWithProof(
                        TransactionListWithProof::new_empty(),
                    )))
                }
                StorageServiceRequest::GetStorageServerSummary => response_sender.send(Ok(
                    StorageServiceResponse::StorageServerSummary(mock_storage_summary(200)),
                )),
                _ => panic!("unexpected: {:?}", request),
            }
        }
    });

    // Advance time so the poller sends a data summary request.
    tokio::task::yield_now().await;
    let summary_poll_interval = Duration::from_millis(1_000);
    mock_time.advance_async(summary_poll_interval).await;

    // Initially this request range is serviceable by this peer.
    let global_summary = client.get_global_data_summary();
    assert!(global_summary
        .advertised_data
        .transactions
        .contains(&CompleteDataRange::new(0, 200).unwrap()));

    // Keep decreasing this peer's score by considering its responses bad.
    // Eventually its score drops below IGNORE_PEER_THRESHOLD.
    for _ in 0..20 {
        let result = client.get_transactions_with_proof(200, 0, 200, false).await;

        if let Ok(response) = result {
            response
                .context
                .response_callback
                .notify_bad_response(crate::ResponseError::ProofVerificationError);
        }
    }

    // Peer is eventually ignored and this request range unserviceable.
    client.update_global_summary_cache();
    let global_summary = client.get_global_data_summary();
    assert!(!global_summary
        .advertised_data
        .transactions
        .contains(&CompleteDataRange::new(0, 200).unwrap()));

    // This peer still responds to the StorageServerSummary requests.
    // Its score keeps increasing and this peer is eventually added back.
    for _ in 0..20 {
        mock_time.advance_async(summary_poll_interval).await;
    }

    let global_summary = client.get_global_data_summary();
    assert!(global_summary
        .advertised_data
        .transactions
        .contains(&CompleteDataRange::new(0, 200).unwrap()));
}

#[tokio::test]
async fn optimal_chunk_size_calculations() {
    // Create a test storage service config
    let max_account_states_chunk_sizes = 500;
    let max_epoch_chunk_size = 600;
    let max_transaction_chunk_size = 700;
    let max_transaction_output_chunk_size = 800;
    let storage_service_config = StorageServiceConfig {
        max_account_states_chunk_sizes,
        max_concurrent_requests: 0,
        max_epoch_chunk_size,
        max_network_channel_size: 0,
        max_transaction_chunk_size,
        max_transaction_output_chunk_size,
        storage_summary_refresh_interval_ms: 0,
    };

    // Test median calculations
    let optimal_chunk_sizes = calculate_optimal_chunk_sizes(
        &storage_service_config,
        vec![100, 200, 300, 100],
        vec![7, 5, 6, 8, 10],
        vec![900, 700, 500],
        vec![40],
    );
    assert_eq!(200, optimal_chunk_sizes.account_states_chunk_size);
    assert_eq!(7, optimal_chunk_sizes.epoch_chunk_size);
    assert_eq!(700, optimal_chunk_sizes.transaction_chunk_size);
    assert_eq!(40, optimal_chunk_sizes.transaction_output_chunk_size);

    // Test no advertised data
    let optimal_chunk_sizes =
        calculate_optimal_chunk_sizes(&storage_service_config, vec![], vec![], vec![], vec![]);
    assert_eq!(
        max_account_states_chunk_sizes,
        optimal_chunk_sizes.account_states_chunk_size
    );
    assert_eq!(max_epoch_chunk_size, optimal_chunk_sizes.epoch_chunk_size);
    assert_eq!(
        max_transaction_chunk_size,
        optimal_chunk_sizes.transaction_chunk_size
    );
    assert_eq!(
        max_transaction_output_chunk_size,
        optimal_chunk_sizes.transaction_output_chunk_size
    );

    // Verify the config caps the amount of chunks
    let optimal_chunk_sizes = calculate_optimal_chunk_sizes(
        &storage_service_config,
        vec![1000, 1000, 2000, 3000],
        vec![70, 50, 60, 80, 100],
        vec![9000, 7000, 5000],
        vec![400],
    );
    assert_eq!(
        max_account_states_chunk_sizes,
        optimal_chunk_sizes.account_states_chunk_size
    );
    assert_eq!(70, optimal_chunk_sizes.epoch_chunk_size);
    assert_eq!(
        max_transaction_chunk_size,
        optimal_chunk_sizes.transaction_chunk_size
    );
    assert_eq!(400, optimal_chunk_sizes.transaction_output_chunk_size);
}

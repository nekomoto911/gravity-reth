#![allow(missing_docs)]

//! Gravity pipe execution, driven along the mainnet hardfork timeline.
//!
//! One node starts from mainnet genesis and walks genesis → Prague → Alpha → Beta →
//! Gamma in wall-clock time. Every block is executed by the pipe (grevm), committed,
//! and persisted before the next one is built. Epoch changes happen throughout the
//! timeline, never on a hardfork activation block.

mod hardfork;
mod node;
mod timeline;

use gravity_storage::block_view_storage::BlockViewStorage;
use node::{BlockInput, Builder, Node};
use reth_node_builder::EngineNodeLauncher;
use reth_node_ethereum::{node::EthereumAddOns, EthereumNode};
use reth_pipe_exec_layer_ext_v2::{new_pipe_exec_layer_api, ExecutionArgs};
use reth_provider::{providers::BlockchainProvider, BlockHashReader, HeaderProvider};
use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use timeline::{Fork, Phase, Timeline};

const DATADIR: &str = "data/gravity_pipe";

/// Sleep between two blocks; block timestamps are the wall-clock time at build.
const BLOCK_INTERVAL: Duration = Duration::from_secs(1);

const MIN_EPOCH_CHANGES: usize = 5;

#[test]
fn gravity_pipe() {
    let timeline = Timeline::starting_now();
    let genesis = timeline.genesis_json();
    node::run_node(&genesis, DATADIR, move |builder| run_timeline(builder, timeline));
}

async fn run_timeline(builder: Builder, timeline: Timeline) -> eyre::Result<()> {
    // Step 1: launch reth and attach the pipe execution layer at genesis.
    let handle = builder
        .with_types_and_provider::<EthereumNode, BlockchainProvider<_>>()
        .with_components(EthereumNode::components())
        .with_add_ons(EthereumAddOns::default())
        .launch_with_fn(|builder| {
            let launcher = EngineNodeLauncher::new(
                builder.task_executor().clone(),
                builder.config().datadir(),
                reth_engine_primitives::TreeConfig::default(),
            );
            builder.launch_with(launcher)
        })
        .await?;
    let chain_spec = handle.node.chain_spec();
    let eth_api = handle.node.rpc_registry.eth_api().clone();
    let provider = handle.node.provider;

    let genesis_header = provider.header_by_number(0)?.unwrap();
    let genesis_hash = provider.block_hash(0)?.unwrap();
    let (args_tx, args_rx) = tokio::sync::oneshot::channel();
    let pipe = new_pipe_exec_layer_api(
        chain_spec.clone(),
        BlockViewStorage::new(provider.clone()),
        genesis_header.clone(),
        genesis_hash,
        args_rx,
        eth_api.clone(),
    );
    args_tx.send(ExecutionArgs { block_number_to_block_id: BTreeMap::new() }).unwrap();
    let mut node = Node::new(pipe, &eth_api, genesis_header.timestamp).await;

    // Step 2: produce blocks until the last phase has changed epoch, or the schedule
    // runs out.
    let mut epoch_changes: Vec<(u64, Phase)> = Vec::new();
    loop {
        tokio::time::sleep(BLOCK_INTERVAL).await;
        let timestamp_us = SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros() as u64;
        let parent_timestamp = node.parent_timestamp();
        let phase = timeline.phase(timestamp_us / 1_000_000, parent_timestamp);

        let block = node
            .produce_block(BlockInput {
                timestamp_us,
                may_change_epoch: !matches!(phase, Phase::Activation(_)),
                ..Default::default()
            })
            .await;

        // The chain spec must agree with the timeline on which block activates a fork.
        for fork in Fork::ALL {
            assert_eq!(
                fork.transitions_at(&chain_spec, block.timestamp, parent_timestamp),
                phase == Phase::Activation(fork),
                "block {} ({phase}): chain spec disagrees on {fork:?} activation",
                block.number,
            );
        }
        if block.epoch_changed {
            epoch_changes.push((block.number, phase));
        }
        println!(
            "[gravity_pipe] block {} at {} ({phase}){}",
            block.number,
            block.timestamp,
            if block.epoch_changed { ", epoch changed" } else { "" }
        );

        if phase == Phase::After(Fork::Gamma) && block.epoch_changed ||
            block.timestamp >= timeline.deadline()
        {
            break;
        }
    }

    // Step 3: every phase saw an epoch change, and there were enough of them overall.
    let required_phases =
        std::iter::once(Phase::Genesis).chain(Fork::ALL.into_iter().map(Phase::After));
    for phase in required_phases {
        assert!(
            epoch_changes.iter().any(|(_, p)| *p == phase),
            "no epoch change in phase {phase}; {timeline:?}; epoch changes: {epoch_changes:?}"
        );
    }
    assert!(
        epoch_changes.len() >= MIN_EPOCH_CHANGES,
        "only {} epoch changes: {epoch_changes:?}",
        epoch_changes.len()
    );
    Ok(())
}

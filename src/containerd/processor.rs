// RGB node providing smart contracts functionality for Bitcoin & Lightning.
//
// Written in 2022 by
//     Dr. Maxim Orlovsky <orlovsky@lnp-bp.org>
//
// Copyright (C) 2022 by LNP/BP Standards Association, Switzerland.
//
// You should have received a copy of the MIT License along with this software.
// If not, see <https://opensource.org/licenses/MIT>.

use std::collections::{BTreeMap, BTreeSet};

use bitcoin::{OutPoint, Txid};
use commit_verify::lnpbp4;
use rgb::schema::TransitionType;
use rgb::{
    bundle, validation, Anchor, BundleId, ConsignmentType, ContractId, ContractState, Genesis,
    InmemConsignment, Node, NodeId, NodeOutpoint, Schema, SchemaId, SealEndpoint, Transition,
    TransitionBundle, Validator, Validity,
};
use rgb_rpc::OutpointSelection;

use super::Runtime;
use crate::{DaemonError, Db};

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Display, Error, From)]
#[display(doc_comments)]
pub enum StashError {
    /// contract is unknown. Probably you haven't imported the contract yet.
    GenesisAbsent,

    /// schema {0} is unknown.
    ///
    /// It may happen due to RGB Node bug, or indicate internal stash inconsistency and compromised
    /// stash data storage.
    SchemaAbsent(SchemaId),

    /// transition {0} is absent.
    ///
    /// It may happen due to RGB Node bug, or indicate internal stash inconsistency and compromised
    /// stash data storage.
    TransitionAbsent(NodeId),

    /// witness Txid is not known for transition {0}
    ///
    /// It may happen due to RGB Node bug, or indicate internal stash inconsistency and compromised
    /// stash data storage.
    TransitionTxidAbsent(NodeId),

    /// anchor for txid {0} is absent
    ///
    /// It may happen due to RGB Node bug, or indicate internal stash inconsistency and compromised
    /// stash data storage.
    AnchorAbsent(Txid),

    /// bundle data for txid {0} is absent
    ///
    /// It may happen due to RGB Node bug, or indicate internal stash inconsistency and compromised
    /// stash data storage.
    BundleAbsent(Txid),

    /// the anchor is not related to the contract
    ///
    /// It may happen due to RGB Node bug, or indicate internal stash inconsistency and compromised
    /// stash data storage.
    #[from(lnpbp4::LeafNotKnown)]
    UnrelatedAnchor,

    /// bundle reveal error
    ///
    /// It may happen due to RGB Node bug, or indicate internal stash inconsistency and compromised
    /// stash data storage.
    #[from]
    #[display(inner)]
    BundleReveal(bundle::RevealError),

    /// the resulting bundle size exceeds consensus restrictions
    ///
    /// It may happen due to RGB Node bug, or indicate internal stash inconsistency and compromised
    /// stash data storage.
    OutsizedBundle,
}

impl Runtime {
    pub(super) fn process_consignment<C: ConsignmentType>(
        &mut self,
        consignment: InmemConsignment<C>,
        force: bool,
    ) -> Result<validation::Status, DaemonError> {
        let contract_id = consignment.contract_id();
        let id = consignment.id();

        info!("Registering consignment {} for contract {}", id, contract_id);

        let mut state = self.db.retrieve(Db::CONTRACTS, contract_id)?.unwrap_or_else(|| {
            debug!("Contract {} was previously unknown", contract_id);
            ContractState::with(contract_id, &consignment.genesis)
        });
        trace!("Starting with contract state {:?}", state);

        debug!("Validating consignment {} for contract {}", id, contract_id);
        let status = Validator::validate(&consignment, &self.electrum);
        info!("Consignment validation result is {}", status.validity());

        match status.validity() {
            Validity::Valid => {}
            Validity::UnresolvedTransactions if force => {
                warn!("Forcing import of consignment with non-mined transactions");
            }
            _ => {
                error!("Invalid consignment: {:?}", status);
                return Ok(status);
            }
        }

        info!("Storing consignment {} into database", id);
        trace!("Schema: {:?}", consignment.schema);
        self.db.store(Db::SCHEMATA, consignment.schema.schema_id(), &consignment.schema)?;
        if let Some(root_schema) = &consignment.root_schema {
            trace!("Root schema: {:?}", root_schema);
            self.db.store(Db::SCHEMATA, root_schema.schema_id(), root_schema)?;
        }

        trace!("Genesis: {:?}", consignment.genesis);
        self.db.store_merge(Db::GENESIS, contract_id, consignment.genesis)?;

        for (anchor, bundle) in consignment.anchored_bundles {
            let bundle_id = bundle.bundle_id();
            let witness_txid = anchor.txid;
            debug!("Processing anchored bundle {} for txid {}", bundle_id, witness_txid);
            trace!("Anchor: {:?}", anchor);
            trace!("Bundle: {:?}", bundle);
            let anchor = anchor
                .into_merkle_block(contract_id, bundle_id.into())
                .expect("broken anchor data");
            debug!("Restored anchor id is {}", anchor.anchor_id());
            trace!("Restored anchor: {:?}", anchor);
            self.db.store_merge_h(Db::ANCHORS, anchor.txid, anchor)?;
            let mut data =
                bundle.concealed_iter().map(|(id, set)| (*id, set.clone())).collect::<Vec<_>>();
            for (transition, inputs) in bundle.into_revealed_iter() {
                let node_id = transition.node_id();
                let transition_type = transition.transition_type();
                debug!("Processing state transition {}", node_id);
                trace!("State transition: {:?}", transition);

                // TODO: For owned state, use only state which is a part of state tips
                state.add_transition(witness_txid, &transition);
                trace!("Contract state now is {:?}", state);

                trace!("Storing state transition data");
                data.push((node_id, inputs.clone()));
                self.db.store_merge(Db::TRANSITIONS, node_id, transition)?;
                self.db.store(Db::TRANSITION_TXID, node_id, &witness_txid)?;

                trace!("Indexing transition");
                let index_id = Db::index_two_pieces(contract_id, transition_type);
                self.db.insert_into_set(Db::CONTRACT_TRANSITIONS, index_id, node_id)?;
            }
            self.db.store_h(Db::BUNDLES, witness_txid, &data)?;
        }
        for extension in consignment.state_extensions {
            let node_id = extension.node_id();
            debug!("Processing state extension {}", node_id);
            trace!("State transition: {:?}", extension);

            // TODO: For owned state, use only state which is a part of state tips
            state.add_extension(&extension);
            trace!("Contract state now is {:?}", state);

            self.db.store_merge(Db::EXTENSIONS, node_id, extension)?;
        }

        debug!("Storing contract state for {}", contract_id);
        trace!("Final contract state is {:?}", state);
        self.db.store(Db::CONTRACTS, contract_id, &state)?;

        info!("Consignment processing complete for {}", id);
        Ok(status)
    }

    pub(super) fn compose_consignment<T: ConsignmentType>(
        &mut self,
        contract_id: ContractId,
        include: BTreeSet<TransitionType>,
        outpoint_selection: OutpointSelection,
        _phantom: T,
    ) -> Result<InmemConsignment<T>, DaemonError> {
        let genesis: Genesis =
            self.db.retrieve(Db::GENESIS, contract_id)?.ok_or(StashError::GenesisAbsent)?;
        let schema_id = genesis.schema_id();
        let schema: Schema = self
            .db
            .retrieve(Db::SCHEMATA, schema_id)?
            .ok_or(StashError::SchemaAbsent(schema_id))?;
        let root_schema_id = schema.root_id;
        let root_schema = if root_schema_id != zero!() {
            Some(
                self.db
                    .retrieve(Db::SCHEMATA, root_schema_id)?
                    .ok_or(StashError::SchemaAbsent(root_schema_id))?,
            )
        } else {
            None
        };

        let mut collector = Collector::new(contract_id);
        let mut tips: Vec<NodeOutpoint> = vec![];
        for transition_type in include {
            let node_ids = self.db.transitions_by_type(contract_id, transition_type)?;
            tips.extend(collector.process(&mut self.db, node_ids, &outpoint_selection)?);
        }

        collector = collector.iterate(&mut self.db)?;

        collector.consignment(schema, root_schema, genesis, tips)
    }
}

struct Collector {
    pub contract_id: ContractId,
    pub anchored_bundles: BTreeMap<Txid, (Anchor<lnpbp4::MerkleProof>, TransitionBundle)>,
    pub endpoints: Vec<(BundleId, SealEndpoint)>,
    pub endpoint_inputs: Vec<NodeId>,
}

impl Collector {
    pub fn new(contract_id: ContractId) -> Self {
        Collector {
            contract_id,
            anchored_bundles: empty![],
            endpoints: vec![],
            endpoint_inputs: vec![],
        }
    }

    // TODO: Support state extensions
    pub fn process(
        &mut self,
        db: &mut Db,
        node_ids: impl IntoIterator<Item = NodeId>,
        outpoint_selection: &OutpointSelection,
    ) -> Result<Vec<NodeOutpoint>, DaemonError> {
        let contract_id = self.contract_id;

        let mut tips: Vec<NodeOutpoint> = vec![];
        for transition_id in node_ids {
            let transition: Transition = db
                .retrieve(Db::TRANSITIONS, transition_id)?
                .ok_or(StashError::TransitionAbsent(transition_id))?;

            let witness_txid: Txid = db
                .retrieve(Db::TRANSITION_TXID, transition_id)?
                .ok_or(StashError::TransitionTxidAbsent(transition_id))?;

            let bundle = if let Some((_, bundle)) = self.anchored_bundles.get_mut(&witness_txid) {
                bundle
            } else {
                let anchor: Anchor<lnpbp4::MerkleBlock> = db
                    .retrieve_h(Db::ANCHORS, witness_txid)?
                    .ok_or(StashError::AnchorAbsent(witness_txid))?;
                let bundle: TransitionBundle = db
                    .retrieve_h(Db::BUNDLES, witness_txid)?
                    .ok_or(StashError::BundleAbsent(witness_txid))?;
                let anchor = anchor.to_merkle_proof(contract_id)?;
                self.anchored_bundles.insert(witness_txid, (anchor, bundle));
                &mut self.anchored_bundles.get_mut(&witness_txid).expect("stdlib broken").1
            };

            let bundle_id = bundle.bundle_id();
            for (output_no, (_, assignments)) in transition.owned_rights().iter().enumerate() {
                for seal in assignments.filter_revealed_seals() {
                    let txid = seal.txid.unwrap_or(witness_txid);
                    let outpoint = OutPoint::new(txid, seal.vout);
                    let seal_endpoint = SealEndpoint::from(seal);
                    if outpoint_selection.includes(outpoint) {
                        tips.push(NodeOutpoint::new(transition_id, output_no as u16));
                        self.endpoints.push((bundle_id, seal_endpoint));
                        self.endpoint_inputs
                            .extend(transition.parent_outputs().into_iter().map(|out| out.node_id));
                    }
                }
            }

            bundle.reveal_transition(transition)?;
        }

        Ok(tips)
    }

    pub fn iterate(mut self, db: &mut Db) -> Result<Self, DaemonError> {
        // Collect all transitions between endpoints and genesis independently from their type
        loop {
            let node_ids = self.endpoint_inputs;
            self.endpoint_inputs = vec![];
            self.process(db, node_ids, &OutpointSelection::All)?;
            if self.endpoint_inputs.is_empty() {
                break;
            }
        }
        Ok(self)
    }

    pub fn consignment<T: ConsignmentType>(
        self,
        schema: Schema,
        root_schema: Option<Schema>,
        genesis: Genesis,
        tips: Vec<NodeOutpoint>,
    ) -> Result<InmemConsignment<T>, DaemonError> {
        let anchored_bundles = self
            .anchored_bundles
            .into_values()
            .collect::<Vec<_>>()
            .try_into()
            .map_err(|_| StashError::OutsizedBundle)?;

        let tips = tips.into_iter().collect();

        Ok(InmemConsignment::<T>::with(
            schema,
            root_schema,
            genesis,
            tips,
            self.endpoints,
            anchored_bundles,
            empty!(),
        ))
    }
}

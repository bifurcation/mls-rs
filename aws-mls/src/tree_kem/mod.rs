use alloc::vec;
use alloc::vec::Vec;
use aws_mls_codec::{MlsDecode, MlsEncode, MlsSize};
#[cfg(feature = "std")]
use core::fmt::Display;
use itertools::Itertools;

#[cfg(feature = "std")]
use std::collections::HashMap;

use aws_mls_core::{error::IntoAnyError, identity::IdentityProvider};

#[cfg(feature = "tree_index")]
use aws_mls_core::identity::SigningIdentity;

use math as tree_math;
use node::{LeafIndex, NodeIndex, NodeVec};

use self::leaf_node::LeafNode;

use crate::client::MlsError;
use crate::crypto::{self, CipherSuiteProvider, HpkeSecretKey};

use crate::group::{
    proposal::{AddProposal, ProposalType, RemoveProposal, UpdateProposal},
    proposal_filter::ProposalBundle,
    Sender,
};
use crate::tree_kem::tree_hash::TreeHashes;

mod capabilities;
pub(crate) mod hpke_encryption;
mod lifetime;
pub(crate) mod math;
pub mod node;
pub mod parent_hash;
pub mod path_secret;
mod private;
mod tree_hash;
pub mod tree_validator;
pub mod update_path;

pub use capabilities::*;
pub use lifetime::*;
pub(crate) use private::*;
pub use update_path::*;

use tree_index::*;

use self::path_secret::{PathSecret, PathSecretGenerator};
pub mod kem;
pub mod leaf_node;
pub mod leaf_node_validator;
mod tree_index;

#[cfg(feature = "std")]
pub(crate) mod tree_utils;

#[cfg(all(test, feature = "external_commit"))]
mod interop_test_vectors;

#[derive(Clone, Debug, MlsEncode, MlsDecode, MlsSize, Default)]
pub struct TreeKemPublic {
    #[cfg(feature = "tree_index")]
    index: TreeIndex,
    pub(crate) nodes: NodeVec,
    tree_hashes: TreeHashes,
}

impl PartialEq for TreeKemPublic {
    fn eq(&self, other: &Self) -> bool {
        self.nodes == other.nodes
    }
}

impl TreeKemPublic {
    pub fn new() -> TreeKemPublic {
        Default::default()
    }

    #[cfg(not(feature = "tree_index"))]
    pub(crate) async fn import_node_data(nodes: NodeVec) -> Result<TreeKemPublic, MlsError> {
        Ok(TreeKemPublic {
            nodes,
            ..Default::default()
        })
    }

    #[cfg(feature = "tree_index")]
    pub(crate) async fn import_node_data<IP>(
        nodes: NodeVec,
        identity_provider: &IP,
    ) -> Result<TreeKemPublic, MlsError>
    where
        IP: IdentityProvider,
    {
        let mut tree = TreeKemPublic {
            nodes,
            ..Default::default()
        };

        tree.initialize_index_if_necessary(identity_provider)
            .await?;

        Ok(tree)
    }

    #[cfg(feature = "tree_index")]
    pub(crate) async fn initialize_index_if_necessary<IP: IdentityProvider>(
        &mut self,
        identity_provider: &IP,
    ) -> Result<(), MlsError> {
        if !self.index.is_initialized() {
            self.index = TreeIndex::new();

            for (leaf_index, leaf) in self.nodes.non_empty_leaves() {
                index_insert(&mut self.index, leaf, leaf_index, identity_provider).await?;
            }
        }

        Ok(())
    }

    #[cfg(feature = "tree_index")]
    pub(crate) fn get_leaf_node_with_identity(&self, identity: &[u8]) -> Option<LeafIndex> {
        self.index.get_leaf_index_with_identity(identity)
    }

    #[cfg(not(feature = "tree_index"))]
    pub(crate) async fn get_leaf_node_with_identity<I: IdentityProvider>(
        &self,
        identity: &[u8],
        id_provider: &I,
    ) -> Result<Option<LeafIndex>, MlsError> {
        for (i, leaf) in self.nodes.non_empty_leaves() {
            let leaf_id = id_provider
                .identity(&leaf.signing_identity)
                .await
                .map_err(|e| MlsError::IdentityProviderError(e.into_any_error()))?;

            if leaf_id == identity {
                return Ok(Some(i));
            }
        }

        Ok(None)
    }

    pub(crate) fn export_node_data(&self) -> NodeVec {
        self.nodes.clone()
    }

    pub async fn derive<I: IdentityProvider, CP: CipherSuiteProvider>(
        leaf_node: LeafNode,
        secret_key: HpkeSecretKey,
        identity_provider: &I,
        cipher_suite_provider: &CP,
    ) -> Result<(TreeKemPublic, TreeKemPrivate), MlsError> {
        let mut public_tree = TreeKemPublic::new();
        public_tree
            .add_leaves(vec![leaf_node], identity_provider, cipher_suite_provider)
            .await?;

        let private_tree = TreeKemPrivate::new_self_leaf(LeafIndex(0), secret_key);

        Ok((public_tree, private_tree))
    }

    pub fn total_leaf_count(&self) -> u32 {
        self.nodes.total_leaf_count()
    }

    #[cfg(any(test, feature = "custom_proposal", feature = "tree_index"))]
    pub fn occupied_leaf_count(&self) -> u32 {
        self.nodes.occupied_leaf_count()
    }

    pub fn get_leaf_node(&self, index: LeafIndex) -> Result<&LeafNode, MlsError> {
        self.nodes.borrow_as_leaf(index)
    }

    pub fn find_leaf_node(&self, leaf_node: &LeafNode) -> Option<LeafIndex> {
        self.nodes.non_empty_leaves().find_map(
            |(index, node)| {
                if node == leaf_node {
                    Some(index)
                } else {
                    None
                }
            },
        )
    }

    #[cfg(feature = "custom_proposal")]
    pub fn can_support_proposal(&self, proposal_type: ProposalType) -> bool {
        #[cfg(feature = "tree_index")]
        return self.index.count_supporting_proposal(proposal_type) == self.occupied_leaf_count();

        #[cfg(not(feature = "tree_index"))]
        self.nodes
            .non_empty_leaves()
            .all(|(_, l)| l.capabilities.proposals.contains(&proposal_type))
    }

    // Note that a partial failure of this function will leave the tree in a bad state. Modifying a
    // tree should always be done on a clone of the tree, which is how commits are processed
    pub async fn add_leaves<I: IdentityProvider, CP: CipherSuiteProvider>(
        &mut self,
        leaf_nodes: Vec<LeafNode>,
        id_provider: &I,
        cipher_suite_provider: &CP,
    ) -> Result<Vec<LeafIndex>, MlsError> {
        let mut added = self.add_leaves_internal(&leaf_nodes, id_provider).await?;
        self.update_hashes(&mut Vec::new(), &added.added, cipher_suite_provider)?;

        if let Some(e) = added.errors.pop() {
            return Err(e);
        }

        Ok(added.added)
    }

    pub async fn rekey_leaf<C>(
        &mut self,
        index: LeafIndex,
        leaf_node: LeafNode,
        identity_provider: C,
    ) -> Result<(), MlsError>
    where
        C: IdentityProvider,
    {
        // Update the cache
        #[cfg(feature = "tree_index")]
        {
            let existing_leaf = self.nodes.borrow_as_leaf(index)?;

            let existing_identity = identity_provider
                .identity(&existing_leaf.signing_identity)
                .await
                .map_err(|e| MlsError::IdentityProviderError(e.into_any_error()))?;

            self.index.remove(existing_leaf, &existing_identity);
            index_insert(&mut self.index, &leaf_node, index, &identity_provider).await?;
        }

        #[cfg(not(feature = "tree_index"))]
        index_insert(&self.nodes, &leaf_node, index, &identity_provider).await?;

        *self.nodes.borrow_as_leaf_mut(index)? = leaf_node;

        Ok(())
    }

    pub fn non_empty_leaves(&self) -> impl Iterator<Item = (LeafIndex, &LeafNode)> + '_ {
        self.nodes.non_empty_leaves()
    }

    pub fn leaves(&self) -> impl Iterator<Item = Option<&LeafNode>> + '_ {
        self.nodes.leaves()
    }

    pub(crate) fn update_node(
        &mut self,
        pub_key: crypto::HpkePublicKey,
        index: NodeIndex,
    ) -> Result<(), MlsError> {
        self.nodes
            .borrow_or_fill_node_as_parent(index, &pub_key)
            .map(|p| {
                p.public_key = pub_key;
                p.unmerged_leaves = vec![];
            })
    }

    pub(crate) async fn apply_update_path<IP, CP>(
        &mut self,
        sender: LeafIndex,
        update_path: &ValidatedUpdatePath,
        identity_provider: IP,
        cipher_suite_provider: &CP,
    ) -> Result<(), MlsError>
    where
        IP: IdentityProvider,
        CP: CipherSuiteProvider,
    {
        // Install the new leaf node
        let existing_leaf = self.nodes.borrow_as_leaf_mut(sender)?;

        #[cfg(feature = "tree_index")]
        let original_leaf_node = existing_leaf.clone();

        #[cfg(feature = "tree_index")]
        let original_identity = identity_provider
            .identity(&original_leaf_node.signing_identity)
            .await
            .map_err(|e| MlsError::IdentityProviderError(e.into_any_error()))?;

        *existing_leaf = update_path.leaf_node.clone();

        // Update the rest of the nodes on the direct path
        let path = self.nodes.direct_path(sender)?;

        for (node, dp) in update_path.nodes.iter().zip(path.into_iter()) {
            node.as_ref()
                .map(|n| self.update_node(n.public_key.clone(), dp))
                .transpose()?;
        }

        #[cfg(feature = "tree_index")]
        self.index.remove(&original_leaf_node, &original_identity);

        index_insert(
            #[cfg(feature = "tree_index")]
            &mut self.index,
            #[cfg(not(feature = "tree_index"))]
            &self.nodes,
            &update_path.leaf_node,
            sender,
            &identity_provider,
        )
        .await?;

        // Verify the parent hash of the new sender leaf node and update the parent hash values
        // in the local tree
        self.update_parent_hashes(sender, Some(&update_path.leaf_node), cipher_suite_provider)?;

        Ok(())
    }

    fn update_unmerged(&mut self, index: LeafIndex) -> Result<(), MlsError> {
        // For a given leaf index, find parent nodes and add the leaf to the unmerged leaf
        self.nodes.direct_path(index)?.into_iter().for_each(|i| {
            if let Ok(p) = self.nodes.borrow_as_parent_mut(i) {
                p.unmerged_leaves.push(index)
            }
        });

        Ok(())
    }

    pub async fn batch_edit<I, CP>(
        &mut self,
        proposal_bundle: &mut ProposalBundle,
        id_provider: &I,
        cipher_suite_provider: &CP,
        filter: bool,
    ) -> Result<BatchEditOutput, MlsError>
    where
        I: IdentityProvider,
        CP: CipherSuiteProvider,
    {
        // Apply removes (they commute with updates because they don't touch the same leaves)
        let mut bad_indices = vec![];
        let mut removed = vec![];

        for (i, p) in proposal_bundle.by_type::<RemoveProposal>().enumerate() {
            let index = p.proposal.to_remove;
            let res = self.nodes.blank_leaf_node(index);

            match res {
                Ok(Some(old_leaf)) => {
                    // This shouldn't fail if `blank_leaf_node` succedded.
                    self.nodes.blank_direct_path(index)?;

                    #[cfg(feature = "tree_index")]
                    {
                        // If this fails, it's not because the proposal is bad.
                        let identity = identity(&old_leaf.signing_identity, id_provider).await?;

                        self.index.remove(&old_leaf, &identity);
                    }

                    removed.push((index, old_leaf));
                }
                Err(e) => {
                    if !filter || p.is_by_value() {
                        return Err(e);
                    }

                    bad_indices.push(i);
                }
                Ok(None) => {
                    if !filter || p.is_by_value() {
                        return Err(MlsError::RemovingNonExistingMember);
                    }

                    bad_indices.push(i);
                }
            }
        }

        bad_indices
            .iter()
            .rev()
            .for_each(|i| proposal_bundle.remove::<RemoveProposal>(*i));

        // Remove from the tree old leaves from updates
        let mut partial_updates = vec![];
        let mut bad_indices = vec![];

        for (i, p) in proposal_bundle.by_type::<UpdateProposal>().enumerate() {
            let index = match p.sender {
                Sender::Member(index) => LeafIndex(index),
                _ => {
                    if !filter || p.is_by_value() {
                        return Err(MlsError::InvalidProposalTypeForSender {
                            proposal_type: ProposalType::UPDATE,
                            sender: p.sender,
                            by_ref: p.is_by_reference(),
                        });
                    }

                    bad_indices.push(i);
                    continue;
                }
            };

            let new_leaf = p.proposal.leaf_node.clone();

            // `blank_leaf_node` shouldn't fail because we already found the leaf when verifying the proposal
            match self.nodes.blank_leaf_node(index)? {
                Some(old_leaf) => {
                    let res = id_provider
                        .valid_successor(&old_leaf.signing_identity, &new_leaf.signing_identity)
                        .await
                        .map_err(|e| MlsError::IdentityProviderError(e.into_any_error()));

                    match res {
                        Ok(true) => {
                            #[cfg(feature = "tree_index")]
                            let old_id = identity(&old_leaf.signing_identity, id_provider).await?;
                            #[cfg(feature = "tree_index")]
                            self.index.remove(&old_leaf, &old_id);

                            partial_updates.push((index, old_leaf, new_leaf));
                        }
                        Err(e) => {
                            if !filter || p.is_by_value() {
                                return Err(MlsError::IdentityProviderError(e.into_any_error()));
                            }

                            bad_indices.push(i);
                            self.nodes.insert_leaf(index, old_leaf);
                        }
                        Ok(false) => {
                            if !filter || p.is_by_value() {
                                return Err(MlsError::InvalidSuccessor);
                            }

                            bad_indices.push(i);
                            self.nodes.insert_leaf(index, old_leaf);
                        }
                    }
                }
                None => {
                    if !filter || p.is_by_value() {
                        return Err(MlsError::UpdatingNonExistingMember);
                    }

                    bad_indices.push(i);
                }
            }
        }

        bad_indices
            .iter()
            .rev()
            .for_each(|i| proposal_bundle.remove::<UpdateProposal>(*i));

        // Compute a maximal set of updates that can be applied

        // Updates that can't be applied and can't be reverted due to other updates being applied
        let mut bad_indices = vec![];

        #[allow(unused)]
        let res = loop {
            let res = find_max_update_set(
                #[cfg(feature = "tree_index")]
                self.index.clone(),
                #[cfg(not(feature = "tree_index"))]
                &self.nodes,
                &mut bad_indices,
                &partial_updates,
                id_provider,
                filter,
            )
            .await;

            if let Ok(Some(index)) = res {
                break index;
            }

            // Updates must be by reference
            if !filter {
                res?;
            }
        };

        #[cfg(feature = "tree_index")]
        {
            self.index = res.tree_index;
        }

        bad_indices.sort();

        bad_indices
            .iter()
            .rev()
            .for_each(|i| proposal_bundle.remove::<UpdateProposal>(*i));

        // For updating hashes later
        let mut updated = vec![];

        // Finish the updates : add new leaves and blank the paths
        for (i, (index, old_leaf, new_leaf)) in partial_updates.into_iter().enumerate() {
            if !bad_indices.contains(&i) {
                self.nodes.insert_leaf(index, new_leaf);

                // This shouldn't fail because we already found the leaf before.
                self.nodes.blank_direct_path(index)?;

                updated.push(index);
            } else {
                self.nodes.insert_leaf(index, old_leaf);
            }
        }

        // Apply adds
        let additions = proposal_bundle
            .by_type::<AddProposal>()
            .map(|p| p.proposal.key_package.leaf_node.clone())
            .collect::<Vec<_>>();

        let added_info = self.add_leaves_internal(&additions, id_provider).await?;

        for (i, e) in added_info
            .bad_indices
            .into_iter()
            .zip(added_info.errors.into_iter())
            .rev()
        {
            if !filter || proposal_bundle.add_proposals()[i].is_by_value() {
                return Err(e);
            }

            proposal_bundle.remove::<AddProposal>(i)
        }

        self.nodes.trim();

        let mut path_blanked = proposal_bundle
            .by_type::<RemoveProposal>()
            .map(|p| p.proposal().to_remove)
            .chain(updated.into_iter())
            .collect_vec();

        self.update_hashes(&mut path_blanked, &added_info.added, cipher_suite_provider)?;

        Ok(BatchEditOutput {
            removed,
            added: added_info.added,
        })
    }

    async fn add_leaves_internal<I: IdentityProvider>(
        &mut self,
        additions: &[LeafNode],
        id_provider: &I,
    ) -> Result<AddedLeavesInfo, MlsError> {
        let mut bad_indices = vec![];
        let mut start = LeafIndex(0);
        let mut added = vec![];
        let mut errors = vec![];

        for (i, leaf) in additions.iter().enumerate() {
            let index = self.nodes.next_empty_leaf(start);

            #[cfg(feature = "tree_index")]
            let res = index_insert(&mut self.index, leaf, index, id_provider).await;

            #[cfg(not(feature = "tree_index"))]
            let res = index_insert(&self.nodes, leaf, index, id_provider).await;

            if let Err(e) = res {
                bad_indices.push(i);
                errors.push(e);
            } else {
                self.nodes.insert_leaf(index, leaf.clone());
                self.update_unmerged(index)?;
                start = index;
                added.push(index);
            }
        }

        Ok(AddedLeavesInfo {
            bad_indices,
            added,
            errors,
        })
    }
}

struct AddedLeavesInfo {
    pub bad_indices: Vec<usize>,
    pub added: Vec<LeafIndex>,
    pub errors: Vec<MlsError>,
}

pub struct BatchEditOutput {
    pub removed: Vec<(LeafIndex, LeafNode)>,
    pub added: Vec<LeafIndex>,
}

struct MaxUpdateSetOutput {
    #[cfg(feature = "tree_index")]
    tree_index: TreeIndex,
}

async fn find_max_update_set<I: IdentityProvider>(
    #[cfg(feature = "tree_index")] mut try_index: TreeIndex,
    #[cfg(not(feature = "tree_index"))] nodes: &NodeVec,
    broken_updates: &mut Vec<usize>,
    partial_updates: &[(LeafIndex, LeafNode, LeafNode)],
    id_provider: &I,
    filter: bool,
) -> Result<Option<MaxUpdateSetOutput>, MlsError> {
    let mut bad_indices = vec![];

    let partial_updates = partial_updates
        .iter()
        .enumerate()
        .filter(|(i, _)| !broken_updates.contains(i));

    for (i, (index, old_leaf, new_leaf)) in partial_updates {
        #[cfg(feature = "tree_index")]
        let res = index_insert(&mut try_index, new_leaf, *index, id_provider).await;

        #[cfg(not(feature = "tree_index"))]
        let res = index_insert(nodes, new_leaf, *index, id_provider).await;

        if res.is_err() {
            #[cfg(feature = "tree_index")]
            let res = index_insert(&mut try_index, old_leaf, *index, id_provider).await;

            #[cfg(not(feature = "tree_index"))]
            let res = index_insert(nodes, old_leaf, *index, id_provider).await;
            let err = res.is_err();

            if !filter {
                res?;
            }

            if err {
                broken_updates.push(i);
                return Ok(None);
            }

            bad_indices.push(i);
        }

        if !filter {
            res?;
        }
    }

    broken_updates.append(&mut bad_indices);

    Ok(Some(MaxUpdateSetOutput {
        #[cfg(feature = "tree_index")]
        tree_index: try_index,
    }))
}

#[cfg(feature = "tree_index")]
async fn identity<I: IdentityProvider>(
    signing_id: &SigningIdentity,
    provider: &I,
) -> Result<Vec<u8>, MlsError> {
    provider
        .identity(signing_id)
        .await
        .map_err(|e| MlsError::IdentityProviderError(e.into_any_error()))
}

#[cfg(feature = "std")]
impl Display for TreeKemPublic {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", tree_utils::build_ascii_tree(&self.nodes))
    }
}

#[cfg(test)]
use crate::group::{proposal::Proposal, proposal_filter::ProposalSource};

#[cfg(test)]
impl TreeKemPublic {
    pub async fn update_leaf<I, CP>(
        &mut self,
        leaf_index: u32,
        leaf_node: LeafNode,
        identity_provider: &I,
        cipher_suite_provider: &CP,
    ) -> Result<(), MlsError>
    where
        I: IdentityProvider,
        CP: CipherSuiteProvider,
    {
        let p = Proposal::Update(UpdateProposal {
            leaf_node: leaf_node.clone(),
        });

        let mut bundle = ProposalBundle::default();
        bundle.add(p, Sender::Member(leaf_index), ProposalSource::ByValue);

        self.batch_edit(&mut bundle, identity_provider, cipher_suite_provider, true)
            .await?;

        Ok(())
    }

    pub async fn remove_leaves<I, CP>(
        &mut self,
        indexes: Vec<LeafIndex>,
        identity_provider: &I,
        cipher_suite_provider: &CP,
    ) -> Result<Vec<(LeafIndex, LeafNode)>, MlsError>
    where
        I: IdentityProvider,
        CP: CipherSuiteProvider,
    {
        let proposals = indexes
            .iter()
            .copied()
            .map(|to_remove| Proposal::Remove(RemoveProposal { to_remove }));

        let mut bundle = ProposalBundle::default();

        for p in proposals {
            bundle.add(p, Sender::Member(0), ProposalSource::ByValue)
        }

        self.batch_edit(&mut bundle, identity_provider, cipher_suite_provider, true)
            .await
            .map(|res| res.removed)
    }

    pub fn get_leaf_nodes(&self) -> Vec<&LeafNode> {
        self.nodes.non_empty_leaves().map(|(_, l)| l).collect()
    }
}

#[cfg(test)]
pub(crate) mod test_utils {
    use crate::crypto::test_utils::TestCryptoProvider;
    use crate::signer::Signable;
    use alloc::vec::Vec;
    use alloc::{format, vec};
    use aws_mls_core::crypto::CipherSuiteProvider;
    use aws_mls_core::group::Capabilities;
    use aws_mls_core::identity::BasicCredential;

    use crate::identity::test_utils::get_test_signing_identity;
    use crate::{
        cipher_suite::CipherSuite,
        crypto::{test_utils::test_cipher_suite_provider, HpkeSecretKey, SignatureSecretKey},
        identity::basic::BasicIdentityProvider,
        tree_kem::leaf_node::test_utils::get_basic_test_node_sig_key,
    };

    use super::leaf_node::{ConfigProperties, LeafNodeSigningContext, LeafNodeSource};
    use super::node::LeafIndex;
    use super::Lifetime;
    use super::{
        leaf_node::{test_utils::get_basic_test_node, LeafNode},
        TreeKemPrivate, TreeKemPublic,
    };

    #[derive(Debug)]
    pub(crate) struct TestTree {
        pub public: TreeKemPublic,
        pub private: TreeKemPrivate,
        pub creator_leaf: LeafNode,
        pub creator_signing_key: SignatureSecretKey,
        pub creator_hpke_secret: HpkeSecretKey,
    }

    pub(crate) async fn get_test_tree(cipher_suite: CipherSuite) -> TestTree {
        let (creator_leaf, creator_hpke_secret, creator_signing_key) =
            get_basic_test_node_sig_key(cipher_suite, "creator").await;

        let (test_public, test_private) = TreeKemPublic::derive(
            creator_leaf.clone(),
            creator_hpke_secret.clone(),
            &BasicIdentityProvider,
            &test_cipher_suite_provider(cipher_suite),
        )
        .await
        .unwrap();

        TestTree {
            public: test_public,
            private: test_private,
            creator_leaf,
            creator_signing_key,
            creator_hpke_secret,
        }
    }

    pub async fn get_test_leaf_nodes(cipher_suite: CipherSuite) -> Vec<LeafNode> {
        [
            get_basic_test_node(cipher_suite, "A").await,
            get_basic_test_node(cipher_suite, "B").await,
            get_basic_test_node(cipher_suite, "C").await,
        ]
        .to_vec()
    }

    impl TreeKemPublic {
        #[cfg(feature = "tree_index")]
        pub fn equal_internals(&self, other: &TreeKemPublic) -> bool {
            self.tree_hashes == other.tree_hashes && self.index == other.index
        }
    }

    #[derive(Debug, Clone)]
    pub struct TreeWithSigners {
        pub tree: TreeKemPublic,
        pub signers: Vec<Option<SignatureSecretKey>>,
        pub group_id: Vec<u8>,
    }

    impl TreeWithSigners {
        pub async fn make_full_tree<P: CipherSuiteProvider>(
            n_leaves: u32,
            cs: &P,
        ) -> TreeWithSigners {
            let mut tree = TreeWithSigners {
                tree: TreeKemPublic::new(),
                signers: vec![],
                group_id: cs.random_bytes_vec(cs.kdf_extract_size()).unwrap(),
            };

            tree.add_member("Alice", cs).await;

            // A adds B, B adds C, C adds D etc.
            for i in 1..n_leaves {
                tree.add_member(&format!("Alice{i}"), cs).await;
                tree.update_committer_path(i - 1, cs).await;
            }

            tree
        }

        pub async fn add_member<P: CipherSuiteProvider>(&mut self, name: &str, cs: &P) {
            let (leaf, signer) = make_leaf(name, cs).await;
            let index = self.tree.nodes.next_empty_leaf(LeafIndex(0));
            self.tree.nodes.insert_leaf(index, leaf);
            self.tree.update_unmerged(index).unwrap();
            let index = *index as usize;

            match self.signers.len() {
                l if l == index => self.signers.push(Some(signer)),
                l if l > index => self.signers[index] = Some(signer),
                _ => panic!("signer tree size mismatch"),
            }
        }

        pub fn remove_member(&mut self, member: u32) {
            self.tree
                .nodes
                .blank_direct_path(LeafIndex(member))
                .unwrap();

            self.tree.nodes.blank_leaf_node(LeafIndex(member)).unwrap();

            *self
                .signers
                .get_mut(member as usize)
                .expect("signer tree size mismatch") = None;
        }

        pub async fn update_committer_path<P: CipherSuiteProvider>(
            &mut self,
            committer: u32,
            cs: &P,
        ) {
            let committer = LeafIndex(committer);

            let path = self.tree.nodes.direct_path(committer).unwrap();
            let filtered = self.tree.nodes.filtered(committer).unwrap();

            for (i, f) in path.into_iter().zip(filtered) {
                if !f {
                    self.tree
                        .update_node(cs.kem_generate().unwrap().1, i)
                        .unwrap();
                }
            }

            self.tree.tree_hashes.current = vec![];
            self.tree.tree_hashes.original = vec![];
            self.tree.tree_hash(cs).unwrap();

            let parent_hash = self.tree.update_parent_hashes(committer, None, cs).unwrap();

            self.tree
                .nodes
                .borrow_as_leaf_mut(committer)
                .unwrap()
                .leaf_node_source = LeafNodeSource::Commit(parent_hash);

            self.tree.tree_hashes.current = vec![];
            self.tree.tree_hashes.original = vec![];
            self.tree.tree_hash(cs).unwrap();

            let context = LeafNodeSigningContext {
                group_id: Some(&self.group_id),
                leaf_index: Some(*committer),
            };

            let signer = self.signers[*committer as usize].as_ref().unwrap();

            self.tree
                .nodes
                .borrow_as_leaf_mut(committer)
                .unwrap()
                .sign(cs, signer, &context)
                .unwrap();

            self.tree.tree_hashes.current = vec![];
            self.tree.tree_hashes.original = vec![];
            self.tree.tree_hash(cs).unwrap();
        }
    }

    pub async fn make_leaf<P: CipherSuiteProvider>(
        name: &str,
        cs: &P,
    ) -> (LeafNode, SignatureSecretKey) {
        let (signing_identity, signature_key) =
            get_test_signing_identity(cs.cipher_suite(), name.as_bytes().to_vec());

        let capabilities = Capabilities {
            credentials: vec![BasicCredential::credential_type()],
            cipher_suites: TestCryptoProvider::all_supported_cipher_suites(),
            ..Default::default()
        };

        let properties = ConfigProperties {
            capabilities,
            extensions: Default::default(),
        };

        let (leaf, _) = LeafNode::generate(
            cs,
            properties,
            signing_identity,
            &signature_key,
            Lifetime::years(1).unwrap(),
        )
        .await
        .unwrap();

        (leaf, signature_key)
    }
}

#[cfg(test)]
mod tests {
    use crate::client::test_utils::{TEST_CIPHER_SUITE, TEST_PROTOCOL_VERSION};
    use crate::crypto::test_utils::{test_cipher_suite_provider, TestCryptoProvider};

    #[cfg(feature = "custom_proposal")]
    use crate::group::proposal::ProposalType;

    use crate::group::proposal::{Proposal, RemoveProposal, UpdateProposal};
    use crate::group::proposal_filter::{ProposalBundle, ProposalSource};
    use crate::group::proposal_ref::ProposalRef;
    use crate::group::Sender;
    use crate::identity::basic::BasicIdentityProvider;
    use crate::key_package::test_utils::test_key_package;
    use crate::tree_kem::leaf_node::test_utils::get_basic_test_node;
    use crate::tree_kem::leaf_node::LeafNode;
    use crate::tree_kem::node::{LeafIndex, Node, NodeIndex, NodeTypeResolver, Parent};
    use crate::tree_kem::parent_hash::ParentHash;
    use crate::tree_kem::test_utils::{get_test_leaf_nodes, get_test_tree};
    use crate::tree_kem::{MlsError, TreeKemPublic};
    use alloc::borrow::ToOwned;
    use alloc::vec;
    use alloc::vec::Vec;
    use assert_matches::assert_matches;

    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    #[cfg(not(target_arch = "wasm32"))]
    use futures_test::test;

    #[test]
    async fn test_derive() {
        for cipher_suite in TestCryptoProvider::all_supported_cipher_suites() {
            let test_tree = get_test_tree(cipher_suite).await;

            assert_eq!(
                test_tree.public.nodes[0],
                Some(Node::Leaf(test_tree.creator_leaf.clone()))
            );

            assert_eq!(test_tree.private.self_index, LeafIndex(0));

            assert_eq!(
                test_tree.private.secret_keys[0],
                Some(test_tree.creator_hpke_secret)
            );
        }
    }

    #[test]
    async fn test_import_export() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);
        let mut test_tree = get_test_tree(TEST_CIPHER_SUITE).await;

        let additional_key_packages = get_test_leaf_nodes(TEST_CIPHER_SUITE).await;

        test_tree
            .public
            .add_leaves(
                additional_key_packages,
                &BasicIdentityProvider,
                &cipher_suite_provider,
            )
            .await
            .unwrap();

        let exported = test_tree.public.export_node_data();

        let imported = TreeKemPublic::import_node_data(
            exported,
            #[cfg(feature = "tree_index")]
            &BasicIdentityProvider,
        )
        .await
        .unwrap();

        assert_eq!(test_tree.public.nodes, imported.nodes);

        #[cfg(feature = "tree_index")]
        assert_eq!(test_tree.public.index, imported.index);
    }

    #[test]
    async fn test_add_leaf() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);
        let mut tree = TreeKemPublic::new();

        let leaf_nodes = get_test_leaf_nodes(TEST_CIPHER_SUITE).await;

        let res = tree
            .add_leaves(
                leaf_nodes.clone(),
                &BasicIdentityProvider,
                &cipher_suite_provider,
            )
            .await
            .unwrap();

        // The leaf count should be equal to the number of packages we added
        assert_eq!(res.len(), leaf_nodes.len());
        assert_eq!(tree.occupied_leaf_count(), leaf_nodes.len() as u32);

        // Each added package should be at the proper index and searchable in the tree
        res.into_iter().zip(leaf_nodes.clone()).for_each(|(r, kp)| {
            assert_eq!(tree.get_leaf_node(r).unwrap(), &kp);
        });

        // Verify the underlying state
        #[cfg(feature = "tree_index")]
        assert_eq!(tree.index.len(), tree.occupied_leaf_count() as usize);

        assert_eq!(tree.nodes.len(), 5);
        assert_eq!(tree.nodes[0], leaf_nodes[0].clone().into());
        assert_eq!(tree.nodes[1], None);
        assert_eq!(tree.nodes[2], leaf_nodes[1].clone().into());
        assert_eq!(tree.nodes[3], None);
        assert_eq!(tree.nodes[4], leaf_nodes[2].clone().into());
    }

    #[test]
    async fn test_get_key_packages() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);
        let mut tree = TreeKemPublic::new();

        let key_packages = get_test_leaf_nodes(TEST_CIPHER_SUITE).await;

        tree.add_leaves(key_packages, &BasicIdentityProvider, &cipher_suite_provider)
            .await
            .unwrap();

        let key_packages = tree.get_leaf_nodes();
        assert_eq!(key_packages, key_packages.to_owned());
    }

    #[test]
    async fn test_add_leaf_duplicate() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);
        let mut tree = TreeKemPublic::new();

        let key_packages = get_test_leaf_nodes(TEST_CIPHER_SUITE).await;

        tree.add_leaves(
            key_packages.clone(),
            &BasicIdentityProvider,
            &cipher_suite_provider,
        )
        .await
        .unwrap();

        let res = tree
            .add_leaves(key_packages, &BasicIdentityProvider, &cipher_suite_provider)
            .await;

        assert_matches!(res, Err(MlsError::DuplicateLeafData(_)));
    }

    #[test]
    async fn test_add_leaf_empty_leaf() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);
        let mut tree = get_test_tree(TEST_CIPHER_SUITE).await.public;
        let key_packages = get_test_leaf_nodes(TEST_CIPHER_SUITE).await;

        tree.add_leaves(
            [key_packages[0].clone()].to_vec(),
            &BasicIdentityProvider,
            &cipher_suite_provider,
        )
        .await
        .unwrap();

        tree.nodes[0] = None; // Set the original first node to none
                              //
        tree.add_leaves(
            [key_packages[1].clone()].to_vec(),
            &BasicIdentityProvider,
            &cipher_suite_provider,
        )
        .await
        .unwrap();

        assert_eq!(tree.nodes[0], key_packages[1].clone().into());
        assert_eq!(tree.nodes[1], None);
        assert_eq!(tree.nodes[2], key_packages[0].clone().into());
        assert_eq!(tree.nodes.len(), 3)
    }

    #[test]
    async fn test_add_leaf_unmerged() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);
        let mut tree = get_test_tree(TEST_CIPHER_SUITE).await.public;
        let key_packages = get_test_leaf_nodes(TEST_CIPHER_SUITE).await;

        tree.add_leaves(
            [key_packages[0].clone(), key_packages[1].clone()].to_vec(),
            &BasicIdentityProvider,
            &cipher_suite_provider,
        )
        .await
        .unwrap();

        tree.nodes[3] = Parent {
            public_key: vec![].into(),
            parent_hash: ParentHash::empty(),
            unmerged_leaves: vec![],
        }
        .into();

        tree.add_leaves(
            [key_packages[2].clone()].to_vec(),
            &BasicIdentityProvider,
            &cipher_suite_provider,
        )
        .await
        .unwrap();

        assert_eq!(
            tree.nodes[3].as_parent().unwrap().unmerged_leaves,
            vec![LeafIndex(3)]
        )
    }

    #[test]
    async fn test_update_leaf() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);
        // Create a tree
        let mut tree = get_test_tree(TEST_CIPHER_SUITE).await.public;

        let key_packages = get_test_leaf_nodes(TEST_CIPHER_SUITE).await;

        tree.add_leaves(key_packages, &BasicIdentityProvider, &cipher_suite_provider)
            .await
            .unwrap();

        // Add in parent nodes so we can detect them clearing after update
        tree.nodes
            .direct_path(LeafIndex(0))
            .unwrap()
            .iter()
            .for_each(|&i| {
                tree.nodes
                    .borrow_or_fill_node_as_parent(i, &b"pub_key".to_vec().into())
                    .unwrap();
            });

        let original_size = tree.occupied_leaf_count();
        let original_leaf_index = LeafIndex(1);

        let updated_leaf = get_basic_test_node(TEST_CIPHER_SUITE, "A").await;

        tree.update_leaf(
            *original_leaf_index,
            updated_leaf.clone(),
            &BasicIdentityProvider,
            &cipher_suite_provider,
        )
        .await
        .unwrap();

        // The tree should not have grown due to an update
        assert_eq!(tree.occupied_leaf_count(), original_size);

        // The cache of tree package indexes should not have grown
        #[cfg(feature = "tree_index")]
        assert_eq!(tree.index.len() as u32, tree.occupied_leaf_count());

        // The key package should be updated in the tree
        assert_eq!(
            tree.get_leaf_node(original_leaf_index).unwrap(),
            &updated_leaf
        );

        // Verify that the direct path has been cleared
        tree.nodes
            .direct_path(LeafIndex(0))
            .unwrap()
            .iter()
            .for_each(|&i| {
                assert!(tree.nodes[i as usize].is_none());
            });
    }

    #[test]
    async fn test_update_leaf_not_found() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        // Create a tree
        let mut tree = get_test_tree(TEST_CIPHER_SUITE).await.public;

        let key_packages = get_test_leaf_nodes(TEST_CIPHER_SUITE).await;

        tree.add_leaves(key_packages, &BasicIdentityProvider, &cipher_suite_provider)
            .await
            .unwrap();

        let new_key_package = get_basic_test_node(TEST_CIPHER_SUITE, "new").await;

        assert_matches!(
            tree.update_leaf(
                128,
                new_key_package,
                &BasicIdentityProvider,
                &cipher_suite_provider
            )
            .await,
            Err(MlsError::InvalidNodeIndex(256))
        );
    }

    #[test]
    async fn test_remove_leaf() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        // Create a tree
        let mut tree = get_test_tree(TEST_CIPHER_SUITE).await.public;
        let key_packages = get_test_leaf_nodes(TEST_CIPHER_SUITE).await;

        let indexes = tree
            .add_leaves(
                key_packages.clone(),
                &BasicIdentityProvider,
                &cipher_suite_provider,
            )
            .await
            .unwrap();

        let original_leaf_count = tree.occupied_leaf_count();

        // Remove two leaves from the tree
        let expected_result: Vec<(LeafIndex, LeafNode)> = indexes
            .clone()
            .into_iter()
            .zip(key_packages)
            .map(|(index, ln)| (index, ln))
            .collect();

        let res = tree
            .remove_leaves(
                indexes.clone(),
                &BasicIdentityProvider,
                &cipher_suite_provider,
            )
            .await
            .unwrap();

        assert_eq!(res, expected_result);

        // The leaves should be removed from the tree
        assert_eq!(
            tree.occupied_leaf_count(),
            original_leaf_count - indexes.len() as u32
        );
    }

    #[test]
    async fn test_remove_leaf_middle() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        // Create a tree
        let mut tree = get_test_tree(TEST_CIPHER_SUITE).await.public;
        let leaf_nodes = get_test_leaf_nodes(TEST_CIPHER_SUITE).await;

        let to_remove = tree
            .add_leaves(
                leaf_nodes.clone(),
                &BasicIdentityProvider,
                &cipher_suite_provider,
            )
            .await
            .unwrap()[0];

        let original_leaf_count = tree.occupied_leaf_count();

        let res = tree
            .remove_leaves(
                vec![to_remove],
                &BasicIdentityProvider,
                &cipher_suite_provider,
            )
            .await
            .unwrap();

        assert_eq!(res, vec![(to_remove, leaf_nodes[0].clone())]);

        // The leaf count should have been reduced by 1
        assert_eq!(tree.occupied_leaf_count(), original_leaf_count - 1);

        // There should be a blank in the tree
        assert_eq!(
            tree.nodes.get(NodeIndex::from(to_remove) as usize).unwrap(),
            &None
        );
    }

    #[test]
    async fn test_create_blanks() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        // Create a tree
        let mut tree = get_test_tree(TEST_CIPHER_SUITE).await.public;

        let key_packages = get_test_leaf_nodes(TEST_CIPHER_SUITE).await;

        tree.add_leaves(key_packages, &BasicIdentityProvider, &cipher_suite_provider)
            .await
            .unwrap();

        let original_leaf_count = tree.occupied_leaf_count();

        let to_remove = vec![LeafIndex(2)];

        // Remove the leaf from the tree
        tree.remove_leaves(to_remove, &BasicIdentityProvider, &cipher_suite_provider)
            .await
            .unwrap();

        // The occupied leaf count should have been reduced by 1
        assert_eq!(tree.occupied_leaf_count(), original_leaf_count - 1);

        // The total leaf count should remain unchanged
        assert_eq!(tree.total_leaf_count(), original_leaf_count);

        // The location of key_packages[1] should now be blank
        let removed_location = tree
            .nodes
            .get(NodeIndex::from(LeafIndex(2)) as usize)
            .unwrap();

        assert_eq!(removed_location, &None);
    }

    #[test]
    async fn test_remove_leaf_failure() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        // Create a tree
        let mut tree = get_test_tree(TEST_CIPHER_SUITE).await.public;

        assert_matches!(
            tree.remove_leaves(
                vec![LeafIndex(128)],
                &BasicIdentityProvider,
                &cipher_suite_provider
            )
            .await,
            Err(MlsError::InvalidNodeIndex(256))
        );
    }

    #[test]
    async fn test_find_leaf_node() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);
        // Create a tree
        let mut tree = get_test_tree(TEST_CIPHER_SUITE).await.public;

        let leaf_nodes = get_test_leaf_nodes(TEST_CIPHER_SUITE).await;

        tree.add_leaves(
            leaf_nodes.clone(),
            &BasicIdentityProvider,
            &cipher_suite_provider,
        )
        .await
        .unwrap();

        // Find each node
        for (i, leaf_node) in leaf_nodes.iter().enumerate() {
            let expected_index = LeafIndex(i as u32 + 1);
            assert_eq!(tree.find_leaf_node(leaf_node), Some(expected_index));
        }
    }

    #[test]
    async fn batch_edit_works() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        let mut tree = get_test_tree(TEST_CIPHER_SUITE).await.public;
        let leaf_nodes = get_test_leaf_nodes(TEST_CIPHER_SUITE).await;

        tree.add_leaves(
            leaf_nodes.clone(),
            &BasicIdentityProvider,
            &cipher_suite_provider,
        )
        .await
        .unwrap();

        let mut bundle = ProposalBundle::default();

        let kp = test_key_package(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE, "D").await;
        let add = Proposal::Add(kp.into());
        bundle.add(add, Sender::Member(0), ProposalSource::ByValue);

        let update = UpdateProposal {
            leaf_node: get_basic_test_node(TEST_CIPHER_SUITE, "A").await,
        };

        let update = Proposal::Update(update);
        let pref = ProposalRef::new_fake(vec![1, 2, 3]);
        bundle.add(update, Sender::Member(1), ProposalSource::ByReference(pref));

        let remove = RemoveProposal {
            to_remove: LeafIndex(2),
        };

        let remove = Proposal::Remove(remove);
        bundle.add(remove, Sender::Member(0), ProposalSource::ByValue);

        tree.batch_edit(
            &mut bundle,
            &BasicIdentityProvider,
            &cipher_suite_provider,
            true,
        )
        .await
        .unwrap();

        assert_eq!(bundle.add_proposals().len(), 1);
        assert_eq!(bundle.remove_proposals().len(), 1);
        assert_eq!(bundle.update_proposals().len(), 1);
    }

    #[cfg(feature = "custom_proposal")]
    #[test]
    async fn custom_proposal_support() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);
        let mut tree = TreeKemPublic::new();

        let test_proposal_type = ProposalType::from(42);

        let mut leaf_nodes = get_test_leaf_nodes(TEST_CIPHER_SUITE).await;

        leaf_nodes
            .iter_mut()
            .for_each(|n| n.capabilities.proposals.push(test_proposal_type));

        tree.add_leaves(leaf_nodes, &BasicIdentityProvider, &cipher_suite_provider)
            .await
            .unwrap();

        assert!(tree.can_support_proposal(test_proposal_type));
        assert!(!tree.can_support_proposal(ProposalType::from(43)));

        tree.add_leaves(
            vec![get_basic_test_node(TEST_CIPHER_SUITE, "another").await],
            &BasicIdentityProvider,
            &cipher_suite_provider,
        )
        .await
        .unwrap();

        assert!(!tree.can_support_proposal(test_proposal_type));
    }
}

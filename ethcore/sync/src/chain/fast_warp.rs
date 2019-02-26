// Copyright 2015-2019 Parity Technologies (UK) Ltd.
// This file is part of Parity Ethereum.

// Parity Ethereum is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity Ethereum is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity Ethereum.  If not, see <http://www.gnu.org/licenses/>.
use std::collections::{HashSet, HashMap};
use std::iter::FromIterator;
use std::time::Instant;

// use ethcore_blockchain::{BlockChainDB};
use block_sync::{BlockDownloader, BlockRequest, BlockDownloaderImportError as DownloaderImportError};
use blocks::{SyncHeader};
use bytes::Bytes;
use ethcore::account_db::{AccountDBMut};
use ethereum_types::{H256, U256};
use ethtrie::{TrieDBMut, RlpCodec};
use hash::{KECCAK_EMPTY, KECCAK_NULL_RLP};
use hashdb::HashDB;
use journaldb::JournalDB;
use network::{PeerId};
use rlp::{Rlp, Prototype};
use sync_io::SyncIo;
use trie::{TrieMut, NibbleSlice, NodeCodec, node::Node};
use types::basic_account::BasicAccount;
use types::BlockNumber;

use super::BlockSet;

/// Number of blocks bellow best-block to start fast-warp sync
const BLOCKS_DELTA_START_SYNC: u64 = 3_000;
/// Number of blocks headers to download at first
const NUM_BLOCKS_HEADERS: u64 = 50_000;

#[derive(Debug, Clone)]
pub enum NodeDataRequest {
	/// The request is for a path in the state tree
	State,
	/// The request is for a storage path in the account's
	/// storage trie (the H256 value is the account's hash)
	Storage(H256),
	/// The request is for an account's code
	/// (the H256 value is the account's hash)
	Code(H256),
}

/// Define possible actions for the fast-warp sync
pub enum FastWarpAction {
	/// Continue on the same step
	Continue,
	/// Current step is done, move to next step
	NextStep,
	/// Error in the current state, abort
	Error,
}

/// Available requests for Fast Warp
pub enum FastWarpRequest {
	/// Retrieve some block data
	BlockSync(BlockRequest),
	/// Get a fast-warp chunk from a peer
	FastWarpData(H256, H256),
	/// Get a sub-trie from a peer
	NodeData(Vec<H256>),
	/// Request total difficulty from peer
	TotalDifficulty(BlockNumber),
	/// Request a block header
	BlockHeader(BlockNumber),
}

/// State of the Fast-Warp sync
pub enum FastWarpState {
	/// Sync is waiting for something (more peers, queue was full, etc.)
	Idle,
	/// Syncing the block headers from peers
	BlockSync(BlockDownloader),
	/// Syncing the state with FastWarp requests
	StateSync(StateDownloader),
	/// Filling the state trie with NodeData requests
	TrieSync(TrieDownloader),
	/// An error occured during the sync
	Error,
	/// Sync is finished
	Done,
}

/// State Downloader for the fast-warp protocol
pub struct StateDownloader {
	/// Hash of the next account to request
	next_account_from: H256,
	/// Key of the next state storage to request
	next_storage_from: H256,
	/// Hash of the last account's address received
	last_account_hash: H256,
	/// Key of the last storage key-value pair received
	last_storage_key: H256,
	/// To compute ETA
	started_at: Instant,
}

impl StateDownloader {
	/// Create a new State Downloader
	pub fn new() -> Self {
		StateDownloader {
			next_account_from: H256::zero(),
			// Sensibly large account on Kovan
			// next_account_from: H256::from("0e84c7646acf8871fa5598a0dbce244b49fb9577e531ef260e21af123d279e9e"),
			next_storage_from: H256::zero(),
			last_account_hash: H256::zero(),
			last_storage_key: H256::zero(),
			started_at: Instant::now(),
		}
	}

	/// Request a fast-warp chunk to a peer
	pub fn request(&self, _peer_id: PeerId) -> FastWarpRequest {
		FastWarpRequest::FastWarpData(self.next_account_from, self.next_storage_from)
	}

	/// Process incoming packet
	pub fn process(&mut self, r: &Rlp, db: &mut Box<JournalDB>, state_root: &mut H256) -> FastWarpAction {
		// This should be [account_hash, storage_key, storage_root]
		let num_accounts = r.item_count().unwrap();

		if num_accounts == 0 {
			return FastWarpAction::NextStep;
		}

		let mut last_item = (H256::zero(), H256::zero(), H256::zero());
		let mut account_pairs = Vec::with_capacity(num_accounts);
		account_pairs.resize(num_accounts, (H256::new(), Vec::new()));

		let mut should_finish = false;

		{
			let hashdb = db.as_hashdb_mut();

			for (idx, (account_rlp, account_pair)) in r.into_iter().zip(account_pairs.iter_mut()).enumerate() {
				let account_hash: H256 = account_rlp.val_at(0).expect("Invalid account_hash");

				let mut acct_db = AccountDBMut::from_hash(hashdb, account_hash);
				let mut storage_root = if self.last_account_hash == account_hash {
					self.last_storage_key
				}  else {
					H256::zero()
				};
				let mut last_storage_key = H256::zero();

				let storage_rlp = account_rlp.at(2).expect("Invalid storage_rlp");
				let storage_count = storage_rlp.item_count().expect("Invalid storage_count");

				{
					let mut storage_trie = if storage_root.is_zero() {
						TrieDBMut::new(&mut acct_db, &mut storage_root)
					} else {
						TrieDBMut::from_existing(&mut acct_db, &mut storage_root)
							.expect("Couldn't open TrieDB from storage_root")
					};

					for pair_rlp in storage_rlp.iter() {
						let k: Bytes  = pair_rlp.val_at(0).expect("Invalid storage_key RLP");
						let v: Bytes = pair_rlp.val_at(1).expect("Invalid storage_value RLP");

						last_storage_key = H256::from_slice(&k);
						storage_trie.insert(&k, &v).expect("Failed to insert KV pair in storage Trie");
					}
				}

				if storage_count == 0 {
					// trace!(target: "sync", "No storage!");
					// If there is no storage and only one element, which is the same as previously,
					// it is OVER
					if num_accounts == 1 && account_hash == self.last_account_hash {
						should_finish = true;
						break;
					}
				}

				let account_data_rlp = account_rlp.at(1).expect("Invalid account_data RLP");
				let account_nonce: U256 = account_data_rlp.val_at(0).expect("Invalid account nonce RLP");
				let account_balance: U256 = account_data_rlp.val_at(1).expect("Invalid account nonce RLP");
				let account_storage_root: H256 = account_data_rlp.val_at(2).expect("Invalid account nonce RLP");

				let code_hash = match account_data_rlp.item_count().expect("Invalid account_data RLP") {
					3 => KECCAK_EMPTY,
					4 => {
						let code: Bytes = account_data_rlp.val_at(3).expect("Invalid code");
						let code_hash = acct_db.insert(&code);

						code_hash
					},
					i => panic!("Invalid account_data_rlp items: {}", i),
				};

				let acc = BasicAccount {
					nonce: account_nonce,
					balance: account_balance,
					storage_root: storage_root,
					code_hash: code_hash,
				};

				let thin_rlp = ::rlp::encode(&acc);
				*account_pair = (account_hash, thin_rlp);

				// Ie. not the last one, storage_root should be known
				let is_last_item = idx == num_accounts - 1;
				if !is_last_item {
					if account_storage_root != storage_root {
						trace!(target: "sync",
							"Invalid storage_root! expected {:?}, got {:?}",
							account_storage_root, storage_root
						);
					}
				} else {
					last_item = (account_hash, last_storage_key, storage_root);
				}
			}
		}

		if should_finish {
			FastWarp::commit(db);
			return FastWarpAction::NextStep;
		}

		let progress = (last_item.0[0] as u32 * 256 + last_item.0[1] as u32)  as f64 / (256 * 256) as f64;
		let elapsed = self.started_at.elapsed();
		let elapsed = elapsed.as_secs() as f64 + (elapsed.subsec_nanos() as f64) / 1_000_000_000.0;
		let eta = (elapsed / progress) - elapsed;

		info!(
			target: "fast-warp",
			"Got fast-warp data up to {:?} ({} accounts) progress={}% ; eta={}s",
			last_item.0, num_accounts,
			(progress * 10_000.0).round() / 100.0,
			eta.round() as u32,
		);

		{
			let mut account_trie = if *state_root != KECCAK_NULL_RLP {
				TrieDBMut::from_existing(db.as_hashdb_mut(), state_root).unwrap()
			} else {
				TrieDBMut::new(db.as_hashdb_mut(), state_root)
			};

			for (hash, thin_rlp) in account_pairs {
				account_trie.insert(&hash, &thin_rlp).unwrap();
			}
		}

		FastWarp::commit(db);
		self.update(last_item.0, last_item.1, last_item.2);
		FastWarpAction::Continue
	}

	/// Update internal values
	fn update(&mut self, account_from: H256, storage_from: H256, storage_root: H256) {
		self.next_account_from = account_from;
		self.next_storage_from = H256::from(U256::from(storage_from) + U256::one());

		self.last_account_hash = account_from;
		self.last_storage_key = storage_root;
	}
}

/// Trie Downloader, requesting trie chunks with NodeData requests
pub struct TrieDownloader {
	/// State root target
	target: H256,
	// Set of common node keys from FastWarp sync and current sync from `target`
	common_nodes: HashSet<H256>,
	/// Inflight NodeData requests to peers
	in_flight_requests: HashMap<PeerId, Vec<H256>>,
	// Node Data keys to query to remote peer
	node_data_queries: HashSet<H256>,
	// Nibble Prefixes for each key
	node_data_prefixes: HashMap<H256, Vec<u8>>,
	// What requests have been sent
	node_data_requests: HashMap<H256, NodeDataRequest>,
}

impl TrieDownloader {
	/// Create a new Trie Downloader, targeting the given state root
	pub fn new(target: H256) -> Self {
		trace!(target: "fast-warp", "Starting Trie-Dl with target={:#?}", target);
		let mut node_data_requests = HashMap::new();

		// First, query the head of the state tree
		node_data_requests.insert(target, NodeDataRequest::State);
		let node_data_queries = HashSet::from_iter(vec![ target ]);

		TrieDownloader {
			common_nodes: HashSet::new(),
			in_flight_requests: HashMap::new(),
			node_data_prefixes: HashMap::new(),
			node_data_queries,
			node_data_requests,
			target,
		}
	}

	pub fn request(&mut self, peer_id: PeerId) -> FastWarpRequest {
		let mut node_data_hashes: Vec<H256> = self.node_data_queries
			.iter()
			.map(|h| h.clone())
			.collect::<Vec<H256>>();

		node_data_hashes.sort_unstable();
		let n = ::std::cmp::min(20, node_data_hashes.len());
		node_data_hashes = node_data_hashes[0..n].to_vec();

		for node_data_hash in node_data_hashes.iter() {
			self.node_data_queries.remove(node_data_hash);
		}

		self.in_flight_requests.insert(peer_id, node_data_hashes.clone());
		FastWarpRequest::NodeData(node_data_hashes)
	}

	/// Process incoming packet
	pub fn process(&mut self, peer_id: PeerId, r: &Rlp, db: &mut Box<JournalDB>, state_root: &mut H256) -> Result<FastWarpAction, DownloaderImportError> {
		let node_data_hashes = match self.in_flight_requests.get(&peer_id) {
			Some(vec) => vec,
			None => return Ok(FastWarpAction::Continue),
		};

		if node_data_hashes.len() != r.item_count().unwrap_or(0) {
			debug!(target: "sync",
				"Invalid NodeData RLP: asked for {} hashes, got {} items",
				node_data_hashes.len(),
				r.item_count().unwrap_or(0),
			);

			for node_data_hash in node_data_hashes.iter() {
				self.node_data_queries.insert(*node_data_hash);
			}

			return Err(DownloaderImportError::Invalid);
		}

		let mut accounts_to_insert = Vec::new();

		for (rlp_data, node_data_key) in r.iter().zip(node_data_hashes) {
			let request = self.node_data_requests.remove(&node_data_key).unwrap();
			let state_data: Bytes = rlp_data.data().expect("Invalid RLP").to_vec();

			match request {
				NodeDataRequest::Code(account_hash) => {
					warn!("Got code request: acc={:?} data={:?}", account_hash, state_data);
					continue;
				},
				_ => (),
			}

			// Insert the data in DB
			let inserted_key = db.as_hashdb_mut().insert(&state_data);
			info!("Inserted state data, expected_key? {}", inserted_key == *node_data_key);

			let node = RlpCodec::decode(&state_data).expect("Invalid RlpCodec");

			let prefix = self.node_data_prefixes.remove(&node_data_key).unwrap_or_default();
			// The request that was sent should always exist!
			self.node_data_queries.remove(&node_data_key);

			match node {
				Node::Empty => (),
				Node::Leaf(path, data) => {

					let prefix_len = prefix.len();
					let mut nibble_builder = Vec::new();
					let mut i = 0;
					let mut offset = 0;
					if prefix_len % 2 == 1 {
						nibble_builder.push(prefix[0]);
						i = 1;
						offset = 1;
					}
					while i < prefix_len {
						nibble_builder.push(16 * prefix[i] + prefix[i + 1]);
						i += 2;
					}

					let nibble_prefix = NibbleSlice::new_offset(&nibble_builder, offset);
					let hash_nible = NibbleSlice::new_composed(&nibble_prefix, &path);

					let key_vec = hash_nible.encoded(false);
					// First byte is 0 if length is even ; it should always be 32
					let key = H256::from_slice(&key_vec[1..]);
					warn!("Got NodeLeaf: r={:?} ; k={:?} ; data={:?}", request, key, data);

					match request {
						NodeDataRequest::State => {
							let account_hash = key;
							let account_trie = if *state_root != KECCAK_NULL_RLP {
								TrieDBMut::from_existing(db.as_hashdb_mut(), state_root).unwrap()
							} else {
								TrieDBMut::new(db.as_hashdb_mut(), state_root)
							};

							let account: BasicAccount = Rlp::new(data).as_val().expect("Invalid Account data");

							if let Some(account_in_db) = account_trie.get(&account_hash).unwrap() {
								let account_in_db: BasicAccount = Rlp::new(&account_in_db)
									.as_val().expect("Invalid Account data in DB");

								if account_in_db != account {
									accounts_to_insert.push((account_hash, account));
								}
							} else {
								accounts_to_insert.push((account_hash, account));
							}
						},
						_ => {
							warn!("Got request...");
						},
					}
				},
				Node::Extension(path, key_bytes) => {
					let key_rlp = Rlp::new(key_bytes);
					let key: H256 = match key_rlp.prototype().expect("Invalid Extension Key RLP") {
						Prototype::Null => continue,
						Prototype::Data(0) => continue,
						Prototype::Data(32) => {
							key_rlp.as_val().expect("Invalid Extension Key RLP")
						},
						proto => {
							warn!("Invalid Extension Key RLP: {:?}", proto);
							continue;
						},
					};

					if !db.as_hashdb().contains(&key) {
						let mut ext_prefixes = prefix.clone();
						let mut path_nibbles: Vec<u8> = path.iter().collect();
						ext_prefixes.append(&mut path_nibbles);

						self.node_data_prefixes.insert(key, ext_prefixes);
						self.node_data_requests.insert(key, request.clone());
						self.node_data_queries.insert(key);
					} else {
						self.common_nodes.insert(key);
					}
				},
				Node::Branch(branches, data_opt) => {
					for (branch_idx, branch_rlp) in branches.iter().enumerate() {
						let branch_rlp = Rlp::new(branch_rlp);
						let branch_key: H256 = match branch_rlp.prototype()
								.expect("Invalid Branch RLP")
						{
							Prototype::Null => continue,
							Prototype::Data(0) => continue,
							Prototype::Data(32) => {
								branch_rlp.as_val().expect("Invalid Branch RLP")
							},
							proto => {
								error!("Invalid branch RLP: {:?}", proto);
								continue;
							},
						};

						if !db.as_hashdb().contains(&branch_key) {
							// trace!(target: "sync", "Doesn't have branch in DB {:#?}", branch_key);
							let mut branch_prefixes = prefix.clone();
							branch_prefixes.push(branch_idx as u8);
							self.node_data_prefixes.insert(branch_key, branch_prefixes);
							self.node_data_requests.insert(branch_key, request.clone());
							self.node_data_queries.insert(branch_key);
						} else {
							self.common_nodes.insert(branch_key);
						}
					}
					if let Some(branch_data) = data_opt {
						warn!("Node Branch Data: r={:?} ; d={:#?}", request, branch_data);
					}
				},
			}
		}

		if accounts_to_insert.len() > 0 {
			// trace!(target: "sync", "New/Modified accounts detected!\n{:#?}", accounts_to_insert);

			let mut storage_root_queries = HashSet::new();
			let mut code_hash_queries = HashSet::new();

			{
				let db = db.as_hashdb_mut();
				for (account_hash, account) in accounts_to_insert.iter() {
					let acct_db = AccountDBMut::from_hash(db, *account_hash);

					if account.storage_root != KECCAK_NULL_RLP && !acct_db.contains(&account.storage_root) {
						storage_root_queries.insert(account.storage_root);

						self.node_data_requests.insert(account.storage_root, NodeDataRequest::Storage(account_hash.clone()));
						self.node_data_queries.insert(account.storage_root);
					}
					if account.code_hash != KECCAK_EMPTY && !acct_db.contains(&account.code_hash) {
						code_hash_queries.insert(account.code_hash);

						self.node_data_requests.insert(account.code_hash, NodeDataRequest::Code(account_hash.clone()));
						self.node_data_queries.insert(account.code_hash);
					}
				}
			}

			if storage_root_queries.len() > 0 {
				trace!(target: "sync", "Storage root queries: {:?}", storage_root_queries);
			}
			if code_hash_queries.len() > 0 {
				trace!(target: "sync", "Code hash queries: {:?}", code_hash_queries);
			}

			{
				let mut account_trie = if *state_root != KECCAK_NULL_RLP {
					TrieDBMut::from_existing(db.as_hashdb_mut(), state_root).unwrap()
				} else {
					TrieDBMut::new(db.as_hashdb_mut(), state_root)
				};

				for (hash, account) in accounts_to_insert.iter() {
					let thin_rlp = ::rlp::encode(account);
					account_trie.insert(&hash, &thin_rlp).unwrap();
				}
			}

			FastWarp::commit(db);
			trace!(target: "sync", "New state root: {:#?}", *state_root);
		}

		if self.node_data_queries.len() == 0 {
			let success = db.as_hashdb_mut().contains(&self.target);

			if success {
				info!(target: "sync", "Successfully finished Node Data requests");
				self.prune(db, state_root);
				return Ok(FastWarpAction::NextStep);
			} else {
				error!(target: "fast-warp", "Errored while fast-warping: could not find target in TrieDB");
				return Ok(FastWarpAction::Error);
			}
		}

		Ok(FastWarpAction::Continue)
	}

	/// Prune the DB removing all the old state data from the FastWarpSync state
	fn prune (&self, db: &mut Box<JournalDB>, state_root: &H256) {
		let mut to_delete: Vec<H256> = Vec::new();
		let mut count = 0;

		if *state_root != self.target {
			to_delete.push(*state_root);
		}

		while let Some(state_key) = to_delete.pop() {
			count += 1;

			if let Some(state_data) = db.as_hashdb().get(&state_key) {
				let node = RlpCodec::decode(&state_data).expect("Invalid RlpCodec");

				// trace!(target: "fast-warp", "Node: {:?}", node);

				// Only need to delete Branch children, if any
				match node {
					Node::Empty |
						Node::Leaf(_, _) |
						Node::Extension(_, _) => (),
					Node::Branch(branches, _) => {
						for branch_rlp in branches.iter() {
							let branch_rlp = Rlp::new(branch_rlp);
							let branch_key: H256 = match branch_rlp.prototype()
									.expect("Invalid Branch RLP")
							{
								Prototype::Null => continue,
								Prototype::Data(0) => continue,
								Prototype::Data(32) => {
									branch_rlp.as_val().expect("Invalid Branch RLP")
								},
								proto => {
									error!("Invalid branch RLP: {:?}", proto);
									continue;
								},
							};

							if branch_key == self.target || self.common_nodes.contains(&branch_key) {
							} else {
								to_delete.push(branch_key);
							}
						}
					},
				}
			} else {
				trace!(target: "fast-warp", "DB doesn't contain key {:#?}", state_key);
			}

			db.as_hashdb_mut().remove(&state_key);
		}

		FastWarp::commit(db);
		info!(target: "fast-warp", "Done pruning. Deleted {} keys", count);
	}
}

pub struct FastWarp {
	/// State of the fast-warp sync
	state: FastWarpState,
	/// Current state root
	state_root: H256,
	/// Mapping of block numbers to total difficulty of the block
	total_difficulties: HashMap<BlockNumber, U256>,
	/// Block number of sync start
	start_block_number: Option<BlockNumber>,
}

impl FastWarp {
	pub fn new() -> std::io::Result<FastWarp> {
		Ok(FastWarp {
			state: FastWarpState::Idle,
			state_root: KECCAK_NULL_RLP,
			total_difficulties: HashMap::new(),
			start_block_number: None,
		})
	}

	pub fn is_done(&self) -> bool {
		match self.state {
			FastWarpState::Done => true,
			_ => false,
		}
	}

	pub fn blocks_downloader(&mut self) -> Option<&mut BlockDownloader> {
		match self.state {
			FastWarpState::BlockSync(ref mut block_dl) => Some(block_dl),
			_ => None,
		}
	}

	/// Process incoming packet
	pub fn process(&mut self, io: &mut SyncIo, peer_id: PeerId, r: &Rlp) -> Result<(), DownloaderImportError> {
		let mut db = io.chain().journal_db();
		let next_state = match self.state {
			FastWarpState::BlockSync(ref mut _block_dl) => {
				trace!(target: "fast-warp", "Invalid packet with state: BlockSync");
				None
			},
			FastWarpState::StateSync(ref mut state_dl) => {
				let action = state_dl.process(r, &mut db, &mut self.state_root);

				match action {
					FastWarpAction::NextStep => {
						let target_block_header = io.chain().best_block_header();
						let target_state_root: H256 = target_block_header.state_root().clone();
						Some(FastWarpState::TrieSync(TrieDownloader::new(target_state_root)))
					},
					FastWarpAction::Continue => None,
					FastWarpAction::Error => Some(FastWarpState::Error),
				}
			},
			FastWarpState::TrieSync(ref mut trie_dl) => {
				let action = trie_dl.process(peer_id, r, &mut db, &mut self.state_root)?;

				match action {
					FastWarpAction::NextStep => {
						let block_number = io.chain().chain_info().best_block_number;
						let block_hash = io.chain().chain_info().best_block_hash;
						FastWarp::finalize(&mut db, block_number, block_hash);
						Some(FastWarpState::Done)
					},
					FastWarpAction::Continue => None,
					FastWarpAction::Error => Some(FastWarpState::Error),
				}
			},
			FastWarpState::Error => {
				trace!(target: "fast-warp", "Invalid packet with state: Error");
				None
			},
			FastWarpState::Done => {
				trace!(target: "fast-warp", "Invalid packet with state: Done");
				None
			},
			FastWarpState::Idle => {
				trace!(target: "fast-warp", "Invalid packet with state: Idle");
				None
			},
		};

		if let Some(state) = next_state {
			self.state = state;
		}

		Ok(())
	}

	/// Request to the given Peer
	pub fn request(&mut self, io: &mut SyncIo, peer_id: PeerId, highest_block: Option<BlockNumber>) -> Option<FastWarpRequest> {
		match self.state {
			FastWarpState::Idle => {
				// Try setting the starting block number, if not set yet
				if self.start_block_number.is_none() {
					match highest_block {
						Some(highest_block_number) if highest_block_number >= NUM_BLOCKS_HEADERS => {
							self.start_block_number = Some(highest_block_number - NUM_BLOCKS_HEADERS);
						},
						_ => (),
					}
				}

				let start_bn = match self.start_block_number {
					Some(bn) => bn,
					None => return None,
				};

				let parent_start_bn = start_bn - 1;
				if !self.total_difficulties.contains_key(&parent_start_bn) {
					return Some(FastWarpRequest::TotalDifficulty(parent_start_bn));
				}

				// Set the start block_number if highest block known
				let start_bh = match io.chain_overlay().read().get(&start_bn) {
					None => {
						return Some(FastWarpRequest::BlockHeader(start_bn));
					},
					Some(bytes) => {
						SyncHeader::from_rlp(bytes.to_vec()).unwrap().header.hash()
					},
				};

				trace!(target: "fast-warp", "Starting block downloads at {}", start_bn);
				let block_dl = BlockDownloader::new(BlockSet::FastWarpBlocks, &start_bh, start_bn, true);
				self.state = FastWarpState::BlockSync(block_dl);
				return self.request(io, peer_id, highest_block);
			},
			FastWarpState::BlockSync(_) => {
				// let latest_bn = {
				// 	let block_dl = match self.state {
				// 		FastWarpState::BlockSync(ref block_dl) => Some(block_dl),
				// 		_ => None,
				// 	}.unwrap();
				// 	block_dl.last_imported_block_number()
				// };
				// let latest_bn = io.chain().chain_info().ancient_block_number.unwrap_or(0);
				let latest_bn = io.chain().chain_info().best_block_number;
				let queue_info = io.chain().queue_info();
				let bn_delta = highest_block.map(|bn| {
					if bn > latest_bn {
						bn - latest_bn
					} else {
						0
					}
				}).unwrap_or(1_000_000);

				info!(target: "fast-warp",
					"Blocks-delta: {} ; Latest-block: {} ; Queue: {:?}",
					bn_delta, latest_bn, queue_info,
				);
				if bn_delta <= BLOCKS_DELTA_START_SYNC {
					info!(target: "fast-warp", "Less than {} blocks from tip. Syncing state.", BLOCKS_DELTA_START_SYNC);
					self.state = FastWarpState::StateSync(StateDownloader::new());
					self.request(io, peer_id, highest_block)
				} else {
					let block_dl = match self.state {
						FastWarpState::BlockSync(ref mut block_dl) => Some(block_dl),
						_ => None,
					}.unwrap();
					block_dl.request_blocks(io, 0).map(|req| FastWarpRequest::BlockSync(req))
				}
			},
			FastWarpState::StateSync(ref mut state_dl) => {
				Some(state_dl.request(peer_id))
			},
			FastWarpState::TrieSync(ref mut trie_dl) => {
				Some(trie_dl.request(peer_id))
			},
			FastWarpState::Done | FastWarpState::Error => {
				trace!(target: "fast-warp", "Invalid state for request.");
				None
			},
		}
	}

	pub fn set_total_difficulty(&mut self, block_number: BlockNumber, total_diff: U256) {
		self.total_difficulties.insert(block_number, total_diff);
		trace!(target: "fast-warp", "Set total difficulty for block #{}", block_number);
	}

	/// Commit changes to disk
	pub fn commit(db: &mut Box<JournalDB>) {
		let backing = db.backing().clone();
		let mut batch = backing.transaction();
		db.inject(&mut batch).unwrap();
		backing.write_buffered(batch);
		db.flush();
	}

	/// Finalize the restoration
	pub fn finalize(db: &mut Box<JournalDB>, era: u64, id: H256) {
		let mut batch = db.backing().transaction();
		db.journal_under(&mut batch, era, &id).unwrap();
		db.backing().write_buffered(batch);
	}
}
// Copyright 2016 The Grin Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Implementation of the chain block acceptance (or refusal) pipeline.

use std::sync::{Arc, Mutex};

use secp;
use time;

use core::consensus;
use core::core::hash::{Hash, Hashed};
use core::core::target::Difficulty;
use core::core::{BlockHeader, Block, Proof};
use core::pow;
use types;
use types::{Tip, ChainStore, ChainAdapter, NoopAdapter};
use store;

bitflags! {
  /// Options for block validation
  pub flags Options: u32 {
    const NONE = 0b00000001,
    /// Runs with the easier version of the Proof of Work, mostly to make testing easier.
    const EASY_POW = 0b00000010,
  }
}

/// Contextual information required to process a new block and either reject or
/// accept it.
pub struct BlockContext {
	opts: Options,
	store: Arc<ChainStore>,
	adapter: Arc<ChainAdapter>,
	head: Tip,
	tip: Option<Tip>,
}

#[derive(Debug)]
pub enum Error {
	/// The block doesn't fit anywhere in our chain
	Unfit(String),
	/// Difficulty is too low either compared to ours or the block PoW hash
	DifficultyTooLow,
	/// Addition of difficulties on all previous block is wrong
	WrongTotalDifficulty,
	/// Size of the Cuckoo graph in block header doesn't match PoW requirements
	WrongCuckooSize,
	/// The proof of work is invalid
	InvalidPow,
	/// The block doesn't sum correctly or a tx signature is invalid
	InvalidBlockProof(secp::Error),
	/// Block time is too old
	InvalidBlockTime,
	/// Internal issue when trying to save or load data from store
	StoreErr(types::Error),
}

/// Runs the block processing pipeline, including validation and finding a
/// place for the new block in the chain. Returns the new
/// chain head if updated.
pub fn process_block(b: &Block,
                     store: Arc<ChainStore>,
                     adapter: Arc<ChainAdapter>,
                     opts: Options)
                     -> Result<Option<Tip>, Error> {
	// TODO should just take a promise for a block with a full header so we don't
	// spend resources reading the full block when its header is invalid

	let head = try!(store.head().map_err(&Error::StoreErr));

	let mut ctx = BlockContext {
		opts: opts,
		store: store,
		adapter: adapter,
		head: head,
		tip: None,
	};

	info!("Starting validation pipeline for block {} at {}.",
	      b.hash(),
	      b.header.height);
	try!(check_known(b.hash(), &mut ctx));
	try!(validate_header(&b, &mut ctx));
	try!(set_tip(&b.header, &mut ctx));
	try!(validate_block(b, &mut ctx));
	info!("Block at {} with hash {} is valid, going to save and append.",
	      b.header.height,
	      b.hash());
	try!(add_block(b, &mut ctx));
	// TODO a global lock should be set before that step or even earlier
	try!(update_tips(&mut ctx));

	// TODO make sure we always return the head, and not a fork that just got longer
	Ok(ctx.tip)
}

/// Quick in-memory check to fast-reject any block we've already handled
/// recently. Keeps duplicates from the network in check.
fn check_known(bh: Hash, ctx: &mut BlockContext) -> Result<(), Error> {
	if bh == ctx.head.last_block_h || bh == ctx.head.prev_block_h {
		return Err(Error::Unfit("already known".to_string()));
	}
	Ok(())
}

/// First level of black validation that only needs to act on the block header
/// to make it as cheap as possible. The different validations are also
/// arranged by order of cost to have as little DoS surface as possible.
/// TODO require only the block header (with length information)
fn validate_header(b: &Block, ctx: &mut BlockContext) -> Result<(), Error> {
	let header = &b.header;
	if header.height > ctx.head.height + 1 {
		// TODO actually handle orphans and add them to a size-limited set
		return Err(Error::Unfit("orphan".to_string()));
	}

	let prev = try!(ctx.store.get_block_header(&header.previous).map_err(&Error::StoreErr));

	if header.timestamp <= prev.timestamp {
		// prevent time warp attacks and some timestamp manipulations by forcing strict
		// time progression
		return Err(Error::InvalidBlockTime);
	}
	if header.timestamp >
	   time::now() + time::Duration::seconds(12 * (consensus::BLOCK_TIME_SEC as i64)) {
		// refuse blocks more than 12 blocks intervals in future (as in bitcoin)
		// TODO add warning in p2p code if local time is too different from peers
		return Err(Error::InvalidBlockTime);
	}

	if b.header.total_difficulty !=
	   prev.total_difficulty.clone() + Difficulty::from_hash(&prev.hash()) {
		return Err(Error::WrongTotalDifficulty);
	}

	// verify the proof of work and related parameters
	let (difficulty, cuckoo_sz) = consensus::next_target(header.timestamp.to_timespec().sec,
	                                                     prev.timestamp.to_timespec().sec,
	                                                     prev.difficulty,
	                                                     prev.cuckoo_len);
	if header.difficulty < difficulty {
		return Err(Error::DifficultyTooLow);
	}
	if header.cuckoo_len != cuckoo_sz && !ctx.opts.intersects(EASY_POW) {
		return Err(Error::WrongCuckooSize);
	}

	if ctx.opts.intersects(EASY_POW) {
		if !pow::verify_size(b, 16) {
			return Err(Error::InvalidPow);
		}
	} else if !pow::verify(b) {
		return Err(Error::InvalidPow);
	}

	Ok(())
}

fn set_tip(h: &BlockHeader, ctx: &mut BlockContext) -> Result<(), Error> {
	// TODO actually support more than one branch
	if h.previous != ctx.head.last_block_h {
		return Err(Error::Unfit("Just don't know where to put it right now".to_string()));
	}
	// TODO validate block header height
	ctx.tip = Some(ctx.head.clone());
	Ok(())
}

fn validate_block(b: &Block, ctx: &mut BlockContext) -> Result<(), Error> {
	// TODO check tx merkle tree
	let curve = secp::Secp256k1::with_caps(secp::ContextFlag::Commit);
	try!(b.verify(&curve).map_err(&Error::InvalidBlockProof));
	Ok(())
}

fn add_block(b: &Block, ctx: &mut BlockContext) -> Result<(), Error> {
	// save the block and appends it to the selected tip
	ctx.tip = ctx.tip.as_ref().map(|t| t.append(b.hash()));
	ctx.store.save_block(b).map_err(&Error::StoreErr);

	// broadcast the block
	let adapter = ctx.adapter.clone();
	adapter.block_accepted(b);
	Ok(())
}

fn update_tips(ctx: &mut BlockContext) -> Result<(), Error> {
	let tip = ctx.tip.as_ref().unwrap();
	ctx.store.save_head(tip).map_err(&Error::StoreErr)
}

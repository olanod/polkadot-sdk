// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! API implementation for submitting transactions.

use crate::{
	transaction::{
		api::TransactionApiServer,
		error::Error,
		event::{
			TransactionBlock, TransactionBroadcasted, TransactionDropped, TransactionError,
			TransactionEvent,
		},
	},
	SubscriptionTaskExecutor,
};
use jsonrpsee::{
	types::{
		error::{CallError, ErrorObject},
		SubscriptionResult,
	},
	SubscriptionSink,
};
use sc_transaction_pool_api::{
	error::IntoPoolError, BlockHash, TransactionFor, TransactionPool, TransactionSource,
	TransactionStatus,
};
use std::sync::Arc;

use sp_api::ProvideRuntimeApi;
use sp_blockchain::HeaderBackend;
use sp_core::Bytes;
use sp_runtime::traits::Block as BlockT;

use codec::Decode;
use futures::{FutureExt, StreamExt, TryFutureExt};

/// An API for transaction RPC calls.
pub struct Transaction<Pool, Client> {
	/// Substrate client.
	client: Arc<Client>,
	/// Transactions pool.
	pool: Arc<Pool>,
	/// Executor to spawn subscriptions.
	executor: SubscriptionTaskExecutor,
}

impl<Pool, Client> Transaction<Pool, Client> {
	/// Creates a new [`Transaction`].
	pub fn new(client: Arc<Client>, pool: Arc<Pool>, executor: SubscriptionTaskExecutor) -> Self {
		Transaction { client, pool, executor }
	}
}

/// Currently we treat all RPC transactions as externals.
///
/// Possibly in the future we could allow opt-in for special treatment
/// of such transactions, so that the block authors can inject
/// some unique transactions via RPC and have them included in the pool.
const TX_SOURCE: TransactionSource = TransactionSource::External;

/// Extrinsic has an invalid format.
///
/// # Note
///
/// This is similar to the old `author` API error code.
const BAD_FORMAT: i32 = 1001;

impl<Pool, Client> TransactionApiServer<BlockHash<Pool>> for Transaction<Pool, Client>
where
	Pool: TransactionPool + Sync + Send + 'static,
	Pool::Hash: Unpin,
	<Pool::Block as BlockT>::Hash: Unpin,
	Client: HeaderBackend<Pool::Block> + ProvideRuntimeApi<Pool::Block> + Send + Sync + 'static,
{
	fn submit_and_watch(&self, mut sink: SubscriptionSink, xt: Bytes) -> SubscriptionResult {
		// This is the only place where the RPC server can return an error for this
		// subscription. Other defects must be signaled as events to the sink.
		let decoded_extrinsic = match TransactionFor::<Pool>::decode(&mut &xt[..]) {
			Ok(decoded_extrinsic) => decoded_extrinsic,
			Err(e) => {
				let err = CallError::Custom(ErrorObject::owned(
					BAD_FORMAT,
					format!("Extrinsic has invalid format: {}", e),
					None::<()>,
				));
				let _ = sink.reject(err);
				return Ok(())
			},
		};

		let best_block_hash = self.client.info().best_hash;

		let submit = self
			.pool
			.submit_and_watch(best_block_hash, TX_SOURCE, decoded_extrinsic)
			.map_err(|e| {
				e.into_pool_error()
					.map(Error::from)
					.unwrap_or_else(|e| Error::Verification(Box::new(e)))
			});

		let fut = async move {
			match submit.await {
				Ok(stream) => {
					let mut state = TransactionState::new();
					let stream =
						stream.filter_map(|event| async move { state.handle_event(event) });
					sink.pipe_from_stream(stream.boxed()).await;
				},
				Err(err) => {
					// We have not created an `Watcher` for the tx. Make sure the
					// error is still propagated as an event.
					let event: TransactionEvent<<Pool::Block as BlockT>::Hash> = err.into();
					sink.pipe_from_stream(futures::stream::once(async { event }).boxed()).await;
				},
			};
		};

		self.executor.spawn("substrate-rpc-subscription", Some("rpc"), fut.boxed());
		Ok(())
	}
}

/// The transaction's state that needs to be preserved between
/// multiple events generated by the transaction-pool.
///
/// # Note
///
/// In the future, the RPC server can submit only the last event when multiple
/// identical events happen in a row.
#[derive(Clone, Copy)]
struct TransactionState {
	/// True if the transaction was previously broadcasted.
	broadcasted: bool,
}

impl TransactionState {
	/// Construct a new [`TransactionState`].
	pub fn new() -> Self {
		TransactionState { broadcasted: false }
	}

	/// Handle events generated by the transaction-pool and convert them
	/// to the new API expected state.
	#[inline]
	pub fn handle_event<Hash: Clone, BlockHash: Clone>(
		&mut self,
		event: TransactionStatus<Hash, BlockHash>,
	) -> Option<TransactionEvent<BlockHash>> {
		match event {
			TransactionStatus::Ready | TransactionStatus::Future =>
				Some(TransactionEvent::<BlockHash>::Validated),
			TransactionStatus::Broadcast(peers) => {
				// Set the broadcasted flag once if we submitted the transaction to
				// at least one peer.
				self.broadcasted = self.broadcasted || !peers.is_empty();

				Some(TransactionEvent::Broadcasted(TransactionBroadcasted {
					num_peers: peers.len(),
				}))
			},
			TransactionStatus::InBlock((hash, index)) =>
				Some(TransactionEvent::BestChainBlockIncluded(Some(TransactionBlock {
					hash,
					index,
				}))),
			TransactionStatus::Retracted(_) => Some(TransactionEvent::BestChainBlockIncluded(None)),
			TransactionStatus::FinalityTimeout(_) =>
				Some(TransactionEvent::Dropped(TransactionDropped {
					broadcasted: self.broadcasted,
					error: "Maximum number of finality watchers has been reached".into(),
				})),
			TransactionStatus::Finalized((hash, index)) =>
				Some(TransactionEvent::Finalized(TransactionBlock { hash, index })),
			TransactionStatus::Usurped(_) => Some(TransactionEvent::Invalid(TransactionError {
				error: "Extrinsic was rendered invalid by another extrinsic".into(),
			})),
			TransactionStatus::Dropped => Some(TransactionEvent::Invalid(TransactionError {
				error: "Extrinsic dropped from the pool due to exceeding limits".into(),
			})),
			TransactionStatus::Invalid => Some(TransactionEvent::Invalid(TransactionError {
				error: "Extrinsic marked as invalid".into(),
			})),
		}
	}
}

// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! The top-level routing/network map tracking logic lives here.
//!
//! You probably want to create a NetGraphMsgHandler and use that as your RoutingMessageHandler and then
//! interrogate it to get routes for your own payments.

use bitcoin::secp256k1::key::PublicKey;

use ln::channelmanager::ChannelDetails;
use ln::features::{ChannelFeatures, NodeFeatures};
use ln::msgs::{DecodeError, ErrorAction, LightningError, MAX_VALUE_MSAT};
use routing::network_graph::{NetworkGraph, RoutingFees, DirectionalChannelInfo, NodeInfo};
use util::ser::{Writeable, Readable};
use util::logger::Logger;

use std::cmp;
use std::collections::{HashMap, BinaryHeap};
use std::ops::Deref;

/// A hop in a route
#[derive(Clone)]
pub struct RouteHop {
	/// The node_id of the node at this hop.
	pub pubkey: PublicKey,
	/// The node_announcement features of the node at this hop. For the last hop, these may be
	/// amended to match the features present in the invoice this node generated.
	pub node_features: NodeFeatures,
	/// The channel that should be used from the previous hop to reach this node.
	pub short_channel_id: u64,
	/// The channel_announcement features of the channel that should be used from the previous hop
	/// to reach this node.
	pub channel_features: ChannelFeatures,
	/// The fee taken on this hop (for paying for the use of the *next* channel in the path).
	/// For the last hop, this should be the full value of the payment.
	pub fee_msat: u64,
	/// The CLTV delta added for this hop. For the last hop, this should be the full CLTV value
	/// expected at the destination, in excess of the current block height.
	pub cltv_expiry_delta: u32,
}

impl PartialEq for RouteHop {
    fn eq(&self, other: &Self) -> bool {
        self.short_channel_id == other.short_channel_id && self.pubkey == other.pubkey
    }
}


impl Writeable for Vec<RouteHop> {
	fn write<W: ::util::ser::Writer>(&self, writer: &mut W) -> Result<(), ::std::io::Error> {
		(self.len() as u8).write(writer)?;
		for hop in self.iter() {
			hop.pubkey.write(writer)?;
			hop.node_features.write(writer)?;
			hop.short_channel_id.write(writer)?;
			hop.channel_features.write(writer)?;
			hop.fee_msat.write(writer)?;
			hop.cltv_expiry_delta.write(writer)?;
		}
		Ok(())
	}
}

impl Readable for Vec<RouteHop> {
	fn read<R: ::std::io::Read>(reader: &mut R) -> Result<Vec<RouteHop>, DecodeError> {
		let hops_count: u8 = Readable::read(reader)?;
		let mut hops = Vec::with_capacity(hops_count as usize);
		for _ in 0..hops_count {
			hops.push(RouteHop {
				pubkey: Readable::read(reader)?,
				node_features: Readable::read(reader)?,
				short_channel_id: Readable::read(reader)?,
				channel_features: Readable::read(reader)?,
				fee_msat: Readable::read(reader)?,
				cltv_expiry_delta: Readable::read(reader)?,
			});
		}
		Ok(hops)
	}
}

/// A route directs a payment from the sender (us) to the recipient. If the recipient supports MPP,
/// it can take multiple paths. Each path is composed of one or more hops through the network.
#[derive(Clone, PartialEq)]
pub struct Route {
	/// The list of routes taken for a single (potentially-)multi-part payment. The pubkey of the
	/// last RouteHop in each path must be the same.
	/// Each entry represents a list of hops, NOT INCLUDING our own, where the last hop is the
	/// destination. Thus, this must always be at least length one. While the maximum length of any
	/// given path is variable, keeping the length of any path to less than 20 should currently
	/// ensure it is viable.
	pub paths: Vec<Vec<RouteHop>>,
}

impl Writeable for Route {
	fn write<W: ::util::ser::Writer>(&self, writer: &mut W) -> Result<(), ::std::io::Error> {
		(self.paths.len() as u64).write(writer)?;
		for hops in self.paths.iter() {
			hops.write(writer)?;
		}
		Ok(())
	}
}

impl Readable for Route {
	fn read<R: ::std::io::Read>(reader: &mut R) -> Result<Route, DecodeError> {
		let path_count: u64 = Readable::read(reader)?;
		let mut paths = Vec::with_capacity(cmp::min(path_count, 128) as usize);
		for _ in 0..path_count {
			paths.push(Readable::read(reader)?);
		}
		Ok(Route { paths })
	}
}

/// A channel descriptor which provides a last-hop route to get_route
pub struct RouteHint {
	/// The node_id of the non-target end of the route
	pub src_node_id: PublicKey,
	/// The short_channel_id of this channel
	pub short_channel_id: u64,
	/// The fees which must be paid to use this channel
	pub fees: RoutingFees,
	/// The difference in CLTV values between this node and the next node.
	pub cltv_expiry_delta: u16,
	/// The minimum value, in msat, which must be relayed to the next hop.
	pub htlc_minimum_msat: u64,
	/// The maximum value in msat available for routing with a single HTLC.
	pub htlc_maximum_msat: Option<u64>,
}

#[derive(Eq, PartialEq, Clone)]
struct RouteGraphNode {
	pubkey: PublicKey,
	lowest_fee_to_peer_through_node: u64,
	lowest_fee_to_node: u64,
}

impl cmp::Ord for RouteGraphNode {
	fn cmp(&self, other: &RouteGraphNode) -> cmp::Ordering {
		other.lowest_fee_to_peer_through_node.cmp(&self.lowest_fee_to_peer_through_node)
			.then_with(|| other.pubkey.serialize().cmp(&self.pubkey.serialize()))
	}
}

impl cmp::PartialOrd for RouteGraphNode {
	fn partial_cmp(&self, other: &RouteGraphNode) -> Option<cmp::Ordering> {
		Some(self.cmp(other))
	}
}

// It's useful to keep track of the hops associated with the fees required to use them,
// so that we can choose cheaper paths (as per Dijkstra's algorithm).
/// Fee values should be updated only in the context of the whole path, see update_value_and_recompute_fees.
/// These fee values are useful to choose hops as we traverse the graph "payee-to-payer".
#[derive(Clone)]
struct PaymentHop {
	route_hop: RouteHop,
	/// Liquidity available considering the following limitations:
	/// - UTXO capacity
	/// - htlc_maximum_msat (direction-dependent)
	/// - use of the same channel by other paths per MPP (including fees paid there)
	/// It does not take into account fees to be paid on the current hop, so
	/// this amount *should* cover transferring a value AND paying fees (on this channel).
	available_liquidity_msat: u64,
	/// Minimal fees required to route to the previous hop via any of its inbound channels.
	src_lowest_inbound_fees: RoutingFees,
	/// Fees of the channel used in this hop.
	channel_fees: RoutingFees,
	/// All the fees paid *after* this channel on the way to the destination
	following_hops_fees_msat: u64,
	/// Fee paid for the use of the current channel (see channel_fees).
	/// The value will be actually paid on the previous hop.
	hop_use_fee_msat: u64,
	/// Fee required to reach the source node of the current channel (estimate, see src_lowest_inbound_fees)
	prev_hop_use_estimate_fee_msat: u64,
}

impl PaymentHop {
	/// How attractive this channel is in terms of the paid fees.
	fn get_fee_weight_msat(&self) -> u64 {
		let at_current_hop_fee_msat = self.hop_use_fee_msat.checked_add(self.prev_hop_use_estimate_fee_msat);
		if let Some(fee_msat) = at_current_hop_fee_msat {
			if let Some(total_fee_msat) = fee_msat.checked_add(self.following_hops_fees_msat) {
				return total_fee_msat;
			}
		}
		return u64::max_value();
	}

	/// Should be called only after the paid fees (fee_msat) is propagated to the channel which pays them
	/// (one hop before the hop they are paying for).
	fn get_fee_paid_msat(&self) -> u64 {
		if let Some(fee_paid_msat) = self.following_hops_fees_msat.checked_add(self.route_hop.fee_msat) {
			return fee_paid_msat;
		} else {
			return u64::max_value();
		}
	}
}

// Instantiated with a list of hops with correct data in them collected during path finding,
// an instance of this struct should be further modified only via given methods.
#[derive(Clone)]
struct PaymentPath {
	hops: Vec<PaymentHop>,
}

impl PaymentPath {
	fn get_value_msat(&self) -> u64 {
		return self.hops.last().unwrap().route_hop.fee_msat;
	}

	fn get_total_fee_paid_msat(&self) -> u64 {
		if self.hops.len() < 1 {
			return 0;
		} else {
			return self.hops.first().unwrap().following_hops_fees_msat;
		}
	}

	// If an amount transferred by the path is updated, the fees should be adjusted.
	// Any other way to change fees may result in an inconsistency.
	fn update_value_and_recompute_fees(&mut self, value_msat: u64) {
		let mut total_fee_paid_msat = 0 as u64;
		for i in (1..self.hops.len()).rev() {
			let cur_hop_amount_msat = total_fee_paid_msat + value_msat;
			let mut cur_hop = self.hops.get_mut(i).unwrap();
			cur_hop.following_hops_fees_msat = total_fee_paid_msat;
			let new_fee = compute_fees(cur_hop_amount_msat, cur_hop.channel_fees);
			cur_hop.hop_use_fee_msat = new_fee;
			total_fee_paid_msat += new_fee;
		}

		// Propagate updated fees for the use of the channels to one hop back,
		// where they will be actually paid (fee_msat).
		// For the last hop it will represent the value being transferred over this path.
		for i in 0..self.hops.len() - 1 {
			let next_hop_use_fee_msat = self.hops.get(i + 1).unwrap().hop_use_fee_msat;
			self.hops.get_mut(i).unwrap().route_hop.fee_msat = next_hop_use_fee_msat;
		}
		self.hops.last_mut().unwrap().route_hop.fee_msat = value_msat;
	}
}

fn compute_fees(amount_msat: u64, channel_fees: RoutingFees) -> u64 {
	let proportional_fee_millions = amount_msat.checked_mul(channel_fees.proportional_millionths as u64);
	if let Some(new_fee) = proportional_fee_millions.and_then(|part| {
			(channel_fees.base_msat as u64).checked_add(part / 1_000_000) }) {

		return new_fee;
	} else {
		unreachable!();
	}
}

/// Placeholder for routing state during a collection of payment paths construction session.
struct RoutingState {
	targeted_edges: BinaryHeap<RouteGraphNode>,
	weighted_vertices: HashMap<PublicKey, PaymentHop>,
	payer_node_id: PublicKey,
	/// We don't want multiple paths (as per MPP) share liquidity of the same channels.
	///
	/// This map allows paths to be aware of the channel use by other paths in the same call.
	/// This would help to make a better path finding decisions and not "overbook" channels.
	/// It is currently unaware of the directions. But if we moved 1 BTC in one direction and
	/// 1 BTC in the opposite direction, should they cancel out? Probably not, because
	/// in the worst-case order of HTLC forwarding, channel liquidity can be overflown.
	/// TODO: we could let a caller specify this. Definitely useful when considering our own channels.
	bookkeeped_channels_liquidity_available_msat: HashMap<u64, u64>,
	recommended_value_msat: u64,
	/// Keeping track of how much value we already collected across other paths. Helps to decide:
	/// - how much a new path should be transferring (upper bound);
	/// - whether a channel should be disregarded because it's available liquidity is too small comparing
	///   to how much more we need to collect;
	/// - when we want to stop looking for new paths.
	already_collected_value_msat: u64
}

impl RoutingState {
	fn new(graph_size: usize, payer_node_id: PublicKey, recommended_value_msat: u64) -> Self {
		RoutingState {
			targeted_edges: BinaryHeap::new(), //TODO: Do we care about switching to eg Fibbonaci heap?
			weighted_vertices: HashMap::with_capacity(graph_size),
			payer_node_id,
			bookkeeped_channels_liquidity_available_msat: HashMap::new(),
			recommended_value_msat,
			already_collected_value_msat: 0,
		}
	}

	/// Adds weighted vertice as identified by scid which goes from source node to destination
	/// node with fees described in channel details.
	///
	/// `following_hops_fees_msat` represents the fees paid for using all the channel *after*
	/// this one since that value has to be transferred over this channel.
	/// TODO: direction of *after*
	fn add_vertice(&mut self, scid: u64, src_node_id: &PublicKey, dest_node_id: &PublicKey, directional_info: &DirectionalChannelInfo, capacity_sats: Option<u64>, features: ChannelFeatures, following_hops_fees_msat: u64, network: &NetworkGraph) {

		// Assign a liquidity to the channel either from bookkeeped previous routing usage
		// or from known channel relay policy's `htlc_maximum_msat`.
		let available_liquidity_msat = self.bookkeeped_channels_liquidity_available_msat.entry(scid.clone()).or_insert_with(|| {
			let mut initial_liquidity_available_msat = None;
			if let Some(capacity_sats) = capacity_sats {
				initial_liquidity_available_msat = Some(capacity_sats * 1000);
			}

			if let Some(htlc_maximum_msat) = directional_info.htlc_maximum_msat {
				if let Some(available_msat) = initial_liquidity_available_msat {
					initial_liquidity_available_msat = Some(cmp::min(available_msat, htlc_maximum_msat));
				} else {
					initial_liquidity_available_msat = Some(htlc_maximum_msat);
				}
			}

			match initial_liquidity_available_msat {
				Some(available_msat) => available_msat,
				// We assume channels with unknown balance have a capacity of 0.0001 BTC (or 10_000 sats).
				None => 10_000 * 1000
			}
		});

		// Routing Fragmentation Mitigation heuristic:
		//
		// Routing fragmentation across many payment paths increases the overall routing
		// fees as you have irreducible routing fees per-link used (`fee_base_msat`).
		// Taking too many paths also smaller paths also increases the chance of payment failure.
		// Thus to avoid this effect, we require from our collected links to provide
		// at least a minimal liquidity contribution to the recommended value yet-to-be-fulfilled.
		//
		// This requirement is currently 5% of the already-collected value. This means as
		// we successfully advance in our collection, the absolute liquidity contribution is lowered,
		// thus increasing the number of potential channels to be selected.

		// Update the absolute liquidity left to collect from previously built paths.
		let value_left_to_collect_msat = self.recommended_value_msat - self.already_collected_value_msat;
		// Derive the minimal liquidity contribution with a ratio of 20 (5%).
		let minimal_liquidity_contribution_msat: u64 = value_left_to_collect_msat / 20;
		// Verify the liquidity offered by this channel complies to the minimal contribution.
		let has_sufficient_liquidity = *available_liquidity_msat >= minimal_liquidity_contribution_msat;

		// It is tricky to compare available liquidity to $following_hops_fees_msat here
		// to see if this channel is capable of paying for the use of the following channels.
		// It may be misleading because we might later choose to reduce the value transferred
		// over these channels, and the channel which was insufficient might become sufficient.
		// Worst case: we drop a good channel here because it can't cover the high following fees
		// caused by one expensive channel, but then this channel could have been used if the amount being
		// transferred over this path is lower.
		// We do this for now, but this check is a subject for removal.
		let can_cover_following_hops = *available_liquidity_msat > following_hops_fees_msat;

		// Includes paying fees for the use of the following channels.
		let amount_to_transfer_over_msat: u64 = cmp::min(*available_liquidity_msat, value_left_to_collect_msat);

		//TODO: Explore simply adding fee to hit htlc_minimum_msat
		if has_sufficient_liquidity && can_cover_following_hops && amount_to_transfer_over_msat >= directional_info.htlc_minimum_msat {
			let hm_entry = self.weighted_vertices.entry(*src_node_id);
			let old_entry = hm_entry.or_insert_with(|| {
				// If there was previously no known way to access the source node (recall it goes payee-to-payer) of `scid`,
				// first add a semi-dummy record just to compute the fees to reach the source node.
				// This will affect our decision on selecting `scid` as a way to reach the `dest_node_id`.
				let node = network.get_nodes().get(&src_node_id).unwrap();
				let mut fee_base_msat = u32::max_value();
				let mut fee_proportional_millionths = u32::max_value();
				if let Some(fees) = node.lowest_inbound_channel_fees {
					fee_base_msat = fees.base_msat;
					fee_proportional_millionths = fees.proportional_millionths;
				};
				PaymentHop {
					route_hop: RouteHop {
						pubkey: dest_node_id.clone(),
						node_features: NodeFeatures::empty(),
						short_channel_id: 0,
						channel_features: features.clone(),
						fee_msat: 0,
						cltv_expiry_delta: 0,
					},
					available_liquidity_msat: 0,
					src_lowest_inbound_fees: RoutingFees {
						base_msat: fee_base_msat,
						proportional_millionths: fee_proportional_millionths,
					},
					channel_fees: directional_info.fees,
					following_hops_fees_msat: u64::max_value(),
					hop_use_fee_msat: u64::max_value(),
					prev_hop_use_estimate_fee_msat: u64::max_value(),
				}
			});

			let hop_use_fee_msat = compute_fees(amount_to_transfer_over_msat, directional_info.fees);
			let mut prev_hop_use_estimate_fee_msat = 0;
			let mut total_fee_msat = following_hops_fees_msat;
			if *src_node_id != self.payer_node_id {
				// Ignore hop_use_fee_msat for channel-from-us as we assume all channels-from-us
				// will have the same effective-fee
				total_fee_msat += hop_use_fee_msat;
				prev_hop_use_estimate_fee_msat = compute_fees(total_fee_msat + amount_to_transfer_over_msat, old_entry.src_lowest_inbound_fees);
				total_fee_msat += prev_hop_use_estimate_fee_msat;
			}

			let new_graph_node = RouteGraphNode {
				pubkey: *src_node_id,
				lowest_fee_to_peer_through_node: total_fee_msat,
				lowest_fee_to_node: following_hops_fees_msat as u64 + hop_use_fee_msat,
			};
			// Update the way of reaching `dest_node_id` with the given `scid`, if this way is cheaper
			// than the already known (considering the cost to "reach" this channel from the route destination,
			// the cost of using this channel, and the cost of routing to the source node of this channel).
			if old_entry.get_fee_weight_msat() > total_fee_msat {
				self.targeted_edges.push(new_graph_node);
				old_entry.following_hops_fees_msat = following_hops_fees_msat;
				old_entry.hop_use_fee_msat = hop_use_fee_msat;
				old_entry.prev_hop_use_estimate_fee_msat = prev_hop_use_estimate_fee_msat;
				old_entry.route_hop = RouteHop {
					pubkey: dest_node_id.clone(),
					node_features: NodeFeatures::empty(),
					short_channel_id: scid.clone(),
					channel_features: features.clone(),
					fee_msat: 0, // This value will be later filled with hop_use_fee_msat of the following channel
					cltv_expiry_delta: directional_info.cltv_expiry_delta as u32,
				};
				old_entry.available_liquidity_msat = available_liquidity_msat.clone();
				old_entry.channel_fees = directional_info.fees;
			}
		}
	}

	/// Find ways (channels with destimation) to reach a given node and store them
	/// in the corresponding data structures (routing graph etc).
	///
	/// `fee_to_target_msat` represents how much it costs to reach to this node from the payee,
	/// or, in other words, how much will be paid in fees after this node (to the best of our knowledge).
	/// This data can later be helpful to optimize routing (pay lower fees).
	fn select_weighted_vertice_to_target_edge(&mut self, node: &NodeInfo, node_id: &PublicKey, fee_to_target_msat: u64, first_hops: Option<&[&ChannelDetails]>, network: &NetworkGraph) {

		let features;
		if let Some(node_info) = node.announcement_info.as_ref() {
			features = node_info.features.clone();
		} else {
			features = NodeFeatures::empty();
		}

		if !features.requires_unknown_bits() {
			for chan_id in node.channels.iter() {
				let chan = network.get_channels().get(chan_id).unwrap();
				if !chan.features.requires_unknown_bits() {
					if chan.node_one == *node_id {
						// ie `node` is one, ie next hop in A* is two, via the two_to_one channel
						if first_hops.is_none() || chan.node_two != self.payer_node_id {
							if let Some(two_to_one) = chan.two_to_one.as_ref() {
								if two_to_one.enabled {
									self.add_vertice(*chan_id, &chan.node_two, &chan.node_one, two_to_one, chan.capacity_sats, chan.features.clone(), fee_to_target_msat, network);
								}
							}
						}
					} else {
						if first_hops.is_none() || chan.node_one != self.payer_node_id {
							if let Some(one_to_two) = chan.one_to_two.as_ref() {
								if one_to_two.enabled {
									self.add_vertice(*chan_id, &chan.node_one, &chan.node_two, one_to_two, chan.capacity_sats, chan.features.clone(), fee_to_target_msat, network);
								}
							}
						}
					}
				}
			}
		}
	}
}

/// Gets a route from us (payer) to the given target node (payee).
///
/// Extra routing hops between known nodes and the target will be used if they are included in
/// last_hops.
///
/// If some channels aren't announced, it may be useful to fill in a first_hops with the
/// results from a local ChannelManager::list_usable_channels() call. If it is filled in, our
/// view of our local channels (from net_graph_msg_handler) will be ignored, and only those in first_hops
/// will be used.
///
/// Panics if first_hops contains channels without short_channel_ids
/// (ChannelManager::list_usable_channels will never include such channels).
///
/// The fees on channels from us to next-hops are ignored (as they are assumed to all be
/// equal), however the enabled/disabled bit on such channels as well as the htlc_minimum_msat
/// *is* checked as they may change based on the receiving node.
pub fn get_route<L: Deref>(our_node_id: &PublicKey, network: &NetworkGraph, payee: &PublicKey, first_hops: Option<&[&ChannelDetails]>,
	last_hops: &[&RouteHint], final_value_msat: u64, final_cltv: u32, logger: L) -> Result<Route, LightningError> where L::Target: Logger {
	// TODO: Obviously *only* using total fee cost sucks. We should consider weighting by
	// uptime/success in using a node in the past.
	if *payee == *our_node_id {
		return Err(LightningError{err: "Cannot generate a route to ourselves".to_owned(), action: ErrorAction::IgnoreError});
	}

	if final_value_msat > MAX_VALUE_MSAT {
		return Err(LightningError{err: "Cannot generate a route of more value than all existing satoshis".to_owned(), action: ErrorAction::IgnoreError});
	}

	// The general routing idea is the following:
	// 1. Fill first/last hops communicated by the caller.
	// 2. Attempt to construct a path from payer to payee for transferring any ~sufficient (described later) value.
	//    If succeed, remember which channels were used and how much liquidity they have available,
	//    so that future paths don't rely on the same liquidity.
	// 3. Prooceed to the next step if:
	//    - we hit the recommended target value;
	//    - OR if we could not construct a new path. Any next attempt to construct it will fail too.
	//    Otherwise, repeat step 2.
	// 4. See if we managed to collect paths which aggregately are able to transfer target value
	//    (not recommended value). If yes, proceed. If not, fail routing.
	// 5. Randomly combine paths into routes having enough to fulfill the payment. (TODO: knapsack?)
	// 6. Of all the found paths, select only those with the lowest total fee.
	// 7. The last path in every selected route is likely to be more than we need.
	//    Reduce its value-to-transfer and recompute fees.
	// 8. Choose the best route by the lowest total fee.

	// As for the actual search algorithm,
	// we do a payee-to-payer Dijkstra's sorting by each node's distance from the payee
	// plus the minimum per-HTLC fee to get from it to another node (aka "shitty A*").
	// TODO: There are a few tweaks we could do, including possibly pre-calculating more stuff
	// to use as the A* heuristic beyond just the cost to get one node further than the current
	// one.

	// When arranging a route, we select multiple paths so that we can make a multi-path payment.
	// Don't stop searching for paths when we think they're sufficient to transfer a given value aggregately.
	// Search for higher value, so that we collect many more paths, and then select the best combination among them.
	const ROUTE_CAPACITY_PROVISION_FACTOR: u64 = 4;
	let recommended_value_msat = final_value_msat * ROUTE_CAPACITY_PROVISION_FACTOR as u64;

	let mut routing_state = RoutingState::new(network.get_nodes().len(), *our_node_id, recommended_value_msat);

	// Step (1).
	// Prepare the data we'll use for payee-to-payer search by inserting first hops suggested by the caller as targets.
	// Our search will then attempt to reach them while traversing from the payee node.
	let mut first_hop_targets = HashMap::with_capacity(if first_hops.is_some() { first_hops.as_ref().unwrap().len() } else { 0 });
	if let Some(hops) = first_hops {
		for chan in hops {
			let short_channel_id = chan.short_channel_id.expect("first_hops should be filled in with usable channels, not pending ones");
			if chan.remote_network_id == *payee {
				return Ok(Route {
					paths: vec![vec![RouteHop {
						pubkey: chan.remote_network_id,
						node_features: chan.counterparty_features.to_context(),
						short_channel_id,
						channel_features: chan.counterparty_features.to_context(),
						fee_msat: final_value_msat,
						cltv_expiry_delta: final_cltv,
					}]],
				});
			}
			first_hop_targets.insert(chan.remote_network_id, (short_channel_id, chan.counterparty_features.clone()));
		}
		if first_hop_targets.is_empty() {
			return Err(LightningError{err: "Cannot route when there are no outbound routes away from us".to_owned(), action: ErrorAction::IgnoreError});
		}
	}

	let mut payment_paths = Vec::<PaymentPath>::new();

	// TODO: diversify by nodes (so that all paths aren't doomed if one node is offline).
	'paths_collection: loop {
		// For every new path, start from scratch, except bookkeeped_channels_liquidity_available_msat,
		// which will improve the further iterations of path finding. Also don't erase first_hop_targets.
		routing_state.targeted_edges.clear();
		routing_state.weighted_vertices.clear();

		// Add the payee as a target, so that the payee-to-payer search algorithm knows what to start with.
		match network.get_nodes().get(payee) {
			None => {},
			Some(node) => {
				if first_hops.is_some() {
					if let Some(&(ref first_hop, ref features)) = first_hop_targets.get(&payee) {
						routing_state.add_vertice(*first_hop, our_node_id, payee, &DirectionalChannelInfo::default(), None::<u64>, features.to_context(), 0, network);
					}
				}
				routing_state.select_weighted_vertice_to_target_edge(node, payee, 0, first_hops, network);
			},
		}

		// Step (1).
		// If a caller provided us with last hops, add them to routing targets.
		// Since this happens earlier than general path finding, they will be somewhat prioritized,
		// although currently it matters only if the fees are exactly the same.
		for hop in last_hops.iter() {
			if first_hops.is_none() || hop.src_node_id != *our_node_id { // first_hop overrules last_hops
				if network.get_nodes().get(&hop.src_node_id).is_some() {
					if first_hops.is_some() {
						if let Some(&(ref first_hop, ref features)) = first_hop_targets.get(&hop.src_node_id) {
							// Currently there are no channel-context features defined, so we are a
							// bit lazy here. In the future, we should pull them out via our
							// ChannelManager, but there's no reason to waste the space until we
							// need them.
							routing_state.add_vertice(*first_hop, our_node_id , &hop.src_node_id, &DirectionalChannelInfo::default(), None::<u64>, features.to_context(), 0, network);
						}
					}
					// BOLT 11 doesn't allow inclusion of features for the last hop hints, which
					// really sucks, cause we're gonna need that eventually.

					// Convert a route hint to a directional info
					let from_route_hint = DirectionalChannelInfo {
						last_update: 0,
						enabled: false,
						cltv_expiry_delta: hop.cltv_expiry_delta,
						htlc_minimum_msat: hop.htlc_minimum_msat,
						htlc_maximum_msat: hop.htlc_maximum_msat,
						fees: hop.fees,
						last_update_message: None,
					};
					routing_state.add_vertice(hop.short_channel_id, &hop.src_node_id, payee, &from_route_hint, None::<u64>, ChannelFeatures::empty(), 0, network);
				}
			}
		}

		// At this point, targets are filled with the data from first and last hops communicated by the caller, and the payment receiver.
		let mut found_new_path = false;

		// Step (2).
		'path_construction: while let Some(RouteGraphNode { pubkey, lowest_fee_to_node, .. }) = routing_state.targeted_edges.pop() {

			// Since we're going payee-to-payer, hitting our node as a target means that we should stop traversing the
			// graph and arrange the path out of what we found.
			if pubkey == *our_node_id {
				let mut new_entry = routing_state.weighted_vertices.remove(&our_node_id).unwrap();
				let mut ordered_hops = vec!(new_entry.clone());
				// At most, we may need the value to be transferred and fees for that transfer.
				// Assume 900% fees as the highest possible amount we may need, keep in mind fees may be charged on every hop.
				let mut path_bottleneck_msat = final_value_msat * 10;

				loop {
					if let Some(&(_, ref features)) = first_hop_targets.get(&ordered_hops.last().unwrap().route_hop.pubkey) {
						ordered_hops.last_mut().unwrap().route_hop.node_features = features.to_context();
					} else if let Some(node) = network.get_nodes().get(&ordered_hops.last().unwrap().route_hop.pubkey) {
						if let Some(node_info) = node.announcement_info.as_ref() {
							ordered_hops.last_mut().unwrap().route_hop.node_features = node_info.features.clone();
						} else {
							ordered_hops.last_mut().unwrap().route_hop.node_features = NodeFeatures::empty();
						}
					} else {
						// We should be able to fill in features for everything except the last
						// hop, if the last hop was provided via a BOLT 11 invoice (though we
						// should be able to extend it further as BOLT 11 does have feature
						// flags for the last hop node itself).
						assert!(ordered_hops.last().unwrap().route_hop.pubkey == *payee);
					}

					if new_entry.available_liquidity_msat > new_entry.following_hops_fees_msat {
						// How much value a path can transfer is how much the weakest link can transfer after paying
						// for the use of the following channels.
						path_bottleneck_msat = cmp::min(path_bottleneck_msat, new_entry.available_liquidity_msat - new_entry.following_hops_fees_msat);
					} else {
						// TODO: explain
						path_bottleneck_msat = cmp::min(path_bottleneck_msat, new_entry.available_liquidity_msat / 10);
					}

					if ordered_hops.last().unwrap().route_hop.pubkey == *payee {
						break;
					}

					new_entry = match routing_state.weighted_vertices.remove(&ordered_hops.last().unwrap().route_hop.pubkey) {
						Some(payment_hop) => payment_hop,
						None => {
							// If we can't reach a given node, something wen't wrong during path traverse.
							// Stop looking for more paths here because next iterations will face the exact same issue.
							break 'paths_collection;
						}
					};
					// We "propagate" the fees one hop backward (topologically) here, so that fees paid on the
					// current channels are associated with the previous channel (where they will be paid).
					ordered_hops.last_mut().unwrap().route_hop.fee_msat = new_entry.hop_use_fee_msat;
					ordered_hops.last_mut().unwrap().route_hop.cltv_expiry_delta = new_entry.route_hop.cltv_expiry_delta;
					ordered_hops.push(new_entry.clone());
				}
				ordered_hops.last_mut().unwrap().route_hop.fee_msat = final_value_msat;
				ordered_hops.last_mut().unwrap().route_hop.cltv_expiry_delta = final_cltv;

				let mut payment_path = PaymentPath {hops: ordered_hops};
				// Since a path allows to transfer as much value as the smallest channel it has ("bottleneck"),
				// we should recompute the fees so sender HTLC don't overpay fees when traversing larger channels than the bottleneck.
				// This may happen because when we were selecting those channels we were not aware how much value this path
				// will transfer, and the relative fee for them might have been computed considering a larger value.
				payment_path.update_value_and_recompute_fees(path_bottleneck_msat);

				// Remember that we used these channels so that we don't rely on the same liquidity in future paths.
				for (_, payment_hop) in payment_path.hops.iter().enumerate() {
					let channel_liquidity_available_msat = routing_state.bookkeeped_channels_liquidity_available_msat.get_mut(&payment_hop.route_hop.short_channel_id).unwrap();
					if *channel_liquidity_available_msat < payment_hop.get_fee_paid_msat() {
						break 'path_construction;
					}
					*channel_liquidity_available_msat -= payment_hop.get_fee_paid_msat();
				}
				// Track the total amount all our collected paths allow to send so that we:
				// - know when to stop looking for more paths
				// - know which of the hops are useless considering how much more sats we need
				routing_state.already_collected_value_msat += payment_path.get_value_msat();

				payment_paths.push(payment_path);
				found_new_path = true;
				break 'path_construction;
			}

			// Otherwise, since the current target node is not us, keep "unrolling" the payment graph from
			// payee to payer by finding a way to reach the current target from the payer side.
			match network.get_nodes().get(&pubkey) {
				None => {},
				Some(node) => {
					if first_hops.is_some() {
						if let Some(&(ref first_hop, ref features)) = first_hop_targets.get(&pubkey) {
							routing_state.add_vertice(*first_hop, our_node_id, &pubkey, &DirectionalChannelInfo::default(), None::<u64>, features.to_context(), lowest_fee_to_node, network);
						}
					}
					routing_state.select_weighted_vertice_to_target_edge(node, &pubkey, lowest_fee_to_node, first_hops, network);
				},
			}
		}

		// Step (3).
		// Stop either when recommended value is reached, or if during last iteration no new path was found.
		// In the latter case, making another path finding attempt could not help,
		// because we deterministically terminate the search due to low liquidity.
		if routing_state.already_collected_value_msat >= recommended_value_msat || !found_new_path {
			break 'paths_collection;
		}
	}

	// Step (4).
	if payment_paths.len() == 0 {
		return Err(LightningError{err: "Failed to find a path to the given destination".to_owned(), action: ErrorAction::IgnoreError});
	}

	if routing_state.already_collected_value_msat < final_value_msat {
		return Err(LightningError{err: "Failed to find a sufficient route to the given destination".to_owned(), action: ErrorAction::IgnoreError});
	}

	// Sort by total fees and take the best paths.
	payment_paths.sort_by_key(|path| path.get_total_fee_paid_msat());
	if payment_paths.len() > 50 {
		payment_paths.truncate(50);
	}

	// Draw multiple sufficient routes by randomly combining the selected paths.
	let mut drawn_routes = Vec::new();
	for i in 0..payment_paths.len() {
		let mut cur_route = Vec::<PaymentPath>::new();
		let mut aggregate_route_value_msat = 0;

		// Step (5).
		// TODO: real random shuffle
		// Currently just starts with i_th and goes up to i-1_th in a looped way.
		let cur_payment_paths = [&payment_paths[i..], &payment_paths[..i]].concat();

		// Step (6).
		for payment_path in cur_payment_paths {
			cur_route.push(payment_path.clone());
			aggregate_route_value_msat += payment_path.get_value_msat();
			if aggregate_route_value_msat >= final_value_msat {
				// Last path likely overpaid. Substract it from the most expensive
				// (in terms of proportional fee) path in this route and recompute fees.
				// This might be not the most economically efficient way, but fewer paths
				// also makes routing more reliable.
				let mut overpaid_value_msat = aggregate_route_value_msat - final_value_msat;

				// First, drop some expensive low-value paths entirely if possible.
				// Sort by value so that we drop many really-low values first.
				cur_route.sort_by_key(|path| path.get_value_msat());
				// We should make sure that at least 1 path left.
				let mut paths_left = cur_route.len();
				cur_route.retain(|path| {
					if paths_left == 1 {
						return true
					}
					let mut keep = true;
					let path_value_msat = path.get_value_msat();
					if path_value_msat <= overpaid_value_msat {
						keep = false;
						overpaid_value_msat -= path_value_msat;
						paths_left -= 1;
					}
					keep
				});

				if overpaid_value_msat == 0 {
					break;
				}

				assert!(cur_route.len() > 0);

				// Step (7).
				// Now, substract from the most-expensive path the remaining value.
				cur_route.sort_by_key(|path| { path.hops.iter().map(|hop| hop.channel_fees.proportional_millionths).sum::<u32>() });
				let expensive_payment_path = cur_route.last_mut().unwrap();
				let expensive_path_new_value_msat = expensive_payment_path.get_value_msat() - overpaid_value_msat;
				expensive_payment_path.update_value_and_recompute_fees(expensive_path_new_value_msat);
				break;
			}
		}
		drawn_routes.push(cur_route);
	}


	// Step (8).
	// Select the best route by lowest total fee.
	drawn_routes.sort_by_key(|paths| paths.iter().map(|path| path.get_total_fee_paid_msat()).sum::<u64>());
	let mut selected_paths = Vec::<Vec::<RouteHop>>::new();
	for payment_path in drawn_routes.first().unwrap() {
		selected_paths.push(payment_path.hops.iter().map(|payment_hop| payment_hop.route_hop.clone()).collect());
	}

	let route = Route { paths: selected_paths };
	log_trace!(logger, "Got route: {}", log_route!(route));
	return Ok(route);
}

#[cfg(test)]
mod tests {
	use routing::router::{get_route, RouteHint, RoutingFees};
	use routing::network_graph::NetGraphMsgHandler;
	use ln::features::{ChannelFeatures, InitFeatures, NodeFeatures};
	use ln::msgs::{ErrorAction, LightningError, OptionalField, UnsignedChannelAnnouncement, ChannelAnnouncement, RoutingMessageHandler,
	   NodeAnnouncement, UnsignedNodeAnnouncement, ChannelUpdate, UnsignedChannelUpdate};
	use ln::channelmanager;
	use util::test_utils;
	use util::ser::Writeable;

	use bitcoin::hashes::sha256d::Hash as Sha256dHash;
	use bitcoin::hashes::Hash;
	use bitcoin::network::constants::Network;
	use bitcoin::blockdata::constants::genesis_block;
	use bitcoin::blockdata::script::Builder;
	use bitcoin::blockdata::opcodes;
	use bitcoin::blockdata::transaction::TxOut;

	use hex;

	use bitcoin::secp256k1::key::{PublicKey,SecretKey};
	use bitcoin::secp256k1::{Secp256k1, All};

	use std::sync::Arc;

	// Using the same keys for LN and BTC ids
	fn add_channel(net_graph_msg_handler: &NetGraphMsgHandler<Arc<test_utils::TestChainSource>, Arc<test_utils::TestLogger>>, secp_ctx: &Secp256k1<All>, node_1_privkey: &SecretKey,
	   node_2_privkey: &SecretKey, features: ChannelFeatures, short_channel_id: u64) {
		let node_id_1 = PublicKey::from_secret_key(&secp_ctx, node_1_privkey);
		let node_id_2 = PublicKey::from_secret_key(&secp_ctx, node_2_privkey);

		let unsigned_announcement = UnsignedChannelAnnouncement {
			features,
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id,
			node_id_1,
			node_id_2,
			bitcoin_key_1: node_id_1,
			bitcoin_key_2: node_id_2,
			excess_data: Vec::new(),
		};

		let msghash = hash_to_message!(&Sha256dHash::hash(&unsigned_announcement.encode()[..])[..]);
		let valid_announcement = ChannelAnnouncement {
			node_signature_1: secp_ctx.sign(&msghash, node_1_privkey),
			node_signature_2: secp_ctx.sign(&msghash, node_2_privkey),
			bitcoin_signature_1: secp_ctx.sign(&msghash, node_1_privkey),
			bitcoin_signature_2: secp_ctx.sign(&msghash, node_2_privkey),
			contents: unsigned_announcement.clone(),
		};
		match net_graph_msg_handler.handle_channel_announcement(&valid_announcement) {
			Ok(res) => assert!(res),
			_ => panic!()
		};
	}

	fn update_channel(net_graph_msg_handler: &NetGraphMsgHandler<Arc<test_utils::TestChainSource>, Arc<test_utils::TestLogger>>, secp_ctx: &Secp256k1<All>, node_privkey: &SecretKey, update: UnsignedChannelUpdate) {
		let msghash = hash_to_message!(&Sha256dHash::hash(&update.encode()[..])[..]);
		let valid_channel_update = ChannelUpdate {
			signature: secp_ctx.sign(&msghash, node_privkey),
			contents: update.clone()
		};

		match net_graph_msg_handler.handle_channel_update(&valid_channel_update) {
			Ok(res) => assert!(res),
			Err(_) => panic!()
		};
	}


	fn add_or_update_node(net_graph_msg_handler: &NetGraphMsgHandler<Arc<test_utils::TestChainSource>, Arc<test_utils::TestLogger>>, secp_ctx: &Secp256k1<All>, node_privkey: &SecretKey,
	   features: NodeFeatures, timestamp: u32) {
		let node_id = PublicKey::from_secret_key(&secp_ctx, node_privkey);
		let unsigned_announcement = UnsignedNodeAnnouncement {
			features,
			timestamp,
			node_id,
			rgb: [0; 3],
			alias: [0; 32],
			addresses: Vec::new(),
			excess_address_data: Vec::new(),
			excess_data: Vec::new(),
		};
		let msghash = hash_to_message!(&Sha256dHash::hash(&unsigned_announcement.encode()[..])[..]);
		let valid_announcement = NodeAnnouncement {
			signature: secp_ctx.sign(&msghash, node_privkey),
			contents: unsigned_announcement.clone()
		};

		match net_graph_msg_handler.handle_node_announcement(&valid_announcement) {
			Ok(_) => (),
			Err(_) => panic!()
		};
	}

	fn get_nodes(secp_ctx: &Secp256k1<All>) -> (SecretKey, PublicKey, Vec<SecretKey>, Vec<PublicKey>) {
		let privkeys: Vec<SecretKey> = (2..10).map(|i| {
			SecretKey::from_slice(&hex::decode(format!("{:02}", i).repeat(32)).unwrap()[..]).unwrap()
		}).collect();

		let pubkeys = privkeys.iter().map(|secret| PublicKey::from_secret_key(&secp_ctx, secret)).collect();

		let our_privkey = SecretKey::from_slice(&hex::decode("01".repeat(32)).unwrap()[..]).unwrap();
		let our_id = PublicKey::from_secret_key(&secp_ctx, &our_privkey);

		(our_privkey, our_id, privkeys, pubkeys)
	}

	fn id_to_feature_flags(id: u8) -> Vec<u8> {
		// Set the feature flags to the id'th odd (ie non-required) feature bit so that we can
		// test for it later.
		let idx = (id - 1) * 2 + 1;
		if idx > 8*3 {
			vec![1 << (idx - 8*3), 0, 0, 0]
		} else if idx > 8*2 {
			vec![1 << (idx - 8*2), 0, 0]
		} else if idx > 8*1 {
			vec![1 << (idx - 8*1), 0]
		} else {
			vec![1 << idx]
		}
	}

	fn build_graph() -> (Secp256k1<All>, NetGraphMsgHandler<std::sync::Arc<test_utils::TestChainSource>, std::sync::Arc<crate::util::test_utils::TestLogger>>, std::sync::Arc<test_utils::TestChainSource>, std::sync::Arc<test_utils::TestLogger>) {
		let secp_ctx = Secp256k1::new();
		let logger = Arc::new(test_utils::TestLogger::new());
		let chain_monitor = Arc::new(test_utils::TestChainSource::new(Network::Testnet));
		let net_graph_msg_handler = NetGraphMsgHandler::new(None, Arc::clone(&logger));
		// Build network from our_id to node7:
		//
		//        -1(1)2-  node0  -1(3)2-
		//       /                       \
		// our_id -1(12)2- node7 -1(13)2--- node2
		//       \                       /
		//        -1(2)2-  node1  -1(4)2-
		//
		//
		// chan1  1-to-2: disabled
		// chan1  2-to-1: enabled, 0 fee
		//
		// chan2  1-to-2: enabled, ignored fee
		// chan2  2-to-1: enabled, 0 fee
		//
		// chan3  1-to-2: enabled, 0 fee
		// chan3  2-to-1: enabled, 100 msat fee
		//
		// chan4  1-to-2: enabled, 100% fee
		// chan4  2-to-1: enabled, 0 fee
		//
		// chan12 1-to-2: enabled, ignored fee
		// chan12 2-to-1: enabled, 0 fee
		//
		// chan13 1-to-2: enabled, 200% fee
		// chan13 2-to-1: enabled, 0 fee
		//
		//
		//       -1(5)2- node3 -1(8)2--
		//       |         2          |
		//       |       (11)         |
		//      /          1           \
		// node2--1(6)2- node4 -1(9)2--- node6 (not in global route map)
		//      \                      /
		//       -1(7)2- node5 -1(10)2-
		//
		// chan5  1-to-2: enabled, 100 msat fee
		// chan5  2-to-1: enabled, 0 fee
		//
		// chan6  1-to-2: enabled, 0 fee
		// chan6  2-to-1: enabled, 0 fee
		//
		// chan7  1-to-2: enabled, 100% fee
		// chan7  2-to-1: enabled, 0 fee
		//
		// chan8  1-to-2: enabled, variable fee (0 then 1000 msat)
		// chan8  2-to-1: enabled, 0 fee
		//
		// chan9  1-to-2: enabled, 1001 msat fee
		// chan9  2-to-1: enabled, 0 fee
		//
		// chan10 1-to-2: enabled, 0 fee
		// chan10 2-to-1: enabled, 0 fee
		//
		// chan11 1-to-2: enabled, 0 fee
		// chan11 2-to-1: enabled, 0 fee

		let (our_privkey, _, privkeys, _) = get_nodes(&secp_ctx);

		add_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, &privkeys[0], ChannelFeatures::from_le_bytes(id_to_feature_flags(1)), 1);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 1,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[0], NodeFeatures::from_le_bytes(id_to_feature_flags(1)), 0);

		add_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, &privkeys[1], ChannelFeatures::from_le_bytes(id_to_feature_flags(2)), 2);
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 2,
			timestamp: 1,
			flags: 0,
			cltv_expiry_delta: u16::max_value(),
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: u32::max_value(),
			fee_proportional_millionths: u32::max_value(),
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[1], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 2,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[1], NodeFeatures::from_le_bytes(id_to_feature_flags(2)), 0);

		add_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, &privkeys[7], ChannelFeatures::from_le_bytes(id_to_feature_flags(12)), 12);
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 1,
			flags: 0,
			cltv_expiry_delta: u16::max_value(),
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: u32::max_value(),
			fee_proportional_millionths: u32::max_value(),
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[7], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[7], NodeFeatures::from_le_bytes(id_to_feature_flags(8)), 0);

		add_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], &privkeys[2], ChannelFeatures::from_le_bytes(id_to_feature_flags(3)), 3);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 1,
			flags: 0,
			cltv_expiry_delta: (3 << 8) | 1,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: (3 << 8) | 2,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 100,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		add_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[1], &privkeys[2], ChannelFeatures::from_le_bytes(id_to_feature_flags(4)), 4);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[1], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 4,
			timestamp: 1,
			flags: 0,
			cltv_expiry_delta: (4 << 8) | 1,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 1000000,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 4,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: (4 << 8) | 2,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		add_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[7], &privkeys[2], ChannelFeatures::from_le_bytes(id_to_feature_flags(13)), 13);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[7], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 13,
			timestamp: 1,
			flags: 0,
			cltv_expiry_delta: (13 << 8) | 1,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 2000000,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 13,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: (13 << 8) | 2,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[2], NodeFeatures::from_le_bytes(id_to_feature_flags(3)), 0);

		add_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], &privkeys[4], ChannelFeatures::from_le_bytes(id_to_feature_flags(6)), 6);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 6,
			timestamp: 1,
			flags: 0,
			cltv_expiry_delta: (6 << 8) | 1,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[4], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 6,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: (6 << 8) | 2,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new(),
		});

		add_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[4], &privkeys[3], ChannelFeatures::from_le_bytes(id_to_feature_flags(11)), 11);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[4], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 11,
			timestamp: 1,
			flags: 0,
			cltv_expiry_delta: (11 << 8) | 1,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[3], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 11,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: (11 << 8) | 2,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[4], NodeFeatures::from_le_bytes(id_to_feature_flags(5)), 0);

		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[3], NodeFeatures::from_le_bytes(id_to_feature_flags(4)), 0);

		add_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], &privkeys[5], ChannelFeatures::from_le_bytes(id_to_feature_flags(7)), 7);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 7,
			timestamp: 1,
			flags: 0,
			cltv_expiry_delta: (7 << 8) | 1,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 1000000,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[5], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 7,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: (7 << 8) | 2,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[5], NodeFeatures::from_le_bytes(id_to_feature_flags(6)), 0);

		(secp_ctx, net_graph_msg_handler, chain_monitor, logger)
	}

	#[test]
	fn simple_route_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (_, our_id, _, nodes) = get_nodes(&secp_ctx);

		// Simple route to 3 via 2
		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, &Vec::new(), 100, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 2);

		assert_eq!(route.paths[0][0].pubkey, nodes[1]);
		assert_eq!(route.paths[0][0].short_channel_id, 2);
		assert_eq!(route.paths[0][0].fee_msat, 100);
		assert_eq!(route.paths[0][0].cltv_expiry_delta, (4 << 8) | 1);
		assert_eq!(route.paths[0][0].node_features.le_flags(), &id_to_feature_flags(2));
		assert_eq!(route.paths[0][0].channel_features.le_flags(), &id_to_feature_flags(2));

		assert_eq!(route.paths[0][1].pubkey, nodes[2]);
		assert_eq!(route.paths[0][1].short_channel_id, 4);
		assert_eq!(route.paths[0][1].fee_msat, 100);
		assert_eq!(route.paths[0][1].cltv_expiry_delta, 42);
		assert_eq!(route.paths[0][1].node_features.le_flags(), &id_to_feature_flags(3));
		assert_eq!(route.paths[0][1].channel_features.le_flags(), &id_to_feature_flags(4));
	}

	#[test]
	fn disable_channels_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (our_privkey, our_id, privkeys, nodes) = get_nodes(&secp_ctx);

		// // Disable channels 4 and 12 by flags=2
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[1], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 4,
			timestamp: 2,
			flags: 2, // to disable
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 2,
			flags: 2, // to disable
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// If all the channels require some features we don't understand, route should fail
		if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, &Vec::new(), 100, 42, Arc::clone(&logger)) {
			assert_eq!(err, "Failed to find a path to the given destination");
		} else { panic!(); }

		// If we specify a channel to node7, that overrides our local channel view and that gets used
		let our_chans = vec![channelmanager::ChannelDetails {
			channel_id: [0; 32],
			short_channel_id: Some(42),
			remote_network_id: nodes[7].clone(),
			counterparty_features: InitFeatures::from_le_bytes(vec![0b11]),
			channel_value_satoshis: 0,
			user_id: 0,
			outbound_capacity_msat: 0,
			inbound_capacity_msat: 0,
			is_live: true,
		}];
		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], Some(&our_chans.iter().collect::<Vec<_>>()),  &Vec::new(), 100, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 2);

		assert_eq!(route.paths[0][0].pubkey, nodes[7]);
		assert_eq!(route.paths[0][0].short_channel_id, 42);
		assert_eq!(route.paths[0][0].fee_msat, 200);
		assert_eq!(route.paths[0][0].cltv_expiry_delta, (13 << 8) | 1);
		assert_eq!(route.paths[0][0].node_features.le_flags(), &vec![0b11]); // it should also override our view of their features
		assert_eq!(route.paths[0][0].channel_features.le_flags(), &Vec::<u8>::new()); // No feature flags will meet the relevant-to-channel conversion

		assert_eq!(route.paths[0][1].pubkey, nodes[2]);
		assert_eq!(route.paths[0][1].short_channel_id, 13);
		assert_eq!(route.paths[0][1].fee_msat, 100);
		assert_eq!(route.paths[0][1].cltv_expiry_delta, 42);
		assert_eq!(route.paths[0][1].node_features.le_flags(), &id_to_feature_flags(3));
		assert_eq!(route.paths[0][1].channel_features.le_flags(), &id_to_feature_flags(13));
	}

	#[test]
	fn disable_node_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (_, our_id, privkeys, nodes) = get_nodes(&secp_ctx);

		// Disable nodes 1, 2, and 8 by requiring unknown feature bits
		let mut unknown_features = NodeFeatures::known();
		unknown_features.set_required_unknown_bits();
		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[0], unknown_features.clone(), 1);
		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[1], unknown_features.clone(), 1);
		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[7], unknown_features.clone(), 1);

		// If all nodes require some features we don't understand, route should fail
		if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, &Vec::new(), 100, 42, Arc::clone(&logger)) {
			assert_eq!(err, "Failed to find a path to the given destination");
		} else { panic!(); }

		// If we specify a channel to node7, that overrides our local channel view and that gets used
		let our_chans = vec![channelmanager::ChannelDetails {
			channel_id: [0; 32],
			short_channel_id: Some(42),
			remote_network_id: nodes[7].clone(),
			counterparty_features: InitFeatures::from_le_bytes(vec![0b11]),
			channel_value_satoshis: 0,
			user_id: 0,
			outbound_capacity_msat: 0,
			inbound_capacity_msat: 0,
			is_live: true,
		}];
		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], Some(&our_chans.iter().collect::<Vec<_>>()), &Vec::new(), 100, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 2);

		assert_eq!(route.paths[0][0].pubkey, nodes[7]);
		assert_eq!(route.paths[0][0].short_channel_id, 42);
		assert_eq!(route.paths[0][0].fee_msat, 200);
		assert_eq!(route.paths[0][0].cltv_expiry_delta, (13 << 8) | 1);
		assert_eq!(route.paths[0][0].node_features.le_flags(), &vec![0b11]); // it should also override our view of their features
		assert_eq!(route.paths[0][0].channel_features.le_flags(), &Vec::<u8>::new()); // No feature flags will meet the relevant-to-channel conversion

		assert_eq!(route.paths[0][1].pubkey, nodes[2]);
		assert_eq!(route.paths[0][1].short_channel_id, 13);
		assert_eq!(route.paths[0][1].fee_msat, 100);
		assert_eq!(route.paths[0][1].cltv_expiry_delta, 42);
		assert_eq!(route.paths[0][1].node_features.le_flags(), &id_to_feature_flags(3));
		assert_eq!(route.paths[0][1].channel_features.le_flags(), &id_to_feature_flags(13));

		// Note that we don't test disabling node 3 and failing to route to it, as we (somewhat
		// naively) assume that the user checked the feature bits on the invoice, which override
		// the node_announcement.
	}

	#[test]
	fn our_chans_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (_, our_id, _, nodes) = get_nodes(&secp_ctx);

		// Route to 1 via 2 and 3 because our channel to 1 is disabled
		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[0], None, &Vec::new(), 100, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 3);

		assert_eq!(route.paths[0][0].pubkey, nodes[1]);
		assert_eq!(route.paths[0][0].short_channel_id, 2);
		assert_eq!(route.paths[0][0].fee_msat, 200);
		assert_eq!(route.paths[0][0].cltv_expiry_delta, (4 << 8) | 1);
		assert_eq!(route.paths[0][0].node_features.le_flags(), &id_to_feature_flags(2));
		assert_eq!(route.paths[0][0].channel_features.le_flags(), &id_to_feature_flags(2));

		assert_eq!(route.paths[0][1].pubkey, nodes[2]);
		assert_eq!(route.paths[0][1].short_channel_id, 4);
		assert_eq!(route.paths[0][1].fee_msat, 100);
		assert_eq!(route.paths[0][1].cltv_expiry_delta, (3 << 8) | 2);
		assert_eq!(route.paths[0][1].node_features.le_flags(), &id_to_feature_flags(3));
		assert_eq!(route.paths[0][1].channel_features.le_flags(), &id_to_feature_flags(4));

		assert_eq!(route.paths[0][2].pubkey, nodes[0]);
		assert_eq!(route.paths[0][2].short_channel_id, 3);
		assert_eq!(route.paths[0][2].fee_msat, 100);
		assert_eq!(route.paths[0][2].cltv_expiry_delta, 42);
		assert_eq!(route.paths[0][2].node_features.le_flags(), &id_to_feature_flags(1));
		assert_eq!(route.paths[0][2].channel_features.le_flags(), &id_to_feature_flags(3));

		// If we specify a channel to node7, that overrides our local channel view and that gets used
		let our_chans = vec![channelmanager::ChannelDetails {
			channel_id: [0; 32],
			short_channel_id: Some(42),
			remote_network_id: nodes[7].clone(),
			counterparty_features: InitFeatures::from_le_bytes(vec![0b11]),
			channel_value_satoshis: 0,
			user_id: 0,
			outbound_capacity_msat: 0,
			inbound_capacity_msat: 0,
			is_live: true,
		}];
		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], Some(&our_chans.iter().collect::<Vec<_>>()), &Vec::new(), 100, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 2);

		assert_eq!(route.paths[0][0].pubkey, nodes[7]);
		assert_eq!(route.paths[0][0].short_channel_id, 42);
		assert_eq!(route.paths[0][0].fee_msat, 200);
		assert_eq!(route.paths[0][0].cltv_expiry_delta, (13 << 8) | 1);
		assert_eq!(route.paths[0][0].node_features.le_flags(), &vec![0b11]);
		assert_eq!(route.paths[0][0].channel_features.le_flags(), &Vec::<u8>::new()); // No feature flags will meet the relevant-to-channel conversion

		assert_eq!(route.paths[0][1].pubkey, nodes[2]);
		assert_eq!(route.paths[0][1].short_channel_id, 13);
		assert_eq!(route.paths[0][1].fee_msat, 100);
		assert_eq!(route.paths[0][1].cltv_expiry_delta, 42);
		assert_eq!(route.paths[0][1].node_features.le_flags(), &id_to_feature_flags(3));
		assert_eq!(route.paths[0][1].channel_features.le_flags(), &id_to_feature_flags(13));
	}

	fn last_hops(nodes: &Vec<PublicKey>) -> Vec<RouteHint> {
		let zero_fees = RoutingFees {
			base_msat: 0,
			proportional_millionths: 0,
		};
		vec!(RouteHint {
			src_node_id: nodes[3].clone(),
			short_channel_id: 8,
			fees: zero_fees,
			cltv_expiry_delta: (8 << 8) | 1,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: None,
		}, RouteHint {
			src_node_id: nodes[4].clone(),
			short_channel_id: 9,
			fees: RoutingFees {
				base_msat: 1001,
				proportional_millionths: 0,
			},
			cltv_expiry_delta: (9 << 8) | 1,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: None,
		}, RouteHint {
			src_node_id: nodes[5].clone(),
			short_channel_id: 10,
			fees: zero_fees,
			cltv_expiry_delta: (10 << 8) | 1,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: None,
		})
	}

	#[test]
	fn last_hops_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (_, our_id, _, nodes) = get_nodes(&secp_ctx);

		// Simple test across 2, 3, 5, and 4 via a last_hop channel
		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[6], None, &last_hops(&nodes).iter().collect::<Vec<_>>(), 100, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 5);

		assert_eq!(route.paths[0][0].pubkey, nodes[1]);
		assert_eq!(route.paths[0][0].short_channel_id, 2);
		assert_eq!(route.paths[0][0].fee_msat, 100);
		assert_eq!(route.paths[0][0].cltv_expiry_delta, (4 << 8) | 1);
		assert_eq!(route.paths[0][0].node_features.le_flags(), &id_to_feature_flags(2));
		assert_eq!(route.paths[0][0].channel_features.le_flags(), &id_to_feature_flags(2));

		assert_eq!(route.paths[0][1].pubkey, nodes[2]);
		assert_eq!(route.paths[0][1].short_channel_id, 4);
		assert_eq!(route.paths[0][1].fee_msat, 0);
		assert_eq!(route.paths[0][1].cltv_expiry_delta, (6 << 8) | 1);
		assert_eq!(route.paths[0][1].node_features.le_flags(), &id_to_feature_flags(3));
		assert_eq!(route.paths[0][1].channel_features.le_flags(), &id_to_feature_flags(4));

		assert_eq!(route.paths[0][2].pubkey, nodes[4]);
		assert_eq!(route.paths[0][2].short_channel_id, 6);
		assert_eq!(route.paths[0][2].fee_msat, 0);
		assert_eq!(route.paths[0][2].cltv_expiry_delta, (11 << 8) | 1);
		assert_eq!(route.paths[0][2].node_features.le_flags(), &id_to_feature_flags(5));
		assert_eq!(route.paths[0][2].channel_features.le_flags(), &id_to_feature_flags(6));

		assert_eq!(route.paths[0][3].pubkey, nodes[3]);
		assert_eq!(route.paths[0][3].short_channel_id, 11);
		assert_eq!(route.paths[0][3].fee_msat, 0);
		assert_eq!(route.paths[0][3].cltv_expiry_delta, (8 << 8) | 1);
		// If we have a peer in the node map, we'll use their features here since we don't have
		// a way of figuring out their features from the invoice:
		assert_eq!(route.paths[0][3].node_features.le_flags(), &id_to_feature_flags(4));
		assert_eq!(route.paths[0][3].channel_features.le_flags(), &id_to_feature_flags(11));

		assert_eq!(route.paths[0][4].pubkey, nodes[6]);
		assert_eq!(route.paths[0][4].short_channel_id, 8);
		assert_eq!(route.paths[0][4].fee_msat, 100);
		assert_eq!(route.paths[0][4].cltv_expiry_delta, 42);
		assert_eq!(route.paths[0][4].node_features.le_flags(), &Vec::<u8>::new()); // We dont pass flags in from invoices yet
		assert_eq!(route.paths[0][4].channel_features.le_flags(), &Vec::<u8>::new()); // We can't learn any flags from invoices, sadly
	}

	#[test]
	fn our_chans_last_hop_connect_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (_, our_id, _, nodes) = get_nodes(&secp_ctx);

		// Simple test with outbound channel to 4 to test that last_hops and first_hops connect
		let our_chans = vec![channelmanager::ChannelDetails {
			channel_id: [0; 32],
			short_channel_id: Some(42),
			remote_network_id: nodes[3].clone(),
			counterparty_features: InitFeatures::from_le_bytes(vec![0b11]),
			channel_value_satoshis: 0,
			user_id: 0,
			outbound_capacity_msat: 0,
			inbound_capacity_msat: 0,
			is_live: true,
		}];
		let mut last_hops = last_hops(&nodes);
		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[6], Some(&our_chans.iter().collect::<Vec<_>>()), &last_hops.iter().collect::<Vec<_>>(), 100, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 2);

		assert_eq!(route.paths[0][0].pubkey, nodes[3]);
		assert_eq!(route.paths[0][0].short_channel_id, 42);
		assert_eq!(route.paths[0][0].fee_msat, 0);
		assert_eq!(route.paths[0][0].cltv_expiry_delta, (8 << 8) | 1);
		assert_eq!(route.paths[0][0].node_features.le_flags(), &vec![0b11]);
		assert_eq!(route.paths[0][0].channel_features.le_flags(), &Vec::<u8>::new()); // No feature flags will meet the relevant-to-channel conversion

		assert_eq!(route.paths[0][1].pubkey, nodes[6]);
		assert_eq!(route.paths[0][1].short_channel_id, 8);
		assert_eq!(route.paths[0][1].fee_msat, 100);
		assert_eq!(route.paths[0][1].cltv_expiry_delta, 42);
		assert_eq!(route.paths[0][1].node_features.le_flags(), &Vec::<u8>::new()); // We dont pass flags in from invoices yet
		assert_eq!(route.paths[0][1].channel_features.le_flags(), &Vec::<u8>::new()); // We can't learn any flags from invoices, sadly

		last_hops[0].fees.base_msat = 1000;

		// Revert to via 6 as the fee on 8 goes up
		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[6], None, &last_hops.iter().collect::<Vec<_>>(), 100, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 4);

		assert_eq!(route.paths[0][0].pubkey, nodes[1]);
		assert_eq!(route.paths[0][0].short_channel_id, 2);
		assert_eq!(route.paths[0][0].fee_msat, 200); // fee increased as its % of value transferred across node
		assert_eq!(route.paths[0][0].cltv_expiry_delta, (4 << 8) | 1);
		assert_eq!(route.paths[0][0].node_features.le_flags(), &id_to_feature_flags(2));
		assert_eq!(route.paths[0][0].channel_features.le_flags(), &id_to_feature_flags(2));

		assert_eq!(route.paths[0][1].pubkey, nodes[2]);
		assert_eq!(route.paths[0][1].short_channel_id, 4);
		assert_eq!(route.paths[0][1].fee_msat, 100);
		assert_eq!(route.paths[0][1].cltv_expiry_delta, (7 << 8) | 1);
		assert_eq!(route.paths[0][1].node_features.le_flags(), &id_to_feature_flags(3));
		assert_eq!(route.paths[0][1].channel_features.le_flags(), &id_to_feature_flags(4));

		assert_eq!(route.paths[0][2].pubkey, nodes[5]);
		assert_eq!(route.paths[0][2].short_channel_id, 7);
		assert_eq!(route.paths[0][2].fee_msat, 0);
		assert_eq!(route.paths[0][2].cltv_expiry_delta, (10 << 8) | 1);
		// If we have a peer in the node map, we'll use their features here since we don't have
		// a way of figuring out their features from the invoice:
		assert_eq!(route.paths[0][2].node_features.le_flags(), &id_to_feature_flags(6));
		assert_eq!(route.paths[0][2].channel_features.le_flags(), &id_to_feature_flags(7));

		assert_eq!(route.paths[0][3].pubkey, nodes[6]);
		assert_eq!(route.paths[0][3].short_channel_id, 10);
		assert_eq!(route.paths[0][3].fee_msat, 100);
		assert_eq!(route.paths[0][3].cltv_expiry_delta, 42);
		assert_eq!(route.paths[0][3].node_features.le_flags(), &Vec::<u8>::new()); // We dont pass flags in from invoices yet
		assert_eq!(route.paths[0][3].channel_features.le_flags(), &Vec::<u8>::new()); // We can't learn any flags from invoices, sadly

		// ...but still use 8 for larger payments as 6 has a variable feerate
		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[6], None, &last_hops.iter().collect::<Vec<_>>(), 2000, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 5);

		assert_eq!(route.paths[0][0].pubkey, nodes[1]);
		assert_eq!(route.paths[0][0].short_channel_id, 2);
		assert_eq!(route.paths[0][0].fee_msat, 3000);
		assert_eq!(route.paths[0][0].cltv_expiry_delta, (4 << 8) | 1);
		assert_eq!(route.paths[0][0].node_features.le_flags(), &id_to_feature_flags(2));
		assert_eq!(route.paths[0][0].channel_features.le_flags(), &id_to_feature_flags(2));

		assert_eq!(route.paths[0][1].pubkey, nodes[2]);
		assert_eq!(route.paths[0][1].short_channel_id, 4);
		assert_eq!(route.paths[0][1].fee_msat, 0);
		assert_eq!(route.paths[0][1].cltv_expiry_delta, (6 << 8) | 1);
		assert_eq!(route.paths[0][1].node_features.le_flags(), &id_to_feature_flags(3));
		assert_eq!(route.paths[0][1].channel_features.le_flags(), &id_to_feature_flags(4));

		assert_eq!(route.paths[0][2].pubkey, nodes[4]);
		assert_eq!(route.paths[0][2].short_channel_id, 6);
		assert_eq!(route.paths[0][2].fee_msat, 0);
		assert_eq!(route.paths[0][2].cltv_expiry_delta, (11 << 8) | 1);
		assert_eq!(route.paths[0][2].node_features.le_flags(), &id_to_feature_flags(5));
		assert_eq!(route.paths[0][2].channel_features.le_flags(), &id_to_feature_flags(6));

		assert_eq!(route.paths[0][3].pubkey, nodes[3]);
		assert_eq!(route.paths[0][3].short_channel_id, 11);
		assert_eq!(route.paths[0][3].fee_msat, 1000);
		assert_eq!(route.paths[0][3].cltv_expiry_delta, (8 << 8) | 1);
		// If we have a peer in the node map, we'll use their features here since we don't have
		// a way of figuring out their features from the invoice:
		assert_eq!(route.paths[0][3].node_features.le_flags(), &id_to_feature_flags(4));
		assert_eq!(route.paths[0][3].channel_features.le_flags(), &id_to_feature_flags(11));

		assert_eq!(route.paths[0][4].pubkey, nodes[6]);
		assert_eq!(route.paths[0][4].short_channel_id, 8);
		assert_eq!(route.paths[0][4].fee_msat, 2000);
		assert_eq!(route.paths[0][4].cltv_expiry_delta, 42);
		assert_eq!(route.paths[0][4].node_features.le_flags(), &Vec::<u8>::new()); // We dont pass flags in from invoices yet
		assert_eq!(route.paths[0][4].channel_features.le_flags(), &Vec::<u8>::new()); // We can't learn any flags from invoices, sadly
	}

	#[test]
	fn available_amount_while_routing_test() {
		// Tests whether we choose the correct available channel amount while routing.
		
		let (secp_ctx, mut net_graph_msg_handler, chain_monitor, logger) = build_graph();
		let (our_privkey, our_id, privkeys, nodes) = get_nodes(&secp_ctx);

		// We will use a simple single-path route from our node to node2 via node0: channels {1, 3}.

		// First disable all other paths.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 2,
			timestamp: 2,
			flags: 2,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 2,
			flags: 2,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Make the first channel (#1) is very permissive, and we will be testing all limits on the second channel.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 1,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// First, let's see if routing works if we have absolutely no idea about the available amount.
		// In this case, it should be set to 10_000 sats.
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		{
			// Attempt to route more than is available results in a failure.
			if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, &Vec::new(), 10_000_001, 42, Arc::clone(&logger)) {
				assert_eq!(err, "Failed to find a sufficient route to the given destination");
			} else { panic!(); }
		}

		{
			// Now, attempt to route an exact amount we have should be fine.
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, &Vec::new(), 10_000_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 1);
			let path = route.paths.last().unwrap();
			assert_eq!(path.len(), 2);
			assert_eq!(path.last().unwrap().pubkey, nodes[2]);
			assert_eq!(path.last().unwrap().fee_msat, 10_000_000);
		}


		// Now let's see if routing works if we know only htlc_maximum_msat.
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 3,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(15_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		{
			// Attempt to route more than is available results in a failure.
			if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, &Vec::new(), 15_001, 42, Arc::clone(&logger)) {
				assert_eq!(err, "Failed to find a sufficient route to the given destination");
			} else { panic!(); }
		}

		{
			// Now, attempt to route an exact amount we have should be fine.
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, &Vec::new(), 15_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 1);
			let path = route.paths.last().unwrap();
			assert_eq!(path.len(), 2);
			assert_eq!(path.last().unwrap().pubkey, nodes[2]);
			assert_eq!(path.last().unwrap().fee_msat, 15_000);
		}

		// Now let's see if routing works if we know only capacity from the UTXO.

		// We can't change UTXO capacity on the fly, so we'll disable the existing channel and add another one with
		// the capacity we need.
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 4,
			flags: 2,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		let good_script = Builder::new().push_opcode(opcodes::all::OP_PUSHNUM_2)
		.push_slice(&PublicKey::from_secret_key(&secp_ctx, &privkeys[0]).serialize())
		.push_slice(&PublicKey::from_secret_key(&secp_ctx, &privkeys[2]).serialize())
		.push_opcode(opcodes::all::OP_PUSHNUM_2)
		.push_opcode(opcodes::all::OP_CHECKMULTISIG).into_script().to_v0_p2wsh();

		*chain_monitor.utxo_ret.lock().unwrap() = Ok(TxOut { value: 15, script_pubkey: good_script.clone() });
		net_graph_msg_handler.add_chain_access(Some(chain_monitor));

		add_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], &privkeys[2], ChannelFeatures::from_le_bytes(id_to_feature_flags(3)), 333);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 333,
			timestamp: 1,
			flags: 0,
			cltv_expiry_delta: (3 << 8) | 1,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 333,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: (3 << 8) | 2,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 100,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		{
			// Attempt to route more than is available results in a failure.
			if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, &Vec::new(), 15_001, 42, Arc::clone(&logger)) {
				assert_eq!(err, "Failed to find a sufficient route to the given destination");
			} else { panic!(); }
		}

		{
			// Now, attempt to route an exact amount we have should be fine.
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, &Vec::new(), 15_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 1);
			let path = route.paths.last().unwrap();
			assert_eq!(path.len(), 2);
			assert_eq!(path.last().unwrap().pubkey, nodes[2]);
			assert_eq!(path.last().unwrap().fee_msat, 15_000);
		}

		// Now let's see if routing chooses htlc_maximum_msat over UTXO capacity.
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 333,
			timestamp: 6,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(10_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		{
			// Attempt to route more than is available results in a failure.
			if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, &Vec::new(), 10_001, 42, Arc::clone(&logger)) {
				assert_eq!(err, "Failed to find a sufficient route to the given destination");
			} else { panic!(); }
		}

		{
			// Now, attempt to route an exact amount we have should be fine.
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, &Vec::new(), 10_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 1);
			let path = route.paths.last().unwrap();
			assert_eq!(path.len(), 2);
			assert_eq!(path.last().unwrap().pubkey, nodes[2]);
			assert_eq!(path.last().unwrap().fee_msat, 10_000);
		}
	}

	#[test]
	fn simple_mpp_route_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (our_privkey, our_id, privkeys, nodes) = get_nodes(&secp_ctx);

		// We need a route consisting of 3 paths:
		// From our node to node2 via node0, node7, node1 (three paths one hop each).
		// To achieve this, the amount being transferred should be around
		// the total capacity of these 3 paths.

		// First, we set limits on these (previously unlimited) channels.
		// Their aggregate capacity will be 50 + 60 + 180 = 290 sats.

		// Path via node0 is channels {1, 3}. Limit them to 100 and 50 sats (total limit 50);
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 1,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(50_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Path via node7 is channels {12, 13}. Limit them to 60 and 60 sats (total limit 60);
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(60_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[7], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 13,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(60_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Path via node1 is channels {2, 4}. Limit them to 200 and 180 sats (total capacity 180 sats).
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 2,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(200_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[1], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 4,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(180_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		{
			// Attempt to route more than is available results in a failure.
			if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, &Vec::new(), 300_000, 42, Arc::clone(&logger)) {
				assert_eq!(err, "Failed to find a sufficient route to the given destination");
			} else { panic!(); }
		}

		{
			// Now, attempt to route 250 sats (just a bit below the capacity).
			// Our algorithm should provide us with these 3 paths.
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, &Vec::new(), 250_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 3);
			let mut total_amount_paid_msat = 0;
			for path in &route.paths {
				assert_eq!(path.len(), 2);
				assert_eq!(path.last().unwrap().pubkey, nodes[2]);
				total_amount_paid_msat += path.last().unwrap().fee_msat;
			}
			assert_eq!(total_amount_paid_msat, 250_000);
		}

		{
			// Attempt to route an exact amount is also fine
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, &Vec::new(), 290_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 3);
			let mut total_amount_paid_msat = 0;
			for path in &route.paths {
				assert_eq!(path.len(), 2);
				assert_eq!(path.last().unwrap().pubkey, nodes[2]);
				total_amount_paid_msat += path.last().unwrap().fee_msat;
			}
			assert_eq!(total_amount_paid_msat, 290_000);
		}
	}


	#[test]
	fn long_mpp_route_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (our_privkey, our_id, privkeys, nodes) = get_nodes(&secp_ctx);

		// We need a route consisting of 35 paths:
		// From our node to node3 via {node0, node2}, {node7, node2, node4} and {node7, node2}.
		// Note that these paths overlap (channels 5, 12, 13).
		// We will route 300 sats.
		// Each path will have 100 sats capacity, those channels which are used twice will have 200 sats capacity.

		// Disable oter potential paths.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 2,
			timestamp: 2,
			flags: 2,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 7,
			timestamp: 2,
			flags: 2,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Path via {node0, node2} is channels {1, 3, 5}.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 1,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Capacity of 200 sats because this channel will be used by 3rd path as well.
		add_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], &privkeys[3], ChannelFeatures::from_le_bytes(id_to_feature_flags(5)), 5);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 5,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(200_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});


		// Path via {node7, node2, node4} is channels {12, 13, 6, 11}.
		// Add 100 sats to the capacities of {12, 13}, because these channels
		// are also used for 3rd path. 100 sats for the rest. Total capacity: 100 sats.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(200_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[7], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 13,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(200_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 6,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[4], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 11,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});


		// Path via {node7, node2} is channels {12, 13, 5}.
		// We already limited them to 200 sats (they are used twice for 100 sats).
		// Nothing to do here.

		{
			// Attempt to route more than is available results in a failure.
			if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, &Vec::new(), 350_000, 42, Arc::clone(&logger)) {
				assert_eq!(err, "Failed to find a sufficient route to the given destination");
			} else { panic!(); }
		}

		{
			// Now, attempt to route 300 sats (exact amount we can route).
			// Our algorithm should provide us with these 3 paths, 100 sats each.
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[3], None, &Vec::new(), 300_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 3);

			let mut total_amount_paid_msat = 0;
			for path in &route.paths {
				assert_eq!(path.last().unwrap().pubkey, nodes[3]);
				total_amount_paid_msat += path.last().unwrap().fee_msat;
			}
			assert_eq!(total_amount_paid_msat, 300_000);
		}

	}

	#[test]
	fn fees_on_mpp_route_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (our_privkey, our_id, privkeys, nodes) = get_nodes(&secp_ctx);

		// We need a route consisting of 2 paths:
		// From our node to node3 via {node0, node2} and {node7, node2, node4}.
		// We will route 200 sats, Each path will have 100 sats capacity.

		// This test is not particularly stable: e.g., there's a way to route via {node0, node2, node4}.
		// It works while pathfinding is deterministic, but can be broken otherwise.
		// It's fine to ignore this concern for now.

		// Disable other potential paths.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 2,
			timestamp: 2,
			flags: 2,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 7,
			timestamp: 2,
			flags: 2,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Path via {node0, node2} is channels {1, 3, 5}.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 1,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		add_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], &privkeys[3], ChannelFeatures::from_le_bytes(id_to_feature_flags(5)), 5);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 5,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});


		// Path via {node7, node2, node4} is channels {12, 13, 6, 11}.
		// All channels should be 100 sats capacity. But for the fee experiment,
		// we'll add absolute fee of 150 sats paid for the use channel 6 (paid to node2 on channel 13).
		// Since channel 12 allows to deliver only 250 sats to channel 13, channel 13 can transfer only
		// 100 sats (and pay 150 sats in fees for the use of channel 6), so no matter how large are other channels,
		// the whole path will be limited by 100 sats with just these 2 conditions:
		// - channel 12 capacity is 250 sats
		// - fee for channel 6 is 150 sats
		// Let's test this by enforcing these 2 conditions and removing other limits.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(250_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[7], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 13,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 6,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 150_000,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[4], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 11,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});


		{
			// Attempt to route more than is available results in a failure.
			if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[3], None, &Vec::new(), 210_000, 42, Arc::clone(&logger)) {
				assert_eq!(err, "Failed to find a sufficient route to the given destination");
			} else { panic!(); }
		}

		{
			// Now, attempt to route 300 sats (exact amount we can route).
			// Our algorithm should provide us with these 3 paths, 100 sats each.
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[3], None, &Vec::new(), 200_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 2);

			let mut total_amount_paid_msat = 0;
			for path in &route.paths {
				assert_eq!(path.last().unwrap().pubkey, nodes[3]);
				total_amount_paid_msat += path.last().unwrap().fee_msat;
			}
			assert_eq!(total_amount_paid_msat, 200_000);
		}

	}

}

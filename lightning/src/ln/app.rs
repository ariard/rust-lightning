// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use bitcoin::blockdata::block::BlockHeader;
use bitcoin::hash_types::BlockHash;

use ln::msgs::{ApplicationMessageHandler, LightningError};
use ln::msgs;
use util::events::{MessageSendEvent, MessageSendEventsProvider};

use std::io::prelude::*;
use std::net::{Ipv4Addr, TcpStream, SocketAddrV4};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct CustomMsgHandler {
	chain_hash: BlockHash,
	to_send: Mutex<Vec<BlockHeader>>,
	to_handle: Mutex<Vec<BlockHeader>>,

	interface_socket: SocketAddrV4,
	interface_stream: <Mutex<Option<TcpStream>>>,
	startup_complete: AtomicUsize,
}

impl CustomMsgHandler {
	pub fn new(hash: BlockHash, port: u16) -> CustomMsgHandler {
		let socket = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), port);
		CustomMsgHandler {
			chain_hash: hash,
			to_send: Mutex::new(vec![]),
			to_handle: Mutex::new(vec![]),
			interface_socket: socket,
			interface_stream: Mutex::new(None),
			startup_complete: AtomicUsize::new(0),
		}
	}

	pub fn process_pending_events(&self) -> bool {

		let mut maybe_headers = Vec::new();
		if self.startup_complete.load(Ordering::Relaxed) == 0 {
			let stream = TcpStream::connect(self.interface_socket);
			if let Ok(mut interface_stream) = self.interface_stream.lock() {
				*interface_stream = Some(stream);
			}
			self.startup_complete.store(1, Ordering::Release);
		} else {
			if let Ok(mut interface_stream) = self.interface_stream.lock() {
				if let Some(ref mut stream) = interface_stream {
					//TODO read
					//TODO write
				}
			}
		}
		if let Ok(mut to_handle) = self.to_handle.lock() {
			to_handle.append(&mut to_handle);
		}
		true
	}
}

impl ApplicationMessageHandler for CustomMsgHandler {
	fn handle_header(&self, mut msg: msgs::BitcoinHeader) -> Result<(), LightningError> {
		if let Ok(mut to_send) = self.to_send.lock() {
			to_send.append(&mut msg.header);
		}
		Ok(())
	}
}

impl MessageSendEventsProvider for CustomMsgHandler {
	fn get_and_clear_pending_msg_events(&self) -> Vec<MessageSendEvent> {
		let mut msg_events = vec![];
		if let Ok(mut to_handle) = self.to_handle.lock() {
			loop {
				let set_size = to_handle.len();
				if set_size == 0 { return msg_events; }
				let to_handle_subset: Vec<BlockHeader> = Vec::new();
				let fetched_elems = if set_size < 818 { set_size } else { 818 };
				let to_handle_subset = to_handle.drain(0..fetched_elems).collect();
				let header_msg = msgs::BitcoinHeader {
					chain_hash: self.chain_hash,
					header: to_handle_subset,
				};

				msg_events.push(MessageSendEvent::BroadcastBitcoinHeader {
					msg: header_msg
				});
			}
		}
		msg_events
	}
}

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
use util::ser::{Readable, Writeable};

use std::io::prelude::*;
use std::net::{Ipv4Addr, TcpStream, SocketAddrV4};
use std::ops::DerefMut;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct CustomMsgHandler {
	chain_hash: BlockHash,
	to_validation: Mutex<Vec<BlockHeader>>,
	to_network: Mutex<Vec<BlockHeader>>,
	interface_socket: SocketAddrV4,
	interface_stream: Mutex<Option<TcpStream>>,
	startup_complete: AtomicUsize,
}

impl CustomMsgHandler {
	pub fn new(hash: BlockHash, port: u16) -> CustomMsgHandler {
		let socket = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), port);
		CustomMsgHandler {
			chain_hash: hash,
			to_validation: Mutex::new(vec![]),
			to_network: Mutex::new(vec![]),
			interface_socket: socket,
			interface_stream: Mutex::new(None),
			startup_complete: AtomicUsize::new(0),
		}
	}

	pub fn process_pending_events(&self) {

		let mut valid_headers: Vec<BlockHeader> = Vec::new();
		let mut pending_headers: Vec<BlockHeader> = Vec::new();
		if self.startup_complete.load(Ordering::Relaxed) == 0 {
			if let Ok(stream) = TcpStream::connect(self.interface_socket) {
				if let Ok(mut interface_stream) = self.interface_stream.lock() {
					*interface_stream = Some(stream);
				}
				self.startup_complete.store(1, Ordering::Release);
			}
		} else {
			if let Ok(mut to_validation) = self.to_validation.lock() {
				pending_headers.append(&mut to_validation.drain(0..818 * 80).collect());
			}
			if let Ok(ref mut stream) = self.interface_stream.lock() {
				if let Some(ref mut stream) = stream.deref_mut() {

					// read: (size) | (size * headers)
					let mut buf = [0; 8];
					let mut len = 0;
					if let Ok(_) = stream.read_exact(&mut buf) {
						len = u64::from_be_bytes(buf);
					}
					let mut headers_buf = Vec::with_capacity(len as usize * 80);
					if let Ok(_) = stream.read_exact(&mut headers_buf) {
						for _ in 0..len {
							if let Ok(h) = Readable::read(&mut headers_buf.as_slice()) {
								valid_headers.push(h);
							} else { panic!("read error CustomMsgHandler::process_pending_events"); }
						}
					}
					// write: (size) | (size * headers)
					let len = pending_headers.len();
					if let Err(_) = stream.write_all(&len.to_be_bytes()) { panic!("write error CustomMsgHandler::process_pending_events"); }
					for h in pending_headers {
						if let Err(_) = h.write(stream) { panic!("write error CustomMsgHandler::process_pending_events"); }
					}
				}
			}
		}
		if let Ok(mut to_network) = self.to_network.lock() {
			to_network.append(&mut valid_headers);
		}
	}
}

impl ApplicationMessageHandler for CustomMsgHandler {
	fn handle_header(&self, mut msg: msgs::BitcoinHeader) -> Result<(), LightningError> {
		if let Ok(mut to_validation) = self.to_validation.lock() {
			to_validation.append(&mut msg.header);
		}
		Ok(())
	}
}

impl MessageSendEventsProvider for CustomMsgHandler {
	fn get_and_clear_pending_msg_events(&self) -> Vec<MessageSendEvent> {
		let mut msg_events = vec![];
		if let Ok(mut to_network) = self.to_network.lock() {
			loop {
				let set_size = to_network.len();
				if set_size == 0 { return msg_events; }
				let to_network_subset: Vec<BlockHeader> = Vec::new();
				let fetched_elems = if set_size < 818 { set_size } else { 818 };
				let to_network_subset = to_network.drain(0..fetched_elems).collect();
				let header_msg = msgs::BitcoinHeader {
					chain_hash: self.chain_hash,
					header: to_network_subset,
				};

				msg_events.push(MessageSendEvent::BroadcastBitcoinHeader {
					msg: header_msg
				});
			}
		}
		msg_events
	}
}

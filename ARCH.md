Rust-Lightning is broken into a number of high-level structures with various APIs to hook
them together, as well as a number of APIs for you to provide external data which you,
the user, have to provide.

The two most important structures which nearly every application of Rust-Lightning will
need to use are ChannelManager and ChannelMonitor. ChannelManager holds multiple Channels,
routes payments between them and exposes a simple API to make and receive payments.
Individual ChannelMonitors monitor the on-chain state of a Channel and punish
counterparties if they misbehave, with the ManyChannelMonitor API providing a way for you
to receive ChannelMonitorUpdates and persist them to disk before the Channel steps forward.

Additional high-level structures that you may use are the Router (which handles receiving
channel and node announcements as well as calculating routes for sending payments) and
PeerManager (which handles the authenticated and encrypted communication protocol and
routes messages to/from a ChannelManager and Router instance as appropriate).

The ways each of the high-level structs communicate with each other is public, so that
you can easily add hooks in between and add additional special handling or modification
thereof. Further, APIs for key generation, transaction broadcasting, and block fetching
must be provided by you.

At a high level, some of the common interfaces fit together as follows:



                                    -----------------
                                    | KeysInterface |
                                    -----------------
                                    ^     --------------
         --------------------       |     | UserConfig |
    /----| MessageSendEvent |       |     --------------
   |     --------------------       |    /            ------------------------
   | (as MessageSendEventsProvider) |   /     ------> | BroadcasterInterface |
   |                     |          |  /     /\       ------------------------
   |                     ^          | v  ---/  \
   |                 ------------------ /      ----------------------
   |              ->-| ChannelManager |--->---| ManyChannelMonitor |
   v             /   ------------------       ----------------------
--------------- /        ^        (as EventsProvider)   ^
| PeerManager |-         |             \     /         /
---------------          |            --\---/----------
    |       -----------------------  /   \ /
    |       | ChainWatchInterface | -     v
    |       -----------------------   ---------
    |         |                       | Event |
     \        v                       ---------
      \   ----------
       -> | Router |
          ----------

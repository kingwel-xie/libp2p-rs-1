
Kad-DHT is used to provide the content based routing. It is the core component of libp2p.

In `rust-libp2p`, it is implemented in the well-known poll method, which is hard for developers to understand in a way. Furthermore, the implementation is quite different from the `go-libp2p-kad-dht` in many aspects, particularly in the k-bucket data structure and the implementation of iterative query. 

After reviewing both two implementations, we decided to:
- reuse the k-bucket data structure of `rust-libp2p`, but to re-design the entry insertion mechanism
- rewrite the iterative query, which will be running in a separate task, communicating with the Kad main loop via a channel

In addition, similar to Swarm, Kad main logic runs in a message loop, which handles all Kad request messages, commands from Kad-Control, events generated by all kinds of query, notifications from protocol handler, and so on.


### Main Loop & Controller
For exactly the same reason as Swarm, Kad runs a main loop for handling all kinds of messages/events. Similarly, a Kad Controller is exposed as the API interface. The main loop can be spawned as a task. On the other side, the controller is the API interface of Kad. Any Kad-DHT operations, such as bootstrap/find-peer, have to be initiated via the controller. In addition, the controller also provides `Dump` command to allow user to supervise the running status of Kad-DHT.

### KBuckets
No like `go-libp2p-kad-dht`, `rust-libp2p` uses a different approach to implement the k-bucket based routing table. It is pre-allocated, contains up to 256 buckets which individually contain up to k-value entries/nodes. There is a pending entry insertion mechanism in case that kbucket is full. Therefore, it maintains a least-connected list so that an 'oldest' entry can be replaced. 

We eventually choose to keep the k-bucket data structure, but to remove the pending insertion. As an alternative, each entry has its aliveness timestamp, which will be updated once upon the entry node is queried. An entry can be replaced by a new one when it is found to be too old, i.e., now() > aliveness + threshold. Meanwhile, Kad will run heath check periodically to remove entries whose aliveness is not fresh any longer.

### Iterative query

In `rust-libp2p`, the iterative query will terminate when up to k-value closer peers are discovered. However, refer to `go-libp2p-kad-dht`, we re-implement the iterative query which terminates when no closer peer is found in the continuous last beta-value rounds query, or there is no more peer to query. Any successful peer query will cause the peer to be added to the routing table, while any failed one will cause it to be removed from the routing table.

### Identified notifications

By applying the Notifiee trait implemented by Kad protocol handler, any peer identified as a qualified Kad peer(claims it supports Kad protocol) might be added to the routing table.  

### KadConnectionType

Kad maintains a hash table to hold all connected peers by listening to connected/disconnected notifications from the protocol handler. This is to provide the KadConnectionType as required by Kad protocol.

### Bootstrap & Refresh

Bootstrap is actually using refresh procedure for booting up Kad-DHT routing table. The refresh procedure includes two stages: self and random lookup. The latter means lookup the random peer Id which is supposed to be in the specific bucket. Currently, a simple algorithm to generate the random peer Id is so called best-effort which can not guarantee the generated Id fits into the bucket. This can be improved later.

A refresh timer is started whenever a refresh is completed, so that refresh will be re-run in a configurable period. Kad will run health check for all 'old' entries before starting a new round refresh.

### Interactive Cli

By using `xcli`, we create several cli commands for debugging Kad-DHT:

```no_run
bootstrap   : bootstrap     
add         : add [<peer>] [<multi_address>]
rm          : rm [<peer>] [<multi_address>]
dump        : dump          
messenger   : messengers    
stats       : dump stats    
findpeer    : findpeer <peerid>
getvalue    : getvalue <key>
```

All commands are using API to communicate with Kad. It is a powerful tool for debugging. Hopefully more commands will be added later.


### Statistics

Currently the API support exposing the basic statistics as below. It is far from enough, but better than nothing.

```no_run
pub struct KademliaStats {
    pub total_queries: usize,
    pub total_refreshes: usize,
    pub query: QueryStats,
    pub message_rx: MessageStats,
}
```

Cli command 'dht stats' will dump out the statistics.

### Misc.

`rust-libp2p` supports the extened publisher and expiry fields for provider. Probably we'll remove it in future, to keep compatible with `go-libp2p-kad-dht`. Besides, the reproviding mechanism might be removed as well, if we decide to create a standalone provide/re-provide mechanism.

The GetRecord/PutRecord is so-called as-is. We don't support the value format in ipfs, neither does `rust-libp2p`.  


## Getting started

See code in `examples/kad_simple.rs`, in which you will find out how to build the Swarm and Kad objects, and how to make them work.
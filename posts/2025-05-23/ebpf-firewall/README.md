---
slug: ebpf-firewall-post
title: How We Built an eBPF-based General Purpose Packet Filter
authors: [conectado]
tags: [ebpf, firewall, rust, aya]
draft: true
---

## Introduction

As part of our migration to multi-site in Firezone, the VPN and Firewall were rewritten in Rust. As part of that rewrite we decided to improve how we were handling the firewall.

In the past we were using [nftables](https://netfilter.org/projects/nftables/), the way we did this was shelling-out to the console and use the nft cli.

This has a number of problems, for one, shelling out can be prone to error, inefficient and hard to unit-test, also there's no Rust lib to call into nftables and even the existing libraries in C can be too complicated to use. Furthermore, we were limited to nft's data structure, which meant doing a lot of work to not mess up the rules, this in turn made doing improvements really slow.

So, after a lot of consideration, we decided that creating our own firewall using [ebpf](https://ebpf.io/).

## eBPF

We will briefly go into what eBPF is, however if you want a more detailed introduction you can take a look at [the official introduction](https://ebpf.io/what-is-ebpf).

### What is eBPF

eBPF stands for extended Berkeley Packet Filter, which is an upgrade from BPF (Berkeley Packet Filter), while eBPF brings a lot of improvements over BPF, the gist is the same, run user-defined packet-filtering code inside of the kernel as part of the packet processing pipeline.

This is an oversimplification because eBPF programs can be hooked into multiple different events in the kernel, but for our intent and purposes, we just want to hook into the packet-classifier pipeline.

### How the kernel safely runs user-defined code

eBPF has its own ISA and there is a runtime for said ISA in the kernel (JIT'd), the instructions allow calling into parts of the kernel that we might need and give us the necessary context to process a given packet.

Furthermore, to prevent crashes in the kernel and hogging out resources, before an eBPF program gets loaded into the kernel, it goes through a verification process, a verifier program sets a bunch of limitations to the program to prevent situations like infinite loops and crashes: 
* It sets an upper-bound for the number of times a loop within the code can run (or strictly prohibits them in older kernel versions).
* An upper-bound in the number of instructions the program can execute in total.
* Keeps track of read/writes to know that you are using valid memory locations.

To know the specifics of how it works, you should take a look at the [kernel's doc](https://www.kernel.org/doc/Documentation/networking/filter.txt) and even more in details you can take a look at [cillium's docs](https://docs.cilium.io/en/stable/bpf/#bpf-guide).

### Packet pipeline

Without getting into the specifics, an eBPF program can be hooked into different hooks on the packet processing pipeline for the linux kernel. eBPF allows hooking into [XDP](https://www.iovisor.org/technology/xdp) which runs just after the packet reached the NIC. However, [XDP doesn't support Wireguard](https://lore.kernel.org/netdev/20200813195816.67222-1-Jason@zx2c4.com/T/) since wireguard doesn't have L2 headers, for more info on this see [this fly.io blogpost](https://fly.io/blog/bpf-xdp-packet-filters-and-udp/#xdp).

Fret not, for eBPF can also hook into the network's [TC Ingress](https://man7.org/linux/man-pages/man8/tc-bpf.8.html) that doesn't make any assumption about the packet's header.

### Maps

Maps are the kernel structures that enable us to share data between the eBPF kernel program and a user-space program. Maps, as their name suggests, allow us to store relations as `Key -> Value` the types of maps offered by eBPF is limited however there are a lot of them. For our use case we just need to concern ourselves with [trie](https://github.com/torvalds/linux/blob/master/kernel/bpf/lpm_trie.c) and hash maps.

Again I refer to [cillium's documentation](https://docs.cilium.io/en/v1.12/bpf/#maps) for more information.

### TL;DR

eBPF is an ISA and its corresponding implementation for user-defined programs that run in the kernel that we used for filtering packets, however, the programs we can run have some severe limitations so that they don't crash the kernel.

## Firewall

With this in mind, we wanted to write a Firewall that can allow/deny traffic based on the following:
- Destination CIDR
- Possibly multiple sender IPs
- Possibly a port-range along with UDP, TCP or both

We took an additional simplification to make the implementation easier (we will see why further along), you can configure the firewall to allow all or deny all, then when a packet matches a rule it'd invert that behavior: 
> A firewall that denies all traffic with a rule for destination `10.20.30.40`, would deny all traffic from all sources to all destinations except for traffic to `10.20.30.40` from any destination. 

### Aya

When implementing the firewall in Rust, there are two possible libraries:
- [aya](https://github.com/aya-rs/aya)
- [redbpf](https://github.com/foniod/redbpf)
`redbpf` relies on bindings to `libbpf` while `aya` only relies on `libc`, also, aya's tooling seems to be easier to use and it uses cargo's target for ebpf instead of relying on `cargo bpf` which is easier when writing the Firewall as a library.

So, taking that into account, we went with `aya`.

Normally, the way an aya program is structured, we have a user-space crate that loads the eBPF program and is a proxy between it and the user and a crate that targets eBPF which has more limitations (such as being `no-std`).

Aya normally expects to be run as a binary and not as a library or as a separate userspace/bpf library, so we needed to do some tuning for the template to be able to run it as we wanted, a single library crate that you include.

More about how we did this in a separate blogpost.


### Architecture

** TODO: ** Add diagrams

So, with this information, we can plan an architecture.

Thinking about the steps of how ideally, the firewall would work:

1. A packet arrives
2. The source is classified into a `user_id` if any
3. Check existing rules for a match
4. Decide REJECT/ACCEPT action based on previous step

Let's go over how we can achieve this.

### The anatomy of a rule

As we said before, a rule has 3 components

- Associated `user_id`
- Destination CIDR
- Destination Port Ranges

The `user_id` is an arbitrary name, the idea is that a single rule can associate multiple source ips, so we can associate a rule with an `id` and then we can update the `id` association with multiple ips during runtime.

Rules need to be stored in eBPF's maps, that way we can update the maps during run-time from userspace to be able to update the rules.

And we need to be able to lookup those maps using information from the incoming packets.

The way we did this is having the following maps:

- `SOURCE_MAP`: A hashmap mapping source ips to `id`s
- `RULE_MAP`: A trie mapping CIDR to port ranges

** Note: ** There is actually an ipv6 and ipv4 version of both of those maps, so we have 4 maps in total.

### `SOURCE_MAP`

The `SOURCE_MAP` is really simple, we store the mapping IP -> ID, so we can associate let's say IP `10.10.10.3` and `10.10.10.4` with id `4` and `10.1.1.5` with id `7`. Now when a packet comes from `10.10.10.4` we lookup the map and get `4` and for `10.1.1.5` we get `7`.

### `RULE_MAP`

[Tries](https://en.wikipedia.org/wiki/Trie) are prefix-matching trees, they let us efficiently store and lookup ranges grouped by prefix. For ips this is exactly how [CIDR](https://en.wikipedia.org/wiki/Classless_Inter-Domain_Routing) works.

The nodes in the trie tree correspond to an ip and a prefix, so `10.5.8.0/24` corresponds to the bits of `10.5.8.0` and 24 bits so that node would match with `10.5.8.0` to `10.5.8.255` but if you store `10.5.8.128/25` that'd match with `10.5.8.128` to `10.5.8.255`.

Both nodes, `10.5.8.0/24` and `10.5.8.128/25`, could be stored but the `lookup` method in the `trie` only allows us to find the node with the most specific match. so `10.5.8.110` would match with `10.5.8.0/24` and `10.5.8.170` would match with `10.5.8.128/25` but we would never learn that it also matches with `10.5.8.0/24`.

So a trie would give us a way to associate CIDR with... something? The question is what's this "something".

Well, we don't need an action, since we know the action is just inverting the default one. What we still lack to know if the rule matches is a port-range (with its protocol) and the associated `id`. 

However, if we store multiple rules, all associated with the same destination IP we would need to lookup all the rules for each packet and this is incredibly inefficient. But there's a solution; we can easily store the ID and protocol in the key!

So instead of storing `10.5.8.0/24` as the key, and when we do lookup we get all the other data, we store `6.TCP.10.5.8.0` as the key and the prefix instead of 24 is 33, 4 extra bytes for the id, 1 for the protocol (we use the protocol number).

If this is still unclear we will see more when we discuss the specific of the code, just know that we store the id and protocol as part of the key, so we can compose the number `id.protocol.ip` with the prefix and look it up.

The result value will be the port range, which we have to map to the port of the incoming packet; we will talk more about how we do this below. 

For sources with no id we use `0` as the id. For non-TCP/UDP packets, we store the rule in both the TCP and UDP key and look up TCP(arbitrarily) but only match for port 0 (which we use to encode all ports), for rules with both protocols, we store the rule twice once for TCP and once for UDP.

### Another look at the ip matching

Let's look again at the steps when a packet arrives:

1. Packet arrives
2. Lookup destination IP to get an ID or use 0
3. Create the lookup key (ID.PROTOCOL.DESTINATION_IP)
4. Use the lookup key to get the port ranges
5. Lookup the port in the port ranges
6. REJECT/ACCEPT based on that

Basically, this is a high-level on how the ebpf side works. Now, let's take a look at the userspace-side.

### Userspace

Now that we know how we access the rules, let's see how we need to store them from userspace.

Remember that the Trie map provided by the kernel, for search, only has its lookup method that lets us know the longest prefix. So let's think about this scenario:

We store these rules:

```
10.0.0.0/24 port 80 Allowed
10.0.0.1/32 port 23 Allowed
```

now a packet comes from 10.0.0.1 with port 80, and we use the Trie lookup method to see if it's allowed, the lookup method returns all the ports from the longest prefix, meaning that port 23 is allowed but the packet has port 80 so it's denied.

This is, of course, erroneous because IP `10.0.0.1` is part of range `10.0.0.0/24` which has port 80 is allowed. So what do we do about this? Changing the trie API is impossible without modifying the kernel and even if we submit the PR for that the new API would only be included in the latest kernels.

We could lookup manually each range before deciding an action, so when packet with `10.0.0.1` comes we check `10.0.0.1/32` then `10.0.0.0/31` the `10.0.0.0/30` so on and so on before taking a decision. The performance of this is bad, specially if we are going to do that about each packet.

What we can do is present an API from userspace that when adding rules, it propagates them to all pertinent IPs, so:

If you have:

```
10.0.0.1/32 port 23 Allowed
```

and you want to add `10.0.0.0/24 port 80`, the API would automatically propagate it to:

```
10.0.0.1/32 port 23, 80 Allowed
10.0.0.0/24 port 80 Allowed
```

and if you have:

```
10.0.0.0/24 port 80 Allowed
```

then you add `10.0.0.1/32 port 23`, the API actually updates the rules like:

```
10.0.0.1/32 port 23, 80 Allowed
10.0.0.0/24 port 80 Allowed
```

So you see how the API automatically propagates rules to have a consistent table.

### Port Ranges


## At last, CODE!

With the previous section you should have a clear picture of the architecture of what we built and why, but from now on we will **need** to see code.

We will not go over how Aya's API or eBPF code works; hopefully it will be self-evident with the explanations so far to understand the gist of it.

### Code structure

We have 3 crates in our repo:
* Userspace
* Common (This contains the codebase shared by both)
* eBPF

From userspace we have the `Firewall` which is the entrypoint for everything. You first initialize and load a firewall by calling `Firewall::new` for a given interface.

The most interesting part is the `RuleTracker`; this is an internal struct (which is almost directly exposed by `Firewall` using pass-through methods), this is where the magic happens, userspace `Rule`s are converted into valid eBPF `Rule`, we keep track of them to propagate them and obviously add them to the maps in eBPF.

So let's see how that happens.

### A rule

#### eBPF

So, how does a rule look like in reality:

```rs
#[repr(C)]
pub struct RuleStore {
    // Sorted non-overlapping ranges
    // bit 0-15: port-start
    // bit 16-31: port-end
    rules: [u32; MAX_RULES],
    rules_len: u32,
}
```

Structs in eBPF need to be `repr(C)`, which just means it uses C's ABI; eBPF structs need to be a simple array of bytes (with no padding to convince the eBPF verifier that we never read uninitialized memory).

So, let's analyze this struct; this is the value that we are going to store in the map. This only needs to be the port-ranges (remember that the key already stores the `id`, `destination`, `protocol`).

So we store the rules as an array of port-ranges. With a range being the inclusive intervals of the port covered by the rule, we use the first 2 bytes for the starting port and the last 2 bytes for the ending port.

Then in `rules_len` we just have the number of rules stored (the rest is padded with 0).

#### Userspace

From userspace we have many different structs that represent a rule at different stages of processing; these are not complex but we have different parts of the code that require different information for what to do with the rule.

But for the user-facing API, a rule looks like:

```rs
pub enum Rule {
    V4(RuleImpl<Ipv4Net>),
    V6(RuleImpl<Ipv6Net>),
}
```

Alright, not very interesting without knowing how `RuleImpl` looks:

```rs
pub struct RuleImpl<T> {
    pub(crate) id: Option<u32>,
    pub(crate) dest: T,
    pub(crate) port_range: Option<PortRange>,
}
```

So we see that a rule in userspace contains all the information for a rule, but the important part is:

```rs
pub(crate) struct PortRange {
    pub(crate) ports: RangeInclusive<u16>,
    pub(crate) proto: Protocol,
}
```

Cool! We use Rust's `RangeInclusive<T>` to represent ports with `u16` as a port and a `Protocol` (which is just an enum to indicate the kind of rule).

So when we do `Firewall::add_rule(rule)` we need to make it arrive to eBPF with the struct we previously saw.

We also need to propagate correctly like we saw in the previous userspace architecture, so let's walk through how that works!

#### Translation

So at some point we want to add the `rule` to the eBPF map. For that, we first need to determine the key that we are going to use.

That's easy:

```rs
// Convert the id to big endian bytes
let key_id = id.to_be_bytes();

// Convert the ip to bytes
let key_cidr = ip.normalize().as_octets();
let mut key_data = [0u8; N];

// Store in a new `key_data`: protocol.id.ip
let (left_key_data, cidr) = key_data.split_at_mut(5);
let (id, prot) = left_key_data.split_at_mut(4);
prot[0] = proto;
id.copy_from_slice(&key_id);
cidr.copy_from_slice(key_cidr.as_ref());

// Calculate the prefix and return the eBPF key
Key::new(
    u32::from(ip.prefix()) + (left_key_data.len() * 8) as u32,
    key_data,
)
```

Cool, this is the key that we are going to insert to the eBPF map.

Now we need to calculate the value that we are going to insert.

Remember that the `rule` field in the eBPF `RuleStore` looked like this:

```rs
    rules: [u32; MAX_RULES],
```

So we have a maximum number of rules; one reason for this is that we need to store an array, we can't store a vector (we can't share heap allocations). But this number is also limited by the lookup algorithm.

Remember that the verifier imposes a limit on the number of times a loop can run, so if we linearly search all ranges we were getting capped at around 1000 intervals before we would reach the upper limit on loops; this is okay, this would be enough but we can do better.

If we can somehow sort the port ranges we could do binary search; this means fewer loop iterations and faster lookup in general. But how to sort it exactly?

This is actually the reason each rule doesn't have an Accept/Reject; instead it only inverts the default behavior, also why we store all UDP rules together and TCP rules together.

To sort, we just need to merge intervals. Merging intervals is a well studied problem that can be done in `O(N)` and since it's from userspace we could afford worse performance.

Merging intervals just means that from the existing intervals we create a series of new non-overlapping intervals that cover the same area and we sort them.

So `[10-20, 15-30, 6-9]` would become `[6-9, 10-30]`. Then we can do binary search on this interval. We will see below the specifics of that, but this is the code to get the intervals from a series of port ranges:


```rs
fn resolve_overlap(port_ranges: &mut [&PortRange<T>]) -> Vec<(u16, u16)>
{
    let mut res = Vec::new();
    
    // Sort the ranges by the starting port
    port_ranges.sort_by_key(|p| p.ports.ports.start());

    // Get the first port into the result
    if let Some(range) = port_ranges.first() {
        res.push((*range.ports.ports.start(), *range.ports.ports.end()));
    } else {
        return res;
    }

    // Go through each range
    for range in &port_ranges[1..] {
        let last_res = res
            .last_mut()
            .expect("should contain at least the first element of port_ranges");

        // If the new range falls within the last one, extend it
        if last_res.1 >= *range.ports.ports.start() {
            *last_res = (last_res.0, last_res.1.max(*range.ports.ports.end()));
        // Otherwise, add a new range
        } else {
            res.push((*range.ports.ports.start(), *range.ports.ports.end()));
        }
    }
    
    res
}
```

As you can see, doing this is really easy. We still need to do some small work to get it from `Vec<(u16, u16)>` to  `[u32; RULE_MAX]` but you can imagine it's pretty trivial.

### Propagating Rules

Then, the last interesting part from userspace is how to propagate the rules.

So when you insert a new rule, you need to keep track of all the rules that would include it, since when a lookup happens it would return the new rule because it's more specific. So basically the gist is to traverse all rules and include all the port ranges from those "above" in the new rule.

```rs
(The code here requires a lot of context on the specifics of how we did this but it's nothing interesting, check if you can rewrite it in a "context-free" way)
```

And then there could be more specific rules that are included in the new rule; you need to propagate the new rule (the port-range) to those.

```rs
(The code here requires a lot of context on the specifics of how we did this but it's nothing interesting, check if you can rewrite it in a "context-free" way)
```

### eBPF Rule lookup


When a packet arrives we first extract the network headers (source, destination, protocol and port).

```rs
    let (source, dest, proto) = load_ntw_headers(&ctx, version)?;
    let port = get_port(&ctx, version, proto)?;
    let class = source_class(source_map, source);
    let action = get_action(class, dest, rule_map, port, proto);
```

Then, we get the source id:

```rs
unsafe fn source_class<const N: usize>(
    source_map: &HashMap<[u8; N], u32>,
    address: [u8; N],
) -> Option<[u8; 4]> {
    source_map.get(&address).map(|x| u32::to_be_bytes(*x))
}
```
Then we try to calculate what action we should take:

```rs
fn get_action<const N: usize, const M: usize>(
    group: Option<[u8; 4]>,
    address: [u8; N],
    rule_map: &LpmTrie<[u8; M], RuleStore>,
    port: u16,
    proto: u8,
) -> i32 {
    let proto = if port == 0 { TCP } else { proto };
    let default_action = get_default_action();

    let rule_store = rule_map.get(&Key::new((M * 8) as u32, get_key(group, proto, address)));
    if is_stored(&rule_store, port) {
        return invert_action(default_action);
    }

    if group.is_some() {
        let rule_store = rule_map.get(&Key::new((M * 8) as u32, get_key(None, proto, address)));
        if is_stored(&rule_store, port) {
            return invert_action(default_action);
        }
    }

    default_action
}
```

Here we try to extract the action from the existing map; we get the `RuleStore` by constructing the key corresponding to that packet:

```rs
fn get_key<const N: usize, const M: usize>(
    group: Option<[u8; 4]>,
    proto: u8,
    address: [u8; N],
) -> [u8; M] {
    let group = group.unwrap_or_default();
    let mut res = [0; M];
    let (res_left, res_address) = res.split_at_mut(5);
    let (res_group, res_proto) = res_left.split_at_mut(4);
    res_group.copy_from_slice(&group);
    res_proto[0] = proto;
    res_address.copy_from_slice(&address);
    res
}
```
And most importantly, we lookup in the rulestore if the `port` that arrived exists.

```rs
    pub fn lookup(&self, val: u16) -> bool {
        // 0 means all ports
        if self.rules_len > 0 {
            // SAFETY: We know that rules_len < MAX_RULES
            if start(*unsafe { self.rules.get_unchecked(0) }) == 0 {
                return true;
            }
        }

        // We test for 0 in all non tcp/udp packets
        // it's worth returning early for those cases.
        if val == 0 {
            return false;
        }

        // Reimplementation of partition_point to satisfy verifier
        let mut size = self.rules_len as usize;
        // appeasing the verifier
        if size >= MAX_RULES {
            return false;
        }
        let mut left = 0;
        let mut right = size;
        #[cfg(not(feature = "user"))]
        let mut i = 0;
        while left < right {
            let mid = left + size / 2;

            // This can never happen but we need the verifier to believe us
            let r = if mid < MAX_RULES {
                // SAFETY: We are already bound checking
                *unsafe { self.rules.get_unchecked(mid) }
            } else {
                return false;
            };
            let cmp = start(r) <= val;
            if cmp {
                left = mid + 1;
            } else {
                right = mid;
            }
            size = right - left;

            #[cfg(not(feature = "user"))]
            {
                i += 1;
                // This should never happen, here just to satisfy verifier
                if i >= MAX_ITER {
                    return false;
                }
            }
        }

        if left == 0 {
            false
        } else {
            let index = left - 1;
            if index >= MAX_RULES {
                return false;
            }
            // SAFETY: Again, we are already bound checking
            end(*unsafe { self.rules.get_unchecked(index) }) >= val
        }
    }
```

If 0 is included we consider it to be "all ports".

Otherwise we basically do binary search on the starting port to see the maximum starting port that is less than the port we are checking for, and then we check for the ending port to see if it's more than the port we are checking.

Binary search for the maximum that keeps a predicate as true is an algorithm that already exists in Rust called partition point; we needed to reimplement it to make it compatible with eBPF due to the verifier limitations.

## Conclusion

eBPF is awesome but complicated! (something along those lines) and future for firezone rearchitecture and check it out!

//! The refusals for want of room and for asking too often. A refused call is a JSON-RPC error whose `data.refusal` names
//! the refusal. The OpenRPC document lists the codes of these two families under `x-arca-refusals`, one entry per code:
//! its `family`, a summary, and the schema of the error's `data`.
//!
//! `family` is what a caller checks. `capacity`: the call would pass a cap of a tenant or of the node. `rate`: the
//! caller asked too often, or already has too much waiting, and a wait ends it. The family `rate` has three codes:
//! `rate` itself, and `RateLimited` and `TooManyProposals`, the names the two proposal refusals have always had. Those
//! two are kept as they are, so a caller reads the family from the document instead of knowing three names.
//!
//! For `capacity`, `rate` and `RateLimited` the `data` holds fields beside the code: whose cap and which one, or how
//! long to wait. The list is not every refusal code a verb answers. A code outside it carries no field beside `refusal`
//! (and `untrusted`, for a refusal that quotes a service).

use serde_json::{Map, Value, json};

/// The call would pass a cap on room: instances, containers, virtual machines, CPUs, memory, disk, tenants or addresses.
pub const CAPACITY: &str = "capacity";
/// The caller asked too often, or too much of its own already waits: a wait ends it.
pub const RATE: &str = "rate";
/// The proposal window's refusal: one token recorded its thirty new proposals of ten minutes. An older name, kept.
pub const RATE_LIMITED: &str = "RateLimited";
/// Eight action proposals already wait. An older name, kept; it carries no field.
pub const TOO_MANY_PROPOSALS: &str = "TooManyProposals";

/// The two families of listed refusals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Capacity,
    Rate,
}

impl Family {
    /// The family as an entry's `family` carries it.
    pub fn as_str(self) -> &'static str {
        match self {
            Family::Capacity => "capacity",
            Family::Rate => "rate",
        }
    }
}

/// Whose cap a `capacity` refusal names: the tenant's own, or the node's. A call in a tenant can be refused by either,
/// for the same kind of cap, and only this tells the two apart: a tenant that is under its own caps can still be
/// refused because the node is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Whose {
    Tenant,
    Node,
}

impl Whose {
    /// Both, in the order the schema lists them.
    pub const ALL: [Whose; 2] = [Whose::Tenant, Whose::Node];

    /// The owner of the cap as `data.scope` carries it.
    pub fn as_str(self) -> &'static str {
        match self {
            Whose::Tenant => "tenant",
            Whose::Node => "node",
        }
    }
}

/// Which cap a `capacity` refusal names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    Instances,
    Containers,
    VirtualMachines,
    Cpu,
    Memory,
    Disk,
    Tenants,
    Addresses,
}

impl Limit {
    /// Every cap, in the order the schema lists them.
    pub const ALL: [Limit; 8] = [
        Limit::Instances,
        Limit::Containers,
        Limit::VirtualMachines,
        Limit::Cpu,
        Limit::Memory,
        Limit::Disk,
        Limit::Tenants,
        Limit::Addresses,
    ];

    /// The cap as `data.limit` carries it.
    pub fn as_str(self) -> &'static str {
        match self {
            Limit::Instances => "instances",
            Limit::Containers => "containers",
            Limit::VirtualMachines => "virtualMachines",
            Limit::Cpu => "cpu",
            Limit::Memory => "memory",
            Limit::Disk => "disk",
            Limit::Tenants => "tenants",
            Limit::Addresses => "addresses",
        }
    }
}

/// What a `capacity` refusal says beside its sentence: whose cap it is, which cap, and the amounts that are known.
/// `have` is what is in use, `want` what the call asked for and `max` what the cap allows. Memory and disk are bytes;
/// every other cap is a count. An amount that is not known, or that no 64-bit number holds, is left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Room {
    pub scope: Whose,
    pub limit: Limit,
    pub have: Option<u64>,
    pub want: Option<u64>,
    pub max: Option<u64>,
}

impl Room {
    /// A cap with all three amounts. One that does not fit 64 bits (an amount that could not be read) is left out.
    pub fn of(scope: Whose, limit: Limit, have: u128, want: u128, max: u128) -> Room {
        Room {
            scope,
            limit,
            have: u64::try_from(have).ok(),
            want: u64::try_from(want).ok(),
            max: u64::try_from(max).ok(),
        }
    }

    /// The fields the door writes beside `refusal`.
    pub fn fields(&self) -> Map<String, Value> {
        let mut fields = Map::new();
        fields.insert("scope".into(), json!(self.scope.as_str()));
        fields.insert("limit".into(), json!(self.limit.as_str()));
        for (key, amount) in [("have", self.have), ("want", self.want), ("max", self.max)] {
            if let Some(amount) = amount {
                fields.insert(key.into(), json!(amount));
            }
        }
        fields
    }
}

/// The field a `rate` refusal carries when the wait is known: whole seconds, rounded up, never 0.
pub fn retry_after(seconds: Option<u64>) -> Map<String, Value> {
    let mut fields = Map::new();
    if let Some(seconds) = seconds {
        fields.insert("retryAfterSeconds".into(), json!(seconds.max(1)));
    }
    fields
}

/// The seconds from `now_ms` until `at_ms`, rounded up.
pub fn seconds_until(at_ms: u64, now_ms: u64) -> u64 {
    at_ms.saturating_sub(now_ms).div_ceil(1000)
}

/// The field a listed refusal gains when it comes from running an approved action: `proposeAgain: true`. The approval
/// was spent by the run, so the same call cannot be made again: the action is proposed again and approved again.
pub const PROPOSE_AGAIN: &str = "proposeAgain";

/// One listed refusal: its code, its family, when a verb answers it, and the schema of the `data` its error carries.
pub struct RefusalCode {
    pub code: &'static str,
    pub family: Family,
    pub summary: &'static str,
    pub data: fn() -> Value,
}

/// The listed refusals, in the order the document lists them.
pub fn refusals() -> &'static [RefusalCode] {
    &[
        RefusalCode {
            code: CAPACITY,
            family: Family::Capacity,
            summary: "The call would pass a cap, and nothing was written. scope says whose cap: tenant (the tenant's own caps, or its address block) or node (the node's caps, its room for tenants, or its service subnet); a call in a tenant can be refused by either. limit names the cap: a count of instances, containers, virtual machines, CPUs, tenants or addresses, or an amount of memory or disk. have (in use), want (asked) and max (allowed) are there when known, in bytes for memory and disk and as a count for every other cap; with scope node they are the node's amounts, not the tenant's. proposeAgain: true means the refusal came from running an approved action: that approval is spent, so the action is proposed again once there is room.",
            data: capacity_data,
        },
        RefusalCode {
            code: RATE,
            family: Family::Rate,
            summary: "The caller asked too often, or too much of its own already waits: a wait ends it. retryAfterSeconds is there when the wait is known; where room can also be made sooner (a waiting request is decided), it is the longest the wait can be. After the wait the same call is allowed again, with one exception: proposeAgain: true means the refusal came from running an approved action, whose approval is spent, so after the wait the action is proposed again and approved again.",
            data: rate_data,
        },
        RefusalCode {
            code: RATE_LIMITED,
            family: Family::Rate,
            summary: "One token recorded its thirty new action proposals of ten minutes. The same kind of refusal as rate, under the name it has always had. retryAfterSeconds is the wait until the window has a free place.",
            data: rate_limited_data,
        },
        RefusalCode {
            code: TOO_MANY_PROPOSALS,
            family: Family::Rate,
            summary: "Eight action proposals already wait for the owner, from any token. The same kind of refusal as rate, under the name it has always had. It carries no field: room is made when one is approved or declined, or lapses fifteen minutes after it was made.",
            data: too_many_proposals_data,
        },
    ]
}

fn amount() -> Value {
    json!({ "type": "integer", "minimum": 0 })
}

fn propose_again() -> Value {
    json!({ "const": true })
}

fn capacity_data() -> Value {
    let scopes: Vec<&str> = Whose::ALL.iter().map(|scope| scope.as_str()).collect();
    let limits: Vec<&str> = Limit::ALL.iter().map(|limit| limit.as_str()).collect();
    json!({
        "type": "object",
        "required": ["refusal", "scope", "limit"],
        "properties": {
            "refusal": { "const": CAPACITY },
            "scope": { "type": "string", "enum": scopes },
            "limit": { "type": "string", "enum": limits },
            "have": amount(),
            "want": amount(),
            "max": amount(),
            PROPOSE_AGAIN: propose_again()
        }
    })
}

fn rate_data() -> Value {
    json!({
        "type": "object",
        "required": ["refusal"],
        "properties": {
            "refusal": { "const": RATE },
            "retryAfterSeconds": { "type": "integer", "minimum": 1 },
            PROPOSE_AGAIN: propose_again()
        }
    })
}

fn rate_limited_data() -> Value {
    json!({
        "type": "object",
        "required": ["refusal"],
        "properties": {
            "refusal": { "const": RATE_LIMITED },
            "retryAfterSeconds": { "type": "integer", "minimum": 1 }
        }
    })
}

fn too_many_proposals_data() -> Value {
    json!({
        "type": "object",
        "required": ["refusal"],
        "properties": {
            "refusal": { "const": TOO_MANY_PROPOSALS }
        }
    })
}

/// The `x-arca-refusals` list of the OpenRPC document.
pub(crate) fn document() -> Value {
    refusals()
        .iter()
        .map(|refusal| {
            json!({
                "code": refusal.code,
                "family": refusal.family.as_str(),
                "summary": refusal.summary,
                "data": (refusal.data)()
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::conforms;

    #[test]
    fn a_room_writes_whose_cap_which_cap_and_only_the_amounts_it_knows() {
        let all = Room::of(Whose::Node, Limit::Memory, 4 << 30, 4 << 30, 6 << 30).fields();
        assert_eq!(
            Value::Object(all.clone()),
            json!({ "scope": "node", "limit": "memory", "have": 4u64 << 30, "want": 4u64 << 30,
                "max": 6u64 << 30 })
        );
        // An amount that could not be read is counted as more than any cap; it is not written as a number.
        let unread = Room::of(Whose::Tenant, Limit::Cpu, 4, u128::MAX, 8).fields();
        assert_eq!(
            Value::Object(unread),
            json!({ "scope": "tenant", "limit": "cpu", "have": 4, "max": 8 })
        );
        let bare = Room {
            scope: Whose::Node,
            limit: Limit::Addresses,
            have: None,
            want: None,
            max: None,
        };
        assert_eq!(
            Value::Object(bare.fields()),
            json!({ "scope": "node", "limit": "addresses" })
        );
    }

    #[test]
    fn every_cap_and_owner_a_room_can_name_is_in_the_capacity_schema_and_both_are_required() {
        let schema = capacity_data();
        for scope in Whose::ALL {
            for limit in Limit::ALL {
                let mut data = Room::of(scope, limit, 1, 1, 1).fields();
                data.insert("refusal".into(), json!(CAPACITY));
                assert_eq!(
                    conforms(&schema, &Value::Object(data.clone())),
                    Ok(()),
                    "{scope:?} {limit:?}"
                );
                data.insert(PROPOSE_AGAIN.into(), json!(true));
                assert_eq!(conforms(&schema, &Value::Object(data)), Ok(()));
            }
        }
        for bad in [
            json!({ "refusal": CAPACITY, "scope": "node", "limit": "gpus" }),
            json!({ "refusal": CAPACITY, "scope": "project", "limit": "cpu" }),
            json!({ "refusal": CAPACITY, "scope": "node" }),
            json!({ "refusal": CAPACITY, "limit": "cpu" }),
            json!({ "refusal": CAPACITY, "scope": "node", "limit": "cpu", "proposeAgain": false }),
        ] {
            assert!(conforms(&schema, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_wait_is_whole_seconds_rounded_up_and_never_zero() {
        assert_eq!(seconds_until(61_001, 1_000), 61);
        assert_eq!(seconds_until(1_000, 5_000), 0);
        assert_eq!(
            Value::Object(retry_after(Some(0))),
            json!({ "retryAfterSeconds": 1 })
        );
        assert_eq!(Value::Object(retry_after(None)), json!({}));
        for (code, schema) in [(RATE, rate_data()), (RATE_LIMITED, rate_limited_data())] {
            let mut data = retry_after(Some(0));
            data.insert("refusal".into(), json!(code));
            assert_eq!(conforms(&schema, &Value::Object(data)), Ok(()), "{code}");
            assert_eq!(
                conforms(&schema, &json!({ "refusal": code })),
                Ok(()),
                "{code}"
            );
        }
    }

    #[test]
    fn the_listed_codes_are_distinct_and_the_old_names_are_of_the_rate_family() {
        let mut codes: Vec<&str> = refusals().iter().map(|refusal| refusal.code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), refusals().len());
        let family = |code: &str| {
            refusals()
                .iter()
                .find(|refusal| refusal.code == code)
                .map(|refusal| refusal.family)
        };
        assert_eq!(family(CAPACITY), Some(Family::Capacity));
        for code in [RATE, RATE_LIMITED, TOO_MANY_PROPOSALS] {
            assert_eq!(family(code), Some(Family::Rate), "{code}");
        }
        assert_eq!(
            conforms(
                &too_many_proposals_data(),
                &json!({ "refusal": TOO_MANY_PROPOSALS })
            ),
            Ok(())
        );
    }
}

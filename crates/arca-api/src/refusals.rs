//! The refusals that carry fields beside their code. A refused call is a JSON-RPC error whose `data.refusal` names the
//! refusal. For the codes listed here `data` holds more: what ran out, or how long to wait. The OpenRPC document lists
//! them under `x-arca-refusals`, one entry per code with the schema of its `data`.
//!
//! Two families are listed. `capacity`: the call would pass a cap of a tenant or of the node. `rate`: the caller asked
//! too often, or already has too much waiting, and the same call is allowed again after a wait. `RateLimited` is the
//! name the proposal window's refusal has always had; it is kept, and it carries the same field as `rate`.
//!
//! The list is not every refusal code a verb answers. A code outside it carries no field beside `refusal` (and
//! `untrusted`, for a refusal that quotes a service).

use serde_json::{Map, Value, json};

/// The call would pass a cap on room: instances, containers, virtual machines, CPUs, memory, disk, tenants or addresses.
pub const CAPACITY: &str = "capacity";
/// The caller asked too often, or too much of its own already waits: the same call is allowed again after a wait.
pub const RATE: &str = "rate";
/// The proposal window's refusal: one token recorded its thirty new proposals of ten minutes. An older name, kept.
pub const RATE_LIMITED: &str = "RateLimited";

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

/// What a `capacity` refusal says beside its sentence: the cap, and the amounts that are known. `have` is what is in use,
/// `want` what the call asked for and `max` what the cap allows. Memory and disk are bytes; every other cap is a count.
/// An amount that is not known, or that no 64-bit number holds, is left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Room {
    pub limit: Limit,
    pub have: Option<u64>,
    pub want: Option<u64>,
    pub max: Option<u64>,
}

impl Room {
    /// A cap with all three amounts. One that does not fit 64 bits (an amount that could not be read) is left out.
    pub fn of(limit: Limit, have: u128, want: u128, max: u128) -> Room {
        Room {
            limit,
            have: u64::try_from(have).ok(),
            want: u64::try_from(want).ok(),
            max: u64::try_from(max).ok(),
        }
    }

    /// The fields the door writes beside `refusal`.
    pub fn fields(&self) -> Map<String, Value> {
        let mut fields = Map::new();
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

/// One listed refusal: its code, when a verb answers it, and the schema of the `data` its error carries.
pub struct RefusalCode {
    pub code: &'static str,
    pub summary: &'static str,
    pub data: fn() -> Value,
}

/// The listed refusals, in the order the document lists them.
pub fn refusals() -> &'static [RefusalCode] {
    &[
        RefusalCode {
            code: CAPACITY,
            summary: "The call would pass a cap of a tenant or of the node: a count of instances, containers, virtual machines, CPUs, tenants or addresses, or an amount of memory or disk. Nothing was written. limit names the cap; have (in use), want (asked) and max (allowed) are there when known, in bytes for memory and disk and as a count for every other cap.",
            data: capacity_data,
        },
        RefusalCode {
            code: RATE,
            summary: "The caller asked too often, or too much of its own already waits: the same call is allowed again after a wait. retryAfterSeconds is there when the wait is known; where room can also be made sooner (a waiting request is decided), it is the longest the wait can be.",
            data: rate_data,
        },
        RefusalCode {
            code: RATE_LIMITED,
            summary: "One token recorded its thirty new action proposals of ten minutes. The same kind of refusal as rate, under the name it has always had. retryAfterSeconds is the wait until the window has a free place.",
            data: rate_limited_data,
        },
    ]
}

fn amount() -> Value {
    json!({ "type": "integer", "minimum": 0 })
}

fn capacity_data() -> Value {
    let limits: Vec<&str> = Limit::ALL.iter().map(|limit| limit.as_str()).collect();
    json!({
        "type": "object",
        "required": ["refusal", "limit"],
        "properties": {
            "refusal": { "const": CAPACITY },
            "limit": { "type": "string", "enum": limits },
            "have": amount(),
            "want": amount(),
            "max": amount()
        }
    })
}

fn rate_data_for(code: &str) -> Value {
    json!({
        "type": "object",
        "required": ["refusal"],
        "properties": {
            "refusal": { "const": code },
            "retryAfterSeconds": { "type": "integer", "minimum": 1 }
        }
    })
}

fn rate_data() -> Value {
    rate_data_for(RATE)
}

fn rate_limited_data() -> Value {
    rate_data_for(RATE_LIMITED)
}

/// The `x-arca-refusals` list of the OpenRPC document.
pub(crate) fn document() -> Value {
    refusals()
        .iter()
        .map(|refusal| {
            json!({
                "code": refusal.code,
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
    fn a_room_writes_its_cap_and_only_the_amounts_it_knows() {
        let all = Room::of(Limit::Memory, 4 << 30, 4 << 30, 6 << 30).fields();
        assert_eq!(
            Value::Object(all.clone()),
            json!({ "limit": "memory", "have": 4u64 << 30, "want": 4u64 << 30, "max": 6u64 << 30 })
        );
        // An amount that could not be read is counted as more than any cap; it is not written as a number.
        let unread = Room::of(Limit::Cpu, 4, u128::MAX, 8).fields();
        assert_eq!(
            Value::Object(unread),
            json!({ "limit": "cpu", "have": 4, "max": 8 })
        );
        let bare = Room {
            limit: Limit::Addresses,
            have: None,
            want: None,
            max: None,
        };
        assert_eq!(
            Value::Object(bare.fields()),
            json!({ "limit": "addresses" })
        );
    }

    #[test]
    fn every_cap_a_room_can_name_is_in_the_capacity_schema() {
        let schema = capacity_data();
        for limit in Limit::ALL {
            let mut data = Room::of(limit, 1, 1, 1).fields();
            data.insert("refusal".into(), json!(CAPACITY));
            assert_eq!(conforms(&schema, &Value::Object(data)), Ok(()), "{limit:?}");
        }
        assert!(conforms(&schema, &json!({ "refusal": CAPACITY, "limit": "gpus" })).is_err());
        assert!(conforms(&schema, &json!({ "refusal": CAPACITY })).is_err());
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
    fn the_listed_codes_are_distinct() {
        let mut codes: Vec<&str> = refusals().iter().map(|refusal| refusal.code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), refusals().len());
    }
}

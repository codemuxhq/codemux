//! Cross-cutting primitives shared by every component crate.
//!
//! Per AD-19, this crate carries IDs only. Zero vendor deps, zero business
//! logic, zero error types. If something domain-shaped drifts in, it is
//! mis-filed and belongs in a specific component.
//!
//! IDs wrap `Arc<str>` so clones are refcount bumps, not heap allocations —
//! the TUI event loop passes IDs through channels constantly.

use std::convert::Infallible;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

macro_rules! newtype_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Arc<str>);

        impl $name {
            #[must_use]
            pub fn new(value: impl Into<Arc<str>>) -> Self {
                Self(value.into())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = Infallible;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Arc::from(s)))
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(Arc::from(value))
            }
        }
    };
}

newtype_id!(HostId);
newtype_id!(AgentId);
newtype_id!(GroupId);

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn display_matches_inner_string() {
        assert_eq!(HostId::new("devpod-uber-1").to_string(), "devpod-uber-1");
    }

    #[test]
    fn as_str_matches_inner_string() {
        assert_eq!(AgentId::new("feed").as_str(), "feed");
    }

    #[test]
    fn from_str_is_infallible() {
        let id: HostId = "laptop".parse().unwrap();
        assert_eq!(id, HostId::new("laptop"));
    }

    #[test]
    fn from_string_matches_new() {
        assert_eq!(GroupId::from(String::from("x")), GroupId::new("x"));
    }

    #[test]
    fn ids_are_hashable_and_equal_by_value() {
        let mut m: HashMap<AgentId, i32> = HashMap::new();
        m.insert(AgentId::new("alpha"), 1);
        assert_eq!(m.get(&AgentId::new("alpha")), Some(&1));
    }

    #[test]
    fn clone_shares_the_underlying_string() {
        let a = AgentId::new("shared");
        let b = a.clone();
        assert_eq!(a, b);
        assert_eq!(a.as_str().as_ptr(), b.as_str().as_ptr());
    }

    #[test]
    fn id_types_are_distinct() {
        fn takes_host(_: HostId) {}
        fn takes_agent(_: AgentId) {}
        takes_host(HostId::new("h"));
        takes_agent(AgentId::new("a"));
    }
}

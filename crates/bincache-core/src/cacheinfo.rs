//! `GET /nix-cache-info`, the three lines a client reads before it trusts anything else.
//!
//! Lines are `Key: Value`, exactly as `nix/src/libstore/binary-cache-store.cc` parses them.

use swrite::SWrite as _;

/// Whether the cache invites the bulk narinfo queries that closure resolution produces.
/// An enum rather than a bool because the wire spelling is `1` / `0`, and because a bare
/// `true` at a call site says nothing about which question it answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MassQuery {
    Wanted,
    Unwanted,
}

impl MassQuery {
    const fn digit(self) -> &'static str {
        match self {
            Self::Wanted => "1",
            Self::Unwanted => "0",
        }
    }
}

/// Lower priority wins when several configured caches hold the same path.
/// `cache.nixos.org` sits at 40.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Priority(pub u32);

#[derive(Clone, Debug)]
pub struct CacheInfo {
    pub store_dir: crate::storepath::Dir,
    pub mass_query: MassQuery,
    pub priority: Priority,
}

impl CacheInfo {
    #[must_use]
    pub fn render(&self) -> String {
        let mut body = String::with_capacity(64);
        swrite::swriteln!(body, "StoreDir: {}", self.store_dir);
        swrite::swriteln!(body, "WantMassQuery: {}", self.mass_query.digit());
        swrite::swriteln!(body, "Priority: {}", self.priority.0);
        body
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn renders_the_three_lines() {
        let info = crate::cacheinfo::CacheInfo {
            store_dir: crate::storepath::Dir::new("/nix/store".to_owned()).expect("absolute"),
            mass_query: crate::cacheinfo::MassQuery::Wanted,
            priority: crate::cacheinfo::Priority(30),
        };
        expect_test::expect![[r#"
            StoreDir: /nix/store
            WantMassQuery: 1
            Priority: 30
        "#]]
        .assert_eq(&info.render());
    }
}

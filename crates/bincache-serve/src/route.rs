//! Parse, don't validate, at the socket.
//!
//! The request target is decoded into typed values here, before anything downstream runs.
//! A malformed key dies with a `400` rather than reaching a lookup, and everything past
//! this point holds a fixed-width key that is correct by construction. On a public
//! endpoint the decode doubles as input validation.

use snafu::OptionExt as _;
use snafu::ResultExt as _;

/// The three URL shapes a binary cache answers, plus the operator endpoint.
pub const CACHE_INFO: &str = "/nix-cache-info";
pub const METRICS: &str = "/metrics";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    CacheInfo,
    Metrics,
    Narinfo(bincache_core::storepath::Hash),
    Nar(bincache_core::narurl::NarUrl),
}

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("request target {target:?} is not absolute"))]
    Relative { target: String },

    #[snafu(display("request target {target:?} matches no route this cache serves"))]
    Unknown { target: String },

    #[snafu(display("the narinfo key is not a store path hash"))]
    Key { source: bincache_core::storepath::Error },

    #[snafu(display("the NAR name is malformed"))]
    Nar { source: bincache_core::narurl::Error },
}

/// Resolves a request target. The query string is dropped: nix appends store parameters
/// like `?compression=none` to the *store URI*, not to the paths it then requests, and no
/// route here varies on one.
pub fn resolve(target: &str) -> Result<Route, Error> {
    let path = target.split(['?', '#']).next().unwrap_or(target);
    let path = path.strip_prefix('/').context(RelativeSnafu { target })?;

    if path == CACHE_INFO.trim_start_matches('/') {
        return Ok(Route::CacheInfo);
    }
    if path == METRICS.trim_start_matches('/') {
        return Ok(Route::Metrics);
    }
    if let Some(name) = path.strip_prefix(bincache_core::narurl::PREFIX) {
        let url = bincache_core::narurl::NarUrl::parse(name).context(NarSnafu)?;
        return Ok(Route::Nar(url));
    }
    if let Some(key) = path.strip_suffix(bincache_core::narinfo::SUFFIX) {
        let hash = bincache_core::storepath::Hash::parse(key).context(KeySnafu)?;
        return Ok(Route::Narinfo(hash));
    }
    UnknownSnafu { target }.fail()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    const KEY: &str = "5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j";

    #[test]
    fn resolves_the_three_protocol_routes() {
        assert_eq!(
            crate::route::resolve("/nix-cache-info").expect("resolves"),
            crate::route::Route::CacheInfo
        );

        let narinfo = crate::route::resolve(&format!("/{KEY}.narinfo")).expect("resolves");
        let expected = bincache_core::storepath::Hash::parse(KEY).expect("parses");
        assert_eq!(narinfo, crate::route::Route::Narinfo(expected));

        let digest = bincache_core::hash::Sha256::digest(b"nar");
        let name = format!("/nar/{}.nar.zst", digest.base32());
        let nar = crate::route::resolve(&name).expect("resolves");
        assert_eq!(
            nar,
            crate::route::Route::Nar(bincache_core::narurl::NarUrl {
                file_hash: digest,
                compression: bincache_core::compression::Compression::Zstd,
            })
        );
    }

    #[test]
    fn drops_a_query_string() {
        let resolved = crate::route::resolve("/nix-cache-info?whatever=1").expect("resolves");
        assert_eq!(resolved, crate::route::Route::CacheInfo);
    }

    /// The point of decoding at the socket: garbage never reaches a lookup.
    #[test]
    fn refuses_a_key_that_is_not_a_store_path_hash() {
        let short = crate::route::resolve("/abc.narinfo");
        assert!(matches!(short, Err(crate::route::Error::Key { .. })));

        let dropped = crate::route::resolve("/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee.narinfo");
        assert!(matches!(dropped, Err(crate::route::Error::Key { .. })));
    }

    #[test]
    fn refuses_targets_it_does_not_serve() {
        assert!(matches!(
            crate::route::resolve("/log/whatever.drv"),
            Err(crate::route::Error::Unknown { .. })
        ));
        assert!(matches!(
            crate::route::resolve(&format!("/{KEY}.ls")),
            Err(crate::route::Error::Unknown { .. })
        ));
        assert!(matches!(
            crate::route::resolve("nix-cache-info"),
            Err(crate::route::Error::Relative { .. })
        ));
    }

    /// The `PUT` target for an uncompressed upload resolves to the same route shape, which
    /// is what lets the handler read the expected NAR hash straight out of the URL.
    #[test]
    fn resolves_an_uncompressed_upload_target() {
        let digest = bincache_core::hash::Sha256::digest(b"nar");
        let resolved =
            crate::route::resolve(&format!("/nar/{}.nar", digest.base32())).expect("resolves");
        assert_eq!(
            resolved,
            crate::route::Route::Nar(bincache_core::narurl::NarUrl {
                file_hash: digest,
                compression: bincache_core::compression::Compression::None,
            })
        );
    }
}

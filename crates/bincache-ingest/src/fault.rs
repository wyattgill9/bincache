//! Whose mistake a refused push describes.

/// Whether a refusal blames the request or the server that received it.
///
/// This decides two things at the wire boundary, and it has to be a property of the error
/// type rather than a judgement made at the call site, because only the owner of a variant
/// knows which it is.
///
/// The first is the status. A pre-compressed upload and a failed `fsync` are both reasons
/// to refuse, but answering `400` to the second tells a build node to stop retrying
/// something that would succeed on the next attempt.
///
/// The second is whether the reason may be sent. `nix copy` shows the pusher a status code,
/// so a client-fault reason left in the server log reaches nobody who can act on it. A
/// server-fault reason names paths inside the data directory and stays in the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    Client,
    Server,
}

//! Negotiated-capability profile for a connected client.
//!
//! Every wire divergence DbSaci makes is decided the way a real Oracle server
//! decides it: from what the client negotiated in the TTC handshake — the
//! `TNS_CCAP_*` / `TNS_RCAP_*` capability vectors in the DataTypes request and
//! the Protocol-message version list. The client's driver-name banner is never
//! consulted. (The `DE AD BE EF` NA exchange is tracked only so its response
//! can be sent — OCI, ojdbc and ODP.NET all run it, so it identifies nothing.)

use crate::wire::ProtocolRequest;

// --- TNS_CCAP_* compile-capability array indices (from python-oracledb's
//     `impl/thin/constants.pxi`; a real 19c/23c client fills the same array). ---

/// `TNS_CCAP_FIELD_VERSION`: the negotiated TTC field version. Drives the
/// row-metadata / column-describe layout and the end-of-call framing.
/// Values seen: 6 = 11.2, 12 = 19.1, 14 = 20.1, 24 = 23.4 (the current maximum).
const CCAP_FIELD_VERSION: usize = 7;

/// `TNS_CCAP_OCI1` — the first OCI-client capability register. The thick OCI
/// client saturates it (`0xff`); every thin driver (python-oracledb, ojdbc,
/// ODP.NET, oracle-rs) sends a fixed conservative `0x90`. This — not the NA /
/// "Secure Network Services" negotiation, which ojdbc and ODP.NET also run — is
/// what identifies the OCI thick client and its TTC dialect (little-endian
/// fixed-width integers, the `_oci` describe/row/end-of-call builders, OCI
/// auth framing).
const CCAP_OCI1: usize = 16;
const CCAP_OCI1_THICK: u8 = 0xff;

/// `TNS_CCAP_FEATURE_BACKPORT2`. Non-zero once the client implements the 19c+
/// response-completion protocol (`END_OF_RESPONSE` data-flag bit + an explicit
/// `ORA-01403` end-of-fetch + the extended end-of-call trailer). oracle-rs
/// 0.1.7 leaves it zero and runs its fetch loop off a hardcoded "no more rows",
/// so it neither needs nor tolerates those completion signals.
const CCAP_FEATURE_BACKPORT2: usize = 45;

/// `TNS_CCAP_FIELD_VERSION_20_1` — the threshold at which a client switches from
/// the compact thin describe to the newer describe/row/end-of-call shape.
/// ojdbc 8 (21.x) negotiates exactly this; ojdbc 11 (23.x) and ODP.NET
/// negotiate 23.4 (24) and use the same framing, so the test is `>=`.
const FIELD_VERSION_20_1: u8 = 14;

/// What the connected client negotiated. Construct once, at the end of the TTC
/// handshake, and pass it (or the derived predicates) to every wire builder.
#[derive(Debug, Clone, Default)]
pub struct WireProfile {
    /// Highest TTC protocol version the client offered, plus its descending
    /// list of also-accepted versions (Protocol message). The only handshake
    /// signal available *before* the capability vectors arrive; used solely to
    /// pick the Protocol *response* shape (`did_na && accepted == [5]` is the
    /// OCI thick client).
    pub protocol_version: u8,
    pub accepted_versions: Vec<u8>,
    /// The client ran the ANO / NA ("Secure Network Services", `DE AD BE EF`)
    /// negotiation after ACCEPT. NOT an OCI signal — ojdbc and ODP.NET run it
    /// too (they can do Kerberos / wallet auth); only python-oracledb thin and
    /// oracle-rs set `TNS_NSI_DISABLE_NA`. Kept because the NA response must be
    /// sent the moment that packet arrives.
    pub did_na_negotiation: bool,
    compile_caps: Vec<u8>,
    #[allow(dead_code)]
    runtime_caps: Vec<u8>,
}

impl WireProfile {
    pub fn new(
        did_na_negotiation: bool,
        proto: &ProtocolRequest,
        compile_caps: Vec<u8>,
        runtime_caps: Vec<u8>,
    ) -> Self {
        Self {
            protocol_version: proto.version,
            accepted_versions: proto.accepted_versions.clone(),
            did_na_negotiation,
            compile_caps,
            runtime_caps,
        }
    }

    fn ccap(&self, i: usize) -> u8 {
        self.compile_caps.get(i).copied().unwrap_or(0)
    }

    /// The negotiated `TNS_CCAP_FIELD_VERSION`.
    pub fn field_version(&self) -> u8 {
        self.ccap(CCAP_FIELD_VERSION)
    }

    /// This is the OCI thick client — it speaks the OCI TTC dialect
    /// (little-endian fixed-width integers, the `_oci` describe/row/
    /// end-of-call builders, OCI auth framing). Identified by the elevated
    /// `TNS_CCAP_OCI1` register that no thin driver sends. Requires the caps
    /// vector; before it arrives (Protocol phase) use
    /// [`Self::probably_oci_at_protocol`].
    pub fn oci_dialect(&self) -> bool {
        self.ccap(CCAP_OCI1) == CCAP_OCI1_THICK
    }

    /// Best guess at "OCI thick client" during the Protocol phase, before the
    /// capability vectors are available: it ran NA negotiation (so it is one of
    /// OCI / ojdbc / ODP.NET) and offered exactly protocol version 5 as its
    /// fallback list (ojdbc sends the full `5,4,3,2,1`; ODP.NET sends none).
    pub fn probably_oci_at_protocol(&self) -> bool {
        self.did_na_negotiation && self.accepted_versions == [5]
    }

    /// The client uses the newer row / describe / end-of-call wire shape (both
    /// ojdbc thin and ODP.NET managed do): the describe is a bare
    /// column-descriptor array with no leading chunk / max-row-size, the
    /// end-of-call is the short `0x04` form, and there is no `0x1d` marker.
    /// Clients that negotiate field version 20.1 or newer use it; 19.1 and
    /// earlier use the compact thin describe. (OCI has its own dialect — see
    /// [`Self::oci_dialect`].)
    pub fn newer_describe_framing(&self) -> bool {
        !self.oci_dialect() && self.field_version() >= FIELD_VERSION_20_1
    }

    /// The client needs explicit response-completion signals: the
    /// `END_OF_RESPONSE` (`0x2000`) data-flag on the final packet, an explicit
    /// `ORA-01403 no data found` at end of fetch, and the extended end-of-call
    /// trailer. Every modern client does; oracle-rs 0.1.7 does not, and would
    /// mis-frame them.
    pub fn wants_response_completion(&self) -> bool {
        self.oci_dialect()
            || self.field_version() >= FIELD_VERSION_20_1
            || self.ccap(CCAP_FEATURE_BACKPORT2) != 0
    }

    /// The client ran the ANO / NA exchange **and** sent an empty
    /// protocol-version fallback list in its Protocol message. That pair is a
    /// distinct point in the negotiation space: OCI runs NA and offers `[5]`,
    /// ojdbc (8 and 11) run NA and offer `[5,4,3,2,1]`, and the drivers that
    /// send an empty list (python-oracledb thin, oracle-rs) do not run NA.
    /// Field version alone cannot separate it — ojdbc 11 (23.x) negotiates 23.4
    /// too.
    ///
    /// A client at this point needs the shorter DataTypes-negotiation response
    /// and the long-form (1-byte-chunk) key/value framing in phase-two auth.
    /// The wire builders selected by this predicate carry those bytes; they were
    /// derived from the observable handshake of ODP.NET managed, the
    /// implementation known to land at this negotiation point.
    pub fn na_without_version_list(&self) -> bool {
        self.did_na_negotiation && self.accepted_versions.is_empty()
    }
}

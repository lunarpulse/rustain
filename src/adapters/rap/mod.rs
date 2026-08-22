mod key_store;
mod peer_delivery;
mod topic;
mod transport;
mod wire;

pub use key_store::{IdentityKeyStore, KeyStoreError};
pub use peer_delivery::{
    FrameSettlement, MAX_PEER_MESSAGE_BYTES, PeerDeliveryError, TOPIC_FRAME_TTL_MS,
    TOPIC_RECIPIENT_SUFFIX, TOPIC_SENDER_SUFFIX, TopicEffects, VerifiedPeerConsent,
    VerifiedPeerConsumer, VerifiedPeerFrameHandler, recipient_rooted_at, topic_frame,
    translate_verified_peer_envelope,
};
pub use topic::{
    MAX_HEADS_PER_FRAME, PeerTopicStore, TOPIC_HEAD_DOMAIN, TopicAdmission, TopicDivergence,
    TopicError, TopicGossip, advance_head,
};
pub use transport::RapTransport;
pub use wire::{
    ATTACH_PROOF_DOMAIN, AgentSigner, RAP_DOMAIN, ReplayReservation, ReplayWindow, VerifyError,
    attach_proof_transcript, entry_hash, sign_envelope, verify_attach_proof, verify_envelope,
    verify_envelope_reserved,
};

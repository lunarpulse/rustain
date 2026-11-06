//! The keymap registry (Story 21.3): one table of bindings that drives key
//! resolution for the leader, the key decoding contract and every discovery
//! surface (help, which-key, palette, status hints, docs). Pure adapter-side
//! module: no I/O.

pub mod govern;
pub mod model;
pub mod render;
pub mod render_docs;
pub mod render_help;
pub mod render_tips;
pub mod resolve;
pub mod table;
pub mod table_card;
pub mod table_chat;
pub mod table_input;
pub mod table_inspector;
pub mod table_nav;
pub mod table_overlay;
pub mod table_panels;
pub mod table_permission;
pub mod table_tool;
pub mod table_viewer;
pub mod text_edit;

pub use model::{
    AvailCtx, Availability, Binding, CommandId, Context, Help, HelpCat, Host, Hosts, Key, Reach,
    bind,
};
pub use resolve::{LeaderStep, Resolved, Violation, leader_step, resolve, table, validate};

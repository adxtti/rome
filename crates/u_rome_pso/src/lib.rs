//! PSIN / PSO data documents (PSO v122): section parsing, schemas, instance
//! data, hash name resolution and JSON export.
//!
//! # Format
//!
//! A document is a chain of sections, each an ASCII tag plus a big-endian u32
//! size that includes the 8-byte header; bodies are little-endian.
//!
//! - Main layout: `PSIN -> LSCH -> LENM -> LSNS -> LGID -> LRSC -> CHKS`
//!   (all 1,012 files of script.rpf). Fully decoded.
//! - Localisation layout: `PSIN -> PMAP -> PSCH -> PSIG -> STDN|STFN -> STNS
//!   -> CHKS`. Sections are walked and kept opaque ([`Layout::Pmap`]).
//!
//! Unknown sections are always kept and exposed through
//! [`PsoDocument::section_body`].
//!
//! Sources: rage9-re `docs/SCRIPT_FORMAT.md` (MIT OR Apache-2.0),
//! rage-tools-vi `docs/data/script-schema.md` (MIT), GTA6-Asset-Info
//! `formats/08-meta.md`, `formats/17-other.md`,
//! `structure/04-pso-objectgraph.md` (GPL documentation, facts only; no code
//! copied).
//!
//! # Usage
//!
//! ```no_run
//! use u_rome_pso::{HashMapNames, PsoDocument, to_json_with_schema};
//!
//! # fn main() -> Result<(), u_rome_pso::PsoError> {
//! let bytes = std::fs::read("doc.pso").unwrap_or_default();
//! let doc = PsoDocument::parse(&bytes)?;
//! for s in doc.sections() {
//!     println!("{} at {:#x}, {} bytes", s.tag_str(), s.offset, s.size);
//! }
//! let root = doc.root()?;
//! let names = HashMapNames::with_known_names();
//! let json = to_json_with_schema(&root, &names, Some(doc.schema()));
//! println!("{json:#}");
//! # Ok(())
//! # }
//! ```
//!
//! # Provisional facts
//!
//! Where sources disagree the crate picks one reading and exposes a flag:
//! - [`TypeInfo::provisional`] for type codes whose layout is inferred
//!   (`0x14` may hold f32 bits, `0x44` strings, `0x52`, `0x62`, `0x65`,
//!   `0x71`, `0x75`, `0x31`, `0x4f`).
//! - [`RootInfo::provisional`] when the root block was not taken from
//!   `LRSC +0x10` (GTA6-Asset-Info reads `+0x3A` as the root; rage9-re as the
//!   enum count).
//! - [`Lsns::provisional`] for multi-table `LSNS` framing.
//! - [`BlockCountSource`] records how the block count was found.
//! - The `CHKS` checksum algorithm is unknown and not verified.
//! - Pool reference block index width: bits 0..11 ([`MAX_BLOCK_INDEX`]).
//! - Name case: [`HashMapNames::add`] registers case-sensitive and lowercase
//!   JOAAT.
//!
//! Non-fatal problems are collected in [`PsoDocument::warnings`] and logged
//! with `log::warn!`.

pub mod document;
pub mod error;
pub mod json;
pub mod names;
pub mod schema;
pub mod section;
pub mod value;

pub use document::{
    Block, BlockCountSource, Checks, Layout, Lsns, MAX_BLOCK_INDEX, PsoDocument, ResourceHeader,
    RootInfo, RootSource,
};
pub use error::{PsoError, Result};
pub use json::{hash_label, to_json, to_json_with_schema};
pub use names::{HashMapNames, KNOWN_NAMES, NameLookup, NoNames, script_dom};
pub use schema::{
    ANONYMOUS_MEMBER, EnumDef, EnumValue, MemberDef, Schema, StructDef, TypeInfo, type_code,
    type_info,
};
pub use section::Section;
pub use u_rome_core::Hash32;
pub use value::{DecodeOptions, Instance, MAX_INSTANCES, Value};

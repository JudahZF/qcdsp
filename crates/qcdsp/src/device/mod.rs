//! Installable Quad Cortex blocks.
//!
//! A [`Block`] is compiled into three VISA callbacks (init, process,
//! set-parameter) that are appended to the stock Volume framework from the
//! user's own firmware. The result is a core 1 ModuleBlob, the core 2-4 diff
//! records and the two catalogue entries; `qc-deploy` encrypts and installs them.

mod emit;
mod layout;
mod metadata;
pub mod template;

use qc_format::{
    blob::{ModuleBlob, Section},
    diff::DiffRecord,
};

pub use layout::Layout;
pub use metadata::Resources;
pub use template::{Entries, VolumeTemplate};

use crate::{Block, Result};

/// Prefix of every generated blob name.
pub const BLOB_PREFIX: &str = "qcdsp_";

pub struct DeviceBlock {
    pub blob_name: String,
    pub model_id: u32,
    pub display_name: String,
    pub category: u32,
    pub core1: ModuleBlob,
    pub diffs: [DiffRecord; 3],
    pub model_element: String,
    pub module_element: String,
    pub layout: Layout,
    pub resources: Resources,
    pub entries: Entries,
}

/// Compile `block` for the device. `model_id` must be unused in the catalogue;
/// `qc-deploy` checks that against the user's snapshot before staging.
pub fn build(
    block: &Block,
    template: &VolumeTemplate,
    model_id: u32,
    display_name: &str,
) -> Result<DeviceBlock> {
    block.validate()?;
    // Prefixed so a generated block can never take a stock (or future) NDSP name.
    let blob_name = format!(
        "{BLOB_PREFIX}{}",
        block.blob_name().trim_start_matches(BLOB_PREFIX)
    );
    qc_format::blob::validate_blob_name(&blob_name)?;
    let layout = Layout::new(block)?;
    let generated = emit::generate(block, &layout)?;
    let (core1, diffs) = template.link(
        &blob_name,
        &generated.parcels,
        generated.entries,
        layout.bytes(),
    )?;
    let resources = Resources {
        code_bytes: core1.content(Section::Code).map_or(0, <[u8]>::len),
        state_bytes: layout.bytes(),
        cycles_per_call: generated.cycles_per_call,
    };
    Ok(DeviceBlock {
        model_element: metadata::model_element(block, &blob_name, model_id, display_name)?,
        module_element: metadata::module_element(
            block,
            &blob_name,
            model_id,
            display_name,
            &resources,
        )?,
        blob_name,
        model_id,
        display_name: display_name.to_owned(),
        category: block.category().id(),
        core1,
        diffs,
        layout,
        resources,
        entries: generated.entries,
    })
}

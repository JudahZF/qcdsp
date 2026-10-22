//! Installable Quad Cortex blocks.
//!
//! A [`Block`] is compiled into four VISA routines (init, process,
//! set-parameter, fault recovery), plus destroy when its banks need a heap arena, that are
//! appended to the stock Volume framework from the user's own firmware. The result is a core 1 ModuleBlob, the core 2-4 diff
//! records and the two catalogue entries; `qc-deploy` encrypts and installs them.

mod emit;
mod layout;
mod metadata;
pub mod template;

use qc_format::{
    blob::{ModuleBlob, Section},
    diff::DiffRecord,
};

pub use emit::Generated;
pub use layout::{Arena, Layout};
pub use metadata::{Resources, model_element, module_element};
pub use qc_link::KernelMap;
pub use template::{Entries, VolumeTemplate};

use crate::{Block, Result, codegen};

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

/// The generated routines of `block` and its state layout, without the stock template: the
/// parcels start at [`template::STOCK_PARCELS`] of the Volume code section, and `entries` are
/// offsets in parcels from its start. A SHARC+ model can run them (Tetrad's `tetrad-sharc`
/// tests do).
pub fn generate(block: &Block) -> Result<(Layout, Generated)> {
    let kernel = codegen::compile_with(block, emit::kernel_registers(block))?;
    let layout = Layout::new(block, &kernel.contract)?;
    let generated = emit::generate(block, &layout, &kernel)?;
    Ok((layout, generated))
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
    let (layout, generated) = generate(block)?;
    let (core1, diffs) = template.link(&blob_name, &generated, layout.bytes())?;
    let resources = Resources {
        code_bytes: core1.content(Section::Code).map_or(0, <[u8]>::len),
        state_bytes: layout.bytes(),
        cycles_per_call: generated.cycles_per_call,
    };
    resources.check_heaps(layout.arena)?;
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

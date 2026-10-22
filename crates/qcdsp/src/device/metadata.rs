//! `ModelRepo.xml` and `moduleReport.xml` entries for a generated block.

use qc_format::metadata::escape;

use super::template::{STOCK_CPU, STOCK_DM_HEAP, STOCK_STATE_BYTES};
use crate::{Block, Result};

/// Cycles available to one core for a 32-sample block at 48 kHz on a 450 MHz
/// SHARC+ core. Used only to turn a cycle estimate into a report percentage.
const CYCLES_PER_BLOCK: f64 = 450e6 * 32.0 / 48_000.0;
/// Over-report generated work: an under-reported CPU figure lets ZenUI place
/// blocks that overrun the core; an over-reported one only refuses a layout.
const CPU_SAFETY: f64 = 2.0;
const SW_PAD: usize = 256;

pub struct Resources {
    pub code_bytes: usize,
    pub state_bytes: u32,
    pub cycles_per_call: u64,
}

impl Resources {
    /// The DM heap the kernel charges: Volume's own figure with its state allocation replaced
    /// by the generated one. `heap_malloc` (k1 `0x16adfa`) takes `(bytes + 11) & !7` per
    /// allocation (an 8-byte header, rounded to 8 bytes).
    pub fn dm_heap(&self) -> u32 {
        STOCK_DM_HEAP - heap_charge(STOCK_STATE_BYTES) + heap_charge(self.state_bytes)
    }

    pub fn cpu(&self) -> f64 {
        let generated = self.cycles_per_call as f64 / CYCLES_PER_BLOCK * 100.0 * CPU_SAFETY;
        ((STOCK_CPU + generated) * 1000.0).ceil() / 1000.0
    }
}

fn heap_charge(bytes: u32) -> u32 {
    (bytes + 11) & !7
}

fn number(value: f32) -> String {
    // Shortest round-trip form keeps parameter limits exact in the catalogue.
    format!("{value}")
}

pub fn model_element(
    block: &Block,
    blob_name: &str,
    model_id: u32,
    display_name: &str,
) -> Result<String> {
    let mut xml = format!(
        r#"<Model blob="{}" id="{model_id}" name="{}"><Padding cpu="0.4" dm_hp="64" sw="256"/>"#,
        escape(blob_name)?,
        escape(display_name)?,
    );
    // Declared 0..1: the set-parameter callback maps to the DSP range itself, so
    // it does not depend on whether ZenUI delivers raw or normalized values.
    for parameter in block.parameters() {
        let span = parameter.max - parameter.min;
        let default = if span > 0.0 {
            (parameter.default - parameter.min) / span
        } else {
            0.0
        };
        xml.push_str(&format!(
            r#"<Parameter defaultValue="{}" max="1" min="0" name="{}" steps="1000" type="float"/>"#,
            number(default),
            escape(&parameter.name)?,
        ));
    }
    xml.push_str("</Model>");
    Ok(xml)
}

pub fn module_element(
    block: &Block,
    blob_name: &str,
    model_id: u32,
    display_name: &str,
    resources: &Resources,
) -> Result<String> {
    let cpu = resources.cpu();
    let mut xml = format!(
        r#"<Module name="{}" blob_name="{}" id="{model_id}" category="{}" dm_heap="{}" pm_heap="0" sd_heap="0" dma="0" pm="0" dm="32" sw="{}" sw_pad="{SW_PAD}" nw="0" cpu_pad="0.4" Latency="0">"#,
        escape(display_name)?,
        escape(blob_name)?,
        block.category().id(),
        resources.dm_heap(),
        resources.code_bytes + SW_PAD,
    );
    for test in [
        "Startup",
        "Idle",
        "Retrieving Latency",
        "Stimulus Low",
        "Stimulus Med",
        "Stimulus High",
        "Params Low",
        "Params Med",
        "Params High",
    ] {
        xml.push_str(&format!(
            r#"<Test name="{test}" min="{cpu}" max="{cpu}" avg="{cpu}" pk_l="-40" pk_r="-40"/>"#
        ));
    }
    xml.push_str("</Module>");
    Ok(xml)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Volume's own state (0x44 bytes) gives its stock figure; an even word count is charged
    /// 8 bytes of header and rounding, an odd one 4.
    #[test]
    fn dm_heap_follows_the_kernel_allocator() {
        let heap = |state_bytes| {
            Resources {
                code_bytes: 0,
                state_bytes,
                cycles_per_call: 0,
            }
            .dm_heap()
        };
        assert_eq!(heap(STOCK_STATE_BYTES), STOCK_DM_HEAP);
        assert_eq!(heap(64), 136 + 72);
        assert_eq!(heap(68), 136 + 72);
        assert_eq!(heap(72), 136 + 80);
    }
}

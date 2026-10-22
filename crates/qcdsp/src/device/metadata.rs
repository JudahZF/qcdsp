//! `ModelRepo.xml` and `moduleReport.xml` entries for a generated block.

use qc_format::metadata::escape;

use super::{
    layout::{Arena, Heap},
    template::{DM_HEAP_BYTES, SD_HEAP_BYTES, STOCK_CPU, STOCK_DM_HEAP, STOCK_STATE_BYTES},
};
use crate::{Block, Error, Result};

/// Cycles available to one core for a 32-sample block at 48 kHz on a 450 MHz
/// SHARC+ core. Used only to turn a cycle estimate into a report percentage.
const CYCLES_PER_BLOCK: f64 = 450e6 * 32.0 / 48_000.0;
/// Over-report generated work: an under-reported CPU figure lets ZenUI place
/// blocks that overrun the core; an over-reported one only refuses a layout.
const CPU_SAFETY: f64 = 2.0;
const SW_PAD: usize = 256;

pub struct Resources {
    pub code_bytes: usize,
    /// Volume's state allocation; an arena is separate ([`super::Layout::arena`]).
    pub state_bytes: u32,
    pub cycles_per_call: u64,
}

impl Resources {
    pub fn dm_heap(&self, arena: Option<Arena>) -> u32 {
        dm_heap(self.state_bytes, arena)
    }

    /// The SD heap the kernel charges: the block's SD arena, if it has one.
    pub fn sd_heap(&self, arena: Option<Arena>) -> u32 {
        arena
            .filter(|arena| arena.heap == Heap::Sd)
            .map_or(0, |arena| heap_charge(arena.bytes()))
    }

    /// Refuses a block whose heap figures exceed the heaps themselves: no layout could place
    /// it. Smaller figures are for ZenUI's fit (with every other block on the core) to decide.
    pub fn check_heaps(&self, arena: Option<Arena>) -> Result<()> {
        for (name, heap, length) in [
            ("DM", self.dm_heap(arena), DM_HEAP_BYTES),
            ("SD", self.sd_heap(arena), SD_HEAP_BYTES),
        ] {
            if heap > length {
                return Err(Error::Invalid(format!(
                    "the block needs {heap} bytes of {name} heap; the heap has {length}"
                )));
            }
        }
        Ok(())
    }

    pub fn cpu(&self) -> f64 {
        let generated = self.cycles_per_call as f64 / CYCLES_PER_BLOCK * 100.0 * CPU_SAFETY;
        ((STOCK_CPU + generated) * 1000.0).ceil() / 1000.0
    }
}

/// The DM heap the kernel charges: Volume's own figure with its state allocation replaced by
/// the generated one, plus the block's arena when it is in heap 0. `heap_malloc` (k1
/// `0x16adfa`) takes `(bytes + 11) & !7` per allocation (an 8-byte header, rounded to 8 bytes).
pub(super) fn dm_heap(state_bytes: u32, arena: Option<Arena>) -> u32 {
    STOCK_DM_HEAP - heap_charge(STOCK_STATE_BYTES)
        + heap_charge(state_bytes)
        + arena
            .filter(|arena| arena.heap == Heap::Dm)
            .map_or(0, |arena| heap_charge(arena.bytes()))
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
    // The arena follows from the block's layout, as in `device::build`.
    let (layout, _) = super::generate(block)?;
    let mut xml = format!(
        r#"<Module name="{}" blob_name="{}" id="{model_id}" category="{}" dm_heap="{}" pm_heap="0" sd_heap="{}" dma="0" pm="0" dm="32" sw="{}" sw_pad="{SW_PAD}" nw="0" cpu_pad="0.4" Latency="0">"#,
        escape(display_name)?,
        escape(blob_name)?,
        block.category().id(),
        resources.dm_heap(layout.arena),
        resources.sd_heap(layout.arena),
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
        let heap = |state_bytes| resources(state_bytes).dm_heap(None);
        assert_eq!(heap(STOCK_STATE_BYTES), STOCK_DM_HEAP);
        assert_eq!(heap(64), 136 + 72);
        assert_eq!(heap(68), 136 + 72);
        assert_eq!(heap(72), 136 + 80);
    }

    fn resources(state_bytes: u32) -> Resources {
        Resources {
            code_bytes: 0,
            state_bytes,
            cycles_per_call: 0,
        }
    }

    fn arena(bytes: u32) -> Option<Arena> {
        arena_in(Heap::Dm, bytes)
    }

    fn arena_in(heap: Heap, bytes: u32) -> Option<Arena> {
        Some(Arena {
            pointer: 17,
            words: bytes / 4,
            heap,
        })
    }

    /// An arena is a second heap-0 allocation with its own header and rounding, and the
    /// figure may not exceed heap 0.
    #[test]
    fn arena_is_charged_to_the_dm_heap_up_to_its_size() {
        let r = resources(72);
        let state = r.dm_heap(None);
        assert_eq!(r.dm_heap(arena(72_008)), state + 72_016);
        assert_eq!(r.dm_heap(arena(72_012)), state + 72_016);
        assert_eq!(r.dm_heap(arena(72_016)), state + 72_024);
        let budget = DM_HEAP_BYTES - state - 8;
        assert!(r.check_heaps(arena(budget)).is_ok());
        assert!(r.check_heaps(arena(budget + 8)).is_err());
    }

    /// An SD arena is charged to the SD heap only, up to the heap's length.
    #[test]
    fn sd_arena_is_charged_to_the_sd_heap_up_to_its_size() {
        let r = resources(72);
        let sd = arena_in(Heap::Sd, 524_288);
        assert_eq!(r.dm_heap(sd), r.dm_heap(None));
        assert_eq!(r.sd_heap(sd), 524_296);
        assert_eq!(r.sd_heap(arena(524_288)), 0);
        assert!(r.check_heaps(arena_in(Heap::Sd, SD_HEAP_BYTES - 8)).is_ok());
        assert!(r.check_heaps(arena_in(Heap::Sd, SD_HEAP_BYTES)).is_err());
    }
}

//! The CorOS builds that deployment supports. This is the one place for their
//! firmware hashes. Only hashes are recorded; no firmware bytes are in this crate.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::FirmwarePin;

/// A supported CorOS build: the on-disk SHA-256 of the pinned files in
/// `/opt/neuraldsp` that are the same on every unit. The two `ZPUKernel` files
/// are not here, because the installer translates them per unit (see [`KernelPin`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KnownBuild {
    pub version: &'static str,
    pub zenui_sha256: &'static str,
    /// `ZPU_SC589_SHARC_Core0.blwldr`, the SOC2 SHARC image. Owned blocks run on
    /// SOC1 core 1 (`ZPUKernel_Core1.ldr`) and do not depend on it. Generated
    /// blocks also get per-core records for cores 2..4, and SOC2 runs this image.
    /// It changed in 4.1.1 and is encrypted, so its call contract is not checked.
    /// The pin still checks it per build, so an unknown image is refused.
    pub soc2_core0_sha256: &'static str,
    pub soc2_spl_sha256: &'static str,
    pub soc2_spl_atma_sha256: &'static str,
}

/// From 4.1.0 to 4.1.1 only ZenUI and the SOC2 Core0 image changed. The Volume
/// template and its per-core records are byte-equal in both builds.
pub const KNOWN_BUILDS: [KnownBuild; 2] = [
    KnownBuild {
        version: "4.1.0",
        zenui_sha256: "c3c2d0a645443948bf4ffd7a9a2222911a548a773c8e09c218af1aeed65c09d7",
        soc2_core0_sha256: "68d72264663d5fc9fa11470f09c4031791425bd6a37d66e0f60ddc256a7acf10",
        soc2_spl_sha256: "cdd237e91971ca48300bbd445c71370ac37c9c46ca08b4a8533d4fa856a613af",
        soc2_spl_atma_sha256: "d183e8bc271ce304577af1d4a84b9514c6856ee6c37ef54020f54eab574c9e66",
    },
    KnownBuild {
        version: "4.1.1",
        zenui_sha256: "a595eca83aa06b9dc7f687ed915d911b7c9ee55e4df3f0839273deae5a2c4684",
        soc2_core0_sha256: "ea07b813275bd87c8f9f49184f83a068314e50e583bb092c40e8e265ffd830e8",
        soc2_spl_sha256: "cdd237e91971ca48300bbd445c71370ac37c9c46ca08b4a8533d4fa856a613af",
        soc2_spl_atma_sha256: "d183e8bc271ce304577af1d4a84b9514c6856ee6c37ef54020f54eab574c9e66",
    },
];

/// The on-disk SHA-256 of the two SOC1 kernels, from the user's own trusted copy.
/// The installer translates these files per unit, so their hashes differ between
/// units. Both known builds ship the same kernel content, so one pin serves both.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelPin {
    #[serde(rename = "ZPUKernel_Core1.ldr")]
    pub core1_sha256: String,
    #[serde(rename = "ZPUKernel_Core2.ldr")]
    pub core2_sha256: String,
}

impl KnownBuild {
    /// The full pin of this build on the unit that `kernels` comes from.
    pub fn pin(&self, kernels: &KernelPin) -> FirmwarePin {
        let runtime = [
            ("ZPUKernel_Core1.ldr", kernels.core1_sha256.as_str()),
            ("ZPUKernel_Core2.ldr", kernels.core2_sha256.as_str()),
            ("ZPU_SC589_SHARC_Core0.blwldr", self.soc2_core0_sha256),
            ("SOC2_SPL_Core0.blwldr", self.soc2_spl_sha256),
            ("SOC2_SPL_Core0_Atma.blwldr", self.soc2_spl_atma_sha256),
        ];
        FirmwarePin {
            zenui_sha256: self.zenui_sha256.to_owned(),
            runtime_sha256: BTreeMap::from(
                runtime.map(|(name, hash)| (name.to_owned(), hash.to_owned())),
            ),
        }
    }

    /// The known build that `pin` was made from, if any.
    pub fn of(pin: &FirmwarePin) -> Option<&'static Self> {
        let kernel = |name: &str| pin.runtime_sha256.get(name).cloned().unwrap_or_default();
        let kernels = KernelPin {
            core1_sha256: kernel("ZPUKernel_Core1.ldr"),
            core2_sha256: kernel("ZPUKernel_Core2.ldr"),
        };
        KNOWN_BUILDS
            .iter()
            .find(|build| build.pin(&kernels) == *pin)
    }
}

/// The pins of every known build on the unit that `kernels` comes from.
pub fn known_pins(kernels: &KernelPin) -> Vec<FirmwarePin> {
    KNOWN_BUILDS
        .iter()
        .map(|build| build.pin(kernels))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Error, select};

    fn kernels() -> KernelPin {
        KernelPin {
            core1_sha256: "1".repeat(64),
            core2_sha256: "2".repeat(64),
        }
    }

    /// On-disk hashes that a snapshot with exactly these pinned files would have.
    fn found(pin: &FirmwarePin) -> BTreeMap<String, Option<String>> {
        pin.entries()
            .map(|(path, hash)| (path, Some(hash.clone())))
            .collect()
    }

    #[test]
    fn each_known_build_passes_and_other_builds_are_refused() {
        let pins = known_pins(&kernels());
        for (build, pin) in KNOWN_BUILDS.iter().zip(&pins) {
            pin.validate().unwrap();
            assert_eq!(select(&pins, &found(pin)).unwrap(), pin);
            assert_eq!(KnownBuild::of(pin), Some(build));
        }

        // 4.1.1 ZenUI with the 4.1.0 SOC2 image is not a known build.
        let mut mixed = found(&pins[1]);
        mixed.insert(
            "sharc_binaries/ZPU_SC589_SHARC_Core0.blwldr".into(),
            Some(KNOWN_BUILDS[0].soc2_core0_sha256.into()),
        );
        assert!(matches!(select(&pins, &mixed), Err(Error::Firmware(_))));

        // A kernel from another unit is refused.
        let mut other_unit = found(&pins[0]);
        other_unit.insert(
            "sharc_binaries/ZPUKernel_Core1.ldr".into(),
            Some("3".repeat(64)),
        );
        assert!(matches!(
            select(&pins, &other_unit),
            Err(Error::Firmware(_))
        ));
    }
}

//! Applying an arbitrary parsed patch to arbitrary content must never panic.

#![no_main]

use flickzeug::{ApplyConfig, FuzzyConfig};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (&[u8], &[u8])| {
    let (patch, base) = input;
    let Ok(diff) = flickzeug::Diff::from_bytes(patch) else {
        return;
    };

    let _ = flickzeug::apply_bytes(base, &diff);

    // rattler-build style lenient config
    let config = ApplyConfig {
        fuzzy_config: FuzzyConfig {
            max_fuzz: 2,
            ignore_whitespace: true,
            ignore_case: false,
            similarity_threshold: 0.8,
        },
        ..ApplyConfig::default()
    };
    let _ = flickzeug::apply_bytes_reporting(base, &diff, &config);
});

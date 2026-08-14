//! Parsing untrusted patch input must never panic, and anything that parses
//! must survive a format -> reparse roundtrip.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Bytes front end
    if let Ok(diff) = flickzeug::Diff::from_bytes(data) {
        let formatted = diff.to_bytes();
        let reparsed = flickzeug::Diff::from_bytes(&formatted)
            .expect("formatted output of a parsed diff must reparse");
        assert_eq!(
            formatted,
            reparsed.to_bytes(),
            "formatting must be stable across parse"
        );
    }
    let _ = flickzeug::patch_from_bytes(data);

    // str front end
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(diff) = flickzeug::Diff::from_str(text) {
            let formatted = diff.to_string();
            flickzeug::Diff::from_str(&formatted)
                .expect("formatted output of a parsed diff must reparse");
        }
        let _ = flickzeug::patch_from_str(text);
    }
});

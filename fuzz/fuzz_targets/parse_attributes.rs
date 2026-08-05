//! `ConfigDSNW`'s attribute-list parser, driven over raw pointers.
//!
//! `test_support::parse_attributes_summary_w` wraps
//! `ffi::setup::parse_attributes_w`, which walks a `*const u16` the Driver
//! Manager supplied, hunting for the double null that ends the list. It is the
//! one parser in core whose failure mode is a read past the end of an
//! allocation rather than a panic, which is what makes AddressSanitizer worth
//! the nightly toolchain here.
//!
//! # Every buffer is terminated
//!
//! The parser's safety contract is that the pointer is null, or points to a
//! valid double-null-terminated `u16` sequence. So each shape below appends that
//! terminator itself and fuzzes what comes *before* it. Handing the parser an
//! unterminated buffer would certainly produce an ASAN report, and it would be a
//! report about this file: the read past the end would be the caller breaking a
//! contract it agreed to, not the parser exceeding one. What is worth fuzzing is
//! the walk over a buffer that is terminated but says nothing else sensible.
//!
//! The parser bounds each segment at `i16::MAX` code units precisely so a caller
//! that gets this wrong is contained rather than unbounded, and the
//! `OverlongSegment` shape reaches that bound from inside a real allocation.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use stackable_odbc_core::test_support::parse_attributes_summary_w;

/// One code unit past the parser's own per-segment scan limit, which is
/// `i16::MAX`. Declared here rather than imported because it is crate-private,
/// and a copy that drifts makes this shape stop reaching the bound rather than
/// start failing, so the assertion below checks the bound was actually hit.
const PAST_SEGMENT_SCAN_LIMIT: usize = i16::MAX as usize + 1;

#[derive(Arbitrary, Debug)]
enum Shape {
    /// A `u16`-aligned buffer, which is the ordinary case.
    Aligned,
    /// A buffer whose `u16` sequence starts at an odd byte address.
    ///
    /// The Driver Manager promises no alignment, and the parser reads every code
    /// unit with `read_unaligned` for that reason. An aligned read of this
    /// pointer is undefined behaviour, and in a debug build it aborts without
    /// unwinding, which no panic hook can contain. A regression to
    /// `slice::from_raw_parts` would be caught here and nowhere else.
    Unaligned,
    /// A segment longer than the parser will scan, so the scan limit fires with
    /// every read still inside the allocation.
    OverlongSegment,
}

#[derive(Arbitrary, Debug)]
struct Input {
    shape: Shape,
    units: Vec<u16>,
}

fuzz_target!(|input: Input| {
    match input.shape {
        Shape::Aligned => {
            let mut buf = input.units;
            buf.extend_from_slice(&[0, 0]);
            // SAFETY: `buf` is non-empty and ends in two zero `u16`s, so it is a
            // double-null-terminated sequence, and it outlives the call.
            let _ = unsafe { parse_attributes_summary_w(buf.as_ptr()) };
        }

        Shape::Unaligned => {
            // One byte of padding in front, so the `u16` sequence begins at an
            // odd address. Everything after it stays a whole number of code
            // units, which keeps the terminator two aligned-to-the-sequence
            // zeros rather than a split pair.
            let mut bytes = vec![0u8];
            for unit in &input.units {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
            bytes.extend_from_slice(&[0, 0, 0, 0]);

            // SAFETY: offset 1 is inside `bytes`, which holds `1 + 2n + 4`
            // bytes, so the sequence from there is `n + 2` whole `u16`s ending
            // in two zeros. The pointer is read only with `read_unaligned`, so
            // the odd address is sound, and `bytes` outlives the call.
            let ptr = unsafe { bytes.as_ptr().add(1) }.cast::<u16>();
            let _ = unsafe { parse_attributes_summary_w(ptr) };
        }

        Shape::OverlongSegment => {
            // The fuzzed units come first, so their bytes still drive real
            // segments, and the overlong run is appended behind a separator.
            let mut buf = input.units;
            buf.push(0);

            // Decided on the prefix alone, before the run and the terminator are
            // appended: those end the list by construction, so a check made
            // after them would be true every time and assert nothing.
            let ends_early = ends_the_list(&buf);

            buf.resize(buf.len() + PAST_SEGMENT_SCAN_LIMIT, u16::from(b'A'));
            buf.extend_from_slice(&[0, 0]);

            // SAFETY: as the aligned case; `buf` ends in two zero `u16`s.
            let (_, _, syntax_error) = unsafe { parse_attributes_summary_w(buf.as_ptr()) };

            // Where the run is reached at all, the scan limit must have fired.
            assert!(
                syntax_error || ends_early,
                "a segment of {PAST_SEGMENT_SCAN_LIMIT} code units must trip the scan limit"
            );
        }
    }
});

/// Whether the parser stops inside `units` rather than walking off its end.
///
/// It stops at an empty segment, which is a leading null or two consecutive
/// ones.
fn ends_the_list(units: &[u16]) -> bool {
    units.first() == Some(&0) || units.windows(2).any(|pair| pair == [0, 0])
}

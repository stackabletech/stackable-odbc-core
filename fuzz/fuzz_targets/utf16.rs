#![no_main]

use libfuzzer_sys::fuzz_target;
use stackable_odbc_core::utf16::{utf16_to_string, write_utf16};

// Fuzzes the UTF-16 marshalling helpers. Every output buffer is allocated at
// exactly the length passed to the function, so AddressSanitizer flags any read
// or write past the caller's buffer — the buffer-overrun defect class that
// clippy and Miri-less unit tests cannot see.
fuzz_target!(|data: &[u8]| {
    // Interpret the input as little-endian UTF-16 code units.
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();

    // Explicit-length reading: any code units are valid; the length is exact.
    if !units.is_empty() {
        unsafe {
            let _ = utf16_to_string(units.as_ptr(), units.len() as i32);
        }
    }

    // SQL_NTS reading requires a null-terminated buffer (utf16_to_string's
    // documented safety contract, which the Driver Manager upholds in practice).
    // Append a terminator so the scan stays in bounds; feeding an unterminated
    // buffer with SQL_NTS would violate the contract, not find a real bug.
    let mut nts = units.clone();
    nts.push(0);
    unsafe {
        let _ = utf16_to_string(nts.as_ptr(), -3);
    }

    // Writing side: a spread of output buffer sizes, each allocated to its exact
    // length so an off-by-one overrun is caught.
    let s = String::from_utf16_lossy(&units);
    for &buf_len in &[0i16, 1, 2, 3, 5, 16] {
        let mut buf = vec![0u16; buf_len.max(0) as usize];
        let mut len_out: i16 = 0;
        unsafe {
            let _ = write_utf16(&s, buf.as_mut_ptr(), buf_len, &mut len_out);
        }
    }
});

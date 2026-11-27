#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| jail::fuzzing::proxy_request(data));

use mterm::profile::TerminalProfile;
use mterm::terminal::TerminalEmulator;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
}

#[test]
fn fuzz_random_sequences() {
    let pieces: &[&[u8]] = &[b"\x1b[", b"\x1b]", b"\x1b(0", b"\x1b7", b"\x1b8", b"\x1bM", b"\x1bD", b"\x1bE", b"\x1bc",
        b";", b"?", b"0", b"1", b"9", b"65535", b"99999", b"H", b"J", b"K", b"r", b"m", b"L", b"M", b"P", b"@", b"X",
        b"S", b"T", b"G", b"d", b"A", b"B", b"C", b"D", b"h", b"l", b"1049", b"47", b"\x07", b"\x1b\\", b"\r", b"\n", b"\t",
        b"\x08", "漢字".as_bytes(), "e\u{301}".as_bytes(), b"x", b"52;c;", b"aGk=", b"?", b"38;5;", b"38;2;", b":", b"\xff", b"\xe6", b"Z", b"b", b"g", b"n", b"c", b"t", b"s", b"u"];
    for seed in 1..300u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15));
        let cols = (rng.next() % 200 + 1) as usize;
        let rows = (rng.next() % 60 + 1) as usize;
        let mut emu = TerminalEmulator::new(cols, rows, TerminalProfile::default());
        for i in 0..3000 {
            let p = pieces[(rng.next() as usize) % pieces.len()];
            emu.process_bytes(p);
            if i % 500 == 0 {
                let c = (rng.next() % 200 + 1) as usize; let r = (rng.next() % 60 + 1) as usize;
                emu.resize(c, r);
            }
            for vr in 0..emu.rows { let _ = emu.visible_row(vr); }
        }
    }
}

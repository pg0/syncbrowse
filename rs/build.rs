//! Puts syncbrowse.ico into the exe as icon resource 1 (Explorer shows it, the window loads it from there).
//! Writes the .res file itself, so the build needs no rc.exe and no extra crate.
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=syncbrowse.ico");
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return; // link.exe takes the .res as it is; other linkers would need a converted object
    }
    let ico = fs::read("syncbrowse.ico").expect("syncbrowse.ico");
    let u32_at = |at: usize| u32::from_le_bytes(ico[at..at + 4].try_into().unwrap()) as usize;
    let mut res = vec![0, 0, 0, 0, 32, 0, 0, 0, 0xFF, 0xFF, 0, 0, 0xFF, 0xFF, 0, 0];
    res.resize(32, 0); // a .res file starts with an empty entry
    let mut group = ico[..6].to_vec(); // same header as the .ico: reserved, type 1, count
    for i in 0..u16::from_le_bytes([ico[4], ico[5]]) {
        let e = 6 + 16 * i as usize; // directory entry: size and offset of one image in the file
        entry(&mut res, 3, i + 1, &ico[u32_at(e + 12)..][..u32_at(e + 8)]); // RT_ICON
        group.extend_from_slice(&ico[e..e + 12]);
        group.extend_from_slice(&(i + 1).to_le_bytes()); // the group points at the RT_ICON id, not a file offset
    }
    entry(&mut res, 14, 1, &group); // RT_GROUP_ICON
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("icon.res");
    fs::write(&out, res).unwrap();
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

/// Resource header (data size, header size 32, numeric type and name, flags), then the data padded to 4 bytes.
fn entry(res: &mut Vec<u8>, kind: u16, name: u16, data: &[u8]) {
    res.extend_from_slice(&(data.len() as u32).to_le_bytes());
    res.extend_from_slice(&32u32.to_le_bytes());
    for v in [0xFFFF, kind, 0xFFFF, name] {
        res.extend_from_slice(&v.to_le_bytes());
    }
    res.extend_from_slice(&[0, 0, 0, 0, 0x30, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // version, MOVEABLE|PURE|DISCARDABLE, neutral language
    res.extend_from_slice(data);
    res.resize(res.len().next_multiple_of(4), 0);
}

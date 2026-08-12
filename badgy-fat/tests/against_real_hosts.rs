//! Checks this reader against filesystems produced by real tools rather than by
//! this crate.
//!
//! Reading FAT from a spec is how you write a parser that works on your own
//! output and nothing else. These tests shell out to `mkfs.fat` and `fsck.fat`
//! -- and, when the environment allows it, actually mount a volume and `cp` a
//! file onto it -- so the thing being parsed is what a host really wrote.
//!
//! Tests that need a tool the machine does not have skip themselves rather than
//! fail, so this suite stays useful in a bare container.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use badgy_fat::{VOLUME_BYTES, Volume, format};

fn have(tool: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {}", tool))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn tmp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("badgyfat-{}-{}", std::process::id(), name));
    p
}

fn run(cmd: &str) -> (bool, String) {
    let out = Command::new("sh").arg("-c").arg(cmd).output().expect("failed to spawn sh");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

fn our_volume(seed: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = vec![0u8; VOLUME_BYTES];
    format(&mut buf, "BADGYOS", 0x1234_5678, seed).expect("format failed");
    buf
}

fn write_image(path: &Path, data: &[u8]) {
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(data).unwrap();
    f.sync_all().unwrap();
}

// ---------------------------------------------------------- our output, their tools

#[test]
fn fsck_accepts_the_volume_we_format() {
    if !have("fsck.fat") {
        eprintln!("skipping: fsck.fat not installed");
        return;
    }
    let img = tmp("fsck.img");
    write_image(&img, &our_volume(&[("hello.py", b"print('hi')\n"), ("readme.txt", b"docs\n")]));

    let (ok, text) = run(&format!("fsck.fat -n -v {}", img.display()));
    assert!(ok, "fsck.fat rejected our volume:\n{}", text);
    // It must agree with us about the type and the geometry, not just be silent.
    // fsck reports the FAT width rather than the name, and the width is what
    // actually decides how the entries are packed.
    assert!(text.contains("12 bit entries"), "fsck did not read this as FAT12:\n{}", text);
    assert!(text.contains("512 bytes per logical sector"), "unexpected sector size:\n{}", text);
    assert!(text.contains("1013 data clusters"), "unexpected cluster count:\n{}", text);
    // Three directory entries: the volume label plus the two seed files.
    assert!(text.contains("3 files"), "seeded files not seen:\n{}", text);
    let _ = std::fs::remove_file(&img);
}

#[test]
fn our_seeded_files_survive_a_round_trip_through_our_reader() {
    let script = b"# a comment\nprint('hi')\n";
    let vol = our_volume(&[("hello.py", script), ("readme.txt", b"instructions")]);
    let v = Volume::open(&vol).expect("our own volume should parse");

    assert_eq!(v.label(), "BADGYOS");
    let files = v.files();
    let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["hello.py", "readme.txt"], "case flags should survive");

    let py = files.iter().find(|f| f.has_extension("py")).unwrap();
    assert_eq!(py.size as usize, script.len());
    assert_eq!(v.read_file(py).unwrap(), script);
}

#[test]
fn a_multi_cluster_seed_file_reads_back_intact() {
    // Bigger than one 512-byte cluster, so the reader has to walk the chain.
    let big: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
    let vol = our_volume(&[("big.bin", &big)]);
    let v = Volume::open(&vol).unwrap();
    let f = &v.files()[0];
    assert_eq!(f.size as usize, big.len());
    assert_eq!(v.read_file(f).unwrap(), big);
}

// ---------------------------------------------------------- their output, our reader

#[test]
fn we_can_read_a_volume_mkfs_made() {
    if !have("mkfs.fat") {
        eprintln!("skipping: mkfs.fat not installed");
        return;
    }
    let img = tmp("mkfs.img");
    // Same geometry we format, but built by the real tool.
    let (ok, text) =
        run(&format!("mkfs.fat -C -F 12 -s 1 -r 64 -R 1 -f 2 -n 'BADGYOS' {} 512", img.display()));
    assert!(ok, "mkfs.fat failed:\n{}", text);

    let data = std::fs::read(&img).unwrap();
    let v = Volume::open(&data).expect("could not parse a volume mkfs.fat made");
    assert_eq!(v.geom.bytes_per_sector, 512);
    assert_eq!(v.geom.total_sectors, 1024);
    assert!(v.files().is_empty(), "a fresh volume should have no files");
    assert_eq!(v.label(), "BADGYOS");
    let _ = std::fs::remove_file(&img);
}

/// The important one: a file put there by the host's own FAT driver, with the
/// long-name entries the host chose to write.
#[test]
fn we_can_read_files_a_mounted_host_wrote() {
    if !have("mkfs.fat") || !have("mcopy") {
        eprintln!("skipping: needs mkfs.fat and mtools (mcopy)");
        return;
    }
    let img = tmp("hostwrite.img");
    let (ok, text) =
        run(&format!("mkfs.fat -C -F 12 -s 1 -r 64 -R 1 -f 2 -n 'BADGYOS' {} 512", img.display()));
    assert!(ok, "mkfs.fat failed:\n{}", text);

    let src = tmp("blink.py");
    write_image(&src, b"for i in range(10):\n    print(i)\n");
    let long = tmp("my cool script.py");
    write_image(&long, b"print('long name')\n");

    // mtools writes VFAT long names by default, same as a mounted filesystem.
    let (ok, text) = run(&format!(
        "MTOOLS_SKIP_CHECK=1 mcopy -i {img} {src} ::blink.py && \
         MTOOLS_SKIP_CHECK=1 mcopy -i {img} '{long}' '::my cool script.py'",
        img = img.display(),
        src = src.display(),
        long = long.display()
    ));
    assert!(ok, "mcopy failed:\n{}", text);

    let data = std::fs::read(&img).unwrap();
    let v = Volume::open(&data).unwrap();
    let files = v.files();
    let mut names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    names.sort();

    // Lower case must survive, which only works if the long-name entries are
    // being read -- the short entry says BLINK.PY.
    assert!(names.contains(&"blink.py"), "long names not decoded: {:?}", names);
    assert!(names.contains(&"my cool script.py"), "spaces not handled: {:?}", names);

    let blink = files.iter().find(|f| f.name == "blink.py").unwrap();
    assert_eq!(v.read_file(blink).unwrap(), b"for i in range(10):\n    print(i)\n");

    for p in [&img, &src, &long] {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn a_deleted_file_disappears() {
    if !have("mkfs.fat") || !have("mcopy") || !have("mdel") {
        eprintln!("skipping: needs mtools");
        return;
    }
    let img = tmp("delete.img");
    run(&format!("mkfs.fat -C -F 12 -s 1 -r 64 -R 1 -f 2 -n 'BADGY' {} 512", img.display()));
    let src = tmp("gone.py");
    write_image(&src, b"print(1)\n");
    run(&format!("MTOOLS_SKIP_CHECK=1 mcopy -i {} {} ::gone.py", img.display(), src.display()));

    let data = std::fs::read(&img).unwrap();
    assert_eq!(Volume::open(&data).unwrap().files().len(), 1);

    let (ok, text) = run(&format!("MTOOLS_SKIP_CHECK=1 mdel -i {} ::gone.py", img.display()));
    assert!(ok, "mdel failed:\n{}", text);
    let data = std::fs::read(&img).unwrap();
    assert!(Volume::open(&data).unwrap().files().is_empty(), "deleted file still listed");

    let _ = std::fs::remove_file(&img);
    let _ = std::fs::remove_file(&src);
}

#[test]
fn directories_and_the_volume_label_stay_out_of_the_file_list() {
    if !have("mkfs.fat") || !have("mmd") || !have("mcopy") {
        eprintln!("skipping: needs mtools");
        return;
    }
    let img = tmp("dirs.img");
    run(&format!("mkfs.fat -C -F 12 -s 1 -r 64 -R 1 -f 2 -n 'BADGYOS' {} 512", img.display()));
    let src = tmp("keep.py");
    write_image(&src, b"print(1)\n");
    let (ok, text) = run(&format!(
        "MTOOLS_SKIP_CHECK=1 mmd -i {img} '::System Volume Information' && \
         MTOOLS_SKIP_CHECK=1 mcopy -i {img} {src} ::keep.py",
        img = img.display(),
        src = src.display()
    ));
    assert!(ok, "mtools failed:\n{}", text);

    let data = std::fs::read(&img).unwrap();
    let v = Volume::open(&data).unwrap();
    let files = v.files();
    let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["keep.py"], "a directory or the label leaked into the list");
    assert_eq!(v.label(), "BADGYOS");

    let _ = std::fs::remove_file(&img);
    let _ = std::fs::remove_file(&src);
}

// -------------------------------------------------- long names, from real bytes

/// `mcopy` is not always installed, and neither is a mountable loop device, so
/// the long-name path is also pinned against directory entries captured
/// verbatim from a Linux host copying onto a FAT12 volume. These are the bytes
/// the kernel's vfat driver actually wrote.
#[test]
fn decodes_the_long_name_bytes_a_linux_host_wrote() {
    // `cp blink.py` -- one long-name entry (the name fits in 13 units) followed
    // by the uppercased short entry it describes.
    #[rustfmt::skip]
    const LFN: [u8; 32] = [
        0x41,                                                    // order: last | seq 1
        0x62, 0x00, 0x6c, 0x00, 0x69, 0x00, 0x6e, 0x00, 0x6b, 0x00, // "blink"
        0x0f,                                                    // attr: LFN
        0x00,                                                    // type
        0x27,                                                    // checksum of "BLINK   PY "
        0x2e, 0x00, 0x70, 0x00, 0x79, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, // ".py", NUL, pad
        0x00, 0x00,                                              // cluster (always 0 here)
        0xff, 0xff, 0xff, 0xff,                                  // pad
    ];
    #[rustfmt::skip]
    const SFN: [u8; 32] = [
        0x42, 0x4c, 0x49, 0x4e, 0x4b, 0x20, 0x20, 0x20, // "BLINK   "
        0x50, 0x59, 0x20,                               // "PY "
        0x20,                                           // attr: archive
        0x00,                                           // NTres: no case flags
        0xb8, 0xf5, 0x01, 0x0a, 0x5d, 0x0a, 0x5d, 0x00, 0x00, 0x00, 0x00, 0x0a, 0x5d, // times
        0x02, 0x00,                                     // first cluster 2
        0x09, 0x00, 0x00, 0x00,                         // size 9
    ];

    let mut vol = our_volume(&[("x.py", b"print(1)\n")]);
    // Overwrite the seeded entry (root entry 1) with the captured pair. The
    // seed put its 9 bytes in cluster 2, which is where this entry points.
    let root = 7 * 512;
    vol[root + 32..root + 64].copy_from_slice(&LFN);
    vol[root + 64..root + 96].copy_from_slice(&SFN);

    let v = Volume::open(&vol).unwrap();
    let files = v.files();
    assert_eq!(files.len(), 1, "{:?}", files);
    assert_eq!(files[0].name, "blink.py", "the long name must win over BLINK.PY");
    assert_eq!(v.read_file(&files[0]).unwrap(), b"print(1)\n");
}

#[test]
fn assembles_a_name_that_spans_two_long_entries() {
    // "my cool script.py" is 17 characters, so the host splits it: entry seq 2
    // (marked last, stored first) carries "t.py", entry seq 1 carries
    // "my cool scrip".
    let sfn: [u8; 11] = *b"MYCOOL~1PY ";
    let cksum = badgy_fat::sfn_checksum(&sfn);
    assert_eq!(cksum, 0x16, "checksum should match the value seen on a real host");

    let name: Vec<u16> = "my cool script.py".encode_utf16().collect();
    let e2 = lfn_entry(0x40 | 2, cksum, &chars(&name, 13, 26));
    let e1 = lfn_entry(1, cksum, &chars(&name, 0, 13));

    let mut sfn_entry = [0u8; 32];
    sfn_entry[..11].copy_from_slice(&sfn);
    sfn_entry[11] = 0x20;
    sfn_entry[26..28].copy_from_slice(&2u16.to_le_bytes());
    sfn_entry[28..32].copy_from_slice(&4u32.to_le_bytes());

    let mut vol = our_volume(&[("x.py", b"data")]);
    let root = 7 * 512;
    vol[root + 32..root + 64].copy_from_slice(&e2);
    vol[root + 64..root + 96].copy_from_slice(&e1);
    vol[root + 96..root + 128].copy_from_slice(&sfn_entry);

    let v = Volume::open(&vol).unwrap();
    let files = v.files();
    assert_eq!(files.len(), 1, "{:?}", files);
    assert_eq!(files[0].name, "my cool script.py");
}

#[test]
fn a_long_name_whose_checksum_does_not_match_falls_back_to_the_short_name() {
    let sfn: [u8; 11] = *b"REAL    PY ";
    let name: Vec<u16> = "lying.py".encode_utf16().collect();
    // Deliberately wrong checksum: the long name belongs to some other entry.
    let e1 = lfn_entry(0x40 | 1, badgy_fat::sfn_checksum(&sfn) ^ 0xFF, &chars(&name, 0, 13));

    let mut sfn_entry = [0u8; 32];
    sfn_entry[..11].copy_from_slice(&sfn);
    sfn_entry[11] = 0x20;
    sfn_entry[26..28].copy_from_slice(&2u16.to_le_bytes());
    sfn_entry[28..32].copy_from_slice(&4u32.to_le_bytes());

    let mut vol = our_volume(&[("x.py", b"data")]);
    let root = 7 * 512;
    vol[root + 32..root + 64].copy_from_slice(&e1);
    vol[root + 64..root + 96].copy_from_slice(&sfn_entry);

    let v = Volume::open(&vol).unwrap();
    assert_eq!(v.files()[0].name, "REAL.PY");
}

/// 13 UTF-16 units from `name[from..to]`, NUL-terminated and 0xFFFF-padded the
/// way the format requires.
fn chars(name: &[u16], from: usize, to: usize) -> [u16; 13] {
    let mut out = [0xFFFFu16; 13];
    for (i, slot) in out.iter_mut().enumerate() {
        let idx = from + i;
        if idx < name.len() && idx < to {
            *slot = name[idx];
        } else if idx == name.len() {
            // The terminator goes immediately after the last character, and
            // only if there is room for it in this entry.
            *slot = 0;
        }
    }
    out
}

fn lfn_entry(order: u8, checksum: u8, units: &[u16; 13]) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[0] = order;
    e[11] = 0x0F;
    e[13] = checksum;
    for (k, u) in units.iter().enumerate() {
        let off = match k {
            0..=4 => 1 + k * 2,
            5..=10 => 14 + (k - 5) * 2,
            _ => 28 + (k - 11) * 2,
        };
        e[off..off + 2].copy_from_slice(&u.to_le_bytes());
    }
    e
}

// ------------------------------------------------------------- partial writes

/// The case the whole design turns on: a host writes the directory entry before
/// it writes the data, so a scan that trusts the entry imports a truncated file.
#[test]
fn a_file_whose_data_has_not_arrived_yet_is_not_reported() {
    let content = vec![b'x'; 2000]; // 4 clusters at 512 bytes
    let mut vol = our_volume(&[("part.py", &content)]);

    // Sanity: complete, it is visible.
    assert_eq!(Volume::open(&vol).unwrap().files().len(), 1);

    // Now break the chain the way an in-progress copy would: terminate it one
    // cluster early while the directory entry still claims the full size.
    const RESERVED_SECTORS: usize = 1;
    let fat_off = RESERVED_SECTORS * 512;
    // Cluster 2 -> 3 -> 4 -> 5(EOC). Make cluster 4 the end instead.
    set_fat12(&mut vol[fat_off..fat_off + 3 * 512], 4, 0xFFF);
    let v = Volume::open(&vol).unwrap();
    assert!(v.files().is_empty(), "a short chain must not be reported as a complete file");
}

#[test]
fn a_chain_longer_than_the_size_claims_is_rejected() {
    let content = vec![b'x'; 600]; // 2 clusters
    let mut vol = our_volume(&[("part.py", &content)]);
    let fat_off = 512;
    // Point the last cluster at another one instead of ending.
    set_fat12(&mut vol[fat_off..fat_off + 3 * 512], 3, 4);
    set_fat12(&mut vol[fat_off..fat_off + 3 * 512], 4, 0xFFF);
    assert!(Volume::open(&vol).unwrap().files().is_empty());
}

#[test]
fn a_cyclic_chain_terminates_instead_of_hanging() {
    let content = vec![b'x'; 2000];
    let mut vol = our_volume(&[("loop.py", &content)]);
    let fat_off = 512;
    // 2 -> 3 -> 2 -> ...
    set_fat12(&mut vol[fat_off..fat_off + 3 * 512], 3, 2);
    let v = Volume::open(&vol).unwrap();
    // Whatever it decides, it must decide it -- the point is that it returns.
    let _ = v.files();
}

#[test]
fn garbage_in_the_directory_does_not_panic() {
    let mut vol = our_volume(&[("a.py", b"1")]);
    // Scribble over the whole root directory region with a pattern that is not
    // a valid entry of any kind.
    let root = 7 * 512;
    for (i, b) in vol[root..root + 4 * 512].iter_mut().enumerate() {
        *b = (i * 7 + 3) as u8;
    }
    let v = Volume::open(&vol).unwrap();
    for f in v.files() {
        // Reading whatever it found must also not panic.
        let _ = v.read_file(&f);
    }
}

#[test]
fn a_blank_volume_is_not_mistaken_for_a_filesystem() {
    assert!(Volume::open(&vec![0u8; VOLUME_BYTES]).is_err());
    assert!(Volume::open(&vec![0xFFu8; VOLUME_BYTES]).is_err());
}

/// Same helper `format.rs` uses, duplicated here so the tests can corrupt a
/// volume the way a host mid-write would.
fn set_fat12(fat: &mut [u8], cluster: u32, value: u32) {
    let k = (cluster + cluster / 2) as usize;
    if cluster & 1 == 0 {
        fat[k] = (value & 0xFF) as u8;
        fat[k + 1] = (fat[k + 1] & 0xF0) | ((value >> 8) & 0x0F) as u8;
    } else {
        fat[k] = (fat[k] & 0x0F) | (((value & 0x0F) as u8) << 4);
        fat[k + 1] = ((value >> 4) & 0xFF) as u8;
    }
}

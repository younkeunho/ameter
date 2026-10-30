//! Embeds the icon (assets/ameter.ico), the version information and the application manifest
//! into both executables.
//!
//! The resources are written straight into a COFF object with a `.rsrc` section (what a
//! resource compiler produces), so building needs no rc.exe or windres on either toolchain.

use std::env;
use std::fs;
use std::path::PathBuf;

const RT_ICON: u32 = 3;
const RT_GROUP_ICON: u32 = 14;
const RT_VERSION: u32 = 16;
const RT_MANIFEST: u32 = 24;
/// CREATEPROCESS_MANIFEST_RESOURCE_ID: the manifest the loader reads for an .exe.
const MANIFEST_ID: u32 = 1;
const LANG_NEUTRAL: u32 = 0;
const LANG_EN_US: u32 = 0x0409;

struct Resource {
    kind: u32,
    name: u32,
    lang: u32,
    data: Vec<u8>,
}

fn main() {
    println!("cargo:rerun-if-changed=assets/ameter.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let machine: u16 = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => 0x8664,
        Ok("aarch64") => 0xAA64,
        Ok("x86") => 0x014C,
        other => panic!("unsupported architecture {other:?}"),
    };

    let mut resources = icons(&fs::read("assets/ameter.ico").expect("assets/ameter.ico"));
    resources.push(Resource { kind: RT_VERSION, name: 1, lang: LANG_EN_US, data: version_info() });
    resources.push(Resource { kind: RT_MANIFEST, name: MANIFEST_ID, lang: LANG_EN_US, data: MANIFEST.as_bytes().to_vec() });

    let extension = if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") { "obj" } else { "o" };
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join(format!("ameter-resources.{extension}"));
    fs::write(&out, coff(machine, resources)).unwrap();
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

fn pad(buf: &mut Vec<u8>, align: usize) {
    while !buf.len().is_multiple_of(align) {
        buf.push(0);
    }
}

fn u16s(buf: &mut Vec<u8>, values: &[u16]) {
    buf.extend(values.iter().flat_map(|v| v.to_le_bytes()));
}

fn u32s(buf: &mut Vec<u8>, values: &[u32]) {
    buf.extend(values.iter().flat_map(|v| v.to_le_bytes()));
}

/// Windows 10/11 compatibility (layered child windows, which the meter is, need Windows 8 or
/// later declared here) and per-monitor v2 DPI awareness.
const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>
"#;

// ---------------------------------------------------------------- icon

/// The images of an .ico file become RT_ICON 1..n; its directory becomes RT_GROUP_ICON 1.
fn icons(ico: &[u8]) -> Vec<Resource> {
    let word = |at: usize| u16::from_le_bytes([ico[at], ico[at + 1]]);
    let dword = |at: usize| u32::from_le_bytes([ico[at], ico[at + 1], ico[at + 2], ico[at + 3]]);
    assert!(word(0) == 0 && word(2) == 1, "assets/ameter.ico is not an icon file");
    let count = word(4);
    let mut resources = Vec::new();
    let mut group = Vec::new();
    u16s(&mut group, &[0, 1, count]);
    for i in 0..count {
        let at = 6 + 16 * i as usize;
        let (size, offset) = (dword(at + 8) as usize, dword(at + 12) as usize);
        let id = i + 1;
        resources.push(Resource { kind: RT_ICON, name: id as u32, lang: LANG_NEUTRAL, data: ico[offset..offset + size].to_vec() });
        // GRPICONDIRENTRY: the ICONDIRENTRY fields with the resource id in place of the file offset.
        group.extend(&ico[at..at + 12]);
        u16s(&mut group, &[id]);
    }
    resources.push(Resource { kind: RT_GROUP_ICON, name: 1, lang: LANG_NEUTRAL, data: group });
    resources
}

// ---------------------------------------------------------------- version information

/// A VS_VERSIONINFO node: header, NUL-terminated UTF-16 key, value, then DWORD-aligned children.
fn node(key: &str, value: &[u8], value_len: u16, text: bool, children: &[Vec<u8>]) -> Vec<u8> {
    let mut buf = Vec::new();
    u16s(&mut buf, &[0, value_len, text as u16]);
    u16s(&mut buf, &key.encode_utf16().chain([0]).collect::<Vec<_>>());
    pad(&mut buf, 4);
    buf.extend(value);
    for child in children {
        pad(&mut buf, 4);
        buf.extend(child);
    }
    let len = buf.len() as u16;
    buf[..2].copy_from_slice(&len.to_le_bytes());
    buf
}

fn string(key: &str, value: &str) -> Vec<u8> {
    let units: Vec<u16> = value.encode_utf16().chain([0]).collect();
    let mut bytes = Vec::new();
    u16s(&mut bytes, &units);
    node(key, &bytes, units.len() as u16, true, &[])
}

fn version_info() -> Vec<u8> {
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let major: u32 = env::var("CARGO_PKG_VERSION_MAJOR").unwrap().parse().unwrap();
    let minor: u32 = env::var("CARGO_PKG_VERSION_MINOR").unwrap().parse().unwrap();
    let patch: u32 = env::var("CARGO_PKG_VERSION_PATCH").unwrap().parse().unwrap();
    let authors = env::var("CARGO_PKG_AUTHORS").unwrap();

    let mut fixed = Vec::new();
    u32s(
        &mut fixed,
        &[
            0xFEEF_04BD,           // signature
            0x0001_0000,           // structure version
            (major << 16) | minor, // file version
            patch << 16,
            (major << 16) | minor, // product version
            patch << 16,
            0x3F,        // flags mask
            0,           // flags
            0x0004_0004, // VOS_NT_WINDOWS32
            1,           // VFT_APP
            0,
            0,
            0,
        ],
    );

    let table = node(
        "040904B0",
        &[],
        0,
        true,
        &[
            string("CompanyName", &authors),
            string("FileDescription", &env::var("CARGO_PKG_DESCRIPTION").unwrap()),
            string("FileVersion", &version),
            string("InternalName", "ameter"),
            string("LegalCopyright", &format!("Copyright (c) 2026 {authors}")),
            string("ProductName", "ameter"),
            string("ProductVersion", &version),
        ],
    );
    let strings = node("StringFileInfo", &[], 0, true, &[table]);
    let translation = node("Translation", &[0x09, 0x04, 0xB0, 0x04], 4, false, &[]);
    let vars = node("VarFileInfo", &[], 0, true, &[translation]);
    node("VS_VERSION_INFO", &fixed, fixed.len() as u16, false, &[strings, vars])
}

// ---------------------------------------------------------------- COFF object

/// A one-section COFF object holding the resource tree (type / name / language) and the data.
/// Data entries hold section offsets fixed up to image RVAs by ADDR32NB relocations.
fn coff(machine: u16, mut resources: Vec<Resource>) -> Vec<u8> {
    resources.sort_by_key(|r| (r.kind, r.name, r.lang));

    // Group into type -> name -> [resource index].
    type Names = Vec<(u32, Vec<usize>)>;
    let mut tree: Vec<(u32, Names)> = Vec::new();
    for (i, r) in resources.iter().enumerate() {
        if tree.last().is_none_or(|t| t.0 != r.kind) {
            tree.push((r.kind, Vec::new()));
        }
        let names = &mut tree.last_mut().unwrap().1;
        if names.last().is_none_or(|n| n.0 != r.name) {
            names.push((r.name, Vec::new()));
        }
        names.last_mut().unwrap().1.push(i);
    }

    let dir_size = |entries: usize| 16 + 8 * entries as u32;
    // Offsets: root, type directories, name directories, then data entries, then data.
    let mut offset = dir_size(tree.len());
    let mut type_dirs = Vec::new();
    for (_, names) in &tree {
        type_dirs.push(offset);
        offset += dir_size(names.len());
    }
    let mut name_dirs = Vec::new();
    for (_, names) in &tree {
        let mut dirs = Vec::new();
        for (_, langs) in names {
            dirs.push(offset);
            offset += dir_size(langs.len());
        }
        name_dirs.push(dirs);
    }
    let entries_at = offset;
    let mut data_at = Vec::new();
    let mut cursor = entries_at + 16 * resources.len() as u32;
    for r in &resources {
        cursor = cursor.next_multiple_of(8);
        data_at.push(cursor);
        cursor += r.data.len() as u32;
    }

    const SUBDIRECTORY: u32 = 0x8000_0000;
    let directory = |buf: &mut Vec<u8>, entries: &[(u32, u32)]| {
        u32s(buf, &[0, 0, 0]); // characteristics, timestamp, major/minor version
        u16s(buf, &[0, entries.len() as u16]); // named entries, id entries
        for (id, target) in entries {
            u32s(buf, &[*id, *target]);
        }
    };

    let mut section = Vec::new();
    directory(&mut section, &tree.iter().zip(&type_dirs).map(|((kind, _), at)| (*kind, at | SUBDIRECTORY)).collect::<Vec<_>>());
    for (t, (_, names)) in tree.iter().enumerate() {
        directory(&mut section, &names.iter().zip(&name_dirs[t]).map(|((name, _), at)| (*name, at | SUBDIRECTORY)).collect::<Vec<_>>());
    }
    for (_, names) in &tree {
        for (_, langs) in names {
            directory(&mut section, &langs.iter().map(|&i| (resources[i].lang, entries_at + 16 * i as u32)).collect::<Vec<_>>());
        }
    }
    let mut relocations = Vec::new();
    for (i, r) in resources.iter().enumerate() {
        relocations.push(section.len() as u32);
        u32s(&mut section, &[data_at[i], r.data.len() as u32, 0, 0]);
    }
    for (i, r) in resources.iter().enumerate() {
        pad(&mut section, 8);
        assert_eq!(section.len() as u32, data_at[i]);
        section.extend(&r.data);
    }
    pad(&mut section, 4);

    const HEADERS: u32 = 20 + 40;
    let relocations_at = HEADERS + section.len() as u32;
    let symbols_at = relocations_at + 10 * relocations.len() as u32;
    let reloc_type: u16 = match machine {
        0x8664 => 3, // IMAGE_REL_AMD64_ADDR32NB
        0xAA64 => 2, // IMAGE_REL_ARM64_ADDR32NB
        _ => 7,      // IMAGE_REL_I386_DIR32NB
    };

    let mut obj = Vec::new();
    u16s(&mut obj, &[machine, 1]);
    u32s(&mut obj, &[0, symbols_at, 2]); // timestamp, symbol table, symbols (section + aux)
    u16s(&mut obj, &[0, 0]); // optional header size, characteristics
    obj.extend(b".rsrc\0\0\0");
    u32s(&mut obj, &[0, 0, section.len() as u32, HEADERS, relocations_at, 0]);
    u16s(&mut obj, &[relocations.len() as u16, 0]);
    u32s(&mut obj, &[0x4030_0040]); // initialized data, 4-byte aligned, read
    obj.extend(&section);
    for at in &relocations {
        u32s(&mut obj, &[*at, 0]); // offset, symbol 0 (the section)
        u16s(&mut obj, &[reloc_type]);
    }
    // Section symbol with its section-definition auxiliary record.
    obj.extend(b".rsrc\0\0\0");
    u32s(&mut obj, &[0]);
    u16s(&mut obj, &[1, 0]); // section number, type
    obj.extend([3, 1]); // IMAGE_SYM_CLASS_STATIC, one aux record
    u32s(&mut obj, &[section.len() as u32]);
    u16s(&mut obj, &[relocations.len() as u16, 0]);
    u32s(&mut obj, &[0]); // checksum
    u16s(&mut obj, &[0]); // number
    obj.extend([0, 0, 0, 0]); // selection + padding
    u32s(&mut obj, &[4]); // empty string table
    obj
}

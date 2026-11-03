//! Minimal in-place Mach-O editing for tweak injection: list/add/rewrite dylib load commands,
//! add rpaths, and detect FairPlay encryption. Works on thin and fat (universal) binaries.
//!
//! Load commands are added in the padding between the load command area and the first section,
//! like `insert_dylib`/`optool`, so file offsets never move. The app is re-signed afterwards.

use anyhow::{Result, bail};

const FAT_MAGIC: u32 = 0xcafe_babe;
const FAT_MAGIC_64: u32 = 0xcafe_babf;
const MH_MAGIC: u32 = 0xfeed_face;
const MH_MAGIC_64: u32 = 0xfeed_facf;

const LC_SEGMENT: u32 = 0x1;
const LC_SEGMENT_64: u32 = 0x19;
pub const LC_LOAD_DYLIB: u32 = 0xc;
const LC_ID_DYLIB: u32 = 0xd;
pub const LC_LOAD_WEAK_DYLIB: u32 = 0x8000_0018;
const LC_REEXPORT_DYLIB: u32 = 0x8000_001f;
const LC_LAZY_LOAD_DYLIB: u32 = 0x20;
const LC_LOAD_UPWARD_DYLIB: u32 = 0x8000_0023;
const LC_RPATH: u32 = 0x8000_001c;
const LC_ENCRYPTION_INFO: u32 = 0x21;
const LC_ENCRYPTION_INFO_64: u32 = 0x2c;

const DYLIB_LOADS: &[u32] = &[
    LC_LOAD_DYLIB,
    LC_LOAD_WEAK_DYLIB,
    LC_REEXPORT_DYLIB,
    LC_LAZY_LOAD_DYLIB,
    LC_LOAD_UPWARD_DYLIB,
];

fn u32_le(d: &[u8], off: usize) -> Result<u32> {
    match d.get(off..off + 4) {
        Some(b) => Ok(u32::from_le_bytes(b.try_into().expect("4 bytes"))),
        None => bail!("Truncated Mach-O"),
    }
}

fn u32_be(d: &[u8], off: usize) -> Result<u32> {
    match d.get(off..off + 4) {
        Some(b) => Ok(u32::from_be_bytes(b.try_into().expect("4 bytes"))),
        None => bail!("Truncated Mach-O"),
    }
}

fn u64_be(d: &[u8], off: usize) -> Result<u64> {
    match d.get(off..off + 8) {
        Some(b) => Ok(u64::from_be_bytes(b.try_into().expect("8 bytes"))),
        None => bail!("Truncated Mach-O"),
    }
}

fn put_u32(d: &mut [u8], off: usize, v: u32) {
    d[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn is_macho(data: &[u8]) -> bool {
    matches!(u32_be(data, 0), Ok(FAT_MAGIC | FAT_MAGIC_64))
        || matches!(u32_le(data, 0), Ok(MH_MAGIC | MH_MAGIC_64))
}

/// `(offset, size)` of each architecture slice.
fn slices(data: &[u8]) -> Result<Vec<(usize, usize)>> {
    match u32_be(data, 0)? {
        FAT_MAGIC => {
            let n = u32_be(data, 4)? as usize;
            (0..n)
                .map(|i| {
                    let base = 8 + i * 20;
                    Ok((
                        u32_be(data, base + 8)? as usize,
                        u32_be(data, base + 12)? as usize,
                    ))
                })
                .collect()
        }
        FAT_MAGIC_64 => {
            let n = u32_be(data, 4)? as usize;
            (0..n)
                .map(|i| {
                    let base = 8 + i * 32;
                    Ok((
                        u64_be(data, base + 8)? as usize,
                        u64_be(data, base + 16)? as usize,
                    ))
                })
                .collect()
        }
        _ => Ok(vec![(0, data.len())]),
    }
}

struct Header {
    is64: bool,
    ncmds: u32,
    sizeofcmds: u32,
}

impl Header {
    fn size(&self) -> usize {
        if self.is64 { 32 } else { 28 }
    }

    fn align(&self) -> usize {
        if self.is64 { 8 } else { 4 }
    }
}

fn header(slice: &[u8]) -> Result<Header> {
    let is64 = match u32_le(slice, 0)? {
        MH_MAGIC_64 => true,
        MH_MAGIC => false,
        other => bail!("Not a little-endian Mach-O (magic {other:#x})"),
    };
    Ok(Header {
        is64,
        ncmds: u32_le(slice, 16)?,
        sizeofcmds: u32_le(slice, 20)?,
    })
}

/// `(offset, cmd, cmdsize)` of every load command in a slice.
fn load_commands(slice: &[u8], h: &Header) -> Result<Vec<(usize, u32, usize)>> {
    let mut out = Vec::with_capacity(h.ncmds as usize);
    let mut off = h.size();
    let end = h.size() + h.sizeofcmds as usize;
    for _ in 0..h.ncmds {
        let cmd = u32_le(slice, off)?;
        let size = u32_le(slice, off + 4)? as usize;
        if size < 8 || off + size > end {
            bail!("Corrupt load command at {off:#x}");
        }
        out.push((off, cmd, size));
        off += size;
    }
    Ok(out)
}

fn c_string(slice: &[u8], start: usize, end: usize) -> String {
    let bytes = &slice[start.min(end)..end];
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..len]).into_owned()
}

/// File offset of the first section/segment data, i.e. where free load-command space ends.
fn first_data_offset(slice: &[u8], h: &Header, cmds: &[(usize, u32, usize)]) -> Result<usize> {
    let mut limit = slice.len();
    for &(off, cmd, _) in cmds {
        let (nsects_off, sect_start, sect_size, sect_offset_field) = match cmd {
            LC_SEGMENT_64 => (off + 64, off + 72, 80, 48),
            LC_SEGMENT => (off + 48, off + 56, 68, 40),
            _ => continue,
        };
        let nsects = u32_le(slice, nsects_off)? as usize;
        for i in 0..nsects {
            let s = sect_start + i * sect_size;
            let flags = u32_le(slice, s + sect_size - if h.is64 { 16 } else { 12 })?;
            // Zero-fill sections (S_ZEROFILL=1, S_GB_ZEROFILL=0xc, S_THREAD_LOCAL_ZEROFILL=0x12) have no file data
            let section_type = flags & 0xff;
            if matches!(section_type, 0x1 | 0xc | 0x12) {
                continue;
            }
            let offset = u32_le(slice, s + sect_offset_field)? as usize;
            if offset > 0 {
                limit = limit.min(offset);
            }
        }
    }
    Ok(limit)
}

/// Dylibs each slice links (first slice's list; slices of one binary link the same libraries).
pub fn linked_dylibs(data: &[u8]) -> Result<Vec<String>> {
    let &(start, size) = slices(data)?
        .first()
        .ok_or_else(|| anyhow::anyhow!("Empty fat binary"))?;
    let slice = &data[start..start + size];
    let h = header(slice)?;
    let mut out = Vec::new();
    for (off, cmd, size) in load_commands(slice, &h)? {
        if DYLIB_LOADS.contains(&cmd) {
            let name_off = u32_le(slice, off + 8)? as usize;
            out.push(c_string(slice, off + name_off, off + size));
        }
    }
    Ok(out)
}

/// True if any slice has a non-zero `cryptid` (App Store FairPlay encryption).
pub fn is_encrypted(data: &[u8]) -> Result<bool> {
    for (start, size) in slices(data)? {
        let slice = &data[start..start + size];
        let h = header(slice)?;
        for (off, cmd, _) in load_commands(slice, &h)? {
            if (cmd == LC_ENCRYPTION_INFO || cmd == LC_ENCRYPTION_INFO_64)
                && u32_le(slice, off + 16)? != 0
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn insert_command(slice: &mut [u8], command: &[u8]) -> Result<()> {
    let h = header(slice)?;
    let cmds = load_commands(slice, &h)?;
    let end = h.size() + h.sizeofcmds as usize;
    let limit = first_data_offset(slice, &h, &cmds)?;
    if end + command.len() > limit {
        bail!(
            "Not enough free space in the Mach-O header to add a load command (need {} bytes, have {})",
            command.len(),
            limit.saturating_sub(end)
        );
    }
    if slice[end..end + command.len()].iter().any(|&b| b != 0) {
        bail!("Mach-O header padding is not empty; refusing to overwrite it");
    }
    slice[end..end + command.len()].copy_from_slice(command);
    put_u32(slice, 16, h.ncmds + 1);
    put_u32(slice, 20, h.sizeofcmds + command.len() as u32);
    Ok(())
}

fn build_path_command(
    cmd: u32,
    header_len: usize,
    path: &str,
    align: usize,
    extra: &[u32],
) -> Vec<u8> {
    let raw = header_len + path.len() + 1;
    let size = raw.div_ceil(align) * align;
    let mut buf = vec![0u8; size];
    put_u32(&mut buf, 0, cmd);
    put_u32(&mut buf, 4, size as u32);
    put_u32(&mut buf, 8, header_len as u32);
    for (i, v) in extra.iter().enumerate() {
        put_u32(&mut buf, 12 + i * 4, *v);
    }
    buf[header_len..header_len + path.len()].copy_from_slice(path.as_bytes());
    buf
}

const LC_CODE_SIGNATURE: u32 = 0x1d;
const CSMAGIC_EMBEDDED_SIGNATURE: u32 = 0xfade_0cc0;
const CSMAGIC_CODEDIRECTORY: u32 = 0xfade_0c02;
const CSMAGIC_EMBEDDED_ENTITLEMENTS: u32 = 0xfade_7171;
const CSMAGIC_BLOBWRAPPER: u32 = 0xfade_0b01;

/// What `codesign -d` / `ldid -e` would report about a binary's signature.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct CodeSignature {
    /// Signing identifier (usually the bundle id).
    pub identifier: Option<String>,
    pub team_id: Option<String>,
    pub entitlements: Option<plist::Dictionary>,
    /// False for ad-hoc / pseudo-signed binaries (no certificate).
    pub has_certificate: bool,
}

/// Read the embedded code signature of the first slice, or `None` if unsigned.
pub fn code_signature(data: &[u8]) -> Result<Option<CodeSignature>> {
    let &(start, size) = slices(data)?
        .first()
        .ok_or_else(|| anyhow::anyhow!("Empty fat binary"))?;
    let slice = &data[start..start + size];
    let h = header(slice)?;
    let Some((off, _, _)) = load_commands(slice, &h)?
        .into_iter()
        .find(|(_, cmd, _)| *cmd == LC_CODE_SIGNATURE)
    else {
        return Ok(None);
    };
    let sig_off = u32_le(slice, off + 8)? as usize;
    let sig_len = u32_le(slice, off + 12)? as usize;
    let Some(blob) = slice.get(sig_off..sig_off + sig_len) else {
        bail!("Code signature points outside the binary");
    };
    if u32_be(blob, 0)? != CSMAGIC_EMBEDDED_SIGNATURE {
        bail!("Unknown code signature format");
    }
    let mut sig = CodeSignature::default();
    let count = u32_be(blob, 8)? as usize;
    for i in 0..count {
        let entry = 12 + i * 8;
        let blob_off = u32_be(blob, entry + 4)? as usize;
        let magic = u32_be(blob, blob_off)?;
        let len = u32_be(blob, blob_off + 4)? as usize;
        let Some(body) = blob.get(blob_off..blob_off + len) else {
            continue;
        };
        match magic {
            CSMAGIC_CODEDIRECTORY if sig.identifier.is_none() => {
                let version = u32_be(body, 8)?;
                let ident = u32_be(body, 20)? as usize;
                sig.identifier = Some(c_string(body, ident, body.len()));
                if version >= 0x20200 {
                    let team = u32_be(body, 48)? as usize;
                    if team != 0 {
                        sig.team_id =
                            Some(c_string(body, team, body.len())).filter(|t| !t.is_empty());
                    }
                }
            }
            CSMAGIC_EMBEDDED_ENTITLEMENTS => {
                sig.entitlements = plist::Value::from_reader_xml(&body[8..])
                    .ok()
                    .and_then(plist::Value::into_dictionary);
            }
            // An empty wrapper (8-byte header only) means ad-hoc
            CSMAGIC_BLOBWRAPPER => sig.has_certificate = len > 8,
            _ => {}
        }
    }
    Ok(Some(sig))
}

/// Add an `LC_LOAD_DYLIB` (or weak) command for `path` to every slice.
/// Returns `false` if the binary already loads it.
pub fn add_dylib(data: &mut [u8], path: &str, weak: bool) -> Result<bool> {
    if linked_dylibs(data)?.iter().any(|p| p == path) {
        return Ok(false);
    }
    let cmd = if weak {
        LC_LOAD_WEAK_DYLIB
    } else {
        LC_LOAD_DYLIB
    };
    for (start, size) in slices(data)? {
        let slice = &mut data[start..start + size];
        let align = header(slice)?.align();
        // dylib_command: cmd, cmdsize, name.offset, timestamp, current_version, compatibility_version
        let command = build_path_command(cmd, 24, path, align, &[2, 0x10000, 0x10000]);
        insert_command(slice, &command)?;
    }
    Ok(true)
}

pub fn rpaths(data: &[u8]) -> Result<Vec<String>> {
    let &(start, size) = slices(data)?
        .first()
        .ok_or_else(|| anyhow::anyhow!("Empty fat binary"))?;
    let slice = &data[start..start + size];
    let h = header(slice)?;
    let mut out = Vec::new();
    for (off, cmd, size) in load_commands(slice, &h)? {
        if cmd == LC_RPATH {
            let path_off = u32_le(slice, off + 8)? as usize;
            out.push(c_string(slice, off + path_off, off + size));
        }
    }
    Ok(out)
}

/// Add an `LC_RPATH` to every slice unless it's already there.
pub fn add_rpath(data: &mut [u8], path: &str) -> Result<bool> {
    if rpaths(data)?.iter().any(|p| p == path) {
        return Ok(false);
    }
    for (start, size) in slices(data)? {
        let slice = &mut data[start..start + size];
        let align = header(slice)?.align();
        let command = build_path_command(LC_RPATH, 12, path, align, &[]);
        insert_command(slice, &command)?;
    }
    Ok(true)
}

/// Point dylib references matching `old` at `new`, in place. Fails if `new` doesn't fit in the
/// existing command. Returns how many commands changed (counted on the first slice).
pub fn change_dylib_path(data: &mut [u8], old: &str, new: &str) -> Result<usize> {
    let mut changed_first = 0;
    for (i, (start, size)) in slices(data)?.into_iter().enumerate() {
        let slice = &mut data[start..start + size];
        let h = header(slice)?;
        for (off, cmd, size) in load_commands(slice, &h)? {
            if !(DYLIB_LOADS.contains(&cmd) || cmd == LC_ID_DYLIB) {
                continue;
            }
            let name_off = u32_le(slice, off + 8)? as usize;
            if c_string(slice, off + name_off, off + size) != old {
                continue;
            }
            let space = size - name_off;
            if new.len() + 1 > space {
                bail!(
                    "\"{new}\" is too long to replace \"{old}\" in place ({space} bytes available)"
                );
            }
            let dst = &mut slice[off + name_off..off + size];
            dst.fill(0);
            dst[..new.len()].copy_from_slice(new.as_bytes());
            if i == 0 {
                changed_first += 1;
            }
        }
    }
    Ok(changed_first)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A tiny arm64 Mach-O: header, one __TEXT segment with one section at file offset 0x400,
    /// one LC_LOAD_DYLIB for libSystem, and an LC_ENCRYPTION_INFO_64 with the given cryptid.
    pub(crate) fn sample_macho(cryptid: u32) -> Vec<u8> {
        let mut d = vec![0u8; 0x800];
        put_u32(&mut d, 0, MH_MAGIC_64);
        put_u32(&mut d, 4, 0x0100_000c); // CPU_TYPE_ARM64
        put_u32(&mut d, 12, 2); // MH_EXECUTE
        let mut off = 32;
        // LC_SEGMENT_64 with one section
        let seg_size = 72 + 80;
        put_u32(&mut d, off, LC_SEGMENT_64);
        put_u32(&mut d, off + 4, seg_size as u32);
        d[off + 8..off + 14].copy_from_slice(b"__TEXT");
        put_u32(&mut d, off + 64, 1); // nsects
        let sect = off + 72;
        d[sect..sect + 6].copy_from_slice(b"__text");
        put_u32(&mut d, sect + 48, 0x400); // offset
        off += seg_size;
        // LC_LOAD_DYLIB
        let dylib = build_path_command(
            LC_LOAD_DYLIB,
            24,
            "/usr/lib/libSystem.B.dylib",
            8,
            &[2, 0, 0],
        );
        d[off..off + dylib.len()].copy_from_slice(&dylib);
        off += dylib.len();
        // LC_ENCRYPTION_INFO_64
        put_u32(&mut d, off, LC_ENCRYPTION_INFO_64);
        put_u32(&mut d, off + 4, 24);
        put_u32(&mut d, off + 16, cryptid);
        off += 24;
        put_u32(&mut d, 16, 3);
        put_u32(&mut d, 20, (off - 32) as u32);
        d
    }

    fn fat(slices_data: &[Vec<u8>]) -> Vec<u8> {
        let mut out = vec![0u8; 0x1000];
        out[0..4].copy_from_slice(&FAT_MAGIC.to_be_bytes());
        out[4..8].copy_from_slice(&(slices_data.len() as u32).to_be_bytes());
        for (i, s) in slices_data.iter().enumerate() {
            let offset = out.len();
            let base = 8 + i * 20;
            out[base + 8..base + 12].copy_from_slice(&(offset as u32).to_be_bytes());
            out[base + 12..base + 16].copy_from_slice(&(s.len() as u32).to_be_bytes());
            out.extend_from_slice(s);
        }
        out
    }

    /// Append a code signature with a CodeDirectory, entitlements and an optional CMS blob.
    pub(crate) fn with_signature(
        mut d: Vec<u8>,
        ident: &str,
        team: &str,
        ents: &str,
        cms: bool,
    ) -> Vec<u8> {
        fn be(v: &mut Vec<u8>, x: u32) {
            v.extend_from_slice(&x.to_be_bytes());
        }
        // CodeDirectory v0x20200 with identifier and team strings right after an 88-byte header
        let mut cd = Vec::new();
        let ident_off = 88u32;
        let team_off = ident_off + ident.len() as u32 + 1;
        be(&mut cd, CSMAGIC_CODEDIRECTORY);
        be(&mut cd, 0); // length, patched below
        be(&mut cd, 0x20200);
        be(&mut cd, 0);
        be(&mut cd, 0);
        be(&mut cd, ident_off);
        cd.resize(48, 0);
        be(&mut cd, team_off);
        cd.resize(88, 0);
        cd.extend_from_slice(ident.as_bytes());
        cd.push(0);
        cd.extend_from_slice(team.as_bytes());
        cd.push(0);
        let cd_len = cd.len() as u32;
        cd[4..8].copy_from_slice(&cd_len.to_be_bytes());

        let mut ent = Vec::new();
        be(&mut ent, CSMAGIC_EMBEDDED_ENTITLEMENTS);
        be(&mut ent, 8 + ents.len() as u32);
        ent.extend_from_slice(ents.as_bytes());

        let mut wrapper = Vec::new();
        be(&mut wrapper, CSMAGIC_BLOBWRAPPER);
        be(&mut wrapper, if cms { 12 } else { 8 });
        if cms {
            wrapper.extend_from_slice(&[1, 2, 3, 4]);
        }

        let blobs = [(0u32, cd), (5, ent), (0x10000, wrapper)];
        let mut sb = Vec::new();
        be(&mut sb, CSMAGIC_EMBEDDED_SIGNATURE);
        be(&mut sb, 0);
        be(&mut sb, blobs.len() as u32);
        let mut offset = 12 + blobs.len() as u32 * 8;
        for (kind, b) in &blobs {
            be(&mut sb, *kind);
            be(&mut sb, offset);
            offset += b.len() as u32;
        }
        for (_, b) in &blobs {
            sb.extend_from_slice(b);
        }
        let sb_len = sb.len() as u32;
        sb[4..8].copy_from_slice(&sb_len.to_be_bytes());

        let data_off = d.len() as u32;
        d.extend_from_slice(&sb);
        let mut cmd = vec![0u8; 16];
        put_u32(&mut cmd, 0, LC_CODE_SIGNATURE);
        put_u32(&mut cmd, 4, 16);
        put_u32(&mut cmd, 8, data_off);
        put_u32(&mut cmd, 12, sb_len);
        insert_command(&mut d, &cmd).unwrap();
        d
    }

    #[test]
    fn reads_code_signature() {
        let ents = r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>get-task-allow</key><true/></dict></plist>"#;
        let signed = with_signature(
            sample_macho(0),
            "com.example.demo",
            "ABCDE12345",
            ents,
            true,
        );
        let sig = code_signature(&signed).unwrap().unwrap();
        assert_eq!(sig.identifier.as_deref(), Some("com.example.demo"));
        assert_eq!(sig.team_id.as_deref(), Some("ABCDE12345"));
        assert!(sig.has_certificate);
        let e = sig.entitlements.unwrap();
        assert_eq!(
            e.get("get-task-allow").and_then(plist::Value::as_boolean),
            Some(true)
        );

        let adhoc = with_signature(sample_macho(0), "x", "", ents, false);
        let sig = code_signature(&adhoc).unwrap().unwrap();
        assert!(!sig.has_certificate);
        assert_eq!(sig.team_id, None);
        assert_eq!(code_signature(&sample_macho(0)).unwrap(), None);
    }

    #[test]
    fn detects_encryption() {
        assert!(is_encrypted(&sample_macho(1)).unwrap());
        assert!(!is_encrypted(&sample_macho(0)).unwrap());
        assert!(is_macho(&sample_macho(0)));
        assert!(!is_macho(b"PK\x03\x04"));
    }

    #[test]
    fn adds_dylib_once() {
        let mut d = sample_macho(0);
        assert!(add_dylib(&mut d, "@executable_path/Frameworks/Tweak.dylib", false).unwrap());
        assert!(!add_dylib(&mut d, "@executable_path/Frameworks/Tweak.dylib", false).unwrap());
        assert_eq!(
            linked_dylibs(&d).unwrap(),
            vec![
                "/usr/lib/libSystem.B.dylib",
                "@executable_path/Frameworks/Tweak.dylib"
            ]
        );
    }

    #[test]
    fn adds_rpath_and_rewrites_paths_in_fat_binaries() {
        let mut d = fat(&[sample_macho(0), sample_macho(0)]);
        assert!(add_rpath(&mut d, "@executable_path/Frameworks").unwrap());
        assert_eq!(rpaths(&d).unwrap(), vec!["@executable_path/Frameworks"]);
        assert_eq!(
            change_dylib_path(&mut d, "/usr/lib/libSystem.B.dylib", "@rpath/libS.dylib").unwrap(),
            1
        );
        assert_eq!(linked_dylibs(&d).unwrap(), vec!["@rpath/libS.dylib"]);
        // second slice was rewritten too
        let (start, size) = slices(&d).unwrap()[1];
        assert_eq!(
            linked_dylibs(&d[start..start + size]).unwrap(),
            vec!["@rpath/libS.dylib"]
        );
    }

    #[test]
    fn refuses_paths_that_do_not_fit() {
        let mut d = sample_macho(0);
        let long = format!("@rpath/{}", "x".repeat(64));
        assert!(change_dylib_path(&mut d, "/usr/lib/libSystem.B.dylib", &long).is_err());
    }

    #[test]
    fn refuses_when_header_is_full() {
        let mut d = sample_macho(0);
        let huge = format!("@executable_path/{}", "y".repeat(0x400));
        assert!(add_dylib(&mut d, &huge, false).is_err());
    }
}

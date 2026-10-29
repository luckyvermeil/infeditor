//! Win32 resource emission for the build script: ICO reading and `.res`
//! writing.
//!
//! `build.rs` writes the executable's icon resource by hand instead of
//! compiling a `.rc` with `rc.exe`, so this byte format is part of the build.
//! The module lives in the library (and is pulled into the build script with
//! `#[path]`) purely so it can be **tested**: a build script's own tests are
//! never run by `cargo test`, which left the format checks (offsets, the null
//! resource, DWORD alignment) unverified. A mistake there is not a crash but a
//! silent absence of the icon, or LNK1136 at link time.
//!
//! Nothing here touches the filesystem or the environment: it is pure byte
//! munging, which is also why it compiles unchanged on non-Windows hosts.

/// `RT_ICON`: one image of the icon.
pub const RT_ICON: u16 = 3;
/// `RT_GROUP_ICON`: the directory that binds the images into one icon.
pub const RT_GROUP_ICON: u16 = 14;

/// One image of the `.ico`, ready to become an [`RT_ICON`] resource.
pub struct IcoImage {
    /// Encoded width in the group directory: `0` means 256.
    pub width: u8,
    /// Encoded height in the group directory: `0` means 256.
    pub height: u8,
    /// The frame's bytes, verbatim (PNG-compressed frames stay compressed —
    /// a `.res` stores them as-is).
    pub data: Vec<u8>,
}

/// Minimal ICO reader: the directory only, no decoding.
///
/// Panics on a truncated file rather than guessing: this runs in the build
/// script, where a malformed asset should stop the build with a message naming
/// the file, not produce an executable with a broken icon.
pub fn parse_ico(bytes: &[u8]) -> Vec<IcoImage> {
    assert!(bytes.len() >= 6, "truncated ico header");
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let at = 6 + i * 16;
        let entry = bytes.get(at..at + 16).expect("truncated ico directory");
        let size = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize;
        let offset = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize;
        let data = bytes
            .get(offset..offset + size)
            .expect("ico image out of bounds")
            .to_vec();
        out.push(IcoImage {
            width: entry[0],
            height: entry[1],
            data,
        });
    }
    out
}

/// `GRPICONDIR` + `GRPICONDIRENTRY[]` for `images`, whose resource ids are
/// 1-based and local to the `.res` file.
///
/// The group entry is what Explorer reads when it wants "the icon of this exe";
/// the individual frames are what it draws at 16/32/48/256 px. Both resource
/// types must be present — a lone [`RT_GROUP_ICON`] renders as a blank icon,
/// and lone [`RT_ICON`]s are not reachable by name.
pub fn group_icon(images: &[IcoImage]) -> Vec<u8> {
    let mut group = Vec::with_capacity(6 + 14 * images.len());
    group.extend(0u16.to_le_bytes()); // reserved
    group.extend(1u16.to_le_bytes()); // type: icon
    group.extend((images.len() as u16).to_le_bytes());
    for (i, img) in images.iter().enumerate() {
        group.extend([img.width, img.height, 0, 0]); // w, h, colour count, reserved
        group.extend(1u16.to_le_bytes()); // planes
        group.extend(32u16.to_le_bytes()); // bits per pixel
        group.extend((img.data.len() as u32).to_le_bytes());
        group.extend(((i + 1) as u16).to_le_bytes()); // resource id
    }
    group
}

/// A `.res` (win32 resource file) being assembled.
///
/// Layout per resource: one 32-byte header (type and name both ordinals, so no
/// Unicode strings follow the counts) + the payload, each DWORD-aligned.
#[derive(Default)]
pub struct ResFile {
    bytes: Vec<u8>,
}

impl ResFile {
    /// Starts the file with the mandatory **null resource** (all-zero header).
    ///
    /// `rc.exe` always emits it and `cvtres` (which the linker runs on `.res`
    /// input) treats it as the file signature: without it the file is rejected
    /// with LNK1136 "invalid or corrupted file", even though every following
    /// record is well formed.
    pub fn new() -> Self {
        let mut file = Self::default();
        file.bytes.extend(0u32.to_le_bytes()); // DataSize: 0
        file.bytes.extend(32u32.to_le_bytes()); // HeaderSize
        file.bytes.extend(0xFFFFu16.to_le_bytes()); // Type: ordinal 0
        file.bytes.extend(0u16.to_le_bytes());
        file.bytes.extend(0xFFFFu16.to_le_bytes()); // Name: ordinal 0
        file.bytes.extend(0u16.to_le_bytes());
        file.bytes.extend([0u8; 16]); // DataVersion, flags, language, version, chars
        file
    }

    /// Append one resource. `ty` is an `RT_*` ordinal, `name` its id.
    pub fn push(&mut self, ty: u16, name: u16, data: &[u8]) {
        // Same flags rc.exe picks: MOVEABLE|DISCARDABLE for the frames, plus
        // PURE on the group. Kept identical so this module's output can be
        // byte-compared against `rc.exe` when the format is ever doubted.
        let flags: u16 = if ty == RT_GROUP_ICON { 0x1030 } else { 0x1010 };
        let header = {
            let mut h = Vec::with_capacity(32);
            h.extend((data.len() as u32).to_le_bytes()); // DataSize
            h.extend(32u32.to_le_bytes()); // HeaderSize
            h.extend(0xFFFFu16.to_le_bytes()); // Type is an ordinal…
            h.extend(ty.to_le_bytes()); // …this one
            h.extend(0xFFFFu16.to_le_bytes()); // Name is an ordinal…
            h.extend(name.to_le_bytes()); // …this one
            h.extend(0u32.to_le_bytes()); // DataVersion
            h.extend(flags.to_le_bytes());
            h.extend(0x0409u16.to_le_bytes()); // LanguageId: en-US, what rc.exe defaults to
            h.extend(0u32.to_le_bytes()); // Version
            h.extend(0u32.to_le_bytes()); // Characteristics
            h
        };
        debug_assert_eq!(header.len(), 32);
        self.bytes.extend(header);
        self.bytes.extend(data);
        while self.bytes.len() % 4 != 0 {
            self.bytes.push(0);
        }
    }

    /// The complete file.
    pub fn bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny well-formed ICO: one 1×1 BGRA frame.
    fn one_image_ico() -> Vec<u8> {
        let frame = [0xAAu8, 0xBB, 0xCC, 0xDD];
        let mut ico = Vec::new();
        ico.extend(0u16.to_le_bytes()); // reserved
        ico.extend(1u16.to_le_bytes()); // type: icon
        ico.extend(1u16.to_le_bytes()); // count
        ico.extend([1u8, 1, 0, 0]); // w, h, colours, reserved
        ico.extend(1u16.to_le_bytes()); // planes
        ico.extend(32u16.to_le_bytes()); // bpp
        ico.extend((frame.len() as u32).to_le_bytes());
        ico.extend(22u32.to_le_bytes()); // offset: 6 + 16
        ico.extend(frame);
        ico
    }

    #[test]
    fn ico_directory_offsets_are_honoured() {
        let images = parse_ico(&one_image_ico());
        assert_eq!(images.len(), 1);
        assert_eq!((images[0].width, images[0].height), (1, 1));
        assert_eq!(images[0].data, [0xAA, 0xBB, 0xCC, 0xDD]);
    }

    /// 256 px frames are encoded as `0` in the directory byte and must not be
    /// confused with "no frame".
    #[test]
    fn a_256px_frame_is_reported_as_zero_not_dropped() {
        let mut ico = one_image_ico();
        ico[6] = 0; // width byte
        ico[7] = 0; // height byte
        let images = parse_ico(&ico);
        assert_eq!(images.len(), 1);
        assert_eq!((images[0].width, images[0].height), (0, 0));
    }

    /// The group directory must list one entry per frame with the resource id
    /// the `.res` gives that frame — a mismatch is a blank icon at that size.
    #[test]
    fn group_directory_matches_the_resource_ids() {
        let images = parse_ico(&one_image_ico());
        let group = group_icon(&images);
        assert_eq!(group.len(), 6 + 14 * images.len());
        assert_eq!(&group[0..2], &0u16.to_le_bytes()); // reserved
        assert_eq!(&group[2..4], &1u16.to_le_bytes()); // type: icon
        assert_eq!(&group[4..6], &1u16.to_le_bytes()); // count
                                                       // First entry: w, h, colours, reserved, planes, bpp, size, id.
        assert_eq!(group[6], 1);
        assert_eq!(group[7], 1);
        assert_eq!(&group[10..12], &1u16.to_le_bytes());
        assert_eq!(&group[12..14], &32u16.to_le_bytes());
        assert_eq!(&group[14..18], &4u32.to_le_bytes());
        assert_eq!(&group[18..20], &1u16.to_le_bytes());

        // And the res file gives the frames those very ids.
        let mut res = ResFile::new();
        for (i, img) in images.iter().enumerate() {
            res.push(RT_ICON, (i + 1) as u16, &img.data);
        }
        res.push(RT_GROUP_ICON, 1, &group);
        let bytes = res.bytes();
        // Second record (past the 32-byte null resource + the first 32-byte
        // header + 4 bytes of frame) is the group resource, named 1.
        let null_res = 32;
        let first = null_res + 32 + 4;
        assert_eq!(
            &bytes[first..first + 4],
            &(group.len() as u32).to_le_bytes()
        );
        // Header offsets: type ordinal +8, type +10, data version +16, flags
        // +20 (MOVEABLE|DISCARDABLE|PURE for the group), language +22.
        assert_eq!(&bytes[first + 8..first + 10], &0xFFFFu16.to_le_bytes());
        assert_eq!(&bytes[first + 10..first + 12], &RT_GROUP_ICON.to_le_bytes());
        assert_eq!(&bytes[first + 20..first + 22], &0x1030u16.to_le_bytes());
        assert_eq!(&bytes[first + 22..first + 24], &0x0409u16.to_le_bytes());
    }

    /// LNK1136: `cvtres` reads the leading record as the file signature, so the
    /// first 32 bytes must be the all-zero resource, and every record must be
    /// DWORD-aligned (an unaligned payload makes the linker reject the file).
    #[test]
    fn res_starts_with_the_null_resource_and_stays_aligned() {
        let mut res = ResFile::new();
        let odd = [1u8, 2, 3];
        res.push(RT_ICON, 1, &odd);
        res.push(RT_GROUP_ICON, 1, &[9u8; 6]);
        let bytes = res.bytes();

        assert_eq!(&bytes[0..4], &0u32.to_le_bytes(), "DataSize 0");
        assert_eq!(&bytes[4..8], &32u32.to_le_bytes(), "HeaderSize 32");
        assert_eq!(&bytes[8..10], &0xFFFFu16.to_le_bytes());
        assert_eq!(&bytes[10..12], &0u16.to_le_bytes());
        assert_eq!(&bytes[12..14], &0xFFFFu16.to_le_bytes());
        assert_eq!(&bytes[14..16], &0u16.to_le_bytes());
        assert_eq!(&bytes[16..32], &[0u8; 16]);

        // 32 (null) + 32 header + 3 payload → pad to 68, then 32 + 6 → pad to 108.
        assert_eq!(bytes.len(), 108);
        assert_eq!(&bytes[64..67], &[1, 2, 3], "payload written verbatim");
        assert_eq!(bytes[67], 0, "payload padded to a DWORD");
        assert_eq!(&bytes[106..108], &[0u8; 2], "tail padded too");
    }
}

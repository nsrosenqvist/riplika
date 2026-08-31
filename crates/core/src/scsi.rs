//! Sending a command straight to the drive.
//!
//! Some of what an optical drive can do has no kernel interface in front of it.
//! The table of contents does, and so does an ordinary read; CD-Text and raw
//! sectors do not. Those go out as SCSI command blocks through `SG_IO`.
//!
//! This is the crate's other way out of the process, alongside
//! [`Runner`](crate::host::Runner) and [`Fs`](crate::host::Fs). It is not
//! behind a trait for the same reason [`disc`](crate::disc) is not: there is
//! nothing here to fake that would be worth faking. What can be tested is what
//! comes back, and every caller keeps its parsing separate so that it can be.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;

/// Why a command did not answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failed {
    /// The drive refused, which for a read means the sectors are unreadable.
    Refused,
    /// The ioctl itself failed - no device, no permission.
    Fatal(String),
}

/// Send a command and read back what it produces.
///
/// The answer is cut to the length the drive says it sent. For a reply that
/// carries its own length - the table of contents, CD-Text - that is the right
/// thing. For a run of fixed-size sectors it is not; see [`read_sectors`].
pub fn read(file: &File, cdb: &[u8], expect: usize) -> std::result::Result<Vec<u8>, Failed> {
    let (mut buffer, resid) = transfer(file, cdb, expect)?;
    buffer.truncate(buffer.len().saturating_sub(resid));
    Ok(buffer)
}

/// Read a run of whole sectors, disbelieving a residual too small to be one.
///
/// The USB bridge in front of at least one drive here reports a residual on
/// transfers that plainly completed: the buffer comes back filled to the last
/// byte, every sector's sync pattern sits at its exact stride, and yet the
/// drive claims 96 or 256 bytes were never sent. Cutting the answer there
/// leaves a part sector, which every caller reads as a failed read - so a good
/// read of two sectors was being thrown away, and the pregap scan was falling
/// back to its slow path against a drive that had answered correctly.
///
/// A residual shorter than a single sector cannot describe a missing sector,
/// so it is noise and is ignored. One sector or more is believed.
pub fn read_sectors(
    file: &File,
    cdb: &[u8],
    sector: usize,
    count: usize,
) -> std::result::Result<Vec<u8>, Failed> {
    let (mut buffer, resid) = transfer(file, cdb, sector * count)?;
    buffer.truncate(believable(buffer.len(), resid, sector));
    Ok(buffer)
}

/// How much of an answer to keep, given what the drive says it held back.
fn believable(got: usize, resid: usize, sector: usize) -> usize {
    if resid < sector { got } else { got.saturating_sub(resid) }
}

/// The ioctl itself, answering with what came back and what the drive says it
/// held back.
fn transfer(
    file: &File,
    cdb: &[u8],
    expect: usize,
) -> std::result::Result<(Vec<u8>, usize), Failed> {
    let mut buffer = vec![0u8; expect];
    let mut command = cdb.to_vec();
    let mut sense = [0u8; 32];

    let mut hdr: SgIoHdr = unsafe { std::mem::zeroed() };
    hdr.interface_id = i32::from(b'S');
    hdr.dxfer_direction = SG_DXFER_FROM_DEV;
    hdr.cmd_len = command.len() as u8;
    hdr.mx_sb_len = sense.len() as u8;
    hdr.dxfer_len = buffer.len() as u32;
    hdr.dxferp = buffer.as_mut_ptr().cast();
    hdr.cmdp = command.as_mut_ptr();
    hdr.sbp = sense.as_mut_ptr();
    hdr.timeout = 20_000;

    if unsafe { libc::ioctl(file.as_raw_fd(), SG_IO as _, &raw mut hdr) } != 0 {
        return Err(Failed::Fatal(std::io::Error::last_os_error().to_string()));
    }
    if hdr.status != 0 || hdr.host_status != 0 {
        return Err(Failed::Refused);
    }
    Ok((buffer, hdr.resid.max(0) as usize))
}

/// Open the device and send one command. For the one-off questions.
pub fn ask(device: &Path, cdb: &[u8], expect: usize) -> Option<Vec<u8>> {
    let file = File::open(device).ok()?;
    read(&file, cdb, expect).ok()
}

/// `GET CONFIGURATION`, asking only for the current profile.
pub fn get_configuration() -> [u8; 10] {
    [0x46, 0x00, 0, 0, 0, 0, 0, 0, 8, 0]
}

/// `READ TOC/PMA/ATIP` in the CD-Text format.
pub fn read_cd_text(length: u16) -> [u8; 10] {
    let l = length.to_be_bytes();
    [0x43, 0x00, 0x05, 0, 0, 0, 0, l[0], l[1], 0]
}

/// `READ CD`: whole sectors, sync pattern and headers included.
///
/// The last flag byte is what makes it raw. `0xF8` asks for the sync pattern,
/// both header fields, the user data and the error-correction bytes - all 2352
/// bytes of the sector as it is written on the disc, rather than the 2048 the
/// kernel hands out after checking and discarding the rest.
pub fn read_cd_raw(lba: u32, sectors: u32) -> [u8; 12] {
    let l = lba.to_be_bytes();
    let n = sectors.to_be_bytes();
    [0xBE, 0x00, l[0], l[1], l[2], l[3], n[1], n[2], n[3], 0xF8, 0x00, 0x00]
}

/// `READ CD`, and the Q subchannel with it.
///
/// Sixteen bytes are appended to each sector saying which track it belongs to
/// and whether it is inside the track or in the silence in front of it. That
/// second fact is what a cue sheet needs and the table of contents does not
/// carry.
pub fn read_cd_raw_with_q(lba: u32, sectors: u32) -> [u8; 12] {
    let mut cdb = read_cd_raw(lba, sectors);
    cdb[10] = 0x02;
    cdb
}

/// Bytes a raw sector takes when the Q subchannel comes with it.
pub const RAW_WITH_Q: usize = 2352 + 16;

/// `READ CD`, and the drive's own account of what it could not read.
///
/// C2 is one bit per byte of the sector, set where the drive's error
/// correction gave up and handed back a guess. It is the difference between
/// "these two passes disagree" and "the disc is damaged from here on", and it
/// costs nothing but the extra bytes.
pub fn read_cd_raw_with_c2(lba: u32, sectors: u32) -> [u8; 12] {
    let mut cdb = read_cd_raw(lba, sectors);
    cdb[9] = 0xFA;
    cdb
}

/// One bit per byte of a 2352-byte sector, rounded up to whole bytes.
pub const C2_BYTES: usize = 294;

/// Bytes a raw sector takes when the C2 error pointers come with it.
pub const RAW_WITH_C2: usize = 2352 + C2_BYTES;

const SG_IO: libc::c_ulong = 0x2285;
const SG_DXFER_FROM_DEV: i32 = -3;

/// Mirrors the kernel's `struct sg_io_hdr`.
#[repr(C)]
struct SgIoHdr {
    interface_id: i32,
    dxfer_direction: i32,
    cmd_len: u8,
    mx_sb_len: u8,
    iovec_count: u16,
    dxfer_len: u32,
    dxferp: *mut libc::c_void,
    cmdp: *mut u8,
    sbp: *mut u8,
    timeout: u32,
    flags: u32,
    pack_id: i32,
    usr_ptr: *mut libc::c_void,
    status: u8,
    masked_status: u8,
    msg_status: u8,
    sb_len_wr: u8,
    host_status: u16,
    driver_status: u16,
    resid: i32,
    duration: u32,
    info: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_residual_shorter_than_a_sector_is_not_believed() {
        // What this drive reports on transfers that plainly completed: the
        // buffer comes back filled, the sync patterns are all at their stride,
        // and the residual still says a few dozen bytes never arrived.
        for lie in [88, 96, 128, 176, 192, 256] {
            assert_eq!(believable(4704, lie, 2352), 4704, "residual of {lie} is noise");
        }
    }

    #[test]
    fn a_residual_of_a_whole_sector_or_more_is_believed() {
        assert_eq!(believable(4704, 2352, 2352), 2352);
        assert_eq!(believable(7056, 4704, 2352), 2352);
    }

    #[test]
    fn a_residual_longer_than_the_answer_leaves_nothing_rather_than_panicking() {
        assert_eq!(believable(2352, 9999, 2352), 0);
    }

    #[test]
    fn the_c2_read_asks_for_error_pointers_and_changes_nothing_else() {
        let plain = read_cd_raw(1234, 7);
        let with_c2 = read_cd_raw_with_c2(1234, 7);
        assert_eq!(with_c2[9], 0xFA, "sync, headers, user data, ECC and C2");
        assert_eq!(plain[..9], with_c2[..9], "the same sectors are being asked for");
        assert_eq!(plain[10..], with_c2[10..], "no subchannel either way");
    }

    #[test]
    fn a_c2_sector_is_the_sector_and_one_bit_for_each_of_its_bytes() {
        assert_eq!(C2_BYTES, 2352 / 8);
        assert_eq!(RAW_WITH_C2, 2352 + C2_BYTES);
    }

    #[test]
    fn a_raw_read_asks_for_the_whole_sector_and_not_the_cooked_part() {
        let cdb = read_cd_raw(0, 1);
        assert_eq!(cdb[0], 0xBE);
        // Without this byte the drive returns 2048 bytes of user data, and an
        // image built from those matches no preservation database.
        assert_eq!(cdb[9], 0xF8, "sync, headers, user data and error correction");
    }

    #[test]
    fn the_address_and_the_count_go_in_big_endian() {
        let cdb = read_cd_raw(0x0102_0304, 0x0000_0506);
        assert_eq!(&cdb[2..6], &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(&cdb[6..9], &[0x00, 0x05, 0x06]);
    }

    #[test]
    fn a_count_that_will_not_fit_in_three_bytes_is_truncated_not_wrapped() {
        // The field is 24 bits; nothing asks for eight million sectors at once,
        // but it should be obvious what happens if it did.
        let cdb = read_cd_raw(0, 0x00FF_FFFF);
        assert_eq!(&cdb[6..9], &[0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn asking_for_the_subchannel_changes_only_the_last_byte() {
        let plain = read_cd_raw(1234, 1);
        let with_q = read_cd_raw_with_q(1234, 1);
        assert_eq!(with_q[10], 0x02, "formatted Q");
        assert_eq!(plain[..10], with_q[..10], "the read itself is the same read");
    }

    #[test]
    fn the_cd_text_request_carries_its_own_buffer_size() {
        assert_eq!(read_cd_text(0x2000), [0x43, 0x00, 0x05, 0, 0, 0, 0, 0x20, 0x00, 0]);
    }
}

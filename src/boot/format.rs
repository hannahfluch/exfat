use std::{
    io::{self, Seek, SeekFrom, Write},
    ops::{Div, Sub},
};

use bytemuck::{bytes_of, cast_slice};
use checked_num::CheckedU64;

use crate::{disk, error::ExFatError};

use super::{
    checksum::Checksum, sector::BootSector, FileSystemRevision, FormatOptions, VolumeSerialNumber,
    BACKUP_BOOT_OFFSET, EXTENDED_BOOT, EXTENDED_BOOT_SIGNATURE, FIRST_CLUSTER_INDEX,
    MAIN_BOOT_OFFSET, MAX_CLUSTER_COUNT, MAX_CLUSTER_SIZE, UPCASE_TABLE_SIZE_BYTES,
};

#[derive(Copy, Clone, Debug)]
pub struct Formatter {
    pub(super) partition_offset: u64,
    pub(super) volume_length: u64,
    pub(super) fat_offset: u32,
    pub(super) fat_length: u32,
    pub(super) cluster_heap_offset: u32,
    pub(super) cluster_count: u32,
    pub(super) first_cluster_of_root_directory: u32,
    pub(super) file_system_revision: FileSystemRevision,
    pub(super) volume_flags: u16,
    pub(super) bytes_per_sector_shift: u8,
    pub(super) sectors_per_cluster_shift: u8,
    pub(super) number_of_fats: u8,
    pub(super) uptable_offset_bytes: u32,
    pub(super) bitmap_offset_bytes: u32,
    pub(super) bytes_per_sector: u16,
    pub(super) bytes_per_cluster: u32,
    pub(super) size: u64,
    pub(super) boundary_align: u32,
    pub(super) volume_serial_number: VolumeSerialNumber,
    pub(super) root_offset_bytes: u32,
    pub(super) format_options: FormatOptions,
}

impl Formatter {
    pub fn try_new(
        partition_offset: u64,
        bytes_per_sector: u16,
        bytes_per_cluster: u32,
        size: u64,
        boundary_align: u32,
        format_options: FormatOptions,
    ) -> Result<Formatter, ExFatError> {
        if format_options.dev_size < size {
            return Err(ExFatError::InvalidFileSize);
        }

        if !bytes_per_sector.is_power_of_two() || !(512..=4096).contains(&bytes_per_sector) {
            return Err(ExFatError::InvalidBytesPerSector(bytes_per_sector));
        }

        // format volume with a single FAT
        let number_of_fats = 1u8;
        let volume_flags = 0;

        // transform partition_offset to be measured by sectors
        let partition_offset = partition_offset / bytes_per_sector as u64;

        if !bytes_per_cluster.is_power_of_two()
            || !(bytes_per_sector as u32..=MAX_CLUSTER_SIZE).contains(&bytes_per_cluster)
        {
            return Err(ExFatError::InvlaidClusterSize(bytes_per_cluster));
        }
        let bytes_per_sector_shift = bytes_per_sector.ilog2() as u8;
        let sectors_per_cluster_shift = (bytes_per_cluster / bytes_per_sector as u32).ilog2() as u8;

        let volume_length = size / bytes_per_sector as u64;

        if volume_length < (1 << (20 - bytes_per_sector_shift)) {
            return Err(ExFatError::InvalidSize(size));
        }

        let fat_offset_bytes: u32 = (CheckedU64::new(bytes_per_sector as u64) * 24
            + partition_offset)
            .ok_or(ExFatError::InvalidPartitionOffset(partition_offset))?
            .next_multiple_of(boundary_align as u64)
            .sub(partition_offset)
            .try_into()
            .map_err(|_| ExFatError::BoundaryAlignemntTooBig(boundary_align))?;

        let fat_offset = fat_offset_bytes / bytes_per_sector as u32;

        let max_clusters: CheckedU64 =
            ((CheckedU64::new(size) - fat_offset_bytes as u64 - number_of_fats as u64 * 8 - 1)
                / (bytes_per_cluster as u64 + 4 * number_of_fats as u64)
                + 1)
            .ok_or(ExFatError::InvlaidClusterSize(bytes_per_cluster))?
            .into();

        let fat_length_bytes = ((max_clusters + 2) * 4)
            .ok_or(ExFatError::InvlaidClusterSize(bytes_per_cluster))?
            .next_multiple_of(bytes_per_sector as u64);

        let fat_length: u32 = (fat_length_bytes / bytes_per_sector as u64)
            .try_into()
            .map_err(|_| ExFatError::InvlaidClusterSize(bytes_per_cluster))?;

        let mut cluster_heap_offset_bytes = ((partition_offset
            + fat_offset_bytes as u64
            + fat_length_bytes * number_of_fats as u64)
            .next_multiple_of(boundary_align as u64)
            - partition_offset) as u32;

        let mut cluster_heap_offset = cluster_heap_offset_bytes / bytes_per_sector as u32;

        if cluster_heap_offset_bytes as u64 >= size {
            return Err(ExFatError::BoundaryAlignemntTooBig(boundary_align));
        }

        let mut cluster_count: u32 = ((size - cluster_heap_offset_bytes as u64)
            / bytes_per_cluster as u64)
            .try_into()
            .map_err(|_| ExFatError::InvlaidClusterSize(bytes_per_cluster))?;

        if cluster_count
            > MAX_CLUSTER_COUNT.min(
                ((volume_length - cluster_heap_offset as u64)
                    / 2u64.pow(sectors_per_cluster_shift as u32)) as u32,
            )
        {
            return Err(ExFatError::InvlaidClusterSize(bytes_per_cluster));
        }

        // bitmap is first cluster of cluster heap
        let mut bitmap_offset_bytes = cluster_heap_offset_bytes;
        let mut bitmap_length_bytes = cluster_count.next_multiple_of(8) / 8;

        if format_options.pack_bitmap {
            let fat_end_bytes = fat_offset_bytes as u64 + fat_length_bytes;
            let mut bitmap_length_bytes_packed;
            let mut bitmap_length_clusters_packed =
                bitmap_length_bytes.next_multiple_of(bytes_per_cluster);

            loop {
                let bitmap_cluster_count_packed = bitmap_length_clusters_packed / bytes_per_cluster;
                // check if there is enough space to put bitmap before alignment boundary
                if ((cluster_heap_offset_bytes - bitmap_length_clusters_packed) as u64)
                    < fat_end_bytes
                    || cluster_count > MAX_CLUSTER_COUNT - bitmap_cluster_count_packed
                {
                    return Err(ExFatError::CannotPackBitmap);
                }

                let total_cluster_count = cluster_count + bitmap_cluster_count_packed;
                bitmap_length_bytes_packed = total_cluster_count.next_multiple_of(8).div(8);
                let new_bitmap_length_clusters =
                    bitmap_length_bytes_packed.next_multiple_of(bytes_per_cluster);

                if new_bitmap_length_clusters == bitmap_length_clusters_packed {
                    cluster_heap_offset_bytes -= bitmap_length_clusters_packed;
                    cluster_count = total_cluster_count;
                    bitmap_offset_bytes -= bitmap_length_clusters_packed;
                    bitmap_length_bytes = bitmap_length_bytes_packed;
                    break;
                }
                bitmap_length_clusters_packed = new_bitmap_length_clusters;
            }

            // reassing changed variable
            cluster_heap_offset = cluster_heap_offset_bytes / bytes_per_sector as u32;
        }
        let cluster_length = bitmap_length_bytes.next_multiple_of(bytes_per_cluster);

        let uptable_offset_bytes = bitmap_offset_bytes + cluster_length;
        let uptable_start_cluster = FIRST_CLUSTER_INDEX as u32 + cluster_length / bytes_per_cluster;
        let uptable_length_bytes = UPCASE_TABLE_SIZE_BYTES;

        let cluster_length = (uptable_length_bytes as u32).next_multiple_of(bytes_per_cluster);

        let root_offset_bytes = uptable_offset_bytes + cluster_length;
        let first_cluster_of_root_directory =
            uptable_start_cluster + cluster_length / bytes_per_cluster;

        let file_system_revision = FileSystemRevision::default();
        let volume_serial_number = VolumeSerialNumber::try_new()?;
        Ok(Self {
            partition_offset,
            volume_length,
            bytes_per_sector_shift,
            fat_offset,
            number_of_fats,
            fat_length,
            cluster_heap_offset,
            cluster_count,
            sectors_per_cluster_shift,
            first_cluster_of_root_directory,
            volume_flags,
            volume_serial_number,
            file_system_revision,
            bitmap_offset_bytes,
            uptable_offset_bytes,
            size,
            bytes_per_cluster,
            bytes_per_sector,
            boundary_align,
            root_offset_bytes,
            format_options,
        })
    }

    /// Attempts to write the boot region onto the device. The file length must be the same as the
    /// provided `dev_size` in the [`Formatter`].
    pub fn write<T: Write + Seek>(&self, f: &mut T) -> Result<(), ExFatError> {
        let old_pos = f.stream_position()?;
        let len = f.seek(SeekFrom::End(0))?;

        if old_pos != len {
            f.seek(SeekFrom::Start(old_pos))?;
        }

        assert_eq!(len, self.format_options.dev_size);

        if len != self.format_options.dev_size {
            return Err(ExFatError::InvalidFileSize);
        }

        let size = if self.format_options.full_format {
            self.size
        } else {
            self.root_offset_bytes as u64 + self.bytes_per_cluster as u64
        };

        // clear disk size as needed
        disk::write_zeroes(f, size, 0)?;

        // write main boot region
        self.write_boot_region(f, MAIN_BOOT_OFFSET)?;

        // write backup boot region
        self.write_boot_region(f, BACKUP_BOOT_OFFSET)?;

        Ok(())
    }

    /// Attempts to write a boot region to a disk at the specified sector offet.
    fn write_boot_region<T: Write + Seek>(
        &self,
        f: &mut T,
        mut offset_sectors: u64,
    ) -> io::Result<()> {
        let mut checksum = Checksum::new(self.bytes_per_sector);

        let boot_sector = BootSector::new(self);

        // write boot sector
        let bytes = bytes_of(&boot_sector);
        self.write_sector(f, bytes, offset_sectors)?;
        checksum.boot_sector(bytes);
        offset_sectors += 1;

        // write extended boot sectors
        let bytes = self.write_extended(f, offset_sectors, EXTENDED_BOOT)?;
        checksum.extended_boot_sector(cast_slice(&bytes), EXTENDED_BOOT);
        offset_sectors += EXTENDED_BOOT;

        // write oem sector (unused so entirely empty)
        // todo: add flash/custom parameter support
        disk::write_zeroes(
            f,
            self.bytes_per_sector as u64,
            self.offset_sector_bytes(offset_sectors),
        )?;
        checksum.zero_sector();
        offset_sectors += 1;

        // write reserved sector
        disk::write_zeroes(
            f,
            self.bytes_per_sector as u64,
            self.offset_sector_bytes(offset_sectors),
        )?;
        checksum.zero_sector();
        offset_sectors += 1;

        // checksum sector
        self.write_checksum(f, checksum, offset_sectors)?;

        Ok(())
    }

    /// Attempts to write a single sector at the specified offset (given in sectors).
    fn write_sector<T: Write + Seek>(
        &self,
        f: &mut T,
        bytes: &[u8],
        offset_sectors: u64,
    ) -> io::Result<()> {
        f.seek(SeekFrom::Start(self.offset_sector_bytes(offset_sectors)))?;
        f.write_all(bytes)
    }

    /// Attempts to write a given amount of extended boot sectors at the specified offset (given in
    /// sectors). Returns the buffer of the extended boot sector.
    fn write_extended<T: Write + Seek>(
        &self,
        f: &mut T,
        offset_sectors: u64,
        amount: u64,
    ) -> io::Result<Vec<u32>> {
        f.seek(SeekFrom::Start(self.offset_sector_bytes(offset_sectors)))?;

        let buffer_len = self.bytes_per_sector as usize / 4;
        let mut buffer = vec![0; buffer_len];

        buffer[buffer_len - 1] = EXTENDED_BOOT_SIGNATURE.to_le();

        for i in 0..amount {
            let sector_offset = offset_sectors + i;
            self.write_sector(f, cast_slice(&buffer), sector_offset)?;
        }

        Ok(buffer)
    }

    /// Attempts to write the checksum sector
    fn write_checksum<T: Write + Seek>(
        &self,
        f: &mut T,
        checksum: Checksum,
        offset_sectors: u64,
    ) -> io::Result<()> {
        f.seek(SeekFrom::Start(self.offset_sector_bytes(offset_sectors)))?;

        let checksum = checksum.get();

        let buffer_len = self.bytes_per_sector as usize / 4;
        let mut buffer = vec![0u32; buffer_len];

        for i in buffer.iter_mut() {
            *i = checksum;
        }

        self.write_sector(f, cast_slice(&buffer), offset_sectors)?;

        Ok(())
    }

    /// Offset in bytes until the given sector index.
    fn offset_sector_bytes(&self, sector_index: u64) -> u64 {
        self.bytes_per_sector as u64 * sector_index
    }
}

#[test]
fn boot_region() {
    use std::io::Read;

    let size: u64 = 32 * crate::MB as u64;
    let mut f = std::io::Cursor::new(vec![0u8; size as usize]);
    let bytes_per_sector = 512;
    let bytes_per_cluster = 4 * crate::KB as u32;

    let formatter = Formatter::try_new(
        0,
        bytes_per_sector,
        bytes_per_cluster,
        size,
        crate::DEFAULT_BOUNDARY_ALIGNEMENT,
        FormatOptions::new(false, false, size),
    )
    .unwrap();
    formatter.write(&mut f).unwrap();

    let offset_main_checksum_bytes = 11 * bytes_per_sector as u64;
    let offset_backup_checksum_bytes = 23 * bytes_per_sector as u64;

    // assert checksum is the same for main boot region and backup boot region
    let mut read_main = vec![0u8; 8];
    f.seek(std::io::SeekFrom::Start(offset_main_checksum_bytes))
        .unwrap();
    f.read_exact(&mut read_main).unwrap();

    let mut read_backup = vec![0u8; 8];

    f.seek(std::io::SeekFrom::Start(offset_backup_checksum_bytes))
        .unwrap();
    f.read_exact(&mut read_backup).unwrap();

    assert_eq!(
        read_backup, read_main,
        "checksum of main and backup boot region must be equal"
    );
}

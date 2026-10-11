//! Portable project-image provenance. This reads only raw bytes; it never mounts
//! or traverses a guest filesystem. The ordinary image SHA includes the trailer.
use crate::{Error, Result, CHUNK_BYTES};
use ahvm_engine::{
    project_digest, ProjectImageMetadata, PROJECT_IMAGE_MAX_BYTES, PROJECT_IMAGE_TRAILER_BYTES,
};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
};

const MAGIC: &[u8; 8] = b"AHVMPJ01";
const HEADER_BYTES: usize = 16;
const MAX_METADATA_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedImage {
    pub digest: String,
    pub logical_bytes: u64,
    pub filesystem_bytes: u64,
}

pub fn logical_bytes(source_bytes: u64) -> Result<u64> {
    let total = source_bytes
        .checked_add(PROJECT_IMAGE_TRAILER_BYTES)
        .ok_or(Error::InvalidInput)?;
    if source_bytes == 0
        || !source_bytes.is_multiple_of(CHUNK_BYTES as u64)
        || total > PROJECT_IMAGE_MAX_BYTES
    {
        return Err(Error::InvalidInput);
    }
    Ok(total)
}

pub fn trailer(metadata: &ProjectImageMetadata) -> Result<Vec<u8>> {
    let json = metadata
        .canonical_bytes()
        .map_err(|_| Error::InvalidInput)?;
    let mut bytes = vec![0; PROJECT_IMAGE_TRAILER_BYTES as usize];
    bytes[..8].copy_from_slice(MAGIC);
    bytes[8..12].copy_from_slice(&(json.len() as u32).to_be_bytes());
    bytes[HEADER_BYTES..HEADER_BYTES + json.len()].copy_from_slice(&json);
    Ok(bytes)
}

pub fn decode_trailer(bytes: &[u8]) -> Result<ProjectImageMetadata> {
    if bytes.len() != PROJECT_IMAGE_TRAILER_BYTES as usize
        || &bytes[..8] != MAGIC
        || bytes[12..HEADER_BYTES].iter().any(|b| *b != 0)
    {
        return Err(Error::Corrupt);
    }
    let length = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
    if !(1..=MAX_METADATA_BYTES).contains(&length)
        || bytes[HEADER_BYTES + length..].iter().any(|b| *b != 0)
    {
        return Err(Error::Corrupt);
    }
    let json = &bytes[HEADER_BYTES..HEADER_BYTES + length];
    let metadata: ProjectImageMetadata =
        serde_json::from_slice(json).map_err(|_| Error::Corrupt)?;
    if metadata.canonical_bytes().map_err(|_| Error::Corrupt)? != json {
        return Err(Error::Corrupt);
    }
    Ok(metadata)
}

/// Bounds the filesystem's declared extent before placing provenance after it.
/// Geometry follows the ext4 superblock at byte 1024, including the optional
/// 64-bit block count: https://www.kernel.org/doc/html/latest/filesystems/ext4/super.html
/// This is an extent check, not a filesystem integrity or sanitization claim.
pub fn ext4_bytes(input: &mut (impl Read + Seek), source_bytes: u64) -> Result<u64> {
    logical_bytes(source_bytes)?;
    input
        .seek(SeekFrom::Start(1024))
        .map_err(|_| Error::Store)?;
    let mut superblock = [0; 0x158];
    input
        .read_exact(&mut superblock)
        .map_err(|_| Error::Corrupt)?;
    let le32 =
        |offset: usize| u32::from_le_bytes(superblock[offset..offset + 4].try_into().unwrap());
    if u16::from_le_bytes(superblock[0x38..0x3a].try_into().unwrap()) != 0xef53 {
        return Err(Error::Corrupt);
    }
    let log_size = le32(0x18);
    if log_size > 6 {
        return Err(Error::Corrupt);
    }
    let high = le32(0x150);
    if high != 0 && le32(0x60) & 0x80 == 0 {
        return Err(Error::Corrupt);
    }
    let blocks = u64::from(le32(0x4)) | (u64::from(high) << 32);
    let block_size = 1024u64 << log_size;
    let size = blocks.checked_mul(block_size).ok_or(Error::Corrupt)?;
    if blocks <= u64::from(le32(0x14))
        || size < 2048
        || size > source_bytes
        || (log_size == 0 && le32(0x14) != 1)
        || (log_size != 0 && le32(0x14) != 0)
    {
        return Err(Error::Corrupt);
    }
    Ok(size)
}

pub fn hash_file(file: &mut File) -> Result<String> {
    let before = file.metadata().map_err(|_| Error::Store)?;
    if !before.is_file() {
        return Err(Error::InvalidInput);
    }
    file.seek(SeekFrom::Start(0)).map_err(|_| Error::Store)?;
    let mut hash = Sha256::new();
    let mut bytes = vec![0; CHUNK_BYTES];
    let mut read = 0u64;
    loop {
        let count = file.read(&mut bytes).map_err(|_| Error::Store)?;
        if count == 0 {
            break;
        }
        hash.update(&bytes[..count]);
        read = read.checked_add(count as u64).ok_or(Error::Corrupt)?;
    }
    let after = file.metadata().map_err(|_| Error::Store)?;
    if read != before.len()
        || after.len() != before.len()
        || after.modified().ok() != before.modified().ok()
    {
        return Err(Error::Conflict);
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// Verify the export independently, then append the frozen provenance. A failed
/// export SHA/extent check leaves the source length unchanged. Callers keep any
/// incomplete append charged and never expose it as a completed artifact.
pub fn finalize_export(
    file: &mut File,
    source_bytes: u64,
    export_sha256: &str,
    metadata: &ProjectImageMetadata,
) -> Result<VerifiedImage> {
    let logical_bytes = logical_bytes(source_bytes)?;
    let trailer = trailer(metadata)?;
    if !project_digest(export_sha256)
        || file.metadata().map_err(|_| Error::Store)?.len() != source_bytes
    {
        return Err(Error::InvalidInput);
    }
    let filesystem_bytes = ext4_bytes(file, source_bytes)?;
    if hash_file(file)? != export_sha256 {
        return Err(Error::Corrupt);
    }
    file.seek(SeekFrom::Start(source_bytes))
        .map_err(|_| Error::Store)?;
    file.write_all(&trailer).map_err(|_| Error::Store)?;
    file.sync_all().map_err(|_| Error::Store)?;
    if file.metadata().map_err(|_| Error::Store)?.len() != logical_bytes {
        return Err(Error::Corrupt);
    }
    let digest = hash_file(file)?;
    Ok(VerifiedImage {
        digest,
        logical_bytes,
        filesystem_bytes,
    })
}

pub fn read_metadata(file: &mut File) -> Result<ProjectImageMetadata> {
    let total = file.metadata().map_err(|_| Error::Store)?.len();
    let source = total
        .checked_sub(PROJECT_IMAGE_TRAILER_BYTES)
        .ok_or(Error::Corrupt)?;
    if logical_bytes(source).map_err(|_| Error::Corrupt)? != total {
        return Err(Error::Corrupt);
    }
    ext4_bytes(file, source)?;
    file.seek(SeekFrom::Start(source))
        .map_err(|_| Error::Store)?;
    let mut bytes = vec![0; PROJECT_IMAGE_TRAILER_BYTES as usize];
    file.read_exact(&mut bytes).map_err(|_| Error::Corrupt)?;
    decode_trailer(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::Cursor,
        sync::atomic::{AtomicU64, Ordering},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn metadata() -> ProjectImageMetadata {
        ProjectImageMetadata {
            schema: 1,
            tenant_id: "11111111-1111-1111-1111-111111111111".into(),
            project_id: "22222222-2222-2222-2222-222222222222".into(),
            build_id: "33333333-3333-3333-3333-333333333333".into(),
            source_commit: "a".repeat(40),
            base_image_digest: "b".repeat(64),
            revision_id: "c".repeat(64),
            install_hook_digest: Some("d".repeat(64)),
            start_hook_digest: None,
            canonical_workdir: "/workspace/project".into(),
        }
    }
    fn disk() -> Vec<u8> {
        let mut bytes = vec![0; CHUNK_BYTES * 2];
        bytes[1024 + 4..1024 + 8].copy_from_slice(&32u32.to_le_bytes());
        bytes[1024 + 0x18..1024 + 0x1c].copy_from_slice(&2u32.to_le_bytes());
        bytes[1024 + 0x38..1024 + 0x3a].copy_from_slice(&0xef53u16.to_le_bytes());
        bytes[5000..5004].copy_from_slice(b"data");
        bytes
    }
    fn file(bytes: &[u8]) -> (std::path::PathBuf, File) {
        let path = std::env::temp_dir().join(format!(
            "ahvm-project-image-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all(bytes).unwrap();
        (path, file)
    }
    #[test]
    fn trailer_changes_digest_for_each_frozen_build_and_keeps_extent_outside_it() {
        let bytes = disk();
        let source_hash = crate::digest(&bytes);
        let (path, mut raw) = file(&bytes);
        let first =
            finalize_export(&mut raw, bytes.len() as u64, &source_hash, &metadata()).unwrap();
        assert_eq!(
            first.logical_bytes,
            bytes.len() as u64 + PROJECT_IMAGE_TRAILER_BYTES
        );
        assert_eq!(first.filesystem_bytes, bytes.len() as u64);
        assert_eq!(read_metadata(&mut raw).unwrap(), metadata());
        assert_ne!(first.digest, source_hash);
        fs::remove_file(path).unwrap();
        let mut second_metadata = metadata();
        second_metadata.build_id = "44444444-4444-4444-4444-444444444444".into();
        let (path, mut raw) = file(&bytes);
        let second =
            finalize_export(&mut raw, bytes.len() as u64, &source_hash, &second_metadata).unwrap();
        assert_ne!(first.digest, second.digest);
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn corrupt_export_never_gets_a_provenance_trailer() {
        let bytes = disk();
        let (path, mut raw) = file(&bytes);
        assert!(
            finalize_export(&mut raw, bytes.len() as u64, &"0".repeat(64), &metadata()).is_err()
        );
        assert_eq!(raw.metadata().unwrap().len(), bytes.len() as u64);
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn malformed_ext4_geometry_cannot_overlap_trailer_or_overflow() {
        let mut bytes = disk();
        bytes[1028..1032].copy_from_slice(&33u32.to_le_bytes());
        assert!(ext4_bytes(&mut Cursor::new(&bytes), bytes.len() as u64).is_err());
        bytes[1028..1032].copy_from_slice(&32u32.to_le_bytes());
        bytes[1024 + 0x150..1024 + 0x154].copy_from_slice(&1u32.to_le_bytes());
        assert!(ext4_bytes(&mut Cursor::new(&bytes), bytes.len() as u64).is_err());
        bytes[1024 + 0x60..1024 + 0x64].copy_from_slice(&0x80u32.to_le_bytes());
        assert!(ext4_bytes(&mut Cursor::new(&bytes), bytes.len() as u64).is_err());
        bytes[1024 + 0x18..1024 + 0x1c].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(ext4_bytes(&mut Cursor::new(&bytes), bytes.len() as u64).is_err());
    }
    #[test]
    fn trailer_rejects_reserved_padding_and_noncanonical_metadata() {
        let valid = trailer(&metadata()).unwrap();
        assert_eq!(decode_trailer(&valid).unwrap(), metadata());
        for at in [0, 12, valid.len() - 1] {
            let mut bytes = valid.clone();
            bytes[at] ^= 1;
            assert!(decode_trailer(&bytes).is_err());
        }
        let mut bytes = valid;
        bytes[8..12].copy_from_slice(&65535u32.to_be_bytes());
        assert!(decode_trailer(&bytes).is_err());
        let json = serde_json::to_string_pretty(&metadata()).unwrap();
        let mut bytes = vec![0; CHUNK_BYTES];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8..12].copy_from_slice(&(json.len() as u32).to_be_bytes());
        bytes[16..16 + json.len()].copy_from_slice(json.as_bytes());
        assert!(decode_trailer(&bytes).is_err());
    }
    #[test]
    fn existing_image_limit_already_includes_trailer() {
        assert!(logical_bytes(0).is_err());
        assert!(logical_bytes(CHUNK_BYTES as u64 + 1).is_err());
        assert!(logical_bytes(PROJECT_IMAGE_MAX_BYTES).is_err());
        assert_eq!(
            logical_bytes(PROJECT_IMAGE_MAX_BYTES - PROJECT_IMAGE_TRAILER_BYTES).unwrap(),
            PROJECT_IMAGE_MAX_BYTES
        );
    }
}

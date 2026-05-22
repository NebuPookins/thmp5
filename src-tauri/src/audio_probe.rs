use anyhow::{Context, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::{Hint, ProbeResult};

pub(crate) fn probe_media_source<M>(
    path: &Path,
    media_source: M,
    force_extension: Option<&str>,
) -> Result<ProbeResult>
where
    M: MediaSource + 'static,
{
    let media_source = MediaSourceStream::new(Box::new(media_source), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) =
        force_extension.or_else(|| path.extension().and_then(|ext| ext.to_str()))
    {
        hint.with_extension(extension);
    }

    symphonia::default::get_probe()
        .format(
            &hint,
            media_source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .context("Failed to probe audio format")
}

pub(crate) fn open_wave_mp3_payload(path: &Path) -> Result<Option<FileSegment>> {
    let mut file = File::open(path).with_context(|| format!("Cannot open {}", path.display()))?;
    let Some((data_offset, data_len)) = wave_mp3_data_range(&mut file)? else {
        return Ok(None);
    };
    Ok(Some(FileSegment::new(file, data_offset, data_len)?))
}

pub(crate) struct FileSegment {
    file: File,
    start: u64,
    len: u64,
    pos: u64,
}

impl FileSegment {
    fn new(mut file: File, start: u64, len: u64) -> Result<Self> {
        file.seek(SeekFrom::Start(start))?;
        Ok(Self {
            file,
            start,
            len,
            pos: 0,
        })
    }
}

impl Read for FileSegment {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.len.saturating_sub(self.pos) as usize;
        if remaining == 0 {
            return Ok(0);
        }
        let to_read = remaining.min(buf.len());
        let read = self.file.read(&mut buf[..to_read])?;
        self.pos = self.pos.saturating_add(read as u64);
        Ok(read)
    }
}

impl Seek for FileSegment {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let next = match pos {
            SeekFrom::Start(offset) => offset as i128,
            SeekFrom::End(offset) => i128::from(self.len) + i128::from(offset),
            SeekFrom::Current(offset) => i128::from(self.pos) + i128::from(offset),
        };
        if next < 0 || next > i128::from(self.len) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek out of bounds",
            ));
        }
        let next = next as u64;
        self.file.seek(SeekFrom::Start(self.start + next))?;
        self.pos = next;
        Ok(self.pos)
    }
}

impl MediaSource for FileSegment {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

pub(crate) fn id3v2_end_offset(file: &mut File) -> Option<u64> {
    let mut header = [0u8; 10];
    file.read_exact(&mut header).ok()?;
    if &header[0..3] != b"ID3" {
        return None;
    }
    let size = ((header[6] as u64) << 21)
        | ((header[7] as u64) << 14)
        | ((header[8] as u64) << 7)
        | (header[9] as u64);
    Some(10 + size)
}

fn wave_mp3_data_range(file: &mut File) -> Result<Option<(u64, u64)>> {
    // Some files have multiple consecutive ID3v2 tags before the RIFF/WAVE
    // header (e.g. ID3v2.3 followed by ID3v2.4).  Skip all of them.
    let mut riff_offset = id3v2_end_offset(file).unwrap_or_default();
    loop {
        file.seek(SeekFrom::Start(riff_offset))?;
        let mut tag_header = [0u8; 10];
        if file.read_exact(&mut tag_header).is_err() || &tag_header[0..3] != b"ID3" {
            break;
        }
        let size = ((tag_header[6] as u64) << 21)
            | ((tag_header[7] as u64) << 14)
            | ((tag_header[8] as u64) << 7)
            | (tag_header[9] as u64);
        riff_offset += 10 + size;
    }

    file.seek(SeekFrom::Start(riff_offset))?;

    let mut header = [0u8; 12];
    if file.read_exact(&mut header).is_err() {
        return Ok(None);
    }
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return Ok(None);
    }

    let mut format_tag = None;
    let mut data_range = None;
    loop {
        let mut chunk_header = [0u8; 8];
        if file.read_exact(&mut chunk_header).is_err() {
            break;
        }

        let chunk_size =
            u32::from_le_bytes(chunk_header[4..8].try_into().expect("chunk size slice")) as u64;
        let chunk_data_offset = file.stream_position()?;

        match &chunk_header[0..4] {
            b"fmt " if chunk_size >= 2 => {
                let mut tag = [0u8; 2];
                file.read_exact(&mut tag)?;
                format_tag = Some(u16::from_le_bytes(tag));
            }
            b"data" => {
                data_range = Some((chunk_data_offset, chunk_size));
            }
            _ => {}
        }

        let padded_size = chunk_size + (chunk_size % 2);
        file.seek(SeekFrom::Start(chunk_data_offset + padded_size))?;

        if format_tag.is_some() && data_range.is_some() {
            break;
        }
    }

    let is_mp3_wave = matches!(format_tag, Some(0x0050 | 0x0055));
    Ok(if is_mp3_wave { data_range } else { None })
}

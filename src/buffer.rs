use bytes::{BufMut, Bytes, BytesMut};

use crate::error::{Error, Result};

pub const MAX_SHORT_LENGTH: u8 = 252;
pub const ESCAPE_CHAR: u8 = 253;
pub const LONG_INDICATOR: u8 = 254;
pub const NULL_INDICATOR: u8 = 255;

#[derive(Debug)]
pub struct ReadBuffer {
    data: Bytes,
    pos: usize,
}

impl ReadBuffer {
    pub fn new(data: Bytes) -> Self {
        Self { data, pos: 0 }
    }

    pub fn from_slice(data: &[u8]) -> Self {
        Self {
            data: Bytes::copy_from_slice(data),
            pos: 0,
        }
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn has_remaining(&self, n: usize) -> bool {
        self.remaining() >= n
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.ensure_remaining(n)?;
        self.pos += n;
        Ok(())
    }

    fn ensure_remaining(&self, n: usize) -> Result<()> {
        if self.remaining() < n {
            Err(Error::BufferUnderflow {
                needed: n,
                available: self.remaining(),
            })
        } else {
            Ok(())
        }
    }

    pub fn read_u8(&mut self) -> Result<u8> {
        self.ensure_remaining(1)?;
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

    pub fn read_u16_be(&mut self) -> Result<u16> {
        self.ensure_remaining(2)?;
        let v = u16::from_be_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    pub fn read_i16_be(&mut self) -> Result<i16> {
        self.ensure_remaining(2)?;
        let v = i16::from_be_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    pub fn read_u16_le(&mut self) -> Result<u16> {
        self.ensure_remaining(2)?;
        let v = u16::from_le_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    pub fn read_u32_be(&mut self) -> Result<u32> {
        self.ensure_remaining(4)?;
        let v = u32::from_be_bytes([
            self.data[self.pos],
            self.data[self.pos + 1],
            self.data[self.pos + 2],
            self.data[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }

    pub fn read_u64_be(&mut self) -> Result<u64> {
        self.ensure_remaining(8)?;
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&self.data[self.pos..self.pos + 8]);
        self.pos += 8;
        Ok(u64::from_be_bytes(bytes))
    }

    pub fn read_bytes(&mut self, n: usize) -> Result<Bytes> {
        self.ensure_remaining(n)?;
        let v = self.data.slice(self.pos..self.pos + n);
        self.pos += n;
        Ok(v)
    }

    pub fn read_bytes_vec(&mut self, n: usize) -> Result<Vec<u8>> {
        self.ensure_remaining(n)?;
        let v = self.data[self.pos..self.pos + n].to_vec();
        self.pos += n;
        Ok(v)
    }

    fn read_ub_length(&mut self) -> Result<u8> {
        Ok(self.read_u8()? & 0x7f)
    }

    pub fn read_ub1(&mut self) -> Result<u8> {
        self.read_u8()
    }

    pub fn read_ub2(&mut self) -> Result<u16> {
        match self.read_ub_length()? {
            0 => Ok(0),
            1 => Ok(self.read_u8()? as u16),
            2 => self.read_u16_be(),
            n => Err(Error::InvalidLengthIndicator(n)),
        }
    }

    pub fn read_sb2(&mut self) -> Result<i16> {
        match self.read_ub_length()? {
            0 => Ok(0),
            1 => Ok(self.read_u8()? as i8 as i16),
            2 => self.read_i16_be(),
            n => Err(Error::InvalidLengthIndicator(n)),
        }
    }

    pub fn read_ub4(&mut self) -> Result<u32> {
        match self.read_ub_length()? {
            0 => Ok(0),
            1 => Ok(self.read_u8()? as u32),
            2 => Ok(self.read_u16_be()? as u32),
            3 => {
                let b = self.read_bytes_vec(3)?;
                Ok(((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32))
            }
            4 => self.read_u32_be(),
            n => Err(Error::InvalidLengthIndicator(n)),
        }
    }

    pub fn read_ub8(&mut self) -> Result<u64> {
        match self.read_ub_length()? {
            0 => Ok(0),
            1 => Ok(self.read_u8()? as u64),
            2 => Ok(self.read_u16_be()? as u64),
            3 => {
                let b = self.read_bytes_vec(3)?;
                Ok(((b[0] as u64) << 16) | ((b[1] as u64) << 8) | (b[2] as u64))
            }
            4 => Ok(self.read_u32_be()? as u64),
            5..=7 => {
                let len = self.read_ub_length()? as usize;
                let b = self.read_bytes_vec(len)?;
                let mut v = 0u64;
                for &x in &b {
                    v = (v << 8) | (x as u64);
                }
                Ok(v)
            }
            8 => self.read_u64_be(),
            n => Err(Error::InvalidLengthIndicator(n)),
        }
    }

    pub fn read_bytes_with_length(&mut self) -> Result<Option<Vec<u8>>> {
        let len = self.read_u8()?;
        if len == NULL_INDICATOR {
            return Ok(None);
        }
        if len == LONG_INDICATOR {
            let mut result = Vec::new();
            loop {
                let chunk_len = self.read_ub4()? as usize;
                if chunk_len == 0 {
                    break;
                }
                let chunk = self.read_bytes_vec(chunk_len)?;
                result.extend(chunk);
            }
            return Ok(Some(result));
        }
        let actual_len = if len == ESCAPE_CHAR {
            self.read_u8()? as usize
        } else {
            len as usize
        };
        if actual_len == 0 {
            return Ok(Some(Vec::new()));
        }
        self.read_bytes_vec(actual_len).map(Some)
    }

    /// Read a CLR value where a `0xFE` long-form uses **single-byte** chunk
    /// length prefixes (`[len:u8][bytes]…[0x00]`), the framing Oracle uses when
    /// the "big CLR chunks" capability is *not* negotiated.
    /// ODP.NET managed emits this for `AUTH_*` values longer than 252 bytes
    /// (e.g. `AUTH_CONNECT_STRING`); [`read_bytes_with_length`] assumes the
    /// `ub4`-prefixed big-chunk form and would misread it.
    pub fn read_bytes_with_length_1b_chunks(&mut self) -> Result<Option<Vec<u8>>> {
        let len = self.read_u8()?;
        if len == NULL_INDICATOR {
            return Ok(None);
        }
        if len == LONG_INDICATOR {
            let mut result = Vec::new();
            loop {
                let chunk_len = self.read_u8()? as usize;
                if chunk_len == 0 {
                    break;
                }
                result.extend(self.read_bytes_vec(chunk_len)?);
            }
            return Ok(Some(result));
        }
        let actual_len = if len == ESCAPE_CHAR {
            self.read_u8()? as usize
        } else {
            len as usize
        };
        if actual_len == 0 {
            return Ok(Some(Vec::new()));
        }
        self.read_bytes_vec(actual_len).map(Some)
    }

    pub fn read_string_with_length(&mut self) -> Result<Option<String>> {
        match self.read_bytes_with_length()? {
            None => Ok(None),
            Some(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|e| Error::DataConversionError(e.to_string())),
        }
    }

    pub fn read_string_with_ub4_length(&mut self) -> Result<Option<String>> {
        let outer = self.read_ub4()?;
        if outer == 0 {
            return Ok(None);
        }
        self.read_string_with_length()
    }

    pub fn peek_u8(&self) -> Result<u8> {
        self.ensure_remaining(1)?;
        Ok(self.data[self.pos])
    }

    pub fn remaining_slice(&self) -> &[u8] {
        &self.data[self.pos..]
    }
}

#[derive(Debug)]
pub struct WriteBuffer {
    data: BytesMut,
}

impl WriteBuffer {
    pub fn new() -> Self {
        Self {
            data: BytesMut::with_capacity(8192),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            data: BytesMut::with_capacity(capacity),
        }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }

    pub fn freeze(self) -> Bytes {
        self.data.freeze()
    }

    pub fn into_inner(self) -> BytesMut {
        self.data
    }

    pub fn write_u8(&mut self, value: u8) {
        self.data.put_u8(value);
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.data.put_slice(bytes);
    }

    pub fn write_zeros(&mut self, n: usize) {
        for _ in 0..n {
            self.data.put_u8(0);
        }
    }

    pub fn write_u16_be(&mut self, value: u16) {
        self.data.put_u16(value);
    }

    pub fn write_u16_le(&mut self, value: u16) {
        self.data.put_u16_le(value);
    }

    pub fn write_u32_be(&mut self, value: u32) {
        self.data.put_u32(value);
    }

    pub fn write_u32_le(&mut self, value: u32) {
        self.data.put_u32_le(value);
    }

    pub fn write_u64_be(&mut self, value: u64) {
        self.data.put_u64(value);
    }

    pub fn write_ub1(&mut self, value: u8) {
        self.write_u8(value);
    }

    pub fn write_ub2(&mut self, value: u16) {
        match value {
            0 => self.write_u8(0),
            1..=255 => {
                self.write_u8(1);
                self.write_u8(value as u8);
            }
            _ => {
                self.write_u8(2);
                self.write_u16_be(value);
            }
        }
    }

    pub fn write_sb2(&mut self, value: i16) {
        if value == 0 {
            self.write_u8(0);
        } else if value > 0 && value <= 127 {
            self.write_u8(1);
            self.write_u8(value as u8);
        } else if (-127..0).contains(&value) {
            self.write_u8(1 | 0x80);
            self.write_u8((-value) as u8);
        } else {
            self.write_u8(2 | 0x80);
            self.write_i16_be(value);
        }
    }

    pub fn write_i16_be(&mut self, value: i16) {
        self.data.put_i16(value);
    }

    pub fn write_ub4(&mut self, value: u32) {
        match value {
            0 => self.write_u8(0),
            1..=255 => {
                self.write_u8(1);
                self.write_u8(value as u8);
            }
            256..=65535 => {
                self.write_u8(2);
                self.write_u16_be(value as u16);
            }
            _ => {
                self.write_u8(4);
                self.write_u32_be(value);
            }
        }
    }

    pub fn write_ub8(&mut self, value: u64) {
        match value {
            0 => self.write_u8(0),
            1..=255 => {
                self.write_u8(1);
                self.write_u8(value as u8);
            }
            256..=65535 => {
                self.write_u8(2);
                self.write_u16_be(value as u16);
            }
            65536..=4294967295 => {
                self.write_u8(4);
                self.write_u32_be(value as u32);
            }
            _ => {
                self.write_u8(8);
                self.write_u64_be(value);
            }
        }
    }

    pub fn write_bytes_with_length(&mut self, bytes: Option<&[u8]>) {
        const CHUNK_SIZE: usize = 32767;
        match bytes {
            None => self.write_u8(NULL_INDICATOR),
            Some(data) => {
                let len = data.len();
                if len == 0 {
                    self.write_u8(0);
                } else if len <= MAX_SHORT_LENGTH as usize {
                    self.write_u8(len as u8);
                    self.write_bytes(data);
                } else {
                    self.write_u8(LONG_INDICATOR);
                    let mut offset = 0;
                    while offset < len {
                        let chunk_len = std::cmp::min(len - offset, CHUNK_SIZE);
                        self.write_ub4(chunk_len as u32);
                        self.write_bytes(&data[offset..offset + chunk_len]);
                        offset += chunk_len;
                    }
                    self.write_ub4(0);
                }
            }
        }
    }

    pub fn write_string_with_length(&mut self, s: Option<&str>) {
        self.write_bytes_with_length(s.map(|x| x.as_bytes()));
    }

    pub fn truncate(&mut self, len: usize) {
        self.data.truncate(len);
    }

    pub fn patch_u16_be(&mut self, pos: usize, value: u16) {
        let bytes = value.to_be_bytes();
        self.data[pos] = bytes[0];
        self.data[pos + 1] = bytes[1];
    }

    pub fn patch_u32_be(&mut self, pos: usize, value: u32) {
        let bytes = value.to_be_bytes();
        self.data[pos] = bytes[0];
        self.data[pos + 1] = bytes[1];
        self.data[pos + 2] = bytes[2];
        self.data[pos + 3] = bytes[3];
    }
}

impl Default for WriteBuffer {
    fn default() -> Self {
        Self::new()
    }
}

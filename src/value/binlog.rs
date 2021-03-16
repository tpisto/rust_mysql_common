// Copyright (c) 2021 Anatoly Ikorsky
//
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. All files in the project carrying such notice may not be copied,
// modified, or distributed except according to those terms.

use std::{convert::TryFrom, io};

use crate::{
    binlog::{decimal, jsonb, jsondiff::JsonDiff, misc::*},
    constants::{ColumnFlags, ColumnType},
    io::ParseBuf,
    misc::unexpected_buf_eof,
    proto::MyDeserialize,
    value::Value::*,
};

/// Value of a binlog event.
#[derive(Debug, Clone, PartialEq)]
pub enum BinlogValue<'a> {
    /// MySql value.
    Value(super::Value),
    /// JSONB value.
    Jsonb(jsonb::Value<'a>),
    /// Value of a partial JSON modification event.
    JsonDiff(Vec<JsonDiff<'a>>),
}

impl<'a> BinlogValue<'a> {
    /// Returns a `'static` version of `self`.
    pub fn into_owned(self) -> BinlogValue<'static> {
        match self {
            BinlogValue::Value(x) => BinlogValue::Value(x),
            BinlogValue::Jsonb(x) => BinlogValue::Jsonb(x.into_owned()),
            BinlogValue::JsonDiff(x) => {
                BinlogValue::JsonDiff(x.into_iter().map(|x| x.into_owned()).collect())
            }
        }
    }
}

impl<'de> MyDeserialize<'de> for BinlogValue<'de> {
    /// <col_type, col_meta, is_unsigned, is_partial>
    type Ctx = (ColumnType, &'de [u8], bool, bool);

    fn deserialize(
        (mut col_type, col_meta, is_unsigned, is_partial): Self::Ctx,
        buf: &mut ParseBuf<'de>,
    ) -> io::Result<Self> {
        use ColumnType::*;

        let mut length = 0_usize;

        if col_type == MYSQL_TYPE_TYPED_ARRAY {
            let type_byte = col_meta[0];
            col_type = ColumnType::try_from(type_byte).unwrap_or(col_type);
        }

        if col_type == MYSQL_TYPE_STRING {
            if col_meta[0] >= 1 {
                let byte0 = col_meta[0] as usize;
                let byte1 = col_meta[1] as usize;

                if (byte0 & 0x30) != 0x30 {
                    // a long CHAR() field: see #37426
                    length = byte1 | (((byte0 & 0x30) ^ 0x30) << 4);
                    col_type = ColumnType::try_from(byte0 as u8 | 0x30).unwrap_or(col_type);
                } else {
                    length = byte1;
                }
            } else {
                length = (ParseBuf(&col_meta[..])).eat_u16_le() as usize;
            }
        }

        match col_type {
            MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG
            | MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => {
                let mut flags = ColumnFlags::empty();
                flags.set(ColumnFlags::UNSIGNED_FLAG, is_unsigned);
                super::Value::deserialize_bin((col_type, flags), &mut *buf).map(BinlogValue::Value)
            }
            MYSQL_TYPE_TIMESTAMP => {
                let val = buf.checked_eat_u32_le().ok_or_else(unexpected_buf_eof)?;
                Ok(BinlogValue::Value(Int(val as i64)))
            }
            MYSQL_TYPE_INT24 => {
                if is_unsigned {
                    let val = buf.checked_eat_u24_le().ok_or_else(unexpected_buf_eof)?;
                    Ok(BinlogValue::Value(Int(val as i64)))
                } else {
                    let val = buf.checked_eat_i24_le().ok_or_else(unexpected_buf_eof)?;
                    Ok(BinlogValue::Value(Int(val as i64)))
                }
            }
            MYSQL_TYPE_TIME => {
                let tmp = buf.checked_eat_u24_le().ok_or_else(unexpected_buf_eof)?;
                let h = tmp / 10000;
                let m = (tmp % 10000) / 100;
                let s = tmp % 100;
                Ok(BinlogValue::Value(Time(
                    false, 0, h as u8, m as u8, s as u8, 0,
                )))
            }
            MYSQL_TYPE_DATETIME => {
                // read YYYYMMDDHHMMSS representaion
                let raw = buf.checked_eat_u64_le().ok_or_else(unexpected_buf_eof)?;
                let d_part = raw / 1_000_000;
                let t_part = raw % 1_000_000;
                Ok(BinlogValue::Value(Date(
                    (d_part / 10000) as u16,
                    ((d_part % 10000) / 100) as u8,
                    (d_part % 100) as u8,
                    (t_part / 10000) as u8,
                    ((t_part % 10000) / 100) as u8,
                    (t_part % 100) as u8,
                    0,
                )))
            }
            MYSQL_TYPE_YEAR => {
                let y = buf.checked_eat_u8().ok_or_else(unexpected_buf_eof)? as i32;
                Ok(BinlogValue::Value(Bytes(
                    (1900 + y).to_string().into_bytes(),
                )))
            }
            MYSQL_TYPE_NEWDATE => {
                let tmp = buf.checked_eat_u24_le().ok_or_else(unexpected_buf_eof)?;
                let d = tmp & 31;
                let m = (tmp >> 5) & 15;
                let y = tmp >> 9;
                Ok(BinlogValue::Value(Date(
                    y as u16, m as u8, d as u8, 0, 0, 0, 0,
                )))
            }
            MYSQL_TYPE_BIT => {
                let nbits = col_meta[0] as usize * 8 + (col_meta[1] as usize);
                let nbytes = (nbits + 7) / 8;
                let bytes = buf.checked_eat(nbytes).ok_or_else(unexpected_buf_eof)?;
                Ok(BinlogValue::Value(Bytes(bytes.into())))
            }
            MYSQL_TYPE_TIMESTAMP2 => {
                let dec = col_meta[0];
                let (sec, usec) = my_timestamp_from_binary(&mut *buf, dec)?;
                if usec == 0 {
                    Ok(BinlogValue::Value(Bytes(sec.to_string().into_bytes())))
                } else {
                    Ok(BinlogValue::Value(Bytes(
                        format!("{}.{:06}", sec, usec).into_bytes(),
                    )))
                }
            }
            MYSQL_TYPE_DATETIME2 => {
                let dec = col_meta[0];
                my_datetime_packed_from_binary(&mut *buf, dec as u32)
                    .map(datetime_from_packed)
                    .map(BinlogValue::Value)
            }
            MYSQL_TYPE_TIME2 => {
                let dec = col_meta[0];
                my_time_packed_from_binary(&mut *buf, dec as u32)
                    .map(time_from_packed)
                    .map(BinlogValue::Value)
            }
            MYSQL_TYPE_JSON => {
                length = buf.checked_eat_u32_le().ok_or_else(unexpected_buf_eof)? as usize;
                let mut json_value_buf =
                    buf.checked_eat_buf(length).ok_or_else(unexpected_buf_eof)?;
                if is_partial {
                    let mut diffs = Vec::new();
                    while !json_value_buf.is_empty() {
                        diffs.push(JsonDiff::deserialize((), &mut json_value_buf)?);
                    }
                    Ok(BinlogValue::JsonDiff(diffs))
                } else {
                    let value = jsonb::Value::deserialize((), &mut json_value_buf)?;
                    Ok(BinlogValue::Jsonb(value.into_owned()))
                }
            }
            MYSQL_TYPE_NEWDECIMAL => {
                // precision is the maximum number of decimal digits
                let precision = col_meta[0] as usize;
                // scale (aka decimals) is the number of decimal digits after the point
                let scale = col_meta[1] as usize;

                let dec = decimal::Decimal::read_bin(&mut *buf, precision, scale, false)?;

                Ok(BinlogValue::Value(Bytes(dec.to_string().into_bytes())))
            }
            MYSQL_TYPE_ENUM => match col_meta[1] {
                1 => {
                    let val = buf.checked_eat_u8().ok_or_else(unexpected_buf_eof)?;
                    Ok(BinlogValue::Value(Int(val as i64)))
                }
                2 => {
                    let val = buf.checked_eat_u16_le().ok_or_else(unexpected_buf_eof)?;
                    Ok(BinlogValue::Value(Int(val as i64)))
                }
                _ => Err(io::Error::new(io::ErrorKind::InvalidData, "Unknown ENUM")),
            },
            MYSQL_TYPE_SET => {
                let nbytes = col_meta[1] as usize * 8;
                let bytes = buf.checked_eat(nbytes).ok_or_else(unexpected_buf_eof)?;
                Ok(BinlogValue::Value(Bytes(bytes.into())))
            }
            MYSQL_TYPE_TINY_BLOB
            | MYSQL_TYPE_MEDIUM_BLOB
            | MYSQL_TYPE_LONG_BLOB
            | MYSQL_TYPE_BLOB => {
                let nbytes = match col_meta[0] {
                    1 => buf.checked_eat_u8().ok_or_else(unexpected_buf_eof)? as usize,
                    2 => buf.checked_eat_u16_le().ok_or_else(unexpected_buf_eof)? as usize,
                    3 => buf.checked_eat_u24_le().ok_or_else(unexpected_buf_eof)? as usize,
                    4 => buf.checked_eat_u32_le().ok_or_else(unexpected_buf_eof)? as usize,
                    _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "Unknown BLOB")),
                };
                let bytes = buf.checked_eat(nbytes).ok_or_else(unexpected_buf_eof)?;
                Ok(BinlogValue::Value(Bytes(bytes.into())))
            }
            MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING => {
                let type_len = (col_meta[0] as u16 | ((col_meta[1] as u16) << 8)) as usize;
                let nbytes = if type_len < 256 {
                    buf.checked_eat_u8().ok_or_else(unexpected_buf_eof)? as usize
                } else {
                    buf.checked_eat_u16_le().ok_or_else(unexpected_buf_eof)? as usize
                };
                let bytes = buf.checked_eat(nbytes).ok_or_else(unexpected_buf_eof)?;
                Ok(BinlogValue::Value(Bytes(bytes.into())))
            }
            MYSQL_TYPE_STRING => {
                let nbytes = if length < 256 {
                    buf.checked_eat_u8().ok_or_else(unexpected_buf_eof)? as usize
                } else {
                    buf.checked_eat_u16_le().ok_or_else(unexpected_buf_eof)? as usize
                };
                let bytes = buf.checked_eat(nbytes).ok_or_else(unexpected_buf_eof)?;
                Ok(BinlogValue::Value(Bytes(bytes.into())))
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Don't know how to handle column",
                ))
            }
        }
    }
}
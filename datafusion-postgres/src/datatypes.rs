use std::iter;
use std::str::FromStr;
use std::sync::Arc;

use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use chrono::{NaiveDate, NaiveDateTime};
use datafusion::arrow::array::*;
use datafusion::arrow::datatypes::*;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::{DFSchema, ParamValues};
use datafusion::prelude::*;
use datafusion::scalar::ScalarValue;
use futures::{stream, StreamExt};
use pgwire::api::portal::{Format, Portal};
use pgwire::api::results::{DataRowEncoder, FieldInfo, QueryResponse};
use pgwire::api::Type;
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use timezone::Tz;

pub(crate) fn into_pg_type(df_type: &DataType) -> PgWireResult<Type> {
    Ok(match df_type {
        DataType::Null => Type::UNKNOWN,
        DataType::Boolean => Type::BOOL,
        DataType::Int8 | DataType::UInt8 => Type::CHAR,
        DataType::Int16 | DataType::UInt16 => Type::INT2,
        DataType::Int32 | DataType::UInt32 => Type::INT4,
        DataType::Int64 | DataType::UInt64 => Type::INT8,
        DataType::Timestamp(_, tz) => {
            if tz.is_some() {
                Type::TIMESTAMPTZ
            } else {
                Type::TIMESTAMP
            }
        }
        DataType::Time32(_) | DataType::Time64(_) => Type::TIME,
        DataType::Date32 | DataType::Date64 => Type::DATE,
        DataType::Interval(_) => Type::INTERVAL,
        DataType::Binary | DataType::FixedSizeBinary(_) | DataType::LargeBinary => Type::BYTEA,
        DataType::Float16 | DataType::Float32 => Type::FLOAT4,
        DataType::Float64 => Type::FLOAT8,
        DataType::Utf8 => Type::VARCHAR,
        DataType::LargeUtf8 => Type::TEXT,
        DataType::List(field) | DataType::FixedSizeList(field, _) | DataType::LargeList(field) => {
            match field.data_type() {
                DataType::Boolean => Type::BOOL_ARRAY,
                DataType::Int8 | DataType::UInt8 => Type::CHAR_ARRAY,
                DataType::Int16 | DataType::UInt16 => Type::INT2_ARRAY,
                DataType::Int32 | DataType::UInt32 => Type::INT4_ARRAY,
                DataType::Int64 | DataType::UInt64 => Type::INT8_ARRAY,
                DataType::Timestamp(_, tz) => {
                    if tz.is_some() {
                        Type::TIMESTAMPTZ_ARRAY
                    } else {
                        Type::TIMESTAMP_ARRAY
                    }
                }
                DataType::Time32(_) | DataType::Time64(_) => Type::TIME_ARRAY,
                DataType::Date32 | DataType::Date64 => Type::DATE_ARRAY,
                DataType::Interval(_) => Type::INTERVAL_ARRAY,
                DataType::FixedSizeBinary(_) | DataType::Binary => Type::BYTEA_ARRAY,
                DataType::Float16 | DataType::Float32 => Type::FLOAT4_ARRAY,
                DataType::Float64 => Type::FLOAT8_ARRAY,
                DataType::Utf8 => Type::VARCHAR_ARRAY,
                DataType::LargeUtf8 => Type::TEXT_ARRAY,
                list_type => {
                    return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_owned(),
                        "XX000".to_owned(),
                        format!("Unsupported List Datatype {list_type}"),
                    ))));
                }
            }
        }
        DataType::Utf8View => Type::TEXT,
        _ => {
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_owned(),
                "XX000".to_owned(),
                format!("Unsupported Datatype {df_type}"),
            ))));
        }
    })
}

fn get_bool_value(arr: &Arc<dyn Array>, idx: usize) -> bool {
    arr.as_any()
        .downcast_ref::<BooleanArray>()
        .unwrap()
        .value(idx)
}

fn get_bool_list_value(arr: &Arc<dyn Array>, idx: usize) -> Vec<Option<bool>> {
    let list_arr = arr.as_any().downcast_ref::<ListArray>().unwrap().value(idx);
    list_arr
        .as_any()
        .downcast_ref::<BooleanArray>()
        .unwrap()
        .iter()
        .collect()
}

macro_rules! get_primitive_value {
    ($name:ident, $t:ty, $pt:ty) => {
        fn $name(arr: &Arc<dyn Array>, idx: usize) -> $pt {
            arr.as_any()
                .downcast_ref::<PrimitiveArray<$t>>()
                .unwrap()
                .value(idx)
        }
    };
}

get_primitive_value!(get_i8_value, Int8Type, i8);
get_primitive_value!(get_i16_value, Int16Type, i16);
get_primitive_value!(get_i32_value, Int32Type, i32);
get_primitive_value!(get_i64_value, Int64Type, i64);
get_primitive_value!(get_u8_value, UInt8Type, u8);
get_primitive_value!(get_u16_value, UInt16Type, u16);
get_primitive_value!(get_u32_value, UInt32Type, u32);
get_primitive_value!(get_u64_value, UInt64Type, u64);
get_primitive_value!(get_f32_value, Float32Type, f32);
get_primitive_value!(get_f64_value, Float64Type, f64);

macro_rules! get_primitive_list_value {
    ($name:ident, $t:ty, $pt:ty) => {
        fn $name(arr: &Arc<dyn Array>, idx: usize) -> Vec<Option<$pt>> {
            let list_arr = arr.as_any().downcast_ref::<ListArray>().unwrap().value(idx);
            list_arr
                .as_any()
                .downcast_ref::<PrimitiveArray<$t>>()
                .unwrap()
                .iter()
                .collect()
        }
    };

    ($name:ident, $t:ty, $pt:ty, $f:expr) => {
        fn $name(arr: &Arc<dyn Array>, idx: usize) -> Vec<Option<$pt>> {
            let list_arr = arr.as_any().downcast_ref::<ListArray>().unwrap().value(idx);
            list_arr
                .as_any()
                .downcast_ref::<PrimitiveArray<$t>>()
                .unwrap()
                .iter()
                .map(|val| val.map($f))
                .collect()
        }
    };
}

get_primitive_list_value!(get_i8_list_value, Int8Type, i8);
get_primitive_list_value!(get_i16_list_value, Int16Type, i16);
get_primitive_list_value!(get_i32_list_value, Int32Type, i32);
get_primitive_list_value!(get_i64_list_value, Int64Type, i64);
get_primitive_list_value!(get_u8_list_value, UInt8Type, i8, |val: u8| { val as i8 });
get_primitive_list_value!(get_u16_list_value, UInt16Type, i16, |val: u16| {
    val as i16
});
get_primitive_list_value!(get_u32_list_value, UInt32Type, u32);
get_primitive_list_value!(get_u64_list_value, UInt64Type, i64, |val: u64| {
    val as i64
});
get_primitive_list_value!(get_f32_list_value, Float32Type, f32);
get_primitive_list_value!(get_f64_list_value, Float64Type, f64);

fn get_utf8_view_value(arr: &Arc<dyn Array>, idx: usize) -> &str {
    arr.as_any()
        .downcast_ref::<StringViewArray>()
        .unwrap()
        .value(idx)
}

fn get_utf8_value(arr: &Arc<dyn Array>, idx: usize) -> &str {
    arr.as_any()
        .downcast_ref::<StringArray>()
        .unwrap()
        .value(idx)
}

fn get_large_utf8_value(arr: &Arc<dyn Array>, idx: usize) -> &str {
    arr.as_any()
        .downcast_ref::<LargeStringArray>()
        .unwrap()
        .value(idx)
}

fn get_binary_value(arr: &Arc<dyn Array>, idx: usize) -> &[u8] {
    arr.as_any()
        .downcast_ref::<BinaryArray>()
        .unwrap()
        .value(idx)
}

fn get_large_binary_value(arr: &Arc<dyn Array>, idx: usize) -> &[u8] {
    arr.as_any()
        .downcast_ref::<LargeBinaryArray>()
        .unwrap()
        .value(idx)
}

fn get_date32_value(arr: &Arc<dyn Array>, idx: usize) -> Option<NaiveDate> {
    arr.as_any()
        .downcast_ref::<Date32Array>()
        .unwrap()
        .value_as_date(idx)
}

fn get_date64_value(arr: &Arc<dyn Array>, idx: usize) -> Option<NaiveDate> {
    arr.as_any()
        .downcast_ref::<Date64Array>()
        .unwrap()
        .value_as_date(idx)
}

fn get_time32_second_value(arr: &Arc<dyn Array>, idx: usize) -> Option<NaiveDateTime> {
    arr.as_any()
        .downcast_ref::<Time32SecondArray>()
        .unwrap()
        .value_as_datetime(idx)
}

fn get_time32_millisecond_value(arr: &Arc<dyn Array>, idx: usize) -> Option<NaiveDateTime> {
    arr.as_any()
        .downcast_ref::<Time32MillisecondArray>()
        .unwrap()
        .value_as_datetime(idx)
}

fn get_time64_microsecond_value(arr: &Arc<dyn Array>, idx: usize) -> Option<NaiveDateTime> {
    arr.as_any()
        .downcast_ref::<Time64MicrosecondArray>()
        .unwrap()
        .value_as_datetime(idx)
}
fn get_time64_nanosecond_value(arr: &Arc<dyn Array>, idx: usize) -> Option<NaiveDateTime> {
    arr.as_any()
        .downcast_ref::<Time64NanosecondArray>()
        .unwrap()
        .value_as_datetime(idx)
}

fn encode_value(
    encoder: &mut DataRowEncoder,
    arr: &Arc<dyn Array>,
    idx: usize,
) -> PgWireResult<()> {
    match arr.data_type() {
        DataType::Null => encoder.encode_field(&None::<i8>)?,
        DataType::Boolean => encoder.encode_field(&get_bool_value(arr, idx))?,
        DataType::Int8 => encoder.encode_field(&get_i8_value(arr, idx))?,
        DataType::Int16 => encoder.encode_field(&get_i16_value(arr, idx))?,
        DataType::Int32 => encoder.encode_field(&get_i32_value(arr, idx))?,
        DataType::Int64 => encoder.encode_field(&get_i64_value(arr, idx))?,
        DataType::UInt8 => encoder.encode_field(&(get_u8_value(arr, idx) as i8))?,
        DataType::UInt16 => encoder.encode_field(&(get_u16_value(arr, idx) as i16))?,
        DataType::UInt32 => encoder.encode_field(&get_u32_value(arr, idx))?,
        DataType::UInt64 => encoder.encode_field(&(get_u64_value(arr, idx) as i64))?,
        DataType::Float32 => encoder.encode_field(&get_f32_value(arr, idx))?,
        DataType::Float64 => encoder.encode_field(&get_f64_value(arr, idx))?,
        DataType::Utf8 => encoder.encode_field(&get_utf8_value(arr, idx))?,
        DataType::Utf8View => encoder.encode_field(&get_utf8_view_value(arr, idx))?,
        DataType::LargeUtf8 => encoder.encode_field(&get_large_utf8_value(arr, idx))?,
        DataType::Binary => encoder.encode_field(&get_binary_value(arr, idx))?,
        DataType::LargeBinary => encoder.encode_field(&get_large_binary_value(arr, idx))?,
        DataType::Date32 => encoder.encode_field(&get_date32_value(arr, idx))?,
        DataType::Date64 => encoder.encode_field(&get_date64_value(arr, idx))?,
        DataType::Time32(unit) => match unit {
            TimeUnit::Second => encoder.encode_field(&get_time32_second_value(arr, idx))?,
            TimeUnit::Millisecond => {
                encoder.encode_field(&get_time32_millisecond_value(arr, idx))?
            }
            _ => {}
        },
        DataType::Time64(unit) => match unit {
            TimeUnit::Microsecond => {
                encoder.encode_field(&get_time64_microsecond_value(arr, idx))?
            }
            TimeUnit::Nanosecond => encoder.encode_field(&get_time64_nanosecond_value(arr, idx))?,
            _ => {}
        },
        DataType::Timestamp(unit, timezone) => match unit {
            TimeUnit::Second => {
                let ts_array = arr.as_any().downcast_ref::<TimestampSecondArray>().unwrap();
                if let Some(tz) = timezone {
                    let tz = Tz::from_str(tz.as_ref())
                        .map_err(|e| PgWireError::ApiError(Box::new(e)))?;
                    let value = ts_array
                        .value_as_datetime_with_tz(idx, tz)
                        .map(|d| d.fixed_offset());
                    encoder.encode_field(&value)?;
                } else {
                    let value = ts_array.value_as_datetime(idx);
                    encoder.encode_field(&value)?
                }
            }
            TimeUnit::Millisecond => {
                let ts_array = arr
                    .as_any()
                    .downcast_ref::<TimestampMillisecondArray>()
                    .unwrap();
                if let Some(tz) = timezone {
                    let tz = Tz::from_str(tz.as_ref())
                        .map_err(|e| PgWireError::ApiError(Box::new(e)))?;
                    let value = ts_array
                        .value_as_datetime_with_tz(idx, tz)
                        .map(|d| d.fixed_offset());
                    encoder.encode_field(&value)?;
                } else {
                    let value = ts_array.value_as_datetime(idx);
                    encoder.encode_field(&value)?
                }
            }
            TimeUnit::Microsecond => {
                let ts_array = arr
                    .as_any()
                    .downcast_ref::<TimestampMicrosecondArray>()
                    .unwrap();
                if let Some(tz) = timezone {
                    let tz = Tz::from_str(tz.as_ref())
                        .map_err(|e| PgWireError::ApiError(Box::new(e)))?;
                    let value = ts_array
                        .value_as_datetime_with_tz(idx, tz)
                        .map(|d| d.fixed_offset());
                    encoder.encode_field(&value)?;
                } else {
                    let value = ts_array.value_as_datetime(idx);
                    encoder.encode_field(&value)?
                }
            }
            TimeUnit::Nanosecond => {
                let ts_array = arr
                    .as_any()
                    .downcast_ref::<TimestampNanosecondArray>()
                    .unwrap();
                if let Some(tz) = timezone {
                    let tz = Tz::from_str(tz.as_ref())
                        .map_err(|e| PgWireError::ApiError(Box::new(e)))?;
                    let value = ts_array
                        .value_as_datetime_with_tz(idx, tz)
                        .map(|d| d.fixed_offset());
                    encoder.encode_field(&value)?;
                } else {
                    let value = ts_array.value_as_datetime(idx);
                    encoder.encode_field(&value)?
                }
            }
        },

        DataType::List(field) | DataType::FixedSizeList(field, _) | DataType::LargeList(field) => {
            match field.data_type() {
                DataType::Null => encoder.encode_field(&None::<i8>)?,
                DataType::Boolean => encoder.encode_field(&get_bool_list_value(arr, idx))?,
                DataType::Int8 => encoder.encode_field(&get_i8_list_value(arr, idx))?,
                DataType::Int16 => encoder.encode_field(&get_i16_list_value(arr, idx))?,
                DataType::Int32 => encoder.encode_field(&get_i32_list_value(arr, idx))?,
                DataType::Int64 => encoder.encode_field(&get_i64_list_value(arr, idx))?,
                DataType::UInt8 => encoder.encode_field(&get_u8_list_value(arr, idx))?,
                DataType::UInt16 => encoder.encode_field(&get_u16_list_value(arr, idx))?,
                DataType::UInt32 => encoder.encode_field(&get_u32_list_value(arr, idx))?,
                DataType::UInt64 => encoder.encode_field(&get_u64_list_value(arr, idx))?,
                DataType::Float32 => encoder.encode_field(&get_f32_list_value(arr, idx))?,
                DataType::Float64 => encoder.encode_field(&get_f64_list_value(arr, idx))?,
                DataType::Utf8 => {
                    let list_arr = arr.as_any().downcast_ref::<ListArray>().unwrap().value(idx);
                    let value: Vec<_> = list_arr
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .unwrap()
                        .iter()
                        .collect();
                    encoder.encode_field(&value)?
                }
                DataType::Binary => {
                    let list_arr = arr.as_any().downcast_ref::<ListArray>().unwrap().value(idx);
                    let value: Vec<_> = list_arr
                        .as_any()
                        .downcast_ref::<BinaryArray>()
                        .unwrap()
                        .iter()
                        .collect();
                    encoder.encode_field(&value)?
                }
                DataType::LargeBinary => {
                    let list_arr = arr.as_any().downcast_ref::<ListArray>().unwrap().value(idx);
                    let value: Vec<_> = list_arr
                        .as_any()
                        .downcast_ref::<LargeBinaryArray>()
                        .unwrap()
                        .iter()
                        .collect();
                    encoder.encode_field(&value)?
                }

                DataType::Date32 => {
                    let list_arr = arr.as_any().downcast_ref::<ListArray>().unwrap().value(idx);
                    let value: Vec<_> = list_arr
                        .as_any()
                        .downcast_ref::<Date32Array>()
                        .unwrap()
                        .iter()
                        .collect();
                    encoder.encode_field(&value)?
                }
                DataType::Date64 => {
                    let list_arr = arr.as_any().downcast_ref::<ListArray>().unwrap().value(idx);
                    let value: Vec<_> = list_arr
                        .as_any()
                        .downcast_ref::<Date64Array>()
                        .unwrap()
                        .iter()
                        .collect();
                    encoder.encode_field(&value)?
                }
                DataType::Time32(unit) => match unit {
                    TimeUnit::Second => {
                        let list_arr = arr.as_any().downcast_ref::<ListArray>().unwrap().value(idx);
                        let value: Vec<_> = list_arr
                            .as_any()
                            .downcast_ref::<Time32SecondArray>()
                            .unwrap()
                            .iter()
                            .collect();
                        encoder.encode_field(&value)?
                    }
                    TimeUnit::Millisecond => {
                        let list_arr = arr.as_any().downcast_ref::<ListArray>().unwrap().value(idx);
                        let value: Vec<_> = list_arr
                            .as_any()
                            .downcast_ref::<Time32MillisecondArray>()
                            .unwrap()
                            .iter()
                            .collect();
                        encoder.encode_field(&value)?
                    }
                    _ => {}
                },
                DataType::Time64(unit) => match unit {
                    TimeUnit::Microsecond => {
                        let list_arr = arr.as_any().downcast_ref::<ListArray>().unwrap().value(idx);
                        let value: Vec<_> = list_arr
                            .as_any()
                            .downcast_ref::<Time64MicrosecondArray>()
                            .unwrap()
                            .iter()
                            .collect();
                        encoder.encode_field(&value)?
                    }
                    TimeUnit::Nanosecond => {
                        let list_arr = arr.as_any().downcast_ref::<ListArray>().unwrap().value(idx);
                        let value: Vec<_> = list_arr
                            .as_any()
                            .downcast_ref::<Time64NanosecondArray>()
                            .unwrap()
                            .iter()
                            .collect();
                        encoder.encode_field(&value)?
                    }
                    _ => {}
                },
                DataType::Timestamp(unit, timezone) => match unit {
                    TimeUnit::Second => {
                        let list_array =
                            arr.as_any().downcast_ref::<ListArray>().unwrap().value(idx);
                        let array_iter = list_array
                            .as_any()
                            .downcast_ref::<TimestampSecondArray>()
                            .unwrap()
                            .iter();

                        if let Some(tz) = timezone {
                            let tz = Tz::from_str(tz.as_ref())
                                .map_err(|e| PgWireError::ApiError(Box::new(e)))?;
                            let value: Vec<_> = array_iter
                                .map(|i| {
                                    i.and_then(|i| {
                                        DateTime::from_timestamp(i, 0).map(|dt| {
                                            Utc.from_utc_datetime(&dt.naive_utc())
                                                .with_timezone(&tz)
                                                .fixed_offset()
                                        })
                                    })
                                })
                                .collect();
                            encoder.encode_field(&value)?;
                        } else {
                            let value: Vec<_> = array_iter
                                .map(|i| {
                                    i.and_then(|i| {
                                        DateTime::from_timestamp(i, 0).map(|dt| dt.naive_utc())
                                    })
                                })
                                .collect();
                            encoder.encode_field(&value)?
                        }
                    }
                    TimeUnit::Millisecond => {
                        let list_array =
                            arr.as_any().downcast_ref::<ListArray>().unwrap().value(idx);
                        let array_iter = list_array
                            .as_any()
                            .downcast_ref::<TimestampMillisecondArray>()
                            .unwrap()
                            .iter();

                        if let Some(tz) = timezone {
                            let tz = Tz::from_str(tz.as_ref())
                                .map_err(|e| PgWireError::ApiError(Box::new(e)))?;
                            let value: Vec<_> = array_iter
                                .map(|i| {
                                    i.and_then(|i| {
                                        DateTime::from_timestamp_millis(i).map(|dt| {
                                            Utc.from_utc_datetime(&dt.naive_utc())
                                                .with_timezone(&tz)
                                                .fixed_offset()
                                        })
                                    })
                                })
                                .collect();
                            encoder.encode_field(&value)?;
                        } else {
                            let value: Vec<_> = array_iter
                                .map(|i| {
                                    i.and_then(|i| {
                                        DateTime::from_timestamp_millis(i).map(|dt| dt.naive_utc())
                                    })
                                })
                                .collect();
                            encoder.encode_field(&value)?
                        }
                    }
                    TimeUnit::Microsecond => {
                        let list_array =
                            arr.as_any().downcast_ref::<ListArray>().unwrap().value(idx);
                        let array_iter = list_array
                            .as_any()
                            .downcast_ref::<TimestampMicrosecondArray>()
                            .unwrap()
                            .iter();

                        if let Some(tz) = timezone {
                            let tz = Tz::from_str(tz.as_ref())
                                .map_err(|e| PgWireError::ApiError(Box::new(e)))?;
                            let value: Vec<_> = array_iter
                                .map(|i| {
                                    i.and_then(|i| {
                                        DateTime::from_timestamp_micros(i).map(|dt| {
                                            Utc.from_utc_datetime(&dt.naive_utc())
                                                .with_timezone(&tz)
                                                .fixed_offset()
                                        })
                                    })
                                })
                                .collect();
                            encoder.encode_field(&value)?;
                        } else {
                            let value: Vec<_> = array_iter
                                .map(|i| {
                                    i.and_then(|i| {
                                        DateTime::from_timestamp_micros(i).map(|dt| dt.naive_utc())
                                    })
                                })
                                .collect();
                            encoder.encode_field(&value)?
                        }
                    }
                    TimeUnit::Nanosecond => {
                        let list_array =
                            arr.as_any().downcast_ref::<ListArray>().unwrap().value(idx);
                        let array_iter = list_array
                            .as_any()
                            .downcast_ref::<TimestampNanosecondArray>()
                            .unwrap()
                            .iter();

                        if let Some(tz) = timezone {
                            let tz = Tz::from_str(tz.as_ref())
                                .map_err(|e| PgWireError::ApiError(Box::new(e)))?;
                            let value: Vec<_> = array_iter
                                .map(|i| {
                                    i.map(|i| {
                                        Utc.from_utc_datetime(
                                            &DateTime::from_timestamp_nanos(i).naive_utc(),
                                        )
                                        .with_timezone(&tz)
                                        .fixed_offset()
                                    })
                                })
                                .collect();
                            encoder.encode_field(&value)?;
                        } else {
                            let value: Vec<_> = array_iter
                                .map(|i| i.map(|i| DateTime::from_timestamp_nanos(i).naive_utc()))
                                .collect();
                            encoder.encode_field(&value)?
                        }
                    }
                },

                // TODO: more types
                list_type => {
                    return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_owned(),
                        "XX000".to_owned(),
                        format!(
                            "Unsupported List Datatype {} and array {:?}",
                            list_type, &arr
                        ),
                    ))))
                }
            }
        }
        _ => {
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_owned(),
                "XX000".to_owned(),
                format!(
                    "Unsupported Datatype {} and array {:?}",
                    arr.data_type(),
                    &arr
                ),
            ))))
        }
    }
    Ok(())
}

pub(crate) fn df_schema_to_pg_fields(
    schema: &DFSchema,
    format: &Format,
) -> PgWireResult<Vec<FieldInfo>> {
    schema
        .fields()
        .iter()
        .enumerate()
        .map(|(idx, f)| {
            let pg_type = into_pg_type(f.data_type())?;
            Ok(FieldInfo::new(
                f.name().into(),
                None,
                None,
                pg_type,
                format.format_for(idx),
            ))
        })
        .collect::<PgWireResult<Vec<FieldInfo>>>()
}

pub(crate) async fn encode_dataframe<'a>(
    df: DataFrame,
    format: &Format,
) -> PgWireResult<QueryResponse<'a>> {
    let fields = Arc::new(df_schema_to_pg_fields(df.schema(), format)?);

    let recordbatch_stream = df
        .execute_stream()
        .await
        .map_err(|e| PgWireError::ApiError(Box::new(e)))?;

    let fields_ref = fields.clone();
    let pg_row_stream = recordbatch_stream
        .map(move |rb: datafusion::error::Result<RecordBatch>| {
            let row_stream: Box<dyn Iterator<Item = _> + Send> = match rb {
                Ok(rb) => {
                    let rows = rb.num_rows();
                    let cols = rb.num_columns();

                    let fields = fields_ref.clone();

                    let row_stream = (0..rows).map(move |row| {
                        let mut encoder = DataRowEncoder::new(fields.clone());
                        for col in 0..cols {
                            let array = rb.column(col);
                            if array.is_null(row) {
                                encoder.encode_field(&None::<i8>).unwrap();
                            } else {
                                encode_value(&mut encoder, array, row).unwrap();
                            }
                        }
                        encoder.finish()
                    });
                    Box::new(row_stream)
                }
                Err(e) => Box::new(iter::once(Err(PgWireError::ApiError(e.into())))),
            };

            stream::iter(row_stream)
        })
        .flatten();

    Ok(QueryResponse::new(fields, pg_row_stream))
}

/// Deserialize client provided parameter data.
///
/// First we try to use the type information from `pg_type_hint`, which is
/// provided by the client.
/// If the type is empty or unknown, we fallback to datafusion inferenced type
/// from `inferenced_types`.
/// An error will be raised when neither sources can provide type information.
pub(crate) fn deserialize_parameters<S>(
    portal: &Portal<S>,
    inferenced_types: &[Option<&DataType>],
) -> PgWireResult<ParamValues>
where
    S: Clone,
{
    fn get_pg_type(
        pg_type_hint: Option<&Type>,
        inferenced_type: Option<&DataType>,
    ) -> PgWireResult<Type> {
        if let Some(ty) = pg_type_hint {
            Ok(ty.clone())
        } else if let Some(infer_type) = inferenced_type {
            into_pg_type(infer_type)
        } else {
            Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                "XX000".to_string(),
                "Unknown parameter type".to_string(),
            ))))
        }
    }

    let param_len = portal.parameter_len();
    let mut deserialized_params = Vec::with_capacity(param_len);
    for i in 0..param_len {
        let pg_type = get_pg_type(
            portal.statement.parameter_types.get(i),
            inferenced_types.get(i).and_then(|v| v.to_owned()),
        )?;
        match pg_type {
            // enumerate all supported parameter types and deserialize the
            // type to ScalarValue
            Type::BOOL => {
                let value = portal.parameter::<bool>(i, &pg_type)?;
                deserialized_params.push(ScalarValue::Boolean(value));
            }
            Type::CHAR => {
                let value = portal.parameter::<i8>(i, &pg_type)?;
                deserialized_params.push(ScalarValue::Int8(value));
            }
            Type::INT2 => {
                let value = portal.parameter::<i16>(i, &pg_type)?;
                deserialized_params.push(ScalarValue::Int16(value));
            }
            Type::INT4 => {
                let value = portal.parameter::<i32>(i, &pg_type)?;
                deserialized_params.push(ScalarValue::Int32(value));
            }
            Type::INT8 => {
                let value = portal.parameter::<i64>(i, &pg_type)?;
                deserialized_params.push(ScalarValue::Int64(value));
            }
            Type::TEXT | Type::VARCHAR => {
                let value = portal.parameter::<String>(i, &pg_type)?;
                deserialized_params.push(ScalarValue::Utf8(value));
            }
            Type::BYTEA => {
                let value = portal.parameter::<Vec<u8>>(i, &pg_type)?;
                deserialized_params.push(ScalarValue::Binary(value));
            }

            Type::FLOAT4 => {
                let value = portal.parameter::<f32>(i, &pg_type)?;
                deserialized_params.push(ScalarValue::Float32(value));
            }
            Type::FLOAT8 => {
                let value = portal.parameter::<f64>(i, &pg_type)?;
                deserialized_params.push(ScalarValue::Float64(value));
            }
            Type::TIMESTAMP => {
                let value = portal.parameter::<NaiveDateTime>(i, &pg_type)?;
                deserialized_params.push(ScalarValue::TimestampMicrosecond(
                    value.map(|t| t.and_utc().timestamp_micros()),
                    None,
                ));
            }
            Type::TIMESTAMPTZ => {
                let value = portal.parameter::<DateTime<FixedOffset>>(i, &pg_type)?;
                deserialized_params.push(ScalarValue::TimestampMicrosecond(
                    value.map(|t| t.timestamp_micros()),
                    value.map(|t| t.offset().to_string().into()),
                ));
            }
            Type::DATE => {
                let value = portal.parameter::<NaiveDate>(i, &pg_type)?;
                deserialized_params
                    .push(ScalarValue::Date32(value.map(Date32Type::from_naive_date)));
            }
            // TODO: add more types
            _ => {
                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "FATAL".to_string(),
                    "XX000".to_string(),
                    format!("Unsupported parameter type: {}", pg_type),
                ))));
            }
        }
    }

    Ok(ParamValues::List(deserialized_params))
}

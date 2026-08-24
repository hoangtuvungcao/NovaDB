//! Extended Mathematical, Trigonometric, and Geospatial Functions for NovaDB.

use rusqlite::functions::FunctionFlags;
use rusqlite::Connection;

use crate::Result;

/// Registers mathematical and geospatial functions on the supplied connection.
pub fn register(connection: &Connection) -> Result<()> {
    let deterministic = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;

    // PI() -> 3.141592653589793
    connection.create_scalar_function("pi", 0, deterministic, |_ctx| {
        Ok(std::f64::consts::PI)
    })?;

    // POWER(x, y) / POW(x, y)
    connection.create_scalar_function("power", 2, deterministic, |ctx| {
        let x: f64 = ctx.get(0)?;
        let y: f64 = ctx.get(1)?;
        Ok(x.powf(y))
    })?;
    connection.create_scalar_function("pow", 2, deterministic, |ctx| {
        let x: f64 = ctx.get(0)?;
        let y: f64 = ctx.get(1)?;
        Ok(x.powf(y))
    })?;

    // SQRT(x)
    connection.create_scalar_function("sqrt", 1, deterministic, |ctx| {
        let x: f64 = ctx.get(0)?;
        if x < 0.0 {
            return Err(rusqlite::Error::UserFunctionError("cannot calculate sqrt of negative number".to_string().into()));
        }
        Ok(x.sqrt())
    })?;

    // CBRT(x)
    connection.create_scalar_function("cbrt", 1, deterministic, |ctx| {
        let x: f64 = ctx.get(0)?;
        Ok(x.cbrt())
    })?;

    // EXP(x)
    connection.create_scalar_function("exp", 1, deterministic, |ctx| {
        let x: f64 = ctx.get(0)?;
        Ok(x.exp())
    })?;

    // LN(x)
    connection.create_scalar_function("ln", 1, deterministic, |ctx| {
        let x: f64 = ctx.get(0)?;
        if x <= 0.0 {
            return Err(rusqlite::Error::UserFunctionError("ln argument must be positive".to_string().into()));
        }
        Ok(x.ln())
    })?;

    // LOG10(x)
    connection.create_scalar_function("log10", 1, deterministic, |ctx| {
        let x: f64 = ctx.get(0)?;
        if x <= 0.0 {
            return Err(rusqlite::Error::UserFunctionError("log10 argument must be positive".to_string().into()));
        }
        Ok(x.log10())
    })?;

    // LOG2(x)
    connection.create_scalar_function("log2", 1, deterministic, |ctx| {
        let x: f64 = ctx.get(0)?;
        if x <= 0.0 {
            return Err(rusqlite::Error::UserFunctionError("log2 argument must be positive".to_string().into()));
        }
        Ok(x.log2())
    })?;

    // SIN(x), COS(x), TAN(x)
    connection.create_scalar_function("sin", 1, deterministic, |ctx| {
        let x: f64 = ctx.get(0)?;
        Ok(x.sin())
    })?;
    connection.create_scalar_function("cos", 1, deterministic, |ctx| {
        let x: f64 = ctx.get(0)?;
        Ok(x.cos())
    })?;
    connection.create_scalar_function("tan", 1, deterministic, |ctx| {
        let x: f64 = ctx.get(0)?;
        Ok(x.tan())
    })?;

    // ASIN(x), ACOS(x), ATAN(x), ATAN2(y, x)
    connection.create_scalar_function("asin", 1, deterministic, |ctx| {
        let x: f64 = ctx.get(0)?;
        Ok(x.asin())
    })?;
    connection.create_scalar_function("acos", 1, deterministic, |ctx| {
        let x: f64 = ctx.get(0)?;
        Ok(x.acos())
    })?;
    connection.create_scalar_function("atan", 1, deterministic, |ctx| {
        let x: f64 = ctx.get(0)?;
        Ok(x.atan())
    })?;
    connection.create_scalar_function("atan2", 2, deterministic, |ctx| {
        let y: f64 = ctx.get(0)?;
        let x: f64 = ctx.get(1)?;
        Ok(y.atan2(x))
    })?;

    // DEGREES(rad), RADIANS(deg)
    connection.create_scalar_function("degrees", 1, deterministic, |ctx| {
        let rad: f64 = ctx.get(0)?;
        Ok(rad.to_degrees())
    })?;
    connection.create_scalar_function("radians", 1, deterministic, |ctx| {
        let deg: f64 = ctx.get(0)?;
        Ok(deg.to_radians())
    })?;

    // FLOOR(x), CEIL(x), CEILING(x), TRUNC(x)
    connection.create_scalar_function("floor", 1, deterministic, |ctx| {
        let x: f64 = ctx.get(0)?;
        Ok(x.floor())
    })?;
    connection.create_scalar_function("ceil", 1, deterministic, |ctx| {
        let x: f64 = ctx.get(0)?;
        Ok(x.ceil())
    })?;
    connection.create_scalar_function("ceiling", 1, deterministic, |ctx| {
        let x: f64 = ctx.get(0)?;
        Ok(x.ceil())
    })?;
    connection.create_scalar_function("trunc", 1, deterministic, |ctx| {
        let x: f64 = ctx.get(0)?;
        Ok(x.trunc())
    })?;

    // SIGN(x)
    connection.create_scalar_function("sign", 1, deterministic, |ctx| {
        let x: f64 = ctx.get(0)?;
        if x > 0.0 {
            Ok(1)
        } else if x < 0.0 {
            Ok(-1)
        } else {
            Ok(0)
        }
    })?;

    // MOD(x, y)
    connection.create_scalar_function("mod", 2, deterministic, |ctx| {
        let x: i64 = ctx.get(0)?;
        let y: i64 = ctx.get(1)?;
        if y == 0 {
            return Err(rusqlite::Error::UserFunctionError("division by zero in mod".to_string().into()));
        }
        Ok(x % y)
    })?;

    // GEO_HAVERSINE_DISTANCE(lat1, lon1, lat2, lon2) -> distance in meters
    connection.create_scalar_function("geo_haversine_distance", 4, deterministic, |ctx| {
        let lat1: f64 = ctx.get(0)?;
        let lon1: f64 = ctx.get(1)?;
        let lat2: f64 = ctx.get(2)?;
        let lon2: f64 = ctx.get(3)?;

        let r = 6371000.0; // Earth radius in meters
        let d_lat = (lat2 - lat1).to_radians();
        let d_lon = (lon2 - lon1).to_radians();

        let a = (d_lat / 2.0).sin().powi(2)
            + lat1.to_radians().cos() * lat2.to_radians().cos() * (d_lon / 2.0).sin().powi(2);
        let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());

        Ok(r * c)
    })?;

    // GEO_DISTANCE_KM(lat1, lon1, lat2, lon2) -> distance in kilometers
    connection.create_scalar_function("geo_distance_km", 4, deterministic, |ctx| {
        let lat1: f64 = ctx.get(0)?;
        let lon1: f64 = ctx.get(1)?;
        let lat2: f64 = ctx.get(2)?;
        let lon2: f64 = ctx.get(3)?;

        let r = 6371.0f64; // Earth radius in km
        let d_lat = (lat2 - lat1).to_radians();
        let d_lon = (lon2 - lon1).to_radians();

        let a = (d_lat / 2.0).sin().powi(2)
            + lat1.to_radians().cos() * lat2.to_radians().cos() * (d_lon / 2.0).sin().powi(2);
        let c = 2.0 * a.sqrt().atan2((1.0f64 - a).sqrt());

        Ok(r * c)
    })?;

    // CHOOSE(index, val1, val2, ...)
    connection.create_scalar_function("choose", -1, deterministic, |ctx| {
        if ctx.len() < 2 {
            return Ok(rusqlite::types::Value::Null);
        }
        let index: i64 = ctx.get(0)?;
        if index < 1 || index >= ctx.len() as i64 {
            return Ok(rusqlite::types::Value::Null);
        }
        let val: rusqlite::types::Value = ctx.get(index as usize)?;
        Ok(val)
    })?;

    // GREATEST(v1, v2, ...)
    connection.create_scalar_function("greatest", -1, deterministic, |ctx| {
        if ctx.len() == 0 {
            return Ok(rusqlite::types::Value::Null);
        }
        let mut max_val: Option<f64> = None;
        for i in 0..ctx.len() {
            if let Ok(v) = ctx.get::<f64>(i) {
                max_val = Some(match max_val {
                    Some(cur) => cur.max(v),
                    None => v,
                });
            }
        }
        match max_val {
            Some(v) => {
                if v.fract() == 0.0 {
                    Ok(rusqlite::types::Value::Integer(v as i64))
                } else {
                    Ok(rusqlite::types::Value::Real(v))
                }
            }
            None => Ok(rusqlite::types::Value::Null),
        }
    })?;

    // LEAST(v1, v2, ...)
    connection.create_scalar_function("least", -1, deterministic, |ctx| {
        if ctx.len() == 0 {
            return Ok(rusqlite::types::Value::Null);
        }
        let mut min_val: Option<f64> = None;
        for i in 0..ctx.len() {
            if let Ok(v) = ctx.get::<f64>(i) {
                min_val = Some(match min_val {
                    Some(cur) => cur.min(v),
                    None => v,
                });
            }
        }
        match min_val {
            Some(v) => {
                if v.fract() == 0.0 {
                    Ok(rusqlite::types::Value::Integer(v as i64))
                } else {
                    Ok(rusqlite::types::Value::Real(v))
                }
            }
            None => Ok(rusqlite::types::Value::Null),
        }
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        super::register(&conn).unwrap();
        conn
    }

    #[test]
    fn test_math_functions() {
        let conn = setup();
        let pi: f64 = conn.query_row("SELECT pi()", [], |r| r.get(0)).unwrap();
        assert!((pi - std::f64::consts::PI).abs() < 1e-10);

        let p: f64 = conn.query_row("SELECT power(2.0, 3.0)", [], |r| r.get(0)).unwrap();
        assert_eq!(p, 8.0);

        let sq: f64 = conn.query_row("SELECT sqrt(16.0)", [], |r| r.get(0)).unwrap();
        assert_eq!(sq, 4.0);

        let s: i64 = conn.query_row("SELECT sign(-42)", [], |r| r.get(0)).unwrap();
        assert_eq!(s, -1);

        let m: i64 = conn.query_row("SELECT mod(10, 3)", [], |r| r.get(0)).unwrap();
        assert_eq!(m, 1);
    }

    #[test]
    fn test_geospatial_haversine() {
        let conn = setup();
        // Hanoi (21.0285, 105.8542) to HCMC (10.8231, 106.6297) ~ 1130-1150 km
        let dist_km: f64 = conn.query_row(
            "SELECT geo_distance_km(21.0285, 105.8542, 10.8231, 106.6297)",
            [],
            |r| r.get(0),
        ).unwrap();
        assert!(dist_km > 1100.0 && dist_km < 1200.0);
    }
}

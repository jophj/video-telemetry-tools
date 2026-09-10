use anyhow::Result;
use gmax::math::*;

fn main() -> Result<()> {
    let path = std::env::args().nth(1).unwrap_or_default();
    let mut f = gmax::mp4::Mp4::open(&path)?;
    let g = gmax::gpmf::from_mp4(&mut f)?;
    let opts = gmax::telemetry::Options::default();
    let t = gmax::telemetry::compute(&g, &opts)?;

    println!("device={:?} n={} hz={:.5} hz_nominal={} dur={:.3}s", t.device_name, t.n, t.hz, t.hz_nominal, t.duration_s);
    println!("t0={:?} fallback={}", t.t0.map(|d| d.to_rfc3339()), t.t0_is_fallback);
    println!("gps: {} fixes @ {:?} Hz, {} valid ({:.1}%)",
        t.gps.len(), t.gps_hz().map(|h| format!("{h:.4}")), t.valid_fix_count(), t.fix_percentage());
    let mut by_reason = std::collections::BTreeMap::new();
    for fx in &t.gps { if let Some(r) = fx.reject { *by_reason.entry(r.as_str()).or_insert(0usize) += 1; } }
    for (k, v) in &by_reason { println!("  rejected {v:>6}  {k}"); }
    println!("LEAN SIGN: flipped={} |r|={:.4} n={} confidence={}",
        t.lean_sign.flipped, t.lean_sign.correlation, t.lean_sign.samples, t.lean_sign.confidence.as_str());
    println!("  gps convention r={:?}", t.lean_sign.gps_correlation.map(|r| format!("{r:+.4}")));
    for w in &t.warnings { println!("  warn: {w}"); }

    let stats = |name: &str, v: &[f64]| {
        let fin: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
        if fin.is_empty() { println!("  {name:<16} (empty)"); return; }
        let mut s = fin.clone(); s.sort_by(|a,b| a.partial_cmp(b).unwrap());
        println!("  {name:<16} min={:>9.3} p0.5={:>9.3} med={:>8.3} p99.5={:>8.3} max={:>8.3}",
            s[0], quantile_sorted(&s, 0.005), quantile_sorted(&s, 0.5), quantile_sorted(&s, 0.995), s[s.len()-1]);
    };
    println!("channels:");
    stats("lean_deg", &t.lean_deg);
    stats("pitch_deg", &t.pitch_deg);
    stats("yaw_rate_dps", &t.yaw_rate_dps);
    if let Some(v) = &t.lean_rate_dps { stats("lean_rate_dps", v); }
    if let Some(v) = &t.accel_lon_g { stats("accel_lon_g", v); }
    if let Some(v) = &t.accel_lat_g { stats("accel_lat_g", v); }
    if let Some(v) = &t.accel_vert_g { stats("accel_vert_g", v); }
    if let Some(v) = &t.accel_total_g { stats("accel_total_g", v); }
    if let Some(v) = &t.imu_temp_c { stats("imu_temp_c", v); }
    println!("distance={:.2} km  climb={:.0} m", t.distance_m()/1000.0, t.elevation_gain_m());

    println!("first valid fix:");
    if let Some(fx) = t.gps.iter().find(|f| f.valid) {
        println!("  t={:.3}s utc={:?} lat={:.7} lon={:.7} alt={:.2} spd={:.2} dop={:?} fix={:?}",
            fx.time, fx.utc.map(|d| d.to_rfc3339()), fx.lat, fx.lon, fx.alt_m, fx.speed_2d, fx.dop, fx.fix);
    }
    println!("sample 0 gps: {:?}", t.gps.first().map(|f| (f.lat, f.lon, f.dop, f.fix, f.reject.map(|r| r.as_str()))));
    Ok(())
}

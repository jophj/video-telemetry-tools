//! Unit tests for the shared numeric helpers.

use gmax::math::*;

#[test]
fn quantile_is_the_linear_estimator() {
    // The "type 7" estimator, which is what motion-record-analysis uses for
    // its cadence statistics. Cadence drives every window in the moments
    // engine, so this has to match rather than merely be reasonable.
    let v = [1.0, 2.0, 3.0, 4.0];
    assert_eq!(quantile_sorted(&v, 0.0), 1.0);
    assert_eq!(quantile_sorted(&v, 1.0), 4.0);
    assert_eq!(quantile_sorted(&v, 0.5), 2.5);
    // pos = 3 * 0.25 = 0.75, so 1 + (2-1)*0.75
    assert_eq!(quantile_sorted(&v, 0.25), 1.75);
    assert_eq!(quantile_sorted(&[7.0], 0.5), 7.0);
    assert!(quantile_sorted(&[], 0.5).is_nan());
}

#[test]
fn moving_average_shortens_at_the_edges_rather_than_reflecting() {
    // Matches the prototype's `_movavg`: window [i-h, i+h] inclusive of the
    // extra right-hand sample, clipped at the bounds.
    let x = [1.0, 2.0, 3.0, 4.0, 5.0];
    let out = moving_average(&x, 3);
    assert_eq!(out[0], 1.5); // mean of [1, 2]
    assert_eq!(out[2], 3.0); // mean of [2, 3, 4]
    assert_eq!(out[4], 4.5); // mean of [4, 5]
    // A width below 2 is a no-op.
    assert_eq!(moving_average(&x, 1), x.to_vec());
}

#[test]
fn angle_delta_takes_the_short_way_round() {
    // The reason this exists: a one-degree turn through north must not read as
    // a 359-degree turn the other way, or every heading change near due north
    // gets the wrong sign.
    assert_eq!(angle_delta(359.0, 0.0), 1.0);
    assert_eq!(angle_delta(0.0, 359.0), -1.0);
    assert_eq!(angle_delta(10.0, 20.0), 10.0);
    assert_eq!(angle_delta(20.0, 10.0), -10.0);
    // Exactly opposite resolves to +180, not -180.
    assert_eq!(angle_delta(0.0, 180.0), 180.0);
}

#[test]
fn bearing_and_haversine_agree_with_known_values() {
    // Due north from the equator.
    assert!((bearing(0.0, 0.0, 1.0, 0.0) - 0.0).abs() < 1e-9);
    // Due east.
    assert!((bearing(0.0, 0.0, 0.0, 1.0) - 90.0).abs() < 1e-9);
    // One degree of latitude is about 111.2 km.
    let d = haversine(0.0, 0.0, 1.0, 0.0);
    assert!((d - 111_195.0).abs() < 50.0, "got {d}");
    assert_eq!(haversine(44.0, 11.0, 44.0, 11.0), 0.0);
}

#[test]
fn great_circle_interpolation_hits_both_ends_and_the_middle() {
    let (a_lat, a_lon) = (44.0, 11.0);
    let (b_lat, b_lon) = (44.1, 11.1);
    let (lat, lon) = interpolate_great_circle(a_lat, a_lon, b_lat, b_lon, 0.0);
    assert!((lat - a_lat).abs() < 1e-9 && (lon - a_lon).abs() < 1e-9);
    let (lat, lon) = interpolate_great_circle(a_lat, a_lon, b_lat, b_lon, 1.0);
    assert!((lat - b_lat).abs() < 1e-9 && (lon - b_lon).abs() < 1e-9);
    // Over the distance this is actually used for -- one second of riding
    // between two GPS fixes, about 18 m -- a great circle is indistinguishable
    // from a straight line, well under a micrometre.
    let step = 0.00016;
    let (lat, lon) = interpolate_great_circle(44.0, 11.0, 44.0 + step, 11.0 + step, 0.5);
    let deviation = haversine(lat, lon, 44.0 + step / 2.0, 11.0 + step / 2.0);
    assert!(deviation < 1e-3, "deviation over 18 m was {deviation} m");

    // Over a long segment it must bow the right way: the great-circle midpoint
    // of an east-north leg in the northern hemisphere sits poleward of the mean
    // of the endpoints. This is the reason for using slerp rather than a linear
    // blend, so assert the behaviour rather than a tolerance.
    let (lat, lon) = interpolate_great_circle(a_lat, a_lon, b_lat, b_lon, 0.5);
    assert!(lat > 44.05, "expected the midpoint poleward of 44.05, got {lat}");
    assert!(haversine(lat, lon, 44.05, 11.05) < 10.0);

    // Identical endpoints must not divide by zero.
    let (lat, lon) = interpolate_great_circle(44.0, 11.0, 44.0, 11.0, 0.5);
    assert_eq!((lat, lon), (44.0, 11.0));
}

#[test]
fn fmt_num_matches_javascript_round_then_stringify() {
    // motion-record-analysis rounds to N places and lets JS stringify the
    // Number, so trailing zeros vanish and -0 prints as 0. Reproducing that is
    // the difference between matching its GPX byte for byte and merely
    // resembling it.
    assert_eq!(fmt_num(55.0, 2), "55");
    assert_eq!(fmt_num(0.18, 3), "0.18");
    assert_eq!(fmt_num(201.0, 1), "201");
    assert_eq!(fmt_num(918.0, 1), "918");
    assert_eq!(fmt_num(1013.7000122070312, 1), "1013.7");
    assert_eq!(fmt_num(-0.0, 3), "0");
    assert_eq!(fmt_num(-0.0004, 2), "0");
    assert_eq!(fmt_num(44.1727632, 7), "44.1727632");
    assert_eq!(fmt_num(f64::NAN, 2), "0");
}

#[test]
fn principal_axis_finds_the_dominant_direction() {
    // Variation almost entirely along Y.
    let deviations = vec![
        [0.01, 1.0, 0.0],
        [-0.01, -1.0, 0.0],
        [0.0, 0.5, 0.01],
        [0.0, -0.5, -0.01],
    ];
    let axis = principal_axis(&covariance(&deviations));
    assert!(axis[1].abs() > 0.99, "expected Y-dominant, got {axis:?}");
}

#[test]
fn pearson_handles_constant_series_without_dividing_by_zero() {
    assert_eq!(pearson(&[1.0, 1.0, 1.0], &[1.0, 2.0, 3.0]), 0.0);
    let r = pearson(&[1.0, 2.0, 3.0], &[2.0, 4.0, 6.0]);
    assert!((r - 1.0).abs() < 1e-12);
    let r = pearson(&[1.0, 2.0, 3.0], &[-2.0, -4.0, -6.0]);
    assert!((r + 1.0).abs() < 1e-12);
}

#[test]
fn circular_mean_averages_across_north() {
    let m = circular_mean(&[350.0, 10.0]).unwrap();
    assert!(m < 1.0 || m > 359.0, "got {m}");
    // Diametrically opposed angles have no mean direction.
    assert!(circular_mean(&[0.0, 180.0]).is_none());
}

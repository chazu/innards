//! One native aggregate per terminal column; extrema survive decimation.
use super::{
    data::Data,
    protocol::{Glyph, Props},
};
#[derive(Clone)]
pub struct Sample {
    pub x: f64,
    pub y: f64,
    pub key: String,
}
pub struct Plot {
    pub revision: u64,
    pub width: u16,
    pub height: u16,
    pub domain: Option<(f64, f64)>,
    pub config: (String, String, Option<[f64; 2]>),
    pub cells: Vec<Glyph>,
    pub columns: Vec<Option<Sample>>,
    pub label: String,
}
fn samples<'a>(data: &'a Data, p: &'a Props) -> impl Iterator<Item = Sample> + 'a {
    // Windowed sources are intentionally unsupported for plots: a complete
    // bounded sampling batch is required to preserve extrema.
    (0..if data.windowed { 0 } else { data.total }).filter_map(move |i| {
        let row = data.row(i)?;
        let x = row
            .fields
            .get(if p.x_field.is_empty() {
                "x"
            } else {
                &p.x_field
            })?
            .parse::<f64>()
            .ok()?;
        let y = row
            .fields
            .get(if p.y_field.is_empty() {
                "y"
            } else {
                &p.y_field
            })?
            .parse::<f64>()
            .ok()?;
        (x.is_finite() && y.is_finite()).then(|| Sample {
            x,
            y,
            key: row.key.clone(),
        })
    })
}
pub fn extent(data: &Data, p: &Props) -> Option<(f64, f64)> {
    let mut points = samples(data, p);
    let first = points.next()?;
    let (mut lo, mut hi) = (first.x, first.x);
    for point in points {
        lo = lo.min(point.x);
        hi = hi.max(point.x);
    }
    if lo == hi {
        hi = lo + 1.;
    }
    Some((lo, hi))
}
impl Plot {
    pub fn build(
        data: &Data,
        p: &Props,
        width: u16,
        height: u16,
        domain: Option<(f64, f64)>,
    ) -> Self {
        let (lo, hi) = domain.or_else(|| extent(data, p)).unwrap_or((0., 1.));
        let mut buckets: Vec<Option<(Sample, Sample)>> = vec![None; width as usize];
        let (mut ymin, mut ymax) = (f64::INFINITY, f64::NEG_INFINITY);
        if width > 0 && height > 0 && hi > lo && lo.is_finite() && hi.is_finite() {
            for point in samples(data, p) {
                if point.x < lo || point.x > hi {
                    continue;
                }
                let column = (((point.x - lo) / (hi - lo) * f64::from(width.saturating_sub(1)))
                    .round() as usize)
                    .min(width as usize - 1);
                ymin = ymin.min(point.y);
                ymax = ymax.max(point.y);
                if let Some((min, max)) = &mut buckets[column] {
                    if point.y < min.y {
                        *min = point.clone();
                    }
                    if point.y > max.y {
                        *max = point;
                    }
                } else {
                    buckets[column] = Some((point.clone(), point));
                }
            }
        }
        if let Some([low, high]) = p.domain_y {
            ymin = low;
            ymax = high;
        }
        if !ymin.is_finite() {
            ymin = 0.;
            ymax = 1.;
        }
        if ymax <= ymin {
            ymax = ymin + 1.;
        }
        let mut cells = Vec::new();
        let mut columns = Vec::new();
        for (x, bucket) in buckets.into_iter().enumerate() {
            if let Some((min, max)) = bucket {
                let y = |value: f64| {
                    ((1. - ((value - ymin) / (ymax - ymin)).clamp(0., 1.))
                        * f64::from(height.saturating_sub(1)))
                    .round() as u16
                };
                let top = y(max.y);
                let bottom = y(min.y);
                for row in top..=bottom {
                    cells.push(Glyph {
                        x: x as u16,
                        y: row,
                        text: if top == bottom { "•" } else { "│" }.into(),
                        style: "primary".into(),
                    });
                }
                columns.push(Some(max));
            } else {
                columns.push(None);
            }
        }
        Self {
            revision: data.revision,
            width,
            height,
            domain,
            config: (p.x_field.clone(), p.y_field.clone(), p.domain_y),
            cells,
            columns,
            label: format!("x {lo:.1}…{hi:.1}  y {ymin:.1}…{ymax:.1}"),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::protocol::{Collection, Row};
    #[test]
    fn decimation_keeps_single_sample_spike() {
        let data = Data::new(Collection {
            id: "samples".into(),
            revision: 0,
            total: 10000,
            start: 0,
            retention: 10000,
            windowed: false,
            rows: (0..10000)
                .map(|i| Row {
                    key: i.to_string(),
                    fields: [
                        ("x".into(), i.to_string()),
                        ("y".into(), if i == 5001 { "100" } else { "0" }.into()),
                    ]
                    .into(),
                })
                .collect(),
        })
        .unwrap();
        let plot = Plot::build(&data, &Props::default(), 80, 10, None);
        assert!(plot.cells.iter().any(|p| p.y == 0));
        assert!(plot.cells.iter().any(|p| p.y == 9));
        assert_eq!(plot.columns.len(), 80);
        assert!(plot.cells.len() <= 800);
        assert!(plot.columns.iter().flatten().any(|p| p.key == "5001"));
    }
}

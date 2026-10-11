//! Turns a query result into chart series: picks sensible columns and parses numbers. Pure data;
//! drawing lives in the app.

use crate::model::QueryResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartKind {
    Bar,
    Line,
    Pie,
    Scatter,
}

impl ChartKind {
    pub const ALL: [ChartKind; 4] = [ChartKind::Bar, ChartKind::Line, ChartKind::Pie, ChartKind::Scatter];
    pub fn label(self) -> &'static str {
        match self {
            ChartKind::Bar => "Bar",
            ChartKind::Line => "Line",
            ChartKind::Pie => "Pie",
            ChartKind::Scatter => "Scatter",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChartSpec {
    pub kind: ChartKind,
    /// Column used for categories / the horizontal axis.
    pub x: usize,
    /// Value columns (Pie uses the first one).
    pub ys: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub name: String,
    /// One value per plotted row; `None` where the cell was NULL or not a number.
    pub values: Vec<Option<f64>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChartData {
    pub labels: Vec<String>,
    /// Numeric x values when the x column is numeric (Line / Scatter use them).
    pub xs: Option<Vec<f64>>,
    pub series: Vec<Series>,
    /// Rows left out because of the point cap.
    pub omitted_rows: usize,
}

/// Most points drawn; more would only slow the UI down.
pub const MAX_POINTS: usize = 5000;

fn parse_num(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() { None } else { t.parse::<f64>().ok().filter(|f| f.is_finite()) }
}

/// Columns whose non-NULL values are (nearly) all numbers.
pub fn numeric_columns(r: &QueryResult) -> Vec<usize> {
    (0..r.columns.len())
        .filter(|&c| {
            let cells: Vec<&str> = r.rows.iter().filter_map(|row| row.get(c)?.as_deref()).collect();
            !cells.is_empty() && cells.iter().filter(|v| parse_num(v).is_some()).count() * 10 >= cells.len() * 9
        })
        .collect()
}

/// A starting point: the first non-numeric column (else the first column) against the numeric
/// columns; bar for categories, line when the x column looks like a number or date.
pub fn suggest(r: &QueryResult) -> Option<ChartSpec> {
    let nums = numeric_columns(r);
    if nums.is_empty() {
        return None;
    }
    let x = (0..r.columns.len()).find(|c| !nums.contains(c)).unwrap_or(0);
    let ys: Vec<usize> = nums.into_iter().filter(|c| *c != x).take(3).collect();
    if ys.is_empty() {
        return None;
    }
    let datey = r.columns[x].type_name.to_lowercase().contains("date")
        || r.columns[x].type_name.to_lowercase().contains("time");
    Some(ChartSpec { kind: if datey { ChartKind::Line } else { ChartKind::Bar }, x, ys })
}

pub fn build(r: &QueryResult, spec: &ChartSpec) -> ChartData {
    let take = r.rows.len().min(MAX_POINTS);
    let rows = &r.rows[..take];
    let labels: Vec<String> =
        rows.iter().map(|row| row.get(spec.x).cloned().flatten().unwrap_or_else(|| "NULL".into())).collect();
    let nums: Vec<Option<f64>> = labels.iter().map(|l| parse_num(l)).collect();
    let xs = nums.iter().all(Option::is_some).then(|| nums.iter().flatten().copied().collect());
    let series = spec
        .ys
        .iter()
        .filter_map(|&c| {
            let col = r.columns.get(c)?;
            Some(Series {
                name: col.name.clone(),
                values: rows.iter().map(|row| row.get(c).and_then(|v| v.as_deref()).and_then(parse_num)).collect(),
            })
        })
        .collect();
    ChartData { labels, xs: if take == 0 { None } else { xs }, series, omitted_rows: r.rows.len() - take }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ColumnMeta;

    fn result(cols: &[(&str, &str)], rows: &[&[Option<&str>]]) -> QueryResult {
        QueryResult {
            columns: cols
                .iter()
                .map(|(n, t)| ColumnMeta { name: (*n).into(), type_name: (*t).into(), binary: false })
                .collect(),
            rows: rows.iter().map(|r| r.iter().map(|c| c.map(String::from)).collect()).collect(),
            truncated: false,
        }
    }

    #[test]
    fn suggests_category_against_numbers() {
        let r = result(
            &[("city", "text"), ("sales", "int"), ("returns", "int"), ("note", "text")],
            &[&[Some("Oslo"), Some("10"), Some("1"), Some("a")], &[Some("Rome"), Some("20.5"), None, Some("b")]],
        );
        assert_eq!(numeric_columns(&r), vec![1, 2]);
        let s = suggest(&r).unwrap();
        assert_eq!((s.kind, s.x, s.ys.clone()), (ChartKind::Bar, 0, vec![1, 2]));
        let d = build(&r, &s);
        assert_eq!(d.labels, ["Oslo", "Rome"]);
        assert_eq!(d.series[0].values, vec![Some(10.0), Some(20.5)]);
        assert_eq!(d.series[1].values, vec![Some(1.0), None]);
        assert!(d.xs.is_none(), "text categories have no numeric axis");
    }

    #[test]
    fn dates_suggest_a_line_and_numbers_give_an_axis() {
        let r = result(
            &[("day", "date"), ("n", "int")],
            &[&[Some("2024-01-01"), Some("1")], &[Some("2024-01-02"), Some("3")]],
        );
        assert_eq!(suggest(&r).unwrap().kind, ChartKind::Line);
        let r = result(&[("x", "int"), ("y", "int")], &[&[Some("1"), Some("5")], &[Some("2"), Some("7")]]);
        let spec = ChartSpec { kind: ChartKind::Scatter, x: 0, ys: vec![1] };
        assert_eq!(build(&r, &spec).xs, Some(vec![1.0, 2.0]));
    }

    #[test]
    fn nothing_numeric_means_no_suggestion_and_big_results_are_capped() {
        let r = result(&[("a", "text")], &[&[Some("x")]]);
        assert!(suggest(&r).is_none());
        let rows: Vec<Vec<Option<String>>> =
            (0..MAX_POINTS + 7).map(|i| vec![Some(i.to_string()), Some("1".into())]).collect();
        let big = QueryResult { columns: result(&[("i", "int"), ("v", "int")], &[]).columns, rows, truncated: false };
        let d = build(&big, &ChartSpec { kind: ChartKind::Line, x: 0, ys: vec![1] });
        assert_eq!((d.labels.len(), d.omitted_rows), (MAX_POINTS, 7));
    }

    #[test]
    fn mostly_text_columns_are_not_numeric() {
        let r = result(&[("v", "text")], &[&[Some("1")], &[Some("abc")], &[Some("def")]]);
        assert!(numeric_columns(&r).is_empty());
    }
}
